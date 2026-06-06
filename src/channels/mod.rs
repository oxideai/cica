pub mod signal;
pub mod slack;
pub mod telegram;

use anyhow::Result;
use async_trait::async_trait;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Mutex, oneshot};
use tokio::task::JoinHandle;
use tracing::{debug, warn};

use crate::audit;
use crate::backends::{self, QueryResult};
use crate::cron::{
    self, CronSchedule, CronStore, DeliveryTarget, format_timestamp, parse_add_command,
    truncate_for_name,
};
use crate::memory::MemoryIndex;
use crate::onboarding;
use crate::pairing::PairingStore;
use crate::sandbox::{self, TurnJob};
use crate::skills;

/// Abstraction over channel-specific transport operations.
#[async_trait]
pub trait Channel: Send + Sync + 'static {
    /// Channel identifier (e.g., "telegram", "signal")
    fn name(&self) -> &'static str;

    /// Display name for user-facing messages (e.g., "Telegram", "Signal")
    fn display_name(&self) -> &'static str;

    /// Send a text message to the user
    async fn send_message(&self, message: &str) -> Result<()>;

    /// Send a message with attachments (images, files, etc.)
    async fn send_message_with_attachments(
        &self,
        message: &str,
        _attachment_paths: &[PathBuf],
    ) -> Result<()> {
        self.send_message(message).await
    }

    /// Start a typing indicator. Returns a guard that stops the indicator when dropped.
    fn start_typing(&self) -> TypingGuard;
}

/// RAII guard for typing indicators; dropped when the response is ready.
pub struct TypingGuard {
    cancel: Option<oneshot::Sender<()>>,
}

impl TypingGuard {
    pub fn new(cancel: oneshot::Sender<()>) -> Self {
        Self {
            cancel: Some(cancel),
        }
    }

    #[allow(dead_code)]
    pub fn noop() -> Self {
        Self { cancel: None }
    }
}

impl Drop for TypingGuard {
    fn drop(&mut self) {
        if let Some(cancel) = self.cancel.take() {
            let _ = cancel.send(());
        }
    }
}

/// Actions that can result from processing an incoming message.
pub enum MessageAction {
    /// Send a simple response (command output, error message, etc.)
    SendResponse(String),

    /// Execute a cron job immediately
    ExecuteCronJob { job_id: String },

    /// Run onboarding flow with Claude
    Onboarding { message: String },

    /// Query Claude with the user's message
    QueryClaude { text: String },

    /// User not approved - send pairing instructions
    NeedsPairing { code: String },

    /// No action needed (empty message, /start after onboarding, etc.)
    Ignore,
}

/// Determine what action to take for an incoming message.
pub fn determine_action(
    channel: &str,
    user_id: &str,
    text: &str,
    _image_paths: &[PathBuf],
    store: &mut PairingStore,
    username: Option<String>,
    display_name: Option<String>,
) -> Result<MessageAction> {
    let text = text.trim();

    if !store.is_approved(channel, user_id) {
        let settings = crate::config::Config::load()
            .map(|c: crate::config::Config| c.channel_settings(channel))
            .unwrap_or_default();

        if settings.auto_approve {
            store.auto_approve(channel, user_id, username, display_name)?;
        } else {
            let (code, _is_new) =
                store.get_or_create_pending(channel, user_id, username, display_name)?;
            return Ok(MessageAction::NeedsPairing { code });
        }
    }

    let onboarding_complete = onboarding::is_complete_for_user(channel, user_id)?;

    // Commands work even during onboarding.
    match process_command(store, channel, user_id, text, onboarding_complete)? {
        CommandResult::Response(response) => {
            return Ok(MessageAction::SendResponse(response));
        }
        CommandResult::CronRun(job_id) => {
            return Ok(MessageAction::ExecuteCronJob { job_id });
        }
        CommandResult::NotACommand => {}
    }

    if !onboarding_complete {
        let message = if text == "/start" { "hi" } else { text };
        return Ok(MessageAction::Onboarding {
            message: message.to_string(),
        });
    }

    if text == "/start" {
        return Ok(MessageAction::Ignore);
    }

    if text.is_empty() {
        return Ok(MessageAction::Ignore);
    }

    Ok(MessageAction::QueryClaude {
        text: text.to_string(),
    })
}

