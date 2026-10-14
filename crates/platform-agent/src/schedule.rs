//! A server's daily restart window (`ROADMAP.md` item 7).
//!
//! # The rules
//!
//! * **Once a day, inside the window, and only a running server.** A stopped
//!   server is never started by a schedule: a Stop is never undone, the same
//!   rule as the crash watch.
//! * **Nobody playing, or a warning first.** A server restarts when the game
//!   says no humans are on it; one that cannot be asked is never empty. With
//!   players on it restarts only if the operator allowed it, after `say` over
//!   the game's console; a game with no console waits for empty.
//! * **Kept beside the instance**, like restart-on-crash: a container's labels
//!   are fixed at create, and this can change later. Not on the uplink wire's
//!   `InstanceStatus`, which deployed peers decode.

use std::path::Path;

use serde::{Deserialize, Serialize};

/// The file in an instance's folder that holds its schedule.
pub const SCHEDULE_FILE: &str = "restart-schedule.json";

/// When a server restarts each day: from `start_utc` up to, not including,
/// `end_utc`, in whole hours UTC; it may run past midnight (22 to 4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RestartSchedule {
    pub start_utc: u8,
    pub end_utc: u8,
    /// Restart with players on, after warning them in game.
    #[serde(default)]
    pub with_players: bool,
}

/// What a schedule says to do now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RestartDecision {
    /// Outside the window, or already restarted in this one.
    NotNow,
    /// In the window, but not yet, and why.
    Wait(String),
    Restart,
    WarnThenRestart,
}

impl RestartSchedule {
    pub fn validate(&self) -> Result<(), String> {
        if self.start_utc > 23 || self.end_utc > 23 {
            return Err("a window's hours are 0 to 23".into());
        }
        if self.start_utc == self.end_utc {
            return Err("a window has to be at least an hour long".into());
        }
        Ok(())
    }

    /// When the window `now` falls in began, in unix seconds, or `None`
    /// outside the window.
    pub fn window_start(&self, now: i64) -> Option<i64> {
        let day = now - now.rem_euclid(86_400);
        let hour = (now.rem_euclid(86_400) / 3600) as u8;
        let start = day + i64::from(self.start_utc) * 3600;
        if self.start_utc < self.end_utc {
            (self.start_utc..self.end_utc).contains(&hour).then_some(start)
        } else if hour >= self.start_utc {
            Some(start)
        } else if hour < self.end_utc {
            Some(start - 86_400)
        } else {
            None
        }
    }

    /// What to do at `now` with a running server last restarted by this
    /// schedule at `last`, with `humans` people on it (`None`: could not be
    /// asked) and `can_warn` when its game has a console.
    pub fn decision(
        &self,
        humans: Option<u32>,
        now: i64,
        can_warn: bool,
        last: Option<i64>,
    ) -> RestartDecision {
        let Some(began) = self.window_start(now) else { return RestartDecision::NotNow };
        if last.is_some_and(|at| at >= began) {
            return RestartDecision::NotNow;
        }
        let busy = match humans {
            Some(0) => return RestartDecision::Restart,
            Some(n) => format!("{n} playing"),
            None => "it cannot be asked whether anyone is playing".to_string(),
        };
        if self.with_players && can_warn {
            RestartDecision::WarnThenRestart
        } else if self.with_players {
            RestartDecision::Wait(format!(
                "waiting for it to empty: {busy}. Its game has no console to warn players on, so it never restarts with them on"
            ))
        } else {
            RestartDecision::Wait(format!("waiting for it to empty: {busy}"))
        }
    }
}

/// A schedule as stored, with when it last restarted the server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stored {
    pub schedule: RestartSchedule,
    #[serde(default)]
    pub last_unix: Option<i64>,
}

/// Read the schedule in an instance folder. Missing or unreadable is none.
pub fn load(instance_dir: &Path) -> Option<Stored> {
    let bytes = std::fs::read(instance_dir.join(SCHEDULE_FILE)).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Write it beside and rename over, like the other files the agent keeps;
/// `None` removes it.
pub fn save(instance_dir: &Path, stored: Option<&Stored>) -> std::io::Result<()> {
    let path = instance_dir.join(SCHEDULE_FILE);
    match stored {
        None => match std::fs::remove_file(&path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        },
        Some(stored) => {
            std::fs::create_dir_all(instance_dir)?;
            let tmp = path.with_extension("json.tmp");
            std::fs::write(
                &tmp,
                serde_json::to_vec_pretty(stored).map_err(std::io::Error::other)?,
            )?;
            std::fs::rename(&tmp, &path)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: i64 = 86_400;
    const H: i64 = 3600;

    #[test]
    fn a_window_past_midnight_began_the_day_before() {
        let s = RestartSchedule { start_utc: 22, end_utc: 4, with_players: false };
        let day = 100 * DAY;
        assert_eq!(s.window_start(day + 23 * H), Some(day + 22 * H));
        assert_eq!(
            s.window_start(day + 3 * H),
            Some(day - DAY + 22 * H),
            "03:00 belongs to last night's window"
        );
        assert_eq!(s.window_start(day + 4 * H), None);
        assert_eq!(s.window_start(day + 12 * H), None);
        let plain = RestartSchedule { start_utc: 4, end_utc: 6, with_players: false };
        assert_eq!(plain.window_start(day + 5 * H + 59), Some(day + 4 * H));
        assert_eq!(plain.window_start(day + 6 * H), None);
    }

    /// Once a window, never outside it, and silence is not empty.
    #[test]
    fn a_server_restarts_once_a_window_and_only_when_empty_unless_allowed() {
        let s = RestartSchedule { start_utc: 4, end_utc: 6, with_players: false };
        let day = 100 * DAY;
        let now = day + 4 * H + 60;
        assert_eq!(s.decision(Some(0), day + 12 * H, true, None), RestartDecision::NotNow);
        assert_eq!(s.decision(Some(0), now, true, None), RestartDecision::Restart);
        assert_eq!(
            s.decision(Some(0), now, true, Some(day + 4 * H + 10)),
            RestartDecision::NotNow,
            "done today"
        );
        assert_eq!(
            s.decision(Some(0), now, true, Some(day - DAY + 4 * H)),
            RestartDecision::Restart,
            "yesterday's"
        );
        assert!(
            matches!(s.decision(None, now, true, None), RestartDecision::Wait(_)),
            "silence is not empty"
        );
        assert!(matches!(s.decision(Some(3), now, true, None), RestartDecision::Wait(_)));
        let forced = RestartSchedule { with_players: true, ..s };
        assert_eq!(forced.decision(Some(3), now, true, None), RestartDecision::WarnThenRestart);
        assert!(
            matches!(forced.decision(Some(3), now, false, None), RestartDecision::Wait(_)),
            "no console, no force"
        );
    }

    #[test]
    fn a_schedule_survives_and_can_be_cleared() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(load(dir.path()), None);
        let stored = Stored {
            schedule: RestartSchedule { start_utc: 4, end_utc: 6, with_players: true },
            last_unix: Some(5),
        };
        save(dir.path(), Some(&stored)).unwrap();
        assert_eq!(load(dir.path()), Some(stored));
        save(dir.path(), None).unwrap();
        save(dir.path(), None).unwrap();
        assert_eq!(load(dir.path()), None);
        assert!(RestartSchedule { start_utc: 4, end_utc: 4, with_players: false }
            .validate()
            .is_err());
        assert!(RestartSchedule { start_utc: 24, end_utc: 4, with_players: false }
            .validate()
            .is_err());
    }
}
