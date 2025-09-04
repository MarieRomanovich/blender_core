use std::{
    collections::HashSet,
    fs,
    io::Write,
    path::Path,
    sync::Arc,
};

use serde_json::{json, Value};
use teloxide::{
    prelude::*,
    types::{KeyboardButton, KeyboardMarkup}
};
use tokio::sync::RwLock;

use crate::catalog;

const STATE_PATH: &str = "data/state.json";
const FREE_USERS_PATH: &str = "data/free_users.json";

const BTN_IM_ADMIN: &str = "I'm admin";
const BTN_ADD: &str = "Add user";
const BTN_REMOVE: &str = "Remove user";
const BTN_LIST: &str = "List";
// const BTN_ADD_CH: &str = "Add channel";
// const BTN_REMOVE_CH: &str = "Remove channel";
// const BTN_LIST_CH: &str = "List channels";
// const BTN_EXPORT_CSV: &str = "Export CSV";
// const BTN_RELOAD_FROM_CSV: &str = "Reload channels";
// Keep seed path consistent with main
const SEED_PATH: &str = "src/channels_seed.csv";

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

fn read_state() -> Value {
    match fs::read_to_string(STATE_PATH) {
        Ok(s) => serde_json::from_str(&s).unwrap_or_else(|_| json!({})),
        Err(_) => json!({}),
    }
}

fn read_free_users() -> HashSet<String> {
    match fs::read_to_string(FREE_USERS_PATH) {
        Ok(s) => serde_json::from_str::<Vec<String>>(&s)
            .map(|v| v.into_iter().map(|u| u.to_lowercase()).collect())
            .unwrap_or_default(),
        Err(_) => HashSet::new(),
    }
}

fn save_free_users(set: &HashSet<String>) {
    let mut v: Vec<String> = set.iter().cloned().collect();
    v.sort();
    write_json_atomic(FREE_USERS_PATH, &json!(v));
}

fn normalize_username(input: &str) -> Option<String> {
    let t = input.trim().trim_start_matches('@').to_lowercase();
    if t.is_empty() { None } else { Some(t) }
}

#[derive(Clone)]
pub struct Admin {
    password: Option<String>,
    pending_pwd: Arc<RwLock<HashSet<i64>>>,
    pending_add: Arc<RwLock<HashSet<i64>>>,
    pending_remove: Arc<RwLock<HashSet<i64>>>,
    authed: Arc<RwLock<HashSet<i64>>>, // chats with admin rights
    pending_add_ch: Arc<RwLock<HashSet<i64>>>,
    pending_remove_ch: Arc<RwLock<HashSet<i64>>>,
}

impl Admin {
    pub fn new_from_env() -> Self {
        let pwd = std::env::var("ADMIN_PASSWORD").ok().filter(|s| !s.trim().is_empty());
        Self {
            password: pwd,
            pending_pwd: Arc::new(RwLock::new(HashSet::new())),
            pending_add: Arc::new(RwLock::new(HashSet::new())),
            pending_remove: Arc::new(RwLock::new(HashSet::new())),
            authed: Arc::new(RwLock::new(HashSet::new())),
            pending_add_ch: Arc::new(RwLock::new(HashSet::new())),
            pending_remove_ch: Arc::new(RwLock::new(HashSet::new())),
        }
    }

    pub fn enabled(&self) -> bool {
        self.password.is_some()
    }

    // Public reply keyboard shown to everyone (when payments are ON)
    pub fn public_keyboard(&self) -> KeyboardMarkup {
        KeyboardMarkup::new(vec![vec![KeyboardButton::new(BTN_IM_ADMIN)]])
            .resize_keyboard(true)
            .one_time_keyboard(false)
    }

