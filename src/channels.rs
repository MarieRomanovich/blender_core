use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use std::path::Path;

const DB_PATH: &str = "data/bot.db";

pub fn init_db() -> Result<()> {
    std::fs::create_dir_all("data").ok();
    let conn = Connection::open(DB_PATH).context("open db")?;
    conn.execute_batch(
        r#"
        PRAGMA journal_mode = WAL;
        CREATE TABLE IF NOT EXISTS channels (
            id      INTEGER PRIMARY KEY, -- Telegram chat id
            title   TEXT NOT NULL,
            username TEXT
        );
        "#,
    )
    .context("migrate db")?;
    Ok(())
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

pub fn list_channels() -> Result<Vec<(i64, String, Option<String>)>> {
    let conn = Connection::open(DB_PATH).context("open db")?;
    let mut stmt = conn.prepare("SELECT id, title, username FROM channels ORDER BY LOWER(title)")?;
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

pub fn contains_channel(id: i64) -> Result<bool> {
    let conn = Connection::open(DB_PATH).context("open db")?;
    let exists: Option<i64> = conn
        .query_row(
            "SELECT 1 FROM channels WHERE id = ?1 LIMIT 1",
            params![id],
            |r| r.get(0),
        )
        .optional()
        .context("select contains")?;
    Ok(exists.is_some())
}