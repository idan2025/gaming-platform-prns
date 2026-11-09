//! What happens on a node, kept for the admin dashboard: sign-ins and failed
//! sign-ins, lockouts, refused browser requests, admin actions, crashes and the
//! stops the host makes for disk limits. Append-only JSON lines in the node's
//! data folder, kept for `RETENTION_DAYS`, and a periodic sample of the node's
//! capacity (`ops-metrics.jsonl`).
//!
//! What goes in is data about the event, never a secret: a sign-in records the
//! account name, never a password or a code; an address is kept only in the
//! truncated form `addr_prefix` gives. Nothing here decides anything: the log is
//! read, never a source of a decision (a lockout, a refusal) on its own.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// How long events are kept, in days.
pub const RETENTION_DAYS: u64 = 90;
/// How long metric samples are kept, in days.
pub const METRICS_RETENTION_DAYS: u64 = 30;
/// Rewrite the files to drop what is past retention this often at most.
const PRUNE_EVERY: Duration = Duration::from_secs(3600);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpsEvent {
    pub unix: u64,
    /// `sign_in`, `sign_in_failed`, `sign_in_locked`, `request_refused`,
    /// `account_added`, `account_removed`, `server_crashed`,
    /// `server_stopped_host`, `admin_action`.
    pub kind: String,
    /// Who did it, when someone did (an account name, `token`, or `index`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor: Option<String>,
    /// The server, account or request it is about, as text.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub subject: String,
    /// What was refused or why, in words (never a secret).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub detail: String,
    /// The client's address, truncated (`addr_prefix`), when the node knows it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub addr: Option<String>,
}

/// One sample of the node's capacity, taken on a timer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct MetricSample {
    pub unix: u64,
    pub running: u32,
    pub stopped: u32,
    pub players: u32,
    pub max_instances: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_available_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disk_free_bytes: Option<u64>,
}

/// The address with its host part cut: `203.0.113.77` becomes `203.0.113.0/24`,
/// an IPv6 address keeps its first 48 bits. The log says where roughly, not who.
pub fn addr_prefix(addr: &str) -> String {
    if let Ok(v4) = addr.parse::<std::net::Ipv4Addr>() {
        let o = v4.octets();
        return format!("{}.{}.{}.0/24", o[0], o[1], o[2]);
    }
    if let Ok(v6) = addr.parse::<std::net::Ipv6Addr>() {
        let s = v6.segments();
        return format!("{:x}:{:x}:{:x}::/48", s[0], s[1], s[2]);
    }
    "unknown".to_string()
}

pub fn now_unix() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// A JSON-lines file, appended under a lock, pruned past its retention.
struct Lines<T> {
    path: PathBuf,
    keep_secs: u64,
    last_prune: Mutex<Option<u64>>,
    _t: std::marker::PhantomData<T>,
}

pub trait Timed {
    fn unix(&self) -> u64;
}

impl Timed for OpsEvent {
    fn unix(&self) -> u64 {
        self.unix
    }
}

impl Timed for MetricSample {
    fn unix(&self) -> u64 {
        self.unix
    }
}

impl<T: Serialize + for<'de> Deserialize<'de> + Timed + Clone> Lines<T> {
    fn new(path: PathBuf, keep_days: u64) -> Self {
        Self { path, keep_secs: keep_days * 86_400, last_prune: Mutex::new(None), _t: std::marker::PhantomData }
    }

    fn append(&self, item: &T) -> Result<()> {
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        let mut line = serde_json::to_vec(item)?;
        line.push(b'\n');
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .with_context(|| format!("opening {}", self.path.display()))?;
        f.write_all(&line).with_context(|| format!("writing {}", self.path.display()))?;
        self.maybe_prune(now_unix())
    }

    /// Drop what is past retention, at most once an hour. A line that does not
    /// parse is kept as it is: it is someone's data, not ours to throw away.
    fn maybe_prune(&self, now: u64) -> Result<()> {
        let mut last = self.last_prune.lock().unwrap_or_else(|e| e.into_inner());
        if last.is_some_and(|t| now.saturating_sub(t) < PRUNE_EVERY.as_secs()) {
            return Ok(());
        }
        *last = Some(now);
        drop(last);
        let cutoff = now.saturating_sub(self.keep_secs);
        let Ok(file) = std::fs::File::open(&self.path) else { return Ok(()) };
        let mut kept = Vec::new();
        let mut dropped = false;
        for line in BufReader::new(file).lines() {
            let line = line?;
            let keep = match serde_json::from_str::<T>(&line) {
                Ok(item) => item.unix() >= cutoff,
                Err(_) => true,
            };
            if keep {
                kept.push(line);
            } else {
                dropped = true;
            }
        }
        if dropped {
            let partial = self.path.with_extension("jsonl.partial");
            let mut out = kept.join("\n");
            if !out.is_empty() {
                out.push('\n');
            }
            std::fs::write(&partial, out).with_context(|| format!("writing {}", partial.display()))?;
            std::fs::rename(&partial, &self.path).with_context(|| format!("replacing {}", self.path.display()))?;
        }
        Ok(())
    }

