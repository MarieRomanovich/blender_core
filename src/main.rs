use csv::ReaderBuilder;
use tokio::fs as tokio_fs;
use std::{env, fs, io::Write, path::{Path, PathBuf}, time::{Duration, SystemTime, UNIX_EPOCH}};
use tokio::time::sleep;
use teloxide::types::{InputFile, ChatId};

use anyhow::Context;
use dotenvy::dotenv;
use grammers_client::{reply_markup, Client, Config};
use grammers_session::Session;
use serde_json::{json, Value};
use teloxide::{
    prelude::*,
    types::{InlineKeyboardButton, InlineKeyboardMarkup},
};
// Additional imports for dptree-style dispatcher and Payments type
use teloxide::{dispatching::HandlerExt, dispatching::UpdateFilterExt, dptree};
use crate::payments::{Payments, Subscriber};
use teloxide::types::{CallbackQuery};
use teloxide::prelude::*;
use std::sync::Arc;
use std::collections::HashSet;

mod payments;
mod admin;
mod channels;
mod catalog;
mod forward; 
mod user_forward;

const CHATS_EXPORT_PATH: &str = "src/chats_export.csv";

const DATA_DIR: &str = "data";
const STATE_PATH: &str = "data/state.json"; // {"target_chat_id": i64, "subscribed": bool, "paid": bool}

fn ensure_data_dir() -> anyhow::Result<()> {
    fs::create_dir_all(DATA_DIR).context("create data dir")?;
    Ok(())
}

async fn send_startup_to_all(bot: &Bot) {
    // message to send (exact text provided)
    let startup_msg = r#"Лучшее что ты можешь сделать прямо сейчас - ДЕЙСТВОВАТЬ !

Коротко о BLENDER — множество приваток, которые я лично отбирал с 2019 года и это самый дешевый и самый качественный агрегатор который вы могли только найти.

Наш канал: t.me/blender
Поддержка: @ex_managers

🔻 Сумма всех приваток: 7394$/мес
✅ Сумма всех приваток у нас: 42$/мес "#;

    // optional image path or URL from env
    let startup_img = std::env::var("STARTUP_IMG").ok();

    // file with recipients: JSON array of chat ids or usernames
    let file = std::env::var("STARTUP_ALL_FILE").unwrap_or_else(|_| "data/subscribers.json".to_string());

    let data = match fs::read_to_string(&file) {
        Ok(s) => s,
        Err(e) => {
            log::warn!("send_startup_to_all: failed to read {}: {}", file, e);
            return;
        }
    };

    let list: Value = match serde_json::from_str(&data) {
        Ok(v) => v,
        Err(e) => {
            log::warn!("send_startup_to_all: failed to parse {} as JSON: {}", file, e);
            return;
        }
    };

    let mut recipients = Vec::new();
    if let Value::Array(arr) = list {
        for v in arr {
            match v {
                Value::String(s) => recipients.push(s),
                Value::Number(n) => recipients.push(n.to_string()),
                _ => continue,
            }
        }
    } else {
        log::warn!("send_startup_to_all: {} is not a JSON array", file);
        return;
    }

    for r in recipients {
        // try numeric id first
        if let Ok(id) = r.parse::<i64>() {
            let chat = ChatId(id);
            if let Some(ref img) = startup_img {
                match bot.send_photo(chat, InputFile::file(img.clone()))
                    .caption(startup_msg.to_string())
                    .await
                {
                    Ok(_) => log::info!("startup: sent photo+caption to id {}", id),
                    Err(e) => log::warn!("startup: failed to send photo to id {}: {}", id, e),
                }
            } else {
                match bot.send_message(chat, startup_msg.to_string()).await {
                    Ok(_) => log::info!("startup: sent text to id {}", id),
                    Err(e) => log::warn!("startup: failed to send to id {}: {}", id, e),
                }
            }
        } else {
            // username / channel string
            if let Some(ref img) = startup_img {
                match bot.send_photo(r.clone(), InputFile::file(img.clone()))
                    .caption(startup_msg.to_string())
                    .await
                {
                    Ok(_) => log::info!("startup: sent photo+caption to {}", r),
                    Err(e) => log::warn!("startup: failed to send photo to {}: {}", r, e),
                }
            } else {
                match bot.send_message(r.clone(), startup_msg.to_string()).await {
                    Ok(_) => log::info!("startup: sent text to {}", r),
                    Err(e) => log::warn!("startup: failed to send to {}: {}", r, e),
                }
            }
        }
        sleep(Duration::from_millis(300)).await; // rate-limit delay
    }
}

async fn send_startup_to_target(bot: &Bot) {
    // Default caption (exact text provided)
    let startup_msg = r#"Лучшее что ты можешь сделать прямо сейчас - ДЕЙСТВОВАТЬ !

Коротко о BLENDER — множество приваток, которые я лично отбирал с 2019 года и это самый дешевый и самый качественный агрегатор который вы могли только найти.

Наш канал: t.me/blender
Поддержка: @ex_managers

🔻 Сумма всех приваток: 7394$/мес
✅ Сумма всех приваток у нас: 42$/мес "#;

    // get target from env
    let target_raw = env::var("TARGET_CHANNEL")
        .or_else(|_| env::var("CHANNEL_ID"))
        .unwrap_or_default();

    if target_raw.is_empty() {
        tracing::warn!("send_startup_to_target: TARGET_CHANNEL not set; skipping startup send");
        return;
    }

    // sanitize STARTUP_IMG
    let mut img_opt = env::var("STARTUP_IMG").ok();
    if let Some(ref s) = img_opt {
        img_opt = Some(s.trim().trim_matches('"').to_string());
    }
    // fallback: if no STARTUP_IMG env, try data/startup.jpg
    if img_opt.is_none() {
        let fb = Path::new("data").join("startup.jpg");
        if fb.exists() {
            img_opt = Some(fb.to_string_lossy().to_string());
            tracing::info!(target = %target_raw, startup_img = ?img_opt, "send_startup_to_target: using fallback STARTUP_IMG");
        } else {
            tracing::info!(target = %target_raw, startup_img = ?img_opt, "send_startup_to_target: STARTUP_IMG not set and no fallback found");
        }
    }

    // resolve chat id or username and send
    let send_result = if let Ok(id) = target_raw.parse::<i64>() {
        let chat = ChatId(id);
        if let Some(ref img) = img_opt {
            if Path::new(img).exists() {
                bot.send_photo(chat, InputFile::file(img.clone()))
                    .caption(startup_msg.to_string())
                    .await
            } else {
                tracing::warn!("send_startup_to_target: image not found at {} — sending text only", img);
                bot.send_message(chat, startup_msg.to_string()).await
            }
        } else {
            bot.send_message(chat, startup_msg.to_string()).await
        }
    } else {
        // treat as username/channel string
        let chat_str = target_raw.clone();
        if let Some(ref img) = img_opt {
            if Path::new(img).exists() {
                bot.send_photo(chat_str.clone(), InputFile::file(img.clone()))
                    .caption(startup_msg.to_string())
                    .await
            } else {
                tracing::warn!("send_startup_to_target: image not found at {} — sending text only", img);
                bot.send_message(chat_str.clone(), startup_msg.to_string()).await
            }
        } else {
            bot.send_message(chat_str.clone(), startup_msg.to_string()).await
        }
    };

    match send_result {
        Ok(_) => tracing::info!("startup message sent to {}", target_raw),
        Err(e) => tracing::error!("failed to send startup message to {}: {}", target_raw, e),
    }

    // small pause if you plan to do more startup actions
    let _ = sleep(Duration::from_millis(300)).await;
}

