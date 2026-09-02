//! `cica worker --turn <id>`: run exactly one turn, then exit.
//!
//! Reads the `TurnJob` from the state store, runs it through the same
//! `HydratingProvider` the in-process path uses, and writes the `TurnResult`
//! back to the store. Exits non-zero (without a result) on any failure.

use anyhow::{Result, anyhow};

use crate::config::Config;
use crate::sandbox::LocalProcessProvider;
use crate::sandbox::hydrating::HydratingProvider;
use crate::sandbox::state::default_store;
use crate::sandbox::worker::run_worker_turn;

pub async fn run(turn_id: &str) -> Result<()> {
    let config = Config::load()?;
    let paths = crate::config::paths()?;

    let store = default_store(&config, &paths)?
        .ok_or_else(|| anyhow!("`cica worker` requires [deployment].store to be configured"))?;

    let engine = HydratingProvider::new(
        LocalProcessProvider::new(config.clone(), paths.clone()),
        store.clone(),
        paths.claude_home.clone(),
        paths.cursor_home.clone(),
        paths.base.clone(),
    );

    run_worker_turn(store.as_ref(), &engine, turn_id).await
}
