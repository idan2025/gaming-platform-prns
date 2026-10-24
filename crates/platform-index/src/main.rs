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

/// How often the idle sweep runs.
///
/// Not the same thing as the idle timeout: the timeout decides *whether* an
/// instance is over its welcome, and this decides how late the answer can be.
/// A minute is far below any sane timeout and costs one listing per node.
const REAP_INTERVAL: Duration = Duration::from_secs(60);

/// How often the hosting file is checked for edits.
const HOSTING_RELOAD_EVERY: std::time::Duration = std::time::Duration::from_secs(10);

const USAGE: &str = "\
platform-index — an optional announce indexer and HTTP front door

usage: platform-index [--bind ADDR] [--tcp HOST:PORT] [--auto] [--hosting FILE] [--identity FILE]
       platform-index --help | --version

  --bind ADDR       where the HTTP API listens (default: 127.0.0.1:4760)
  --tcp HOST:PORT   dial a Reticulum TCP peer, or 0.0.0.0:PORT to bind one
  --auto            attach auto-discovered local interfaces
  --identity FILE   keep the index's identity in this file (made on first run,
                    readable by its owner only). Without it the identity is new
                    at every start, so the address players add and the entry in
                    a node's trusted_indexes both stop working after a restart.
  --hosting FILE    enable hosted deploy from this config. Off without it, and
                    off with it unless the file lists games and a node.

An index is a cache of the mesh, never a source of truth: it hears servers the
same way a launcher does and nothing depends on any particular one existing.

RUST_LOG sets log filtering (default: platform_index=info).";

#[tokio::main]
async fn main() -> Result<()> {
    let filter = std::env::var("RUST_LOG").unwrap_or_else(|_| "platform_index=info".to_string());
    let _ = tracing_subscriber::fmt().with_env_filter(filter).try_init();

    let mut bind = "127.0.0.1:4760".to_string();
    let mut tcp = None;
    let mut auto = false;
    let mut hosting_path: Option<String> = None;
    let mut identity_path: Option<String> = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--bind" => bind = args.next().context("--bind needs an address")?,
            "--tcp" => tcp = Some(args.next().context("--tcp needs host:port")?),
            "--auto" => auto = true,
            "--identity" => identity_path = Some(args.next().context("--identity needs a path")?),
            "--hosting" => hosting_path = Some(args.next().context("--hosting needs a path")?),
            "-h" | "--help" => {
                println!("{USAGE}");
                return Ok(());
            }
            "-V" | "--version" => {
                println!("platform-index {}", env!("CARGO_PKG_VERSION"));
                return Ok(());
            }
            other => anyhow::bail!("unknown argument {other:?}\n\n{USAGE}"),
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
            let config = platform_index::hosting::Hosting::load(std::path::Path::new(path))?;
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
    match &identity_path {
        Some(path) => load_or_make_identity(std::path::Path::new(path), &mut secret)?,
        None => {
            tracing::warn!("no --identity file; this index's address changes at every start");
            getrandom_bytes(&mut secret)?;
        }
    }
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

    // Edits to the hosting file are picked up without a restart: an operator
    // adds or removes a node, a game or a quota by saving the file. Checked by
    // modification time; a save that does not parse is logged and the
    // configuration in force stays.
    if let (Some(path), true) = (hosting_path.clone(), state.hosting.is_some()) {
        let state = state.clone();
        tokio::spawn(async move {
            let path = std::path::PathBuf::from(path);
            let modified = |p: &std::path::Path| std::fs::metadata(p).and_then(|m| m.modified()).ok();
            let mut seen = modified(&path);
            let mut tick = tokio::time::interval(HOSTING_RELOAD_EVERY);
            loop {
                tick.tick().await;
                let now = modified(&path);
                if now.is_none() || now == seen {
                    continue;
                }
                seen = now;
                if let Some(hosting) = &state.hosting {
                    if let Err(e) = hosting.reload_from(&path) {
                        tracing::warn!(error = %format!("{e:#}"), "hosting config not reloaded; keeping the one in force");
                    }
                }
            }
        });
    }

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

    // Idle reaping. `PLAN.md` §8 phase 4 wants it "from day one", because public
    // deploy plus anonymous identities is free compute — and until now the
    // policy existed and nothing ran it, so an abandoned server ran until an
    // operator noticed. The sweep is cheap (one listing per node) and its
    // interval is not the timeout: it decides how late a reap can be, not
    // whether one happens.
    if state.hosting.is_some() {
        let reaper_state = state.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(REAP_INTERVAL);
            loop {
                ticker.tick().await;
                let Some(hosting) = &reaper_state.hosting else { break };
                match hosting.reap_idle(std::time::SystemTime::now()).await {
                    Ok(reaped) if !reaped.is_empty() => {
                        tracing::info!(count = reaped.len(), "reaped idle instances")
                    }
                    Ok(_) => {}
                    // A sweep that could not reach a node is a warning, not a
                    // reason to stop sweeping: the next tick tries again.
                    Err(e) => tracing::warn!(error = %format!("{e:#}"), "idle sweep failed"),
                }
                match hosting.expire_stopped().await {
                    Ok(gone) if !gone.is_empty() => tracing::info!(count = gone.len(), "removed stopped servers"),
                    Ok(_) => {}
                    Err(e) => tracing::warn!(error = %format!("{e:#}"), "stopped-server sweep failed"),
                }
            }
        });
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

/// The identity kept in `path` as 128 hex characters, or a new one written
/// there, owner-only. A file that exists and is not an identity is an error and
/// is never overwritten: replacing it would silently change the address every
/// player and every node trusts.
fn load_or_make_identity(path: &std::path::Path, secret: &mut [u8; 64]) -> Result<()> {
    match std::fs::read_to_string(path) {
        Ok(text) => {
            let bytes = hex::decode(text.trim()).with_context(|| format!("{} is not a hex identity", path.display()))?;
            anyhow::ensure!(bytes.len() == 64, "{} holds {} bytes, an identity is 64", path.display(), bytes.len());
            secret.copy_from_slice(&bytes);
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            getrandom_bytes(secret)?;
            write_private(path, hex::encode(secret.as_slice()).as_bytes())
                .with_context(|| format!("writing a new identity to {}", path.display()))?;
            tracing::info!(path = %path.display(), "made a new index identity");
            Ok(())
        }
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

fn write_private(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)?.write_all(bytes)
}

fn getrandom_bytes(buf: &mut [u8]) -> Result<()> {
    getrandom::getrandom(buf).map_err(|e| anyhow::anyhow!("no entropy: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("index-identity-{}-{name}", std::process::id()))
    }

    #[test]
    fn an_identity_is_made_once_and_read_back_the_same() {
        let path = temp("once");
        let _ = std::fs::remove_file(&path);
        let (mut a, mut b) = ([0u8; 64], [0u8; 64]);
        load_or_make_identity(&path, &mut a).unwrap();
        load_or_make_identity(&path, &mut b).unwrap();
        assert_eq!(a, b, "the second start has the first start's identity");
        assert_ne!(a, [0u8; 64]);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        }
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_file_that_is_not_an_identity_is_refused_and_left_alone() {
        let path = temp("junk");
        std::fs::write(&path, "not an identity").unwrap();
        let mut s = [0u8; 64];
        assert!(load_or_make_identity(&path, &mut s).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "not an identity");
        let _ = std::fs::remove_file(&path);
    }
}
