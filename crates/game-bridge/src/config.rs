//! Bridge configuration.
//!
//! Copied from `idan2025/Svencoop-Prns` `src/config.rs` and parametrized by
//! `GameProfile`. Two deliberate differences from the original:
//!
//! - **No `clap`.** The original derived its CLI here because the crate *was*
//!   the CLI. This crate is a library that a launcher drives; a `--sc-port`
//!   flag has no meaning once the game is a runtime-loaded pack. A CLI can
//!   derive its own parser and build these structs.
//! - **`sc_host`/`sc_port` became `game_host`/`game_port`**, defaulted from the
//!   profile rather than hardcoded to 27015.

use std::path::PathBuf;

use crate::profile::GameProfile;

/// Server role: announce a destination, bridge accepted links to a local game
/// server's port.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct ServerArgs {
    /// Which game this bridge fronts. Fixes the app name and the default port.
    pub profile: GameProfile,
    /// Host the real game server runs on. `127.0.0.1` when co-located.
    pub game_host: String,
    /// Port the real game server listens on.
    pub game_port: u16,
    /// Where to persist the server identity. Generated on first run.
    pub identity: PathBuf,
    /// Optional TCP interface, e.g. `0.0.0.0:4234` to bind one, or
    /// `host:port` to dial one. With no interface and `auto` off the node
    /// cannot talk to anything.
    pub tcp: Option<String>,
    /// Wi-Fi/LAN auto-discovery for nearby peers.
    pub auto: bool,
    /// Announce interval in seconds.
    pub announce_interval: u64,
    /// Display name broadcast in this server's announces.
    pub name: Option<String>,
    /// Identity hashes (hex, 32 chars) permitted to link. Empty means open to
    /// anyone, which is v0.1.10's only behaviour.
    ///
    /// Enforcement is necessarily *after* link establishment, not before it —
    /// the engine offers no per-request hook and no identity until the peer
    /// identifies itself. `relay.rs`'s `parse_allowlist` documents why, and
    /// why `identify_timeout_secs` is not optional.
    pub allowlist: Vec<String>,
    /// How long an accepted link may go without identifying itself before it
    /// is closed. Only consulted when `allowlist` is non-empty.
    pub identify_timeout_secs: u64,
}

impl ServerArgs {
    /// Defaults matching the standalone Sven bridge's, with the port taken
    /// from the profile.
    pub fn new(profile: GameProfile) -> Self {
        let game_port = profile.default_port;
        Self {
            profile,
            game_host: "127.0.0.1".to_string(),
            game_port,
            identity: PathBuf::from("./game-bridge-server.identity"),
            tcp: None,
            auto: false,
            announce_interval: 15,
            name: None,
            allowlist: Vec::new(),
            identify_timeout_secs: 10,
        }
    }
}

/// Client role: bind a local port the game client connects to, relay it over a
/// link to an announced server.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct ClientArgs {
    /// Which game this bridge fronts.
    pub profile: GameProfile,
    /// Local port the game client connects to.
    pub listen_port: u16,
    /// Destination hash of the server to join, hex. When `None` the client
    /// waits for an announce of this game's server aspect and takes the first
    /// one it hears.
    pub server_hash: Option<String>,
    /// Where to persist the client identity. Generated on first run.
    pub identity: PathBuf,
    pub tcp: Option<String>,
    pub auto: bool,
}

impl ClientArgs {
    pub fn new(profile: GameProfile) -> Self {
        let listen_port = profile.default_port;
        Self {
            profile,
            listen_port,
            server_hash: None,
            identity: PathBuf::from("./game-bridge-client.identity"),
            tcp: None,
            auto: false,
        }
    }
}

#[derive(Debug, Clone)]
pub enum BridgeConfig {
    Server(ServerArgs),
    Client(ClientArgs),
}

impl BridgeConfig {
    pub fn role(&self) -> BridgeRole {
        match self {
            Self::Server(_) => BridgeRole::Server,
            Self::Client(_) => BridgeRole::Client,
        }
    }

    pub fn profile(&self) -> &GameProfile {
        match self {
            Self::Server(a) => &a.profile,
            Self::Client(a) => &a.profile,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BridgeRole {
    Server,
    Client,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_come_from_the_profile() {
        let p = GameProfile::sven_coop();
        assert_eq!(ServerArgs::new(p.clone()).game_port, 27015);
        assert_eq!(ClientArgs::new(p).listen_port, 27015);
    }
}
