//! Checking for a game's new build, and the disk reserve downloads keep
//! (`ROADMAP.md` §12.1 and §12.2).
//!
//! # The rules
//!
//! * **Checks are off until the operator sets an interval** (decided
//!   2026-10-13). Nothing new runs on a node by surprise; a check is a
//!   steamcmd container, and the operator chose that image.
//! * **"New" is decided from Steam's data on both sides**: the public branch's
//!   `buildid` from `app_info_print` (`content::public_build_id`) against the
//!   copy's own manifest (`content::steam_build_id`). A copy with no manifest
//!   is "unknown", never "up to date".
//! * **A snooze hides a notice; it never stops a check.** "Until a newer
//!   build" remembers the build it skipped, so the next one shows again.
//! * **The reserve is space nothing downloads into**: the larger of 10% of the
//!   disk and 5 GB unless the operator set one (decided 2026-10-13). An
//!   update starts only if it fits above it, and a download crossing it is
//!   stopped (`install.rs`).
//! * **What a found build does is per game, and "tell me" is the default**
//!   (§12.3). "Download it" runs the same update the button does, through the
//!   same queue and reserve gate, never removing the old copy: nothing
//!   automatic deletes. A snooze or a skipped build holds it back too, and a
//!   build it tried once is not tried again until the next check, so a
//!   download stopped at the reserve does not restart every minute.
//! * **Stored beside `content-active.json`, never in `agent.toml`**, which
//!   stays the operator's file and is never rewritten.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

const GIB: u64 = 1024 * 1024 * 1024;
/// The reserve's floor when the operator has not set one.
pub const DEFAULT_RESERVE_MIN_BYTES: u64 = 5 * GIB;
/// The reserve's share of the disk when the operator has not set one.
pub const DEFAULT_RESERVE_PERCENT: u64 = 10;
/// What an update is assumed to add on top of the copy it starts from. The
/// delta is not known before steamcmd runs; the check while it runs is what
/// actually protects the disk, so this only has to be a fair guess.
pub const UPDATE_DELTA_PERCENT: u64 = 10;
/// The longest check interval the page offers, in hours: 30 days.
pub const MAX_CHECK_EVERY_HOURS: u32 = 24 * 30;

/// The reserve on a disk of `total_bytes`, when the operator set none.
pub fn default_reserve(total_bytes: u64) -> u64 {
    (total_bytes / 100 * DEFAULT_RESERVE_PERCENT).max(DEFAULT_RESERVE_MIN_BYTES)
}

/// The space an update of a copy of `copy_bytes` is assumed to need: the
/// whole copy (an update copies first), plus the delta.
pub fn update_needs(copy_bytes: u64) -> u64 {
    copy_bytes.saturating_add(copy_bytes / 100 * UPDATE_DELTA_PERCENT)
}

/// Whether `need` more bytes fit on a disk with `free` bytes without going
/// below `reserve`.
pub fn fits(free: u64, need: u64, reserve: u64) -> bool {
    free >= need.saturating_add(reserve)
}

/// The operator's settings for checks and the reserve.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Settings {
    /// Hours between checks of each game. `None` is off, the default.
    #[serde(default)]
    pub check_every_hours: Option<u32>,
    /// Bytes downloads always leave free. `None` is the default reserve.
    #[serde(default)]
    pub reserve_bytes: Option<u64>,
}

impl Settings {
    pub fn validate(&self) -> Result<(), String> {
        match self.check_every_hours {
            Some(0) => {
                Err("a check interval is at least one hour; leave it empty to turn checks off"
                    .into())
            }
            Some(h) if h > MAX_CHECK_EVERY_HOURS => {
                Err(format!("a check interval is at most {MAX_CHECK_EVERY_HOURS} hours"))
            }
            _ => Ok(()),
        }
    }

    /// The reserve on a disk of `total_bytes`.
    pub fn reserve(&self, total_bytes: u64) -> u64 {
        self.reserve_bytes.unwrap_or_else(|| default_reserve(total_bytes))
    }
}

/// What a game does when a check finds a newer build (§12.3).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OnNewBuild {
    /// Show the notice; the operator presses Update.
    #[default]
    Notify,
    /// Download it in the background. Servers keep running on the old copy.
    Download,
}

