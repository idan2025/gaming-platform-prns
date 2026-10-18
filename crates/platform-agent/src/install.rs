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
//! * **One download at a time per node** (`ROADMAP.md` §12.2). The others wait
//!   as `Queued`, take no space until they run, and can be cancelled while
//!   they wait. That keeps the reserve arithmetic honest: what is free when a
//!   download starts is not also promised to another.
//! * **A download starts only if it fits above the reserve, and stops if it
//!   reaches it** (`Agent::room_for`, `Agent::disk_reached_reserve`). The
//!   check runs when the download leaves the queue, against the disk as it is
//!   then; a stop goes through the same path as a cancel, so it leaves
//!   nothing, and says why.

use std::collections::BTreeMap;
use std::sync::Arc;

use serde::Serialize;
use tokio::sync::Mutex;

use crate::content::InstallProgress;

/// Where one game's install has got to.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum InstallState {
    /// Nothing has been asked for, or nothing is remembered.
    Idle,
    /// Running now. `since_secs` is how long, and `bytes` is what is on disk
    /// in its staging directory so far — both measured, never parsed:
    /// steamcmd reports percentages to its own stdout, and parsing a tool's
    /// console output to drive a progress bar is a promise that breaks the
    /// next time the tool is updated. Two readings give the rate.
    ///
    /// `total_bytes` is present only when something actually knows it. No
    /// driver does yet, so it is always absent: an invented total is a
    /// progress bar that lies.
    Running {
        since_secs: u64,
        bytes: u64,
        #[serde(skip_serializing_if = "Option::is_none")]
        total_bytes: Option<u64>,
    },
    /// Waiting for the download running now to finish (one at a time per
    /// node). Nothing is on disk for it yet.
    Queued { since_secs: u64 },
    /// Finished. `bytes` is what landed, or 0 when it was already installed.
    /// `update` is present when this was an update rather than an install.
    Done {
        bytes: u64,
        already_installed: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        update: Option<crate::agent::UpdateReport>,
        /// Present when this was a check of the files (`Agent::verify_content`).
        #[serde(skip_serializing_if = "Option::is_none")]
        verify: Option<Box<crate::agent::VerifyReport>>,
    },
    /// Failed, carrying the sentence the operator has to act on.
    Failed { error: String },
}

struct Task {
    state: InstallState,
    started: std::time::Instant,
    progress: Arc<InstallProgress>,
    /// Held by a delete (`exclusive`), not a download: it blocks one, and is
    /// never reported as one.
    held: bool,
}

impl InstallState {
    /// Running or waiting to: the slot is taken.
    pub fn is_active(&self) -> bool {
        matches!(self, Self::Running { .. } | Self::Queued { .. })
    }
}

/// How often a running download looks at the disk.
const DISK_WATCH_EVERY: std::time::Duration = std::time::Duration::from_secs(2);

/// The installs this agent knows about, by game id.
pub struct Installs {
    tasks: Mutex<BTreeMap<String, Task>>,
    /// One permit: the download running now.
    queue: tokio::sync::Semaphore,
}

impl Default for Installs {
    fn default() -> Self {
        Self { tasks: Mutex::new(BTreeMap::new()), queue: tokio::sync::Semaphore::new(1) }
    }
}

impl Installs {
    pub fn new() -> Self {
        Self::default()
    }

    /// What this game's install is doing.
    pub async fn state(&self, game_id: &str) -> InstallState {
        let (since_secs, progress) = {
            let tasks = self.tasks.lock().await;
            match tasks.get(game_id) {
                None => return InstallState::Idle,
                Some(t) if t.held => return InstallState::Idle,
                Some(t) => match &t.state {
                    InstallState::Running { .. } => {
                        (t.started.elapsed().as_secs(), t.progress.clone())
                    }
                    InstallState::Queued { .. } => {
                        return InstallState::Queued { since_secs: t.started.elapsed().as_secs() }
                    }
                    other => return other.clone(),
                },
            }
        };
        // Recomputed rather than stored, so a caller polling every few seconds
        // sees numbers that move. The walk happens outside the lock: it reads a
        // directory tree that may be gigabytes.
        InstallState::Running { since_secs, bytes: progress.bytes(), total_bytes: None }
    }

    /// The games with an install running now or waiting to.
    pub async fn running(&self) -> Vec<String> {
        let tasks = self.tasks.lock().await;
        tasks
            .iter()
            .filter(|(_, t)| !t.held && t.state.is_active())
            .map(|(g, _)| g.clone())
            .collect()
    }

