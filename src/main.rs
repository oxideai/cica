mod atomic;
mod audit;
mod backends;
mod channels;
mod cmd;
mod config;
mod cron;
mod memory;
mod onboarding;
mod pairing;
mod runtime;
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
        turn: Option<String>,
        /// Affinity assigned to a persistent worker
        #[arg(long)]
        session: Option<String>,
        /// Stable worker identifier
        #[arg(long)]
        worker_id: Option<String>,
        /// Worker idle limit in seconds
        #[arg(long)]
        idle_secs: Option<u64>,
        /// Per-turn limit in seconds
        #[arg(long)]
        turn_timeout_secs: Option<u64>,
        /// Router-computed worker compatibility hash
        #[arg(long)]
        policy_hash: Option<String>,
        /// Isolated worker data directory
        #[arg(long)]
        home: Option<std::path::PathBuf>,
        /// Shared dependency directory
        #[arg(long)]
        deps: Option<std::path::PathBuf>,
        /// Shared skills directory
        #[arg(long)]
        skills: Option<std::path::PathBuf>,
        /// Configuration file
        #[arg(long)]
        config: Option<std::path::PathBuf>,
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
        Some(Commands::Worker {
            turn,
            session,
            worker_id,
            idle_secs,
            turn_timeout_secs,
            policy_hash,
            home,
            deps,
            skills,
            config,
        }) => {
            cmd::worker::run(
                turn.as_deref(),
                session.as_deref(),
                worker_id.as_deref(),
                idle_secs,
                turn_timeout_secs,
                policy_hash.as_deref(),
                home,
                deps,
                skills,
                config,
            )
            .await
        }
        None => cmd::run::run().await,
    }
}