/// When a game's servers move onto the copy new servers use (§12.4).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MoveServers {
    /// They stay on the copy they were created on until the operator acts.
    #[default]
    Never,
    /// As soon as nobody is playing. A server that cannot be asked is never
    /// empty.
    WhenEmpty,
    /// Inside a daily window, once nobody is playing, or with players on if
    /// the operator allowed a forced move.
    Window,
}

/// The operator's choices for one game. Kept apart from [`Settings`] so a
/// page saving the node-wide settings never resets a game's.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GamePolicy {
    #[serde(default)]
    pub on_new_build: OnNewBuild,
    #[serde(default)]
    pub move_servers: MoveServers,
    /// The window, in whole hours UTC: from `window_start_utc` up to, not
    /// including, `window_end_utc`; it may run past midnight (22 to 4).
    #[serde(default)]
    pub window_start_utc: u8,
    #[serde(default = "default_window_end")]
    pub window_end_utc: u8,
    /// Inside the window, move a server with players on after warning them in
    /// game. Off unless the operator turns it on (decided 2026-10-13).
    #[serde(default)]
    pub force_in_window: bool,
}

fn default_window_end() -> u8 {
    6
}

impl Default for GamePolicy {
    /// Tell, and never move: nothing happens to a game's servers until the
    /// operator says so. The window, once chosen, starts as 00:00 to 06:00.
    fn default() -> Self {
        Self {
            on_new_build: OnNewBuild::Notify,
            move_servers: MoveServers::Never,
            window_start_utc: 0,
            window_end_utc: default_window_end(),
            force_in_window: false,
        }
    }
}

/// A change to a game's choices; what is absent is kept.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct PolicyPatch {
    pub on_new_build: Option<OnNewBuild>,
    pub move_servers: Option<MoveServers>,
    pub window_start_utc: Option<u8>,
    pub window_end_utc: Option<u8>,
    pub force_in_window: Option<bool>,
}

impl GamePolicy {
    pub fn patched(mut self, patch: PolicyPatch) -> Result<Self, String> {
        if let Some(v) = patch.on_new_build {
            self.on_new_build = v;
        }
        if let Some(v) = patch.move_servers {
            self.move_servers = v;
        }
        if let Some(v) = patch.window_start_utc {
            self.window_start_utc = v;
        }
        if let Some(v) = patch.window_end_utc {
            self.window_end_utc = v;
        }
        if let Some(v) = patch.force_in_window {
            self.force_in_window = v;
        }
        if self.window_start_utc > 23 || self.window_end_utc > 23 {
            return Err("a window's hours are 0 to 23".into());
        }
        if self.window_start_utc == self.window_end_utc {
            return Err("a window has to be at least an hour long".into());
        }
        Ok(self)
    }

    /// Whether `utc_hour` is inside the window.
    pub fn in_window(&self, utc_hour: u8) -> bool {
        let (start, end) = (self.window_start_utc, self.window_end_utc);
        if start < end {
            (start..end).contains(&utc_hour)
        } else {
            utc_hour >= start || utc_hour < end
        }
    }

    /// What to do now with a running server that is not on the copy new
    /// servers use. `humans` is how many people are on it, bots left out, and
    /// `None` when the game could not be asked; `can_warn` is whether its game
    /// has a console to warn them on.
    pub fn move_decision(&self, humans: Option<u32>, utc_hour: u8, can_warn: bool) -> MoveDecision {
        let empty = humans == Some(0);
        let busy = || match humans {
            Some(n) => format!("{n} playing"),
            None => "it cannot be asked whether anyone is playing".to_string(),
        };
        match self.move_servers {
            MoveServers::Never => MoveDecision::Stay,
            MoveServers::WhenEmpty if empty => MoveDecision::Move,
            MoveServers::WhenEmpty => MoveDecision::Wait(format!("waiting for it to empty: {}", busy())),
            MoveServers::Window if !self.in_window(utc_hour) => MoveDecision::Wait(format!(
                "waiting for the window, {:02}:00 to {:02}:00 UTC",
                self.window_start_utc, self.window_end_utc
            )),
            MoveServers::Window if empty => MoveDecision::Move,
            MoveServers::Window if self.force_in_window && can_warn => MoveDecision::WarnThenMove,
            MoveServers::Window if self.force_in_window => MoveDecision::Wait(format!(
                "waiting for it to empty: {}. Its game has no console to warn players on, so it is never moved with them on",
                busy()
            )),
            MoveServers::Window => MoveDecision::Wait(format!("in the window, waiting for it to empty: {}", busy())),
        }
    }
}

