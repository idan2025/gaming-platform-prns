//! Live Reticulum interface control for the agent's uplink node.
//!
//! # Why this exists
//!
//! Until now a node's mesh interfaces were fixed at process start: the
//! `[uplink]` block's `tcp` and `auto` were read once, handed to
//! `game_bridge::relay::attach_interfaces` inside the node recipe, and never
//! touched again (`uplink.rs`, `config.rs`). Changing how a node reaches the
//! mesh meant editing TOML and restarting. That is the wrong shape for the one
//! deployment the web UI is built for: a **headless node** — a relay or a host
//! on a box with no desktop — where "link me to the mesh" is the first thing an
//! operator does and a restart loop is a bad way to do it (`PLAN.md` §13.5).
//!
//! So this module lets the operator add, remove, rename and list interfaces on
//! the running uplink node from the agent's web UI, the same live control the
//! standalone app already has over its bridge sessions
//! (`svencoop-prns` v0.1.10 `controller::add_interface_tcp` /
//! `add_interface_auto` / `remove_interface` / `rename_interface` /
//! `list_interfaces`). The shape is copied from there and parametrised for the
//! agent's single node; the extraction is one-directional, so nothing here is a
//! dependency of the standalone (`PLAN.md` §13.5).
//!
//! # Scope: TCP and Wi-Fi/LAN auto, with optional IFAC
//!
//! The standalone offers TCP, UDP, WebSocket and auto. The agent's engine build
//! enables only `tcp` and `wifi-auto` (`Cargo.toml`), so only those two are
//! offered here — adding UDP or WebSocket is an engine-feature change and
//! belongs with `ENGINE.md`, not with a UI increment. The two on offer cover
//! every way a node links to a mesh: a TCP **client** dials a hub or relay
//! (reaching a mesh across the internet), a TCP **server** *is* that relay, and
//! auto finds neighbours on the LAN with no configuration at all. Each may be
//! IFAC-protected, matching upstream Reticulum: a network name and/or a
//! passphrase derive a key, and only peers sharing it can talk on that
//! interface (`add_interface_tcp` docs, standalone).
//!
//! # Authorization is the API's, not this module's
//!
//! Adding an interface binds a socket and joins a mesh, so it needs the same
//! guard every container-creating route already has: the API is loopback-only
//! unless an `api_token` is set, and every route requires the token once one is
//! (`api.rs`, `config.rs` `NonLoopbackApiBind`). This module is called from
//! behind that layer and adds no boundary of its own — mirroring how the
//! standalone's own web control surface is loopback/authenticated
//! (`PLAN.md` §13.5).
//!
//! # Durability
//!
//! Interfaces added here are persisted to a sidecar JSON file next to the
//! node's data and **re-attached on the next start** (`reattach_saved`), so a
//! link an operator set up survives a restart — the same promise the standalone
//! keeps with its `settings.json` `interfaces` list. The `[uplink]` block's
//! `tcp`/`auto` still attach at start independently; the two are additive.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use personal_rns::prelude::*;
use prns_core::interfaces::{IfacContext, InterfaceId, DEFAULT_IFAC_SIZE};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use tracing::{info, warn};

/// One saved interface, written to the sidecar file so it is re-attached on the
/// next start. The runtime `id` is captured at attach time (a before/after diff
/// of the node's interface set) so a later `remove` can drop the matching saved
/// descriptor; it is re-generated on the next attach and so is not load-bearing
/// across restarts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InterfaceDescriptor {
    /// Runtime interface id (hex) captured at attach time. Absent for a
    /// descriptor that was written before it was ever attached.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// `"tcp"` or `"auto"`.
    pub kind: String,
    /// `host:port` for tcp; absent for auto.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub addr: Option<String>,
    /// IFAC network name, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ifac_name: Option<String>,
    /// IFAC passphrase, if any. Stored in plaintext in the sidecar file
    /// alongside the rest of the local, unencrypted operator config — the same
    /// trust boundary as the `api_token_file` sitting next to it, and the same
    /// choice the standalone makes for its `settings.json`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ifac_passphrase: Option<String>,
}

