use anyhow::Context;
use grammers_client::{Client, Config};
use grammers_session::Session;
use grammers_tl_types as tl;
use rusqlite::params;
use std::{path::PathBuf, time::{Duration, Instant}};
use tokio::time::sleep;
use tracing::{error, info};

const POLL_INTERVAL_MS: u64 = 2000; // poll every 2s
const SEND_DELAY_MS: u64 = 200; // 5 msg/sec cap

/// Simple polling forwarder: copies text messages from source_chat to target_chat.
/// - api_id/api_hash: MTProto app credentials
/// - session_path: path to mtproto.session (must be authorized)
/// - source_chat_id / target_chat_id: numeric ids (-100...)
/// NOTE: This implementation only handles plain text messages and basic reply mapping.
/// Media/attachments/forwarded messages require extra handling (download + upload or ForwardMessages TL).

// Resolve a chat by its numeric ID by scanning the cached dialogs; ensure the chat is in your dialog list.
async fn resolve_chat_by_id(client: &mut Client, chat_id: i64) -> anyhow::Result<grammers_client::types::Chat> {
    let mut iter = client.iter_dialogs();
    while let Some(dialog) = iter.next().await? {
        let chat = dialog.chat();
        if chat.id() == chat_id {
            return Ok(chat.clone());
        }
    }
    Err(anyhow::anyhow!(
        "chat with id {} not found in your dialogs; open it once so it's cached",
        chat_id
    ))
}

pub async fn run_user_forward(
    api_id: i32,
    api_hash: &str,
    session_path: PathBuf,
    source_chat_id: i64,
    target_chat_id: i64,
) -> anyhow::Result<()> {
    // connect MTProto client using the existing session
    let mut client = Client::connect(Config {
        session: Session::load_file_or_create(&session_path).context("load session")?,
        api_id,
        api_hash: api_hash.to_string(),
        params: Default::default(),
    })
    .await
    .context("connect mtproto")?;

    let authorized = client.is_authorized().await.context("check authorization")?;
    if !authorized {
        return Err(anyhow::anyhow!("MTProto session not authorized; run interactive auth first"));
    }

    // resolve source/target entity peers
    let source = resolve_chat_by_id(&mut client, source_chat_id).await.context("resolve source")?;
    let target = resolve_chat_by_id(&mut client, target_chat_id).await.context("resolve target")?;

    // track last seen id so we only forward new messages
    let mut last_seen: i32 = 0;
    let mut last_send = Instant::now() - Duration::from_millis(SEND_DELAY_MS);

    loop {
        // fetch recent history (newest-first)
        let resp = client
            .invoke(&tl::functions::messages::GetHistory {
                peer: source.pack().to_input_peer(),
                offset_id: 0,
                offset_date: 0,
                add_offset: 0,
                limit: 50,
                max_id: 0,
                min_id: last_seen, // get messages with id > last_seen
                hash: 0,
            })
            .await?;

        // extract Message objects
        let msgs = match resp {
            tl::enums::messages::Messages::Messages(tl::types::messages::Messages { messages, .. }) => messages,
            tl::enums::messages::Messages::Slice(tl::types::messages::MessagesSlice { messages, .. }) => messages,
            tl::enums::messages::Messages::ChannelMessages(tl::types::messages::ChannelMessages { messages, .. }) => messages,
            _ => vec![],
        };

        // collect real messages with id > last_seen, then reverse to oldest-first
        let mut to_forward = vec![];
        for m in msgs.into_iter() {
            if let tl::enums::Message::Message(m) = m {
                if last_seen == 0 || m.id > last_seen {
                    to_forward.push(m);
                }
            }
        }
        to_forward.sort_by_key(|m| m.id); // ensure oldest->newest

        for m in to_forward.into_iter() {
            // update last_seen before sending to avoid duplication on crash
            if m.id > last_seen {
                last_seen = m.id;
            }
            // only copy plain text for now
            let text = if m.message.is_empty() {
                info!("skipping non-text or empty message id {}", m.id);
                continue;
            } else {
                m.message.clone()
            };

            // respect rate limit
            let elapsed = last_send.elapsed();
            if elapsed < Duration::from_millis(SEND_DELAY_MS) {
                sleep(Duration::from_millis(SEND_DELAY_MS) - elapsed).await;
            }

            // compute reply mapping if you kept a mapping (not implemented here)
            let reply_to_new = 0i32; // TODO: map replies if you store old->new ids

            // send the message as the user into target chat
            match client
                .invoke(&tl::functions::messages::SendMessage {
                    // required
                    peer: target.pack().to_input_peer(),
                    message: text.clone(),
                    random_id: rand::random::<i64>(),

                    // flags (explicitly set since Default is not implemented)
                    no_webpage: true,
                    silent: false,
                    background: false,
                    clear_draft: false,
                    noforwards: false,
                    update_stickersets_order: false,
                    invert_media: false,

                    // optionals
                    reply_to: if reply_to_new > 0 {
                        Some(tl::enums::InputReplyTo::Message(tl::types::InputReplyToMessage {
                            reply_to_msg_id: reply_to_new,
                            top_msg_id: None,
                            reply_to_peer_id: None,
                            quote_text: None,
                            quote_entities: None,
                            quote_offset: None,
                        }))
                    } else {
                        None
                    },
                    reply_markup: None,
                    entities: None,
                    schedule_date: None,
                    send_as: None,
                    quick_reply_shortcut: None,
                })
                .await
            {
                Ok(_) => {
                    info!("copied message id {} to target", m.id);
                }
                Err(e) => {
                    error!("failed to send message id {}: {:?}", m.id, e);
                }
            }

            last_send = Instant::now();
        }

        sleep(Duration::from_millis(POLL_INTERVAL_MS)).await;
    }
}