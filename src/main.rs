mod audit;
mod backends;
mod channels;
mod cmd;
mod config;
mod cron;
mod memory;
mod onboarding;
mod pairing;
mod sandbox;
mod setup;
mod skills;
mod skills_sync;

use anyhow::Result;
use clap::{Parser, Subcommand};
use tracing_subscriber::{EnvFilter, layer::SubscriberExt, util::SubscriberInitExt};

#[derive(Parser)]
#[command(name = "cica")]
#[command(about = "A personal AI assistant that lives in your chat")]
#[command(version)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Set up Cica or add a new channel
    Init,

    /// Approve a pairing request
    Approve {
        /// The pairing code shown to the user
        code: String,
    },

    /// Show where Cica stores its data
    Paths,

    /// Run a single turn as a one-shot worker (internal; used by the router)
    Worker {
        /// The turn id whose job/result live in the state store
        #[arg(long)]
        turn: String,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    if std::env::var("CICA_LOG_JSON").is_ok() {
        tracing_subscriber::registry()
            .with(filter)
            .with(tracing_subscriber::fmt::layer().json())
            .init();
    } else {
        tracing_subscriber::registry()
            .with(filter)
            .with(tracing_subscriber::fmt::layer())
            .init();
    }

    let cli = Cli::parse();

    match cli.command {
        Some(Commands::Init) => cmd::init::run().await,
        Some(Commands::Approve { code }) => cmd::approve::run(&code),
        Some(Commands::Paths) => cmd::paths::run(),
        Some(Commands::Worker { turn }) => cmd::worker::run(&turn).await,
        None => cmd::run::run().await,
    }
}