    /// Items at or after `since`, oldest first, at most `limit` of the newest.
    fn read(&self, since: u64, limit: usize) -> Vec<T> {
        let Ok(file) = std::fs::File::open(&self.path) else { return Vec::new() };
        let mut out: Vec<T> = BufReader::new(file)
            .lines()
            .map_while(Result::ok)
            .filter_map(|l| serde_json::from_str::<T>(&l).ok())
            .filter(|t| t.unix() >= since)
            .collect();
        if out.len() > limit {
            out.drain(..out.len() - limit);
        }
        out
    }
}

/// The node's own ops log and metric samples.
pub struct OpsLog {
    events: Lines<OpsEvent>,
    metrics: Lines<MetricSample>,
}

impl OpsLog {
    pub fn new(data_root: &Path) -> Self {
        Self {
            events: Lines::new(data_root.join("ops-events.jsonl"), RETENTION_DAYS),
            metrics: Lines::new(data_root.join("ops-metrics.jsonl"), METRICS_RETENTION_DAYS),
        }
    }

    /// Record one event. A log that cannot be written is said in the node's
    /// own log and never stops the thing that happened.
    pub fn record(&self, kind: &str, actor: Option<&str>, subject: &str, detail: &str, addr: Option<&str>) {
        let event = OpsEvent {
            unix: now_unix(),
            kind: kind.to_string(),
            actor: actor.map(str::to_string),
            subject: subject.chars().take(200).collect(),
            detail: detail.chars().take(300).collect(),
            addr: addr.map(addr_prefix),
        };
        if let Err(e) = self.events.append(&event) {
            tracing::warn!(error = %format!("{e:#}"), "the ops log could not be written");
        }
    }

    pub fn events_since(&self, since: u64, limit: usize) -> Vec<OpsEvent> {
        self.events.read(since, limit)
    }

    pub fn sample(&self, sample: &MetricSample) {
        if let Err(e) = self.metrics.append(sample) {
            tracing::warn!(error = %format!("{e:#}"), "the metric sample could not be written");
        }
    }

    pub fn samples_since(&self, since: u64, limit: usize) -> Vec<MetricSample> {
        self.metrics.read(since, limit)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_address_is_kept_only_as_its_network() {
        assert_eq!(addr_prefix("203.0.113.77"), "203.0.113.0/24");
        assert_eq!(addr_prefix("2001:db8:1:2::9"), "2001:db8:1::/48");
        assert_eq!(addr_prefix("not an address"), "unknown");
    }

    #[test]
    fn events_are_read_back_in_order_and_past_retention_is_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let log = OpsLog::new(dir.path());
        log.record("sign_in_failed", None, "ana", "wrong username or password", Some("203.0.113.77"));
        log.record("sign_in", Some("ana"), "", "", None);
        let all = log.events_since(0, 100);
        assert_eq!(all.iter().map(|e| e.kind.as_str()).collect::<Vec<_>>(), ["sign_in_failed", "sign_in"]);
        assert_eq!(all[0].addr.as_deref(), Some("203.0.113.0/24"), "the address is never kept in full");
        assert_eq!(log.events_since(0, 1).len(), 1, "the newest is kept at a limit");

        // An old line is dropped on the next prune; a line that does not parse is kept.
        let path = dir.path().join("ops-events.jsonl");
        let old = serde_json::json!({"unix": 1, "kind": "sign_in"});
        let text = format!("{old}\nnot json at all\n{}\n", serde_json::to_string(&all[1]).unwrap());
        std::fs::write(&path, text).unwrap();
        log.events.maybe_prune(now_unix()).unwrap();
        let left = std::fs::read_to_string(&path).unwrap();
        assert!(!left.contains("\"unix\":1,"), "{left}");
        assert!(left.contains("not json at all"), "a line we cannot read is kept");
    }

    #[test]
    fn a_sample_is_read_back() {
        let dir = tempfile::tempdir().unwrap();
        let log = OpsLog::new(dir.path());
        log.sample(&MetricSample { unix: now_unix(), running: 2, stopped: 1, players: 5, max_instances: 6, ..Default::default() });
        let got = log.samples_since(0, 10);
        assert_eq!((got.len(), got[0].running, got[0].players), (1, 2, 5));
    }
}
