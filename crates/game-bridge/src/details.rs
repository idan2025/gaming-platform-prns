//! The detail probe (`PLAN.md` §3.4): what a browser asks a server directly.
//!
//! An announce is 316 bytes and carries a *row*. Everything that does not fit —
//! the live player count, who is on, how long the server has been up — is a
//! Link query to that one server. This module is that query's payload.
//!
//! # Why this is a Link request and not a bigger announce
//!
//! An announce is broadcast to the whole mesh on a timer. Putting a player list
//! in one would multiply every server's traffic by the size of its roster and
//! push it at every listener, forever, whether or not anyone was looking. A
//! probe costs one exchange, to one server, when a person actually opens it.
//! That asymmetry is the whole reason `PLAN.md` §3.3 keeps the announce record
//! small.
//!
//! It also means **the browser must never probe a list**. One probe per server
//! the user explicitly opened; never a background sweep, never "just the
//! visible rows". A browser that probed its list would be a scanner.
//!
//! # The flag that keeps this honest
//!
//! `StatsSource` says whether the numbers are **live** — read out of the game
//! server — or merely **announced**, i.e. the same static config the row
//! already showed. A live figure additionally carries `stats_age_secs`,
//! because the bridge polls the game on a timer rather than querying inside
//! the request: a probe that blocked on a 2-second UDP query would stall the
//! node's event loop for every other peer. So "live" means "read from the game
//! this recently", and the browser is told how recently instead of being left
//! to assume "now". Only GoldSrc and Source can be queried today (`a2s.rs`);
//! a Minecraft or Valheim bridge answers a probe with `Announced` and an empty
//! roster. Reporting configured numbers as live would make the detail pane less
//! trustworthy than the list it came from.
//!
//! # Wire format
//!
//! Request: one byte, the highest schema version the requester understands.
//! Response:
//!
//! ```text
//! byte 0      schema version (1)
//! byte 1      stats source: 0 announced, 1 live
//! byte 2      players
//! byte 3      max_players
//! bytes 4-7   uptime seconds, big-endian u32
//! bytes 8-9   bridge clients (links this bridge is relaying), big-endian u16
//! bytes 10-11 age of the live stats in seconds, big-endian u16; 0 when the
//!             source is Announced
//! byte 12     flags: bit 0 = the player list was truncated to fit
//! byte 13     len(game_id) + game_id
//! then        len(name)    + name
//! then        len(map)     + map
//! byte n      player count that follows, then that many len-prefixed names
//! ```
//!
//! Bounded to `MAX_DETAILS_RESPONSE_LEN` so one response rides one link packet.
//! The roster is truncated to fit rather than the response being refused: a
//! partial list with a flag saying so beats an error.

use crate::announce::{MAX_GAME_ID_LEN, MAX_MAP_LEN, MAX_NAME_LEN};

/// Endpoint id the server registers and the browser requests.
///
/// The trailing `/1` is the schema generation. A breaking change gets a new
/// endpoint id rather than a silently different payload, so an old browser
/// gets a clean "no such endpoint" instead of a mis-parse.
pub const DETAILS_ENDPOINT_ID: &str = "/game-bridge/details/1";

/// Schema version carried in byte 0 of the response.
pub const DETAILS_SCHEMA: u8 = 1;

/// Keep a response inside a single link packet. The engine's own cap is larger
/// and reported as `Decline::ResponseTooLarge`, but a probe that needs
/// reassembly is a probe that can half-arrive.
pub const MAX_DETAILS_RESPONSE_LEN: usize = 1800;

/// Longest player name carried. Longer ones are truncated on a char boundary.
pub const MAX_PLAYER_NAME_LEN: usize = 32;

/// Where the numbers in a `ServerDetails` came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatsSource {
    /// The server's configuration, the same values its announce carries. No
    /// live query was possible — the game speaks no query protocol this build
    /// implements, or the query failed.
    Announced,
    /// Read out of the running game server, `stats_age_secs` ago.
    Live,
}

impl StatsSource {
    fn to_byte(self) -> u8 {
        match self {
            Self::Announced => 0,
            Self::Live => 1,
        }
    }

    fn from_byte(b: u8) -> Self {
        // Anything unrecognised is treated as merely announced. Erring toward
        // "this might be stale" is the safe direction for a trust signal.
        if b == 1 {
            Self::Live
        } else {
            Self::Announced
        }
    }
}

const FLAG_ROSTER_TRUNCATED: u8 = 0x01;

/// What a server says about itself when asked directly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerDetails {
    pub game_id: String,
    pub name: String,
    pub map: String,
    pub players: u8,
    pub max_players: u8,
    pub stats_source: StatsSource,
    /// How long this bridge has been running, not the game server.
    pub uptime_secs: u32,
    /// Links this bridge is currently relaying. Its own view, and the one
    /// number here that cannot be stale.
    pub bridge_clients: u16,
    /// How long ago the live stats were read, in seconds. Zero and meaningless
    /// when `stats_source` is `Announced`.
    pub stats_age_secs: u16,
    pub player_names: Vec<String>,
    /// The roster did not fit and was cut short.
    pub roster_truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DetailsError {
    Truncated,
    InvalidUtf8,
    UnsupportedSchema(u8),
    TooLarge(usize),
}

