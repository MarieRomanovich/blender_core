use std::{env, fs, io::Write, path::Path};

use anyhow::Context;
use dotenvy::dotenv;
use serde_json::{json, Value};
use teloxide::{
    prelude::*,
    types::{InlineKeyboardButton, InlineKeyboardMarkup, KeyboardMarkup, KeyboardButton, KeyboardRemove},
};
mod payments;
mod admin;
use tracing::{error, info};
use tracing_subscriber::{fmt, EnvFilter};

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

fn is_subscribed(st: &Value) -> bool {
    st.get("subscribed").and_then(|b| b.as_bool()).unwrap_or(false)
}
fn is_paid(st: &Value) -> bool {
    st.get("paid").and_then(|b| b.as_bool()).unwrap_or(false)
}
fn is_paid_forced(st: &Value) -> bool {
    st.get("paid_forced").and_then(|b| b.as_bool()).unwrap_or(false)
}

// Normalize "paid" based on payments mode and presence of a confirmed invoice
fn normalize_paid_state(st: &mut Value, pay: &payments::Payments) {
    // If payments are disabled -> force paid
    if !pay.enabled() {
        st["paid"] = Value::from(true);
        st["paid_forced"] = Value::from(true);
        return;
    }
    // Payments enabled:
    // if paid was forced earlier, clear it
    if is_paid_forced(st) {
        st["paid"] = Value::from(false);
        st["paid_forced"] = Value::from(false);
    }
    // If "paid" is true but no invoice_id recorded, clear it to show Pay button
    let has_invoice = st.get("paid_invoice_id").and_then(|v| v.as_i64()).is_some();
    if is_paid(st) && !has_invoice {
        st["paid"] = Value::from(false);
    }
}

fn kb_enable() -> InlineKeyboardMarkup {
    InlineKeyboardMarkup::new(vec![vec![InlineKeyboardButton::callback(
        "Enable notifications",
        "toggle_sub",
    )]])
}
fn kb_disable() -> InlineKeyboardMarkup {
    InlineKeyboardMarkup::new(vec![vec![InlineKeyboardButton::callback(
        "Disable notifications",
        "toggle_sub",
    )]])
}

async fn greet(bot: &Bot, chat_id: ChatId, st: &Value, pay: &payments::Payments, adm: &admin::Admin) {
    let _ = bot.send_message(chat_id, "Welcome! You can use this bot after paying.").await;

    if pay.enabled() && !is_paid(st) {
        // Show public admin button on reply keyboard
        if adm.enabled() {
            let _ = bot
                .send_message(chat_id, "If you're an admin, tap the keyboard button:")
                .reply_markup(adm.public_keyboard())
                .await;
        }
        // Show Pay button inline
        let _ = bot
            .send_message(chat_id, "Or pay to continue:")
            .reply_markup(pay.start_button())
            .await;
    } else {
        let kb = if is_subscribed(st) { kb_disable() } else { kb_enable() };
        let _ = bot
            .send_message(chat_id, "Control notifications below.")
            .reply_markup(kb)
            .await;
        // Hide admin keyboard if any
        let _ = bot
            .send_message(chat_id, "Keyboard hidden.")
            .reply_markup(KeyboardRemove::new())
            .await;
    }
}

// Handle text: admin flow first; then free-access gate; then greet
async fn handle_message(
    bot: Bot,
    msg: Message,
    pay: &payments::Payments,
    adm: &admin::Admin,
) -> anyhow::Result<()> {
    // Admin flow can consume the message
    if adm.on_message(&bot, &msg).await {
        return Ok(());
    }

    ensure_data_dir().ok();
    let mut st = read_state();
    st["target_chat_id"] = Value::from(msg.chat.id.0);
    if st.get("subscribed").is_none() { st["subscribed"] = Value::from(false); }
    if st.get("paid").is_none() { st["paid"] = Value::from(false); }

    // Detect if we grant free access right now
    let mut just_free_granted = false;

    // Mark as paid if sender username is in free-access list (payments ON only)
    if pay.enabled() && !is_paid(&st) {
        let sender_username = msg.from().and_then(|u| u.username.clone());
        if adm.has_free_access(sender_username.as_deref()) {
            st["paid"] = Value::from(true);
            st["paid_forced"] = Value::from(false);
            st["paid_invoice_id"] = Value::from(-2); // free-access marker
            just_free_granted = true;
        }
    }

    // Keep previous normalization
    normalize_paid_state(&mut st, pay);

    write_json_atomic(STATE_PATH, &st).ok();

    // If we’ve just granted free access, inform and show Enable button
    if just_free_granted {
        let _ = bot
            .send_message(msg.chat.id, "Congrats! You were given free access!")
            .await;
        let _ = bot
            .send_message(msg.chat.id, "Enable notifications:")
            .reply_markup(kb_enable())
            .await;
        return Ok(());
    }

    greet(&bot, msg.chat.id, &st, pay, adm).await;
    Ok(())
}