fn read_state() -> Value {
    match fs::read_to_string(STATE_PATH) {
        Ok(s) => serde_json::from_str(&s).unwrap_or_else(|_| json!({})),
        Err(_) => json!({}),
    }
}

fn write_json_atomic(path: &str, v: &Value) -> anyhow::Result<()> {
    let parent = Path::new(path).parent().unwrap();
    fs::create_dir_all(parent).ok();
    let tmp_path = format!("{path}.tmp");
    let data = serde_json::to_vec_pretty(v)?;
    {
        let mut f = fs::File::create(&tmp_path)?;
        f.write_all(&data)?;
        let _ = f.sync_all();
    }
    fs::rename(&tmp_path, path)?;
    Ok(())
}

fn is_paid(st: &Value) -> bool {
    st.get("paid").and_then(|b| b.as_bool()).unwrap_or(false)
}
fn is_paid_forced(st: &Value) -> bool {
    st.get("paid_forced").and_then(|b| b.as_bool()).unwrap_or(false)
}

// Normalize "paid" based on payments mode and presence of a confirmed invoice
fn normalize_paid_state(st: &mut Value, pay: &payments::Payments) {
    if !pay.enabled() {
        st["paid"] = Value::from(true);
        st["paid_forced"] = Value::from(true);
        return;
    }
    if is_paid_forced(st) {
        st["paid"] = Value::from(false);
        st["paid_forced"] = Value::from(false);
    }
    let has_invoice = st.get("paid_invoice_id").and_then(|v| v.as_i64()).is_some();
    if is_paid(st) && !has_invoice {
        st["paid"] = Value::from(false);
    }
}

// ===== Read-only catalog from chats_export.csv =====

const PAGE_SIZE: usize = 10;

fn type_icon(t: &str) -> &'static str {
    match t {
        "channel" => "📣",
        "supergroup" => "👥",
        "group" => "👥",
        "private" => "👤",
        "bot" => "🤖",
        _ => "•",
    }
}

// Returns Vec of (id, title, ctype)
fn load_catalog() -> Vec<(i64, String, String)> {
    let Ok(text) = fs::read_to_string(CHATS_EXPORT_PATH) else { return Vec::new() };
    let mut out = Vec::new();
    for (lineno, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') { continue; }
        if lineno == 0 && line.to_ascii_lowercase().starts_with("type;") { continue; }
        let mut parts = line.splitn(4, ';').map(|s| s.trim());
        let typ = parts.next().unwrap_or_default().to_ascii_lowercase();
        let id_str = parts.next().unwrap_or_default();
        let title = parts.next().unwrap_or_default();
        let Ok(id) = id_str.parse::<i64>() else { continue };
        if title.is_empty() { continue; }
        out.push((id, title.to_string(), typ));
    }
    out.sort_by(|a, b| a.1.to_lowercase().cmp(&b.1.to_lowercase()));
    out
}

async fn show_catalog(bot: &Bot, chat_id: ChatId, _page: usize) {
    let items = load_catalog();
    if items.is_empty() {
        let _ = bot
            .send_message(chat_id, "В файле src/chats_export.csv не найдено чатов")
            .await;
        return;
    }

    // Send catalog as plain text (no buttons)
    let mut body = String::from("Доступные чаты:\n\n");
    for (_id, title, ctype) in items.iter().take(1000) {
        body.push_str(&format!("{} {}\n", type_icon(ctype), title));
    }
    let _ = bot.send_message(chat_id, body).await;
}

// Backward-compat helper used by greet(): show titles from chats_export.csv.
async fn send_chat_titles(bot: &Bot, chat_id: ChatId) {
    show_catalog(bot, chat_id, 1).await;
}

// ===== Flows (no notifications) =====

fn now_ts() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}
fn subscription_days() -> i64 {
    std::env::var("SUBSCRIPTION_DAYS").ok().and_then(|s| s.parse().ok()).filter(|&d| d > 0).unwrap_or(30)
}

// Show pay button if no active subscription; admins still see panel+catalog
async fn greet(bot: &Bot, chat_id: ChatId, _st: &Value, pay: &payments::Payments, adm: &admin::Admin) {
    // Send startup image + caption into the user's (private) chat instead of to the channel.
    // Reads STARTUP_IMG and STARTUP_CAPTION / default caption from env.
    let startup_msg = std::env::var("STARTUP_CAPTION").unwrap_or_else(|_| {
        r#"Лучшее что ты можешь сделать прямо сейчас - ДЕЙСТВОВАТЬ !

Коротко о BLENDER — множество приваток, которые я лично отбирал с 2019 года и это самый дешевый и самый качественный агрегатор который вы могли только найти.

Наш канал: t.me/blender
Поддержка: @ex_managers

🔻 Сумма всех приваток: 7394$/мес
✅ Сумма всех приваток у нас: 42$/мес "#.to_string()
    });

    let mut img_opt = std::env::var("STARTUP_IMG").ok();
    if let Some(ref s) = img_opt {
        img_opt = Some(s.trim().trim_matches('"').to_string());
    }
    // fallback: if no STARTUP_IMG env, try data/startup.jpg
    if img_opt.is_none() {
        let fb = Path::new("data").join("startup.jpg");
        if fb.exists() {
            img_opt = Some(fb.to_string_lossy().to_string());
            tracing::info!(chat = ?chat_id, startup_img = ?img_opt, "greet: using fallback STARTUP_IMG");
        } else {
            tracing::info!(chat = ?chat_id, startup_img = ?img_opt, "greet: STARTUP_IMG not set and no fallback found");
        }
    }

    tracing::info!(chat = ?chat_id, startup_img = ?img_opt, "greet: STARTUP_IMG env value");

    // Prepare admin keyboard once
    let kb = adm.public_keyboard();

    if let Some(ref img_raw) = img_opt {
        let img = img_raw.trim();

        // URL case
        if img.starts_with("http://") || img.starts_with("https://") {
            match url::Url::parse(img) {
                Ok(url) => {
                    match bot
                        .send_photo(chat_id, InputFile::url(url))
                        .caption(startup_msg.clone())
                        .reply_markup(kb.clone())
                        .await
                    {
                        Ok(_) => tracing::info!(chat = ?chat_id, "greet: sent remote photo+keyboard"),
                        Err(e) => tracing::error!(chat = ?chat_id, error = ?e, "greet: failed to send remote photo+keyboard"),
                    }
                }
                Err(e) => {
                    tracing::warn!(chat = ?chat_id, error = ?e, img = %img, "greet: invalid STARTUP_IMG URL; sending text+keyboard instead");
                    match bot
                        .send_message(chat_id, startup_msg.clone())
                        .reply_markup(kb.clone())
                        .await
                    {
                        Ok(_) => tracing::info!(chat = ?chat_id, "greet: sent text+keyboard (invalid URL)"),
                        Err(e) => tracing::error!(chat = ?chat_id, error = ?e, "greet: failed to send text+keyboard"),
                    }
                }
            }
        } else {
            // local file — try given path, cwd-relative, and data/ fallback
            let p1 = Path::new(img).to_path_buf();
            let p2 = env::current_dir().map(|d| d.join(img)).unwrap_or_else(|_| p1.clone());
            let p3 = Path::new("data").join(img);
            let chosen = if p1.exists() {
                p1
            } else if p2.exists() {
                p2
            } else if p3.exists() {
                p3
            } else {
                PathBuf::new()
            };

            if !chosen.as_os_str().is_empty() {
                let chosen_s = chosen.to_string_lossy().to_string();
                match bot
                    .send_photo(chat_id, InputFile::file(chosen_s.clone()))
                    .caption(startup_msg.clone())
                    .reply_markup(kb.clone())
                    .await
                {
                    Ok(_) => tracing::info!(chat = ?chat_id, path = %chosen_s, "greet: sent local photo+keyboard"),
                    Err(e) => tracing::error!(chat = ?chat_id, path = %chosen_s, error = ?e, "greet: failed to send local photo+keyboard"),
                }
            } else {
                tracing::warn!(chat = ?chat_id, img = %img, "greet: STARTUP_IMG not found; sending text+keyboard instead");
                match bot
                    .send_message(chat_id, startup_msg.clone())
                    .reply_markup(kb.clone())
                    .await
                {
                    Ok(_) => tracing::info!(chat = ?chat_id, "greet: sent text+keyboard (no image)"),
                    Err(e) => tracing::error!(chat = ?chat_id, error = ?e, "greet: failed to send text+keyboard"),
                }
            }
        }
    } else {
        match bot
            .send_message(chat_id, startup_msg.clone())
            .reply_markup(kb.clone())
            .await
        {
            Ok(_) => tracing::info!(chat = ?chat_id, "greet: sent text+keyboard"),
            Err(e) => tracing::error!(chat = ?chat_id, error = ?e, "greet: failed to send text+keyboard"),
        }
    }

    // After the greeting, send the titles from chats_export.csv (if present)
    send_chat_titles(bot, chat_id).await;

    // delete the previous single-dot public keyboard marker so the greeting message stays last
    {
        let mut st = read_state();
        let key = chat_id.0.to_string();
        if let Some(existing_id) = st["public_keyboard_msgs"].get(&key).and_then(|v| v.as_i64()) {
            if let Err(e) = bot
                .delete_message(chat_id, teloxide::types::MessageId(existing_id as i32))
                .await
            {
                tracing::warn!(chat = ?chat_id, error = ?e, "failed to delete old public keyboard marker msg_id={}", existing_id);
            } else {
                // remove from state and persist
                if let Some(obj) = st.as_object_mut() {
                    if let Some(pub_msgs) = obj.get_mut("public_keyboard_msgs").and_then(|v| v.as_object_mut()) {
                        pub_msgs.remove(&key);
                    }
                }
                if let Err(e) = write_json_atomic(STATE_PATH, &st) {
                    tracing::warn!(error = ?e, "failed to persist state after removing public keyboard marker");
                }
            }
        }
    }

    // Ensure a persistent admin keyboard is present for this chat (will create or update a dedicated message).
    ensure_persistent_admin_keyboard(bot, chat_id, adm).await;

    // Admins bypass paywall
    if adm.is_authed(chat_id.0).await {
        let _ = bot.send_message(chat_id, "Панель администратора:").reply_markup(adm.panel_keyboard()).await;
        crate::catalog::show_catalog(bot, chat_id, 1).await;
        return;
    }

    if pay.enabled() {
        let active = channels::is_subscription_active(chat_id.0, now_ts()).unwrap_or(false);
        if active {
            crate::catalog::show_catalog(bot, chat_id, 1).await;
        } else {
            // Changed: immediately create invoice and show CryptoBot URL
            start_payment_flow(bot, chat_id, pay, adm).await;
        }
    } else {
        crate::catalog::show_catalog(bot, chat_id, 1).await;
    }
}

