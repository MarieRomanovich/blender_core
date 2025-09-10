use std::{env, fs, path::{Path, PathBuf}};
use std::io::Write;

use anyhow::Context;
use dotenvy::dotenv;
use grammers_client::{Client, Config};
use grammers_session::Session;
use serde_json::{json, Value};
use teloxide::{
    prelude::*,
    types::{InlineKeyboardButton, InlineKeyboardMarkup},
};
use teloxide::types::{ChatId, CallbackQuery};
use tracing::info;
use tracing_subscriber::EnvFilter;

mod payments;
mod admin;
mod channels;
mod catalog;
mod sync_history;
mod forward; // add this import
mod user_forward;

const CHATS_EXPORT_PATH: &str = "src/chats_export.csv";

const DATA_DIR: &str = "data";
const STATE_PATH: &str = "data/state.json"; // {"target_chat_id": i64, "subscribed": bool, "paid": bool}

fn ensure_data_dir() -> anyhow::Result<()> {
    fs::create_dir_all(DATA_DIR).context("create data dir")?;
    Ok(())
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
    if adm.enabled() {
        let _ = bot
            .send_message(chat_id, "Если вы админ, нажмите кнопку на клавиатуре:")
            .reply_markup(adm.public_keyboard())
            .await;
    }

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
            start_payment_flow(bot, chat_id, pay).await;
        }
    } else {
        crate::catalog::show_catalog(bot, chat_id, 1).await;
    }
}

// Start payment flow by creating an invoice and sending a URL button + check button
async fn start_payment_flow(bot: &Bot, chat_id: ChatId, pay: &payments::Payments) {
    if !pay.enabled() {
        let _ = bot.send_message(chat_id, "Платежи отключены.").await;
        return;
    }
    match pay.create_invoice(None).await {
        Ok(inv) => {
            let mut text = format!("Оплатите {} {} через CryptoBot, затем нажмите «Я оплатил — проверить».", inv.amount, inv.asset);
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
        if t.trim().eq_ignore_ascii_case("/ping") {
            bot.send_message(msg.chat.id, "pong").await?;
            return Ok(());
        }

        // admin-only start sync command:
        if t.trim().starts_with("/sync_history") {
            // format: /sync_history <source_chat_id> <target_chat_id>
            if !adm.is_authed(msg.chat.id.0).await {
                let _ = bot.send_message(msg.chat.id, "Только админ может запустить синхронизацию").await;
                return Ok(());
            }
            let parts: Vec<&str> = t.split_whitespace().collect();
            if parts.len() < 3 {
                let _ = bot.send_message(msg.chat.id, "Использование: /sync_history <source_id> <target_id>").await;
                return Ok(());
            }
            let src: i64 = match parts[1].parse() {
                Ok(v) => v,
                Err(_) => {
                    let _ = bot.send_message(msg.chat.id, "Неверный ID источника").await;
                    return Ok(());
                }
            };
            let dst: i64 = match parts[2].parse() {
                Ok(v) => v,
                Err(_) => {
                    let _ = bot.send_message(msg.chat.id, "Неверный ID назначения").await;
                    return Ok(());
                }
            };

            // read MTProto envs
            let api_id: i32 = match std::env::var("API_ID").and_then(|s| s.parse::<i32>().map_err(|_| std::env::VarError::NotPresent)) {
                Ok(v) => v,
                Err(_) => {
                    let _ = bot.send_message(msg.chat.id, "API_ID не установлен или неверен").await;
                    return Ok(());
                }
            };
            let api_hash = match std::env::var("API_HASH") {
                Ok(v) => v,
                Err(_) => {
                    let _ = bot.send_message(msg.chat.id, "API_HASH не установлен").await;
                    return Ok(());
                }
            };
            let session_path = std::env::var("MTPROTO_SESSION").unwrap_or_else(|_| "mtproto.session".into());
            let map_path = std::env::var("TOPIC_MAP_JSON").unwrap_or_else(|_| "topic_map.json".into());
            let topic_map = if std::path::Path::new(&map_path).exists() {
                match std::fs::read_to_string(&map_path).and_then(|s| serde_json::from_str::<std::collections::HashMap<i32,i32>>(&s).map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))) {
                    Ok(m) => Some(m),
                    Err(_) => None,
                }
            } else {
                None
            };

            let reply = bot.send_message(msg.chat.id, format!("Запуск синхронизации: {} -> {} (в фоне)", src, dst)).await;

            // spawn background task
            let api_hash_clone = api_hash.clone();
            tokio::spawn(async move {
                if let Err(e) = sync_history::sync_history(
                    api_id,
                    &api_hash_clone,
                    PathBuf::from(session_path),
                    src,
                    dst,
                    topic_map.as_ref(),
                )
                .await
                {
                    tracing::error!("sync_history failed: {:?}", e);
                } else {
                    tracing::info!("sync_history finished");
                }
            });

            // reply user
            if let Ok(s) = reply {
                let _ = bot.send_message(s.chat.id, "Синхронизация запущена (см. логи).").await;
            }
            return Ok(());
        }
        // end sync command
    }

    // Admin flow can consume messages (password prompts etc.)
    if adm.on_message(&bot, &msg).await {
        return Ok(());
    }

    // Grant free access if user is in admin free list
    let username = msg.from().and_then(|u| u.username.clone());
    if adm.has_free_access(username.as_deref(), msg.chat.id.0) {
        // mark a monthly subscription for them
        let _ = channels::set_paid_until(msg.chat.id.0, now_ts() + subscription_days() * 86_400);
        // send dynamic invite link
        send_channel_invite(&bot, msg.chat.id).await;
        // show catalog as plain text
        show_catalog(&bot, msg.chat.id, 1).await;
        return Ok(());
    }

    // ...existing paywall/greet...
    let st = serde_json::json!({}); // if you still use state, keep your existing read_state
    greet(&bot, msg.chat.id, &st, pay, adm).await;
    Ok(())
}

