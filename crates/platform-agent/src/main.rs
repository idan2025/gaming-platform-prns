//! `platform-agent` — run many game servers on one host.
//!
//! Usage: `platform-agent <config.toml> [pack-dir]`
//!
//! Both paths are the operator's. There is no discovery of a config from the
//! environment and no default that would let the agent start with settings
//! nobody chose: this process creates containers, and it should be obvious from
//! the command line which file told it what to run.

use std::sync::Arc;

use anyhow::{Context, Result};
use game_bridge::GamePack;
use platform_agent::agent::Agent;
use platform_agent::api;
use platform_agent::config::AgentConfig;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_init();

    let mut args = std::env::args().skip(1);
    let config_path = args.next().ok_or_else(|| {
        anyhow::anyhow!("usage: platform-agent <config.toml> [pack-dir]")
    })?;
    let pack_dir = args.next().unwrap_or_else(|| "packs".to_string());

    let config = AgentConfig::load(std::path::Path::new(&config_path))
        .map_err(|e| anyhow::anyhow!("{e}"))
        .with_context(|| format!("loading {config_path}"))?;

    let loaded = GamePack::load_dir(std::path::Path::new(&pack_dir))
        .map_err(|e| anyhow::anyhow!("{e}"))
        .with_context(|| format!("loading packs from {pack_dir}"))?;
    for (path, e) in &loaded.errors {
        tracing::warn!(path = %path, error = %e, "skipping an unreadable game pack");
    }
    tracing::info!(packs = loaded.packs.len(), "game packs loaded");

    let bind = config.api_bind;
    let agent = Arc::new(Agent::new(config, loaded.packs).await?);

    let listener = tokio::net::TcpListener::bind(bind)
        .await
        .with_context(|| format!("binding the local API on {bind}"))?;
    tracing::info!(%bind, "agent listening; this API has no authentication and is loopback-only");

    axum::serve(listener, api::router(agent))
        .await
        .context("serving the local API")?;
    Ok(())
}

fn tracing_init() {
    // Deliberately plain: an agent's log is read over SSH at 3am, not in a
    // dashboard.
    let filter = std::env::var("RUST_LOG").unwrap_or_else(|_| "platform_agent=info".to_string());
    let _ = tracing_subscriber::fmt().with_env_filter(filter).try_init();
}