/// What [`GamePolicy::move_decision`] says to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MoveDecision {
    /// The game's servers stay where they are.
    Stay,
    /// Not now, and why.
    Wait(String),
    Move,
    /// Tell the players, wait, then move.
    WarnThenMove,
}

/// What is remembered about one game's checks.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GameCheck {
    /// When the last check finished, in unix seconds, whether it worked or not.
    #[serde(default)]
    pub checked_unix: Option<i64>,
    /// The public build the last successful check saw.
    #[serde(default)]
    pub latest_build: Option<String>,
    /// Why the last check failed; cleared by the next one that works.
    #[serde(default)]
    pub error: Option<String>,
    /// The notice is hidden until then.
    #[serde(default)]
    pub snooze_until_unix: Option<i64>,
    /// The notice is hidden while this is the newest build.
    #[serde(default)]
    pub skip_build: Option<String>,
    /// The build an automatic download last started for, and when.
    #[serde(default)]
    pub auto_build: Option<String>,
    #[serde(default)]
    pub auto_unix: Option<i64>,
}

impl GameCheck {
    /// Whether a check is due at `now` with checks every `hours`.
    pub fn due(&self, now: i64, hours: u32) -> bool {
        match self.checked_unix {
            None => true,
            Some(at) => now.saturating_sub(at) >= i64::from(hours) * 3600,
        }
    }

    /// Whether the notice for this game is hidden at `now`.
    pub fn snoozed(&self, now: i64) -> bool {
        self.snooze_until_unix.is_some_and(|until| until > now)
            || (self.skip_build.is_some() && self.skip_build == self.latest_build)
    }

    /// Whether an automatic download of the newest build may start at `now`:
    /// not hidden, and not already tried for this build since the last check.
    pub fn may_auto_download(&self, now: i64) -> bool {
        let Some(latest) = &self.latest_build else { return false };
        if self.snoozed(now) {
            return false;
        }
        match (&self.auto_build, self.auto_unix, self.checked_unix) {
            (Some(tried), Some(at), Some(checked)) if tried == latest => checked > at,
            (Some(tried), _, _) if tried == latest => false,
            _ => true,
        }
    }
}

/// One game's notice, for the Installed page.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateNotice {
    pub game_id: String,
    /// The copy new servers use, which is the one an update starts from.
    pub version: String,
    /// The build in that copy's own manifest, if it has one.
    pub installed_build: Option<String>,
    pub latest_build: Option<String>,
    /// `Some(true)` when Steam has a build this copy is not on; `None` when
    /// that cannot be told (never checked, or a copy without a manifest).
    pub available: Option<bool>,
    /// Whether the operator hid the notice for now.
    pub snoozed: bool,
    pub checked_unix: Option<i64>,
    pub error: Option<String>,
    pub snooze_until_unix: Option<i64>,
    pub skip_build: Option<String>,
    pub on_new_build: OnNewBuild,
    pub policy: GamePolicy,
}