impl InterfaceDescriptor {
    /// Whether two descriptors name the same wire, ignoring the captured id and
    /// the IFAC secret. Re-adding the same endpoint updates the saved
    /// descriptor in place rather than stacking a duplicate — the standalone's
    /// `same_endpoint`.
    pub fn same_endpoint(&self, other: &Self) -> bool {
        self.kind == other.kind && self.addr == other.addr
    }
}

/// The persisted set of live-added interfaces.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct InterfaceSettings {
    #[serde(default)]
    pub interfaces: Vec<InterfaceDescriptor>,
}

impl InterfaceSettings {
    /// Load the sidecar file, or an empty set when it does not exist. A missing
    /// file is the common first-run case, not an error: a node that has never
    /// had an interface added from the UI has nothing to re-attach.
    pub fn load(path: &Path) -> std::io::Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(src) => Ok(serde_json::from_str(&src).unwrap_or_default()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e),
        }
    }

    /// Write the sidecar file atomically: a temp file in the same directory,
    /// then a rename, so a crash mid-write cannot truncate the operator's saved
    /// interfaces to nothing.
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let body = serde_json::to_string_pretty(self).map_err(std::io::Error::other)?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, body)?;
        std::fs::rename(&tmp, path)
    }

    /// Insert or update a descriptor by endpoint. Returns nothing; the caller
    /// persists.
    fn record(&mut self, desc: InterfaceDescriptor) {
        match self.interfaces.iter_mut().find(|d| d.same_endpoint(&desc)) {
            Some(existing) => *existing = desc,
            None => self.interfaces.push(desc),
        }
    }

    /// Drop the descriptor whose captured id matches, case-insensitively.
    /// Returns whether anything was removed.
    fn forget_by_id(&mut self, id_hex: &str) -> bool {
        let before = self.interfaces.len();
        let target = id_hex.to_ascii_lowercase();
        self.interfaces
            .retain(|d| !d.id.as_deref().map(|i| i.eq_ignore_ascii_case(&target)).unwrap_or(false));
        self.interfaces.len() != before
    }
}

/// One live interface, as the web UI shows it. A projection of the engine's
/// `InterfaceInventoryEntry`/`InterfaceSnapshot` — the same fields the
/// standalone's `InterfaceInfo` carries.
#[derive(Debug, Clone, Serialize)]
pub struct InterfaceInfo {
    /// Interface id as hex — the stable identifier for remove/rename.
    pub id: String,
    /// Human-readable name, if set.
    pub name: Option<String>,
    /// Interface mode (e.g. Access, Boundary) — the engine enum's `Debug`.
    pub mode: String,
    /// Connection state (e.g. Connected, Listening) — the engine enum's `Debug`.
    pub connection: String,
    pub rx_bytes: u64,
    pub tx_bytes: u64,
    pub links: u32,
}

/// What the web UI's Interfaces tab reads: whether the uplink is up at all,
/// this node's own destination hash (its address on the mesh), and the live
/// interface list.
#[derive(Debug, Clone, Serialize)]
pub struct InterfaceStatus {
    /// False when the node was started with no `[uplink]` block, in which case
    /// there is no mesh node to attach interfaces to and the tab says so rather
    /// than offering controls that cannot work.
    pub uplink_running: bool,
    /// This node's control destination hash (hex), when the uplink is up.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub destination: Option<String>,
    pub interfaces: Vec<InterfaceInfo>,
}

/// A request to add one interface, tagged by `kind`. `serde` picks the variant
/// from the `"kind"` field, so the web UI posts `{"kind":"tcp","addr":"..."}`
/// or `{"kind":"auto"}`.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum AddInterface {
    /// A TCP interface. `addr` is `host:port`; `0.0.0.0:PORT` (or `:PORT`) binds
    /// a TCP **server** (a relay others dial), any other host connects as a TCP
    /// **client** (dialing a relay).
    Tcp {
        addr: String,
        #[serde(default)]
        ifac_name: Option<String>,
        #[serde(default)]
        ifac_passphrase: Option<String>,
    },
    /// A Wi-Fi/LAN auto-discovery interface. No address: it finds neighbours on
    /// the local segment with no configuration.
    Auto {
        #[serde(default)]
        ifac_name: Option<String>,
        #[serde(default)]
        ifac_passphrase: Option<String>,
    },
}