// Start payment flow by creating an invoice and sending a URL button + check button
async fn start_payment_flow(bot: &Bot, chat_id: ChatId, pay: &payments::Payments, adm: &admin::Admin) {
     if !pay.enabled() {
         let _ = bot.send_message(chat_id, "Платежи отключены.").await;
         return;
     }
     match pay.create_invoice(None).await {
         Ok(inv) => {
             let mut text = "Оплатите счёт через CryptoBot, затем нажмите «Я оплатил — проверить».".to_string();
             if let Some(url) = inv.pay_url.as_ref() {
                 text = format!("{text}\n\nСсылка для оплаты: {url}");
             }
             let kb = InlineKeyboardMarkup::new(vec![
                 vec![InlineKeyboardButton::callback(
                     "Я оплатил — проверить ✅",
                     format!("pay:check:{}", inv.invoice_id),
                 )],
             ]);
             let _ = bot.send_message(chat_id, text).reply_markup(kb).await;
             // Also (re)send the admin public keyboard so admins see the "I'm admin" button
             // even after the payment/invoice message is shown.
             if adm.enabled() {
                 send_public_keyboard(bot, chat_id, adm).await;
             }
         }
         Err(e) => {
             let _ = bot
                 .send_message(chat_id, format!("Не удалось создать счёт: {e}"))
                 .await;
         }
     }
 }