async fn start_payment_flow(bot: &Bot, chat_id: ChatId, pay: &payments::Payments) {
    if !pay.enabled() {
        let _ = bot
            .send_message(chat_id, "Payments are disabled right now.")
            .await;
        return;
    }
    match pay.create_invoice(None).await {
        Ok(inv) => {
            let text = format!("Pay {} {} via CryptoBot, then tap “I’ve paid, check”.", inv.amount, inv.asset);
            let _ = bot.send_message(chat_id, text).reply_markup(pay.check_button(&inv)).await;
        }
        Err(e) => {
            let _ = bot.send_message(chat_id, format!("Failed to start payment: {e}")).await;
        }
    }
}

async fn handle_toggle(bot: &Bot, q: &CallbackQuery, pay: &payments::Payments) {
    let mut st = read_state();
    if let Some(msg) = &q.message {
        st["target_chat_id"] = Value::from(msg.chat.id.0);
    }

    // Keep paid flag consistent with current payments mode
    normalize_paid_state(&mut st, pay);

    if pay.enabled() && !is_paid(&st) {
        if let Some(msg) = &q.message {
            start_payment_flow(bot, msg.chat.id, pay).await;
        }
        let _ = bot
            .answer_callback_query(q.id.clone())
            .text("Payment required.")
            .await;
        let _ = write_json_atomic(STATE_PATH, &st);
        return;
    }

    let new_sub = !is_subscribed(&st);
    st["subscribed"] = Value::from(new_sub);
    let _ = write_json_atomic(STATE_PATH, &st);

    if let Some(msg) = &q.message {
        let kb = if new_sub { kb_disable() } else { kb_enable() };
        let _ = bot
            .edit_message_reply_markup(msg.chat.id, msg.id)
            .reply_markup(kb)
            .await;
    }
    let _ = bot
        .answer_callback_query(q.id.clone())
        .text(if new_sub { "Notifications enabled ✅" } else { "Notifications disabled ⛔" })
        .await;
}

async fn handle_pay_callbacks(bot: &Bot, q: &CallbackQuery, pay: &payments::Payments) {
    let Some(data) = q.data.clone() else { return };
    if data == "pay:start" {
        if let Some(msg) = &q.message {
            start_payment_flow(bot, msg.chat.id, pay).await;
        }
        let _ = bot.answer_callback_query(q.id.clone()).await;
        return;
    }
    if let Some(rest) = data.strip_prefix("pay:check:") {
        let id = rest.parse::<i64>().unwrap_or_default();
        if id == 0 {
            let _ = bot
                .answer_callback_query(q.id.clone())
                .text("Invalid invoice.")
                .show_alert(true)
                .await;
            return;
        }
        match pay.get_invoice(id).await {
            Ok(Some(inv)) if inv.status == "paid" => {
                // Mark paid and record invoice id
                let mut st = read_state();
                if let Some(msg) = &q.message {
                    st["target_chat_id"] = Value::from(msg.chat.id.0);
                }
                st["paid"] = Value::from(true);
                st["paid_forced"] = Value::from(false);
                st["paid_invoice_id"] = Value::from(id);
                if st.get("subscribed").is_none() {
                    st["subscribed"] = Value::from(false);
                }
                let _ = write_json_atomic(STATE_PATH, &st);

                if let Some(msg) = &q.message {
                    let _ = bot
                        .edit_message_text(
                            msg.chat.id,
                            msg.id,
                            "✅ Payment succeeded. You can enable notifications via the button below.",
                        )
                        .await;
                    let _ = bot
                        .send_message(msg.chat.id, "Enable notifications:")
                        .reply_markup(kb_enable())
                        .await;
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
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenv().ok();
    fmt().with_env_filter(EnvFilter::from_default_env()).init();
    ensure_data_dir().ok();

    let token = env::var("TELEGRAM_BOT_TOKEN").context("TELEGRAM_BOT_TOKEN not set in .env")?;
    let bot = Bot::new(token);
    let pay = payments::Payments::new_from_env()?;
    let adm = admin::Admin::new_from_env();

    let pay_clone = pay.clone();
    let adm_clone = adm.clone();
    let handler = dptree::entry()
        .branch(Update::filter_message().endpoint(
            move |bot: Bot, msg: Message| {
                let pay = pay_clone.clone();
                let adm = adm_clone.clone();
                async move {
                    let _ = handle_message(bot.clone(), msg, &pay, &adm).await;
                    Ok::<(), anyhow::Error>(())
                }
            },
        ))
        .branch(Update::filter_callback_query().endpoint(
            move |bot: Bot, q: CallbackQuery| {
                let pay = pay.clone();
                async move {
                    if let Some(data) = q.data.clone() {
                        if data.starts_with("pay:") {
                            handle_pay_callbacks(&bot, &q, &pay).await;
                            return Ok::<(), anyhow::Error>(());
                        }
                        if data == "toggle_sub" {
                            handle_toggle(&bot, &q, &pay).await;
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

    info!("Bot stopped.");
    Ok(())
}
