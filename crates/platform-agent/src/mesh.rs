//! Putting a running game server on the Reticulum mesh.
//!
//! This is the piece that makes the node a *mesh* host rather than a
//! LAN-scoped container orchestrator. Until it existed the agent started game
//! containers, published UDP ports on the node's own network, and nothing ever
//! announced them: a browser on the mesh could not find a server this node was
//! running, because nothing said it was there. The agent's `[uplink]` is not
//! this — that is the control destination an *index* drives the node through,
//! and it carries no game traffic.
//!
//! # One bridge per instance, and why
//!
//! Each instance gets its own [`BridgeSession`] in server role, with its own
//! Reticulum identity kept in the instance's directory. That follows from the
//! §3.3 announce record being **per server**: name, map, player counts and the
//! game id all describe one server, and a destination is derived from an
//! identity. Multiplexing several games onto one identity would mean one
//! announce trying to describe all of them.
//!
//! The cost is one node per instance rather than one per host. That is real,
//! and it is bounded by `max_instances`, which an operator already sets with
//! their hardware in mind.
//!
//! # The rules
//!
//! * **An instance's identity is stable across restarts.** It lives beside the
//!   instance's own state, so a server that goes down and comes back keeps the
//!   destination players bookmarked. Regenerating it would silently orphan
//!   every reference to that server.
//! * **The bridge points at the published host port, not the container port.**
//!   The game binds its pack's number inside its own namespace; what the bridge
//!   can reach is what the node published.
//! * **A bridge that will not start does not stop the instance.** A game server
//!   reachable on the LAN and not the mesh is degraded; one that refused to run
//!   because Reticulum was misconfigured is broken. The failure is logged and
//!   surfaced, and the container keeps running.
//! * **No mesh config means no bridges, silently.** A node with no `[mesh]`
//!   section is the LAN-only behaviour that came before, which has to keep
//!   working — `DESIGN.md` §0's baseline is two launchers on a shared
//!   interface, not a node that insists on being configured.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{anyhow, Result};
use game_bridge::config::{AnnounceFormat, ServerArgs};
use game_bridge::profile::GameProfile;
use game_bridge::BridgeSession;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

/// How a node reaches the mesh on behalf of the games it runs.
///
/// Separate from `[uplink]` on purpose: an operator may want their node
/// driveable by an index without hosting anything on the mesh, or the reverse.
/// They are different jobs and one switch for both would force a choice nobody
/// asked for.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MeshConfig {
    /// Attach a TCP interface: `0.0.0.0:4242` to bind one, `host:port` to dial
    /// a peer. With neither this nor `auto`, a bridge starts and reaches
    /// nothing — which is a real configuration, just an isolated one.
    #[serde(default)]
    pub tcp: Option<String>,
    /// Attach auto-discovered local interfaces.
    #[serde(default)]
    pub auto: bool,
    /// Seconds between announces. The default matches the bridge's own.
    #[serde(default = "default_announce_interval")]
    pub announce_interval: u64,
}

fn default_announce_interval() -> u64 {
    30
}

impl Default for MeshConfig {
    fn default() -> Self {
        Self { tcp: None, auto: false, announce_interval: default_announce_interval() }
    }
}

/// What a bridge is doing, for the API and the web UI.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct MeshStatus {
    pub instance_id: String,
    /// Hex destination a browser would join. `None` while starting.
    pub destination: Option<String>,
    pub game_id: String,
    pub name: String,
}

struct Bridge {
    session: BridgeSession,
    status: MeshStatus,
}

/// Every instance's mesh bridge, by instance id.
pub struct MeshBridges {
    config: Option<MeshConfig>,
    bridges: Mutex<BTreeMap<String, Bridge>>,
}

impl MeshBridges {
    pub fn new(config: Option<MeshConfig>) -> Arc<Self> {
        Arc::new(Self { config, bridges: Mutex::new(BTreeMap::new()) })
    }

    /// Whether this node puts its games on the mesh at all.
    pub fn enabled(&self) -> bool {
        self.config.is_some()
    }

    pub async fn status(&self) -> Vec<MeshStatus> {
        self.bridges.lock().await.values().map(|b| b.status.clone()).collect()
    }

    pub async fn status_of(&self, instance_id: &str) -> Option<MeshStatus> {
        self.bridges.lock().await.get(instance_id).map(|b| b.status.clone())
    }

