//! What the node's operator decides, as opposed to what a game pack describes.
//!
//! # The split, and why it is where it is
//!
//! A game pack is data a user may have dropped into a directory. It says what a
//! game *is*: its id, its port, whether it answers a query, which paths it
//! writes to. It deliberately cannot say what **runs** — no command, no argv,
//! and no container image, because an image name selects the code a node
//! executes and is therefore argv with extra steps
//! (`crates/game-bridge/src/pack.rs`, `PLAN.md` §10).
//!
//! So the image, the resource limits, the port range and the data root live
//! here, in a file the node's operator writes. The agent will refuse to run a
//! game it has no entry for, rather than inventing a default image — "I do not
//! know what to run for this game" is the correct answer to an unrecognised
//! pack, and it is what keeps a hostile pack from becoming a hostile container.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Prefix on every container this agent creates.
///
/// The agent only ever manages containers carrying `MANAGED_LABEL`. That is not
/// tidiness: a node very plausibly runs other things — this repo was built on a
/// machine already running a `svencoop-prns-host` container — and an agent that
/// stopped or reaped a container it did not create would be destroying somebody
/// else's service.
pub const CONTAINER_PREFIX: &str = "gpp-";

/// Label stamped on every managed container, and the only thing that makes one
/// eligible for stop, remove or reap.
pub const MANAGED_LABEL: &str = "org.idan2025.gaming-platform-prns.managed";

/// Label carrying the instance id.
pub const INSTANCE_LABEL: &str = "org.idan2025.gaming-platform-prns.instance";

/// Label carrying the identity that asked for this instance, when something is
/// deploying on a user's behalf. The container is the ownership record, so an
/// index needs no database of its own.
pub const OWNER_LABEL: &str = "org.idan2025.gaming-platform-prns.owner";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentConfig {
    /// Where this node keeps shared content and per-instance state.
    pub data_root: PathBuf,
    /// Inclusive UDP port range the agent may allocate from.
    pub port_range: PortRange,
    /// What to run, per game id. A game with no entry cannot be started here.
    #[serde(default)]
    pub games: BTreeMap<String, GameRuntime>,
    /// Stop an instance that has had no players for this long. `None` disables
    /// reaping, which is only reasonable on a node whose instances are all
    /// deliberate — a public one needs it (`DESIGN.md` §4, abuse).
    #[serde(default)]
    pub idle_timeout_secs: Option<u64>,
    /// Refuse to start more than this many instances on this node.
    #[serde(default = "default_max_instances")]
    pub max_instances: usize,
    /// Where the local API listens.
    ///
    /// **Loopback only, enforced.** This API creates and destroys containers and
    /// has no authentication whatsoever, so anyone who can reach it can run any
    /// image the operator configured, on this machine. Binding it to a routable
    /// address would hand that to the network. Identity challenge/response
    /// arrives with the optional index in phase 4 (`PLAN.md` §8); until it does,
    /// the honest boundary is "you must already be on this host", and the config
    /// enforces it rather than documenting it and hoping.
    #[serde(default = "default_api_bind")]
    pub api_bind: SocketAddr,
    /// The Reticulum control uplink. `None` = local-only, today's behavior. When
    /// present, the agent also announces a `platform-agent.control` destination
    /// and answers authenticated create/stop/remove/list requests over a Link,
    /// so an index can drive this node without an inbound port or public IP
    /// (`DESIGN.md` §2.3, `PLAN.md` §8 phase 4). The loopback API above keeps
    /// working either way.
    #[serde(default)]
    pub uplink: Option<UplinkConfig>,
}

/// The agent's Reticulum control uplink, as the operator writes it.
///
/// **`trusted_indexes` is the whole authorization.** The agent authenticates a
/// caller by challenge/response (`platform_auth`), derives the caller's identity
/// from the signing key, and admits the session only if that identity is in this
/// list. An empty list refuses every caller — hosting off, the same shape as
/// `HostingConfig.games` empty meaning hosting off. Putting an index here is the
/// operator saying "I trust that index to deploy on this node and to enforce its
/// own quotas"; from the user's side agents stay untrusted (`DESIGN.md` §5).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UplinkConfig {
    /// Reticulum identity secret this agent announces under. Absolute path to a
    /// 64-byte file (X25519 secret ‖ Ed25519 secret); loaded at start, not here.
    pub identity_secret_path: PathBuf,
    /// Attach a `TcpServerInterface` on `host:port` (`0.0.0.0` for a public
    /// relay) — `None` means auto-discovered interfaces only. Same shape as
    /// `game_bridge::relay::attach_interfaces`.
    #[serde(default)]
    pub tcp: Option<String>,
    /// Auto-attach interfaces. Defaults false to match the bridge's explicit
    /// default; an operator opts in.
    #[serde(default)]
    pub auto: bool,
    /// Hex identity hashes of indexes this node will let deploy. 16 bytes each,
    /// 32 hex characters. **Empty refuses everyone.**
    #[serde(default)]
    pub trusted_indexes: Vec<String>,
}