// Try dynamic invite creation (preferred). Fallback to CHANNEL_INVITE_LINK env var.
// Requires bot to be admin in the channel (with permission to invite/create invite links).
async fn send_channel_invite(bot: &Bot, to_chat: ChatId) {
    // Try CHANNEL_ID env first (numeric chat id, e.g. -1001234567890)
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
                    "Доступ предоставлен, но CHANNEL_ID / CHANNEL_INVITE_LINK не заданы или создание ссылки не удалось. Обратитесь к администратору за ссылкой-приглашением.",
                )
                .await;
        }
    }
}

// Handle payment-related callback queries (minimal placeholder to satisfy call site)
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

                    let _ = bot
                        .edit_message_text(
                            msg.chat.id,
                            msg.id,
                            "✅ Платёж подтверждён. Доступ предоставлен на 30 дней.",
                        )
                        .await;

                    // If the user is NOT an admin, send the invite link (dynamic)
                    if !adm.is_authed(msg.chat.id.0).await {
                        send_channel_invite(bot, msg.chat.id).await;
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
            start_payment_flow(bot, msg.chat.id, pay).await;
        }
        let _ = bot.answer_callback_query(q.id.clone()).await;
        return;
    }

    let _ = bot.answer_callback_query(q.id.clone()).await;
}

// Add this helper (place near other async helpers)
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
            start_payment_flow(bot, msg.chat.id, &pay).await;
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
    let _ = bot.answer_callback_query(qid).await;
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenv().ok();
    tracing_subscriber::fmt().with_env_filter(EnvFilter::from_default_env()).init();

    ensure_data_dir().ok();
    channels::init_db()?; // keep DB for admin/payments state if needed

    // Read TELEGRAM_BOT_TOKEN directly
    let token = std::env::var("TELEGRAM_BOT_TOKEN")
        .context("TELEGRAM_BOT_TOKEN not set; add it to .env or set TELOXIDE_TOKEN when using Bot::from_env")?;
    let bot = Bot::new(token);

    // Clear webhook so long polling works
    let _ = bot.delete_webhook().drop_pending_updates(true).send().await;

    // Shared services
    let pay = payments::Payments::new_from_env()?;
    let adm = admin::Admin::new_from_env();

    // --- spawn history sync on start if requested ---
    // history sync removed; no background spawn

    // Clone for closures
    let pay_msg = pay.clone();
    let adm_msg = adm.clone();
    let pay_cb = pay.clone();
    let adm_cb = adm.clone();

    // Dispatcher
    let handler = teloxide::dptree::entry()
        .branch(Update::filter_message().endpoint(
            move |bot: Bot, msg: Message| {
                let pay = pay_msg.clone();
                let adm = adm_msg.clone();
                async move {
                    if let Some(t) = msg.text() {
                        if t.trim().eq_ignore_ascii_case("/ping") {
                            let _ = bot.send_message(msg.chat.id, "pong").await;
                            return Ok::<(), anyhow::Error>(());
                        }
                    }

                    if adm.on_message(&bot, &msg).await {
                        return Ok(());
                    }

                    if let Err(e) = handle_message(bot.clone(), msg, &pay, &adm).await {
                        tracing::error!(error=?e, "handle_message failed");
                    }
                    Ok::<(), anyhow::Error>(())
                }
            },
        ))
        .branch(Update::filter_callback_query().endpoint(
            move |bot: Bot, q: CallbackQuery| {
                let pay = pay_cb.clone();
                let adm = adm_cb.clone();
                async move {
                    if q.data.is_some() {
                        handle_pay_callback(&bot, q, pay, adm).await;
                    } else {
                        let _ = bot.answer_callback_query(q.id.clone()).await;
                    }
                    Ok::<(), anyhow::Error>(())
                }
            },
        ));

    Dispatcher::builder(bot, handler)
        .enable_ctrlc_handler()
        .build()
        .dispatch()
        .await;

    Ok(())
}
