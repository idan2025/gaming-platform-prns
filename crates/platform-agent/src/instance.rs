//! What the agent is asked to run, and what it currently is.

use std::collections::BTreeMap;

use game_bridge::console::{
    validate_map_name, validate_server_name, MapNameError, ServerNameError, MAX_BOTS,
};
use game_bridge::profile::GameTransport;
use serde::{Deserialize, Serialize};

/// Longest instance id. Matches the store planner's cap, because an id becomes
/// a directory name and a container name.
pub const MAX_INSTANCE_ID_LEN: usize = crate::store::MAX_ID_LEN;

/// A request to run one game server.
///
/// Note what is **not** here: no image, no command, no environment beyond what
/// the operator configured, no host paths. A spec says *which game* and *how
/// big*; the node decides what that means in terms of code to execute
/// (`config.rs`). A spec that could name an image would let whoever submits one
/// run arbitrary code on the node.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstanceSpec {
    /// Stable id, chosen by the caller. Becomes a directory and a container
    /// name, so it is validated the same way the store validates ids.
    pub instance_id: String,
    /// Which game pack this instance runs.
    pub game_id: String,
    /// Display name announced to the mesh.
    pub name: String,
    pub max_players: u8,
    /// Optional fixed port. `None` lets the agent allocate one, which is the
    /// normal case; a fixed port is for an instance that must keep the one it
    /// had across a restart.
    #[serde(default)]
    pub port: Option<u16>,
    /// Fixed host ports for the game's *extra* channels (`GAMES.md` §3), keyed
    /// by channel. Same purpose as `port` and the same default: absent means
    /// "allocate one".
    ///
    /// Which channels exist is the pack's business, not the caller's — a
    /// channel named here that the pack does not declare is ignored rather than
    /// honoured, because a spec that could open a port the game never asked for
    /// is a spec choosing what the node exposes.
    #[serde(default)]
    pub extra_ports: BTreeMap<u8, u16>,
    /// How many bots the server starts with, for a game whose pack declares a
    /// bot implementation. `None` leaves it to the image, which adds none.
    ///
    /// A count, never a command — `console::BotProtocol` turns it into console
    /// lines, and the container gets it as `GPP_BOTS`. A game with no bots
    /// ignores it rather than failing the create: the field is a request, and
    /// the pack decides whether there is anything to request.
    #[serde(default)]
    pub bots: Option<u8>,

    /// Which map the server starts on. `None` leaves it to the image's own
    /// default, which is what every instance did before this field existed.
    ///
    /// A name, never a command: it is validated by
    /// [`game_bridge::console::validate_map_name`] before it reaches either the
    /// container's environment or a console line, and a name that does not pass
    /// fails the create rather than being rewritten.
    #[serde(default)]
    pub map: Option<String>,
    /// Start this server again if it crashes (`agent.rs`, `check_crashes`).
    /// Off unless asked for, and changeable later: it is kept beside the
    /// instance, not on the container, because a container's labels are fixed
    /// at create.
    #[serde(default)]
    pub restart_on_crash: bool,
    /// Who asked for this, when something else is deploying on a user's behalf.
    /// An identity hash in hex; opaque to the agent.
    ///
    /// The agent does no access control with it — a caller that reached the
    /// loopback API is already trusted here. It exists so an **index** can
    /// reconstruct who owns what by listing the node, instead of keeping its own
    /// database that would drift from reality the first time someone stopped a
    /// container by hand. Same reasoning as the agent keeping no instance
    /// database of its own: the containers are the record.
    #[serde(default)]
    pub owner: Option<String>,
}

/// Where an instance is in its life.
///
/// `Unknown` is deliberately a state rather than an error: the agent's view of
/// a container can be stale or absent, and a caller that cannot tell "stopped"
/// from "I could not look" will report the wrong thing to a user.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InstanceState {
    Creating,
    Running,
    Stopped,
    /// The agent has a record of it, but no container exists. Usually somebody
    /// removed the container by hand.
    Missing,
    Unknown,
}

