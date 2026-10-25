//! What an admin did to someone's server, and why, for its owner to read.
//!
//! An admin of the index can stop or remove any server, whoever owns it: the
//! way a hosting operator acts on a report or a breach of its rules. Every such
//! action records a notice for the owner, saying what was done, to which
//! server, when, and the admin's reason, and the owner can ask for it to be
//! looked at again. Accounts here have no email, so this list is how the
//! owner is told.
//!
//! Notices are kept in a file beside the hosting file, written beside and
//! renamed; a file that does not parse is an error, never an empty list,
//! which would lose what owners were told. The store only records: who may
//! act is `hosting.rs`'s.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};

/// The longest reason an admin may give, and the longest note an owner may
/// send back. Shown as text, never markup.
pub const MAX_TEXT: usize = 1000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Action {
    Stopped,
    Removed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Notice {
    /// Sixteen hex characters, made here.
    pub id: String,
    /// The server's owner, by identity hash: who reads it.
    pub owner: String,
    pub instance_id: String,
    pub name: String,
    pub action: Action,
    pub reason: String,
    pub created_unix: u64,
    /// When the owner asked for another look, and what they said.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review_asked_unix: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review_note: Option<String>,
}

/// Whether `text` may be a reason or a note: present, within [`MAX_TEXT`], and
/// with no control characters but line breaks. Refused, never trimmed to fit.
pub fn valid_text(text: &str) -> bool {
    let text = text.trim();
    !text.is_empty()
        && text.chars().count() <= MAX_TEXT
        && !text.chars().any(|c| c.is_control() && c != '\n')
}

pub fn valid_notice_id(id: &str) -> bool {
    id.len() == 16 && id.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

pub struct Notices {
    path: Option<PathBuf>,
    list: Mutex<Vec<Notice>>,
}

impl Default for Notices {
    fn default() -> Self {
        Self { path: None, list: Mutex::new(Vec::new()) }
    }
}

impl Notices {
    pub fn load(path: &Path) -> Result<Self> {
        let list = match std::fs::read_to_string(path) {
            Ok(text) => serde_json::from_str(&text)
                .with_context(|| format!("reading {}", path.display()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        Ok(Self { path: Some(path.to_path_buf()), list: Mutex::new(list) })
    }

    fn save(&self, list: &[Notice]) -> Result<()> {
        let Some(path) = &self.path else { return Ok(()) };
        let partial = path.with_extension("json.partial");
        std::fs::write(&partial, serde_json::to_vec_pretty(list)?)
            .with_context(|| format!("writing {}", partial.display()))?;
        std::fs::rename(&partial, path).with_context(|| format!("writing {}", path.display()))?;
        Ok(())
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<Notice>> {
        self.list.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn record(&self, mut notice: Notice) -> Result<Notice> {
        let mut bytes = [0u8; 8];
        getrandom::getrandom(&mut bytes).map_err(|e| anyhow!("no entropy: {e}"))?;
        notice.id = hex::encode(bytes);
        let mut list = self.lock();
        list.push(notice.clone());
        self.save(&list)?;
        Ok(notice)
    }

    /// Every notice, newest first.
    pub fn all(&self) -> Vec<Notice> {
        let mut list = self.lock().clone();
        list.sort_by(|a, b| b.created_unix.cmp(&a.created_unix));
        list
    }

    pub fn for_owner(&self, owner: &str) -> Vec<Notice> {
        self.all().into_iter().filter(|n| n.owner.eq_ignore_ascii_case(owner)).collect()
    }

    /// The owner asks for another look, once. Anyone else's notice, or one
    /// that does not exist, is the same "no such notice".
    pub fn ask_review(&self, owner: &str, id: &str, note: &str, now: u64) -> Result<Notice> {
        let mut list = self.lock();
        let n = list
            .iter_mut()
            .find(|n| n.id == id && n.owner.eq_ignore_ascii_case(owner))
            .ok_or_else(|| anyhow!("no such notice"))?;
        if n.review_asked_unix.is_some() {
            return Err(anyhow!("you already asked for this to be looked at again"));
        }
        n.review_asked_unix = Some(now);
        n.review_note = Some(note.trim().to_string());
        let out = n.clone();
        self.save(&list)?;
        Ok(out)
    }

    /// An account asked to be forgotten: its notices go.
    pub fn forget(&self, owner: &str) -> Result<usize> {
        let mut list = self.lock();
        let before = list.len();
        list.retain(|n| !n.owner.eq_ignore_ascii_case(owner));
        let removed = before - list.len();
        if removed > 0 {
            self.save(&list)?;
        }
        Ok(removed)
    }
}
