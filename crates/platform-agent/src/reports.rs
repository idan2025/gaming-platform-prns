//! Reports of illegal content or abuse on a hosted server, from anyone.
//!
//! A node that takes sign-ups (`[controller] open_signup`) hosts servers for
//! the public, and anyone, with or without an account, must be able to tell
//! its admins that one of them carries illegal content or breaks the rules.
//! A report is text only, held to small sizes and a rate across everyone,
//! stored in `reports.json` in the data root (owner-only, written beside and
//! renamed), and read and closed by the node's admins. What an admin then does
//! to a server is the index's (`platform-index` `notices.rs`), which tells the
//! owner why.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};

/// Reports taken in an hour, across everyone. Past it a report is refused, so
/// the file cannot be filled by a script.
pub const REPORTS_PER_HOUR: usize = 30;
/// Open reports held at once.
pub const MAX_OPEN: usize = 500;
pub const MAX_TARGET: usize = 200;
pub const MAX_DETAILS: usize = 2000;
pub const MAX_CONTACT: usize = 200;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Category {
    Illegal,
    Abuse,
    Copyright,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Report {
    pub id: String,
    pub category: Category,
    /// Which server: its name, address or anything that finds it.
    pub target: String,
    pub details: String,
    /// How to reach the reporter, if they gave one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contact: Option<String>,
    pub created_unix: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub closed_unix: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub closed_by: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<String>,
}

/// Plain text within `max`, line breaks allowed in `multiline`, no other
/// control characters. Refused, never trimmed to fit.
fn valid(text: &str, max: usize, multiline: bool) -> bool {
    text.chars().count() <= max
        && !text.chars().any(|c| c.is_control() && !(multiline && c == '\n'))
}

pub fn valid_report_id(id: &str) -> bool {
    id.len() == 16 && id.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

pub struct Reports {
    path: PathBuf,
    list: Mutex<Vec<Report>>,
    recent: Mutex<Vec<Instant>>,
}

impl Reports {
    /// A file that does not parse is an error, never an empty list: that would
    /// drop reports nobody has read.
    pub fn load(path: &Path) -> Result<Self> {
        let list = match std::fs::read_to_string(path) {
            Ok(text) => serde_json::from_str(&text)
                .with_context(|| format!("reading {}", path.display()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        Ok(Self {
            path: path.to_path_buf(),
            list: Mutex::new(list),
            recent: Mutex::new(Vec::new()),
        })
    }

    fn save(&self, list: &[Report]) -> Result<()> {
        let partial = self.path.with_extension("json.partial");
        {
            use std::io::Write;
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create(true).truncate(true);
            #[cfg(unix)]
            std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
            let mut file =
                options.open(&partial).with_context(|| format!("writing {}", partial.display()))?;
            file.write_all(&serde_json::to_vec_pretty(list)?)?;
        }
        std::fs::rename(&partial, &self.path)
            .with_context(|| format!("writing {}", self.path.display()))?;
        Ok(())
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<Report>> {
        self.list.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn file(
        &self,
        category: Category,
        target: &str,
        details: &str,
        contact: Option<&str>,
        now: u64,
    ) -> Result<Report> {
        let (target, details) = (target.trim(), details.trim());
        let contact = contact.map(str::trim).filter(|c| !c.is_empty());
        if target.is_empty() || !valid(target, MAX_TARGET, false) {
            return Err(anyhow!("say which server, in up to {MAX_TARGET} characters"));
        }
        if details.is_empty() || !valid(details, MAX_DETAILS, true) {
            return Err(anyhow!("say what is wrong, in up to {MAX_DETAILS} characters"));
        }
        if contact.is_some_and(|c| !valid(c, MAX_CONTACT, false)) {
            return Err(anyhow!("a contact is up to {MAX_CONTACT} characters"));
        }
        {
            let mut recent = self.recent.lock().unwrap_or_else(|e| e.into_inner());
            let hour_ago = Instant::now().checked_sub(Duration::from_secs(3600));
            recent.retain(|t| hour_ago.is_none_or(|h| *t > h));
            if recent.len() >= REPORTS_PER_HOUR {
                return Err(anyhow!("too many reports lately; try again in an hour"));
            }
            recent.push(Instant::now());
        }
        let mut list = self.lock();
        if list.iter().filter(|r| r.closed_unix.is_none()).count() >= MAX_OPEN {
            return Err(anyhow!("too many reports are waiting; try again later"));
        }
        let mut bytes = [0u8; 8];
        getrandom::getrandom(&mut bytes).map_err(|e| anyhow!("no entropy: {e}"))?;
        let report = Report {
            id: hex::encode(bytes),
            category,
            target: target.to_string(),
            details: details.to_string(),
            contact: contact.map(str::to_string),
            created_unix: now,
            closed_unix: None,
            closed_by: None,
            outcome: None,
        };
        list.push(report.clone());
        self.save(&list)?;
        Ok(report)
    }

    /// Every report, open ones first, newest first within each.
    pub fn all(&self) -> Vec<Report> {
        let mut list = self.lock().clone();
        list.sort_by(|a, b| {
            a.closed_unix
                .is_some()
                .cmp(&b.closed_unix.is_some())
                .then(b.created_unix.cmp(&a.created_unix))
        });
        list
    }

    /// An admin closes a report, saying what was done.
    pub fn close(&self, id: &str, by: &str, outcome: &str, now: u64) -> Result<Report> {
        let outcome = outcome.trim();
        if outcome.is_empty() || !valid(outcome, MAX_DETAILS, true) {
            return Err(anyhow!("say what was done, in up to {MAX_DETAILS} characters"));
        }
        let mut list = self.lock();
        let r = list.iter_mut().find(|r| r.id == id).ok_or_else(|| anyhow!("no such report"))?;
        if r.closed_unix.is_some() {
            return Err(anyhow!("that report is already closed"));
        }
        r.closed_unix = Some(now);
        r.closed_by = Some(by.to_string());
        r.outcome = Some(outcome.to_string());
        let out = r.clone();
        self.save(&list)?;
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A report is plain text within its sizes, refused otherwise; it is read
    /// back after a restart and closed once with what was done; and reports
    /// stop at the hourly limit across everyone.
    #[test]
    fn a_report_is_text_kept_until_an_admin_closes_it_and_reports_are_rationed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("reports.json");
        let reports = Reports::load(&path).unwrap();
        assert!(reports.file(Category::Illegal, "", "x", None, 1).is_err(), "no server named");
        assert!(reports.file(Category::Illegal, "srv", "", None, 1).is_err(), "nothing said");
        assert!(
            reports.file(Category::Illegal, "srv\u{7}", "x", None, 1).is_err(),
            "a control character"
        );
        assert!(reports
            .file(Category::Illegal, "srv", &"x".repeat(MAX_DETAILS + 1), None, 1)
            .is_err());
        let made = reports
            .file(Category::Abuse, "Beta a", "slurs in chat\nall evening", Some(" "), 1)
            .unwrap();
        assert_eq!(made.contact, None, "a blank contact is no contact");

        let again = Reports::load(&path).unwrap();
        assert_eq!(again.all(), vec![made.clone()]);
        assert!(
            again.close(&made.id, "admin", "", 2).is_err(),
            "closed without saying what was done"
        );
        again.close(&made.id, "admin", "server stopped, owner told", 2).unwrap();
        assert!(again.close(&made.id, "admin", "again", 3).is_err());

        for i in 1..REPORTS_PER_HOUR {
            reports.file(Category::Other, &format!("s{i}"), "x", None, 1).unwrap();
        }
        assert!(reports.file(Category::Other, "one more", "x", None, 1).is_err());
    }
}
