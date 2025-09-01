use std::{env, fs, io::Write, path::Path};

use anyhow::Context;
use dotenvy::dotenv;
use serde_json::{json, Value};
use teloxide::{
    prelude::*,
    types::{InlineKeyboardButton, InlineKeyboardMarkup},
};
use tracing::{error, info};
use tracing_subscriber::{fmt, EnvFilter};

const DATA_DIR: &str = "data";
const STATE_PATH: &str = "data/state.json"; // {"target_chat_id": i64, "subscribed": bool}

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

fn build_kb(subscribed: bool) -> InlineKeyboardMarkup {
    let label = if subscribed { "Unsubscribe" } else { "Subscribe" };
    InlineKeyboardMarkup::new(vec![vec![InlineKeyboardButton::callback(
        label.to_string(),
        "toggle_sub".to_string(),
    )]])
}

async fn greet(bot: &Bot, chat_id: ChatId, subscribed: bool) {
    let text = "Welcome! Click the button below to start/stop receiving live messages from your chats.";
    let kb = build_kb(subscribed);
    let _ = bot.send_message(chat_id, text).reply_markup(kb).await;
}

async fn handle_message(bot: Bot, msg: Message) -> anyhow::Result<()> {
    ensure_data_dir().ok();
    let mut st = read_state();
    st["target_chat_id"] = Value::from(msg.chat.id.0);
    if st.get("subscribed").is_none() {
        st["subscribed"] = Value::from(false);
    }
    write_json_atomic(STATE_PATH, &st).ok();
    greet(&bot, msg.chat.id, is_subscribed(&st)).await;
    Ok(())
}

async fn handle_callback(bot: Bot, q: CallbackQuery) -> anyhow::Result<()> {
    if let Some(data) = q.data.clone() {
        if data == "toggle_sub" {
            let mut st = read_state();
            if let Some(msg) = &q.message {
                st["target_chat_id"] = Value::from(msg.chat.id.0);
            }
            let new_sub = !is_subscribed(&st);
            st["subscribed"] = Value::from(new_sub);
            write_json_atomic(STATE_PATH, &st).ok();

            if let Some(msg) = q.message {
                let kb = build_kb(new_sub);
                let _ = bot
                    .edit_message_reply_markup(msg.chat.id, msg.id)
                    .reply_markup(kb)
                    .await;
            }
            let _ = bot
                .answer_callback_query(q.id)
                .text(if new_sub { "Subscribed ✅" } else { "Unsubscribed ⛔" })
                .await;
        }
    }
    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenv().ok();
    fmt().with_env_filter(EnvFilter::from_default_env()).init();
    ensure_data_dir().ok();

    let token = env::var("TELEGRAM_BOT_TOKEN").context("TELEGRAM_BOT_TOKEN not set in .env")?;
    let bot = Bot::new(token);

    let handler = dptree::entry()
        .branch(Update::filter_message().endpoint(handle_message))
        .branch(Update::filter_callback_query().endpoint(handle_callback));

    Dispatcher::builder(bot, handler)
        .enable_ctrlc_handler()
        .build()
        .dispatch()
        .await;

    info!("Bot stopped.");
    Ok(())
}