/// Build a message combining text and image paths (@path syntax for Claude Code).
pub fn build_text_with_images(text: &str, image_paths: &[PathBuf]) -> String {
    let mut result = text.to_string();

    for (i, path) in image_paths.iter().enumerate() {
        if let Some(path_str) = path.to_str() {
            if result.is_empty() {
                result = format!("@{}", path_str);
            } else if i == 0 {
                result = format!("{}\n\n@{}", result, path_str);
            } else {
                result = format!("{} @{}", result, path_str);
            }
        }
    }

    result
}

/// Execute an action. Returns `Some(text)` for `QueryClaude` (caller handles with task manager).
pub async fn execute_action(
    channel: &dyn Channel,
    user_id: &str,
    action: MessageAction,
) -> Result<Option<String>> {
    match action {
        MessageAction::SendResponse(response) => {
            channel.send_message(&response).await?;
            Ok(None)
        }

        MessageAction::NeedsPairing { code } => {
            let response = format!(
                "Hi! I don't recognize you yet.\n\n\
                 Pairing code: {}\n\n\
                 Ask the owner to run:\n\
                 cica approve {}",
                code, code
            );
            channel.send_message(&response).await?;
            Ok(None)
        }

        MessageAction::ExecuteCronJob { job_id } => {
            channel.send_message("Running job...").await?;
            let _typing = channel.start_typing();
            let result = execute_cron_job(&job_id, channel.name(), user_id).await;
            let response = result.unwrap_or_else(|e| format!("Job failed: {}", e));
            channel.send_message(&response).await?;
            Ok(None)
        }

        MessageAction::Onboarding { message } => {
            let _typing = channel.start_typing();
            let response = handle_onboarding(channel.name(), user_id, &message).await?;
            channel.send_message(&response).await?;
            Ok(None)
        }

        MessageAction::QueryClaude { text } => Ok(Some(text)),

        MessageAction::Ignore => Ok(None),
    }
}

/// Extract media file paths from Claude's response text.
///
/// Prefers explicit `[attachment:/path/to/file]` markers; falls back to heuristic
/// path detection for backwards compatibility.
fn extract_media_attachments(response: &str) -> Vec<PathBuf> {
    let mut attachments = Vec::new();

    for cap in response.match_indices("[attachment:") {
        let start = cap.0 + "[attachment:".len();
        if let Some(end) = response[start..].find(']') {
            let path_str = response[start..start + end].trim();
            let path = PathBuf::from(path_str);
            if path.exists() && !attachments.contains(&path) {
                attachments.push(path);
            }
        }
    }

    if !attachments.is_empty() {
        return attachments;
    }

    // Fallback: heuristic detection for paths ending in media extensions.
    let media_extensions = [
        ".png", ".jpg", ".jpeg", ".gif", ".webp",
        ".mp4", ".mov", ".webm", ".avi",
    ];

    for line in response.lines() {
        let line = line.trim();

        for ext in &media_extensions {
            if line.contains(ext)
                && let Some(start) = line.find("/Users/")
                && let Some(ext_pos) = line[start..].find(ext)
            {
                let end_pos = start + ext_pos + ext.len();
                let path_str = &line[start..end_pos];
                if std::path::Path::new(path_str).exists() {
                    attachments.push(PathBuf::from(path_str));
                    break;
                }
            }
        }
    }

    attachments
}

/// Remove lines with file paths or attachment markers before sending to the user.
fn remove_file_path_lines(response: &str) -> String {
    let lines: Vec<&str> = response
        .lines()
        .filter(|line| {
            let trimmed = line.trim();
            let lower = trimmed.to_lowercase();
            !trimmed.contains("[attachment:")
                && !trimmed.contains("/Users/")
                && !lower.contains("saved to")
                && !lower.contains("image has been saved")
                && !lower.contains("video has been saved")
                && !lower.contains("file has been saved")
                && !trimmed.is_empty()
        })
        .collect();

    lines.join("\n").trim().to_string()
}