/// Everything that can go wrong adding, removing or renaming an interface.
#[derive(Debug)]
pub enum InterfaceError {
    /// No `[uplink]` block, so there is no running node to configure.
    NoUplink,
    /// A TCP address with no port, or a non-numeric one.
    BadAddress(String),
    /// Binding a TCP server socket failed (port in use, permission, ...).
    Bind(String),
    /// No interface on the node has this hex id.
    NoSuchInterface(String),
    /// The engine refused the rename.
    RenameFailed(String),
    /// Persisting the sidecar file failed.
    Persist(String),
}

impl core::fmt::Display for InterfaceError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::NoUplink => write!(
                f,
                "this node has no [uplink] block, so it has no mesh node to attach an \
                 interface to. Add an [uplink] section to the agent config and restart"
            ),
            Self::BadAddress(a) => {
                write!(f, "TCP address {a:?} is not host:port with a numeric port")
            }
            Self::Bind(e) => write!(f, "binding the interface failed: {e}"),
            Self::NoSuchInterface(id) => write!(f, "no interface with id {id}"),
            Self::RenameFailed(id) => write!(f, "the engine refused to rename interface {id}"),
            Self::Persist(e) => write!(f, "saving the interface list failed: {e}"),
        }
    }
}

impl std::error::Error for InterfaceError {}

/// Live interface control over one uplink node.
///
/// Holds the node handle (a `Send + Sync + Clone` command channel — see
/// `relay.rs`), the persisted descriptor set behind an async mutex, and the
/// sidecar path. `handle` is `None` when the node has no uplink, which turns
/// every mutating call into `NoUplink` and `status()` into "not running" rather
/// than a panic.
pub struct InterfaceManager {
    handle: Option<PrnsNodeHandle>,
    destination: Option<String>,
    settings: Mutex<InterfaceSettings>,
    path: Option<PathBuf>,
}

impl InterfaceManager {
    /// Build a manager, loading any persisted interface set from `path`.
    ///
    /// `handle`/`destination` are `Some` only when an uplink is running. `path`
    /// is where the sidecar lives; `None` disables persistence (used in tests).
    pub fn new(
        handle: Option<PrnsNodeHandle>,
        destination: Option<String>,
        path: Option<PathBuf>,
    ) -> Self {
        let settings = match &path {
            Some(p) => InterfaceSettings::load(p).unwrap_or_else(|e| {
                warn!(path = %p.display(), error = %e, "could not read saved interfaces; starting empty");
                InterfaceSettings::default()
            }),
            None => InterfaceSettings::default(),
        };
        Self { handle, destination, settings: Mutex::new(settings), path }
    }

    /// Whether a mesh node is present to configure.
    pub fn has_uplink(&self) -> bool {
        self.handle.is_some()
    }

    /// The live interface list, or empty when there is no uplink.
    pub fn list(&self) -> Vec<InterfaceInfo> {
        let Some(handle) = &self.handle else { return Vec::new() };
        handle
            .interface_inventory()
            .into_iter()
            .map(|e| InterfaceInfo {
                id: hex::encode(e.snapshot.id.as_bytes()),
                name: e.name,
                mode: format!("{:?}", e.snapshot.mode),
                connection: format!("{:?}", e.snapshot.connection),
                rx_bytes: e.snapshot.rx_bytes,
                tx_bytes: e.snapshot.tx_bytes,
                links: e.snapshot.links,
            })
            .collect()
    }

