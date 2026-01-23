use anyhow::Result;
use tokio::signal;
use tracing::{error, info, warn};

use crate::channels::{signal as signal_channel, telegram};
use crate::config::Config;
use crate::memory::MemoryIndex;
use crate::pairing::PairingStore;
use crate::setup;

/// Run the assistant (default command)
pub async fn run() -> Result<()> {
    // Check if configured
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

    // Ensure runtime dependencies are ready
    info!("Preparing runtime...");
    if let Err(e) = setup::ensure_embedding_model() {
        warn!("Failed to prepare embedding model: {}", e);
    }

    // Index memories for all approved users at startup
    index_all_user_memories();

    // Spawn tasks for each configured channel
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

    // Wait for Ctrl+C
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

    Ok(())
}

/// Index memories for all approved users
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

    // Index memories for each approved user
    for (key, _) in &store.approved {
        // Key format is "channel:user_id"
        let parts: Vec<&str> = key.splitn(2, ':').collect();
        if parts.len() != 2 {
            continue;
        }
        let (channel, user_id) = (parts[0], parts[1]);

        if let Err(e) = index.index_user_memories(channel, user_id) {
            warn!(
                "Failed to index memories for {}:{}: {}",
                channel, user_id, e
            );
        }
    }

    info!("Memory indexing complete");
}
