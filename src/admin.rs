use std::{collections::HashSet, fs, io::Write, path::Path, sync::Arc};

use serde_json::{json, Value};
use teloxide::{
    prelude::*,
    types::{InlineKeyboardButton, InlineKeyboardMarkup, KeyboardButton, KeyboardMarkup, KeyboardRemove},
};
use tokio::sync::RwLock;

const STATE_PATH: &str = "data/state.json";
const ADMIN_BUTTON_TEXT: &str = "I'm admin";

fn read_state() -> Value {
    match fs::read_to_string(STATE_PATH) {
        Ok(s) => serde_json::from_str(&s).unwrap_or_else(|_| json!({})),
        Err(_) => json!({}),
    }
}

fn write_json_atomic(path: &str, v: &Value) {
    let parent = Path::new(path).parent().unwrap();
    let _ = fs::create_dir_all(parent);
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
    pending: Arc<RwLock<HashSet<i64>>>, // chat_id waiting for password
}

impl Admin {
    pub fn new_from_env() -> Self {
        let pwd = std::env::var("ADMIN_PASSWORD")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        Self {
            password: pwd,
            pending: Arc::new(RwLock::new(HashSet::new())),
        }
    }

    pub fn enabled(&self) -> bool {
        self.password.is_some()
    }

    pub fn reply_keyboard(&self) -> KeyboardMarkup {
        KeyboardMarkup::new(vec![vec![KeyboardButton::new(ADMIN_BUTTON_TEXT)]])
            .resize_keyboard(true)
            .one_time_keyboard(false)
    }

    fn is_admin_trigger(text: &str) -> bool {
        let t = text.trim().to_lowercase();
        t == "i'm admin" || t == "im admin" || t == "i’m admin"
    }

    // Returns true if this message was handled by admin flow
    pub async fn on_message(&self, bot: &Bot, msg: &Message) -> bool {
        if !self.enabled() {
            return false;
        }
        let chat = msg.chat.id;
        let Some(text) = msg.text() else {
            return false;
        };

        // Start admin flow when the button text is received
        if Self::is_admin_trigger(text) {
            self.pending.write().await.insert(chat.0);
            let _ = bot.send_message(chat, "Send admin password:").await;
            return true;
        }

        // If pending, treat any text as a password attempt
        if self.pending.read().await.contains(&chat.0) {
            let ok = self.password.as_deref().map(|p| p == text).unwrap_or(false);
            if ok {
                let mut st = read_state();
                st["target_chat_id"] = Value::from(chat.0);
                st["paid"] = Value::from(true);
                st["paid_forced"] = Value::from(false);
                st["paid_invoice_id"] = Value::from(-1); // admin granted
                if st.get("subscribed").is_none() {
                    st["subscribed"] = Value::from(false);
                }
                write_json_atomic(STATE_PATH, &st);

                // Confirm and remove the reply keyboard
                let _ = bot
                    .send_message(chat, "✅ Admin verified.")
                    .reply_markup(KeyboardRemove::new())
                    .await;

                // send the Enable notifications button (callback: toggle_sub)
                let _ = bot
                    .send_message(chat, "Enable notifications:")
                    .reply_markup(InlineKeyboardMarkup::new(vec![vec![
                        InlineKeyboardButton::callback("Enable notifications", "toggle_sub"),
                    ]]))
                    .await;

                self.pending.write().await.remove(&chat.0);
            } else {
                let _ = bot
                    .send_message(chat, "❌ Wrong password. Try again or tap “I'm admin” again.")
                    .await;
            }
            return true;
        }

        false
    }
}