/// One published port of a running instance: which channel it serves, which
/// host port it landed on, and in which transport.
///
/// `host_port` is the node's, allocated from the operator's range; the port
/// *inside* the container is the pack's own number and is not reported, because
/// nothing outside the node can reach it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstancePort {
    pub channel: u8,
    pub host_port: u16,
    pub transport: GameTransport,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstanceStatus {
    pub instance_id: String,
    pub game_id: String,
    pub name: String,
    pub state: InstanceState,
    /// The game's own port — channel 0, and the one a player connects to. Kept
    /// as its own field rather than "the first of `ports`": it is the existing
    /// contract, and every single-port game has exactly this and nothing else.
    pub port: Option<u16>,
    /// Every port this instance publishes, channel 0 included. Empty for a
    /// container started by a build that predates port sets, which is why
    /// `port` is still read on its own.
    #[serde(default)]
    pub ports: Vec<InstancePort>,
    pub container_id: Option<String>,
    /// Seconds since the container started, when it is running.
    #[serde(default)]
    pub uptime_secs: Option<u64>,
    /// Who this was deployed for, if anyone. Read back off the container label.
    #[serde(default)]
    pub owner: Option<String>,
    /// The container's address on the Docker network, when it is running.
    ///
    /// Not part of the public contract a user cares about; it is here so the
    /// agent can reach the game to query it, from inside a container or from
    /// the host.
    #[serde(default)]
    pub container_ip: Option<String>,
    /// Hex Reticulum destination this server is announced under, when the node
    /// puts its games on the mesh. `None` means LAN-only — either the node has
    /// no `[mesh]` section, or this instance's bridge did not start.
    ///
    /// This is the address a player joins from a launcher, and it is the whole
    /// difference between a server on the mesh and a container with a published
    /// port.
    #[serde(default)]
    pub mesh_destination: Option<String>,
    /// Players on this server right now, when the game can be asked.
    ///
    /// `None` is **not zero**, and the difference decides whether an instance
    /// is reaped: a game whose pack declares no query protocol, or one that did
    /// not answer, must never read as empty. An index that treated unknown as
    /// zero would stop a busy server for being idle.
    #[serde(default)]
    pub players_now: Option<u32>,
    /// The map this server is on right now, when the game can be asked.
    ///
    /// `None` carries the same meaning as it does for `players_now`: not "no
    /// map", but "this game answers no query, or did not answer". A UI must
    /// show the difference rather than render an empty string.
    #[serde(default)]
    pub map_now: Option<String>,
    /// The server's player cap, as the game reports it in the same query as
    /// `players_now`. `None` under the same rule: the game could not be asked.
    /// What the game says rather than the spec's `max_players`, because an
    /// instance started by an older agent, or one whose config overrides the
    /// cap, still reports its real one.
    #[serde(default)]
    pub max_players_now: Option<u32>,
    /// How a stopped server ended, read from Docker. `None` while it runs, and
    /// for a container Docker would not describe.
    #[serde(default)]
    pub last_exit: Option<LastExit>,
    /// Whether this node starts the server again when it crashes.
    #[serde(default)]
    pub restart_on_crash: Option<bool>,
    /// Crash restarts in the last hour. The agent stops trying after
    /// `MAX_CRASH_RESTARTS_PER_HOUR`, so a server that dies at boot is left
    /// stopped rather than started forever.
    #[serde(default)]
    pub crash_restarts: Option<u32>,
    /// How many bots this node last asked this server to hold, when it asked at
    /// all.
    ///
    /// **The node's request, not a reading of the game.** A2S reports bots
    /// among its player counts and this build does not separate them, so
    /// presenting a queried number here would be a different fact wearing the
    /// same name. `None` means nobody has set a count on this instance.
    #[serde(default)]
    pub bots: Option<u8>,
}

