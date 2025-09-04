use std::fs;
use teloxide::{prelude::*, types::{InlineKeyboardButton, InlineKeyboardMarkup}};

pub const CHATS_EXPORT_PATH: &str = "src/chats_export.csv";
const PAGE_SIZE: usize = 10;

fn type_icon(t: &str) -> &'static str {
    match t { "channel"=>"📣","supergroup"|"group"=>"👥","private"=>"👤","bot"=>"🤖", _=>"•" }
}

fn load_catalog() -> Vec<(i64, String, String)> {
    let Ok(text) = fs::read_to_string(CHATS_EXPORT_PATH) else { return Vec::new() };
    let mut out = Vec::new();
    for (i, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') { continue; }
        if i == 0 && line.to_ascii_lowercase().starts_with("type;") { continue; }
        let mut p = line.splitn(4, ';').map(|s| s.trim());
        let typ = p.next().unwrap_or_default().to_ascii_lowercase();
        let id: i64 = p.next().and_then(|s| s.parse().ok()).unwrap_or(0);
        let title = p.next().unwrap_or_default();
        if id == 0 || title.is_empty() { continue; }
        out.push((id, title.to_string(), typ));
    }
    out.sort_by(|a,b| a.1.to_lowercase().cmp(&b.1.to_lowercase()));
    out
}

pub fn build_catalog_keyboard(page: usize) -> InlineKeyboardMarkup {
    let items = load_catalog();
    let total = items.len();
    let pages = total.max(1).div_ceil(10);
    let cur = page.clamp(1, pages);
    let start = (cur - 1) * PAGE_SIZE;
    let end = total.min(start + PAGE_SIZE);

    let mut rows: Vec<Vec<InlineKeyboardButton>> = Vec::new();
    for (_id, title, ctype) in &items[start..end] {
        rows.push(vec![InlineKeyboardButton::callback(
            format!("{} {}", type_icon(ctype), title),
            "cat:noop".to_string(),
        )]);
    }
    if pages > 1 {
        let mut nav = Vec::new();
        if cur > 1 { nav.push(InlineKeyboardButton::callback("⬅️ Prev", format!("cat:page:{}", cur-1))); }
        nav.push(InlineKeyboardButton::callback(format!("Page {cur}/{pages}"), "cat:noop".to_string()));
        if cur < pages { nav.push(InlineKeyboardButton::callback("Next ➡️", format!("cat:page:{}", cur+1))); }
        rows.push(nav);
    }
    InlineKeyboardMarkup::new(rows)
}

pub async fn show_catalog(bot: &Bot, chat_id: ChatId, page: usize) {
    if load_catalog().is_empty() {
        let _ = bot.send_message(chat_id, "No chats found in src/chats_export.csv").await;
        return;
    }
    let _ = bot
        .send_message(chat_id, "Browse your chats (buttons are inert):")
        .reply_markup(build_catalog_keyboard(page))
        .await;
}