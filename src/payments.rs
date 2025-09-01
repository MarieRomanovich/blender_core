use anyhow::{anyhow, bail, Context, Result};
use reqwest::{header::HeaderMap, Client};
use serde::Deserialize;
use std::{env, fs, path::PathBuf};
use teloxide::types::{InlineKeyboardButton, InlineKeyboardMarkup};

const API_BASE: &str = "https://pay.crypt.bot/api";
const DEFAULT_ASSET: &str = "USDT";
const DEFAULT_AMOUNT: &str = "42";

#[derive(Debug, Clone)]
pub struct Payments {
    client: Client,
    token: String,
    asset: String,
    amount: String,
    disabled_marker: PathBuf,
    enabled_env: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct ApiEnvelope<T> {
    ok: bool,
    result: Option<T>,
    error: Option<String>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct Invoice {
    pub invoice_id: i64,
    pub status: String, // "active" | "paid" | "expired"
    pub asset: String,
    pub amount: String,
    pub pay_url: Option<String>,
}

#[derive(Debug, Deserialize)]
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
        let enabled_env = env::var("PAYMENTS_ENABLED")
            .ok()
            .and_then(|v| {
                // Allow inline comments and words (e.g., "0  # comment")
                let first = v.split('#').next().unwrap_or("").trim().to_ascii_lowercase();
                match first.as_str() {
                    "0" | "false" | "off" => Some(false),
                    "1" | "true" | "on" => Some(true),
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
            format!("Pay {} {}", self.amount, self.asset),
            "pay:start",
        )]])
    }

    pub fn check_button(&self, invoice: &Invoice) -> InlineKeyboardMarkup {
        let mut rows = Vec::new();
        if let Some(url_str) = &invoice.pay_url {
            if let Ok(url) = reqwest::Url::parse(url_str) {
                rows.push(vec![InlineKeyboardButton::url(
                    format!("Pay {} {}", self.amount, self.asset),
                    url,
                )]);
            }
        }
        rows.push(vec![InlineKeyboardButton::callback(
            "I’ve paid, check",
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
}