/// How a container last ended, as `docker inspect` records it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LastExit {
    pub code: i64,
    /// The kernel killed it for going over its memory limit.
    pub oom_killed: bool,
    /// RFC 3339, as Docker writes it.
    pub finished_at: Option<String>,
    /// Docker's own error, when the container could not even start.
    pub error: Option<String>,
}

impl LastExit {
    /// Whether this ending was a crash rather than a stop.
    ///
    /// A stop ends in 0, or in the signal a stop sends: SIGTERM (143), SIGINT
    /// (130), or SIGKILL (137) once `docker stop`'s grace runs out. Anything
    /// else - a segfault (139), an abort (134), an error exit - is a crash,
    /// and so is the kernel's out-of-memory kill, which also reads as 137.
    /// Docker's start error counts too: the server never ran.
    pub fn is_crash(&self) -> bool {
        self.oom_killed
            || self.error.as_deref().is_some_and(|e| !e.is_empty())
            || !matches!(self.code, 0 | 130 | 137 | 143)
    }
}

/// What one running server is using, as Docker's own counters say.
///
/// **Counters, not rates.** CPU time, bytes on the network and bytes to disk
/// only ever grow, and a rate is the change between two readings, so whoever
/// polls works it out from consecutive samples. That keeps the agent free of
/// per-caller history: two browsers watching the same node each get rates over
/// their own poll interval, and an agent restart loses nothing anyone needed.
///
/// `None` is "Docker did not report it", never zero, for the same reason
/// `players_now` makes that distinction.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstanceUsage {
    pub instance_id: String,
    /// Milliseconds since the Unix epoch when the agent took this reading. The
    /// divisor for every rate, so it is the agent's clock, read once per
    /// container, and not the time the response reached the browser.
    pub sampled_at_ms: u64,
    /// CPU time this container has used, in nanoseconds, all cores together.
    pub cpu_total_ns: u64,
    /// CPU time the whole machine has spent, in nanoseconds, all cores
    /// together. A container's share is its change over this one's.
    pub system_cpu_ns: Option<u64>,
    /// Cores the container can see. Usually the machine's.
    pub online_cpus: Option<u64>,
    /// Memory in use, without the page cache the kernel can drop at will —
    /// the figure `docker stats` shows, not the raw cgroup total.
    pub memory_bytes: Option<u64>,
    /// The container's memory limit; the machine's memory when none is set.
    pub memory_limit_bytes: Option<u64>,
    /// Bytes received and sent, summed over the container's interfaces.
    pub net_rx_bytes: u64,
    pub net_tx_bytes: u64,
    /// Bytes read from and written to disk.
    pub disk_read_bytes: u64,
    pub disk_write_bytes: u64,
    /// Processes and threads in the container.
    pub pids: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpecError {
    EmptyId,
    IdTooLong(usize),
    IdNotAllowed(String),
    EmptyGameId,
    EmptyName,
    /// A display name the game would read as console commands or options.
    BadName(ServerNameError),
    /// The requested starting map is not a usable map name.
    BadMap(MapNameError),
    /// More bots than this build will ask any server for.
    TooManyBots(u8),
}

impl core::fmt::Display for SpecError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::EmptyId => write!(f, "instance id is empty"),
            Self::IdTooLong(n) => {
                write!(f, "instance id is {n} bytes, over the {MAX_INSTANCE_ID_LEN}-byte limit")
            }
            Self::IdNotAllowed(id) => write!(
                f,
                "instance id {id:?} must be lowercase [a-z0-9._-], and must not be \
                 \".\", \"..\", or begin with a dot"
            ),
            Self::EmptyGameId => write!(f, "spec names no game"),
            Self::EmptyName => write!(f, "spec has no display name"),
            Self::BadName(e) => write!(f, "{e}"),
            Self::BadMap(e) => write!(f, "{e}"),
            Self::TooManyBots(n) => {
                write!(f, "{n} bots is over this build's limit of {MAX_BOTS}")
            }
        }
    }
}

impl std::error::Error for SpecError {}