fn default_api_bind() -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], 4750))
}

fn default_max_instances() -> usize {
    8
}

/// How one game runs on this node. Operator-chosen, never pack-supplied.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GameRuntime {
    /// Container image. **The operator's choice**, by reference, pinned by
    /// digest if they care — the agent does not resolve tags for them.
    pub image: String,
    /// Where the game's install lives inside the container. An image fact, so
    /// it belongs to whoever chose the image.
    pub content_root: PathBuf,
    /// Version directory under `<data_root>/content/<game_id>/`. Lets one node
    /// hold two versions of a game and run instances of both.
    pub content_version: String,
    /// Memory cap in bytes. `None` means no cap, which on a shared node means
    /// one instance can take the others down with it.
    #[serde(default)]
    pub memory_limit_bytes: Option<i64>,
    /// CPU quota as a fraction of one core, e.g. `1.5`.
    #[serde(default)]
    pub cpus: Option<f64>,
    /// Extra environment for the container. Not secrets — see `SECURITY` note
    /// in the agent docs.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PortRange {
    pub start: u16,
    pub end: u16,
}

impl PortRange {
    pub fn contains(&self, port: u16) -> bool {
        port >= self.start && port <= self.end
    }

    pub fn len(&self) -> usize {
        if self.end < self.start {
            0
        } else {
            (self.end as usize) - (self.start as usize) + 1
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[derive(Debug)]
pub enum ConfigError {
    Io(std::io::Error),
    Parse(toml::de::Error),
    /// A range that is backwards, empty, or reaches into the privileged ports.
    BadPortRange { start: u16, end: u16, why: &'static str },
    RelativeDataRoot(PathBuf),
    EmptyImage(String),
    RelativeContentRoot { game: String, path: PathBuf },
    /// The local API was pointed at something other than loopback.
    NonLoopbackApiBind(SocketAddr),
    /// A `trusted_indexes` entry was not a 32-char hex identity hash.
    BadTrustedIndex(String),
    /// The uplink identity secret path was relative, not absolute.
    RelativeUplinkIdentityPath(PathBuf),
}

impl core::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "reading agent config: {e}"),
            Self::Parse(e) => write!(f, "parsing agent config: {e}"),
            Self::BadPortRange { start, end, why } => {
                write!(f, "port range {start}-{end} is unusable: {why}")
            }
            Self::RelativeDataRoot(p) => write!(
                f,
                "data_root {} must be absolute: a relative root would resolve against \
                 whatever directory the agent happened to start in",
                p.display()
            ),
            Self::EmptyImage(game) => write!(f, "game {game:?} has no image"),
            Self::RelativeContentRoot { game, path } => write!(
                f,
                "game {game:?} content_root {} must be absolute inside the container",
                path.display()
            ),
            Self::NonLoopbackApiBind(addr) => write!(
                f,
                "api_bind {addr} is not a loopback address. This API creates containers \
                 and has no authentication, so it must not be reachable from the network. \
                 Put a reverse proxy in front of it if you need remote access"
            ),
            Self::BadTrustedIndex(s) => write!(
                f,
                "trusted_indexes entry {s:?} is not a 32-character hex identity hash"
            ),
            Self::RelativeUplinkIdentityPath(p) => write!(
                f,
                "uplink.identity_secret_path {} must be absolute: a relative path would \
                 resolve against whatever directory the agent started in",
                p.display()
            ),
        }
    }
}

impl std::error::Error for ConfigError {}

impl AgentConfig {
    pub fn parse(toml_src: &str) -> Result<Self, ConfigError> {
        let cfg: Self = toml::from_str(toml_src).map_err(ConfigError::Parse)?;
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let src = std::fs::read_to_string(path).map_err(ConfigError::Io)?;
        Self::parse(&src)
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        if !self.data_root.is_absolute() {
            return Err(ConfigError::RelativeDataRoot(self.data_root.clone()));
        }
        let (start, end) = (self.port_range.start, self.port_range.end);
        if end < start {
            return Err(ConfigError::BadPortRange { start, end, why: "end is before start" });
        }
        if start < 1024 {
            return Err(ConfigError::BadPortRange {
                start,
                end,
                why: "reaches into privileged ports, which a game server must not need",
            });
        }
        if !self.api_bind.ip().is_loopback() {
            return Err(ConfigError::NonLoopbackApiBind(self.api_bind));
        }
        if let Some(uplink) = &self.uplink {
            if !uplink.identity_secret_path.is_absolute() {
                return Err(ConfigError::RelativeUplinkIdentityPath(
                    uplink.identity_secret_path.clone(),
                ));
            }
            for hash in &uplink.trusted_indexes {
                if !is_identity_hash_hex(hash) {
                    return Err(ConfigError::BadTrustedIndex(hash.clone()));
                }
            }
        }
        for (game, runtime) in &self.games {
            if runtime.image.trim().is_empty() {
                return Err(ConfigError::EmptyImage(game.clone()));
            }
            if !runtime.content_root.is_absolute() {
                return Err(ConfigError::RelativeContentRoot {
                    game: game.clone(),
                    path: runtime.content_root.clone(),
                });
            }
        }
        Ok(())
    }