// Message handler: routes incoming messages to greet()
async fn handle_message(
    bot: Bot,
    msg: Message,
    pay: &payments::Payments,
    adm: &admin::Admin,
) -> anyhow::Result<()> {
    // --- ignore service messages (new member joins, leaves, pins) ---
    if msg.new_chat_members().is_some() || msg.left_chat_member().is_some() || msg.pinned_message().is_some()
    {
        // do not run the normal greeting/flow for service messages
        return Ok(());
    }

    // determine whether this is a private chat (only send greet in private)
    let is_private = matches!(msg.chat.kind, teloxide::types::ChatKind::Private(_));

    // --- forwarding: configure source/target via env (or hardcode) ---
    if let Ok(src_str) = std::env::var("FORWARD_SOURCE") {
        if let Ok(dst_str) = std::env::var("FORWARD_TARGET") {
            if let (Ok(src_id), Ok(dst_id)) = (src_str.parse::<i64>(), dst_str.parse::<i64>()) {
                // try forward and short-circuit if forwarded
                if let Ok(true) = forward::try_forward(&bot, &msg, src_id, dst_id).await {
                    return Ok(());
                }
            }
        }
    }
    // --- end forwarding ---

    if let Some(t) = msg.text() {
        // quick /start reply for testing bot responsiveness
        if t.trim().starts_with("/start") {
            let username = msg.from().and_then(|u| u.username.clone());
            // ADD: debug logging
            println!("DEBUG /start: username={:?}, has_free_access={}", username, adm.has_free_access(username.as_deref(), msg.chat.id.0));
            if adm.has_free_access(username.as_deref(), msg.chat.id.0) {
                // grant free access: mark subscription, write to subs.json, send folders message
                let expires = now_ts() + subscription_days() * 86_400;
                let _ = channels::set_paid_until(msg.chat.id.0, expires);
                // write to subs.json
                let user_id = msg.chat.id.0 as i64;
                if let Err(e) = pay.add_or_renew_subscriber(user_id, None, 1) {
                    eprintln!("free access: add_or_renew_subscriber failed: {:?}", e);
                }
                if let Err(e) = pay.append_subscription_record(user_id, 1) {
                    eprintln!("free access: append_subscription_record failed: {:?}", e);
                }
                // NEW: send congratulatory message
                let _ = bot.send_message(msg.chat.id, "Поздравляем, вам предоставлен бесплатный доступ!").await;
                // send folders message
                let aktiv_url = reqwest::Url::parse("https://t.me/addlist/Q3mkHDAfwjYyYjU0").ok();
                let ludiki_url = reqwest::Url::parse("https://t.me/addlist/TyvbTgRFp5QwY2Y0").ok();
                let farm_url = reqwest::Url::parse("https://t.me/addlist/qzsI2WN7hXExNTdk").ok();
                let other_url = reqwest::Url::parse("https://t.me/addlist/Gy2SNd_HDPNjNmY0").ok();

                let make_btn = |label: &str, url_opt: Option<reqwest::Url>, cb: &str| {
                    url_opt
                        .map(|u| InlineKeyboardButton::url(label.to_string(), u))
                        .unwrap_or_else(|| InlineKeyboardButton::callback(label.to_string(), cb.to_string()))
                };

                let mut rows = Vec::new();
                rows.push(vec![
                    make_btn("АКТИВНОСТИ +", aktiv_url, "show_folder:aktivnosti"),
                    make_btn("ЛУДИКИ", ludiki_url, "show_folder:ludiki"),
                ]);
                rows.push(vec![
                    make_btn("Фармилка", farm_url, "show_folder:farmilka"),
                    make_btn("Прочее", other_url, "show_folder:other"),
                ]);
                let kb = InlineKeyboardMarkup::new(rows);

                let _ = bot.send_message(msg.chat.id, "Папки с каналами:").reply_markup(kb).await;
                // send invite
                send_channel_invite(&bot, msg.chat.id).await;
            } else {
                // plain user: send startup message with pay button
                send_startup_to_user(&bot, msg.chat.id).await;
            }
            return Ok(());
        }
        if t.trim().eq_ignore_ascii_case("/ping") {
            bot.send_message(msg.chat.id, "pong").await?;
            return Ok(());
        }

        // NEW: handle /paid month promo
        if t.trim() == "/paid month" {
            let user_id = msg.chat.id.0;
            let mut st = read_state();
            if has_used_promo(&st, user_id) {
                let _ = bot.send_message(msg.chat.id, "Вы уже использовали этот промо.").await;
                return Ok(());
            }
            set_promo_pending(&mut st, user_id, true);
            if let Err(e) = write_json_atomic(STATE_PATH, &st) {
                eprintln!("failed to set promo pending: {:?}", e);
            }
            let _ = bot.send_message(msg.chat.id, "Введите username для получения 1 месяца доступа.").await;
            return Ok(());
        }

        // NEW: if promo pending, treat as username input
        let user_id = msg.chat.id.0;
        let mut st = read_state();
        if is_promo_pending(&st, user_id) {
            set_promo_pending(&mut st, user_id, false); // clear pending
            let usernames = load_first_month_usernames();
            let input_username = t.trim().to_string();
            if usernames.contains(&input_username) {
                // grant 1 month
                let expires = now_ts() + subscription_days() * 86_400; // 30 days
                let _ = channels::set_paid_until(user_id, expires);
                // write to subs.json
                if let Err(e) = pay.add_or_renew_subscriber(user_id, None, 1) {
                    eprintln!("promo: add_or_renew_subscriber failed: {:?}", e);
                }
                if let Err(e) = pay.append_subscription_record(user_id, 1) {
                    eprintln!("promo: append_subscription_record failed: {:?}", e);
                }
                // mark as used
                set_used_promo(&mut st, user_id);
                if let Err(e) = write_json_atomic(STATE_PATH, &st) {
                    eprintln!("failed to set used promo: {:?}", e);
                }
                // send folders message
                let aktiv_url = reqwest::Url::parse("https://t.me/addlist/Q3mkHDAfwjYyYjU0").ok();
                let ludiki_url = reqwest::Url::parse("https://t.me/addlist/TyvbTgRFp5QwY2Y0").ok();
                let farm_url = reqwest::Url::parse("https://t.me/addlist/qzsI2WN7hXExNTdk").ok();
                let other_url = reqwest::Url::parse("https://t.me/addlist/Gy2SNd_HDPNjNmY0").ok();

                let make_btn = |label: &str, url_opt: Option<reqwest::Url>, cb: &str| {
                    url_opt
                        .map(|u| InlineKeyboardButton::url(label.to_string(), u))
                        .unwrap_or_else(|| InlineKeyboardButton::callback(label.to_string(), cb.to_string()))
                };

                let mut rows = Vec::new();
                rows.push(vec![
                    make_btn("АКТИВНОСТИ +", aktiv_url, "show_folder:aktivnosti"),
                    make_btn("ЛУДИКИ", ludiki_url, "show_folder:ludiki"),
                ]);
                rows.push(vec![
                    make_btn("Фармилка", farm_url, "show_folder:farmilka"),
                    make_btn("Прочее", other_url, "show_folder:other"),
                ]);
                let kb = InlineKeyboardMarkup::new(rows);

                let _ = bot.send_message(msg.chat.id, "Папки с каналами:").reply_markup(kb).await;
                let _ = bot.send_message(msg.chat.id, "Промо активировано! Доступ предоставлен на 1 месяц.").await;
            } else {
                let _ = bot.send_message(msg.chat.id, "Неверный username. Промо не активировано.").await;
            }
            if let Err(e) = write_json_atomic(STATE_PATH, &st) {
                eprintln!("failed to clear promo pending: {:?}", e);
            }
            return Ok(());
        }

        // NEW: handle admin pending actions
        let user_id = msg.chat.id.0;
        let mut st = read_state();
        if is_admin_pending(&st, user_id, "add_free") {
            set_admin_pending(&mut st, user_id, None);
            let username = t.trim().trim_start_matches('@').to_lowercase();  // CHANGED: add .to_lowercase() for case-insensitivity
            let mut free_users = load_free_users();
            if free_users.insert(username.clone()) {
                if let Err(e) = save_free_users(&free_users) {
                    eprintln!("failed to save free users: {:?}", e);
                    let _ = bot.send_message(msg.chat.id, "Ошибка сохранения.").await;
                } else {
                    let _ = bot.send_message(msg.chat.id, format!("Пользователь {} добавлен в бесплатный доступ.", username)).await;
                }
            } else {
                let _ = bot.send_message(msg.chat.id, "Пользователь уже в списке.").await;
            }
            if let Err(e) = write_json_atomic(STATE_PATH, &st) {
                eprintln!("failed to clear admin pending: {:?}", e);
            }
            return Ok(());
        }
        if is_admin_pending(&st, user_id, "remove_free") {
            set_admin_pending(&mut st, user_id, None);
            let username = t.trim().trim_start_matches('@').to_lowercase();  // CHANGED: add .to_lowercase() for case-insensitivity
            let mut free_users = load_free_users();
            if free_users.remove(&username) {
                if let Err(e) = save_free_users(&free_users) {
                    eprintln!("failed to save free users: {:?}", e);
                    let _ = bot.send_message(msg.chat.id, "Ошибка сохранения.").await;
                } else {
                    let _ = bot.send_message(msg.chat.id, format!("Пользователь {} удалён из бесплатного доступа.", username)).await;
                }
            } else {
                let _ = bot.send_message(msg.chat.id, "Пользователь не найден в списке.").await;
            }
            if let Err(e) = write_json_atomic(STATE_PATH, &st) {
                eprintln!("failed to clear admin pending: {:?}", e);
            }
            return Ok(());
        }
    }

    // Admin flow can consume messages (password prompts etc.)
    if adm.on_message(&bot, &msg).await {
        // ensure the persistent "I'm admin" keyboard exists and remains last
        ensure_persistent_public_keyboard(&bot, msg.chat.id, &adm).await;
        // NEW: Show admin panel keyboard after login
        let _ = bot.send_message(msg.chat.id, "Панель администратора:").reply_markup(adm.panel_keyboard()).await;
        return Ok::<(), anyhow::Error>(());
    }

    // Grant free access if user is in admin free list
    let username = msg.from().and_then(|u| u.username.clone());
    if adm.has_free_access(username.as_deref(), msg.chat.id.0) {
        // mark a monthly subscription for them
        let _ = channels::set_paid_until(msg.chat.id.0, now_ts() + subscription_days() * 86_400);
        // send dynamic invite link
        //send_channel_invite(&bot, msg.chat.id).await;
        // show catalog as plain text
        show_catalog(&bot, msg.chat.id, 1).await;
        return Ok(());
    }

    // Only run the greet / payment flow in private chats.
    if is_private {
        // REMOVED: let st = serde_json::json!({}); // if you still use state, keep your existing read_state
        // REMOVED: greet(&bot, msg.chat.id, &st, pay, adm).await;
        // Now: do nothing in private chats unless a specific command is matched above
    } else {
        // in groups: do nothing by default (prevents sending startup/greet on new member join)
    }

    Ok(())
}

