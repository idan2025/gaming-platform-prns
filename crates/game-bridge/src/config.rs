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
    /// Which shape this server's announces take.
    pub announce_format: AnnounceFormat,
    /// Current and maximum player counts to advertise. Stale by up to one
    /// announce interval by construction; the live number comes from a probe
    /// over a Link (`PLAN.md` §3.4). Ignored by `AnnounceFormat::Legacy`.
    pub players: u8,
    pub max_players: u8,
    /// Current map, if the game has one. Ignored by `AnnounceFormat::Legacy`.
    pub map: Option<String>,
    /// Whether this is a dedicated server rather than a listen server.
    pub dedicated: bool,
    /// Transport mode 1-3 (`MODES.md`).
    pub transport_mode: u8,
    /// Whether joining needs a password.
    pub passworded: bool,
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
    /// Whether this node also carries **other people's** traffic across its
    /// interfaces. See `RelayArgs` for what that means and why the default
    /// differs by role. A host volunteering a game server is already
    /// volunteering resources, so this defaults **on** for a server.
    pub relay_transit: bool,
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
            announce_format: AnnounceFormat::Record,
            players: 0,
            max_players: 0,
            map: None,
            dedicated: true,
            transport_mode: 1,
            passworded: false,
            allowlist: Vec::new(),
            identify_timeout_secs: 10,
            relay_transit: true,
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
    /// Local port the game client connects to. Channel 0, the game itself.
    pub listen_port: u16,
    /// Local ports for the pack's extra channels (`GAMES.md` §3), keyed by
    /// channel. A channel with no entry lands on `listen_port + channel`.
    ///
    /// Not defaulted to the game's *own* port numbers: those belong to the
    /// server's host, and a player whose machine already runs something on
    /// 27015 should not have a bridge fight it for the port.
    #[cfg_attr(feature = "serde", serde(default))]
    pub extra_listen_ports: std::collections::BTreeMap<u8, u16>,
    /// Destination hash of the server to join, hex. When `None` the client
    /// waits for an announce of this game's server aspect and takes the first
    /// one it hears.
    pub server_hash: Option<String>,
    /// Where to persist the client identity. Generated on first run.
    pub identity: PathBuf,
    pub tcp: Option<String>,
    pub auto: bool,
    /// Whether this node also carries other people's traffic.
    ///
    /// **Defaults off, unlike v0.1.10.** A player who installed this to join
    /// one server was, in v0.1.10, unconditionally forwarding strangers'
    /// traffic across whatever connection they were on, with no prompt, no
    /// counter and no off switch (`PLAN.md` §4). On a metered or mobile
    /// connection that is a defect. A client that wants to donate transit can
    /// turn it on, or run the `Relay` role.
    pub relay_transit: bool,
}

impl ClientArgs {
    pub fn new(profile: GameProfile) -> Self {
        let listen_port = profile.default_port;
        Self {
            profile,
            listen_port,
            extra_listen_ports: std::collections::BTreeMap::new(),
            server_hash: None,
            identity: PathBuf::from("./game-bridge-client.identity"),
            tcp: None,
            auto: false,
            relay_transit: false,
        }
    }
}

/// Browse role: listen, list, and nothing else.
///
/// `PLAN.md` §8 phase 2's zero-infrastructure baseline. A browse node binds no
/// game port, announces nothing, registers no destination, and — unless asked —
/// forwards nothing. It attaches interfaces and listens. That is the whole
/// role, and it is deliberately the cheapest thing in the crate: the list must
/// work with no index, no account and no internet, so that a central service
/// can never quietly become load-bearing.
///
/// It holds no identity, and that is also why it has **no transit switch**: a
/// transport identity is what makes a node forward for others, and this role
/// never holds one. Browsing a list is not consent to carry strangers' packets
/// (`PLAN.md` §4); someone who wants to donate transit runs `RelayArgs`.
///
/// It does not need an identity to hear announces. The detail probe in
/// `PLAN.md` §3.4 — opening a Link to ask a server for its player list and
/// mods — does need one, and that is when this grows a field.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct BrowserArgs {
    pub tcp: Option<String>,
    pub auto: bool,
}

impl BrowserArgs {
    pub fn new() -> Self {
        Self { tcp: None, auto: false }
    }
}

impl Default for BrowserArgs {
    fn default() -> Self {
        Self::new()
    }
}

/// Relay role: donate transit and nothing else.
///
/// A node with interfaces and a transport identity, no game, no bound game
/// port, and **no announced destination** — there is nothing to announce, since
/// a transport node is not a service anyone links to. It is the smallest of the
/// four roles (`PLAN.md` §1) and the only way to donate transit deliberately
/// rather than as a side effect of running a game.
///
/// Two things a UI must say plainly, per `PLAN.md` §4:
///
/// - **A relay cannot read what it carries.** Links are end-to-end encrypted
///   and a transport node forwards ciphertext. That is the argument for asking
///   strangers to donate transit; say it rather than burying it.
/// - **It costs bandwidth.** `BridgeSession::transit_stats` is there so a user
///   can see what they are giving, because a donation they cannot see becomes
///   "my connection broke" and gets switched off forever.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct RelayArgs {
    /// Where to persist the relay identity. Generated on first run. A
    /// transport node's identity is stable so paths through it stay stable.
    pub identity: PathBuf,
    /// A public relay wants `0.0.0.0:<port>`, which binds a TCP server that
    /// many peers can dial. Any other host dials one instead.
    pub tcp: Option<String>,
    pub auto: bool,
}

