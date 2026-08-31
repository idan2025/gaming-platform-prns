//! Content installs that outlive the request that asked for them.
//!
//! `POST /content/:game` used to run the whole download inside the HTTP
//! request. A Sven Co-op install is 2.7 GB, so that request lives for tens of
//! minutes — longer than a browser, a proxy or a phone's radio will hold a
//! connection open. The download would usually finish; the operator would
//! usually never learn that it had.
//!
//! So an install runs as a task and the request returns immediately with its
//! state. That also makes the thing an operator actually wants possible:
//! pressing **Start a server** on a game whose files are missing begins the
//! download instead of refusing, because "install it first" is a step the
//! machine can take on its own.
//!
//! # The rules
//!
//! * **One install per game at a time.** Two steamcmd runs into the same
//!   staging directory would race over the same files, and the second would
//!   "succeed" over a tree the first was still writing. Asking again while one
//!   runs joins the existing install rather than starting a second.
//! * **A finished install is remembered until something asks.** A task that
//!   erased itself on completion would lose the failure message for whoever
//!   polls a moment later, and the failure message is the whole value — it is
//!   the sentence naming the missing directory or the digest that did not
//!   match.
//! * **A task holds no lock on the agent.** It owns a game id and reports a
//!   state; everything about whether content is really on disk stays the
//!   provisioner's business, which is checked again at start.

use std::collections::BTreeMap;
use std::sync::Arc;

use serde::Serialize;
use tokio::sync::Mutex;

/// Where one game's install has got to.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum InstallState {
    /// Nothing has been asked for, or nothing is remembered.
    Idle,
    /// Running now. `since_secs` is how long, which is the only honest progress
    /// figure available: steamcmd reports percentages to its own stdout, and
    /// parsing a tool's console output to drive a progress bar is a promise
    /// that breaks the next time the tool is updated.
    Running { since_secs: u64 },
    /// Finished. `bytes` is what landed, or 0 when it was already installed.
    Done { bytes: u64, already_installed: bool },
    /// Failed, carrying the sentence the operator has to act on.
    Failed { error: String },
}

struct Task {
    state: InstallState,
    started: std::time::Instant,
}

/// The installs this agent knows about, by game id.
#[derive(Default)]
pub struct Installs {
    tasks: Mutex<BTreeMap<String, Task>>,
}

impl Installs {
    pub fn new() -> Self {
        Self::default()
    }

    /// What this game's install is doing.
    pub async fn state(&self, game_id: &str) -> InstallState {
        let tasks = self.tasks.lock().await;
        match tasks.get(game_id) {
            None => InstallState::Idle,
            Some(t) => match &t.state {
                // Recomputed rather than stored, so a caller polling every few
                // seconds sees a number that moves.
                InstallState::Running { .. } => InstallState::Running {
                    since_secs: t.started.elapsed().as_secs(),
                },
                other => other.clone(),
            },
        }
    }

    /// Whether an install for this game is running right now.
    pub async fn is_running(&self, game_id: &str) -> bool {
        matches!(self.state(game_id).await, InstallState::Running { .. })
    }

    /// Claim the right to install this game, or report that someone already
    /// has it.
    ///
    /// The check and the claim happen under one lock. Two requests arriving
    /// together would otherwise both see "not running" and both start
    /// steamcmd into the same staging directory.
    pub async fn begin(&self, game_id: &str) -> bool {
        let mut tasks = self.tasks.lock().await;
        if let Some(t) = tasks.get(game_id) {
            if matches!(t.state, InstallState::Running { .. }) {
                return false;
            }
        }
        tasks.insert(
            game_id.to_string(),
            Task {
                state: InstallState::Running { since_secs: 0 },
                started: std::time::Instant::now(),
            },
        );
        true
    }

    /// Record how an install ended.
    pub async fn finish(&self, game_id: &str, state: InstallState) {
        let mut tasks = self.tasks.lock().await;
        if let Some(t) = tasks.get_mut(game_id) {
            t.state = state;
        }
    }
}

/// Start an install in the background, unless one is already running.
///
/// Returns whether this call started it. Either way the caller can poll
/// [`Installs::state`]; the distinction matters only for what to say to a
/// person — "started" or "already going".
pub async fn spawn(
    installs: Arc<Installs>,
    agent: Arc<crate::agent::Agent>,
    game_id: String,
) -> bool {
    if !installs.begin(&game_id).await {
        return false;
    }
    tokio::spawn(async move {
        let outcome = agent.ensure_content(&game_id).await;
        let state = match outcome {
            Ok(crate::content::Provisioned::AlreadyInstalled(_)) => {
                InstallState::Done { bytes: 0, already_installed: true }
            }
            Ok(crate::content::Provisioned::Installed { bytes, .. }) => {
                InstallState::Done { bytes, already_installed: false }
            }
            // The sentence is the product here: it names the missing directory,
            // the digest that did not match, or the switch the operator has not
            // turned on.
            Err(e) => InstallState::Failed { error: format!("{e}") },
        };
        if let InstallState::Failed { error } = &state {
            tracing::warn!(game = %game_id, error = %error, "content install failed");
        } else {
            tracing::info!(game = %game_id, "content install finished");
        }
        installs.finish(&game_id, state).await;
    });
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn an_unknown_game_is_idle_rather_than_an_error() {
        let installs = Installs::new();
        assert_eq!(installs.state("nothing").await, InstallState::Idle);
    }

    /// Two steamcmd runs into one staging directory would race over the same
    /// files, and the second would "succeed" over a tree the first was still
    /// writing.
    #[tokio::test]
    async fn only_one_install_per_game_can_be_claimed() {
        let installs = Installs::new();
        assert!(installs.begin("sven-coop").await);
        assert!(!installs.begin("sven-coop").await, "a second claim must be refused");
        // A different game is unaffected.
        assert!(installs.begin("half-life").await);
    }

    /// And the claim is released when it ends, so a failed install can be
    /// retried rather than wedging the game forever.
    #[tokio::test]
    async fn a_finished_install_can_be_started_again() {
        let installs = Installs::new();
        installs.begin("sven-coop").await;
        installs
            .finish("sven-coop", InstallState::Failed { error: "no disk".into() })
            .await;
        match installs.state("sven-coop").await {
            InstallState::Failed { error } => assert_eq!(error, "no disk"),
            other => panic!("{other:?}"),
        }
        assert!(installs.begin("sven-coop").await, "a failed install must be retryable");
    }

    /// The failure message is the whole value of remembering a finished task —
    /// it is the sentence naming what the operator has to fix — so it survives
    /// until something asks.
    #[tokio::test]
    async fn a_failure_is_remembered_for_whoever_polls_next() {
        let installs = Installs::new();
        installs.begin("g").await;
        installs
            .finish("g", InstallState::Failed { error: "digest did not match".into() })
            .await;
        for _ in 0..3 {
            assert!(matches!(installs.state("g").await, InstallState::Failed { .. }));
        }
    }
}