// Load first_month.json as a set of usernames (for fast lookup)
fn load_first_month_usernames() -> HashSet<String> {
    let path = "src/first_month.json";  // FIXED: was "first_month.json", now points to src/where the file is
    match fs::read_to_string(path) {
        Ok(s) => {
            if let Ok(Value::Array(arr)) = serde_json::from_str(&s) {
                arr.into_iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect()
            } else {
                HashSet::new()
            }
        }
        Err(_) => HashSet::new(),
    }
}

// Load free users from src/free.json as a set of usernames
pub fn load_free_users() -> HashSet<String> {
    let path = "src/free.json";  // CHANGED: from "data/free_users.json" to "src/free.json" for consistency with first_month.json
    match fs::read_to_string(path) {
        Ok(s) => {
            if let Ok(Value::Array(arr)) = serde_json::from_str(&s) {
                arr.into_iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect()
            } else {
                HashSet::new()
            }
        }
        Err(_) => HashSet::new(),
    }
}

// Save free users to src/free.json
fn save_free_users(free_users: &HashSet<String>) -> anyhow::Result<()> {
    let path = "src/free.json";  // CHANGED: from "data/free_users.json" to "src/free.json" for consistency
    let arr: Vec<Value> = free_users.iter().map(|s| Value::String(s.clone())).collect();
    let data = serde_json::to_string_pretty(&arr)?;
    fs::write(path, data)?;
    Ok(())
}

// Check if user has an admin pending action
fn is_admin_pending(st: &Value, user_id: i64, action: &str) -> bool {
    st.get("admin_pending")
        .and_then(|v| v.as_object())
        .and_then(|obj| obj.get(&user_id.to_string()))
        .and_then(|v| v.as_str())
        .map(|s| s == action)
        .unwrap_or(false)
}

// Set admin pending action for user
fn set_admin_pending(st: &mut Value, user_id: i64, action: Option<&str>) {
    if !st.get("admin_pending").is_some() {
        st["admin_pending"] = json!({});
    }
    if let Some(obj) = st["admin_pending"].as_object_mut() {
        if let Some(act) = action {
            obj.insert(user_id.to_string(), Value::String(act.to_string()));
        } else {
            obj.remove(&user_id.to_string());
        }
    }
}

// Check if user has already used the promo
fn has_used_promo(st: &Value, user_id: i64) -> bool {
    st.get("used_promo")
        .and_then(|v| v.as_object())
        .and_then(|obj| obj.get(&user_id.to_string()))
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}

// Set promo as used
fn set_used_promo(st: &mut Value, user_id: i64) {
    if !st.get("used_promo").is_some() {
        st["used_promo"] = json!({});
    }
    if let Some(obj) = st["used_promo"].as_object_mut() {
        obj.insert(user_id.to_string(), Value::Bool(true));
    }
}

// Check if user is pending promo input
fn is_promo_pending(st: &Value, user_id: i64) -> bool {
    st.get("promo_pending")
        .and_then(|v| v.as_object())
        .and_then(|obj| obj.get(&user_id.to_string()))
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}

// Set promo pending
fn set_promo_pending(st: &mut Value, user_id: i64, pending: bool) {
    if !st.get("promo_pending").is_some() {
        st["promo_pending"] = json!({});
    }
    if let Some(obj) = st["promo_pending"].as_object_mut() {
        obj.insert(user_id.to_string(), Value::Bool(pending));
    }
}

// dynamic invite creation (preferred). Fallback to CHANNEL_INVITE_LINK env var.
// Requires bot to be admin in the channel (with permission to invite/create invite links).
async fn send_channel_invite(bot: &Bot, to_chat: ChatId) {
    // Try CHANNEL_ID env first
    if let Ok(chan_str) = std::env::var("CHANNEL_ID") {
        if let Ok(chan_id) = chan_str.parse::<i64>() {
            let channel = ChatId(chan_id);

            // Try to create a single-use invite link via the typed Bot API
            if let Ok(inv) = bot.create_chat_invite_link(channel).member_limit(1).await {
                let link = inv.invite_link;
                let _ = bot
                    .send_message(to_chat, format!("Доступ предоставлен — присоединяйтесь к каналу: {link}"))
                    .await;
                return;
            }

            // Fallback: export the primary invite link
            if let Ok(link) = bot.export_chat_invite_link(channel).await {
                let _ = bot
                    .send_message(to_chat, format!("Доступ предоставлен — присоединяйтесь к каналу: {link}"))
                    .await;
                return;
            }
        }
    }
// Last resort: static invite link from env
    match std::env::var("CHANNEL_INVITE_LINK") {
        Ok(link) if !link.is_empty() => {
            let _ = bot
                .send_message(to_chat, format!("Доступ предоставлен — присоединяйтесь к каналу: {link}"))
                .await;
        }
        _ => {
            let _ = bot
                .send_message(
                    to_chat,
                    "",
                )
                .await;
        }
    }
}





    