/// Execute a Claude query for the user (called from the task_manager callback).
pub async fn execute_claude_query(
    channel: Arc<dyn Channel>,
    user_id: &str,
    messages: Vec<String>,
    session_key: Option<String>,
) {
    let combined_text = messages.join("\n\n");
    let _typing = channel.start_typing();

    let context_prompt = match onboarding::build_context_prompt_for_user(
        Some(channel.display_name()),
        Some(channel.name()),
        Some(user_id),
        Some(&combined_text),
    ) {
        Ok(p) => p,
        Err(e) => {
            warn!("Failed to build context prompt: {}", e);
            let _ = channel
                .send_message(&format!("Sorry, I encountered an error: {}", e))
                .await;
            return;
        }
    };

    let mut store = match PairingStore::load() {
        Ok(s) => s,
        Err(e) => {
            warn!("Failed to load pairing store: {}", e);
            let _ = channel
                .send_message(&format!("Sorry, I encountered an error: {}", e))
                .await;
            return;
        }
    };

    let qr = match query_ai_with_session(
        &mut store,
        channel.name(),
        user_id,
        &combined_text,
        context_prompt,
        session_key.as_deref(),
    )
    .await
    {
        Ok(qr) => qr,
        Err(e) => {
            warn!("AI query failed: {}", e);
            let err_msg = format!("Sorry, I encountered an error: {}", e);
            audit::log_message(
                channel.name(),
                user_id,
                &combined_text,
                &err_msg,
                None,
                None,
                None,
                true,
            );
            let _ = channel.send_message(&err_msg).await;
            return;
        }
    };

    let response = &qr.response;

    audit::log_message(
        channel.name(),
        user_id,
        &combined_text,
        response,
        if qr.session_id.is_empty() {
            None
        } else {
            Some(qr.session_id.as_str())
        },
        qr.duration_ms,
        qr.cost_usd,
        false,
    );

    let attachments = extract_media_attachments(response);

    if !attachments.is_empty() {
        debug!("Sending response with {} attachment(s)", attachments.len());
        let cleaned_response = remove_file_path_lines(response);
        if let Err(e) = channel
            .send_message_with_attachments(&cleaned_response, &attachments)
            .await
        {
            warn!("Failed to send message with attachments: {}", e);
        }
    } else if let Err(e) = channel.send_message(response).await {
        warn!("Failed to send message: {}", e);
    }

    reindex_user_memories(channel.name(), user_id);
}

const DEBOUNCE_MS: u64 = 200;

struct ActiveTask {
    handle: JoinHandle<()>,
}

/// Manages per-user message processing with debouncing and interruption
pub struct UserTaskManager {
    tasks: Mutex<HashMap<String, ActiveTask>>,
    pending: Mutex<HashMap<String, Vec<String>>>,
}

impl UserTaskManager {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            tasks: Mutex::new(HashMap::new()),
            pending: Mutex::new(HashMap::new()),
        })
    }

    /// Queue a message for processing; aborts any in-flight task and batches within DEBOUNCE_MS.
    pub async fn process_message<F, Fut>(
        self: &Arc<Self>,
        user_key: String,
        message: String,
        handler: F,
    ) where
        F: FnOnce(Vec<String>) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = ()> + Send,
    {
        debug!("Queueing message for {}: {}", user_key, message);

        {
            let mut pending = self.pending.lock().await;
            pending
                .entry(user_key.clone())
                .or_insert_with(Vec::new)
                .push(message);
        }

        let mut tasks = self.tasks.lock().await;

        if let Some(existing) = tasks.remove(&user_key) {
            debug!("Aborting existing task for {}", user_key);
            existing.handle.abort();
        }

        let manager = Arc::clone(self);
        let user_key_clone = user_key.clone();

        let handle = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(DEBOUNCE_MS)).await;

            let messages = {
                let mut pending = manager.pending.lock().await;
                pending.remove(&user_key_clone).unwrap_or_default()
            };

            if messages.is_empty() {
                return;
            }

            debug!(
                "Processing {} message(s) for {}",
                messages.len(),
                user_key_clone
            );

            handler(messages).await;

            manager.tasks.lock().await.remove(&user_key_clone);
        });

        tasks.insert(user_key, ActiveTask { handle });
    }
}