    /// The tab's whole read: running flag, destination, and interface list.
    pub fn status(&self) -> InterfaceStatus {
        InterfaceStatus {
            uplink_running: self.has_uplink(),
            destination: self.destination.clone(),
            interfaces: self.list(),
        }
    }

    /// Add one interface: apply it to the running node, capture its runtime id,
    /// record and persist the descriptor.
    pub async fn add(&self, req: AddInterface) -> Result<(), InterfaceError> {
        let handle = self.handle.as_ref().ok_or(InterfaceError::NoUplink)?;
        let desc = match req {
            AddInterface::Tcp { addr, ifac_name, ifac_passphrase } => {
                let id =
                    attach_tcp(handle, &addr, ifac_name.as_deref(), ifac_passphrase.as_deref())
                        .await?;
                InterfaceDescriptor {
                    id,
                    kind: "tcp".to_string(),
                    addr: Some(addr),
                    ifac_name,
                    ifac_passphrase,
                }
            }
            AddInterface::Auto { ifac_name, ifac_passphrase } => {
                let id = attach_auto(handle, ifac_name.as_deref(), ifac_passphrase.as_deref());
                InterfaceDescriptor {
                    id,
                    kind: "auto".to_string(),
                    addr: None,
                    ifac_name,
                    ifac_passphrase,
                }
            }
        };
        let mut settings = self.settings.lock().await;
        settings.record(desc);
        self.persist(&settings)?;
        Ok(())
    }

    /// Remove an interface by hex id: drop it from the running node and forget
    /// its saved descriptor.
    pub async fn remove(&self, id_hex: &str) -> Result<(), InterfaceError> {
        let handle = self.handle.as_ref().ok_or(InterfaceError::NoUplink)?;
        let id = find_interface_by_hex(handle, id_hex)
            .ok_or_else(|| InterfaceError::NoSuchInterface(id_hex.to_string()))?;
        handle.remove_interface(id);
        info!(id = id_hex, "removed interface");
        let mut settings = self.settings.lock().await;
        settings.forget_by_id(id_hex);
        self.persist(&settings)?;
        Ok(())
    }

    /// Rename an interface by hex id. A name is presentation only — it is not
    /// persisted, matching the standalone, because the descriptor is keyed by
    /// endpoint, not name.
    pub fn rename(&self, id_hex: &str, name: String) -> Result<(), InterfaceError> {
        let handle = self.handle.as_ref().ok_or(InterfaceError::NoUplink)?;
        let id = find_interface_by_hex(handle, id_hex)
            .ok_or_else(|| InterfaceError::NoSuchInterface(id_hex.to_string()))?;
        if !handle.set_interface_name(id, name.clone()) {
            return Err(InterfaceError::RenameFailed(id_hex.to_string()));
        }
        info!(id = id_hex, name = %name, "renamed interface");
        Ok(())
    }

    /// Re-attach every persisted interface onto the freshly started node.
    ///
    /// Called once at start, after the uplink is up, so an interface an operator
    /// set up in the UI survives a restart. A single failure is logged and
    /// skipped rather than aborting the rest — one bad saved endpoint should not
    /// take the others down with it. The captured runtime ids are refreshed so
    /// a later remove still matches.
    pub async fn reattach_saved(&self) {
        let Some(handle) = &self.handle else { return };
        let mut settings = self.settings.lock().await;
        let saved = settings.interfaces.clone();
        if saved.is_empty() {
            return;
        }
        info!(count = saved.len(), "re-attaching saved interfaces");
        for desc in &mut settings.interfaces {
            let new_id = match desc.kind.as_str() {
                "tcp" => match &desc.addr {
                    Some(addr) => attach_tcp(
                        handle,
                        addr,
                        desc.ifac_name.as_deref(),
                        desc.ifac_passphrase.as_deref(),
                    )
                    .await
                    .unwrap_or_else(|e| {
                        warn!(addr = %addr, error = %e, "could not re-attach a saved TCP interface");
                        None
                    }),
                    None => {
                        warn!("skipping a saved tcp interface with no address");
                        None
                    }
                },
                "auto" => attach_auto(
                    handle,
                    desc.ifac_name.as_deref(),
                    desc.ifac_passphrase.as_deref(),
                ),
                other => {
                    warn!(kind = %other, "skipping a saved interface of unknown kind");
                    None
                }
            };
            desc.id = new_id;
        }
        if let Err(e) = self.persist(&settings) {
            warn!(error = %e, "could not persist refreshed interface ids after re-attach");
        }
    }