// Handle payment-related callback queries
async fn handle_pay_callbacks(bot: &Bot, q: &CallbackQuery, pay: &payments::Payments, adm: &admin::Admin) {
    let Some(data) = q.data.clone() else { return };

    if let Some(rest) = data.strip_prefix("pay:check:") {
        let invoice_id = rest.parse::<i64>().unwrap_or_default();
        if invoice_id == 0 {
            let _ = bot
                .answer_callback_query(q.id.clone())
                .text("Неверный счёт.")
                .show_alert(true)
                .await;
            return;
        }

        match pay.get_invoice(invoice_id).await {
            Ok(Some(inv)) if inv.status == "paid" => {
                if let Some(msg) = &q.message {
                    // grant subscription: now + SUBSCRIPTION_DAYS
                    let expires = now_ts() + subscription_days() * 86_400;
                    let _ = channels::set_paid_until(msg.chat.id.0, expires);

                    // NEW: persist to payments API (subscribers + ledger)
                    let payer_id = msg.chat.id.0 as i64;
                    if let Err(e) = pay.add_or_renew_subscriber(payer_id, None, 1) {
                        eprintln!("payments: add_or_renew_subscriber failed for {}: {:?}", payer_id, e);
                    }
                    if let Err(e) = pay.append_subscription_record(payer_id, 1) {
                        eprintln!("payments: append_subscription_record failed for {}: {:?}", payer_id, e);
                    }

                    // UNBAN: attempt to unban the payer from chats2.csv
                    let user_id = msg.chat.id.0;
                    if let Err(e) = pay.unban_user_from_chats(bot, user_id, Some("chats2.csv")).await {
                        tracing::warn!(error = ?e, "failed to unban user {} after payment", user_id);
                    }

                    // NEW: send the folders message with buttons (moved from handle_check_callback)
                    let aktiv_url = reqwest::Url::parse("https://t.me/addlist/Q3mkHDAfwjYyYjU0").ok();
                    let ludiki_url = reqwest::Url::parse("https://t.me/addlist/TyvbTgRFp5QwY2Y0").ok();
                    let farm_url = reqwest::Url::parse("https://t.me/addlist/qzsI2WN7hXExNTdk").ok();
                    let other_url = reqwest::Url::parse("https://t.me/addlist/Gy2SNd_HDPNjNmY0").ok();

                    let make_btn = |label: &str, url_opt: Option<reqwest::Url>, cb: &str| {
                        url_opt
                            .map(|u| InlineKeyboardButton::url(label.to_string(), u))
                            .unwrap_or_else(|| InlineKeyboardButton::callback(label.to_string(), cb.to_string()))
                    };

                    let mut rows = Vec::new();
                    rows.push(vec![
                        make_btn("АКТИВНОСТИ +", aktiv_url, "show_folder:aktivnosti"),
                        make_btn("ЛУДИКИ", ludiki_url, "show_folder:ludiki"),
                    ]);
                    rows.push(vec![
                        make_btn("Фармилка", farm_url, "show_folder:farmilka"),
                        make_btn("Прочее", other_url, "show_folder:other"),
                    ]);
                    let kb = InlineKeyboardMarkup::new(rows);

                    let _ = bot.send_message(ChatId(user_id), "Папки с каналами:").reply_markup(kb).await;

                    let _ = bot
                        .edit_message_text(
                            msg.chat.id,
                            msg.id,
                            "✅ Платёж подтверждён. Доступ предоставлен на 30 дней.",
                        )
                        .await;

                    // Send admin panel message and ensure the public "I'm admin" keyboard is present
                    // REMOVED: Panel (visible) so admins can start working immediately
                    // REMOVED: if let Err(e) = bot
                    // REMOVED:     .send_message(msg.chat.id, "Панель администратора:")
                    // REMOVED:     .reply_markup(adm.panel_keyboard())
                    // REMOVED:     .await
                    // REMOVED: {
                    // REMOVED:     tracing::warn!(chat = ?msg.chat.id, error = ?e, "failed to send admin panel after payment");
                    // REMOVED: }
                    // Ensure persistent public keyboard with "I'm admin" button
                    ensure_persistent_public_keyboard(bot, msg.chat.id, adm).await;

                  // Send dynamic invite to the specific group for paid users
                   // Replace -1002988111419 with another id if needed
                   
                    // If the payer is an admin, show admin panel + catalog here
                    if adm.is_authed(msg.chat.id.0).await {
                        crate::catalog::show_catalog(bot, msg.chat.id, 1).await;
                    } else {
                        // If the user is NOT an admin, send the invite link (dynamic)
                        //send_channel_invite(bot, msg.chat.id).await;
                    }
                }
                let _ = bot
                    .answer_callback_query(q.id.clone())
                    .text("Платёж подтверждён ✅")
                    .await;
            }
            Ok(Some(inv)) if inv.status == "active" => {
                let _ = bot
                    .answer_callback_query(q.id.clone())
                    .text("Платёж не завершён. Завершите оплату и попробуйте ещё раз.")
                    .show_alert(true)
                    .await;
            }
            Ok(Some(inv)) => {
                let _ = bot
                    .answer_callback_query(q.id.clone())
                    .text(format!("Статус: {}", inv.status))
                    .show_alert(true)
                    .await;
            }
            Ok(None) => {
                let _ = bot
                    .answer_callback_query(q.id.clone())
                    .text("Счёт не найден.")
                    .show_alert(true)
                    .await;
            }
            Err(e) => {
                let _ = bot
                    .answer_callback_query(q.id.clone())
                    .text(format!("Ошибка проверки: {e}"))
                    .show_alert(true)
                    .await;
            }
        }
        return;
    }

    if data == "pay:start" {
        if let Some(msg) = &q.message {
            start_payment_flow(bot, msg.chat.id, &pay, &adm).await;
        }
        let _ = bot.answer_callback_query(q.id.clone()).await;
        return;
    }

    let _ = bot.answer_callback_query(q.id.clone()).await;
}