/// Result of processing a command
pub enum CommandResult {
    /// Not a command, continue with normal message processing
    NotACommand,
    /// Command was handled, return this response to the user
    Response(String),
    /// Trigger async cron job execution (job_id)
    CronRun(String),
}

/// Available commands
const COMMANDS: &[(&str, &str)] = &[
    ("/commands", "Show available commands"),
    ("/new", "Start a new conversation"),
    ("/skills", "List available skills"),
    ("/cron", "Manage scheduled jobs"),
    ("/usage", "Show your usage stats"),
];

pub fn process_command(
    store: &mut PairingStore,
    channel: &str,
    user_id: &str,
    text: &str,
    onboarding_complete: bool,
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
        if !onboarding_complete {
            return Ok(CommandResult::Response(
                "Please complete the onboarding first. Say \"hello\" to get started!".to_string(),
            ));
        }
        let session_key = format!("{}:{}", channel, user_id);
        let old_session_id = store.sessions.remove(&session_key);
        store.save()?;

        let detail = old_session_id
            .as_ref()
            .map(|sid| format!("{{\"old_session_id\":\"{}\"}}", sid));
        audit::log_event(
            "session_reset",
            Some(channel),
            Some(user_id),
            detail.as_deref(),
        );
        audit::log_event(
            "command_used",
            Some(channel),
            Some(user_id),
            Some("{\"command\":\"/new\"}"),
        );

        return Ok(CommandResult::Response(
            "Starting fresh! Our previous conversation has been cleared.".to_string(),
        ));
    }

    if text == "/usage" {
        audit::log_event(
            "command_used",
            Some(channel),
            Some(user_id),
            Some("{\"command\":\"/usage\"}"),
        );
        let response = match audit::get_usage(channel, user_id) {
            Ok((count, total_cost)) => {
                let cost_line = match total_cost {
                    Some(cost) if cost > 0.0 => format!("Total cost: ${:.4}\n", cost),
                    _ => String::new(),
                };
                format!("Your usage:\n\nMessages: {}\n{}", count, cost_line)
            }
            Err(_) => "Usage stats not available.".to_string(),
        };
        return Ok(CommandResult::Response(response));
    }

    if text == "/skills" {
        audit::log_event(
            "command_used",
            Some(channel),
            Some(user_id),
            Some("{\"command\":\"/skills\"}"),
        );
        let available_skills = skills::discover_skills().unwrap_or_default();
        if available_skills.is_empty() {
            return Ok(CommandResult::Response("No skills installed.".to_string()));
        }
        let mut response = String::from("Available skills:\n");
        for skill in available_skills {
            response.push_str(&format!("\n• {} - {}", skill.name, skill.description));
        }
        return Ok(CommandResult::Response(response));
    }

    if text.starts_with("/cron") {
        audit::log_event(
            "command_used",
            Some(channel),
            Some(user_id),
            Some(&format!(
                "{{\"command\":\"{}\"}}",
                text.split_whitespace()
                    .take(2)
                    .collect::<Vec<_>>()
                    .join(" ")
            )),
        );
        let args = text.strip_prefix("/cron").unwrap_or("").trim();
        return process_cron_command(channel, user_id, args);
    }

    Ok(CommandResult::NotACommand)
}

/// Extract --target <value> from a command string, returning (target, remaining_text).
fn extract_target_flag(input: &str) -> (Option<DeliveryTarget>, String) {
    if let Some(idx) = input.find("--target ") {
        let after_flag = &input[idx + "--target ".len()..];
        let value_end = after_flag.find(' ').unwrap_or(after_flag.len());
        let target_value = &after_flag[..value_end];

        let before = input[..idx].trim();
        let after = if value_end < after_flag.len() {
            after_flag[value_end..].trim()
        } else {
            ""
        };
        let remaining = format!("{} {}", before, after).trim().to_string();

        let target = DeliveryTarget::channel(target_value.to_string());
        (Some(target), remaining)
    } else {
        (None, input.to_string())
    }
}