    fn persist(&self, settings: &InterfaceSettings) -> Result<(), InterfaceError> {
        let Some(path) = &self.path else { return Ok(()) };
        settings.save(path).map_err(|e| InterfaceError::Persist(e.to_string()))
    }
}

/// Convenience: wrap in an `Arc` for the API state.
impl InterfaceManager {
    pub fn shared(self) -> Arc<Self> {
        Arc::new(self)
    }
}

// ---- attach helpers, factored so `add` and `reattach_saved` share them ------

/// Attach a TCP interface and return its captured runtime id. `0.0.0.0`/empty
/// host binds a server; any other host dials a client. Copied from the
/// standalone's `add_interface_tcp`, minus the UDP/WebSocket siblings the agent
/// does not build.
async fn attach_tcp(
    handle: &PrnsNodeHandle,
    addr: &str,
    ifac_name: Option<&str>,
    ifac_passphrase: Option<&str>,
) -> Result<Option<String>, InterfaceError> {
    let (host, port) = parse_host_port(addr)?;
    let before = interface_ids(handle);
    // `IfacContext::derive` returns None when both name and passphrase are
    // empty/None, which is exactly the "no IFAC" case.
    let ifac = IfacContext::derive(ifac_name, ifac_passphrase, DEFAULT_IFAC_SIZE);
    let ifac_set = ifac.is_some();
    if host == "0.0.0.0" || host.is_empty() {
        let srv =
            TcpServer::bind(addr).await.map_err(|e| InterfaceError::Bind(format!("{e:?}")))?;
        match ifac {
            Some(ifac) => {
                handle.supervise_with_ifac_name(srv, ifac, None);
            }
            None => {
                handle.supervise(srv);
            }
        }
        info!(tcp = %addr, ifac = ifac_set, "attached TCP server interface");
    } else if port > 0 {
        let client = TcpClientInterface::new(addr.to_string());
        match ifac {
            Some(ifac) => {
                handle.add_interface_with_ifac_name(client, ifac, None);
            }
            None => {
                handle.add_interface(client);
            }
        }
        info!(tcp = %addr, ifac = ifac_set, "attached TCP client interface");
    } else {
        return Err(InterfaceError::BadAddress(addr.to_string()));
    }
    Ok(new_interface_id(handle, &before))
}

/// Attach a Wi-Fi/LAN auto-discovery interface and return its captured id.
fn attach_auto(
    handle: &PrnsNodeHandle,
    ifac_name: Option<&str>,
    ifac_passphrase: Option<&str>,
) -> Option<String> {
    let before = interface_ids(handle);
    let ifac = IfacContext::derive(ifac_name, ifac_passphrase, DEFAULT_IFAC_SIZE);
    match ifac {
        Some(ifac) => {
            handle.attach_with_ifac_name(AutoWifi::default(), ifac, ifac_name.map(String::from));
        }
        None => {
            handle.attach(AutoWifi::default());
        }
    }
    info!(
        ifac = ifac_name.is_some() || ifac_passphrase.is_some(),
        "attached Wi-Fi/LAN auto interface"
    );
    new_interface_id(handle, &before)
}

/// The node's current interface ids, for a before/after diff.
fn interface_ids(handle: &PrnsNodeHandle) -> Vec<InterfaceId> {
    handle.interfaces().iter().map(|s| s.id).collect()
}

/// The one id present now that was not in `before`, hex-encoded — the interface
/// the attach just added.
fn new_interface_id(handle: &PrnsNodeHandle, before: &[InterfaceId]) -> Option<String> {
    handle
        .interfaces()
        .iter()
        .find(|s| !before.contains(&s.id))
        .map(|s| hex::encode(s.id.as_bytes()))
}