/// The one place an id is judged, shared by the store planner and the container
/// namer so the two can never disagree about what is acceptable.
///
/// Lowercase only, so two ids cannot collide on a case-insensitive filesystem
/// and then fight over one directory. No leading dot, so an id cannot hide a
/// directory. No `.` or `..`, so it cannot mean "the parent".
pub fn validate_id(id: &str) -> Result<(), SpecError> {
    if id.is_empty() {
        return Err(SpecError::EmptyId);
    }
    if id.len() > MAX_INSTANCE_ID_LEN {
        return Err(SpecError::IdTooLong(id.len()));
    }
    if id == "." || id == ".." || id.starts_with('.') {
        return Err(SpecError::IdNotAllowed(id.to_string()));
    }
    if !id
        .bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'.' || b == b'_' || b == b'-')
    {
        return Err(SpecError::IdNotAllowed(id.to_string()));
    }
    Ok(())
}

impl InstanceSpec {
    pub fn validate(&self) -> Result<(), SpecError> {
        validate_id(&self.instance_id)?;
        if self.game_id.is_empty() {
            return Err(SpecError::EmptyGameId);
        }
        if self.name.trim().is_empty() {
            return Err(SpecError::EmptyName);
        }
        // The name becomes `+hostname "<name>"` on a game's command line, and
        // both engines split that line again: `;`, `+` and a leading `-` turn
        // the rest of a name into commands and options. Checked here so both
        // front doors, the loopback API and the uplink, refuse it.
        validate_server_name(&self.name).map_err(SpecError::BadName)?;
        // Judged here, with the rest of the spec, so a bad map name is refused
        // at the API's front door rather than at the moment it is interpolated
        // into a container's environment.
        if let Some(map) = &self.map {
            validate_map_name(map).map_err(SpecError::BadMap)?;
        }
        // The same ceiling the live console path applies, checked here so a
        // create and a later change agree about what is askable.
        if let Some(bots) = self.bots {
            if bots > MAX_BOTS {
                return Err(SpecError::TooManyBots(bots));
            }
        }
        Ok(())
    }