    pub fn runtime_for(&self, game_id: &str) -> Option<&GameRuntime> {
        self.games.get(game_id)
    }
}

/// A Reticulum identity hash is 16 bytes, written as 32 lowercase hex chars.
/// `trusted_indexes` entries must be exactly that, or the uplink cannot match a
/// verified identity against them. Uppercase is refused so a later lookup is a
/// straight byte compare, not a case-fold.
fn is_identity_hash_hex(s: &str) -> bool {
    s.len() == 32 && s.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
data_root = "/var/lib/gaming-platform-prns"
max_instances = 4

[port_range]
start = 27100
end = 27199

[games.sven-coop]
image = "ghcr.io/idan2025/svencoop-prns:0.1.10"
content_root = "/game"
content_version = "5.26"
memory_limit_bytes = 2147483648
cpus = 1.5
"#;

    /// TOML appends land inside the last table, so a key meant for the top
    /// level has to go before the first one.
    fn with_top_level(line: &str) -> String {
        SAMPLE.replacen("max_instances = 4", &format!("max_instances = 4\n{line}"), 1)
    }

    #[test]
    fn a_sample_config_parses() {
        let cfg = AgentConfig::parse(SAMPLE).unwrap();
        assert_eq!(cfg.port_range.len(), 100);
        let rt = cfg.runtime_for("sven-coop").unwrap();
        assert_eq!(rt.content_root, PathBuf::from("/game"));
        assert_eq!(cfg.max_instances, 4);
    }

    /// The rule that keeps a hostile pack from becoming a hostile container: a
    /// game the operator has not configured cannot run here, and the agent does
    /// not invent a default image for it.
    #[test]
    fn a_game_with_no_operator_entry_has_no_runtime() {
        let cfg = AgentConfig::parse(SAMPLE).unwrap();
        assert!(cfg.runtime_for("minecraft").is_none());
    }

    #[test]
    fn unknown_config_keys_are_rejected() {
        let src = SAMPLE.to_string() + "\nrun_as_root = true\n";
        assert!(matches!(AgentConfig::parse(&src), Err(ConfigError::Parse(_))));
    }

    #[test]
    fn a_relative_data_root_is_refused() {
        let src = SAMPLE.replace('"' .to_string().as_str(), "\"").replace(
            "data_root = \"/var/lib/gaming-platform-prns\"",
            "data_root = \"data\"",
        );
        assert!(matches!(AgentConfig::parse(&src), Err(ConfigError::RelativeDataRoot(_))));
    }

    #[test]
    fn a_privileged_or_backwards_port_range_is_refused() {
        let low = SAMPLE.replace("start = 27100", "start = 80").replace("end = 27199", "end = 90");
        assert!(matches!(AgentConfig::parse(&low), Err(ConfigError::BadPortRange { .. })));

        let backwards = SAMPLE.replace("end = 27199", "end = 27000");
        assert!(matches!(AgentConfig::parse(&backwards), Err(ConfigError::BadPortRange { .. })));
    }

    #[test]
    fn an_empty_image_is_refused() {
        let src = SAMPLE.replace("image = \"ghcr.io/idan2025/svencoop-prns:0.1.10\"", "image = \"  \"");
        assert!(matches!(AgentConfig::parse(&src), Err(ConfigError::EmptyImage(_))));
    }

    #[test]
    fn a_relative_container_content_root_is_refused() {
        let src = SAMPLE.replace("content_root = \"/game\"", "content_root = \"game\"");
        assert!(matches!(AgentConfig::parse(&src), Err(ConfigError::RelativeContentRoot { .. })));
    }

    /// An unauthenticated container-creating API on a routable address is a
    /// remote code execution service. The config refuses rather than warns.
    #[test]
    fn a_routable_api_bind_is_refused() {
        for addr in ["0.0.0.0:4750", "192.168.1.10:4750", "[::]:4750"] {
            let src = with_top_level(&format!("api_bind = \"{addr}\""));
            assert!(
                matches!(AgentConfig::parse(&src), Err(ConfigError::NonLoopbackApiBind(_))),
                "api_bind {addr} should have been refused"
            );
        }
    }

    #[test]
    fn the_api_defaults_to_loopback() {
        let cfg = AgentConfig::parse(SAMPLE).unwrap();
        assert!(cfg.api_bind.ip().is_loopback());
        let src = with_top_level("api_bind = \"127.0.0.1:9999\"");
        assert_eq!(AgentConfig::parse(&src).unwrap().api_bind.port(), 9999);
    }

    #[test]
    fn an_agent_without_an_uplink_is_local_only() {
        let cfg = AgentConfig::parse(SAMPLE).unwrap();
        assert!(cfg.uplink.is_none());
    }

    #[test]
    fn an_uplink_block_parses() {
        let src = with_top_level(
            "[uplink]\n\
             identity_secret_path = \"/etc/gpp/agent.key\"\n\
             tcp = \"0.0.0.0:4789\"\n\
             auto = true\n\
             trusted_indexes = [\"a1b2c3d4e5f6a7b8c9d0e1f2a3b4c5d6\"]\n",
        );
        let cfg = AgentConfig::parse(&src).unwrap();
        let uplink = cfg.uplink.expect("uplink present");
        assert_eq!(uplink.identity_secret_path, PathBuf::from("/etc/gpp/agent.key"));
        assert!(uplink.auto);
        assert_eq!(uplink.tcp.as_deref(), Some("0.0.0.0:4789"));
        assert_eq!(uplink.trusted_indexes.len(), 1);
    }

    /// Empty `trusted_indexes` is valid — it means the uplink refuses every
    /// caller, the same shape as `HostingConfig.games` empty meaning hosting off.
    #[test]
    fn an_uplink_with_no_trusted_indexes_is_valid_but_off() {
        let src = with_top_level(
            "[uplink]\nidentity_secret_path = \"/etc/gpp/agent.key\"\ntrusted_indexes = []\n",
        );
        let cfg = AgentConfig::parse(&src).unwrap();
        assert!(cfg.uplink.unwrap().trusted_indexes.is_empty());
    }

    #[test]
    fn an_uplink_identity_hash_must_be_32_hex_chars() {
        let src = with_top_level(
            "[uplink]\nidentity_secret_path = \"/etc/gpp/agent.key\"\n\
             trusted_indexes = [\"not-a-hash\"]\n",
        );
        assert!(matches!(
            AgentConfig::parse(&src),
            Err(ConfigError::BadTrustedIndex(_))
        ));
    }

    #[test]
    fn an_uplink_identity_hash_rejects_uppercase() {
        let src = with_top_level(
            "[uplink]\nidentity_secret_path = \"/etc/gpp/agent.key\"\n\
             trusted_indexes = [\"A1B2C3D4E5F6A7B8C9D0E1F2A3B4C5D6\"]\n",
        );
        assert!(matches!(
            AgentConfig::parse(&src),
            Err(ConfigError::BadTrustedIndex(_))
        ));
    }

    #[test]
    fn an_uplink_identity_secret_path_must_be_absolute() {
        let src = with_top_level(
            "[uplink]\nidentity_secret_path = \"agent.key\"\ntrusted_indexes = []\n",
        );
        assert!(matches!(
            AgentConfig::parse(&src),
            Err(ConfigError::RelativeUplinkIdentityPath(_))
        ));
    }

    #[test]
    fn an_uplink_block_rejects_unknown_keys() {
        let src = with_top_level(
            "[uplink]\nidentity_secret_path = \"/etc/gpp/agent.key\"\n\
             run_anything = true\n",
        );
        assert!(matches!(AgentConfig::parse(&src), Err(ConfigError::Parse(_))));
    }

    #[test]
    fn is_identity_hash_hex_matches_only_32_lowercase_hex() {
        assert!(is_identity_hash_hex("a1b2c3d4e5f6a7b8c9d0e1f2a3b4c5d6"));
        assert!(!is_identity_hash_hex("A1B2C3D4E5F6A7B8C9D0E1F2A3B4C5D6"));
        assert!(!is_identity_hash_hex("a1b2"));
        assert!(!is_identity_hash_hex("a1b2c3d4e5f6a7b8c9d0e1f2a3b4c5d6ee"));
        assert!(!is_identity_hash_hex("z1b2c3d4e5f6a7b8c9d0e1f2a3b4c5d6"));
    }

    #[test]
    fn port_range_bounds_are_inclusive() {
        let r = PortRange { start: 27100, end: 27101 };
        assert!(r.contains(27100) && r.contains(27101));
        assert!(!r.contains(27099) && !r.contains(27102));
        assert_eq!(r.len(), 2);
    }
}
