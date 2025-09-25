use anyhow::{anyhow, bail, Context, Result};
use reqwest::{header::HeaderMap, Client};
use serde::{Deserialize, Serialize};
use serde_json::json;
use teloxide::payloads::{AnswerCallbackQuerySetters, SendMessageSetters};
use std::{env, fs, path::PathBuf};
use teloxide::prelude::{Bot, Requester};
use teloxide::types::{CallbackQuery, ChatId, InlineKeyboardButton, InlineKeyboardMarkup, ReplyMarkup, UserId};
use std::time::{SystemTime, UNIX_EPOCH, Duration};
use chrono::{DateTime, TimeZone};

const API_BASE: &str = "https://pay.crypt.bot/api";
const DEFAULT_ASSET: &str = "USDT";
const DEFAULT_AMOUNT: &str = "42";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Subscriber {
    pub user_id: i64,
    pub username: Option<String>,
    pub added_at: u64,               // epoch seconds
    pub expires_at: Option<u64>,     // epoch seconds; None = neverexpires
}

// new small struct to persist sent reminders
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ReminderRecord {
    user_id: i64,
    expires_at: u64,
}

// Core payments state
#[derive(Debug, Clone)]
pub struct Payments {
    client: Client,
    token: String,
    asset: String,
    amount: String,
    disabled_marker: PathBuf,
    enabled_env: Option<bool>,
}

// Common API envelope used by Crypto Pay API
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ApiEnvelope<T> {
    ok: bool,
    result: Option<T>,
    error: Option<String>,
}

// Minimal Invoice shape we use
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Invoice {
    pub invoice_id: i64,
    pub status: String,
    #[serde(default)]
    pub pay_url: Option<String>,
    #[serde(default)]
    pub payload: Option<String>,
}

// Result shape for getInvoices
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct InvoicesResult {
    items: Vec<Invoice>,
}

impl Payments {
    pub fn new_from_env() -> Result<Self> {
        let token = env::var("CRYPTO_PAY_API_TOKEN")
            .context("CRYPTO_PAY_API_TOKEN not set in .env (from @CryptoBot -> Crypto Pay API)")?;
        let asset = env::var("PAY_ASSET").unwrap_or_else(|_| DEFAULT_ASSET.to_string());
        let amount = env::var("PAY_AMOUNT").unwrap_or_else(|_| DEFAULT_AMOUNT.to_string());
        let client = Client::builder()
            .user_agent("final_try/1.0 (+teloxide)")
            .timeout(std::time::Duration::from_secs(20))
            .build()?;

        let disabled_marker = PathBuf::from("data").join("payments.disabled");
        // filepath: c:\Users\User\final_try\src\payments.rs
        let enabled_env = std::env::var("PAYMENTS_ENABLED")
            .ok()
            .and_then(|raw| {
                let v = raw.split('#').next().unwrap_or("").trim().to_ascii_lowercase();
                match v.as_str() {
                    "1" | "true" | "on" => Some(true),
                    "0" | "false" | "off" => Some(false),
                    _ => None,
                }
            });

        Ok(Self {
            client,
            token,
            asset,
            amount,
            disabled_marker,
            enabled_env,
        })
    }

    pub fn enabled(&self) -> bool {
        if let Some(flag) = self.enabled_env {
            return flag;
        }
        !self.disabled_marker.exists()
    }