impl core::fmt::Display for DetailsError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Truncated => write!(f, "details response is truncated"),
            Self::InvalidUtf8 => write!(f, "invalid UTF-8 in details response"),
            Self::UnsupportedSchema(v) => {
                write!(f, "details schema {v} is not supported by this build")
            }
            Self::TooLarge(n) => write!(f, "details response is {n} bytes, over the cap"),
        }
    }
}

impl std::error::Error for DetailsError {}

/// The request body a browser sends: the newest schema it understands.
pub fn request_body() -> [u8; 1] {
    [DETAILS_SCHEMA]
}

fn truncate(s: &str, max: usize) -> &str {
    let mut end = s.len().min(max);
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

fn push_str(buf: &mut Vec<u8>, s: &str, max: usize) {
    let s = truncate(s, max);
    buf.push(s.len() as u8);
    buf.extend_from_slice(s.as_bytes());
}

fn take<'a>(buf: &'a [u8], pos: &mut usize, n: usize) -> Result<&'a [u8], DetailsError> {
    let end = pos.checked_add(n).ok_or(DetailsError::Truncated)?;
    if end > buf.len() {
        return Err(DetailsError::Truncated);
    }
    let out = &buf[*pos..end];
    *pos = end;
    Ok(out)
}

fn take_byte(buf: &[u8], pos: &mut usize) -> Result<u8, DetailsError> {
    Ok(take(buf, pos, 1)?[0])
}

fn take_str(buf: &[u8], pos: &mut usize) -> Result<String, DetailsError> {
    let len = take_byte(buf, pos)? as usize;
    let bytes = take(buf, pos, len)?;
    std::str::from_utf8(bytes)
        .map(str::to_string)
        .map_err(|_| DetailsError::InvalidUtf8)
}

impl ServerDetails {
    /// Encode, dropping roster entries that do not fit and flagging that it
    /// happened. Only the fixed header plus the three strings are guaranteed a
    /// place; those are bounded by construction and always fit.
    pub fn encode(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(128);
        buf.push(DETAILS_SCHEMA);
        buf.push(self.stats_source.to_byte());
        buf.push(self.players);
        buf.push(self.max_players);
        buf.extend_from_slice(&self.uptime_secs.to_be_bytes());
        buf.extend_from_slice(&self.bridge_clients.to_be_bytes());
        let age = match self.stats_source {
            StatsSource::Live => self.stats_age_secs,
            // Meaningless without a live read; do not ship a number that
            // invites the UI to render "announced, 0s ago".
            StatsSource::Announced => 0,
        };
        buf.extend_from_slice(&age.to_be_bytes());
        let flags_at = buf.len();
        buf.push(0);
        push_str(&mut buf, &self.game_id, MAX_GAME_ID_LEN);
        push_str(&mut buf, &self.name, MAX_NAME_LEN);
        push_str(&mut buf, &self.map, MAX_MAP_LEN);

        // Reserve the roster-count byte, then add names while they fit.
        let count_at = buf.len();
        buf.push(0);
        let mut written: u8 = 0;
        let mut truncated = self.roster_truncated;
        for name in &self.player_names {
            if written == u8::MAX {
                truncated = true;
                break;
            }
            let name = truncate(name, MAX_PLAYER_NAME_LEN);
            if buf.len() + 1 + name.len() > MAX_DETAILS_RESPONSE_LEN {
                truncated = true;
                break;
            }
            buf.push(name.len() as u8);
            buf.extend_from_slice(name.as_bytes());
            written += 1;
        }
        buf[count_at] = written;
        if truncated {
            buf[flags_at] |= FLAG_ROSTER_TRUNCATED;
        }
        buf
    }

