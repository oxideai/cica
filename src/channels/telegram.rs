use anyhow::Result;
use teloxide::prelude::*;
use teloxide::types::ChatAction;
use tracing::{info, warn};

use crate::claude;
use crate::config::TelegramConfig;
use crate::memory::MemoryIndex;
use crate::onboarding;
use crate::pairing::PairingStore;

/// Validate a Telegram bot token by calling getMe
/// Returns the bot username on success
pub async fn validate_token(token: &str) -> Result<String> {
    let bot = Bot::new(token);
    let me = bot.get_me().await?;
    Ok(me.username().to_string())
}

/// Run the Telegram bot
pub async fn run(config: TelegramConfig) -> Result<()> {
    let bot = Bot::new(&config.bot_token);

    info!("Starting Telegram bot...");

    teloxide::repl(bot, move |bot: Bot, msg: Message| async move {
        if let Err(e) = handle_message(&bot, &msg).await {
            warn!("Error handling message: {}", e);
        }
        Ok(())
    })
    .await;

    Ok(())
}

/// Handle an incoming message
async fn handle_message(bot: &Bot, msg: &Message) -> Result<()> {
    let user = msg.from.as_ref();
    let user_id = user.map(|u| u.id.0.to_string()).unwrap_or_default();
    let username = user.and_then(|u| u.username.clone());
    let display_name = user.map(|u| match &u.last_name {
        Some(last) => format!("{} {}", u.first_name, last),
        None => u.first_name.clone(),
    });

    // Check if user is approved
    let mut store = PairingStore::load()?;

    if !store.is_approved("telegram", &user_id) {
        // Create or get existing pairing request
        let (code, _is_new) =
            store.get_or_create_pending("telegram", &user_id, username, display_name)?;

        let response = format!(
            "Hi! I don't recognize you yet.\n\n\
            Pairing code: {}\n\n\
            Ask the owner to run:\n\
            cica approve {}",
            code, code
        );

        bot.send_message(msg.chat.id, response).await?;
        return Ok(());
    }

    // User is approved - process the message
    let Some(text) = msg.text() else {
        return Ok(());
    };

    info!("Message from {}: {}", user_id, text);

    // Check if onboarding is complete for this user
    if !onboarding::is_complete_for_user("telegram", &user_id)? {
        // /start triggers onboarding greeting, not treated as an answer
        let message = if text == "/start" { "hi" } else { text };

        // Show typing indicator
        let _ = bot.send_chat_action(msg.chat.id, ChatAction::Typing).await;

        let response = handle_onboarding("telegram", &user_id, message).await?;
        bot.send_message(msg.chat.id, response).await?;
        return Ok(());
    }

    // Ignore /start after onboarding (already set up)
    if text == "/start" {
        return Ok(());
    }

    // Show typing indicator
    let _ = bot.send_chat_action(msg.chat.id, ChatAction::Typing).await;

    // Check if we have an existing session to resume
    let existing_session = store
        .sessions
        .get(&format!("telegram:{}", user_id))
        .cloned();

    // Query Claude with context (and resume if we have a session)
    let context_prompt = onboarding::build_context_prompt_for_user(
        Some("Telegram"),
        Some("telegram"),
        Some(&user_id),
        Some(text),
    )?;
    let session_key = format!("telegram:{}", user_id);

    let (response, session_id) = {
        let options = claude::QueryOptions {
            system_prompt: Some(context_prompt.clone()),
            resume_session: existing_session.clone(),
            skip_permissions: true,
            ..Default::default()
        };

        match claude::query_with_options(text, options).await {
            Ok((response, session_id)) => (response, session_id),
            Err(e) => {
                let error_msg = e.to_string();
                // If session not found, clear it and retry without resuming
                if error_msg.contains("No conversation found with session ID") {
                    warn!("Session expired, starting fresh conversation");
                    store.sessions.remove(&session_key);
                    store.save()?;

                    let retry_options = claude::QueryOptions {
                        system_prompt: Some(context_prompt),
                        resume_session: None,
                        skip_permissions: true,
                        ..Default::default()
                    };

                    match claude::query_with_options(text, retry_options).await {
                        Ok((response, session_id)) => (response, session_id),
                        Err(e) => {
                            warn!("Claude error on retry: {}", e);
                            (
                                format!("Sorry, I encountered an error: {}", e),
                                String::new(),
                            )
                        }
                    }
                } else {
                    warn!("Claude error: {}", e);
                    (
                        format!("Sorry, I encountered an error: {}", e),
                        String::new(),
                    )
                }
            }
        }
    };

    // Save session ID for future messages
    if !session_id.is_empty() {
        if store.sessions.get(&session_key).map(|s| s.as_str()) != Some(&session_id) {
            store.sessions.insert(session_key, session_id);
            store.save()?;
        }
    }

    bot.send_message(msg.chat.id, response).await?;

    // Re-index memories in case Claude saved new ones
    reindex_user_memories("telegram", &user_id);

    Ok(())
}

/// Re-index memories for a user (called after Claude responds)
fn reindex_user_memories(channel: &str, user_id: &str) {
    match MemoryIndex::open() {
        Ok(mut index) => {
            if let Err(e) = index.index_user_memories(channel, user_id) {
                warn!(
                    "Failed to re-index memories for {}:{}: {}",
                    channel, user_id, e
                );
            }
        }
        Err(e) => {
            warn!("Failed to open memory index: {}", e);
        }
    }
}

/// Handle onboarding flow - Claude drives the conversation (per-user)
async fn handle_onboarding(channel: &str, user_id: &str, message: &str) -> Result<String> {
    let system_prompt = onboarding::system_prompt_for_user(channel, user_id)?;

    let options = claude::QueryOptions {
        system_prompt: Some(system_prompt),
        skip_permissions: true, // Allow writing IDENTITY.md and USER.md
        ..Default::default()
    };

    let (response, _) = claude::query_with_options(message, options).await?;
    Ok(response)
}