    /// Announce this instance on the mesh and relay links to its game port.
    ///
    /// Idempotent: an instance that already has a bridge keeps the one it has,
    /// so a caller re-reconciling does not churn a destination players may be
    /// connected to.
    pub async fn start(
        &self,
        instance_id: &str,
        profile: &GameProfile,
        identity_path: &Path,
        game_addr: (&str, u16),
        name: &str,
        max_players: u8,
    ) -> Result<Option<MeshStatus>> {
        let Some(config) = &self.config else { return Ok(None) };

        let mut bridges = self.bridges.lock().await;
        if let Some(existing) = bridges.get(instance_id) {
            return Ok(Some(existing.status.clone()));
        }

        if let Some(parent) = identity_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let mut args = ServerArgs::new(profile.clone());
        args.game_host = game_addr.0.to_string();
        args.game_port = game_addr.1;
        args.identity = identity_path.to_path_buf();
        args.tcp = config.tcp.clone();
        args.auto = config.auto;
        args.announce_interval = config.announce_interval;
        args.name = Some(name.to_string());
        args.max_players = max_players;
        // A node's instances are dedicated servers by definition: nobody is
        // playing on the machine that runs them.
        args.dedicated = true;
        // The §3.3 record, never the legacy name-only shape. A hosted server is
        // new, so there is no deployed peer whose expectations it has to match —
        // and the record is what carries the game id a browser filters on.
        args.announce_format = AnnounceFormat::Record;

        let session = BridgeSession::start_server(args)
            .await
            .map_err(|e| anyhow!("starting the mesh bridge: {e}"))?;

        let status = MeshStatus {
            instance_id: instance_id.to_string(),
            destination: session.own_hash().map(|h| hex::encode(h.as_bytes())),
            game_id: profile.id.clone(),
            name: name.to_string(),
        };
        tracing::info!(
            instance = %instance_id,
            destination = ?status.destination,
            game_port = game_addr.1,
            "announcing this server on the mesh"
        );
        bridges.insert(instance_id.to_string(), Bridge { session, status: status.clone() });
        Ok(Some(status))
    }

    /// Take an instance off the mesh. Silent when it was never on it.
    pub async fn stop(&self, instance_id: &str) {
        let bridge = self.bridges.lock().await.remove(instance_id);
        if let Some(mut bridge) = bridge {
            tracing::info!(instance = %instance_id, "taking this server off the mesh");
            bridge.session.stop().await;
        }
    }

    /// Drop bridges for instances that are no longer running.
    ///
    /// Called after a listing, because a container can stop without the agent
    /// being the one that stopped it — somebody ran `docker stop`, or the game
    /// crashed. A bridge still announcing a server that is gone is worse than
    /// no bridge: it advertises a destination that accepts a link and then
    /// relays to a closed port.
    pub async fn retain_only(&self, live: &[String]) {
        let stale: Vec<String> = {
            let bridges = self.bridges.lock().await;
            bridges.keys().filter(|id| !live.contains(id)).cloned().collect()
        };
        for id in stale {
            self.stop(&id).await;
        }
    }
}

/// Where an instance's mesh identity lives: beside its own state, so it is
/// stable across restarts and goes away when the instance is removed.
pub fn identity_path(data_root: &Path, instance_id: &str) -> PathBuf {
    data_root.join("instances").join(instance_id).join("mesh.identity")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_node_with_no_mesh_config_starts_no_bridges() {
        let bridges = MeshBridges::new(None);
        assert!(!bridges.enabled());
        let out = bridges
            .start(
                "i1",
                &GameProfile::sven_coop(),
                Path::new("/nonexistent/mesh.identity"),
                ("127.0.0.1", 27015),
                "Test",
                16,
            )
            .await
            .expect("no mesh config is not an error");
        assert!(out.is_none(), "a node with no [mesh] must stay LAN-only");
        assert!(bridges.status().await.is_empty());
    }

    /// The identity sits beside the instance's own state so a restart keeps the
    /// destination players bookmarked, and removing the instance takes it with
    /// them.
    #[test]
    fn an_instances_identity_lives_with_its_instance() {
        let p = identity_path(Path::new("/var/lib/gpp"), "sven-1");
        assert_eq!(p, Path::new("/var/lib/gpp/instances/sven-1/mesh.identity"));
    }

    #[tokio::test]
    async fn stopping_an_instance_that_was_never_on_the_mesh_is_silent() {
        let bridges = MeshBridges::new(Some(MeshConfig::default()));
        bridges.stop("never-existed").await;
        assert!(bridges.status().await.is_empty());
    }

    #[tokio::test]
    async fn retain_only_is_a_no_op_when_everything_is_live() {
        let bridges = MeshBridges::new(Some(MeshConfig::default()));
        bridges.retain_only(&["a".to_string()]).await;
        assert!(bridges.status().await.is_empty());
    }

    /// The default is 30 seconds and `[mesh]` may be written empty — an
    /// operator who wants auto-discovery only should not have to name a number.
    #[test]
    fn an_empty_mesh_section_is_valid_and_defaults() {
        let cfg: MeshConfig = toml::from_str("").unwrap();
        assert_eq!(cfg.announce_interval, 30);
        assert!(!cfg.auto);
        assert!(cfg.tcp.is_none());
    }
}