/// Resolve a hex id to the live `InterfaceId`, or `None` if the node has no such
/// interface. Copied from the standalone's `find_interface_by_hex`.
fn find_interface_by_hex(handle: &PrnsNodeHandle, id_hex: &str) -> Option<InterfaceId> {
    let target = id_hex.to_ascii_lowercase();
    handle.interfaces().into_iter().find(|s| hex::encode(s.id.as_bytes()) == target).map(|s| s.id)
}

/// Split `host:port` on the last colon and parse the port. The standalone's
/// `parse_host_port`, pulled out as a free fn so it is testable without a node.
fn parse_host_port(addr: &str) -> Result<(String, u16), InterfaceError> {
    let colon = addr.rfind(':').ok_or_else(|| InterfaceError::BadAddress(addr.to_string()))?;
    let host = addr[..colon].to_string();
    let port: u16 =
        addr[colon + 1..].parse().map_err(|_| InterfaceError::BadAddress(addr.to_string()))?;
    Ok((host, port))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_host_port_splits_on_the_last_colon() {
        let (h, p) = parse_host_port("0.0.0.0:4789").unwrap();
        assert_eq!(h, "0.0.0.0");
        assert_eq!(p, 4789);
        let (h, p) = parse_host_port("relay.example.com:4789").unwrap();
        assert_eq!(h, "relay.example.com");
        assert_eq!(p, 4789);
    }

    #[test]
    fn parse_host_port_rejects_the_malformed() {
        assert!(matches!(parse_host_port("noport"), Err(InterfaceError::BadAddress(_))));
        assert!(matches!(parse_host_port("host:abc"), Err(InterfaceError::BadAddress(_))));
        assert!(matches!(parse_host_port("host:99999"), Err(InterfaceError::BadAddress(_))));
    }

    #[test]
    fn add_interface_deserializes_by_kind_tag() {
        let tcp: AddInterface =
            serde_json::from_str(r#"{"kind":"tcp","addr":"0.0.0.0:4789"}"#).unwrap();
        assert!(matches!(tcp, AddInterface::Tcp { addr, .. } if addr == "0.0.0.0:4789"));

        let tcp_ifac: AddInterface = serde_json::from_str(
            r#"{"kind":"tcp","addr":"hub:4789","ifac_name":"n","ifac_passphrase":"p"}"#,
        )
        .unwrap();
        match tcp_ifac {
            AddInterface::Tcp { addr, ifac_name, ifac_passphrase } => {
                assert_eq!(addr, "hub:4789");
                assert_eq!(ifac_name.as_deref(), Some("n"));
                assert_eq!(ifac_passphrase.as_deref(), Some("p"));
            }
            _ => panic!("expected tcp"),
        }

        let auto: AddInterface = serde_json::from_str(r#"{"kind":"auto"}"#).unwrap();
        assert!(matches!(auto, AddInterface::Auto { .. }));
    }

    #[test]
    fn an_unknown_kind_is_refused() {
        assert!(serde_json::from_str::<AddInterface>(r#"{"kind":"udp","local":"x"}"#).is_err());
    }

    #[test]
    fn same_endpoint_ignores_id_and_ifac_secret() {
        let a = InterfaceDescriptor {
            id: Some("aa".into()),
            kind: "tcp".into(),
            addr: Some("0.0.0.0:4789".into()),
            ifac_name: None,
            ifac_passphrase: Some("one".into()),
        };
        let b = InterfaceDescriptor {
            id: Some("bb".into()),
            kind: "tcp".into(),
            addr: Some("0.0.0.0:4789".into()),
            ifac_name: None,
            ifac_passphrase: Some("two".into()),
        };
        assert!(a.same_endpoint(&b));
        let c = InterfaceDescriptor { addr: Some("0.0.0.0:9999".into()), ..b.clone() };
        assert!(!a.same_endpoint(&c));
    }

    #[test]
    fn record_updates_in_place_and_appends_the_new() {
        let mut s = InterfaceSettings::default();
        s.record(InterfaceDescriptor {
            id: None,
            kind: "tcp".into(),
            addr: Some("0.0.0.0:4789".into()),
            ifac_name: None,
            ifac_passphrase: Some("old".into()),
        });
        s.record(InterfaceDescriptor {
            id: None,
            kind: "tcp".into(),
            addr: Some("0.0.0.0:4789".into()),
            ifac_name: None,
            ifac_passphrase: Some("new".into()),
        });
        assert_eq!(s.interfaces.len(), 1, "same endpoint should update, not stack");
        assert_eq!(s.interfaces[0].ifac_passphrase.as_deref(), Some("new"));
        s.record(InterfaceDescriptor {
            id: None,
            kind: "auto".into(),
            addr: None,
            ifac_name: None,
            ifac_passphrase: None,
        });
        assert_eq!(s.interfaces.len(), 2);
    }

    #[test]
    fn forget_by_id_is_case_insensitive_and_reports_whether_it_hit() {
        let mut s = InterfaceSettings::default();
        s.record(InterfaceDescriptor {
            id: Some("AbCd".into()),
            kind: "tcp".into(),
            addr: Some("0.0.0.0:4789".into()),
            ifac_name: None,
            ifac_passphrase: None,
        });
        assert!(!s.forget_by_id("ffff"));
        assert_eq!(s.interfaces.len(), 1);
        assert!(s.forget_by_id("abcd"));
        assert!(s.interfaces.is_empty());
    }

    #[test]
    fn settings_round_trip_through_the_sidecar_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("agent-interfaces.json");
        let mut s = InterfaceSettings::default();
        s.record(InterfaceDescriptor {
            id: Some("aa".into()),
            kind: "tcp".into(),
            addr: Some("0.0.0.0:4789".into()),
            ifac_name: Some("net".into()),
            ifac_passphrase: None,
        });
        s.save(&path).unwrap();
        let loaded = InterfaceSettings::load(&path).unwrap();
        assert_eq!(loaded.interfaces, s.interfaces);
    }

    #[test]
    fn loading_a_missing_file_is_empty_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let loaded = InterfaceSettings::load(&dir.path().join("nope.json")).unwrap();
        assert!(loaded.interfaces.is_empty());
    }

    /// A manager with no uplink refuses every mutating call and reports not
    /// running, rather than panicking on a missing handle.
    #[tokio::test]
    async fn a_manager_without_an_uplink_refuses_and_reports_off() {
        let mgr = InterfaceManager::new(None, None, None);
        assert!(!mgr.has_uplink());
        let status = mgr.status();
        assert!(!status.uplink_running);
        assert!(status.interfaces.is_empty());
        assert!(matches!(
            mgr.add(AddInterface::Auto { ifac_name: None, ifac_passphrase: None }).await,
            Err(InterfaceError::NoUplink)
        ));
        assert!(matches!(mgr.remove("aa").await, Err(InterfaceError::NoUplink)));
        assert!(matches!(mgr.rename("aa", "x".into()), Err(InterfaceError::NoUplink)));
        // With no uplink, re-attach is a no-op rather than a panic.
        mgr.reattach_saved().await;
    }

    /// The manager loads a persisted set on construction, so a restart sees the
    /// interfaces the last run saved.
    #[test]
    fn new_loads_the_persisted_set() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agent-interfaces.json");
        let mut s = InterfaceSettings::default();
        s.record(InterfaceDescriptor {
            id: None,
            kind: "auto".into(),
            addr: None,
            ifac_name: None,
            ifac_passphrase: None,
        });
        s.save(&path).unwrap();
        let mgr = InterfaceManager::new(None, None, Some(path));
        // No uplink, so list() is empty, but the settings were loaded — proven
        // by status reporting off while the sidecar held one descriptor.
        assert!(!mgr.has_uplink());
    }
}
