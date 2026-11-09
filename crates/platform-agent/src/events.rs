//! Things that happen on a node, told to whoever wants to hear them.
//!
//! The agent does not know what a notice is, who an admin is, or where a
//! message goes. It reports the event at the place it already logs it, and a
//! build that wants to act on it (a hosted node sending a chat message, say)
//! implements [`NodeEvents`] and sets it with `Agent::set_events` before the
//! agent starts serving. Every method defaults to doing nothing, so a build
//! that does not listen costs nothing, and a new event is added here without
//! touching any build that already exists.
//!
//! What a method is given is plain data about the event, never a handle into
//! the agent: a listener cannot change what it is told about.

use std::sync::Arc;

use crate::instance::LastExit;

/// The server an event is about, as far as the agent knows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerEvent {
    pub instance_id: String,
    pub game_id: Option<String>,
    /// The name the owner gave it, or the agent's record of it.
    pub name: Option<String>,
    /// The account the server belongs to, when it has one.
    pub owner: Option<String>,
}

pub trait NodeEvents: Send + Sync {
    /// A server ended on its own, and the crash watch saw it. Called before
    /// the agent starts it again, whether or not that restart is allowed, so
    /// a listener hears about every crash exactly once.
    fn server_crashed(&self, _server: &ServerEvent, _exit: &LastExit) {}

    /// The agent stopped a server itself, for its own files being over their
    /// limit (`check_disk_limits`). `reason` is the agent's sentence for it.
    fn server_stopped_by_host(&self, _server: &ServerEvent, _reason: &str) {}

    /// A page account was removed. Its name, not its password or sessions.
    fn account_removed(&self, _username: &str) {}

    /// A page account signed in and a session was made for it.
    fn session_started(&self, _username: &str) {}
}

/// Listens to nothing. What an agent uses until a build sets its own.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoEvents;

impl NodeEvents for NoEvents {}

/// The default listener, shared so one agent can hand it out without a clone
/// of the listener itself.
pub fn none() -> Arc<dyn NodeEvents> {
    Arc::new(NoEvents)
}