    /// Ask a running install to stop. Returns the provisioning container to
    /// remove, if one is running, or `Err` when nothing is downloading.
    ///
    /// The task itself discards its staging directory and records
    /// `Failed { error: "cancelled" }` as it unwinds; nothing it fetched is
    /// installed.
    pub async fn cancel(&self, game_id: &str) -> Result<Option<String>, NotRunning> {
        let tasks = self.tasks.lock().await;
        match tasks.get(game_id) {
            Some(t) if !t.held && t.state.is_active() => {
                t.progress.cancel();
                Ok(t.progress.task_container())
            }
            _ => Err(NotRunning),
        }
    }

    /// Hold this game's install slot while `f` runs, so no download can start
    /// into the tree `f` is changing. Refused while one is already running:
    /// the install would rename its staging directory into a tree that was
    /// just removed.
    ///
    /// The slot is released afterwards with nothing remembered, as if no
    /// install had been asked for.
    pub async fn exclusive<T>(
        &self,
        game_id: &str,
        f: impl std::future::Future<Output = T>,
    ) -> Result<T, NotRunning> {
        if self.claim(game_id, true).await.is_none() {
            return Err(NotRunning);
        }
        let out = f.await;
        self.tasks.lock().await.remove(game_id);
        Ok(out)
    }

    /// Whether an install for this game is running right now.
    pub async fn is_running(&self, game_id: &str) -> bool {
        let tasks = self.tasks.lock().await;
        tasks.get(game_id).is_some_and(|t| t.state.is_active())
    }

    /// Claim the right to install this game, or report that someone already
    /// has it.
    ///
    /// The check and the claim happen under one lock. Two requests arriving
    /// together would otherwise both see "not running" and both start
    /// steamcmd into the same staging directory.
    pub async fn begin(&self, game_id: &str) -> Option<Arc<InstallProgress>> {
        self.claim(game_id, false).await
    }

    async fn claim(&self, game_id: &str, held: bool) -> Option<Arc<InstallProgress>> {
        let mut tasks = self.tasks.lock().await;
        if let Some(t) = tasks.get(game_id) {
            if t.state.is_active() {
                return None;
            }
        }
        let progress = Arc::new(InstallProgress::default());
        // A download waits its turn; a delete's hold is not a download and
        // takes the slot at once.
        let state = if held {
            InstallState::Running { since_secs: 0, bytes: 0, total_bytes: None }
        } else {
            InstallState::Queued { since_secs: 0 }
        };
        tasks.insert(
            game_id.to_string(),
            Task {
                state,
                started: std::time::Instant::now(),
                progress: progress.clone(),
                held,
            },
        );
        Some(progress)
    }

    /// Wait for this node's one download slot, then mark the task running.
    /// `None` if it was cancelled while it waited.
    async fn take_turn(&self, game_id: &str, progress: &InstallProgress) -> Option<tokio::sync::SemaphorePermit<'_>> {
        let permit = loop {
            if progress.is_cancelled() {
                return None;
            }
            // A short wait, then look at the cancel flag again: a queued
            // download is cancelled without waiting out the one before it.
            if let Ok(Ok(p)) = tokio::time::timeout(std::time::Duration::from_millis(250), self.queue.acquire()).await {
                break p;
            }
        };
        if progress.is_cancelled() {
            return None;
        }
        let mut tasks = self.tasks.lock().await;
        if let Some(t) = tasks.get_mut(game_id) {
            t.state = InstallState::Running { since_secs: 0, bytes: 0, total_bytes: None };
            t.started = std::time::Instant::now();
        }
        Some(permit)
    }

    /// Record how an install ended.
    pub async fn finish(&self, game_id: &str, state: InstallState) {
        let mut tasks = self.tasks.lock().await;
        if let Some(t) = tasks.get_mut(game_id) {
            t.state = state;
        }
    }
}

/// Nothing is downloading for that game, or something already holds its slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NotRunning;

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
    spawn_version(installs, agent, game_id, None).await
}

