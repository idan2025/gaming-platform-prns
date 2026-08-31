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
    warn_if_data_root_is_not_shared_with_the_daemon(&agent).await;

    // Containers outlive this process, so a restart can find servers running
    // that nothing is announcing. Put them back on the mesh before serving.
    agent.restore_mesh_bridges().await;

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

    // Live mesh-interface control for the web UI (`interfaces.rs`, `PLAN.md`
    // §13.5). When the uplink is up we hand its node handle to a manager, load
    // any interfaces the operator saved from the UI on a previous run, and
    // re-attach them so a mesh link survives a restart. With no uplink the
    // manager is `None` and the routes answer "no uplink" rather than pretending
    // to configure a node that does not exist.
    let interfaces = match &_uplink_node {
        Some(node) => {
            let sidecar = agent.config().data_root.join("agent-interfaces.json");
            let destination = hex::encode(node.destination().as_bytes());
            let manager = platform_agent::interfaces::InterfaceManager::new(
                Some(node.handle()),
                Some(destination),
                Some(sidecar),
            );
            manager.reattach_saved().await;
            Some(manager.shared())
        }
        None => None,
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
        api::router_full(
            agent,
            api_token,
            Some(std::path::PathBuf::from(&pack_dir)),
            interfaces,
        ),
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

/// Warn when this agent's `data_root` is not the same path to the Docker daemon
/// as it is here.
///
/// The agent starts game servers as sibling containers and asks the **host's**
/// daemon to bind-mount each instance directory, so the daemon resolves
/// `data_root` in the host's filesystem rather than in this process's. If the
/// two disagree, Docker does not error: it creates the missing bind source as
/// an empty directory, and a game server starts with no game files. That is a
/// baffling symptom from the other end, so say it here instead.
///
/// The daemon is asked rather than `/proc/self/mountinfo` guessed at, because
/// from inside a container a correct `-v /data:/data` and a broken named volume
/// at `/data` look identical — both are a mount point called `/data`. Anything
/// the daemon will not confirm stays silent: a false alarm about a working
/// deployment teaches an operator to ignore the message.
async fn warn_if_data_root_is_not_shared_with_the_daemon(agent: &Agent) {
    let root = &agent.config().data_root;
    if agent.docker().path_is_shared_with_host(root).await == Some(false) {
        tracing::warn!(
            data_root = %root.display(),
            "data_root is not the same path to the Docker daemon as it is here. \
             Game servers run as sibling containers, so the daemon resolves this path \
             on the HOST — Docker will create an empty directory there instead of \
             failing, and your servers will start with no game files. Bind the same \
             absolute path on both sides (-v {0}:{0}); a named volume cannot work here",
            root.display()
        );
    }
}
