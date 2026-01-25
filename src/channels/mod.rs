pub mod signal;
pub mod telegram;

use anyhow::Result;
use tracing::warn;

use crate::claude;
use crate::memory::MemoryIndex;
use crate::onboarding;
use crate::pairing::PairingStore;

/// Result of processing a command
pub enum CommandResult {
    /// Not a command, continue with normal message processing
    NotACommand,
    /// Command was handled, return this response to the user
    Response(String),
}

/// Available commands
const COMMANDS: &[(&str, &str)] = &[
    ("/commands", "Show available commands"),
    ("/new", "Start a new conversation"),
];

/// Process a command if the message is one.
/// Returns None if the message is not a command.
pub fn process_command(
    store: &mut PairingStore,
    channel: &str,
    user_id: &str,
    text: &str,
) -> Result<CommandResult> {
    let text = text.trim();

    if text == "/commands" {
        let mut response = String::from("Available commands:\n");
        for (cmd, desc) in COMMANDS {
            response.push_str(&format!("\n{} - {}", cmd, desc));
        }
        return Ok(CommandResult::Response(response));
    }

    if text == "/new" {
        let session_key = format!("{}:{}", channel, user_id);
        store.sessions.remove(&session_key);
        store.save()?;
        return Ok(CommandResult::Response(
            "Starting fresh! Our previous conversation has been cleared.".to_string(),
        ));
    }

    Ok(CommandResult::NotACommand)
}

/// Query Claude with automatic session recovery.
///
/// If the session has expired, clears it and retries with a fresh conversation.
/// Returns the response text and the new session ID.
pub async fn query_claude_with_session(
    store: &mut PairingStore,
    channel: &str,
    user_id: &str,
    text: &str,
    context_prompt: String,
) -> Result<(String, String)> {
    let session_key = format!("{}:{}", channel, user_id);
    let existing_session = store.sessions.get(&session_key).cloned();

    let options = claude::QueryOptions {
        system_prompt: Some(context_prompt.clone()),
        resume_session: existing_session,
        skip_permissions: true,
        ..Default::default()
    };

    let (response, session_id) = match claude::query_with_options(text, options).await {
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
    };

    // Save session ID for future messages
    if !session_id.is_empty()
        && store.sessions.get(&session_key).map(|s| s.as_str()) != Some(&session_id)
    {
        store.sessions.insert(session_key, session_id.clone());
        store.save()?;
    }

    Ok((response, session_id))
}

/// Handle onboarding flow - Claude drives the conversation
pub async fn handle_onboarding(channel: &str, user_id: &str, message: &str) -> Result<String> {
    let system_prompt = onboarding::system_prompt_for_user(channel, user_id)?;

    let options = claude::QueryOptions {
        system_prompt: Some(system_prompt),
        skip_permissions: true,
        ..Default::default()
    };

    let (response, _) = claude::query_with_options(message, options).await?;
    Ok(response)
}

/// Re-index memories for a user (called after Claude responds)
pub fn reindex_user_memories(channel: &str, user_id: &str) {
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

/// Information about a channel for display purposes
pub struct ChannelInfo {
    pub name: &'static str,
    pub display_name: &'static str,
}

/// List of all supported channels
pub const SUPPORTED_CHANNELS: &[ChannelInfo] = &[
    ChannelInfo {
        name: "telegram",
        display_name: "Telegram",
    },
    ChannelInfo {
        name: "signal",
        display_name: "Signal",
    },
];

/// Get channel info by name
pub fn get_channel_info(name: &str) -> Option<&'static ChannelInfo> {
    SUPPORTED_CHANNELS.iter().find(|c| c.name == name)
}
