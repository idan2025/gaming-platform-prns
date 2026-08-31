//! What varies between games.
//!
//! The Sven Co-op bridge hardcoded three things the platform must vary: the
//! Reticulum app name, the local port it bridges to, and the assumption that
//! the game speaks UDP (`PLAN.md` §6). A `GameProfile` names them.
//!
//! This is the in-memory shape a `game-pack` manifest deserializes into
//! (`PLAN.md` §8 step 6). It is deliberately owned rather than `&'static`:
//! packs are loaded at runtime, not compiled in.

/// Aspect of a destination that accepts links and bridges them to a game
/// server. Not game-specific — the game lives in the app name.
pub const ASPECT_SERVER: &str = "server";

/// Aspect of a destination that initiates links on a player's behalf.
pub const ASPECT_CLIENT: &str = "client";

/// Longest `game_id` the announce record can carry (`PLAN.md` §3.3).
pub const MAX_GAME_ID_LEN: usize = 24;

/// A protocol for asking a running game server about itself.
///
/// Separate from `GameTransport` because the two do not follow each other: a
/// game can speak UDP and answer nothing (Quake-family), or answer a query on a
/// different port than it plays on. `None` is the honest default — most games
/// have no standard query protocol, and a detail probe against one of them
/// reports announced numbers with `StatsSource::Announced` rather than
/// pretending.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "lowercase"))]
pub enum QueryProtocol {
    /// Valve's A2S, on the same UDP port the game runs on. GoldSrc and Source.
    A2s,
}

/// How the bridge moves the game's traffic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum GameTransport {
    /// One raw datagram per link packet. What the Sven bridge does.
    Udp,
    /// A byte stream spliced onto a link's channel. Needed by Minecraft,
    /// Terraria and every other TCP game.
    ///
    /// Implemented by `stream.rs` (`DESIGN.md` §2.1 `StreamRelay`): the relay
    /// branches on this field, binds a TCP listener instead of a UDP socket on
    /// the client, connects instead of sending on the server, and gives each
    /// connection its own link. It does **not** share the datagram path's
    /// framing — chunking and ordering belong to the channel there.
    Tcp,
}

/// One port a game wants reachable, and the framing channel that carries it.
///
/// `GAMES.md` §3: a Source-engine server is a game port **and** an RCON port
/// (TCP, often the same number) **and** optionally SourceTV. A destination
/// fronts one port, so the extra ones ride framing channels
/// (`framing.rs`) or, for a TCP port, their own stream ids (`stream.rs`).
///
/// Channel 0 is the game itself and is frozen to framing generation 1, so it is
/// never listed here: it comes from `default_port` and `transport`, which stay
/// the single source of truth for the port every peer already speaks.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct GamePort {
    /// Framing channel, 1..=7. Channel 0 is the game port and is implicit.
    pub channel: u8,
    /// What this port is, for humans and logs: `"rcon"`, `"tv"`. Never parsed
    /// into behaviour — a name that selected code would be a pack naming what
    /// runs.
    pub name: String,
    /// Port on the game host.
    pub port: u16,
    /// This port's own transport. RCON is TCP on a server whose game port is
    /// UDP, which is exactly why this is per-port and not per-game.
    pub transport: GameTransport,
}

/// Everything the bridge needs to know about one game.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct GameProfile {
    /// Stable short id. Travels in the announce record's `game_id` field, so
    /// it is ASCII and at most `MAX_GAME_ID_LEN` bytes (`PLAN.md` §3.3).
    pub id: String,
    /// Reticulum app name. Combined with an aspect it derives the destination
    /// hash, so **this value is the wire contract**: change it and every peer
    /// running the old value becomes undiscoverable and unjoinable.
    pub app_name: String,
    /// What to show a human.
    pub display_name: String,
    /// Port the game server listens on when nothing overrides it.
    pub default_port: u16,
    pub transport: GameTransport,
    /// Viability tier from `GAMES.md` §4: 1 low-rate UDP tick, 2 TCP or
    /// bursty, 3 modern high-bitrate. Travels in the announce record so a
    /// browser can warn before a player joins something their mesh cannot
    /// carry. An ordering until phase 0 measures it.
    pub min_link_class: u8,
    /// How to ask this game's server for live stats, if it can be asked.
    pub query: Option<QueryProtocol>,
    /// Paths this game writes to, relative to its install directory. Used by
    /// `platform-agent` to give each instance writable space over a shared
    /// read-only copy of the content. Validated before use, never trusted.
    pub writable_paths: Vec<String>,
    /// Extra ports beyond the game's own, each on its own framing channel
    /// (`GAMES.md` §3). Empty for every single-port game, which is most of
    /// them — and a single-port game's wire behaviour is unchanged by this
    /// field existing.
    #[cfg_attr(feature = "serde", serde(default))]
    pub extra_ports: Vec<GamePort>,
}

