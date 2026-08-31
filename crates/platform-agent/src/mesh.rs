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

/// An interface every game bridge on this node attaches, added at runtime.
///
/// Separate from `MeshConfig.tcp`/`auto`, which are what the operator wrote in
/// the config file and every bridge gets at birth. These are the ones added
/// from the web UI afterwards: persisted, applied to bridges that already
/// exist, and given to bridges started later. An operator should not have to
/// edit TOML and restart to reach one more relay — that is the whole reason the
/// node has a UI.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum MeshInterface {
    Tcp {
        addr: String,
        #[serde(default)]
        ifac_name: Option<String>,
        #[serde(default)]
        ifac_passphrase: Option<String>,
    },
    Auto {
        #[serde(default)]
        ifac_name: Option<String>,
        #[serde(default)]
        ifac_passphrase: Option<String>,
    },
}

impl MeshInterface {
    /// What identifies this interface to a person and in the saved set. The
    /// IFAC secret is deliberately not part of it: re-adding the same address
    /// with a corrected passphrase should replace the entry, not stack a second
    /// one beside it.
    pub fn id(&self) -> String {
        match self {
            Self::Tcp { addr, .. } => format!("tcp:{addr}"),
            Self::Auto { .. } => "auto".to_string(),
        }
    }

    /// The half of this that is safe to hand back over the API. The IFAC
    /// passphrase is a shared secret; it goes in and is never returned, the
    /// same rule `interfaces.rs` follows for the uplink's.
    pub fn public(&self) -> serde_json::Value {
        match self {
            Self::Tcp { addr, ifac_name, .. } => serde_json::json!({
                "id": self.id(), "kind": "tcp", "addr": addr,
                "ifac_name": ifac_name, "ifac": ifac_name.is_some(),
            }),
            Self::Auto { ifac_name, .. } => serde_json::json!({
                "id": self.id(), "kind": "auto", "addr": null,
                "ifac_name": ifac_name, "ifac": ifac_name.is_some(),
            }),
        }
    }

    async fn attach_to(&self, session: &BridgeSession) {
        let handle = session.handle();
        match self {
            Self::Tcp { addr, ifac_name, ifac_passphrase } => {
                // The id it captures is not needed here: this set is keyed by
                // address, and the engine cannot detach a live interface
                // anyway.
                let _ = crate::interfaces::attach_tcp(
                    handle,
                    addr,
                    ifac_name.as_deref(),
                    ifac_passphrase.as_deref(),
                )
                .await;
            }
            Self::Auto { ifac_name, ifac_passphrase } => {
                let _ = crate::interfaces::attach_auto(
                    handle,
                    ifac_name.as_deref(),
                    ifac_passphrase.as_deref(),
                );
            }
        }
    }
}

/// Every instance's mesh bridge, by instance id.
pub struct MeshBridges {
    config: Option<MeshConfig>,
    bridges: Mutex<BTreeMap<String, Bridge>>,
    /// Interfaces added at runtime, applied to every bridge and persisted so a
    /// restart does not quietly take the node off a relay it was using.
    extra: Mutex<Vec<MeshInterface>>,
    extra_path: Option<PathBuf>,
}

impl MeshBridges {
    pub fn new(config: Option<MeshConfig>) -> Arc<Self> {
        Arc::new(Self {
            config,
            bridges: Mutex::new(BTreeMap::new()),
            extra: Mutex::new(Vec::new()),
            extra_path: None,
        })
    }

