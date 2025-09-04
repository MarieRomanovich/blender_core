use anyhow::{anyhow, bail, Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use std::fs;
use std::path::Path;

const DB_PATH: &str = "data/bot.db";

pub fn init_db() -> Result<()> {
    fs::create_dir_all("data").ok();
    let conn = Connection::open(DB_PATH).context("open db")?;
    conn.execute_batch(
        r#"
        PRAGMA journal_mode = WAL;
        CREATE TABLE IF NOT EXISTS channels (
            id        INTEGER PRIMARY KEY,
            title     TEXT NOT NULL,
            username  TEXT,
            ctype     TEXT
        );
        CREATE TABLE IF NOT EXISTS user_subscriptions (
            user_chat_id INTEGER PRIMARY KEY,
            paid_until   INTEGER NOT NULL
        );
        "#,
    )?;
    let _ = conn.execute("ALTER TABLE channels ADD COLUMN ctype TEXT", []);
    Ok(())
}

// Add helpers
pub fn set_paid_until(user_chat_id: i64, paid_until: i64) -> Result<()> {
    let conn = Connection::open(DB_PATH).context("open db")?;
    conn.execute(
        "INSERT INTO user_subscriptions (user_chat_id, paid_until)
         VALUES (?1, ?2)
         ON CONFLICT(user_chat_id) DO UPDATE SET paid_until = excluded.paid_until",
        params![user_chat_id, paid_until],
    )?;
    Ok(())
}

pub fn is_subscription_active(user_chat_id: i64, now_ts: i64) -> Result<bool> {
    let conn = Connection::open(DB_PATH).context("open db")?;
    let v: Option<i64> = conn
        .query_row(
            "SELECT paid_until FROM user_subscriptions WHERE user_chat_id = ?1",
            params![user_chat_id],
            |r| r.get(0),
        )
        .optional()?;
    Ok(v.map_or(false, |until| until > now_ts))
}

pub fn add_channel(id: i64, title: &str, username: Option<&str>) -> Result<bool> {
    let conn = Connection::open(DB_PATH).context("open db")?;
    let changed = conn
        .execute(
            "INSERT OR REPLACE INTO channels (id, title, username) VALUES (?1, ?2, ?3)",
            params![id, title, username],
        )
        .context("insert channel")?;
    Ok(changed > 0)
}

pub fn remove_channel(id: i64) -> Result<bool> {
    let conn = Connection::open(DB_PATH).context("open db")?;
    let changed = conn
        .execute("DELETE FROM channels WHERE id = ?1", params![id])
        .context("delete channel")?;
    Ok(changed > 0)
}

// Keep existing list_channels for backward-compat (channels only)
pub fn list_channels() -> Result<Vec<(i64, String, Option<String>)>> {
    let conn = Connection::open(DB_PATH).context("open db")?;
    let mut stmt =
        conn.prepare("SELECT id, title, username FROM channels WHERE COALESCE(ctype,'channel') = 'channel' ORDER BY LOWER(title)")?;
    let rows = stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<String>>(2)?,
            ))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

// New: list everything with type
pub fn list_items_with_type() -> Result<Vec<(i64, String, Option<String>, String)>> {
    let conn = Connection::open(DB_PATH).context("open db")?;
    let mut stmt = conn.prepare(
        "SELECT id, title, username, COALESCE(ctype,'channel') as ctype
         FROM channels
         ORDER BY LOWER(title)",
    )?;
    let rows = stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<String>>(2)?,
                r.get::<_, String>(3)?,
            ))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

pub fn contains_channel(id: i64) -> Result<bool> {
    let conn = Connection::open(DB_PATH).context("open db")?;
    let exists: Option<i64> = conn
        .query_row(
            "SELECT 1 FROM channels WHERE id = ?1 LIMIT 1",
            params![id],
            |r| r.get(0),
        )
        .optional()
        .context("contains select")?;
    Ok(exists.is_some())
}

pub fn replace_all(chs: &[(i64, &str, Option<&str>)]) -> Result<()> {
    let mut conn = Connection::open(DB_PATH).context("open db")?;
    let tx = conn.transaction().context("begin tx")?;
    tx.execute("DELETE FROM channels", []).context("clear")?;
    let mut ins = tx
        .prepare("INSERT INTO channels (id, title, username) VALUES (?1, ?2, ?3)")
        .context("prepare insert")?;
    for (id, title, username) in chs {
        ins.execute(params![id, title, username]).context("insert")?;
    }
    drop(ins);
    tx.commit().context("commit")?;
    Ok(())
}

pub fn replace_all_from_csv(csv: &str) -> Result<usize> {
    // CSV format: id;title;username(optional). Lines starting with # are comments.
    let mut rows: Vec<(i64, String, Option<String>)> = Vec::new();

    for (lineno, raw) in csv.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut parts = line.splitn(3, ';').map(|s| s.trim());
        let id_str = parts.next().unwrap_or_default();

        // Optional header support
        if lineno == 0
            && (id_str.eq_ignore_ascii_case("id")
                || !id_str.chars().next().unwrap_or(' ').is_ascii_digit())
        {
            continue;
        }

        let id: i64 = id_str
            .parse()
            .map_err(|e| anyhow!("Line {}: invalid id '{}': {e}", lineno + 1, id_str))?;
        let title = parts.next().unwrap_or_default().to_string();
        let username = parts
            .next()
            .map(|s| s.trim_matches('@'))
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string());

        if title.is_empty() {
            bail!("Line {}: title is required (format: id;title;username?)", lineno + 1);
        }
        rows.push((id, title, username));
    }

    let mut conn = Connection::open(DB_PATH).context("open db")?;
    let tx = conn.transaction().context("begin tx")?;
    let mut ins = tx
        .prepare("INSERT INTO channels (id, title, username) VALUES (?1, ?2, ?3)")
        .context("prepare insert")?;
    for (id, title, username) in rows.iter() {
        ins.execute(params![id, title, username.as_deref()]).context("insert")?;
    }
    drop(ins);
    tx.commit().context("commit")?;
    Ok(rows.len())
}

