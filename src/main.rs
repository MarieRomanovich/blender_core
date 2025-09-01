use std::{env, fs, io::Write, path::Path};

use anyhow::Context;
use dotenvy::dotenv;
use serde_json::{json, Value};
use teloxide::{
    prelude::*,
    types::{InlineKeyboardButton, InlineKeyboardMarkup},
};
mod payments;
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

async fn greet(bot: &Bot, chat_id: ChatId, st: &Value, pay: &payments::Payments) {
    // First greeting
    let _ = bot
        .send_message(chat_id, "Welcome! You can use this bot after paying.")
        .await;

    // Second message with either Pay or Enable/Disable
    if !is_paid(st) && pay.enabled() {
        let _ = bot
            .send_message(chat_id, "Tap Pay below to continue.")
            .reply_markup(pay.start_button())
            .await;
    } else {
        let kb = if is_subscribed(st) { kb_disable() } else { kb_enable() };
        let _ = bot
            .send_message(chat_id, "You can control notifications below.")
            .reply_markup(kb)
            .await;
    }
}

async fn handle_message(bot: Bot, msg: Message, pay: &payments::Payments) -> anyhow::Result<()> {
    ensure_data_dir().ok();
    let mut st = read_state();
    st["target_chat_id"] = Value::from(msg.chat.id.0);
    if st.get("subscribed").is_none() {
        st["subscribed"] = Value::from(false);
    }
    if st.get("paid").is_none() {
        st["paid"] = Value::from(false);
    }
    // If payments are disabled, treat as paid
    if !pay.enabled() {
        st["paid"] = Value::from(true);
    }
    write_json_atomic(STATE_PATH, &st).ok();
    greet(&bot, msg.chat.id, &st, pay).await;
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

    // If payments are disabled, ensure paid=true so forwarding can work
    if !pay.enabled() && !is_paid(&st) {
        st["paid"] = Value::from(true);
    }

    // Block toggling only when payments are enabled and user is not paid
    if is_paid(&st) == false && pay.enabled() {
        if let Some(msg) = &q.message {
            start_payment_flow(bot, msg.chat.id, pay).await;
        }
        let _ = bot
            .answer_callback_query(q.id.clone())
            .text("Payment required.")
            .await;
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
                // Mark paid, but don't auto-enable notifications
                let mut st = read_state();
                if let Some(msg) = &q.message {
                    st["target_chat_id"] = Value::from(msg.chat.id.0);
                }
                st["paid"] = Value::from(true);
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

    let pay_clone = pay.clone();
    let handler = dptree::entry()
        .branch(Update::filter_message().endpoint(
            move |bot: Bot, msg: Message| {
                let pay = pay_clone.clone();
                async move {
                    if let Err(e) = handle_message(bot.clone(), msg, &pay).await {
                        error!(error=?e, "handle_message failed");
                    }
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
