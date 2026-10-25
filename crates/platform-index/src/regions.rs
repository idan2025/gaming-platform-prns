//! Region move requests (`ROADMAP.md` §13).
//!
//! A server lives in the region of the worker it runs on, and the region it
//! was created in is its home. A move inside that region is the owner's to
//! make; a move to another region is asked for here and decided by an admin
//! of this index, because it takes room the operator plans region by region.
//! On approval the index moves the server to a worker with room in the new
//! region, which becomes its home: going back is another request.
//!
//! Requests are kept in a file beside the hosting file, written beside and
//! renamed, so an index restart forgets none. The store only records; whether
//! a request may be made, and the move itself, are `hosting.rs`'s.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RequestState {
    Pending,
    Approved,
    Denied,
    /// The owner took it back before an admin decided.
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegionRequest {
    /// Sixteen hex characters, made here.
    pub id: String,
    /// The server, as it is named now. A move inside the region gives a server
    /// a new id, and the request follows it (`RegionRequests::retarget`).
    pub instance_id: String,
    /// The account that owns the server and asked.
    pub owner: String,
    pub game_id: String,
    pub name: String,
    /// The region it was in when asked; `None` is a worker with no region.
    pub from_region: Option<String>,
    pub to_region: String,
    pub created_unix: u64,
    pub state: RequestState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decided_unix: Option<u64>,
    /// The admin who decided, by identity hash.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decided_by: Option<String>,
    /// Why it was denied, or why the last approval could not move it (the
    /// request stays pending then).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// The server's id after the approved move.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub moved_to: Option<String>,
}

