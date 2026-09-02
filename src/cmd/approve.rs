use anyhow::Result;
use tracing::info;

use crate::channels;
use crate::pairing::PairingStore;

pub fn run(code: &str) -> Result<()> {
    let paths = crate::config::paths()?;
    crate::audit::init(
        paths.audit_db.clone(),
        crate::config::Config::load()
            .map(|config| config.audit)
            .unwrap_or(true),
    );
    let mut store = PairingStore::load(&paths)?;

    let request = store.modify(|store| store.approve(code))?;

    let channel_display = channels::get_channel_info(&request.channel)
        .map(|c| c.display_name)
        .unwrap_or(&request.channel);

    let user_display = request
        .display_name
        .as_ref()
        .or(request.username.as_ref())
        .map(|s| s.as_str())
        .unwrap_or(&request.user_id);

    println!("Approved {} user: {}", channel_display, user_display);

    info!(
        "Approved {} user {} ({})",
        request.channel, request.user_id, user_display
    );

    Ok(())
}
