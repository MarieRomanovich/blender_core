use std::{fs, io::Write, path::Path};

use anyhow::Context;
use dotenvy::dotenv;
use serde_json::{json, Value};
use teloxide::{
    prelude::*,
    types::{InlineKeyboardButton, InlineKeyboardMarkup},
};
use tracing::info;
use tracing_subscriber::EnvFilter;

mod payments;
mod admin;
mod channels;
mod catalog; // ensure this is declared if you use the module

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

fn build_catalog_keyboard(page: usize) -> InlineKeyboardMarkup {
    let items = load_catalog();
    let total = items.len();
    let pages = std::cmp::max(1, (total + PAGE_SIZE - 1) / PAGE_SIZE);
    let cur = page.clamp(1, pages);
    let start = (cur - 1) * PAGE_SIZE;
    let end = std::cmp::min(start + PAGE_SIZE, total);

    let mut rows: Vec<Vec<InlineKeyboardButton>> = Vec::new();
    for (_id, title, ctype) in &items[start..end] {
        let text = format!("{} {}", type_icon(ctype), title);
        // Inert buttons: clicking does nothing
        rows.push(vec![InlineKeyboardButton::callback(text, "cat:noop".to_string())]);
    }

    if pages > 1 {
        let mut nav = Vec::new();
        if cur > 1 {
            nav.push(InlineKeyboardButton::callback("⬅️ Prev", format!("cat:page:{}", cur - 1)));
        }
        nav.push(InlineKeyboardButton::callback(format!("Page {cur}/{pages}"), "cat:noop".to_string()));
        if cur < pages {
            nav.push(InlineKeyboardButton::callback("Next ➡️", format!("cat:page:{}", cur + 1)));
        }
        rows.push(nav);
    }

    InlineKeyboardMarkup::new(rows)
}

async fn show_catalog(bot: &Bot, chat_id: ChatId, page: usize) {
    let items = load_catalog();
    if items.is_empty() {
        let _ = bot
            .send_message(chat_id, "No chats found in src/chats_export.csv")
            .await;
        return;
    }
    let _ = bot
        .send_message(chat_id, "Browse your chats (buttons are inert):")
        .reply_markup(build_catalog_keyboard(page))
        .await;
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
            .send_message(chat_id, "If you're an admin, tap the keyboard button:")
            .reply_markup(adm.public_keyboard())
            .await;
    }

    // Admins bypass paywall
    if adm.is_authed(chat_id.0).await {
        let _ = bot.send_message(chat_id, "Admin panel:").reply_markup(adm.panel_keyboard()).await;
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
        let _ = bot.send_message(chat_id, "Payments are disabled.").await;
        return;
    }
    match pay.create_invoice(None).await {
        Ok(inv) => {
            let mut text = format!("Pay {} {} via CryptoBot, then tap “I’ve paid, check”.", inv.amount, inv.asset);
            if let Some(url) = inv.pay_url.as_ref() {
                text = format!("{text}\n\nPayment link: {url}");
            }
            let kb = InlineKeyboardMarkup::new(vec![
                vec![InlineKeyboardButton::callback(
                    "I’ve paid, check ✅",
                    format!("pay:check:{}", inv.invoice_id),
                )],
            ]);
            let _ = bot.send_message(chat_id, text).reply_markup(kb).await;
        }
        Err(e) => {
            let _ = bot
                .send_message(chat_id, format!("Failed to create invoice: {e}"))
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
    if let Some(t) = msg.text() {
        if t.trim().eq_ignore_ascii_case("/ping") {
            bot.send_message(msg.chat.id, "pong").await?;
            return Ok(());
        }
    }

    // Admin flow can consume messages (password prompts etc.)
    if adm.on_message(&bot, &msg).await {
        return Ok(());
    }

    // Grant free access if user is in admin free list
    let username = msg.from().and_then(|u| u.username.clone());
    if adm.has_free_access(username.as_deref(), msg.chat.id.0) {
        // Option A: mark a monthly subscription for them (extend on each message)
        let _ = channels::set_paid_until(msg.chat.id.0, now_ts() + subscription_days() * 86_400);
        crate::catalog::show_catalog(&bot, msg.chat.id, 1).await;
        return Ok(());
    }

    // ...existing paywall/greet...
    let st = serde_json::json!({}); // if you still use state, keep your existing read_state
    greet(&bot, msg.chat.id, &st, pay, adm).await;
    Ok(())
}

// Handle payment-related callback queries (minimal placeholder to satisfy call site)
async fn handle_pay_callbacks(bot: &Bot, q: &CallbackQuery, pay: &payments::Payments) {
    let Some(data) = q.data.clone() else { return };

    if let Some(rest) = data.strip_prefix("pay:check:") {
        let invoice_id = rest.parse::<i64>().unwrap_or_default();
        if invoice_id == 0 {
            let _ = bot
                .answer_callback_query(q.id.clone())
                .text("Invalid invoice.")
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
                            "✅ Payment succeeded. Access granted for 30 days.",
                        )
                        .await;
                    show_catalog(bot, msg.chat.id, 1).await;
                }
                let _ = bot
                    .answer_callback_query(q.id.clone())
                    .text("Payment confirmed ✅")
                    .await;
            }
            Ok(Some(inv)) if inv.status == "active" => {
                let _ = bot
                    .answer_callback_query(q.id.clone())
                    .text("Still unpaid. Complete the payment and try again.")
                    .show_alert(true)
                    .await;
            }
            Ok(Some(inv)) => {
                let _ = bot
                    .answer_callback_query(q.id.clone())
                    .text(format!("Status: {}", inv.status))
                    .show_alert(true)
                    .await;
            }
            Ok(None) => {
                let _ = bot
                    .answer_callback_query(q.id.clone())
                    .text("Invoice not found.")
                    .show_alert(true)
                    .await;
            }
            Err(e) => {
                let _ = bot
                    .answer_callback_query(q.id.clone())
                    .text(format!("Check failed: {e}"))
                    .show_alert(true)
                    .await;
            }
        }
        return;
    }

    // Optional: support a "pay:start" button if you use one
    if data == "pay:start" {
        if let Some(msg) = &q.message {
            start_payment_flow(bot, msg.chat.id, pay).await;
        }
        let _ = bot.answer_callback_query(q.id.clone()).await;
        return;
    }

    let _ = bot.answer_callback_query(q.id.clone()).await;
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenv().ok(); // load .env first
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

    // Clone for closures
    let pay_msg = pay.clone();
    let adm_msg = adm.clone();
    let pay_cb = pay.clone();

    // Dispatcher
    let handler = dptree::entry()
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
                async move {
                    if let Some(data) = q.data.clone() {
                        // Payments
                        if data.starts_with("pay:") {
                            handle_pay_callbacks(&bot, &q, &pay).await;
                            return Ok::<(), anyhow::Error>(());
                        }
                        // Catalog pagination
                        if let Some(rest) = data.strip_prefix("cat:page:") {
                            let page = rest.parse::<usize>().unwrap_or(1);
                            if let Some(msg) = &q.message {
                                let _ = bot
                                    .edit_message_reply_markup(msg.chat.id, msg.id)
                                    .reply_markup(catalog::build_catalog_keyboard(page))
                                    .await;
                            }
                            let _ = bot.answer_callback_query(q.id.clone()).await;
                            return Ok::<(), anyhow::Error>(());
                        }
                        // Inert buttons
                        if data == "cat:noop" {
                            let _ = bot.answer_callback_query(q.id.clone()).await;
                            return Ok::<(), anyhow::Error>(());
                        }
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