// helper
async fn handle_pay_callback(
    bot: &Bot,
    q: CallbackQuery,
    pay: payments::Payments,
    adm: admin::Admin,
) {
    let qid = q.id.clone();
    let data = match q.data.clone() {
        Some(d) => d,
        None => {
            let _ = bot.answer_callback_query(qid).await;
            return;
        }
    };

    tracing::info!(callback_data = %data, chat = ?q.message.as_ref().map(|m| m.chat.id), "callback received");

    // Ack quickly so UI is responsive
    let _ = bot.answer_callback_query(qid.clone()).await;

    if data == "pay:start" {
        if let Some(msg) = &q.message {
            start_payment_flow(bot, msg.chat.id, &pay, &adm).await;
        }
        return;
    }

    if let Some(id_str) = data.strip_prefix("pay:check:") {
        match id_str.parse::<i64>() {
            Ok(invoice_id) if invoice_id != 0 => {
                tracing::info!(invoice_id, "checking invoice");
                match pay.get_invoice(invoice_id).await {
                    Ok(Some(inv)) => {
                        tracing::info!(invoice_id, status = %inv.status, "invoice fetched");
                        match inv.status.as_str() {
                            "paid" => {
                                if let Some(msg) = &q.message {
                                    let expires = now_ts() + subscription_days() * 86_400;
                                    let _ = channels::set_paid_until(msg.chat.id.0, expires);
                                    let _ = bot
                                        .edit_message_text(msg.chat.id, msg.id, "✅ Payment succeeded. Access granted.")
                                        .await;
                                    // Send admin panel (visible) and ensure "I'm admin" keyboard exists
                                    if let Err(e) = bot
                                        .send_message(msg.chat.id, "Панель администратора:")
                                        .reply_markup(adm.panel_keyboard())
                                        .await
                                    {
                                        tracing::warn!(chat = ?msg.chat.id, error = ?e, "failed to send admin panel after payment");
                                    }
                                    ensure_persistent_public_keyboard(bot, msg.chat.id, &adm).await;
                                  // Send dynamic invite to the specific group for paid users
                                  
                                    if !adm.is_authed(msg.chat.id.0).await {
                                        send_channel_invite(bot, msg.chat.id).await;
                                    }
                                }
                                let _ = bot.answer_callback_query(qid).text("Payment confirmed ✅").await;
                            }
                            "active" => {
                                let _ = bot
                                    .answer_callback_query(qid)
                                    .text("Still unpaid. Complete the payment and try again.")
                                    .show_alert(true)
                                    .await;
                            }
                            other => {
                                let _ = bot
                                    .answer_callback_query(qid)
                                    .text(format!("Status: {}", other))
                                    .show_alert(true)
                                    .await;
                            }
                        }
                    }
                    Ok(None) => {
                        let _ = bot
                            .answer_callback_query(qid)
                            .text("Invoice not found.")
                            .show_alert(true)
                            .await;
                    }
                    Err(e) => {
                        tracing::error!(error = ?e, "get_invoice failed");
                        let _ = bot
                            .answer_callback_query(qid)
                            .text(format!("Check failed: {}", e))
                            .show_alert(true)
                            .await;
                    }
                }
            }
            _ => {
                let _ = bot
                    .answer_callback_query(qid)
                    .text("Invalid invoice id")
                    .show_alert(true)
                    .await;
            }
        }
        return;
    }

    // fallback ack (already acked above, but keep for safety)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenv().ok();
    ensure_data_dir().ok();  // ensure data/ dir exists for state.json writes
    let bot = Bot::from_env();
    // debug: print bot identity at startup
    match bot.get_me().await {
        Ok(me) => println!("Bot running as @{} id={}", me.username.clone().unwrap_or_default(), me.id),
        Err(e) => println!("Bot getMe failed: {:?}", e),
    }
    let payments = Arc::new(Payments::new_from_env()?);
    // create admin instance (adjust constructor if your Admin uses a different name)
    let adm = admin::Admin::new_from_env();
     // lightweight global message logger (prints every incoming message to console)
     let debug_msg_logger = Update::filter_message()
         .endpoint(|_bot: Bot, msg: teloxide::types::Message| async move {
             if let Some(text) = msg.text() {
                 println!("DEBUG MSG from chat={} user={:?}: {}", msg.chat.id.0, msg.from().and_then(|u| u.username.clone()), text);
             } else {
                 println!("DEBUG MSG non-text from chat={}", msg.chat.id.0);
             }
             respond(())
         });

         let payments = Arc::new(Payments::new_from_env()?);
    // create admin instance (adjust constructor if your Admin uses a different name)
    let adm = admin::Admin::new_from_env();

    // Manual prune for testing
    if let Err(e) = payments.prune_and_ban_expired_subscribers(&bot, Some("chats2.csv")).await {
        eprintln!("Manual prune failed: {:?}", e);
    } else {
        println!("Manual prune done");
    }
 
    // unified callback handler for any pay:* callback
    let cb_handler = Update::filter_callback_query()
        .filter(|q: CallbackQuery| q.data.as_deref().map_or(false, |d| d.starts_with("pay:")))
        .endpoint({
            let payments = payments.clone();
            let adm = adm.clone();
            move |bot: Bot, q: CallbackQuery| {
                let payments = payments.clone();
                let adm = adm.clone();
                async move {
                    handle_pay_callbacks(&bot, &q, &payments, &adm).await;
                    respond(())
                }
            }
        });
 
    // full message handler: route all messages to your existing handle_message function
    let full_msg_handler = Update::filter_message().endpoint({
        let payments = payments.clone();
        let adm = adm.clone();
        move |bot: Bot, msg: teloxide::types::Message| {
            let payments = payments.clone();
            let adm = adm.clone();
            async move {
                // call your existing handler; ignore and log errors so dispatcher keeps running
                if let Err(e) = handle_message(bot, msg, &*payments, &adm).await {
                    eprintln!("handle_message error: {:?}", e);
                }
                respond(())
            }
        }
    });
 
    // new callback handler for "show:channels": display the catalog and show a "pay" button under the list
    let show_channels_cb = Update::filter_callback_query()
        .filter(|q: CallbackQuery| q.data.as_deref().map_or(false, |d| d == "show:channels"))
        .endpoint({
            let payments = payments.clone();
            let adm = adm.clone();
            move |bot: Bot, q: CallbackQuery| {
                let payments = payments.clone();
                let adm = adm.clone();
                async move {
                    // ack callback so UI doesn't spin
                    let _ = bot.answer_callback_query(q.id.clone()).await;

                    // determine chat to reply in: prefer message.chat if available, else user's private chat
                    let chat = if let Some(ref m) = q.message {
                        m.chat.id
                    } else {
                        ChatId(q.from.id.0 as i64)
                    };

                    // the channels list message (Russian)
                    let body = r#"Ретро-активности:

⚫️Pro Mint 
⚫️FACKBLOCK
⚫️001k
⚫️Вишня 
⚫️Фармилка | Пирожок
⚫️2TOP Squad
⚫️Crypton Prime
⚫️Coin Metrika
⚫️CRYPTUS 
⚫️20/80 Crypto Headlines

Коллеры:

⚫️D Trade ( 3333$/год )
⚫️D ( с 1к$ до 500к$ )
⚫️Слезы Сатоши
⚫️krajekis сигма impulse
⚫️Крипто Свин
⚫️Коля Флипает
⚫️BOBA
⚫️Mr.Mozart
⚫️maloletoff
⚫️Картель
⚫️ARBUZ REBORN

Прочее:

⚫️База Тейта
⚫️База Арсена Маркаряна
⚫️База по качалке
⚫️YouTube HUB - много инфы по англ. ютубу

⚫️ BLENDER club - тут мы делаем выжимки и подсвечиваем самую интересную инфу из выше перечислиных приваток, колим то в что сами заходим и делаем."#;
                    let _ = bot.send_message(chat, body).await;

                    // create invoice for payment
                    match payments.create_invoice(Some(q.from.id.0.to_string())).await {
                        Ok(inv) => {
                            // build keyboard: first row -> "Продлить подписку" (URL), second row -> "Я оплатил — проверить"
                            let mut rows = Vec::new();
                            if let Some(url_str) = inv.pay_url.as_deref() {
                                if let Ok(url) = reqwest::Url::parse(url_str) {
                                    rows.push(vec![InlineKeyboardButton::url(
                                        "Продлить подписку".to_string(),
                                        url,
                                    )]);
                                }
                            }
                            rows.push(vec![InlineKeyboardButton::callback(
                                "Я оплатил — проверить",
                                format!("pay:check:{}", inv.invoice_id),
                            )]);
                            let kb = InlineKeyboardMarkup::new(rows);

                            let mut text = "Если хотите получить доступ — оплатите счёт:".to_string();
                            if let Some(url) = inv.pay_url.as_ref() {
                                text = format!("{text}\n\nСсылка для оплаты: {url}");
                            }
                            let _ = bot.send_message(chat, text).reply_markup(kb).await;
                        }
                        Err(e) => {
                            tracing::warn!(error = ?e, "show_channels_cb: failed to create invoice; skipping invoice send");
                        }
                    }

                    respond(())
                }
            }
        });
 
    // new callback handler for "admin:*" callbacks
    let admin_cb_handler = Update::filter_callback_query()
    .filter(|q: CallbackQuery| q.data.as_deref().map_or(false, |d| d.starts_with("admin:")))
    .endpoint({
        let adm = adm.clone();
        move |bot: Bot, q: CallbackQuery| {
            let adm = adm.clone();
            async move {
                // debug log incoming admin callback
                let data = q.data.clone().unwrap_or_default();
                let uid = q.from.id.0;
                tracing::info!(user = uid, callback_data = %data, "admin callback received");

                // quick auth check here to give immediate feedback and avoid silent failures
                let authed = adm.is_authed(uid as i64).await;
                if !authed {
                    let _ = bot.answer_callback_query(q.id.clone()).text("Доступ запрещён.").await;
                    tracing::warn!(user = uid, "admin callback denied (not authed)");
                    return respond(());
                }

                // handle admin actions directly here (set pending, send prompts, show list)
                if data == "admin:add_free" {
                    let mut st = read_state();
                    set_admin_pending(&mut st, uid as i64, Some("add_free"));
                    if let Err(e) = write_json_atomic(STATE_PATH, &st) {
                        eprintln!("failed to set admin pending: {:?}", e);
                    }
                    let _ = bot.send_message(ChatId(uid as i64), "Отправьте username для ДОБАВЛЕНИЯ (с @ или без):").await;
                } else if data == "admin:remove_free" {
                    let mut st = read_state();
                    set_admin_pending(&mut st, uid as i64, Some("remove_free"));
                    if let Err(e) = write_json_atomic(STATE_PATH, &st) {
                        eprintln!("failed to set admin pending: {:?}", e);
                    }
                    let _ = bot.send_message(ChatId(uid as i64), "Отправьте username для УДАЛЕНИЯ (с @ или без):").await;
                } else if data == "admin:show_free" {
                    let free_users = load_free_users();
                    let list = if free_users.is_empty() {
                        "Пока нет пользователей с бесплатным доступом.".to_string()
                    } else {
                        free_users.iter().map(|u| format!("@{}", u)).collect::<Vec<_>>().join("\n")
                    };
                    let _ = bot.send_message(ChatId(uid as i64), format!("Бесплатные пользователи:\n{}", list)).await;
                }

                let _ = bot.answer_callback_query(q.id.clone()).await;
                respond(())
            }
        }
    });
 
    // ensure admin_cb_handler is branched into your dispatcher
    let handler = dptree::entry()
        .branch(cb_handler)
        .branch(full_msg_handler)
        .branch(debug_msg_logger)
        .branch(show_channels_cb)
        .branch(admin_cb_handler);
 
    Dispatcher::builder(bot.clone(), handler)
        .build()
        .dispatch()
        .await;
 
    Ok(())

   
}
 
