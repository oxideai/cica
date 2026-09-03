//! `cica worker --turn <id>`: run exactly one turn, then exit.
//!
//! Reads the `TurnJob` from the state store, runs it through the same
//! `HydratingProvider` the in-process path uses, and writes the `TurnResult`
//! back to the store. Exits non-zero (without a result) on any failure.

use anyhow::{Result, anyhow};

use std::path::PathBuf;

use crate::config::{Config, Paths};
use crate::sandbox::LocalProcessProvider;
use crate::sandbox::hydrating::HydratingProvider;
use crate::sandbox::state::default_store;
use crate::sandbox::worker::run_worker_turn;

pub async fn run(
    turn_id: Option<&str>,
    session: Option<&str>,
    home: Option<PathBuf>,
    deps: Option<PathBuf>,
    skills: Option<PathBuf>,
    config_file: Option<PathBuf>,
) -> Result<()> {
    let router = crate::config::paths()?;
    let mut router = router;
    if let Some(path) = config_file {
        router.config_file = path;
    }
    if let Some(path) = deps {
        router.deps_dir = path.clone();
        router.bun_dir = path.join("bun");
        router.java_dir = path.join("java");
        router.signal_cli_dir = path.join("signal-cli");
        router.claude_code_dir = path.join("claude-code");
        router.cursor_cli_dir = path.join("cursor-cli");
    }
    if let Some(path) = skills {
        router.skills_dir = path;
    }
    let paths = home.map_or_else(|| router.clone(), |base| Paths::for_worker(base, &router));
    let config = Config::load_from(&paths.config_file)?;

    let store = default_store(&config, &paths)?
        .ok_or_else(|| anyhow!("`cica worker` requires [deployment].store to be configured"))?;

    let engine = HydratingProvider::new(
        LocalProcessProvider::new(config.clone(), paths.clone()),
        store.clone(),
        paths.claude_home.clone(),
        paths.cursor_home.clone(),
        paths.base.clone(),
    );

    let turn_id = turn_id.ok_or_else(|| {
        anyhow!(
            "persistent worker sessions are not available yet{}",
            session
                .map(|value| format!(" ({value})"))
                .unwrap_or_default()
        )
    })?;
    run_worker_turn(store.as_ref(), &engine, turn_id).await
}
