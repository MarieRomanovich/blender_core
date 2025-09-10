use anyhow::Context;
use teloxide::{prelude::*, types::ChatId};
use tracing::{error, info};

/// Try forward an incoming message if it comes from `source_id`.
/// Returns Ok(true) if the message was forwarded, Ok(false) if ignored.
pub async fn try_forward(bot: &Bot, msg: &Message, source_id: i64, target_id: i64) -> anyhow::Result<bool> {
    // Only handle messages that have a chat (should always be true for Message)
    let from_chat = msg.chat.id;
    if from_chat.0 != source_id {
        return Ok(false);
    }

    // We will forward the message into target_id.
    let target_chat = ChatId(target_id);

    // Use forward_message to preserve original sender info.
    // Note: teloxide API version differences: if your teloxide requires .send() or .request,
    // adjust the call accordingly. The common pattern bot.forward_message(...).await often works.
    match bot.forward_message(target_chat, from_chat, msg.id).await {
        Ok(_) => {
            info!("Forwarded message {} from {} -> {}", msg.id, source_id, target_id);
            Ok(true)
        }
        Err(err) => {
            error!("Failed to forward message {}: {:?}", msg.id, err);
            Err(anyhow::anyhow!("forward failed: {:?}", err))
        }
    }
}