// new helper: send startup message + keyboard to user
async fn send_startup_to_user(bot: &Bot, chat_id: ChatId) {
    let startup_msg = r#"Лучшее что ты можешь сделать прямо сейчас - ДЕЙСТВОВАТЬ !

Коротко о BLENDER — множество приваток, которые я лично отбирал с 2019 года и это самый дешевый и самый качественный агрегатор который вы могли только найти.

Наш канал: t.me/blender
Поддержка: @ex_managers

🔻 Сумма всех приваток: 7394$/мес
✅ Сумма всех приваток у нас: 42$/мес "#;

    // keyboard with "Список каналов" callback button
    let kb = InlineKeyboardMarkup::new(vec![vec![InlineKeyboardButton::callback(
        "Список каналов".to_string(),
        "show:channels".to_string(),
    )]]);

    // prefer data/startup.jpg if present
    let local = Path::new("data").join("startup.jpg");
    if local.exists() {
        let _ = bot
            .send_photo(chat_id, InputFile::file(local.to_string_lossy().to_string()))
            .caption(startup_msg.to_string())
            .reply_markup(kb)
            .await;
    } else {
        let _ = bot.send_message(chat_id, startup_msg.to_string()).reply_markup(kb).await;
    }
}

// Helper: send a short non-empty marker message with the public admin keyboard.
async fn send_public_keyboard(bot: &Bot, chat_id: ChatId, adm: &admin::Admin) {
    // Delegate to the ensure-function to (re)create the persistent keyboard message.
    ensure_persistent_public_keyboard(bot, chat_id, adm).await;
}

/// Ensure a persistent public keyboard message exists for chat_id.
/// Stores message id under "public_keyboard_msgs" in state file.
async fn ensure_persistent_public_keyboard(bot: &Bot, chat_id: ChatId, adm: &admin::Admin) {
    if !adm.enabled() {
        return;
    }

    let kb = adm.public_keyboard();
    let mut st = read_state();

    if !st.get("public_keyboard_msgs").is_some() {
        st["public_keyboard_msgs"] = json!({});
    }

    let key = chat_id.0.to_string();

    // Delete previous message if present (avoid duplicates)
    if let Some(existing_id) = st["public_keyboard_msgs"].get(&key).and_then(|v| v.as_i64()) {
        if let Err(e) = bot
            .delete_message(chat_id, teloxide::types::MessageId(existing_id as i32))
            .await
        {
            tracing::warn!(chat = ?chat_id, error = ?e, "failed to delete existing public keyboard msg_id={}, will recreate", existing_id);
        }
    }

    // Send a single-dot marker so the public keyboard is attached without visible helper text
    match bot
        .send_message(chat_id, ".")
        .reply_markup(kb.clone())
        .await
    {
        Ok(m) => {
            tracing::info!(chat = ?chat_id, "created persistent public keyboard msg_id={}", m.id);
            st["public_keyboard_msgs"][&key] = Value::from(m.id.0 as i64);
            if let Err(e) = write_json_atomic(STATE_PATH, &st) {
                tracing::warn!(error = ?e, "failed to persist public_keyboard_msgs state");
            }
        }
        Err(e) => {
            tracing::error!(chat = ?chat_id, error = ?e, "failed to create persistent public keyboard");
        }
    }
}

// Back-compat wrapper: keep old call sites working by delegating to public keyboard
async fn ensure_persistent_admin_keyboard(bot: &Bot, chat_id: ChatId, adm: &admin::Admin) {
    ensure_persistent_public_keyboard(bot, chat_id, adm).await;
}

    impl payments::Payments {
        /// Prune expired subscribers based on subs.json ledger, remove from both subscribers.json and subs.json,
        /// and ban them from chats listed in `chats_csv` (default "chats2.csv").
        pub async fn prune_and_ban_expired(&self, bot: &Bot, chats_csv: Option<&str>) -> anyhow::Result<Vec<(i64,i64)>> {
            // Load latest expiries from subs.json
            let latest_expiries = self.load_latest_expiries_from_subs()?;
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
    
            let mut removed = Vec::new();
            for (&user_id, &exp) in &latest_expiries {
                if exp <= now {
                    removed.push(user_id);
                }
            }
    
            if removed.is_empty() {
                return Ok(vec![]);
            }
    
            // Remove from data/subscribers.json (if present)
            let subs_path = self.subscribers_path();
            if subs_path.exists() {
                let mut subs: Vec<Subscriber> = serde_json::from_str(&fs::read_to_string(&subs_path)?).unwrap_or_default();
                subs.retain(|s| !removed.contains(&s.user_id));
                let _ = fs::write(&subs_path, serde_json::to_string_pretty(&subs).unwrap_or_default());
            }
    
            // Remove from src/subs.json
            let src_path = self.src_subs_path();
            if src_path.exists() {
                let s = fs::read_to_string(&src_path)?;
                let mut arr: Vec<serde_json::Value> = if s.trim().is_empty() {
                    Vec::new()
                } else {
                    serde_json::from_str(&s).unwrap_or_else(|_| Vec::new())
                };
                let removed_set: std::collections::HashSet<i64> = removed.iter().cloned().collect();
                arr.retain(|v| {
                    v.get("user_id")
                        .and_then(|u| u.as_i64())
                        .map(|id| !removed_set.contains(&id))
                        .unwrap_or(true)
                });
                let _ = fs::write(&src_path, serde_json::to_string_pretty(&arr).unwrap_or_default());
            }
    
            // Ban from chats
            let csv_path = chats_csv.unwrap_or("chats2.csv");
            let chats = match self.load_target_chats(csv_path) {
                Ok(c) if !c.is_empty() => c,
                _ => return Ok(vec![]),
            };
    
            let mut attempted = Vec::new();
            for user_id in removed.into_iter() {
                for &chat in &chats {
                    match bot.ban_chat_member(ChatId(chat), UserId(user_id as u64)).await {
                        Ok(_) => {
                            attempted.push((user_id, chat));
                        }
                        Err(err) => {
                            eprintln!("Failed to ban {} from {}: {:?}", user_id, chat, err);
                            attempted.push((user_id, chat));
                        }
                    }
                }
            }
    
            Ok(attempted)
        }


        
    }