impl UpdateNotice {
    pub fn new(
        game_id: &str,
        version: &str,
        installed_build: Option<String>,
        check: &GameCheck,
        policy: &GamePolicy,
        now: i64,
    ) -> Self {
        let available = match (&installed_build, &check.latest_build) {
            (Some(have), Some(latest)) => Some(have != latest),
            _ => None,
        };
        Self {
            game_id: game_id.to_string(),
            version: version.to_string(),
            installed_build,
            latest_build: check.latest_build.clone(),
            available,
            snoozed: check.snoozed(now),
            checked_unix: check.checked_unix,
            error: check.error.clone(),
            snooze_until_unix: check.snooze_until_unix,
            skip_build: check.skip_build.clone(),
            on_new_build: policy.on_new_build,
            policy: policy.clone(),
        }
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct State {
    #[serde(default)]
    settings: Settings,
    #[serde(default)]
    games: BTreeMap<String, GameCheck>,
    #[serde(default)]
    policies: BTreeMap<String, GamePolicy>,
}

pub struct UpdateChecks {
    path: PathBuf,
    state: std::sync::Mutex<State>,
}

impl UpdateChecks {
    pub const FILE: &'static str = "content-updates.json";

    /// Read the file under `data_root`. Missing or unreadable is the defaults:
    /// checks off and the default reserve, which is what a node had before.
    pub fn load(data_root: &Path) -> Self {
        let path = data_root.join(Self::FILE);
        let state = match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|e| {
                tracing::warn!(path = %path.display(), error = %e, "unreadable; update checks are off and the reserve is the default");
                State::default()
            }),
            Err(_) => State::default(),
        };
        Self { path, state: std::sync::Mutex::new(state) }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn settings(&self) -> Settings {
        self.lock().settings.clone()
    }

    pub fn set_settings(&self, settings: Settings) -> Result<(), String> {
        settings.validate()?;
        let mut state = self.lock();
        state.settings = settings;
        self.save(&state).map_err(|e| format!("saving the settings: {e}"))
    }

    pub fn game(&self, game_id: &str) -> GameCheck {
        self.lock().games.get(game_id).cloned().unwrap_or_default()
    }

    /// Remember how a check ended. A failure keeps the last build seen, so a
    /// notice already shown does not vanish because Steam did not answer once.
    pub fn record(
        &self,
        game_id: &str,
        now: i64,
        outcome: Result<String, String>,
    ) -> std::io::Result<()> {
        let mut state = self.lock();
        let entry = state.games.entry(game_id.to_string()).or_default();
        entry.checked_unix = Some(now);
        match outcome {
            Ok(build) => {
                entry.latest_build = Some(build);
                entry.error = None;
            }
            Err(e) => entry.error = Some(e),
        }
        self.save(&state)
    }

    pub fn policy(&self, game_id: &str) -> GamePolicy {
        self.lock().policies.get(game_id).cloned().unwrap_or_default()
    }

    pub fn set_policy(&self, game_id: &str, policy: GamePolicy) -> std::io::Result<()> {
        let mut state = self.lock();
        if policy == GamePolicy::default() {
            state.policies.remove(game_id);
        } else {
            state.policies.insert(game_id.to_string(), policy);
        }
        self.save(&state)
    }

    /// Remember that an automatic download of `build` started at `now`.
    pub fn record_auto(&self, game_id: &str, build: &str, now: i64) -> std::io::Result<()> {
        let mut state = self.lock();
        let entry = state.games.entry(game_id.to_string()).or_default();
        entry.auto_build = Some(build.to_string());
        entry.auto_unix = Some(now);
        self.save(&state)
    }

    /// Hide the notice until `until`, or while `skip_build` is the newest.
    pub fn snooze(
        &self,
        game_id: &str,
        until: Option<i64>,
        skip_build: Option<String>,
    ) -> std::io::Result<()> {
        let mut state = self.lock();
        let entry = state.games.entry(game_id.to_string()).or_default();
        entry.snooze_until_unix = until;
        entry.skip_build = skip_build;
        self.save(&state)
    }

    /// Written beside and renamed over, like `active.rs`.
    fn save(&self, state: &State) -> std::io::Result<()> {
        let tmp = self.path.with_extension("json.tmp");
        let bytes = serde_json::to_vec_pretty(state).map_err(std::io::Error::other)?;
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&tmp, bytes)?;
        std::fs::rename(&tmp, &self.path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_reserve_is_the_larger_of_a_tenth_and_five_gigabytes() {
        assert_eq!(default_reserve(20 * GIB), 5 * GIB, "a small disk keeps 5 GB");
        assert_eq!(default_reserve(200 * GIB), 20 * GIB, "a large one keeps a tenth");
        assert_eq!(
            Settings { reserve_bytes: Some(GIB), ..Default::default() }.reserve(200 * GIB),
            GIB
        );
    }

    #[test]
    fn an_update_needs_its_whole_copy_and_more() {
        assert_eq!(update_needs(1000 * 1024), 1100 * 1024);
        assert!(fits(10, 4, 6));
        assert!(!fits(10, 5, 6));
        assert!(!fits(0, u64::MAX, 1), "no overflow into a yes");
    }

    #[test]
    fn checks_are_off_until_an_interval_is_set() {
        assert_eq!(Settings::default().check_every_hours, None);
        assert!(Settings { check_every_hours: Some(0), ..Default::default() }.validate().is_err());
        assert!(Settings {
            check_every_hours: Some(MAX_CHECK_EVERY_HOURS + 1),
            ..Default::default()
        }
        .validate()
        .is_err());
        assert!(Settings { check_every_hours: Some(6), ..Default::default() }.validate().is_ok());
    }

    #[test]
    fn a_check_is_due_once_its_interval_has_passed() {
        let never = GameCheck::default();
        assert!(never.due(1_000, 6));
        let checked = GameCheck { checked_unix: Some(1_000), ..Default::default() };
        assert!(!checked.due(1_000 + 6 * 3600 - 1, 6));
        assert!(checked.due(1_000 + 6 * 3600, 6));
    }

    /// Skipping a build hides that build only; the next one shows again.
    #[test]
    fn skipping_a_build_hides_that_build_and_no_other() {
        let mut c = GameCheck {
            latest_build: Some("200".into()),
            skip_build: Some("200".into()),
            ..Default::default()
        };
        assert!(c.snoozed(0));
        c.latest_build = Some("300".into());
        assert!(!c.snoozed(0));
        let timed = GameCheck { snooze_until_unix: Some(50), ..Default::default() };
        assert!(timed.snoozed(49));
        assert!(!timed.snoozed(50));
    }

    /// A copy with no manifest cannot be compared, and is not "up to date".
    #[test]
    fn a_copy_without_a_manifest_is_unknown_never_up_to_date() {
        let check = GameCheck { latest_build: Some("200".into()), ..Default::default() };
        let p = GamePolicy::default();
        assert_eq!(UpdateNotice::new("g", "v", None, &check, &p, 0).available, None);
        assert_eq!(
            UpdateNotice::new("g", "v", Some("200".into()), &check, &p, 0).available,
            Some(false)
        );
        assert_eq!(
            UpdateNotice::new("g", "v", Some("100".into()), &check, &p, 0).available,
            Some(true)
        );
        let unchecked = GameCheck::default();
        assert_eq!(
            UpdateNotice::new("g", "v", Some("100".into()), &unchecked, &p, 0).available,
            None
        );
    }

    /// A failed check keeps the build already seen, and survives a restart.
    #[test]
    fn a_failed_check_keeps_the_last_build_seen() {
        let dir = tempfile::tempdir().unwrap();
        let u = UpdateChecks::load(dir.path());
        u.record("g", 10, Ok("200".into())).unwrap();
        u.record("g", 20, Err("Steam did not answer".into())).unwrap();
        let again = UpdateChecks::load(dir.path()).game("g");
        assert_eq!(again.latest_build.as_deref(), Some("200"));
        assert_eq!(again.error.as_deref(), Some("Steam did not answer"));
        assert_eq!(again.checked_unix, Some(20));
    }

    /// An automatic download tries a build once per check: one stopped at the
    /// reserve waits for the next check rather than restarting every minute.
    #[test]
    fn an_automatic_download_tries_a_build_once_per_check() {
        let mut c = GameCheck {
            checked_unix: Some(100),
            latest_build: Some("200".into()),
            ..Default::default()
        };
        assert!(c.may_auto_download(100));
        c.auto_build = Some("200".into());
        c.auto_unix = Some(110);
        assert!(!c.may_auto_download(120), "tried since the last check");
        c.checked_unix = Some(200);
        assert!(c.may_auto_download(200), "a later check allows another try");
        c.latest_build = Some("300".into());
        c.checked_unix = Some(100);
        assert!(c.may_auto_download(200), "a newer build is a new try");
        assert!(!GameCheck::default().may_auto_download(0), "nothing checked, nothing to fetch");
    }

    /// "Remind me later" and "skip this build" hold the download back too.
    #[test]
    fn a_snoozed_or_skipped_build_is_not_downloaded() {
        let skipped = GameCheck {
            latest_build: Some("200".into()),
            skip_build: Some("200".into()),
            ..Default::default()
        };
        assert!(!skipped.may_auto_download(0));
        let later = GameCheck {
            latest_build: Some("200".into()),
            snooze_until_unix: Some(50),
            ..Default::default()
        };
        assert!(!later.may_auto_download(10));
        assert!(later.may_auto_download(50));
    }

    /// The node-wide settings and a game's choice are saved apart, and the
    /// default ("tell me") is what a node had before.
    #[test]
    fn a_games_choice_survives_a_restart_and_defaults_to_notify() {
        let dir = tempfile::tempdir().unwrap();
        let u = UpdateChecks::load(dir.path());
        assert_eq!(u.policy("g").on_new_build, OnNewBuild::Notify);
        u.set_policy("g", GamePolicy { on_new_build: OnNewBuild::Download, ..Default::default() })
            .unwrap();
        u.set_settings(Settings { check_every_hours: Some(6), ..Default::default() }).unwrap();
        let again = UpdateChecks::load(dir.path());
        assert_eq!(again.policy("g").on_new_build, OnNewBuild::Download);
        assert_eq!(again.policy("other").on_new_build, OnNewBuild::Notify);
    }

    /// "When empty" never moves a server it cannot ask: silence is not zero.
    #[test]
    fn a_server_that_cannot_be_asked_is_never_empty() {
        let p = GamePolicy { move_servers: MoveServers::WhenEmpty, ..Default::default() };
        assert_eq!(p.move_decision(Some(0), 12, true), MoveDecision::Move);
        assert!(matches!(p.move_decision(None, 12, true), MoveDecision::Wait(_)));
        assert!(matches!(p.move_decision(Some(2), 12, true), MoveDecision::Wait(_)));
        assert_eq!(
            GamePolicy::default().move_decision(Some(0), 12, true),
            MoveDecision::Stay,
            "never is the default"
        );
    }

    /// A window waits for empty unless a forced move was allowed, and a forced
    /// move needs a console to warn the players on.
    #[test]
    fn a_window_forces_a_move_only_when_allowed_and_only_with_a_warning() {
        let p = GamePolicy {
            move_servers: MoveServers::Window,
            window_start_utc: 22,
            window_end_utc: 4,
            ..Default::default()
        };
        assert!(p.in_window(23) && p.in_window(0) && p.in_window(3));
        assert!(!p.in_window(4) && !p.in_window(12));
        assert!(
            matches!(p.move_decision(Some(0), 12, true), MoveDecision::Wait(_)),
            "outside the window"
        );
        assert_eq!(p.move_decision(Some(0), 1, true), MoveDecision::Move);
        assert!(
            matches!(p.move_decision(Some(3), 1, true), MoveDecision::Wait(_)),
            "no force by default"
        );
        let forced = GamePolicy { force_in_window: true, ..p.clone() };
        assert_eq!(forced.move_decision(Some(3), 1, true), MoveDecision::WarnThenMove);
        assert_eq!(forced.move_decision(None, 1, true), MoveDecision::WarnThenMove);
        assert!(
            matches!(forced.move_decision(Some(3), 1, false), MoveDecision::Wait(_)),
            "no console, no force"
        );
    }

    /// A patch changes what it names; a window must be a real one.
    #[test]
    fn a_patch_keeps_what_it_does_not_name() {
        let p = GamePolicy { on_new_build: OnNewBuild::Download, ..Default::default() };
        let moved = p
            .clone()
            .patched(PolicyPatch {
                move_servers: Some(MoveServers::WhenEmpty),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(moved.on_new_build, OnNewBuild::Download);
        assert_eq!(moved.move_servers, MoveServers::WhenEmpty);
        assert!(p
            .clone()
            .patched(PolicyPatch { window_end_utc: Some(24), ..Default::default() })
            .is_err());
        assert!(p
            .patched(PolicyPatch {
                window_start_utc: Some(6),
                window_end_utc: Some(6),
                ..Default::default()
            })
            .is_err());
    }
}
