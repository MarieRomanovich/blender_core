use std::collections::HashSet;
use teloxide::{
    prelude::*,
    types::{InlineKeyboardButton, InlineKeyboardMarkup, KeyboardButton, KeyboardMarkup},
};
use crate::channels;

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

pub const BTN_FINISH_SELECTING: &str = "Finish selecting";
const PAGE_SIZE: usize = 10;

pub fn build_selection_keyboard(chat_id: ChatId, page: usize) -> InlineKeyboardMarkup {
    let all = channels::list_items_with_type().unwrap_or_default(); // (id, title, username, ctype)
    let selected: HashSet<i64> = channels::get_user_selected_ids(chat_id.0)
        .unwrap_or_default()
        .into_iter()
        .collect();

    let total = all.len();
    let pages = std::cmp::max(1, (total + PAGE_SIZE - 1) / PAGE_SIZE);
    let cur = std::cmp::min(std::cmp::max(1, page), pages);
    let start = (cur - 1) * PAGE_SIZE;
    let end = std::cmp::min(start + PAGE_SIZE, total);

    let mut rows: Vec<Vec<InlineKeyboardButton>> = Vec::new();
    for (id, title, _username, ctype) in &all[start..end] {
        let checked = selected.contains(id);
        let mut text = format!("{} {}", type_icon(ctype), title);
        if checked {
            text.push_str(" ✅");
        }
        rows.push(vec![InlineKeyboardButton::callback(
            text,
            format!("sel:toggle:{id}:{cur}"),
        )]);
    }

    if pages > 1 {
        let mut nav = Vec::new();
        if cur > 1 {
            nav.push(InlineKeyboardButton::callback("⬅️ Prev", format!("sel:page:{}", cur - 1)));
        }
        nav.push(InlineKeyboardButton::callback(
            format!("Page {cur}/{pages}"),
            "sel:noop".to_string(),
        ));
        if cur < pages {
            nav.push(InlineKeyboardButton::callback("Next ➡️", format!("sel:page:{}", cur + 1)));
        }
        rows.push(nav);
    }

    InlineKeyboardMarkup::new(rows)
}

pub fn finish_keyboard() -> KeyboardMarkup {
    KeyboardMarkup::new(vec![vec![KeyboardButton::new(BTN_FINISH_SELECTING)]])
        .resize_keyboard(true)
        .one_time_keyboard(false)
}

pub async fn start_channel_selection(bot: &Bot, chat_id: ChatId) {
    let _ = bot
        .send_message(chat_id, "Select channels you want notifications from:")
        .reply_markup(build_selection_keyboard(chat_id, 1))
        .await;
    let _ = bot
        .send_message(
            chat_id,
            "Tap buttons to toggle channels. When done, press the keyboard button below.",
        )
        .reply_markup(finish_keyboard())
        .await;
}