/// Why a profile cannot be used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProfileError {
    EmptyId,
    /// A port claimed channel 0, which belongs to the game and comes from
    /// `default_port`.
    ExtraPortOnGameChannel,
    /// A channel id the three header bits cannot carry.
    ChannelTooLarge(u8),
    /// Two ports claimed the same channel.
    DuplicateChannel(u8),
    /// A port has no name, or two share one.
    BadPortName(String),
    IdTooLong(usize),
    IdNotAscii,
    EmptyAppName,
    UnknownLinkClass(u8),
    /// The pack describes a transport this build cannot actually bridge.
    ///
    /// No variant reaches this today — UDP is the datagram relay and TCP is
    /// `stream.rs` — but the check stays: the next transport to be declared
    /// before it is spliced must fail loudly rather than be bridged as
    /// something else.
    TransportNotImplemented(GameTransport),
}

impl core::fmt::Display for ProfileError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::EmptyId => write!(f, "game id is empty"),
            Self::ExtraPortOnGameChannel => write!(
                f,
                "channel 0 is the game's own port and comes from default_port; an extra port \
                 cannot claim it (framing generation 1 is frozen there)"
            ),
            Self::ChannelTooLarge(c) => {
                write!(f, "channel {c} is over the maximum of {}", crate::framing::MAX_CHANNEL)
            }
            Self::DuplicateChannel(c) => write!(f, "two ports claim channel {c}"),
            Self::BadPortName(n) => write!(f, "port name {n:?} is empty or duplicated"),
            Self::IdTooLong(n) => {
                write!(f, "game id is {n} bytes, over the {MAX_GAME_ID_LEN}-byte announce budget")
            }
            Self::IdNotAscii => write!(f, "game id must be ASCII"),
            Self::EmptyAppName => write!(f, "app name is empty"),
            Self::UnknownLinkClass(n) => {
                write!(f, "min_link_class {n} is not a GAMES.md §4 tier (1-3)")
            }
            Self::TransportNotImplemented(t) => write!(
                f,
                "transport {t:?} is declared by this pack but not implemented: the relay \
                 pumps datagrams and has no stream splice yet (DESIGN.md §2.1 StreamRelay). \
                 Running it would bridge the wrong protocol rather than fail"
            ),
        }
    }
}

impl std::error::Error for ProfileError {}

impl GameProfile {
    /// Every port this game wants reachable, channel 0 first.
    ///
    /// Channel 0 is synthesized from `default_port`/`transport` rather than
    /// stored, so there is exactly one place the frozen port is written down.
    pub fn ports(&self) -> Vec<GamePort> {
        let mut ports = vec![GamePort {
            channel: crate::framing::CHANNEL_GAME,
            name: "game".to_string(),
            port: self.default_port,
            transport: self.transport,
        }];
        ports.extend(self.extra_ports.iter().cloned());
        ports
    }

    /// What this build advertises in an announce.
    ///
    /// A single-port game announces generation 1: nothing about it needs
    /// channels, and announcing a capability it never exercises would put a
    /// number on the wire that means less than it says. A multi-port game
    /// announces generation 2, which is what tells a peer it may send a
    /// non-zero channel here (`framing.rs`).
    pub fn protocol_version(&self) -> u8 {
        if self.extra_ports.is_empty() {
            crate::framing::FRAMING_V1
        } else {
            crate::framing::FRAMING_V2
        }
    }

    /// Sven Co-op, as `idan2025/Svencoop-Prns` v0.1.10 speaks it.
    ///
    /// The values here are **frozen by `PLAN.md` §5**: `app_name` must stay
    /// `"sven-coop"` or the platform stops being able to join a deployed
    /// standalone server. `destination_hash_matches_deployed_sven` in this
    /// module's tests is the guard.
    pub fn sven_coop() -> Self {
        Self {
            id: "sven-coop".to_string(),
            app_name: "sven-coop".to_string(),
            display_name: "Sven Co-op".to_string(),
            default_port: 27015,
            transport: GameTransport::Udp,
            // GoldSrc, `GAMES.md` §4 tier 1.
            min_link_class: 1,
            query: Some(QueryProtocol::A2s),
            // Only what a server writes. A writable path is an empty
            // directory mounted *over* the content, so listing `svencoop/maps`
            // hid all 108 shipped maps — see `pack.rs`.
            writable_paths: vec!["svencoop/logs".to_string()],
            // GoldSrc plays and answers A2S on one port. Nothing to multiplex,
            // so it announces framing generation 1 like every deployed peer.
            extra_ports: Vec::new(),
        }
    }

