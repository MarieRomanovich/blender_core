use std::{collections::HashSet, fs, io::Write, path::Path};

use serde_json::{json, Value};
use teloxide::{prelude::*, types::InlineKeyboardButton};
use tokio::sync::RwLock;

const STATE_PATH: &str = "data/state.json";

fn read_state() -> Value {
    match fs::read_to_string(STATE_PATH) {
        Ok(s) => serde_json::from_str(&s).unwrap_or_else(|_| json!({})),
        Err(_) => json!({}),
    }
}

fn write_json_atomic(path: &str, v: &Value) {
    let parent = Path::new(path).parent().unwrap();
    fs::create_dir_all(parent).ok();
    let tmp_path = format!("{path}.tmp");
    let data = serde_json::to_vec_pretty(v).unwrap_or_default();
    if let Ok(mut f) = fs::File::create(&tmp_path) {
        let _ = f.write_all(&data);
        let _ = f.sync_all();
        let _ = fs::rename(&tmp_path, path);
    }
}

#[derive(Clone)]
pub struct Admin {
    password: Option<String>,
    pending: std::sync::Arc<RwLock<HashSet<i64>>>, // chat_id -> waiting for password
}

impl Admin {
    pub fn new_from_env() -> Self {
        let pwd = std::env::var("ADMIN_PASSWORD").ok().filter(|s| !s.trim().is_empty());
        Self {
            password: pwd,
            pending: std::sync::Arc::new(RwLock::new(HashSet::new())),
        }
    }

    pub fn enabled(&self) -> bool {
        self.password.is_some()
    }

    pub fn button() -> InlineKeyboardButton {
        InlineKeyboardButton::callback("I’m admin", "admin:start")
    }

    pub async fn on_callback_start(&self, bot: &Bot, q: &CallbackQuery) {
        if !self.enabled() {
            let _ = bot
                .answer_callback_query(q.id.clone())
                .text("Admin mode is not configured.")
                .show_alert(true)
                .await;
            return;
        }
        if let Some(msg) = &q.message {
            self.pending.write().await.insert(msg.chat.id.0);
            let _ = bot
                .send_message(msg.chat.id, "Send admin password here.")
                .await;
        }
        let _ = bot.answer_callback_query(q.id.clone()).await;
    }

    // Returns true if handled (password attempt), false otherwise
    pub async fn on_text(&self, bot: &Bot, msg: &Message) -> bool {
        if !self.enabled() {
            return false;
        }
        let chat_id = msg.chat.id.0;
        if !self.pending.read().await.contains(&chat_id) {
            return false;
        }
        let Some(text) = msg.text() else {
            return true; // ignore non-text while pending
        };
        let ok = self.password.as_deref().map(|p| p == text).unwrap_or(false);
        if ok {
            // Mark paid via admin (free access), don’t auto-enable notifications
            let mut st = read_state();
            st["target_chat_id"] = Value::from(chat_id);
            st["paid"] = Value::from(true);
            st["paid_forced"] = Value::from(false);
            st["paid_invoice_id"] = Value::from(-1); // sentinel: admin-granted
            if st.get("subscribed").is_none() {
                st["subscribed"] = Value::from(false);
            }
            write_json_atomic(STATE_PATH, &st);

            let _ = bot
                .send_message(msg.chat.id, "✅ Admin verified. You can enable notifications via the button below.")
                .reply_markup(
                    teloxide::types::InlineKeyboardMarkup::new(vec![vec![
                        InlineKeyboardButton::callback("Enable notifications", "toggle_sub"),
                    ]]),
                )
                .await;
            self.pending.write().await.remove(&chat_id);
        } else {
            let _ = bot.send_message(msg.chat.id, "❌ Wrong password. Try again.").await;
        }
        true
    }
}