    /// The container name this instance gets. Prefixed so the agent's own
    /// containers are recognisable at a glance in `docker ps`, next to whatever
    /// else the node is running.
    pub fn container_name(&self) -> String {
        format!("{}{}", crate::config::CONTAINER_PREFIX, self.instance_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(id: &str) -> InstanceSpec {
        InstanceSpec {
            instance_id: id.to_string(),
            game_id: "sven-coop".to_string(),
            name: "Test".to_string(),
            max_players: 8,
            port: None,
            extra_ports: BTreeMap::new(),
            map: None,
            bots: None,
            restart_on_crash: false,
            owner: None,
        }
    }

    #[test]
    fn a_reasonable_spec_validates_and_names_its_container() {
        let s = spec("coop-1");
        s.validate().unwrap();
        assert_eq!(s.container_name(), "gpp-coop-1");
    }

    /// Each of these is a way to escape a directory, collide with another
    /// instance, or hide from an operator listing the node.
    #[test]
    fn ids_that_could_escape_or_collide_are_refused() {
        for bad in ["", ".", "..", ".hidden", "Upper", "has space", "has/slash", "a\\b", "a:b"] {
            assert!(
                spec(bad).validate().is_err(),
                "id {bad:?} should have been refused"
            );
        }
        assert_eq!(spec(&"x".repeat(65)).validate(), Err(SpecError::IdTooLong(65)));
    }

    #[test]
    fn the_permitted_alphabet_is_accepted() {
        for good in ["a", "coop-1", "sven.coop_2", "0", "a-b_c.d"] {
            spec(good).validate().unwrap_or_else(|e| panic!("{good:?} was refused: {e}"));
        }
    }

    /// A map name reaches a container's environment and a console line, so the
    /// spec is where a bad one stops — not the moment it is interpolated.
    #[test]
    fn a_spec_refuses_a_map_name_that_is_not_one() {
        let mut s = spec("ok");
        s.map = Some("de_dust2\nquit".to_string());
        assert!(matches!(s.validate(), Err(SpecError::BadMap(_))));
        s.map = Some("../../etc/passwd".to_string());
        assert!(matches!(s.validate(), Err(SpecError::BadMap(_))));
        s.map = Some("svencoop1".to_string());
        s.validate().unwrap();
    }

    #[test]
    fn a_spec_needs_a_game_and_a_name() {
        let mut s = spec("ok");
        s.game_id = String::new();
        assert_eq!(s.validate(), Err(SpecError::EmptyGameId));
        let mut s = spec("ok");
        s.name = "   ".to_string();
        assert_eq!(s.validate(), Err(SpecError::EmptyName));
    }

    /// The spec is where both front doors meet, so this is the test that fails
    /// if `validate_server_name` is ever defined and no longer called.
    #[test]
    fn a_spec_whose_name_would_type_at_the_console_is_refused() {
        let mut s = spec("ok");
        s.name = "probe; sv_password injected".to_string();
        assert!(matches!(s.validate(), Err(SpecError::BadName(_))));
        s.name = "probe -insecure".to_string();
        assert!(matches!(s.validate(), Err(SpecError::BadName(_))));
    }

    /// `Unknown` must round-trip as its own thing: a caller that cannot tell it
    /// from `Stopped` will tell a user their server is down when the truth is
    /// that the agent could not look.
    #[test]
    fn unknown_is_a_state_of_its_own() {
        let json = serde_json::to_string(&InstanceState::Unknown).unwrap();
        assert_eq!(json, "\"unknown\"");
        assert_ne!(InstanceState::Unknown, InstanceState::Stopped);
    }

    /// Lanthorn Host reads these keys by name, and JavaScript reads a renamed
    /// one as `undefined`: a rename would turn "14/16" back into "14" and
    /// nothing would fail. An older agent's reply, without the cap, still
    /// parses.
    #[test]
    fn the_player_cap_is_sent_under_the_key_the_page_reads() {
        let s = InstanceStatus {
            instance_id: "cz".into(),
            game_id: "condition-zero".into(),
            name: "Testing CZ".into(),
            state: InstanceState::Running,
            port: Some(27303),
            ports: Vec::new(),
            container_id: None,
            uptime_secs: None,
            owner: None,
            container_ip: None,
            mesh_destination: None,
            players_now: Some(14),
            max_players_now: Some(16),
            last_exit: None,
            restart_on_crash: None,
            crash_restarts: None,
            map_now: Some("cs_italy_cz".into()),
            bots: Some(14),
        };
        let v = serde_json::to_value(&s).unwrap();
        assert_eq!(v["players_now"], 14);
        assert_eq!(v["max_players_now"], 16);
        let mut older = v.clone();
        older.as_object_mut().unwrap().remove("max_players_now");
        let back: InstanceStatus = serde_json::from_value(older).unwrap();
        assert_eq!(back.max_players_now, None);
    }

    fn exit(code: i64) -> LastExit {
        LastExit { code, oom_killed: false, finished_at: None, error: None }
    }

    /// A stop is never mistaken for a crash, or a crash-restart would undo
    /// every Stop the operator pressed; and a crash is never mistaken for a
    /// stop, or restart-on-crash would do nothing.
    #[test]
    fn a_stop_is_not_a_crash_and_a_crash_is_not_a_stop() {
        for stop in [0, 130, 137, 143] {
            assert!(!exit(stop).is_crash(), "exit {stop} is how a stop ends");
        }
        for crash in [1, 2, 134, 139, 255] {
            assert!(exit(crash).is_crash(), "exit {crash} is a crash");
        }
        assert!(LastExit { oom_killed: true, ..exit(137) }.is_crash(), "out of memory reads as 137 too");
        assert!(LastExit { error: Some("no such file".into()), ..exit(0) }.is_crash());
    }
}