/// [`spawn`] for one picked copy of a game whose copies are picked per server
/// (a Minecraft build). The download is still tracked under the game's id:
/// one download per game at a time, like every other.
pub async fn spawn_version(
    installs: Arc<Installs>,
    agent: Arc<crate::agent::Agent>,
    game_id: String,
    version: Option<String>,
) -> bool {
    let Some(progress) = installs.begin(&game_id).await else {
        return false;
    };
    tokio::spawn(async move {
        let Some(_turn) = installs.take_turn(&game_id, &progress).await else {
            installs.finish(&game_id, InstallState::Failed { error: "cancelled".into() }).await;
            return;
        };
        // A fresh install's size is not known (`ROADMAP.md` §12.2), so it is
        // held to the reserve alone; the watch below is what protects the
        // disk. Content already there needs no room.
        if agent.content_is_missing(&game_id, version.as_deref()).await {
            if let Err(error) = agent.room_for(0).await {
                tracing::warn!(game = %game_id, error = %error, "content install not started");
                installs.finish(&game_id, InstallState::Failed { error }).await;
                return;
            }
        }
        let outcome = watched(&agent, &progress, agent.ensure_content_tracked(&game_id, version.as_deref(), Some(&progress))).await;
        let state = match outcome {
            Ok(crate::content::Provisioned::AlreadyInstalled(_)) => {
                InstallState::Done { bytes: 0, already_installed: true, update: None, verify: None }
            }
            Ok(crate::content::Provisioned::Installed { bytes, .. }) => {
                InstallState::Done { bytes, already_installed: false, update: None, verify: None }
            }
            // The sentence is the product here: it names the missing directory,
            // the digest that did not match, or the switch the operator has not
            // turned on.
            Err(e) => InstallState::Failed { error: progress.stop_reason().unwrap_or_else(|| format!("{e}")) },
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

/// Start an update in the background, in the same slot an install uses: an
/// update and an install of one game, or two updates, never run at once.
pub async fn spawn_update(
    installs: Arc<Installs>,
    agent: Arc<crate::agent::Agent>,
    game_id: String,
    remove_old: bool,
) -> bool {
    let Some(progress) = installs.begin(&game_id).await else {
        return false;
    };
    tokio::spawn(async move {
        let Some(_turn) = installs.take_turn(&game_id, &progress).await else {
            installs.finish(&game_id, InstallState::Failed { error: "cancelled".into() }).await;
            return;
        };
        // An update copies the old copy first, so it needs that much and the
        // delta; never started if that would take the disk below the reserve.
        if let Err(error) = agent.room_for(agent.update_needs(&game_id)).await {
            tracing::warn!(game = %game_id, error = %error, "content update not started");
            installs.finish(&game_id, InstallState::Failed { error }).await;
            return;
        }
        let outcome = watched(&agent, &progress, agent.update_content(&game_id, remove_old, Some(&progress))).await;
        let state = match outcome {
            Ok(report) => {
                tracing::info!(game = %game_id, to = ?report.to_version, old = %report.old, "content update finished");
                let bytes = 0;
                InstallState::Done { bytes, already_installed: report.to_version.is_none(), update: Some(report), verify: None }
            }
            Err(e) => {
                let error = progress.stop_reason().unwrap_or_else(|| format!("{e}"));
                tracing::warn!(game = %game_id, error = %error, "content update failed");
                InstallState::Failed { error }
            }
        };
        installs.finish(&game_id, state).await;
    });
    true
}

/// Check a game's files against Valve's in the background
/// (`Agent::verify_content`), in the same slot and queue as an install or an
/// update, held to the same reserve: it copies the whole copy first.
pub async fn spawn_verify(installs: Arc<Installs>, agent: Arc<crate::agent::Agent>, game_id: String) -> bool {
    let Some(progress) = installs.begin(&game_id).await else {
        return false;
    };
    tokio::spawn(async move {
        let Some(_turn) = installs.take_turn(&game_id, &progress).await else {
            installs.finish(&game_id, InstallState::Failed { error: "cancelled".into() }).await;
            return;
        };
        if let Err(error) = agent.room_for(agent.update_needs(&game_id)).await {
            tracing::warn!(game = %game_id, error = %error, "file check not started");
            installs.finish(&game_id, InstallState::Failed { error }).await;
            return;
        }
        let outcome = watched(&agent, &progress, agent.verify_content(&game_id, Some(&progress))).await;
        let state = match outcome {
            Ok(report) => {
                tracing::info!(game = %game_id, changed = report.changed_count, kept = ?report.repaired_version, "file check finished");
                InstallState::Done { bytes: 0, already_installed: true, update: None, verify: Some(Box::new(report)) }
            }
            Err(e) => {
                let error = progress.stop_reason().unwrap_or_else(|| format!("{e}"));
                tracing::warn!(game = %game_id, error = %error, "file check failed");
                InstallState::Failed { error }
            }
        };
        installs.finish(&game_id, state).await;
    });
    true
}

/// Run `work`, looking at the disk every [`DISK_WATCH_EVERY`]: once it reaches
/// the reserve the run is stopped the way a cancel stops it — its flag set,
/// its container removed — with the reason kept for its `Failed` state.
async fn watched<T>(
    agent: &crate::agent::Agent,
    progress: &InstallProgress,
    work: impl std::future::Future<Output = T>,
) -> T {
    tokio::pin!(work);
    let mut tick = tokio::time::interval(DISK_WATCH_EVERY);
    loop {
        tokio::select! {
            out = &mut work => return out,
            _ = tick.tick() => {
                if progress.is_cancelled() {
                    continue;
                }
                if let Some(reason) = agent.disk_reached_reserve() {
                    tracing::warn!(reason = %reason, "stopping a download at the disk reserve");
                    progress.stop(reason);
                    if let Some(name) = progress.task_container() {
                        let _ = agent.docker().remove_task(&name).await;
                    }
                }
            }
        }
    }
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
        assert!(installs.begin("sven-coop").await.is_some());
        assert!(installs.begin("sven-coop").await.is_none(), "a second claim must be refused");
        // A different game is unaffected.
        assert!(installs.begin("half-life").await.is_some());
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
        assert!(installs.begin("sven-coop").await.is_some(), "a failed install must be retryable");
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

    /// A delete holds the slot, so a download cannot start into a tree being
    /// removed, and a running download refuses the delete.
    #[tokio::test]
    async fn a_delete_and_an_install_never_overlap() {
        let installs = Installs::new();
        installs.begin("g").await;
        assert_eq!(installs.exclusive("g", async {}).await, Err(NotRunning));
        installs.finish("g", InstallState::Done { bytes: 1, already_installed: false, update: None, verify: None }).await;
        // While held, nothing reports a download that is not one, and a
        // download cannot start.
        let inside = installs
            .exclusive("g", async {
                assert!(installs.begin("g").await.is_none());
                (installs.state("g").await, installs.running().await)
            })
            .await;
        assert_eq!(inside, Ok((InstallState::Idle, Vec::new())));
        // Released with nothing remembered.
        assert_eq!(installs.state("g").await, InstallState::Idle);
    }

    /// Cancel reaches the run's shared flag, and is refused when nothing runs.
    #[tokio::test]
    async fn cancel_flags_the_running_install_only() {
        let installs = Installs::new();
        assert_eq!(installs.cancel("g").await, Err(NotRunning));
        let progress = installs.begin("g").await.unwrap();
        assert_eq!(installs.cancel("g").await, Ok(None));
        assert!(progress.is_cancelled());
    }

    /// One download at a time: a second game waits as `Queued`, runs once the
    /// first ends, and can be cancelled while it waits without waiting out
    /// the first.
    #[tokio::test]
    async fn a_second_download_waits_its_turn_and_can_be_cancelled_while_waiting() {
        let installs = Arc::new(Installs::new());
        let a = installs.begin("a").await.unwrap();
        let turn_a = installs.take_turn("a", &a).await.unwrap();
        assert!(matches!(installs.state("a").await, InstallState::Running { .. }));

        let b = installs.begin("b").await.unwrap();
        let waiting = {
            let (installs, b) = (installs.clone(), b.clone());
            tokio::spawn(async move { installs.take_turn("b", &b).await.is_some() })
        };
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
        assert!(matches!(installs.state("b").await, InstallState::Queued { .. }));
        assert_eq!(installs.running().await, vec!["a".to_string(), "b".to_string()], "a queued one is listed");
        assert!(installs.begin("b").await.is_none(), "a queued game's slot is taken");

        // Cancelled while it waits: it gives up without the first finishing.
        installs.cancel("b").await.unwrap();
        let got = tokio::time::timeout(std::time::Duration::from_secs(2), waiting).await.unwrap().unwrap();
        assert!(!got, "a cancelled download must not take a turn");

        // And the next one runs once the first lets go.
        installs.finish("b", InstallState::Failed { error: "cancelled".into() }).await;
        let c = installs.begin("c").await.unwrap();
        drop(turn_a);
        let turn_c = tokio::time::timeout(std::time::Duration::from_secs(2), installs.take_turn("c", &c)).await.unwrap();
        assert!(turn_c.is_some());
        assert!(matches!(installs.state("c").await, InstallState::Running { .. }));
    }

    /// No driver knows a download's total, so none is reported: a progress
    /// bar with an invented end is one that lies.
    #[tokio::test]
    async fn a_running_install_reports_no_total_it_does_not_know() {
        let installs = Installs::new();
        let progress = installs.begin("g").await.unwrap();
        let _turn = installs.take_turn("g", &progress).await.unwrap();
        let json = serde_json::to_value(installs.state("g").await).unwrap();
        assert_eq!(json["state"], "running");
        assert_eq!(json["bytes"], 0);
        assert!(json.get("total_bytes").is_none(), "{json}");
    }
}
