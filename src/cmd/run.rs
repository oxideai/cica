use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use anyhow::Result;
use tokio::signal;
use tokio::sync::Mutex;
use tracing::{error, info, warn};

use crate::channels::{signal as signal_channel, slack, telegram};
use crate::config::Config;
use crate::cron::{CronConfig, CronService, SystemClock};
use crate::memory::MemoryIndex;
use crate::pairing::PairingStore;
use crate::setup;

pub async fn run() -> Result<()> {
    if !Config::exists()? {
        println!("Cica is not configured yet.");
        println!("Run `cica init` to get started.");
        return Ok(());
    }

    let config = Config::load()?;
    let channels = config.configured_channels();

    if channels.is_empty() {
        println!("No channels configured.");
        println!("Run `cica init` to add a channel.");
        return Ok(());
    }

    info!("Starting Cica with channels: {}", channels.join(", "));

    info!("Preparing runtime...");
    if let Err(e) = setup::ensure_deps(&config).await {
        warn!("Failed to prepare dependencies: {}", e);
    }

    index_all_user_memories();

    let cron_service = start_cron_service(&config)?;

    // Skills git-sync (router-side): keep skills_dir + the state store's "skills"
    // prefix fresh from the configured repo. No-op when [skills] is unset.
    if let Some(skills_cfg) = config.skills.clone() {
        match crate::config::paths() {
            Ok(paths) => {
                let store = match crate::sandbox::state::default_store(&config) {
                    Ok(s) => s,
                    Err(e) => {
                        warn!(
                            "Failed to build state store for skills sync (continuing local-only): {e}"
                        );
                        None
                    }
                };
                tokio::spawn(crate::skills_sync::run_sync_loop(
                    skills_cfg,
                    store,
                    paths.skills_dir,
                ));
                info!("Skills sync started");
            }
            Err(e) => warn!("Failed to resolve paths for skills sync: {}", e),
        }
    }

    let mut handles = Vec::new();

    if let Some(telegram_config) = config.channels.telegram {
        handles.push(tokio::spawn(async move {
            if let Err(e) = telegram::run(telegram_config).await {
                error!("Telegram channel error: {}", e);
            }
        }));
    }

    if let Some(signal_config) = config.channels.signal {
        handles.push(tokio::spawn(async move {
            if let Err(e) = signal_channel::run(signal_config).await {
                error!("Signal channel error: {}", e);
            }
        }));
    }

    if let Some(slack_config) = config.channels.slack {
        handles.push(tokio::spawn(async move {
            if let Err(e) = slack::run(slack_config).await {
                error!("Slack channel error: {}", e);
            }
        }));
    }

    tokio::select! {
        _ = signal::ctrl_c() => {
            info!("Received Ctrl+C, shutting down...");
        }
        _ = async {
            for handle in handles {
                let _ = handle.await;
            }
        } => {}
    }

    if let Some(service) = cron_service {
        let mut service = service.lock().await;
        service.stop().await;
    }

    Ok(())
}

fn start_cron_service(config: &Config) -> Result<Option<Arc<Mutex<CronService<SystemClock>>>>> {
    let clock = SystemClock;
    let cron_config = CronConfig::default();

    let mut service = match CronService::new(clock, cron_config) {
        Ok(s) => s,
        Err(e) => {
            warn!("Failed to initialize cron service: {}", e);
            return Ok(None);
        }
    };

    let telegram_token = config
        .channels
        .telegram
        .as_ref()
        .map(|c| c.bot_token.clone());
    let signal_phone = config
        .channels
        .signal
        .as_ref()
        .map(|c| c.phone_number.clone());
    let slack_bot_token = config.channels.slack.as_ref().map(|c| c.bot_token.clone());

    let result_sender: crate::cron::ResultSender =
        Arc::new(move |channel, user_id, target, message| {
            let telegram_token = telegram_token.clone();
            let signal_phone = signal_phone.clone();
            let slack_bot_token = slack_bot_token.clone();

            Box::pin(async move {
                match channel.as_str() {
                    "telegram" => {
                        if let Some(token) = telegram_token {
                            send_telegram_message(&token, &user_id, &message).await
                        } else {
                            Err(anyhow::anyhow!("Telegram not configured"))
                        }
                    }
                    "signal" => {
                        if let Some(_phone) = signal_phone {
                            send_signal_message(&user_id, &message).await
                        } else {
                            Err(anyhow::anyhow!("Signal not configured"))
                        }
                    }
                    "slack" => {
                        if let Some(token) = slack_bot_token {
                            let effective_channel = target.resolve_channel_id(&user_id);
                            send_slack_message(
                                &token,
                                effective_channel,
                                target.thread_id.as_deref(),
                                &message,
                            )
                            .await
                        } else {
                            Err(anyhow::anyhow!("Slack not configured"))
                        }
                    }
                    _ => Err(anyhow::anyhow!("Unknown channel: {}", channel)),
                }
            }) as Pin<Box<dyn Future<Output = Result<()>> + Send>>
        });

    service.start(result_sender);
    info!("Cron scheduler started");

    Ok(Some(Arc::new(Mutex::new(service))))
}

async fn send_telegram_message(token: &str, user_id: &str, message: &str) -> Result<()> {
    use teloxide::prelude::*;

    let bot = Bot::new(token);
    let chat_id: i64 = user_id.parse()?;
    bot.send_message(ChatId(chat_id), message).await?;
    Ok(())
}

async fn send_signal_message(recipient: &str, message: &str) -> Result<()> {
    use jsonrpsee::core::client::ClientT;
    use jsonrpsee::core::params::ObjectParams;
    use jsonrpsee::http_client::HttpClientBuilder;
    use serde_json::Value;

    let url = "http://127.0.0.1:18080/api/v1/rpc";
    let client = HttpClientBuilder::default().build(url)?;

    let mut params = ObjectParams::new();
    params.insert("recipient", vec![recipient])?;
    params.insert("message", message)?;

    let _: Value = client.request("send", params).await?;
    Ok(())
}

async fn send_slack_message(
    bot_token: &str,
    channel_id: &str,
    thread_ts: Option<&str>,
    message: &str,
) -> Result<()> {
    use slack_morphism::prelude::*;

    let client = SlackClient::new(SlackClientHyperConnector::new()?);
    let token = SlackApiToken::new(bot_token.into());
    let session = client.open_session(&token);

    let mrkdwn_message = crate::channels::slack::markdown_to_mrkdwn(message);

    let mut request = SlackApiChatPostMessageRequest::new(
        channel_id.into(),
        SlackMessageContent::new().with_text(mrkdwn_message),
    );

    if let Some(ts) = thread_ts {
        request = request.with_thread_ts(ts.into());
    }

    session.chat_post_message(&request).await?;
    Ok(())
}

/// Startup warm-up: index whatever memories are already on local disk. In cloud
/// mode the per-turn `reindex_user_memories` hook is authoritative (it pulls from
/// the store first), so the index converges after the first turn either way.
fn index_all_user_memories() {
    let store = match PairingStore::load() {
        Ok(s) => s,
        Err(e) => {
            warn!("Failed to load pairing store for memory indexing: {}", e);
            return;
        }
    };

    let mut index = match MemoryIndex::open() {
        Ok(i) => i,
        Err(e) => {
            warn!("Failed to open memory index: {}", e);
            return;
        }
    };

    for (channel, user_ids) in &store.approved {
        for user_id in user_ids {
            if let Err(e) = index.index_user_memories(channel, user_id) {
                warn!(
                    "Failed to index memories for {}:{}: {}",
                    channel, user_id, e
                );
            }
        }
    }

    info!("Memory indexing complete");
}
