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
use platform_agent::agent::Agent;
use platform_agent::api;
use platform_agent::config::AgentConfig;
use platform_agent::packs;
use platform_agent::uplink;

const USAGE: &str = "\
platform-agent — run many game servers on one host

usage: platform-agent <config.toml> [pack-dir]
       platform-agent --help | --version

  <config.toml>  this node's settings: data root, port range, per-game runtimes.
                 There is no default: this process creates containers, so which
                 file told it what to run should be visible on the command line.
  [pack-dir]     directory of game packs (default: ./packs)

The local API is loopback-only and unauthenticated. A `[uplink]` section in the
config additionally serves an authenticated control destination over Reticulum,
so an index can drive this node with no inbound port.

RUST_LOG sets log filtering (default: platform_agent=info).";

#[tokio::main]
async fn main() -> Result<()> {
    tracing_init();

    let mut args = std::env::args().skip(1);
    let config_path = args.next().ok_or_else(|| anyhow::anyhow!("{USAGE}"))?;
    // Before anything is loaded: an operator asking what this is should not be
    // told their `--help` is a missing config file.
    if matches!(config_path.as_str(), "-h" | "--help") {
        println!("{USAGE}");
        return Ok(());
    }
    if matches!(config_path.as_str(), "-V" | "--version") {
        println!("platform-agent {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    let pack_dir = args.next().unwrap_or_else(|| "packs".to_string());

    let config = AgentConfig::load(std::path::Path::new(&config_path))
        .map_err(|e| anyhow::anyhow!("{e}"))
        .with_context(|| format!("loading {config_path}"))?;

    // Packs are read under the operator's trust policy (`PLAN.md` §11.4): a
    // pack this node will not deploy is never loaded, and a pack whose
    // signature failed is refused rather than demoted to unsigned.
    let policy = config.pack_trust_policy();
    if config.pack_trust.is_none() {
        tracing::warn!(
            "no [pack_trust] section: every readable pack in {pack_dir} is deployable here, \
             signed or not. Add one to restrict what this node will run (PLAN.md §11.4)"
        );
    }
    let loaded = packs::load_deployable(
        std::path::Path::new(&pack_dir),
        &policy,
        std::time::SystemTime::now(),
    )
    .map_err(|e| anyhow::anyhow!("{e}"))
    .with_context(|| format!("loading packs from {pack_dir}"))?;
    for (path, e) in &loaded.errors {
        tracing::warn!(path = %path, error = %e, "skipping an unreadable game pack");
    }
    for refused in &loaded.refused {
        tracing::warn!(
            pack = %refused.pack.pack.id,
            file = %refused.pack.file,
            "refusing to load a game pack: {}",
            refused.why()
        );
    }
    tracing::info!(
        packs = loaded.packs.len(),
        refused = loaded.refused.len(),
        "game packs loaded"
    );

    let bind = config.api_bind;
    // Loaded before the agent starts, because a misconfigured token should stop
    // the process rather than surface as a 401 on the first request. Generated
    // on first run when the operator pointed at a file, so a containerised
    // agent has a secret without anyone inventing one.
    let api_token = config
        .load_api_token()
        .with_context(|| "reading the API token")?;
    if let (Some(path), Some(_)) = (&config.api_token_file, &api_token) {
        tracing::info!(path = %path.display(), "API token in use; read it from this file");
    }
    let uplink_config = config.uplink.clone();
    let agent = Arc::new(Agent::new(config, loaded.packs).await?);

    // The Reticulum control uplink is opt-in via `[uplink]` in the config. When
    // present, the agent also announces a `platform-agent.control` destination
    // and answers authenticated create/stop/remove/list requests over a Link, so
    // an index can drive this node with no inbound port and no public IP. The
    // loopback API below keeps working either way.
    let _uplink_node = if let Some(cfg) = uplink_config {
        Some(uplink::start(agent.clone(), cfg).await?)
    } else {
        None
    };

    let listener = tokio::net::TcpListener::bind(bind)
        .await
        .with_context(|| format!("binding the local API on {bind}"))?;
    match &api_token {
        Some(_) => tracing::info!(
            %bind,
            "agent listening; every request needs the API token from api_token_file"
        ),
        None => tracing::info!(
            %bind,
            "agent listening; this API has no authentication and is loopback-only"
        ),
    }

    axum::serve(
        listener,
        api::router_full(agent, api_token, Some(std::path::PathBuf::from(&pack_dir))),
    )
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
