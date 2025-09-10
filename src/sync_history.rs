use anyhow::Context;
use grammers_client::{Client, Config};
use grammers_session::Session;
use grammers_tl_types as tl;
use std::{collections::HashMap, path::PathBuf, time::{Duration, Instant}};
use tokio::time::sleep;
use tracing::{error, info};

const SEND_DELAY_MS: u64 = 200; // 5 messages/sec
const BATCH_PAUSE_COUNT: usize = 200;
const BATCH_PAUSE_SECONDS: u64 = 20;

// Resolve a chat/channel/user by its numeric ID by iterating dialogs to obtain the proper InputPeer (with access_hash).
async fn resolve_chat_by_id(client: &mut Client, chat_id: i64) -> anyhow::Result<grammers_client::types::Chat> {
    let mut iter = client.iter_dialogs();
    while let Some(dialog) = iter.next().await? {
        let chat = dialog.chat();
        if chat.id() == chat_id {
            return Ok(chat.clone());
        }
    }
    Err(anyhow::anyhow!("chat id {} not found in your dialogs; open it once and try again", chat_id))
}

// Export the main entry point of the module
pub async fn sync_history(
    api_id: i32,
    api_hash: &str,
    session_path: PathBuf,
    source_chat_id: i64,
    target_chat_id: i64,
    topic_map: Option<&HashMap<i32, i32>>,
) -> anyhow::Result<()> {
    // Load session file (must be authorized already)
    let session = Session::load_file(&session_path)
        .with_context(|| format!("failed to load MTProto session from {:?}", session_path))?;

    let mut client = Client::connect(Config {
        session,
        api_id,
        api_hash: api_hash.to_string(),
        params: Default::default(),
    })
    .await
    .context("connect mtproto")?;
    // Resolve peers by scanning dialogs (get_entity is not available)
    let source = resolve_chat_by_id(&mut client, source_chat_id)
        .await
        .context("resolve source chat")?;
    let target = resolve_chat_by_id(&mut client, target_chat_id)
        .await
        .context("resolve target chat")?;

    info!("Resolved source and target");

    let source_peer = source.pack().to_input_peer();
    let target_peer = target.pack().to_input_peer();

    // Collect history newest-first in batches using GetHistory TL; adjust limit as needed
    let mut all_msgs: Vec<tl::types::Message> = Vec::new();
    let mut offset_id: i32 = 0;
    let batch_limit: i32 = 100;
    loop {
        let resp = client
            .invoke(&tl::functions::messages::GetHistory {
                peer: source_peer.clone(),
                offset_id,
                offset_date: 0,
                add_offset: 0,
                limit: batch_limit,
                max_id: 0,
                min_id: 0,
                hash: 0,
            })
            .await
            .context("get_history")?;

        let msgs = match resp {
            tl::enums::messages::Messages::Messages(tl::types::messages::Messages { messages, .. }) => messages,
            tl::enums::messages::Messages::Slice(tl::types::messages::MessagesSlice { messages, .. }) => messages,
            tl::enums::messages::Messages::ChannelMessages(tl::types::messages::ChannelMessages { messages, .. }) => messages,
            _ => Vec::new(),
        };

        // collect only real Message variants
        let mut found = 0usize;
        let mut min_id_in_batch: Option<i32> = None;
        for m in msgs.into_iter() {
            if let tl::enums::Message::Message(m) = m {
                if min_id_in_batch.map_or(true, |min| m.id < min) {
                    min_id_in_batch = Some(m.id);
                }
                all_msgs.push(m);
                found += 1;
            }
        }

        if found == 0 {
            break;
        }

        // prepare next offset_id as earliest id in this batch
        if let Some(min_id) = min_id_in_batch {
            offset_id = min_id;
        } else {
            break;
        }

        // small pause to avoid flood
        sleep(Duration::from_millis(200)).await;

        if (found as i32) < batch_limit {
            break;
        }
    }

    // messages were collected newest-first; reverse to send oldest-first
    all_msgs.reverse();
    info!("Fetched {} messages from source", all_msgs.len());

    // mapping old_msg_id -> new_msg_id to preserve replies
    let mut old_to_new: HashMap<i32, i32> = HashMap::new();
    let mut last_send = Instant::now() - Duration::from_millis(SEND_DELAY_MS);
    let mut copied_count: usize = 0;

    for m in all_msgs.into_iter() {
        // skip service messages or messages without textual content
        let text = {
            let s = m.message.clone();
            if s.is_empty() {
                // TODO: add media forwarding later
                info!("skip non-text or service message id {}", m.id);
                continue;
            }
            s
        };

        // compute reply mapping if any (skipped due to API changes in MessageReplyHeader variants)
        let reply_to_new: Option<i32> = None;

        // enforce rate limit
        let elapsed = last_send.elapsed();
        if elapsed < Duration::from_millis(SEND_DELAY_MS) {
            sleep(Duration::from_millis(SEND_DELAY_MS) - elapsed).await;
        }
        let send_res = client
            .invoke(&tl::functions::messages::SendMessage {
                peer: target_peer.clone(),
                message: text.clone(),
                random_id: {
                    let nanos = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_else(|_| Duration::from_secs(0))
                        .as_nanos() as i64;
                    nanos ^ (m.id as i64)
                },
                no_webpage: true,
                reply_markup: None,
                entities: None,
                schedule_date: None,
                // new fields required by newer grammers_tl_types
                silent: false,
                background: false,
                clear_draft: false,
                noforwards: false,
                update_stickersets_order: false,
                invert_media: false,
                reply_to: reply_to_new.map(|id| tl::enums::InputReplyTo::Message(
                    tl::types::InputReplyToMessage {
                        reply_to_peer_id: None,
                        top_msg_id: None,
                        reply_to_msg_id: id,
                        quote_text: None,
                        quote_entities: None,
                        quote_offset: None,
                    }
                )),
                send_as: None,
                quick_reply_shortcut: None,
            })
            .await;

        match send_res {
            Ok(_) => {
                last_send = Instant::now();
                if let Ok(resp) = client
                    .invoke(&tl::functions::messages::GetHistory {
                        peer: target_peer.clone(),
                        offset_id: 0,
                        offset_date: 0,
                        add_offset: 0,
                        limit: 1,
                        max_id: 0,
                        min_id: 0,
                        hash: 0,
                    })
                    .await
                {
                    let messages = match resp {
                        tl::enums::messages::Messages::Messages(tl::types::messages::Messages { messages, .. }) => messages,
                        tl::enums::messages::Messages::Slice(tl::types::messages::MessagesSlice { messages, .. }) => messages,
                        tl::enums::messages::Messages::ChannelMessages(tl::types::messages::ChannelMessages { messages, .. }) => messages,
                        _ => Vec::new(),
                    };
                    if let Some(tl::enums::Message::Message(newm)) = messages.into_iter().next() {
                        old_to_new.insert(m.id, newm.id);
                        copied_count += 1;
                        info!("copied {} -> {}", m.id, newm.id);
                    }
                }
            }
            Err(e) => {
                error!("failed to send message id {}: {:?}", m.id, e);
            }
        }

        // batch pause
        if copied_count > 0 && copied_count % BATCH_PAUSE_COUNT == 0 {
            info!("copied {} messages — pausing {}s", copied_count, BATCH_PAUSE_SECONDS);
            sleep(Duration::from_secs(BATCH_PAUSE_SECONDS)).await;
            last_send = Instant::now() - Duration::from_millis(SEND_DELAY_MS);
        }
    }

    info!("Sync finished. total copied: {}", old_to_new.len());
    Ok(())
}