impl RelayArgs {
    pub fn new() -> Self {
        Self {
            identity: PathBuf::from("./game-bridge-relay.identity"),
            tcp: None,
            auto: false,
        }
    }
}

impl Default for RelayArgs {
    fn default() -> Self {
        Self::new()
    }
}

/// What a server puts in its announce `app_data`.
///
/// **The trade this encodes.** A `Record` is what makes the browser possible at
/// all: `Diagnostic::AnnounceHeard` exposes no aspect and no identity, and the
/// destination hash is one-way, so the game id can only reach a listener inside
/// `app_data` (`PLAN.md` §3.1). Filtering by game, tier, or player count needs
/// it.
///
/// The cost is one-directional and cosmetic: a deployed `svencoop-prns` v0.1.10
/// *client* decodes `app_data` as a bare UTF-8 display name, so a platform
/// server announcing a record shows up in that client's list under a garbled
/// name. It still **joins** — the destination hash is unaffected, and that is
/// what `PLAN.md` §5 actually requires. Nothing in the other direction changes:
/// a platform browser reading a deployed server's bare name is exactly the
/// mandatory fallback in `announce::decode`.
///
/// `Legacy` exists for a server that would rather look right to deployed Sven
/// clients than be filterable in the platform browser. It cannot advertise a
/// game id, so a platform browser can only show it as an unattributed row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum AnnounceFormat {
    /// The `PLAN.md` §3.3 record. The default.
    Record,
    /// A bare UTF-8 display name, byte-identical to what v0.1.10 announces.
    Legacy,
}

#[derive(Debug, Clone)]
pub enum BridgeConfig {
    Server(ServerArgs),
    Client(ClientArgs),
    Relay(RelayArgs),
    Browse(BrowserArgs),
}

impl BridgeConfig {
    pub fn role(&self) -> BridgeRole {
        match self {
            Self::Server(_) => BridgeRole::Server,
            Self::Client(_) => BridgeRole::Client,
            Self::Relay(_) => BridgeRole::Relay,
            Self::Browse(_) => BridgeRole::Browse,
        }
    }

    /// `None` for the relay role: it carries traffic for every game and knows
    /// about none of them.
    pub fn profile(&self) -> Option<&GameProfile> {
        match self {
            Self::Server(a) => Some(&a.profile),
            Self::Client(a) => Some(&a.profile),
            Self::Relay(_) | Self::Browse(_) => None,
        }
    }

    /// Whether this configuration carries other people's traffic.
    pub fn relays_transit(&self) -> bool {
        match self {
            Self::Server(a) => a.relay_transit,
            Self::Client(a) => a.relay_transit,
            Self::Relay(_) => true,
            // A browse node holds no transport identity, so it forwards
            // nothing. Not a policy, a structural fact.
            Self::Browse(_) => false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BridgeRole {
    Server,
    Client,
    /// Donates transit, runs no game. `PLAN.md` §4.
    Relay,
    /// Listens and lists. Runs no game, announces nothing. `PLAN.md` §3.
    Browse,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The `PLAN.md` §4 defect, as a test. v0.1.10 forwarded strangers'
    /// traffic from every client unconditionally. A client must now opt in; a
    /// server, which is already volunteering resources, stays on.
    #[test]
    fn a_client_does_not_donate_transit_by_default() {
        let p = GameProfile::sven_coop();
        assert!(!ClientArgs::new(p.clone()).relay_transit, "a client must opt in");
        assert!(ServerArgs::new(p).relay_transit, "a host is already volunteering");
    }

    /// A browse node cannot forward for anyone, and the type says so rather
    /// than offering a switch that would do nothing.
    #[test]
    fn a_browse_node_never_relays_and_has_no_game() {
        let cfg = BridgeConfig::Browse(BrowserArgs::new());
        assert_eq!(cfg.role(), BridgeRole::Browse);
        assert!(!cfg.relays_transit());
        assert!(cfg.profile().is_none(), "browsing spans every game");
    }

    #[test]
    fn relay_role_always_relays_and_has_no_game() {
        let cfg = BridgeConfig::Relay(RelayArgs::new());
        assert_eq!(cfg.role(), BridgeRole::Relay);
        assert!(cfg.relays_transit());
        assert!(cfg.profile().is_none(), "a relay carries every game and knows none");
    }

    #[test]
    fn relays_transit_follows_the_game_roles_switch() {
        let p = GameProfile::sven_coop();
        let mut client = ClientArgs::new(p.clone());
        assert!(!BridgeConfig::Client(client.clone()).relays_transit());
        client.relay_transit = true;
        assert!(BridgeConfig::Client(client).relays_transit());

        let mut server = ServerArgs::new(p);
        assert!(BridgeConfig::Server(server.clone()).relays_transit());
        server.relay_transit = false;
        assert!(!BridgeConfig::Server(server).relays_transit());
    }

    #[test]
    fn defaults_come_from_the_profile() {
        let p = GameProfile::sven_coop();
        assert_eq!(ServerArgs::new(p.clone()).game_port, 27015);
        assert_eq!(ClientArgs::new(p).listen_port, 27015);
    }
}