    /// Same, but remembering runtime interfaces in a file beside the node's
    /// other state.
    pub fn with_store(config: Option<MeshConfig>, path: PathBuf) -> Arc<Self> {
        let extra = std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str::<Vec<MeshInterface>>(&s).ok())
            .unwrap_or_default();
        Arc::new(Self {
            config,
            bridges: Mutex::new(BTreeMap::new()),
            extra: Mutex::new(extra),
            extra_path: Some(path),
        })
    }

    /// The runtime interfaces, without their IFAC secrets.
    pub async fn interfaces(&self) -> Vec<serde_json::Value> {
        self.extra.lock().await.iter().map(|i| i.public()).collect()
    }

    /// Attach an interface to every bridge now and to every bridge later.
    ///
    /// Re-adding the same id replaces the saved entry rather than stacking a
    /// duplicate, so correcting a passphrase is one action instead of a remove
    /// and an add.
    pub async fn add_interface(&self, iface: MeshInterface) -> Result<()> {
        if !self.enabled() {
            return Err(anyhow!(
                "this node runs its games LAN-only, so there is nothing to attach an \
                 interface to. Add a [mesh] section to its config and restart"
            ));
        }
        {
            let bridges = self.bridges.lock().await;
            for bridge in bridges.values() {
                iface.attach_to(&bridge.session).await;
            }
        }
        let mut extra = self.extra.lock().await;
        let id = iface.id();
        extra.retain(|i| i.id() != id);
        extra.push(iface);
        self.save(&extra)?;
        Ok(())
    }

    /// Forget a runtime interface.
    ///
    /// **Forgetting is not detaching.** The engine offers no way to remove an
    /// interface from a running node, so a bridge that already has this one
    /// keeps it until it restarts. Saying so is better than pretending: an
    /// operator who removes a relay and watches traffic keep flowing would
    /// otherwise conclude the button does nothing.
    pub async fn remove_interface(&self, id: &str) -> Result<bool> {
        let mut extra = self.extra.lock().await;
        let before = extra.len();
        extra.retain(|i| i.id() != id);
        let removed = extra.len() != before;
        if removed {
            self.save(&extra)?;
        }
        Ok(removed)
    }

    fn save(&self, extra: &[MeshInterface]) -> Result<()> {
        let Some(path) = &self.extra_path else { return Ok(()) };
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // Written beside and renamed, so an interrupted save cannot leave a
        // truncated file that reads as "no interfaces" on the next start.
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_string_pretty(extra)?)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
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

        // Everything added since startup, so a bridge born now reaches the same
        // relays as the ones already running.
        for iface in self.extra.lock().await.iter() {
            iface.attach_to(&session).await;
        }

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

    /// A LAN-only node has nothing to attach an interface to, and says so
    /// rather than accepting the setting and silently doing nothing.
    #[tokio::test]
    async fn adding_an_interface_to_a_lan_only_node_is_refused() {
        let bridges = MeshBridges::new(None);
        let err = bridges
            .add_interface(MeshInterface::Auto { ifac_name: None, ifac_passphrase: None })
            .await
            .expect_err("LAN-only has nothing to attach to");
        assert!(format!("{err}").contains("[mesh]"), "{err}");
    }

    /// Re-adding the same address replaces the entry rather than stacking a
    /// duplicate, so fixing a passphrase is one action.
    #[tokio::test]
    async fn re_adding_an_address_replaces_it() {
        let dir = tempfile::tempdir().unwrap();
        let bridges =
            MeshBridges::with_store(Some(MeshConfig::default()), dir.path().join("i.json"));
        bridges
            .add_interface(MeshInterface::Tcp {
                addr: "hub:4789".into(),
                ifac_name: None,
                ifac_passphrase: None,
            })
            .await
            .unwrap();
        bridges
            .add_interface(MeshInterface::Tcp {
                addr: "hub:4789".into(),
                ifac_name: Some("net".into()),
                ifac_passphrase: Some("secret".into()),
            })
            .await
            .unwrap();
        let list = bridges.interfaces().await;
        assert_eq!(list.len(), 1, "the same address must not stack");
        assert_eq!(list[0]["ifac_name"], "net");
    }

    /// **The IFAC passphrase is a shared secret and never comes back out.**
    /// Same rule the uplink's interfaces follow.
    #[tokio::test]
    async fn an_ifac_passphrase_is_never_returned() {
        let dir = tempfile::tempdir().unwrap();
        let bridges =
            MeshBridges::with_store(Some(MeshConfig::default()), dir.path().join("i.json"));
        bridges
            .add_interface(MeshInterface::Tcp {
                addr: "hub:4789".into(),
                ifac_name: Some("net".into()),
                ifac_passphrase: Some("hunter2".into()),
            })
            .await
            .unwrap();
        let rendered = serde_json::to_string(&bridges.interfaces().await).unwrap();
        assert!(!rendered.contains("hunter2"), "the passphrase leaked: {rendered}");
        // But the fact that one is set is worth showing.
        assert!(rendered.contains("\"ifac\":true"), "{rendered}");
    }

    /// Interfaces added from the UI survive a restart, or an operator would
    /// silently drop off a relay every time the node came back.
    #[tokio::test]
    async fn interfaces_are_remembered_across_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("i.json");
        {
            let bridges = MeshBridges::with_store(Some(MeshConfig::default()), path.clone());
            bridges
                .add_interface(MeshInterface::Tcp {
                    addr: "hub:4789".into(),
                    ifac_name: None,
                    ifac_passphrase: None,
                })
                .await
                .unwrap();
        }
        let reopened = MeshBridges::with_store(Some(MeshConfig::default()), path);
        assert_eq!(reopened.interfaces().await.len(), 1);
        assert!(reopened.remove_interface("tcp:hub:4789").await.unwrap());
        assert!(reopened.interfaces().await.is_empty());
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