    // Admin panel keyboard
    pub fn panel_keyboard(&self) -> KeyboardMarkup {
        KeyboardMarkup::new(vec![
            vec![
                KeyboardButton::new(BTN_ADD),     // keep: add user
                KeyboardButton::new(BTN_REMOVE),  // keep: remove user
                KeyboardButton::new(BTN_LIST),    // keep: list users
            ],
            // Removed the row with channel buttons (Add/Remove/List channels)
        ])
        .resize_keyboard(true)
        .one_time_keyboard(false)
    }

    // Returns true if username (without @, case-insensitive) or chat_id is in data/free_users.json
    pub fn has_free_access(&self, username: Option<&str>, chat_id: i64) -> bool {
        let list = read_free_list();
        let u = username
            .unwrap_or_default()
            .trim()
            .trim_start_matches('@')
            .to_lowercase();
        if !u.is_empty() && list.iter().any(|s| s == &u) {
            return true;
        }
        list.iter().any(|s| s == &chat_id.to_string())
    }

    // Check if a chat is admin-authed
    pub async fn is_authed(&self, chat_id: i64) -> bool {
        self.authed.read().await.contains(&chat_id)
    }

    // Returns true if the message was consumed by admin flow
    pub async fn on_message(&self, bot: &Bot, msg: &Message) -> bool {
        if !self.enabled() { return false; }
        let chat = msg.chat.id;
        let Some(text) = msg.text() else { return false; };
        let chat_id = chat.0;

        // If waiting for admin password, verify it
        if self.pending_pwd.read().await.contains(&chat_id) {
            let ok = self.password.as_deref().map(|p| p == text.trim()).unwrap_or(false);
            if ok {
                self.pending_pwd.write().await.remove(&chat_id);
                self.authed.write().await.insert(chat_id);

                // Mark paid via admin (free access)
                let mut st = read_state();
                st["target_chat_id"] = Value::from(chat_id);
                st["paid"] = Value::from(true);
                st["paid_forced"] = Value::from(false);
                st["paid_invoice_id"] = Value::from(-1); // admin-granted
                if st.get("subscribed").is_none() {
                    st["subscribed"] = Value::from(false);
                }
                write_json_atomic(STATE_PATH, &st);

                // 1) Admin panel keyboard
                let _ = bot
                    .send_message(msg.chat.id, "✅ Admin verified. Admin panel:")
                    .reply_markup(self.panel_keyboard())
                    .await;

                // 2) Channel/chat buttons (inert)
                crate::catalog::show_catalog(bot, msg.chat.id, 1).await;

                return true;
            } else {
                let _ = bot.send_message(chat, "❌ Wrong password. Try again.").await;
                return true;
            }
        }

        // If waiting for add/remove username
        if self.pending_add.read().await.contains(&chat_id) {
            if let Some(u) = normalize_username(text) {
                let mut set = read_free_users();
                let inserted = set.insert(u.clone());
                save_free_users(&set);
                let _ = bot
                    .send_message(chat, if inserted {
                        format!("✅ Added @{}", u)
                    } else {
                        format!("ℹ️ @{} is already in the list", u)
                    })
                    .await;
                self.pending_add.write().await.remove(&chat_id);
                let _ = bot.send_message(chat, "Admin panel:").reply_markup(self.panel_keyboard()).await;
            } else {
                let _ = bot.send_message(chat, "Send a valid username (with or without @).").await;
            }
            return true;
        }

        if self.pending_remove.read().await.contains(&chat_id) {
            if let Some(u) = normalize_username(text) {
                let mut set = read_free_users();
                let removed = set.remove(&u);
                save_free_users(&set);
                let _ = bot
                    .send_message(chat, if removed {
                        format!("✅ Removed @{}", u)
                    } else {
                        format!("ℹ️ @{} was not in the list", u)
                    })
                    .await;
                self.pending_remove.write().await.remove(&chat_id);
                let _ = bot.send_message(chat, "Admin panel:").reply_markup(self.panel_keyboard()).await;
            } else {
                let _ = bot.send_message(chat, "Send a valid username (with or without @).").await;
            }
            return true;
        }

        // Not pending anything: handle triggers
        let is_authed = self.authed.read().await.contains(&chat_id);

        // If waiting for channel add/remove, expect a forwarded message from the channel
        if self.pending_add_ch.read().await.contains(&chat_id) || self.pending_remove_ch.read().await.contains(&chat_id) {
            // Use MessageCommon.forward to detect forwarded channel messages
            if let teloxide::types::MessageKind::Common(ref common) = msg.kind {
                if let Some(fwd) = common.forward.clone() {
                    if let teloxide::types::ForwardedFrom::Chat(chat) = fwd.from {
                        let ch_id = chat.id.0;
                        let title = chat
                            .title()
                            .map(|t| t.to_string())
                            .unwrap_or_else(|| "Unknown".to_string());
                        let username = chat.username();

                        if self.pending_add_ch.read().await.contains(&chat_id) {
                            match crate::channels::add_channel(ch_id, title.as_str(), username) {
                                Ok(_) => {
                                    let _ = bot.send_message(ChatId(chat_id), format!("✅ Added channel: {} (id: {})", title, ch_id)).await;
                                }
                                Err(e) => {
                                    let _ = bot.send_message(ChatId(chat_id), format!("❌ Add failed: {e}")).await;
                                }
                            }
                            self.pending_add_ch.write().await.remove(&chat_id);
                        } else if self.pending_remove_ch.read().await.contains(&chat_id) {
                            match crate::channels::remove_channel(ch_id) {
                                Ok(removed) => {
                                    let _ = bot.send_message(ChatId(chat_id), if removed {
                                        format!("✅ Removed channel id: {}", ch_id)
                                    } else {
                                        format!("ℹ️ Channel id {} not found", ch_id)
                                    }).await;
                                }
                                Err(e) => {
                                    let _ = bot.send_message(ChatId(chat_id), format!("❌ Remove failed: {e}")).await;
                                }
                            }
                            self.pending_remove_ch.write().await.remove(&chat_id);
                        }

                        let _ = bot.send_message(ChatId(chat_id), "Admin panel:").reply_markup(self.panel_keyboard()).await;
                        return true;
                    }
                }
            }
            let _ = bot
                .send_message(ChatId(chat_id), "Please forward a message from the target channel here.")
                .await;
            return true;
        }

        match text.trim() {
            BTN_IM_ADMIN => {
                self.pending_pwd.write().await.insert(chat_id);
                let _ = bot.send_message(chat, "Send admin password:").await;
                true
            }
            t if t == BTN_ADD && is_authed => {
                self.pending_add.write().await.insert(chat_id);
                let _ = bot.send_message(chat, "Send username to ADD (with or without @):").await;
                true
            }
            t if t == BTN_REMOVE && is_authed => {
                self.pending_remove.write().await.insert(chat_id);
                let _ = bot.send_message(chat, "Send username to REMOVE (with or without @):").await;
                true
            }
            t if t == BTN_LIST && is_authed => {
                let mut set: Vec<_> = super::admin::read_free_users().into_iter().collect();
                set.sort();
                let list = if set.is_empty() { "No free-access users yet.".to_string() } else { format!("Free-access users:\n@{}", set.join("\n@")) };
                let _ = bot.send_message(chat, list).await;
                true
            }
            _ => false,
        }
    }
}

// Store free users as JSON array of strings: ["user1","user2","123456789"]
fn read_free_list() -> Vec<String> {
    let path = std::path::Path::new("data/free_users.json");
    if let Ok(txt) = fs::read_to_string(path) {
        serde_json::from_str::<Vec<String>>(&txt)
            .unwrap_or_default()
            .into_iter()
            .map(|s| s.trim().trim_start_matches('@').to_lowercase())
            .filter(|s| !s.is_empty())
            .collect()
    } else {
        Vec::new()
    }
}