/// Whether `id` is one this module would have made. Checked before an id from
/// a request is looked up or put in a path.
pub fn valid_request_id(id: &str) -> bool {
    id.len() == 16 && id.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

pub struct RegionRequests {
    /// `None` keeps them in memory only (tests, and an index run without a
    /// hosting file, which has no hosting to ask about anyway).
    path: Option<PathBuf>,
    list: Mutex<Vec<RegionRequest>>,
}

impl Default for RegionRequests {
    fn default() -> Self {
        Self::in_memory()
    }
}

impl RegionRequests {
    pub fn in_memory() -> Self {
        Self { path: None, list: Mutex::new(Vec::new()) }
    }

    /// Read the requests kept at `path`, or none if the file is not there yet.
    /// A file that does not parse is an error, never an empty list: starting
    /// with nothing would forget every pending request without a word.
    pub fn load(path: &Path) -> Result<Self> {
        let list = match std::fs::read_to_string(path) {
            Ok(text) => serde_json::from_str(&text).with_context(|| format!("reading {}", path.display()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        Ok(Self { path: Some(path.to_path_buf()), list: Mutex::new(list) })
    }

    fn save(&self, list: &[RegionRequest]) -> Result<()> {
        let Some(path) = &self.path else { return Ok(()) };
        let partial = path.with_extension("json.partial");
        std::fs::write(&partial, serde_json::to_vec_pretty(list)?)
            .with_context(|| format!("writing {}", partial.display()))?;
        std::fs::rename(&partial, path).with_context(|| format!("writing {}", path.display()))?;
        Ok(())
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<RegionRequest>> {
        self.list.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Record a new pending request. One pending request per server: a second
    /// is refused, so an admin never sees two answers to give for one server.
    pub fn create(&self, mut request: RegionRequest) -> Result<RegionRequest> {
        let mut list = self.lock();
        if list.iter().any(|r| r.instance_id == request.instance_id && r.state == RequestState::Pending) {
            return Err(anyhow!("this server already has a region move waiting for an admin"));
        }
        let mut bytes = [0u8; 8];
        getrandom::getrandom(&mut bytes).map_err(|e| anyhow!("no entropy: {e}"))?;
        request.id = hex::encode(bytes);
        request.state = RequestState::Pending;
        list.push(request.clone());
        self.save(&list)?;
        Ok(request)
    }

    /// Every request, newest first.
    pub fn all(&self) -> Vec<RegionRequest> {
        let mut list = self.lock().clone();
        list.sort_by(|a, b| b.created_unix.cmp(&a.created_unix));
        list
    }

    pub fn for_owner(&self, owner: &str) -> Vec<RegionRequest> {
        self.all().into_iter().filter(|r| r.owner == owner).collect()
    }

    pub fn get(&self, id: &str) -> Option<RegionRequest> {
        self.lock().iter().find(|r| r.id == id).cloned()
    }

    /// Change one pending request. Refused for one already decided: an
    /// approval and a denial must not both land.
    fn update_pending(&self, id: &str, change: impl FnOnce(&mut RegionRequest)) -> Result<RegionRequest> {
        let mut list = self.lock();
        let r = list.iter_mut().find(|r| r.id == id).ok_or_else(|| anyhow!("no such request"))?;
        if r.state != RequestState::Pending {
            return Err(anyhow!("that request was already decided"));
        }
        change(r);
        let out = r.clone();
        self.save(&list)?;
        Ok(out)
    }

    pub fn approve(&self, id: &str, by: &str, now: u64, moved_to: &str) -> Result<RegionRequest> {
        self.update_pending(id, |r| {
            r.state = RequestState::Approved;
            r.decided_by = Some(by.to_string());
            r.decided_unix = Some(now);
            r.moved_to = Some(moved_to.to_string());
            r.note = None;
        })
    }

    pub fn deny(&self, id: &str, by: &str, now: u64, note: Option<String>) -> Result<RegionRequest> {
        self.update_pending(id, |r| {
            r.state = RequestState::Denied;
            r.decided_by = Some(by.to_string());
            r.decided_unix = Some(now);
            r.note = note;
        })
    }

    /// An approval that could not move the server: it stays pending, and says why.
    pub fn note_failure(&self, id: &str, why: &str) -> Result<RegionRequest> {
        self.update_pending(id, |r| r.note = Some(why.to_string()))
    }

    /// The owner takes back a request nobody has decided yet. Anyone else's,
    /// or one that does not exist, is the same "no such request".
    pub fn cancel(&self, owner: &str, id: &str) -> Result<RegionRequest> {
        if self.get(id).is_none_or(|r| r.owner != owner) {
            return Err(anyhow!("no such request"));
        }
        self.update_pending(id, |r| r.state = RequestState::Cancelled)
    }

    /// An account's servers were erased: its own requests go, and where it
    /// decided someone else's, the request stays without its name. Returns how
    /// many requests went.
    pub fn forget(&self, identity: &str) -> Result<usize> {
        let mut list = self.lock();
        let before = list.len();
        list.retain(|r| !r.owner.eq_ignore_ascii_case(identity));
        let removed = before - list.len();
        let mut changed = removed > 0;
        for r in list.iter_mut() {
            if r.decided_by.as_deref().is_some_and(|by| by.eq_ignore_ascii_case(identity)) {
                r.decided_by = None;
                changed = true;
            }
        }
        if changed {
            self.save(&list)?;
        }
        Ok(removed)
    }

    /// A server moved and got a new id: a pending request for it follows.
    pub fn retarget(&self, old: &str, new: &str) -> Result<()> {
        let mut list = self.lock();
        let mut changed = false;
        for r in list.iter_mut().filter(|r| r.instance_id == old && r.state == RequestState::Pending) {
            r.instance_id = new.to_string();
            changed = true;
        }
        if changed {
            self.save(&list)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn asked(instance: &str, owner: &str) -> RegionRequest {
        RegionRequest {
            id: String::new(),
            instance_id: instance.into(),
            owner: owner.into(),
            game_id: "sven-coop".into(),
            name: "srv".into(),
            from_region: Some("il".into()),
            to_region: "eu".into(),
            created_unix: 1,
            state: RequestState::Pending,
            decided_unix: None,
            decided_by: None,
            note: None,
            moved_to: None,
        }
    }

    /// One pending request per server; a decided one cannot be decided again;
    /// only the owner can take theirs back; and the list survives a restart.
    #[test]
    fn a_request_is_decided_once_and_kept_across_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("region-requests.json");
        let store = RegionRequests::load(&path).unwrap();
        let r = store.create(asked("srv-1", "aa")).unwrap();
        assert!(valid_request_id(&r.id));
        assert!(store.create(asked("srv-1", "aa")).is_err(), "a second pending request for one server");

        store.retarget("srv-1", "srv-2").unwrap();
        assert_eq!(store.get(&r.id).unwrap().instance_id, "srv-2", "the request follows a move");

        store.note_failure(&r.id, "no worker in eu has room").unwrap();
        assert_eq!(store.get(&r.id).unwrap().state, RequestState::Pending);
        store.approve(&r.id, "admin", 5, "srv-3").unwrap();
        assert!(store.deny(&r.id, "admin", 6, None).is_err(), "decided twice");

        let again = RegionRequests::load(&path).unwrap();
        let kept = again.get(&r.id).unwrap();
        assert_eq!(kept.state, RequestState::Approved);
        assert_eq!(kept.moved_to.as_deref(), Some("srv-3"));
        assert!(kept.note.is_none(), "an approval clears the last failure");

        let other = again.create(asked("srv-9", "aa")).unwrap();
        assert!(again.cancel("bb", &other.id).is_err(), "someone else's request");
        assert_eq!(again.cancel("aa", &other.id).unwrap().state, RequestState::Cancelled);
        assert!(again.create(asked("srv-9", "aa")).is_ok(), "a cancelled request frees the server");
    }

    #[test]
    fn a_requests_file_that_does_not_parse_is_an_error_not_an_empty_list() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("region-requests.json");
        std::fs::write(&path, b"not json").unwrap();
        assert!(RegionRequests::load(&path).is_err());
        assert!(!valid_request_id("../x"));
        assert!(!valid_request_id("ABCDEF0123456789"));
    }
}