    pub fn validate(&self) -> Result<(), ProfileError> {
        if self.id.is_empty() {
            return Err(ProfileError::EmptyId);
        }
        if self.id.len() > MAX_GAME_ID_LEN {
            return Err(ProfileError::IdTooLong(self.id.len()));
        }
        if !self.id.is_ascii() {
            return Err(ProfileError::IdNotAscii);
        }
        if self.app_name.is_empty() {
            return Err(ProfileError::EmptyAppName);
        }
        if !(1..=3).contains(&self.min_link_class) {
            return Err(ProfileError::UnknownLinkClass(self.min_link_class));
        }
        let mut seen_channels = std::collections::BTreeSet::new();
        let mut seen_names = std::collections::BTreeSet::new();
        for port in &self.extra_ports {
            if port.channel == crate::framing::CHANNEL_GAME {
                return Err(ProfileError::ExtraPortOnGameChannel);
            }
            if port.channel > crate::framing::MAX_CHANNEL {
                return Err(ProfileError::ChannelTooLarge(port.channel));
            }
            if !seen_channels.insert(port.channel) {
                return Err(ProfileError::DuplicateChannel(port.channel));
            }
            if port.name.trim().is_empty() || !seen_names.insert(port.name.clone()) {
                return Err(ProfileError::BadPortName(port.name.clone()));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use personal_rns::prelude::*;

    fn fixed_identity() -> Zeroizing<[u8; IDENTITY_SECRET_KEY_LEN]> {
        Zeroizing::new([0x01u8; IDENTITY_SECRET_KEY_LEN])
    }

    fn destination_hash(app_name: &str, aspect: &str) -> String {
        let d = PreConfiguredDestination::Single {
            app_name,
            aspects: &[aspect],
            identity: fixed_identity(),
            announce_app_data: b"",
            proof: ProofStrategy::ProveAll,
            link_requests: LinkRequestPolicy::AcceptAll,
            ratchet: RatchetPolicy::NoRatchets,
            resource_strategy: ResourceStrategy::AcceptNone,
            maximum_request_bytes: Default::default(),
            request_endpoints: ServeMyRequestEndpoints::No,
        };
        hex::encode(d.destination_hash().expect("valid destination name").as_bytes())
    }

    /// Golden values for a fixed identity secret. If either of these changes,
    /// the platform can no longer reach a deployed v0.1.10 Sven server and no
    /// deployed client can reach a platform-hosted one (`PLAN.md` §5). An
    /// engine bump that alters destination derivation trips this too — which
    /// is the point.
    #[test]
    fn destination_hash_matches_deployed_sven() {
        let p = GameProfile::sven_coop();
        assert_eq!(p.app_name, "sven-coop");
        assert_eq!(destination_hash(&p.app_name, ASPECT_SERVER), SVEN_SERVER_HASH);
        assert_eq!(destination_hash(&p.app_name, ASPECT_CLIENT), SVEN_CLIENT_HASH);
    }

    // Computed from `fixed_identity()` against the pinned engine. Both are
    // the values `idan2025/Svencoop-Prns` v0.1.10 derives from the same
    // identity, because it feeds the same app name and aspects to the same
    // engine tree (`ENGINE.md`).
    const SVEN_SERVER_HASH: &str = "38e8890a391cad0c7ef5da222bd607b8";
    const SVEN_CLIENT_HASH: &str = "d953345aba27b2933585083d14d36c41";

    /// A profile for another game must not collide with Sven's destinations.
    #[test]
    fn a_different_app_name_is_a_different_destination() {
        assert_ne!(destination_hash("minecraft", ASPECT_SERVER), SVEN_SERVER_HASH);
    }

    #[test]
    fn sven_profile_validates() {
        GameProfile::sven_coop().validate().expect("sven profile is valid");
    }

    /// TCP is a supported transport now that `stream.rs` exists. The relay
    /// branches on the field rather than pumping datagrams regardless, which is
    /// what made declaring `tcp` dangerous before: the wrong protocol, working
    /// badly, with nothing in the logs to explain it.
    #[test]
    fn a_tcp_pack_is_accepted_now_that_the_stream_relay_exists() {
        let mut p = GameProfile::sven_coop();
        p.transport = GameTransport::Tcp;
        assert_eq!(p.validate(), Ok(()));
    }

    #[test]
    fn validate_rejects_bad_ids() {
        let mut p = GameProfile::sven_coop();
        p.id = String::new();
        assert_eq!(p.validate(), Err(ProfileError::EmptyId));
        p.id = "x".repeat(MAX_GAME_ID_LEN + 1);
        assert_eq!(p.validate(), Err(ProfileError::IdTooLong(MAX_GAME_ID_LEN + 1)));
        p.id = "sven-cöop".to_string();
        assert_eq!(p.validate(), Err(ProfileError::IdNotAscii));
        p = GameProfile::sven_coop();
        p.app_name = String::new();
        assert_eq!(p.validate(), Err(ProfileError::EmptyAppName));
    }
}