fn process_cron_command(channel: &str, user_id: &str, args: &str) -> Result<CommandResult> {
    let parts: Vec<&str> = args.splitn(2, ' ').collect();
    let subcommand = parts.first().copied().unwrap_or("help");
    let rest = parts.get(1).copied().unwrap_or("");

    match subcommand {
        "list" | "ls" => {
            let store = CronStore::load()?;
            let jobs = store.list_for_user(channel, user_id);

            if jobs.is_empty() {
                return Ok(CommandResult::Response(
                    "No scheduled jobs.\n\nUse /cron add to create one. Try /cron help for usage."
                        .to_string(),
                ));
            }

            let mut response = String::from("Your scheduled jobs:\n");
            for job in jobs {
                let status = job.state.last_status.as_str();
                let next = job
                    .state
                    .next_run_at
                    .map(format_timestamp)
                    .unwrap_or_else(|| "—".to_string());
                let enabled = if job.enabled { "" } else { " (paused)" };
                let target_info = if job.target.channel_id.is_some() {
                    format!(
                        "  Target: {}{}\n",
                        job.target.channel_id.as_deref().unwrap_or("DM"),
                        job.target
                            .thread_id
                            .as_ref()
                            .map(|t| format!(" (thread: {})", t))
                            .unwrap_or_default()
                    )
                } else {
                    String::new()
                };

                response.push_str(&format!(
                    "\n[{}] {}{}\n  Schedule: {}\n{}  Status: {} | Next: {}\n",
                    job.short_id(),
                    job.name,
                    enabled,
                    job.schedule.description(),
                    target_info,
                    status,
                    next
                ));
            }
            Ok(CommandResult::Response(response))
        }

        "add" => {
            if rest.is_empty() {
                return Ok(CommandResult::Response(
                    "Usage: /cron add <schedule> <prompt> [--target <channel_id>]\n\n\
                     Examples:\n\
                     /cron add every 1h Check my emails\n\
                     /cron add every 10s Say hello\n\
                     /cron add 0 9 * * * Good morning!\n\
                     /cron add every 1h Check emails --target C0123456789"
                        .to_string(),
                ));
            }

            let (target, rest_without_target) = extract_target_flag(rest);

            let (schedule, prompt) = match parse_add_command(&rest_without_target) {
                Ok(result) => result,
                Err(e) => return Ok(CommandResult::Response(format!("Error: {}", e))),
            };

            let name = truncate_for_name(&prompt, 30);
            let mut store = CronStore::load()?;
            let job = cron::CronJob::new(
                name.clone(),
                prompt,
                schedule.clone(),
                channel.to_string(),
                user_id.to_string(),
                target,
            );
            let id = store.add(job)?;

            let next = match &schedule {
                CronSchedule::At(ts) => format_timestamp(*ts),
                CronSchedule::Every(_) | CronSchedule::Cron(_) => {
                    let store = CronStore::load()?;
                    store
                        .jobs
                        .get(&id)
                        .and_then(|j| j.state.next_run_at)
                        .map(format_timestamp)
                        .unwrap_or_else(|| "soon".to_string())
                }
            };

            Ok(CommandResult::Response(format!(
                "Created job [{}] \"{}\"\nSchedule: {}\nNext run: {}\n\nUse /cron run {} to test it now!",
                &id[..8],
                name,
                schedule.description(),
                next,
                &id[..8]
            )))
        }

        "remove" | "rm" | "delete" => {
            let id = rest.trim();
            if id.is_empty() {
                return Ok(CommandResult::Response(
                    "Usage: /cron remove <job-id>".to_string(),
                ));
            }

            let mut store = CronStore::load()?;

            // Find job by full ID or prefix
            let job_id = find_job_id(&store, channel, user_id, id)?;

            match store.remove(&job_id, channel, user_id)? {
                Some(job) => Ok(CommandResult::Response(format!(
                    "Removed job [{}] \"{}\"",
                    job.short_id(),
                    job.name
                ))),
                None => Ok(CommandResult::Response(format!("Job not found: {}", id))),
            }
        }

        "run" => {
            let id = rest.trim();
            if id.is_empty() {
                return Ok(CommandResult::Response(
                    "Usage: /cron run <job-id>".to_string(),
                ));
            }

            let store = CronStore::load()?;
            let job_id = find_job_id(&store, channel, user_id, id)?;
            Ok(CommandResult::CronRun(job_id))
        }

        "pause" | "disable" => {
            let id = rest.trim();
            if id.is_empty() {
                return Ok(CommandResult::Response(
                    "Usage: /cron pause <job-id>".to_string(),
                ));
            }

            let mut store = CronStore::load()?;
            let job_id = find_job_id(&store, channel, user_id, id)?;

            let result = if let Some(job) = store.get_mut(&job_id) {
                if job.channel != channel || job.user_id != user_id {
                    return Ok(CommandResult::Response("Job not found".to_string()));
                }
                job.enabled = false;
                job.state.next_run_at = None;
                Some((job.short_id().to_string(), job.name.clone()))
            } else {
                None
            };

            if let Some((short_id, name)) = result {
                store.save()?;
                Ok(CommandResult::Response(format!(
                    "Paused job [{}] \"{}\"",
                    short_id, name
                )))
            } else {
                Ok(CommandResult::Response(format!("Job not found: {}", id)))
            }
        }

        "resume" | "enable" => {
            let id = rest.trim();
            if id.is_empty() {
                return Ok(CommandResult::Response(
                    "Usage: /cron resume <job-id>".to_string(),
                ));
            }

            let mut store = CronStore::load()?;
            let job_id = find_job_id(&store, channel, user_id, id)?;

            let result = if let Some(job) = store.get_mut(&job_id) {
                if job.channel != channel || job.user_id != user_id {
                    return Ok(CommandResult::Response("Job not found".to_string()));
                }
                job.enabled = true;
                job.update_next_run(cron::store::now_millis());
                let next = job
                    .state
                    .next_run_at
                    .map(format_timestamp)
                    .unwrap_or_else(|| "soon".to_string());
                Some((job.short_id().to_string(), job.name.clone(), next))
            } else {
                None
            };

            if let Some((short_id, name, next)) = result {
                store.save()?;
                Ok(CommandResult::Response(format!(
                    "Resumed job [{}] \"{}\"\nNext run: {}",
                    short_id, name, next
                )))
            } else {
                Ok(CommandResult::Response(format!("Job not found: {}", id)))
            }
        }

        _ => Ok(CommandResult::Response(
            "Cron job commands:\n\n\
             /cron list - List your scheduled jobs\n\
             /cron add <schedule> <prompt> [--target <channel_id>] - Create a new job\n\
             /cron remove <job-id> - Delete a job\n\
             /cron run <job-id> - Run immediately (for testing)\n\
             /cron pause <job-id> - Pause a job\n\
             /cron resume <job-id> - Resume a paused job\n\n\
             Schedule formats:\n\
             • every 10s / every 5m / every 1h - Recurring interval\n\
             • at 2024-01-28 14:00 - One-time execution\n\
             • 0 9 * * * - Cron expression (9 AM daily)\n\n\
             Options:\n\
             • --target <channel_id> - Send results to a specific channel (default: DM)\n\n\
             Examples:\n\
             /cron add every 1h Check my inbox\n\
             /cron add every 10s Say hello\n\
             /cron add 0 9 * * * Good morning!\n\
             /cron add every 1h Check emails --target C0123456789"
                .to_string(),
        )),
    }
}

