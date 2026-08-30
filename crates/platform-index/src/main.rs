//! `platform-index` — an optional announce indexer.
//!
//! Usage: `platform-index [--bind ADDR] [--tcp HOST:PORT] [--auto]`
//!
//! It hears servers the same way a launcher does, remembers them a little
//! longer, and offers an HTTP front door. Anyone can run one; nothing depends on
//! any particular one existing (`DESIGN.md` §0).

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use game_bridge::config::BrowserArgs;
use game_bridge::BridgeSession;
use platform_auth::Authenticator;
use platform_index::http::{router, IndexState};
use platform_index::registry::Registry;
use tokio::sync::Mutex;

/// How often the index folds the browse session's view into its own memory.
const INGEST_INTERVAL: Duration = Duration::from_secs(5);

#[tokio::main]
async fn main() -> Result<()> {
    let filter = std::env::var("RUST_LOG").unwrap_or_else(|_| "platform_index=info".to_string());
    let _ = tracing_subscriber::fmt().with_env_filter(filter).try_init();

    let mut bind = "127.0.0.1:4760".to_string();
    let mut tcp = None;
    let mut auto = false;
    let mut hosting_path: Option<String> = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--bind" => bind = args.next().context("--bind needs an address")?,
            "--tcp" => tcp = Some(args.next().context("--tcp needs host:port")?),
            "--auto" => auto = true,
            "--hosting" => hosting_path = Some(args.next().context("--hosting needs a path")?),
            other => anyhow::bail!("unknown argument {other:?}"),
        }
    }

    // Hosting is opt-in twice over: an operator has to pass a config file, and
    // that file has to list games. An index with neither is a directory, which
    // is what most of them should be.
    let hosting = match &hosting_path {
        None => {
            tracing::info!("no --hosting config; this index is a directory only");
            None
        }
        Some(path) => {
            let src = std::fs::read_to_string(path)
                .with_context(|| format!("reading {path}"))?;
            let config: platform_index::hosting::HostingConfig =
                toml::from_str(&src).with_context(|| format!("parsing {path}"))?;
            if config.enabled() {
                tracing::info!(games = ?config.games, nodes = config.nodes.len(), "hosting enabled");
            } else {
                tracing::warn!(
                    "{path} does not enable hosting: it needs both a games list and a node"
                );
            }
            Some(platform_index::hosting::Hosting::new(config))
        }
    };

    // The index's own identity, which is also the audience clients bind their
    // signatures to. Ephemeral for now: a restart invalidates outstanding
    // sessions, which is correct for something that holds no durable state.
    let mut secret = [0u8; 64];
    getrandom_bytes(&mut secret)?;
    let identity = prns_core::identity::PrivateIdentityMaterial::from_slice(&secret)
        .map_err(|e| anyhow::anyhow!("{e:?}"))?;
    tracing::info!(
        identity = %hex::encode(identity.identity_hash().as_bytes()),
        "index identity; clients bind their signatures to this"
    );
    // The agent-client path reuses this same key as the index's credential when
    // it authenticates to a remote agent: one identity, presented the same way to
    // users (who sign for it) and to agents (which trust it via trusted_indexes).
    let agent_client_secret = personal_rns::prelude::Zeroizing::new(secret);

    let browser = BridgeSession::start_browser(BrowserArgs {
        tcp: tcp.clone(),
        auto,
    })
    .await?;
    let state = Arc::new(IndexState {
        registry: Mutex::new(Registry::new()),
        auth: Mutex::new(Authenticator::new(identity.identity_hash())),
        hosting,
    });

    // The Reticulum half. This is the one that has to exist for an index to be
    // a convenience rather than a dependency: a client on a mesh with no
    // internet reaches this, and finds it by hearing its announce.
    let index_node = platform_index::node::start(
        state.clone(),
        agent_client_secret.clone(),
        tcp,
        auto,
    )
    .await?;
    tracing::info!(
        destination = %hex::encode(index_node.destination().as_bytes()),
        "queryable over Reticulum at this destination"
    );

    // If hosting is on and any node is a remote agent (`NodeConfig.agent` set),
    // the index drives it over Reticulum with the same node + identity it serves
    // queries from. Built once, held in the Hosting slot; HTTP-only nodes still
    // take the `api` path, so an index with a mix of both works.
    if state.hosting.is_some() {
        let client = platform_index::agent_client::AgentClient::new(
            index_node.handle(),
            agent_client_secret.clone(),
        );
        if let Err(e) = client.identity_hash() {
            tracing::warn!(error = %format!("{e:#}"), "could not derive the index identity for agent uplink; remote-agent nodes will be unreachable");
        }
        if let Some(h) = &state.hosting {
            h.set_agent_client(client).await;
        }
    }

    let ingest_state = state.clone();
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(INGEST_INTERVAL);
        loop {
            ticker.tick().await;
            let heard = browser.discovered().await;
            let mut registry = ingest_state.registry.lock().await;
            registry.ingest(heard, Instant::now());
        }
    });

    let listener = tokio::net::TcpListener::bind(&bind)
        .await
        .with_context(|| format!("binding the index HTTP API on {bind}"))?;
    tracing::info!(%bind, "index listening; this is a cache of the mesh, not an authority");
    axum::serve(listener, router(state)).await.context("serving the index")?;
    Ok(())
}

fn getrandom_bytes(buf: &mut [u8]) -> Result<()> {
    getrandom::getrandom(buf).map_err(|e| anyhow::anyhow!("no entropy: {e}"))
}
