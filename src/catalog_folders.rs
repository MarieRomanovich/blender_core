use std::{fs, sync::OnceLock};

use teloxide::{
    prelude::*,
    types::{InlineKeyboardButton, InlineKeyboardMarkup},
};

const CHATS_EXPORT_PATH: &str = "src/chats_export.csv";
const PAGE_SIZE_ITEMS: usize = 10;
const PAGE_SIZE_FOLDERS: usize = 8;

#[derive(Clone)]
struct Item {
    id: i64,
    title: String,
    ctype: String,
    folders: Vec<String>, // multiple folders supported
}

fn type_icon(t: &str) -> &'static str {
    match t {
        "channel" => "📣",
        "supergroup" | "group" => "👥",
        "private" => "👤",
        "bot" => "🤖",
        _ => "•",
    }
}

// Parse CSV: type;id;title;username[;folders] where folders like: Work|Personal (or comma separated)
fn load_items_from_disk() -> Vec<Item> {
    let Ok(text) = fs::read_to_string(CHATS_EXPORT_PATH) else { return Vec::new() };
    let mut out = Vec::new();
    for (lineno, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if lineno == 0 && line.to_ascii_lowercase().starts_with("type;") {
            continue;
        }
        let mut parts = line.splitn(5, ';').map(|s| s.trim());
        let typ = parts.next().unwrap_or_default().to_ascii_lowercase();
        let id_str = parts.next().unwrap_or_default();
        let title = parts.next().unwrap_or_default();
        let _username = parts.next(); // unused for UI

        let Ok(id) = id_str.parse::<i64>() else { continue };
        if title.is_empty() {
            continue;
        }

        // folders column (optional)
        let folders_raw = parts.next().unwrap_or_default();
        let mut folders: Vec<String> = if folders_raw.is_empty() {
            vec!["All".to_string()]
        } else {
            // allow both | and , as separators
            folders_raw
                .split(|c| c == '|' || c == ',')
                .map(|s| s.trim())
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string())
                .collect()
        };
        folders.sort_by(|a, b| a.to_lowercase().cmp(&b.to_lowercase()));
        folders.dedup();

        out.push(Item {
            id,
            title: title.to_string(),
            ctype: typ,
            folders,
        });
    }
    out.sort_by(|a, b| a.title.to_lowercase().cmp(&b.title.to_lowercase()));
    out
}

fn load_items() -> Vec<Item> {
    static CATALOG: OnceLock<Vec<Item>> = OnceLock::new();

    CATALOG
        .get_or_init(|| load_items_from_disk())
        .clone() // cheap clone of Vec; items are small
}

fn list_folders(items: &[Item]) -> Vec<String> {
    let mut names: Vec<String> = items
        .iter()
        .flat_map(|i| i.folders.iter().cloned())
        .collect();
    names.sort_by(|a, b| a.to_lowercase().cmp(&b.to_lowercase()));
    names.dedup();
    names
}

fn items_in_folder<'a>(items: &'a [Item], folder: &str) -> Vec<&'a Item> {
    items
        .iter()
        .filter(|i| i.folders.iter().any(|f| f.eq(folder)))
        .collect()
}

// Top-level folders keyboard
pub fn build_folders_keyboard(page: usize) -> InlineKeyboardMarkup {
    let items = load_items();
    let folders = list_folders(&items);

    let total = folders.len();
    let pages = total.max(1usize).div_ceil(PAGE_SIZE_FOLDERS);
    let cur = page.clamp(1, pages);
    let start = (cur - 1) * PAGE_SIZE_FOLDERS;
    let end = total.min(start + PAGE_SIZE_FOLDERS);

    let mut rows: Vec<Vec<InlineKeyboardButton>> = Vec::new();
    for (idx, name) in folders[start..end].iter().enumerate() {
        let absolute_idx = start + idx;
        let count = items_in_folder(&items, name).len();
        let text = format!("📁 {} ({})", name, count);
        rows.push(vec![InlineKeyboardButton::callback(
            text,
            format!("catf:open:{absolute_idx}:1"),
        )]);
    }

    if pages > 1 {
        let mut nav = Vec::new();
        if cur > 1 {
            nav.push(InlineKeyboardButton::callback(
                "⬅️ Prev",
                format!("catf:folders_page:{}", cur - 1),
            ));
        }
        nav.push(InlineKeyboardButton::callback(
            format!("Folders {cur}/{pages}"),
            "catf:noop".to_string(),
        ));
        if cur < pages {
            nav.push(InlineKeyboardButton::callback(
                "Next ➡️",
                format!("catf:folders_page:{}", cur + 1),
            ));
        }
        rows.push(nav);
    }

    InlineKeyboardMarkup::new(rows)
}

// Items inside a folder
pub fn build_folder_items_keyboard(folder_index: usize, page: usize) -> InlineKeyboardMarkup {
    let items = load_items();
    let folders = list_folders(&items);
    let Some(folder_name) = folders.get(folder_index) else {
        return InlineKeyboardMarkup::default();
    };
    let in_folder = items_in_folder(&items, folder_name);

    let total = in_folder.len();
    let pages = total.max(1usize).div_ceil(PAGE_SIZE_ITEMS);
    let cur = page.clamp(1, pages);
    let start = (cur - 1) * PAGE_SIZE_ITEMS;
    let end = total.min(start + PAGE_SIZE_ITEMS);

    let mut rows: Vec<Vec<InlineKeyboardButton>> = Vec::new();
    for item in &in_folder[start..end] {
        rows.push(vec![InlineKeyboardButton::callback(
            format!("{} {}", type_icon(&item.ctype), item.title),
            "catf:noop".to_string(), // inert
        )]);
    }

    let mut nav = vec![InlineKeyboardButton::callback("◀️ Back", "catf:back".to_string())];
    if pages > 1 {
        if cur > 1 {
            nav.push(InlineKeyboardButton::callback(
                "⬅️ Prev",
                format!("catf:page:{folder_index}:{}", cur - 1),
            ));
        }
        nav.push(InlineKeyboardButton::callback(
            format!("Page {cur}/{pages}"),
            "catf:noop".to_string(),
        ));
        if cur < pages {
            nav.push(InlineKeyboardButton::callback(
                "Next ➡️",
                format!("catf:page:{folder_index}:{}", cur + 1),
            ));
        }
    }
    rows.push(nav);

    InlineKeyboardMarkup::new(rows)
}

pub async fn show_folders(bot: &Bot, chat_id: ChatId) {
    let items = load_items();
    if items.is_empty() {
        let _ = bot
            .send_message(chat_id, "No chats found in src/chats_export.csv")
            .await;
        return;
    }
    let _ = bot
        .send_message(chat_id, "Choose a folder:")
        .reply_markup(build_folders_keyboard(1))
        .await;
}