    fn auth_headers(&self) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            "Crypto-Pay-API-Token",
            self.token.parse().expect("valid header value"),
        );
        headers.insert("Accept", "application/json".parse().unwrap());
        headers
    }

    pub async fn create_invoice(&self, payload: Option<String>) -> Result<Invoice> {
        if !self.enabled() {
            bail!("payments disabled");
        }
        let json_body = serde_json::json!({
            "asset": self.asset,
            "amount": self.amount,
            "description": "Subscription",
            "allow_comments": false,
            "allow_anonymous": true,
            "payload": payload.unwrap_or_default(),
        });

        // Try JSON (expected by API)
        let resp = self
            .client
            .post(format!("{API_BASE}/createInvoice"))
            .headers(self.auth_headers())
            .json(&json_body)
            .send()
            .await
            .context("createInvoice request failed")?;

        if resp.status().is_success() {
            let env: ApiEnvelope<Invoice> =
                resp.json().await.context("parse createInvoice json")?;
            if env.ok {
                return env
                    .result
                    .ok_or_else(|| anyhow!("createInvoice: empty result"));
            } else {
                bail!(env.error.unwrap_or_else(|| "createInvoice: unknown error".into()));
            }
        } else {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();

            // Fallback: retry once as form (some proxies break JSON)
            let resp2 = self
                .client
                .post(format!("{API_BASE}/createInvoice"))
                .headers(self.auth_headers())
                .form(&[
                    ("asset", self.asset.as_str()),
                    ("amount", self.amount.as_str()),
                    ("description", "Subscription"),
                    ("allow_comments", "false"),
                    ("allow_anonymous", "true"),
                    ("payload", ""),
                ])
                .send()
                .await
                .context("createInvoice form request failed")?;

            if resp2.status().is_success() {
                let env: ApiEnvelope<Invoice> =
                    resp2.json().await.context("parse createInvoice form json")?;
                if env.ok {
                    return env
                        .result
                        .ok_or_else(|| anyhow!("createInvoice(form): empty result"));
                } else {
                    bail!(env.error.unwrap_or_else(|| "createInvoice(form): unknown error".into()));
                }
            }

            bail!(format!(
                "createInvoice http {} body: {}",
                status.as_u16(),
                body.trim()
            ));
        }
    }

    pub async fn get_invoice(&self, id: i64) -> Result<Option<Invoice>> {
        let resp = self
            .client
            .post(format!("{API_BASE}/getInvoices"))
            .headers(self.auth_headers())
            .json(&serde_json::json!({ "invoice_ids": [id] }))
            .send()
            .await
            .context("getInvoices request failed")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            bail!(format!("getInvoices http {} body: {}", status.as_u16(), body.trim()));
        }

        let env: ApiEnvelope<InvoicesResult> =
            resp.json().await.context("parse getInvoices")?;
        if !env.ok {
            bail!(env.error.unwrap_or_else(|| "getInvoices: unknown error".into()));
        }
        Ok(env.result.map(|r| r.items.into_iter().next()).flatten())
    }
    
    pub fn start_button(&self) -> InlineKeyboardMarkup {
        InlineKeyboardMarkup::new(vec![vec![InlineKeyboardButton::callback(
            format!("Оплатить {} {}", self.amount, self.asset),
            "pay:start",
        )]])
    }
    
    pub fn check_button(&self, invoice: &Invoice) -> InlineKeyboardMarkup {
        let mut rows = Vec::new();
        if let Some(url_str) = &invoice.pay_url {
            if let Ok(url) = reqwest::Url::parse(url_str) {
                rows.push(vec![InlineKeyboardButton::url(
                    format!("Оплатить {} {}", self.amount, self.asset),
                    url,
                )]);
            }
        }
        rows.push(vec![InlineKeyboardButton::callback(
            "Я оплатил — проверить",
            format!("pay:check:{}", invoice.invoice_id),
        )]);
        InlineKeyboardMarkup::new(rows)
    }
    
    pub fn disable_now(&self) {
        if let Some(true) = self.enabled_env {
            return;
        }
        let _ = fs::create_dir_all(
            self.disabled_marker
                .parent()
                .unwrap_or_else(|| std::path::Path::new(".")),
        );
        let _ = fs::write(&self.disabled_marker, b"");
    }

    pub fn enable_now(&self) {
        if self.disabled_marker.exists() {
            let _ = fs::remove_file(&self.disabled_marker);
        }
    }

    // Subscriber persistence (store active subscribers in data/subscribers.json) ---------


    pub fn subscribers_path(&self) -> PathBuf {
        self.disabled_marker
            .parent()
            .unwrap_or_else(|| std::path::Path::new("."))
            .join("subscribers.json")
    }

    pub fn list_subscribers(&self) -> Result<Vec<Subscriber>> {
        let path = self.subscribers_path();
        if !path.exists() {
            return Ok(vec![]);
        }
        let data = fs::read_to_string(&path)
            .with_context(|| format!("reading subscribers file {:?}", path))?;
        let subs: Vec<Subscriber> =
            serde_json::from_str(&data).context("parsing subscribers json")?;
        Ok(subs)
    }

    /// Adds the subscriber if not present. No removal implemented (per request).
    pub fn add_subscriber(&self, user_id: i64, username: Option<String>) -> Result<()> {
        let mut subs = self.list_subscribers()?;
        if subs.iter().any(|s| s.user_id == user_id) {
            return Ok(()); // already present
        }
        let added_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        subs.push(Subscriber {
            user_id,
            username,
            added_at,
            expires_at: None,
        });

        let path = self.subscribers_path();
        let _ = fs::create_dir_all(path.parent().unwrap_or_else(|| std::path::Path::new(".")));
        let json = serde_json::to_string_pretty(&subs).context("serializing subscribers")?;
        fs::write(&path, json).with_context(|| format!("writing subscribers file {:?}", path))?;
        Ok(())
    }
    /// Add or renew subscriber for `months` months (30-day months used here).
    pub fn add_or_renew_subscriber(
        &self,
        user_id: i64,
        username: Option<String>,
        months: u64,
    ) -> Result<()> {
        let mut subs = self.list_subscribers()?;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let period_secs = months.saturating_mul(30 * 24 * 60 * 60);

        // If subscriber exists and still active, extend from existing expiry,
        // otherwise start from now.
        let mut found = false;
        for s in subs.iter_mut() {
            if s.user_id == user_id {
                if let Some(name) = username.clone() {
                    s.username = Some(name);
                }
                let new_expires = match s.expires_at {
                    Some(existing) if existing > now => existing.saturating_add(period_secs),
                    _ => now.saturating_add(period_secs),
                };
                s.expires_at = Some(new_expires);
                found = true;
                break;
            }
        }

        if !found {
            subs.push(Subscriber {
                user_id,
                username,
                added_at: now,
                expires_at: Some(now.saturating_add(period_secs)),
            });
        }

        let path = self.subscribers_path();
        let _ = fs::create_dir_all(path.parent().unwrap_or_else(|| std::path::Path::new(".")));
        let json = serde_json::to_string_pretty(&subs).context("serializing subscribers")?;
        fs::write(&path, json).with_context(|| format!("writing subscribers file {:?}", path))?;
        Ok(())
    }
    
    // Reminder persistence ----------------------------------------------------
    fn reminders_path(&self) -> PathBuf {
        self.disabled_marker
            .parent()
            .unwrap_or_else(|| std::path::Path::new("."))
            .join("reminders.json")
    }

    fn load_reminders(&self) -> Result<Vec<ReminderRecord>> {
        let path = self.reminders_path();
        if !path.exists() {
            return Ok(vec![]);
        }
        let s = fs::read_to_string(&path)
            .with_context(|| format!("reading reminders file {:?}", path))?;
        if s.trim().is_empty() {
            Ok(vec![])
        } else {
            let recs: Vec<ReminderRecord> =
                serde_json::from_str(&s).context("parsing reminders json")?;
            Ok(recs)
        }
    }

    fn save_reminders(&self, recs: &[ReminderRecord]) -> Result<()> {
        let path = self.reminders_path();
        let _ = fs::create_dir_all(path.parent().unwrap_or_else(|| std::path::Path::new(".")));
        let json = serde_json::to_string_pretty(recs).context("serializing reminders")?;
        fs::write(&path, json).with_context(|| format!("writing reminders file {:?}", path))?;
        Ok(())
    }

    /// Path to src/subs.json used as a flat ledger of every payment (user_id, paid_at, expires_at).
    pub fn src_subs_path(&self) -> PathBuf {
        // use current working directory to resolve reliably (e.g., from target/debug or project root)
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        cwd.join("src").join("subs.json")
    }

    /// Append a subscription record to src/subs.json (creates file if missing).
    pub fn append_subscription_record(&self, user_id: i64, months: u64) -> Result<()> {
        let path = self.src_subs_path();
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let period_secs = months.saturating_mul(30 * 24 * 60 * 60);
        let expires = now.saturating_add(period_secs);

        // format as DD/MM/YYYY (ensure chrono is in Cargo.toml)
        let paid_dt = chrono::Utc.timestamp_opt(now as i64, 0).single().unwrap_or_else(|| chrono::Utc.timestamp_opt(0, 0).single().unwrap());
        let expires_dt = chrono::Utc.timestamp_opt(expires as i64, 0).single().unwrap_or_else(|| chrono::Utc.timestamp_opt(0, 0).single().unwrap());
        let paid_str = paid_dt.format("%d/%m/%Y").to_string();
        let expires_str = expires_dt.format("%d/%m/%Y").to_string();

        // read existing array or create
        let mut arr = if path.exists() {
            let s = fs::read_to_string(&path)
                .with_context(|| format!("reading subs json {:?}", path))?;
            if s.trim().is_empty() {
                Vec::new()
            } else {
                serde_json::from_str::<Vec<serde_json::Value>>(&s)
                    .unwrap_or_else(|_| Vec::new())
            }
        } else {
            Vec::new()
        };

        arr.push(json!({
            "user_id": user_id,
            "paid_at": now,
            "expires_at": expires,
            "paid_date": paid_str,
            "expires_date": expires_str
        }));

        let _ = fs::create_dir_all(path.parent().unwrap_or_else(|| std::path::Path::new(".")));
        let out = serde_json::to_string_pretty(&arr).context("serializing subs ledger")?;
        fs::write(&path, out).with_context(|| format!("writing subs ledger {:?}", path))?;

        // log success to help debug
        eprintln!("payments: appended subs.json record for user {} -> {:?}", user_id, path);
        Ok(())
    }
    /// invoice.payload as a Telegram user_id (i64). On success it will add/renew the
    /// invoice.payload as a Telegram user_id (i64). On success it will add/renew the
    /// subscriber for `months` months. If `bot` + `unban_chat_ids` are provided the
    /// method will unban the user from those chats. If `folder_buttons` is provided
    /// it will be sent to the user in a private message.
        pub async fn import_paid_invoices_and_restore(
            &self,
            invoice_ids: &[i64],
            months: u64,
            bot: Option<Bot>,
            unban_chat_ids: Option<&[i64]>,
            folder_buttons: Option<ReplyMarkup>,
        ) -> Result<Vec<i64>> {
        let mut restored = Vec::new();
        // replace dynamic link send with static invite link:
        let static_link = std::env::var("STATIC_INVITE_LINK").ok();
        for &id in invoice_ids {
            if let Some(inv) = self.get_invoice(id).await.context("fetch invoice")? {
                if inv.status != "paid" {
                    continue;
                }
                if let Some(payload) = inv.payload.as_deref() {
                    if let Ok(user_id) = payload.parse::<i64>() {
                        // update internal subscribers list (data/subscribers.json)
                        self.add_or_renew_subscriber(user_id, None, months)?;
                        // append ledger record to src/subs.json
                        self.append_subscription_record(user_id, months)?;
                        restored.push(user_id);
    
                        // try to unban from target chats if bot and chat ids provided
                        if let (Some(bot), Some(chat_ids)) = (bot.as_ref(), unban_chat_ids) {
                            for &chat in chat_ids {
                                let _ = bot
                                    .unban_chat_member(ChatId(chat), UserId(user_id as u64))
                                    .await;
                            }
                        }
    
                        // send static link (if set) or fallback message + folder buttons
                        if let Some(bot) = bot.as_ref() {
                            let text = if let Some(link) = static_link.as_deref() {
                                format!("Спасибо — доступ восстановлен. Hаш чат: {}", link)
                            } else {
                                "Спасибо — доступ восстановлен. Вот ваши кнопки:".to_string()
                            };
                            if let Some(kb) = folder_buttons.clone() {
                                let _ = bot.send_message(ChatId(user_id), text).reply_markup(kb).await;
                            } else {
                                let _ = bot.send_message(ChatId(user_id), text).await;
                            }
                        }
                    }
                }
            }
        }
        Ok(restored)
    }

    /// Send reminders to subscribers whose subscription expires in 3 days (72h).
    /// Returns list of user_ids that were messaged.
    pub async fn send_expiry_reminders(&self, bot: &Bot) -> Result<Vec<i64>> {
        let latest_expiries = self.load_latest_expiries_from_subs()?;
        let mut reminders = self.load_reminders().unwrap_or_default();
        let mut already = std::collections::HashSet::new();
        for r in &reminders {
            already.insert((r.user_id, r.expires_at));
        }

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let window_end = now.saturating_add(3 * 24 * 60 * 60); // 3 days

        let mut sent = Vec::new();
        for (&user_id, &exp) in &latest_expiries {
            if exp > now && exp <= window_end {
                if already.contains(&(user_id, exp)) {
                    continue; // already reminded for this expiry
                }

                // russian message base text
                let base_text = "Ваша подписка истекает через 3 дня, пожалуйста оплатите, чтобы продолжить пользоваться нашими услугами";

                // try to create an invoice with payload = user_id so we get a pay_url
                let invoice_res = self.create_invoice(Some(user_id.to_string())).await;
                match invoice_res {
                    Ok(inv) => {
                        // prefer sending a message with pay_url and a "проверить" button
                        let mut message_text = base_text.to_string();
                        if let Some(url) = inv.pay_url.as_deref() {
                            message_text.push_str("\n\nСсылка для оплаты:\n");
                            message_text.push_str(url);
                        }

                        // build keyboard: first row -> "Продлить подписку" (URL), second row -> "Я оплатил — проверить"
                        let mut rows = Vec::new();
                        if let Some(url_str) = inv.pay_url.as_deref() {
                            if let Ok(url) = reqwest::Url::parse(url_str) {
                                rows.push(vec![InlineKeyboardButton::url(
                                    "Продлить подписку".to_string(),
                                    url,
                                )]);
                            }
                        }
                        rows.push(vec![InlineKeyboardButton::callback(
                            "Я оплатил — проверить",
                            format!("pay:check:{}", inv.invoice_id),
                        )]);
                        let kb = InlineKeyboardMarkup::new(rows);

                        match bot.send_message(ChatId(user_id), message_text).reply_markup(kb).await {
                            Ok(_) => {
                                sent.push(user_id);
                                reminders.push(ReminderRecord { user_id, expires_at: exp });
                            }
                            Err(err) => {
                                eprintln!("failed to send reminder with invoice to {}: {:?}", user_id, err);
                            }
                        }
                    }
                    Err(err) => {
                        // fallback: just send plain text reminder if invoice creation fails
                        match bot.send_message(ChatId(user_id), base_text).await {
                            Ok(_) => {
                                sent.push(user_id);
                                reminders.push(ReminderRecord { user_id, expires_at: exp });
                            }
                            Err(err2) => {
                                eprintln!("failed to send reminder to {}: {:?} (invoice error: {:?})", user_id, err2, err);
                            }
                        }
                    }
                }
            }
        }

        // persist reminders so we don't spam repeatedly
        let _ = self.save_reminders(&reminders);
        Ok(sent)
    }

    /// Spawn a background task that runs send_expiry_reminders every `interval_seconds`.
    /// Example usage: let _handle = payments.spawn_reminder_loop(bot.clone(), 3600);
    pub fn spawn_reminder_loop(self: std::sync::Arc<Self>, bot: Bot, interval_seconds: u64) {
        tokio::spawn({
            let bot = bot.clone();
            async move {
                let interval = Duration::from_secs(interval_seconds);
                loop {
                    if let Err(err) = self.send_expiry_reminders(&bot).await {
                        eprintln!("send_expiry_reminders error: {:?}", err);
                    }
                    tokio::time::sleep(interval).await;
                }
            }
        });
    }
    
    // Single-button helper to compose keyboards
    pub fn pay_button(&self) -> InlineKeyboardButton {
        InlineKeyboardButton::callback(
            format!("Оплатить {} {}", self.amount, self.asset),
            "pay:start",
        )
    }

    /// Load target chat ids from a CSV file (one id per line or comma-separated).
    pub fn load_target_chats(&self, path: &str) -> Result<Vec<i64>> {
        let s = fs::read_to_string(path).with_context(|| format!("reading chats csv {}", path))?;
        let mut out = Vec::new();
        for line in s.lines() {
            for part in line.split(&[',', ';'][..]) {
                let t = part.trim();
                if t.is_empty() { continue; }
                if let Ok(id) = t.parse::<i64>() {
                    out.push(id);
                } else {
                    // try to strip quotes or stray characters
                    let cleaned = t.trim_matches(|c: char| !c.is_numeric() && c != '-' );
                    if let Ok(id) = cleaned.parse::<i64>() {
                        out.push(id);
                    }
                }
            }
        }
        Ok(out)
    }

    /// Removes expired subscribers from data/subscribers.json and returns their user IDs.
    

    /// Prune expired subscribers (removes them from data/subscribers.json), notify them,
    /// and ban each removed user from every chat listed in `chats_csv` (default "chats2.csv").
    /// Returns list of (user_id, chat_id) pairs attempted.
    /// 
    

    /// Handle callback like "pay:check:{invoice_id}".

    /// If invoice is paid and payload contains a telegram user_id, renew subscription and

    /// append a record to src/subs.json, then notify the user.

    pub async fn handle_check_callback(

        &self,

        bot: &Bot,

        cq: CallbackQuery,

    ) -> Result<()> {
        let data = cq.data.unwrap_or_default();
        if !data.starts_with("pay:check:") {
            return Ok(());
        }
        // acknowledge callback immediately
        let _ = bot
            .answer_callback_query(&cq.id)
            .text("Проверяю платёж...")
            .await;

        let id_str = &data["pay:check:".len()..];
        let invoice_id: i64 = id_str
            .parse()
            .context("failed to parse invoice id from callback")?;

        let inv_opt = self.get_invoice(invoice_id).await.context("get_invoice")?;
        match inv_opt {
            Some(inv) if inv.status == "paid" => {
                if let Some(payload) = inv.payload.as_deref() {
                    if let Ok(user_id) = payload.parse::<i64>() {
                        // renew for 1 month
                        if let Err(e) = self.add_or_renew_subscriber(user_id, None, 1) {
                            eprintln!("payments: add_or_renew_subscriber failed for {}: {:?}", user_id, e);
                        }
                        if let Err(e) = self.append_subscription_record(user_id, 1) {
                            eprintln!("payments: append_subscription_record failed for {}: {:?}", user_id, e);
                        }
                        
                        // send only the folders message with buttons
                        // --- NEW: send "folders with channels" message with 4 buttons ---
                        let mut rows = Vec::new();
                        // make "АКТИВНОСТИ +" open the provided t.me/addlist link
                        let aktiv_url = reqwest::Url::parse("https://t.me/addlist/Q3mkHDAfwjYyYjU0").ok();
                        let ludiki_url = reqwest::Url::parse("https://t.me/addlist/TyvbTgRFp5QwY2Y0").ok();
                        match (aktiv_url, ludiki_url) {
                            (Some(aktiv), Some(ludiki)) => {
                                rows.push(vec![
                                    InlineKeyboardButton::url("АКТИВНОСТИ +".to_string(), aktiv),
                                    InlineKeyboardButton::url("ЛУДИКИ".to_string(), ludiki),
                                ]);
                            }
                            (Some(aktiv), None) => {
                                rows.push(vec![
                                    InlineKeyboardButton::url("АКТИВНОСТИ +".to_string(), aktiv),
                                    InlineKeyboardButton::callback("ЛУДИКИ".to_string(), "show_folder:ludiki".to_string()),
                                ]);
                            }
                            (None, Some(ludiki)) => {
                                rows.push(vec![
                                    InlineKeyboardButton::callback("АКТИВНОСТИ +".to_string(), "show_folder:aktivnosti".to_string()),
                                    InlineKeyboardButton::url("ЛУДИКИ".to_string(), ludiki),
                                ]);
                            }
                            (None, None) => {
                                // both failed -> fallback to callbacks
                                rows.push(vec![
                                    InlineKeyboardButton::callback("АКТИВНОСТИ +".to_string(), "show_folder:aktivnosti".to_string()),
                                    InlineKeyboardButton::callback("ЛУДИКИ".to_string(), "show_folder:ludiki".to_string()),
                                ]);
                            }
                        }
                        // make "Фармилка" and "Прочее" open the provided t.me/addlist links, fallback to callbacks if parse fails
                        let farm_url = reqwest::Url::parse("https://t.me/addlist/qzsI2WN7hXExNTdk").ok();
                        let other_url = reqwest::Url::parse("https://t.me/addlist/Gy2SNd_HDPNjNmY0").ok();
                        match (farm_url, other_url) {
                            (Some(farm), Some(other)) => {
                                rows.push(vec![
                                    InlineKeyboardButton::url("Фармилка".to_string(), farm),
                                    InlineKeyboardButton::url("Прочее".to_string(), other),
                                ]);
                            }
                            (Some(farm), None) => {
                                rows.push(vec![
                                    InlineKeyboardButton::url("Фармилка".to_string(), farm),
                                    InlineKeyboardButton::callback("Прочее".to_string(), "show_folder:other".to_string()),
                                ]);
                            }
                            (None, Some(other)) => {
                                rows.push(vec![
                                    InlineKeyboardButton::callback("Фармилка".to_string(), "show_folder:farmilka".to_string()),
                                    InlineKeyboardButton::url("Прочее".to_string(), other),
                                ]);
                            }
                            (None, None) => {
                                rows.push(vec![
                                    InlineKeyboardButton::callback("Фармилка".to_string(), "show_folder:farmilka".to_string()),
                                    InlineKeyboardButton::callback("Прочее".to_string(), "show_folder:other".to_string()),
                                ]);
                            }
                        }
                         let kb = InlineKeyboardMarkup::new(rows);
                         let _ = bot
                             .send_message(ChatId(user_id), "Папки с каналами:")
                             .reply_markup(kb)
                             .await;
                         // --- END NEW ---

                        // notify callback initiator if different
                        let from = &cq.from;
                        if (from.id.0 as i64) != user_id {
                            let _ = bot
                                .send_message(
                                    ChatId(from.id.0 as i64),
                                    "Платёж обнаружен и подписка продлена.",
                                )
                                .await;
                        }
                        let _ = bot
                            .answer_callback_query(&cq.id)
                            .text("Платёж оплачен — подписка обновлена.")
                            .await;
                        return Ok(());
                    }
                }
                // paid but payload missing/invalid
                let _ = bot
                    .answer_callback_query(&cq.id)
                    .text("Платёж найден, но не удалось распознать пользователя.")
                    .await;
                return Ok(());
            }
            Some(_) => {
                let _ = bot
                    .answer_callback_query(&cq.id)
                    .text("Платёж ещё не оплачен.")
                    .await;
                return Ok(());
            }
            None => {
                let _ = bot
                    .answer_callback_query(&cq.id)
                    .text("Инвойс не найден.")
                    .await;
                return Ok(());
            }
        }
    }

    /// Unban a user from all chats listed in `chats_csv` (default "chats2.csv").
    /// Returns list of chat ids attempted.
    pub async fn unban_user_from_chats(
        &self,
        bot: &teloxide::prelude::Bot,
        user_id: i64,
        chats_csv: Option<&str>,
    ) -> Result<Vec<i64>> {
        let csv_path = chats_csv.unwrap_or("chats2.csv");
        let chats = match self.load_target_chats(csv_path) {
            Ok(c) if !c.is_empty() => c,
            _ => return Ok(vec![]),
        };

        let mut succeeded = Vec::new();
        for &chat in &chats {
            match bot.unban_chat_member(ChatId(chat), UserId(user_id as u64)).await {
                Ok(_) => {
                    succeeded.push(chat);
                }
                Err(err) => {
                    // log and continue
                    eprintln!("unban failed for user {} in chat {}: {:?}", user_id, chat, err);
                }
            }
        }
        Ok(succeeded)
    }

    /// Load the latest subscription expiry per user from src/subs.json (ledger).
    /// Returns a map of user_id -> max expires_at (epoch seconds).
    pub fn load_latest_expiries_from_subs(&self) -> Result<std::collections::HashMap<i64, u64>> {
        let path = self.src_subs_path();
        println!("DEBUG: Reading subs.json from {:?}", path);
        if !path.exists() {
            println!("DEBUG: subs.json does not exist");
            return Ok(std::collections::HashMap::new());
        }
        let s = fs::read_to_string(&path)
            .with_context(|| format!("reading subs ledger {:?}", path))?;
        println!("DEBUG: Raw subs.json content: {}", s);
        let arr: Vec<serde_json::Value> = if s.trim().is_empty() {
            Vec::new()
        } else {
            serde_json::from_str(&s).unwrap_or_else(|_| Vec::new())
        };
        println!("DEBUG: Parsed arr: {:?}", arr);

        let mut latest: std::collections::HashMap<i64, u64> = std::collections::HashMap::new();
        for rec in arr {
            println!("DEBUG: Processing record: {:?}", rec);
            if let (Some(user_id), Some(expires)) = (
                rec.get("user_id").and_then(|v| v.as_i64()),
                rec.get("expires_at").and_then(|v| v.as_u64()),
            ) {
                println!("DEBUG: Extracted user_id: {}, expires: {}", user_id, expires);
                latest.entry(user_id).and_modify(|e| *e = (*e).max(expires)).or_insert(expires);
            } else {
                println!("DEBUG: Failed to extract user_id or expires from record");
            }
        }
        println!("DEBUG: Final latest map: {:?}", latest);
        Ok(latest)
    }

    /// Prune expired subscribers based on subs.json ledger, remove from both subscribers.json and subs.json,
    /// and ban them from chats listed in `chats_csv` (default "chats2.csv").
    pub async fn prune_and_ban_expired_subscribers(&self, bot: &Bot, chats_csv: Option<&str>) -> Result<Vec<(i64,i64)>> {
        // Load latest expiries from subs.json
        let latest_expiries = self.load_latest_expiries_from_subs()?;
        println!("DEBUG: Loaded expiries: {:?}", latest_expiries);
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);

        println!("DEBUG: Current time (epoch): {}", now);
        let mut removed = Vec::new();
        for (&user_id, &exp) in &latest_expiries {
            println!("DEBUG: Checking user {} with exp {}", user_id, exp);
            if exp <= now {
                println!("DEBUG: User {} is expired (exp {} <= now {})", user_id, exp, now);
                removed.push(user_id);
            } else {
                println!("DEBUG: User {} is not expired (exp {} > now {})", user_id, exp, now);
            }
        }

        if removed.is_empty() {
            println!("DEBUG: No expired users found");
            return Ok(vec![]);
        }

        // Remove from data/subscribers.json (if present)
        let subs_path = self.subscribers_path();
        if subs_path.exists() {
            let mut subs: Vec<Subscriber> = serde_json::from_str(&fs::read_to_string(&subs_path)?).unwrap_or_default();
            subs.retain(|s| !removed.contains(&s.user_id));
            let _ = fs::write(&subs_path, serde_json::to_string_pretty(&subs).unwrap_or_default());
        }

        // Remove from src/subs.json
        let src_path = self.src_subs_path();
        if src_path.exists() {
            let s = fs::read_to_string(&src_path)?;
            let mut arr: Vec<serde_json::Value> = if s.trim().is_empty() {
                Vec::new()
            } else {
                serde_json::from_str(&s).unwrap_or_else(|_| Vec::new())
            };
            let removed_set: std::collections::HashSet<i64> = removed.iter().cloned().collect();
            arr.retain(|v| {
                v.get("user_id")
                    .and_then(|u| u.as_i64())
                    .map(|id| !removed_set.contains(&id))
                    .unwrap_or(true)
            });
            let _ = fs::write(&src_path, serde_json::to_string_pretty(&arr).unwrap_or_default());
        }

        // Ban from chats
        let csv_path = chats_csv.unwrap_or("chats2.csv");
        let chats = match self.load_target_chats(csv_path) {
            Ok(c) if !c.is_empty() => c,
            _ => {
                println!("DEBUG: No chats loaded from {}", csv_path);
                return Ok(vec![]);
            }
        };

        println!("DEBUG: Found {} expired users, {} chats to ban from", removed.len(), chats.len());
        let mut attempted = Vec::new();
        for user_id in removed.iter() {
            for &chat in &chats {
                attempted.push((*user_id, chat));
                println!("DEBUG: Attempting to ban user {} from chat {}", user_id, chat);
                match bot.ban_chat_member(ChatId(chat), UserId(*user_id as u64)).await {
                    Ok(_) => println!("DEBUG: Successfully banned user {} from chat {}", user_id, chat),
                    Err(err) => eprintln!("DEBUG: Failed to ban user {} from chat {}: {:?}", user_id, chat, err),
                }
            }
        }

        Ok(attempted)
    }
}







