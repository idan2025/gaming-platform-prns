//! A pack repository (`PLAN.md` §13.3 steps 5 and 6, ROADMAP item 8).
//!
//! A repository is a static directory: game packs, the signatures beside
//! them, and an `index.json` this module writes (`game-bridge repo build`)
//! and a launcher reads. Any web host serves one; nothing here runs on a
//! server.
//!
//! # What the index is, and is not
//!
//! The index is a **cache of packs**, the way an index server is a cache of
//! the mesh (`DESIGN.md` §0): it says which packs are there and what their
//! bytes hash to, so a launcher can list them without fetching each one. It
//! is never what decides trust. That is each pack's own detached signature
//! (`signing.rs`), checked by the launcher after download exactly as at load,
//! so a repository cannot vouch for a pack its author did not sign.
//!
//! Rules a later change could quietly break:
//! - **The digest decides, not the URL.** A launcher hashes the fetched pack
//!   and refuses it if it is not the index's `sha256`, before it parses it.
//! - **A file name comes from the index only after it is checked** to be a
//!   bare `<name>.toml` with no path in it ([`valid_file_name`]); a launcher
//!   never writes it anywhere, it names the installed file from the pack's id.
//! - **A pack's id in the index must be the id inside the pack**, checked
//!   after download: an index cannot install one game under another's name.

use std::path::Path;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::signing::SIGNATURE_SUFFIX;
use crate::GamePack;

/// The file a repository's packs are listed in.
pub const INDEX_FILE: &str = "index.json";

/// The index format this build writes and reads.
pub const INDEX_FORMAT: u32 = 1;

/// Largest index a launcher reads: thousands of entries, and still small.
pub const MAX_INDEX_BYTES: usize = 1024 * 1024;

/// Largest pack a launcher fetches, as the node's import
/// (`platform-agent/src/pack_import.rs`).
pub const MAX_PACK_BYTES: usize = 256 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoIndex {
    pub format: u32,
    /// What the repository calls itself.
    pub name: String,
    pub packs: Vec<RepoEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoEntry {
    pub id: String,
    pub display_name: String,
    /// The pack's file, relative to the index: a bare `<name>.toml`.
    pub file: String,
    /// SHA-256 of the pack's bytes, lowercase hex.
    pub sha256: String,
    /// Whether `<file>.sig` is beside it.
    pub signed: bool,
    /// The pack's art URL, already held to `GamePack`'s host allowlist.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub art: Option<String>,
    /// Whether the pack offers LAN rooms.
    #[serde(default)]
    pub lan: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RepoError {
    NotJson(String),
    UnsupportedFormat(u32),
    BadEntry { id: String, why: &'static str },
}

impl core::fmt::Display for RepoError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::NotJson(e) => write!(f, "that is not a pack repository index: {e}"),
            Self::UnsupportedFormat(n) => write!(
                f,
                "the repository's index is format {n}; this Lanthorn reads format {INDEX_FORMAT}"
            ),
            Self::BadEntry { id, why } => write!(f, "the index's entry {id:?} {why}"),
        }
    }
}

impl std::error::Error for RepoError {}

/// A bare `<name>.toml`: letters, digits, `-`, `_` and `.`, no path.
pub fn valid_file_name(name: &str) -> bool {
    name.len() <= 128
        && name.ends_with(".toml")
        && name.len() > ".toml".len()
        && !name.starts_with('.')
        && name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        && !name.contains("..")
}

fn valid_sha256(hex: &str) -> bool {
    hex.len() == 64 && hex.chars().all(|c| matches!(c, '0'..='9' | 'a'..='f'))
}

/// A repository URL a launcher or node will fetch: `https`, or `http` to this
/// machine only (a repository being tested). No query or fragment, so the
/// packs beside the index are found by plain path.
pub fn check_repo_url(url: &str) -> anyhow::Result<String> {
    let url = url.trim();
    let local =
        ["http://localhost:", "http://localhost/", "http://127.0.0.1:", "http://127.0.0.1/"];
    if !(url.starts_with("https://") || local.iter().any(|p| url.starts_with(p))) {
        anyhow::bail!("a pack repository is an https:// address (http:// only for this computer)");
    }
    if url.contains(['?', '#', ' ', '\\']) || url.len() > 512 {
        anyhow::bail!("a pack repository address has no query, fragment or spaces");
    }
    let url = if url.ends_with(INDEX_FILE) {
        url.to_string()
    } else {
        format!("{}/{}", url.trim_end_matches('/'), INDEX_FILE)
    };
    Ok(url)
}

/// The address of `file` beside the index at `index_url`.
pub fn beside(index_url: &str, file: &str) -> String {
    let base = &index_url[..=index_url.rfind('/').unwrap_or(index_url.len() - 1)];
    format!("{base}{file}")
}

/// SHA-256 of `bytes`, lowercase hex.
pub fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

