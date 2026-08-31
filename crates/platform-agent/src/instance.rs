//! What the agent is asked to run, and what it currently is.

use std::collections::BTreeMap;

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
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpecError {
    EmptyId,
    IdTooLong(usize),
    IdNotAllowed(String),
    EmptyGameId,
    EmptyName,
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

    #[test]
    fn a_spec_needs_a_game_and_a_name() {
        let mut s = spec("ok");
        s.game_id = String::new();
        assert_eq!(s.validate(), Err(SpecError::EmptyGameId));
        let mut s = spec("ok");
        s.name = "   ".to_string();
        assert_eq!(s.validate(), Err(SpecError::EmptyName));
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
}