    pub fn decode(buf: &[u8]) -> Result<Self, DetailsError> {
        if buf.len() > MAX_DETAILS_RESPONSE_LEN {
            return Err(DetailsError::TooLarge(buf.len()));
        }
        let mut pos = 0usize;
        let schema = take_byte(buf, &mut pos)?;
        if schema != DETAILS_SCHEMA {
            return Err(DetailsError::UnsupportedSchema(schema));
        }
        let stats_source = StatsSource::from_byte(take_byte(buf, &mut pos)?);
        let players = take_byte(buf, &mut pos)?;
        let max_players = take_byte(buf, &mut pos)?;
        let uptime_secs = u32::from_be_bytes(
            take(buf, &mut pos, 4)?.try_into().map_err(|_| DetailsError::Truncated)?,
        );
        let bridge_clients = u16::from_be_bytes(
            take(buf, &mut pos, 2)?.try_into().map_err(|_| DetailsError::Truncated)?,
        );
        let stats_age_secs = u16::from_be_bytes(
            take(buf, &mut pos, 2)?.try_into().map_err(|_| DetailsError::Truncated)?,
        );
        let flags = take_byte(buf, &mut pos)?;
        let game_id = take_str(buf, &mut pos)?;
        let name = take_str(buf, &mut pos)?;
        let map = take_str(buf, &mut pos)?;

        let count = take_byte(buf, &mut pos)? as usize;
        let mut player_names = Vec::with_capacity(count.min(64));
        for _ in 0..count {
            player_names.push(take_str(buf, &mut pos)?);
        }

        Ok(Self {
            game_id,
            name,
            map,
            players,
            max_players,
            stats_source,
            uptime_secs,
            bridge_clients,
            stats_age_secs,
            player_names,
            roster_truncated: flags & FLAG_ROSTER_TRUNCATED != 0,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> ServerDetails {
        ServerDetails {
            game_id: "sven-coop".to_string(),
            name: "Idan's Server".to_string(),
            map: "svencoop1".to_string(),
            players: 3,
            max_players: 8,
            stats_source: StatsSource::Live,
            uptime_secs: 4321,
            bridge_clients: 2,
            stats_age_secs: 4,
            player_names: vec!["alice".into(), "bob".into(), "carol".into()],
            roster_truncated: false,
        }
    }

    #[test]
    fn round_trips() {
        let d = sample();
        assert_eq!(ServerDetails::decode(&d.encode()).unwrap(), d);
    }

    #[test]
    fn an_unqueryable_game_still_answers() {
        // A Minecraft bridge cannot run an A2S query, so it says so rather
        // than dressing configured numbers up as live ones.
        let d = ServerDetails {
            game_id: "minecraft".to_string(),
            stats_source: StatsSource::Announced,
            player_names: Vec::new(),
            ..sample()
        };
        let back = ServerDetails::decode(&d.encode()).unwrap();
        assert_eq!(back.stats_source, StatsSource::Announced);
        assert!(back.player_names.is_empty());
        assert_eq!(
            back.stats_age_secs, 0,
            "an age is meaningless without a live read and must not be carried"
        );
    }

    /// An unrecognised stats-source byte must read as "announced". A trust
    /// signal that fails open is worse than useless.
    #[test]
    fn an_unknown_stats_source_is_not_treated_as_live() {
        let mut bytes = sample().encode();
        bytes[1] = 99;
        assert_eq!(
            ServerDetails::decode(&bytes).unwrap().stats_source,
            StatsSource::Announced
        );
    }

    #[test]
    fn a_huge_roster_is_truncated_and_flagged() {
        let d = ServerDetails {
            player_names: (0..500).map(|i| format!("player-{i:04}")).collect(),
            ..sample()
        };
        let encoded = d.encode();
        assert!(encoded.len() <= MAX_DETAILS_RESPONSE_LEN);
        let back = ServerDetails::decode(&encoded).unwrap();
        assert!(back.roster_truncated, "truncation must be visible to the browser");
        assert!(!back.player_names.is_empty());
        assert!(back.player_names.len() < 500);
        assert_eq!(back.player_names[0], "player-0000");
    }

    #[test]
    fn over_long_strings_are_cut_on_char_boundaries() {
        let d = ServerDetails {
            name: "é".repeat(200),
            player_names: vec!["ü".repeat(100)],
            ..sample()
        };
        let back = ServerDetails::decode(&d.encode()).unwrap();
        assert_eq!(back.name.chars().count(), MAX_NAME_LEN / 2);
        assert_eq!(back.player_names[0].chars().count(), MAX_PLAYER_NAME_LEN / 2);
    }

    #[test]
    fn truncated_input_never_panics() {
        let bytes = sample().encode();
        for n in 0..bytes.len() {
            assert!(
                ServerDetails::decode(&bytes[..n]).is_err(),
                "a {n}-byte prefix decoded as a whole response"
            );
        }
    }

    #[test]
    fn a_future_schema_is_refused_rather_than_misread() {
        let mut bytes = sample().encode();
        bytes[0] = 2;
        assert_eq!(
            ServerDetails::decode(&bytes),
            Err(DetailsError::UnsupportedSchema(2))
        );
    }

    #[test]
    fn an_oversized_response_is_refused() {
        assert_eq!(
            ServerDetails::decode(&vec![1u8; MAX_DETAILS_RESPONSE_LEN + 1]),
            Err(DetailsError::TooLarge(MAX_DETAILS_RESPONSE_LEN + 1))
        );
    }

    #[test]
    fn the_request_body_advertises_this_builds_schema() {
        assert_eq!(request_body(), [DETAILS_SCHEMA]);
    }
}