impl RepoIndex {
    /// Read an index, refusing one whose entries could not be fetched safely.
    pub fn parse(bytes: &[u8]) -> Result<Self, RepoError> {
        let index: Self =
            serde_json::from_slice(bytes).map_err(|e| RepoError::NotJson(e.to_string()))?;
        if index.format != INDEX_FORMAT {
            return Err(RepoError::UnsupportedFormat(index.format));
        }
        for e in &index.packs {
            let bad = |why| RepoError::BadEntry { id: e.id.clone(), why };
            if !valid_file_name(&e.file) {
                return Err(bad("names a file that is not a bare .toml name"));
            }
            if !valid_sha256(&e.sha256) {
                return Err(bad("has no usable sha256"));
            }
            if e.id.is_empty() || e.id.len() > 64 {
                return Err(bad("has no usable id"));
            }
        }
        Ok(index)
    }

    /// The entry for `id`.
    pub fn entry(&self, id: &str) -> Option<&RepoEntry> {
        self.packs.iter().find(|e| e.id == id)
    }

    /// Build the index for the packs in `dir`, as `game-bridge repo build`
    /// does. Every `.toml` must parse as a pack, and no two may share an id;
    /// a repository that lists a broken pack is worse than one that refuses
    /// to build.
    pub fn build(dir: &Path, name: &str) -> anyhow::Result<Self> {
        let mut packs = Vec::new();
        let mut files: Vec<_> = std::fs::read_dir(dir)?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("toml"))
            .collect();
        files.sort();
        for path in files {
            let file = path.file_name().and_then(|n| n.to_str()).unwrap_or_default().to_string();
            if !valid_file_name(&file) {
                anyhow::bail!(
                    "{file:?} is not a name a repository can list: use letters, digits, - and _"
                );
            }
            let bytes = std::fs::read(&path)?;
            if bytes.len() > MAX_PACK_BYTES {
                anyhow::bail!("{file} is over {MAX_PACK_BYTES} bytes; no launcher would fetch it");
            }
            let src = std::str::from_utf8(&bytes)
                .map_err(|_| anyhow::anyhow!("{file} is not UTF-8 text"))?;
            let pack = GamePack::parse(src)
                .map_err(|e| anyhow::anyhow!("{file} is not a usable pack: {e}"))?;
            if packs.iter().any(|p: &RepoEntry| p.id == pack.id) {
                anyhow::bail!("two packs have the id {:?}", pack.id);
            }
            let mut sig = path.clone().into_os_string();
            sig.push(SIGNATURE_SUFFIX);
            packs.push(RepoEntry {
                art: pack.art_url(),
                lan: pack.lan.is_some(),
                id: pack.id.clone(),
                display_name: pack.display_name.clone(),
                file,
                sha256: sha256_hex(&bytes),
                signed: Path::new(&sig).exists(),
            });
        }
        Ok(Self { format: INDEX_FORMAT, name: name.to_string(), packs })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SVEN: &str = include_str!("../../../packs/sven-coop.toml");

    #[test]
    fn an_index_built_from_a_directory_reads_back_and_names_its_digests() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("sven-coop.toml"), SVEN).unwrap();
        std::fs::write(dir.path().join("README.md"), "not a pack").unwrap();
        let index = RepoIndex::build(dir.path(), "Test packs").unwrap();
        assert_eq!(index.packs.len(), 1);
        let e = &index.packs[0];
        assert_eq!((e.id.as_str(), e.file.as_str()), ("sven-coop", "sven-coop.toml"));
        assert_eq!(e.sha256, sha256_hex(SVEN.as_bytes()));
        assert!(!e.signed);
        let back = RepoIndex::parse(&serde_json::to_vec(&index).unwrap()).unwrap();
        assert_eq!(back, index);
    }

    #[test]
    fn a_broken_pack_or_a_duplicate_id_stops_the_build() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.toml"), "id = ").unwrap();
        assert!(RepoIndex::build(dir.path(), "x").is_err());
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.toml"), SVEN).unwrap();
        std::fs::write(dir.path().join("b.toml"), SVEN).unwrap();
        assert!(RepoIndex::build(dir.path(), "x").unwrap_err().to_string().contains("two packs"));
    }

    /// An index is somebody else's file. A path in it, or a digest that is
    /// not one, is refused rather than followed.
    #[test]
    fn an_index_naming_a_path_or_no_digest_is_refused() {
        let entry = |file: &str, sha: &str| {
            format!(
                r#"{{"format":1,"name":"x","packs":[{{"id":"a","display_name":"A","file":"{file}","sha256":"{sha}","signed":false}}]}}"#
            )
        };
        let good = "a".repeat(64);
        assert!(RepoIndex::parse(entry("a.toml", &good).as_bytes()).is_ok());
        for file in
            ["../a.toml", "x/a.toml", "a.txt", ".toml", "..toml", "C:a.toml", ".hidden.toml"]
        {
            assert!(RepoIndex::parse(entry(file, &good).as_bytes()).is_err(), "{file}");
        }
        for sha in ["", "ABCD", &"g".repeat(64), &"A".repeat(64)] {
            assert!(RepoIndex::parse(entry("a.toml", sha).as_bytes()).is_err(), "{sha}");
        }
        assert!(matches!(
            RepoIndex::parse(br#"{"format":2,"name":"x","packs":[]}"#),
            Err(RepoError::UnsupportedFormat(2))
        ));
    }
}