pub fn replace_all_from_csv_file(path: &str) -> Result<usize> {
    let csv = fs::read_to_string(path).with_context(|| format!("read {path}"))?;
    replace_all_from_csv(&csv)
}

pub fn dump_to_csv_file(path: &str) -> Result<usize> {
    let list = list_channels()?;
    let mut out = String::new();
    out.push_str("# id;title;username\n");
    out.push_str("# username is optional (without @). One channel per line.\n");
    for (id, title, username) in list {
        if let Some(u) = username {
            out.push_str(&format!("{id};{title};{u}\n"));
        } else {
            out.push_str(&format!("{id};{title};\n"));
        }
    }
    if let Some(parent) = Path::new(path).parent() {
        fs::create_dir_all(parent).ok();
    }
    let count = out.lines().filter(|l| !l.starts_with('#') && !l.trim().is_empty()).count();
    fs::write(path, out).with_context(|| format!("write {path}"))?;
    Ok(count)
}

pub fn ensure_seed_file(path: &str) -> Result<usize> {
    if Path::new(path).exists() {
        return Ok(0);
    }
    let list = list_channels()?;
    if list.is_empty() {
        let template = r#"# id;title;username
# Example:
# -1001234567890;My News Channel;mynewschannel
# -1009876543210;Another Channel;
"#;
        if let Some(parent) = Path::new(path).parent() {
            fs::create_dir_all(parent).ok();
        }
        fs::write(path, template).with_context(|| format!("write template {path}"))?;
        return Ok(0);
    }
    dump_to_csv_file(path)
}

pub fn get_user_selected_ids(user_chat_id: i64) -> Result<Vec<i64>> {
    let conn = Connection::open(DB_PATH).context("open db")?;
    let mut stmt = conn.prepare(
        "SELECT channel_id FROM user_selected_channels WHERE user_chat_id = ?1 ORDER BY channel_id",
    )?;
    let rows = stmt
        .query_map(params![user_chat_id], |r| Ok(r.get::<_, i64>(0)?))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

pub fn toggle_user_selection(user_chat_id: i64, channel_id: i64) -> Result<bool> {
    let conn = Connection::open(DB_PATH).context("open db")?;
    let exists: Option<i64> = conn
        .query_row(
            "SELECT 1 FROM user_selected_channels WHERE user_chat_id = ?1 AND channel_id = ?2",
            params![user_chat_id, channel_id],
            |r| r.get(0),
        )
        .optional()
        .context("check exists")?;
    if exists.is_some() {
        conn.execute(
            "DELETE FROM user_selected_channels WHERE user_chat_id = ?1 AND channel_id = ?2",
            params![user_chat_id, channel_id],
        )
        .context("delete selection")?;
        Ok(false) // now unselected
    } else {
        conn.execute(
            "INSERT INTO user_selected_channels (user_chat_id, channel_id) VALUES (?1, ?2)",
            params![user_chat_id, channel_id],
        )
        .context("insert selection")?;
        Ok(true) // now selected
    }
}

// New: import ALL dialogs from chats_export.csv (type;id;title;username)
pub fn replace_all_from_chats_export_csv(csv: &str) -> Result<usize> {
    // Header: type;id;title;username
    let mut rows: Vec<(i64, String, Option<String>, String)> = Vec::new();

    for (lineno, raw) in csv.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if lineno == 0 && line.to_ascii_lowercase().starts_with("type;") {
            continue;
        }
        let mut parts = line.splitn(4, ';').map(|s| s.trim());
        let typ = parts.next().unwrap_or_default().to_ascii_lowercase();
        let id_str = parts.next().unwrap_or_default();
        let id: i64 = id_str
            .parse()
            .map_err(|e| anyhow!("Line {}: invalid id '{}': {e}", lineno + 1, id_str))?;
        let title = parts.next().unwrap_or_default().to_string();
        if title.is_empty() {
            bail!("Line {}: title is required", lineno + 1);
        }
        let username = parts
            .next()
            .map(|s| s.trim_matches('@'))
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string());
        rows.push((id, title, username, typ));
    }

    let mut conn = Connection::open(DB_PATH).context("open db")?;
    let tx = conn.transaction().context("begin tx")?;
    tx.execute("DELETE FROM channels", []).context("clear")?;
    let mut ins = tx.prepare(
        "INSERT INTO channels (id, title, username, ctype) VALUES (?1, ?2, ?3, ?4)",
    )?;
    for (id, title, username, ctype) in rows.iter() {
        ins.execute(params![id, title, username.as_deref(), ctype])?;
    }
    drop(ins);
    tx.commit().context("commit")?;
    Ok(rows.len())
}

pub fn replace_all_from_chats_export_csv_file(path: &str) -> Result<usize> {
    let csv = fs::read_to_string(path).with_context(|| format!("read {path}"))?;
    replace_all_from_chats_export_csv(&csv)
}