/// Execute a cron job manually and return the output.
pub async fn execute_cron_job(job_id: &str, channel: &str, user_id: &str) -> Result<String> {
    let store = CronStore::load()?;
    let job = store
        .get(job_id, channel, user_id)
        .ok_or_else(|| anyhow::anyhow!("Job not found"))?;

    let channel_display = get_channel_info(channel).map(|c| c.display_name);
    let context_prompt = onboarding::build_context_prompt_for_user(
        channel_display,
        Some(channel),
        Some(user_id),
        Some(&job.prompt),
    )?;

    let config = crate::config::Config::load()?;
    let provider = sandbox::default_provider(&config);

    let turn = TurnJob {
        session_id: format!("{}:{}", channel, user_id),
        channel: channel.to_string(),
        user_id: user_id.to_string(),
        prompt: job.prompt.clone(),
        system_prompt: Some(context_prompt),
        resume_session: None,
        cwd: None,
        skip_permissions: true,
        backend: config.backend,
        model: None,
    };

    let tr = provider.run_turn(turn).await?;

    Ok(format!("[Cron: {}]\n\n{}", job.name, tr.response))
}

fn find_job_id(
    store: &CronStore,
    channel: &str,
    user_id: &str,
    id_or_prefix: &str,
) -> Result<String> {
    let id = id_or_prefix.trim();

    if store.get(id, channel, user_id).is_some() {
        return Ok(id.to_string());
    }

    let matches: Vec<_> = store
        .list_for_user(channel, user_id)
        .into_iter()
        .filter(|j| j.id.starts_with(id))
        .collect();

    match matches.len() {
        0 => anyhow::bail!("Job not found: {}", id),
        1 => Ok(matches[0].id.clone()),
        _ => anyhow::bail!(
            "Ambiguous job ID '{}'. Matches: {}",
            id,
            matches
                .iter()
                .map(|j| j.short_id())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

/// Query the AI backend; on session expiry, clears it and retries fresh.
pub async fn query_ai_with_session(
    store: &mut PairingStore,
    channel: &str,
    user_id: &str,
    text: &str,
    context_prompt: String,
    session_key_override: Option<&str>,
) -> Result<QueryResult> {
    let session_key = match session_key_override {
        Some(key) => key.to_string(),
        None => format!("{}:{}", channel, user_id),
    };
    let existing_session = store.sessions.get(&session_key).cloned();

    let config = crate::config::Config::load()?;
    let provider = sandbox::default_provider(&config);

    let job = TurnJob {
        session_id: session_key.clone(),
        channel: channel.to_string(),
        user_id: user_id.to_string(),
        prompt: text.to_string(),
        system_prompt: Some(context_prompt.clone()),
        resume_session: existing_session,
        cwd: None,
        skip_permissions: true,
        backend: config.backend,
        model: None,
    };

    let qr = match provider.run_turn(job).await {
        Ok(tr) => sandbox::query_result_from_turn(tr),
        Err(e) => {
            let error_msg = e.to_string();
            // If session not found, clear it and retry without resuming
            if error_msg.contains("No conversation found with session ID")
                || error_msg.contains("session")
            {
                warn!("Session expired, starting fresh conversation");
                store.sessions.remove(&session_key);
                store.save()?;

                audit::log_event("session_expired", Some(channel), Some(user_id), None);

                let retry_job = TurnJob {
                    session_id: session_key.clone(),
                    channel: channel.to_string(),
                    user_id: user_id.to_string(),
                    prompt: text.to_string(),
                    system_prompt: Some(context_prompt),
                    resume_session: None,
                    cwd: None,
                    skip_permissions: true,
                    backend: config.backend,
                    model: None,
                };

                match provider.run_turn(retry_job).await {
                    Ok(tr) => sandbox::query_result_from_turn(tr),
                    Err(e) => {
                        warn!("AI backend error on retry: {}", e);
                        QueryResult {
                            response: format!("Sorry, I encountered an error: {}", e),
                            session_id: String::new(),
                            duration_ms: None,
                            cost_usd: None,
                        }
                    }
                }
            } else {
                warn!("AI backend error: {}", e);
                QueryResult {
                    response: format!("Sorry, I encountered an error: {}", e),
                    session_id: String::new(),
                    duration_ms: None,
                    cost_usd: None,
                }
            }
        }
    };

    if !qr.session_id.is_empty()
        && store.sessions.get(&session_key).map(|s| s.as_str()) != Some(&qr.session_id)
    {
        store.sessions.insert(session_key, qr.session_id.clone());
        store.save()?;
    }

    Ok(qr)
}

/// Handle onboarding flow - AI drives the conversation
pub async fn handle_onboarding(channel: &str, user_id: &str, message: &str) -> Result<String> {
    let system_prompt = onboarding::system_prompt_for_user(channel, user_id)?;

    let options = backends::QueryOptions {
        system_prompt: Some(system_prompt),
        skip_permissions: true,
        ..Default::default()
    };

    let qr = backends::query_with_options(message, options).await?;
    Ok(qr.response)
}

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
    ChannelInfo {
        name: "slack",
        display_name: "Slack",
    },
];

/// Get channel info by name
pub fn get_channel_info(name: &str) -> Option<&'static ChannelInfo> {
    SUPPORTED_CHANNELS.iter().find(|c| c.name == name)
}
