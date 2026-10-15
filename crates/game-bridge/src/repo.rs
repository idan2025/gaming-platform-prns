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

/// The official pack repository: `packs/` of the project's own repository,
/// published by `.github/workflows/packs.yml` once a change has passed review
/// and signed with the project key ([`crate::signing::PROJECT_KEY_HEX`]).
pub const OFFICIAL_REPOSITORY: &str =
    "https://idan2025.github.io/gaming-platform-prns/packs/index.json";

/// A repository address a launcher or node will read: `https`, `http` to this
/// machine only (a repository being tested), or a folder on this machine — a
/// `file://` URL or a plain absolute path, for a mirror kept locally. No
/// query or fragment, so the packs beside the index are found by plain path.
/// Returns the index's own address.
pub fn check_repo_url(url: &str) -> anyhow::Result<String> {
    let url = url.trim();
    if url.len() > 512 || url.contains(['?', '#']) {
        anyhow::bail!("a pack repository address has no query or fragment");
    }
    let looks_local = url.starts_with("file://") || std::path::Path::new(url).is_absolute();
    if looks_local {
        let path = if url.starts_with("file://") {
            url::Url::parse(url)
                .ok()
                .and_then(|u| u.to_file_path().ok())
                .ok_or_else(|| anyhow::anyhow!("{url:?} is not a folder on this computer"))?
        } else {
            std::path::PathBuf::from(url)
        };
        let dir = if path.file_name().is_some_and(|n| n == INDEX_FILE) {
            path.parent().map(|p| p.to_path_buf()).unwrap_or(path)
        } else {
            path
        };
        let mut index = url::Url::from_directory_path(&dir)
            .map_err(|_| anyhow::anyhow!("{} is not an absolute folder", dir.display()))?;
        index.set_path(&format!("{}{INDEX_FILE}", index.path()));
        return Ok(index.to_string());
    }
    let local =
        ["http://localhost:", "http://localhost/", "http://127.0.0.1:", "http://127.0.0.1/"];
    if !(url.starts_with("https://") || local.iter().any(|p| url.starts_with(p))) {
        anyhow::bail!(
            "a pack repository is an https:// address, a folder on this computer, \
             or http:// to this computer"
        );
    }
    if url.contains([' ', '\\']) {
        anyhow::bail!("a pack repository address has no spaces");
    }
    let url = if url.ends_with(INDEX_FILE) {
        url.to_string()
    } else {
        format!("{}/{}", url.trim_end_matches('/'), INDEX_FILE)
    };
    Ok(url)
}

/// How long the official repository's pack signatures are made for
/// (`.github/workflows/packs.yml`). Long, because a signature is checked at
/// every load, offline too, and an expired one is an error that hides the
/// pack (`signing.rs`); the list's own signature is only checked online, so
/// it stays short. The publish renews both weekly.
pub const PACK_SIGNATURE_DAYS: u64 = 400;

/// A copy whose signature has less than this left is taken again from the
/// official repository, however unchanged its bytes, so a launcher or node
/// that syncs at all never meets an expired one.
pub const REFRESH_BEFORE_SECS: u64 = 200 * 86_400;

/// Whether a signature ending at `expires_at` is due for renewal.
pub fn signature_due(expires_at: Option<u64>, now: std::time::SystemTime) -> bool {
    let now = now.duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    expires_at.is_some_and(|t| t.saturating_sub(now) < REFRESH_BEFORE_SECS)
}

/// How long one fetch may take.
pub const FETCH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Largest signature file read.
pub const MAX_SIG_BYTES: usize = 16 * 1024;

/// Read `url` — `https`, `http` or `file` — refusing anything over `max`
/// bytes. `Ok(None)` is "not there": a 404, or no such file.
pub async fn fetch_optional(url: &str, max: usize) -> anyhow::Result<Option<Vec<u8>>> {
    use anyhow::Context;
    if url.starts_with("file://") {
        let path = url::Url::parse(url)
            .ok()
            .and_then(|u| u.to_file_path().ok())
            .ok_or_else(|| anyhow::anyhow!("{url:?} is not a file on this computer"))?;
        let meta = match tokio::fs::metadata(&path).await {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        if !meta.is_file() || meta.len() as usize > max {
            anyhow::bail!("{} is not a file of at most {max} bytes", path.display());
        }
        return Ok(Some(
            tokio::fs::read(&path).await.with_context(|| format!("reading {}", path.display()))?,
        ));
    }
    // The shell's updater also uses rustls with ring; whichever installs the
    // provider first wins, and the other's attempt is a harmless error.
    let _ = rustls::crypto::ring::default_provider().install_default();
    let client = reqwest::Client::builder().timeout(FETCH_TIMEOUT).build()?;
    let mut response = client.get(url).send().await.with_context(|| format!("fetching {url}"))?;
    if response.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    if !response.status().is_success() {
        anyhow::bail!("{url} answered {}", response.status());
    }
    if response.content_length().is_some_and(|n| n as usize > max) {
        anyhow::bail!("{url} is over {max} bytes");
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.with_context(|| format!("reading {url}"))? {
        body.extend_from_slice(&chunk);
        if body.len() > max {
            anyhow::bail!("{url} is over {max} bytes");
        }
    }
    Ok(Some(body))
}

/// As [`fetch_optional`], with "not there" an error.
pub async fn fetch(url: &str, max: usize) -> anyhow::Result<Vec<u8>> {
    fetch_optional(url, max).await?.ok_or_else(|| anyhow::anyhow!("{url} is not there"))
}

/// A repository's list as read: its exact bytes, parsed, and its signature
/// when there is one, already verified under the index domain. A signature
/// that is there and does not hold is an error, never an unsigned list.
pub struct ReadIndex {
    pub index: RepoIndex,
    pub bytes: Vec<u8>,
    /// Who signed it, and the signature file's exact text.
    pub signature: Option<(prns_core::identity::IdentityHash, String)>,
}

/// Read and check the list at `index_url` (from [`check_repo_url`]).
pub async fn read_index(index_url: &str) -> anyhow::Result<ReadIndex> {
    let bytes = fetch(index_url, MAX_INDEX_BYTES).await?;
    let index = RepoIndex::parse(&bytes)?;
    let sig_url = format!("{index_url}{SIGNATURE_SUFFIX}");
    let signature = match fetch_optional(&sig_url, MAX_SIG_BYTES).await? {
        None => None,
        Some(sig) => {
            let text = String::from_utf8(sig)
                .map_err(|_| anyhow::anyhow!("the list's signature is not text"))?;
            let parsed = crate::signing::PackSignature::parse(&text)
                .map_err(|e| anyhow::anyhow!("the list's signature is unreadable: {e}"))?;
            let who = parsed.verify_index(&bytes, std::time::SystemTime::now()).map_err(|e| {
                anyhow::anyhow!("the repository's list has a signature that does not hold: {e}")
            })?;
            Some((who, text))
        }
    };
    Ok(ReadIndex { index, bytes, signature })
}

/// What [`mirror`] copied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MirrorReport {
    pub name: String,
    pub packs: usize,
    /// Packs whose bytes differ from the copy this replaced, or are new.
    pub changed: Vec<String>,
}

/// Copy the repository at `source` into the folder `dest`, **exactly**.
///
/// A mirror is one way and byte for byte: the list must be signed by `key`
/// (the project key for the official repository), every pack's bytes must
/// be the list's digest and every pack signature must hold, and what lands is
/// those same files and nothing else — the list and its signature unchanged,
/// so whoever reads the mirror checks it against `key` exactly as they would
/// the source. Editing a mirrored file breaks a signature; adding one is not
/// in the signed list. Nothing here ever writes to `source`.
///
/// The copy is built beside `dest` and swapped in, so a reader never sees a
/// half-updated mirror, and a failed run leaves the last good copy.
pub async fn mirror(
    source: &str,
    dest: &Path,
    key: prns_core::identity::IdentityHash,
) -> anyhow::Result<MirrorReport> {
    use anyhow::Context;
    let index_url = check_repo_url(source)?;
    let read = read_index(&index_url).await?;
    match &read.signature {
        Some((who, _)) if *who == key => {}
        Some((who, _)) => anyhow::bail!(
            "the list is signed by {}, not {}: refusing to mirror it",
            hex::encode(who.as_bytes()),
            hex::encode(key.as_bytes())
        ),
        None => anyhow::bail!("the list is not signed: only a signed repository can be mirrored exactly"),
    }
    let now = std::time::SystemTime::now();
    let mut files: Vec<(String, Vec<u8>)> = Vec::new();
    for e in &read.index.packs {
        let bytes = fetch(&beside(&index_url, &e.file), MAX_PACK_BYTES).await?;
        if sha256_hex(&bytes) != e.sha256 {
            anyhow::bail!("{} is not the bytes the list names; nothing was changed", e.file);
        }
        let src = std::str::from_utf8(&bytes)
            .map_err(|_| anyhow::anyhow!("{} is not UTF-8 text", e.file))?;
        let pack = GamePack::parse(src).map_err(|err| anyhow::anyhow!("{} is not a pack: {err}", e.file))?;
        if pack.id != e.id {
            anyhow::bail!("the list names {:?} but {} is {:?}", e.id, e.file, pack.id);
        }
        if e.signed {
            let sig_name = format!("{}{SIGNATURE_SUFFIX}", e.file);
            let text = String::from_utf8(fetch(&beside(&index_url, &sig_name), MAX_SIG_BYTES).await?)
                .map_err(|_| anyhow::anyhow!("{sig_name} is not text"))?;
            crate::signing::PackSignature::parse(&text)
                .map_err(|err| anyhow::anyhow!("{sig_name} is unreadable: {err}"))?
                .verify(&bytes, now)
                .map_err(|err| anyhow::anyhow!("{sig_name} does not hold: {err}"))?;
            files.push((sig_name, text.into_bytes()));
        }
        files.push((e.file.clone(), bytes));
    }
    let (_, sig_text) = read.signature.clone().expect("checked above");
    files.push((INDEX_FILE.to_string(), read.bytes.clone()));
    files.push((format!("{INDEX_FILE}{SIGNATURE_SUFFIX}"), sig_text.into_bytes()));

    let changed: Vec<String> = read
        .index
        .packs
        .iter()
        .filter(|e| {
            std::fs::read(dest.join(&e.file)).map(|b| sha256_hex(&b) != e.sha256).unwrap_or(true)
        })
        .map(|e| e.id.clone())
        .collect();

    let parent = dest.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
    std::fs::create_dir_all(parent)?;
    let name = dest.file_name().and_then(|n| n.to_str()).unwrap_or("mirror");
    let staging = parent.join(format!(".{name}.partial-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&staging);
    std::fs::create_dir_all(&staging)?;
    for (file, bytes) in &files {
        std::fs::write(staging.join(file), bytes)
            .with_context(|| format!("writing {}", staging.join(file).display()))?;
    }
    let old = parent.join(format!(".{name}.old-{}", std::process::id()));
    if dest.exists() {
        std::fs::rename(dest, &old).with_context(|| format!("moving {} aside", dest.display()))?;
    }
    if let Err(e) = std::fs::rename(&staging, dest) {
        let _ = std::fs::rename(&old, dest);
        return Err(e).with_context(|| format!("putting the mirror at {}", dest.display()));
    }
    let _ = std::fs::remove_dir_all(&old);
    Ok(MirrorReport { name: read.index.name.clone(), packs: read.index.packs.len(), changed })
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
        let mut parsed: Vec<GamePack> = Vec::new();
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
            parsed.push(pack);
        }
        // A repository is where a pack reaches strangers, so it lists none a
        // review refuses (`review.rs`). What a review only flags is for the
        // repository's own reviewer, who runs `game-bridge review`.
        for (i, pack) in parsed.iter().enumerate() {
            let others: Vec<GamePack> =
                parsed.iter().enumerate().filter(|(j, _)| *j != i).map(|(_, p)| p.clone()).collect();
            if let Some(why) = crate::review::refusal(&crate::review::review(pack, &others)) {
                anyhow::bail!("{}: {why}", packs[i].file);
            }
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

    /// A repository never lists a pack a review refuses.
    #[test]
    fn a_refused_pack_stops_the_build() {
        let dir = tempfile::tempdir().unwrap();
        let hostile = SVEN.replace(
            "args = [\"+connect {address}\"",
            "args = [\"+exec evil.cfg\", \"+connect {address}\"",
        );
        assert_ne!(hostile, SVEN, "the shipped pack's launch args moved");
        std::fs::write(dir.path().join("sven-coop.toml"), hostile).unwrap();
        let e = RepoIndex::build(dir.path(), "x").unwrap_err().to_string();
        assert!(e.contains("refused") && e.contains("+exec"), "{e}");
    }

    fn key(seed: u8) -> prns_core::identity::PrivateIdentityMaterial {
        prns_core::identity::PrivateIdentityMaterial::from_slice(&[seed; 64]).unwrap()
    }

    /// A signed repository in a folder, as `repo build --identity` and the
    /// publish job make one, with every pack signed too.
    fn signed_repo(dir: &Path, seed: u8) {
        use std::time::{Duration, SystemTime};
        let now = SystemTime::now() - Duration::from_secs(60);
        let day = Duration::from_secs(86_400);
        for e in std::fs::read_dir(dir).unwrap() {
            let p = e.unwrap().path();
            if p.extension().and_then(|e| e.to_str()) == Some("toml") {
                let sig = crate::signing::PackSignature::sign(&std::fs::read(&p).unwrap(), &key(seed), now, day).unwrap();
                std::fs::write(format!("{}{SIGNATURE_SUFFIX}", p.display()), sig.to_toml()).unwrap();
            }
        }
        let index = RepoIndex::build(dir, "Official").unwrap();
        let json = serde_json::to_vec_pretty(&index).unwrap();
        std::fs::write(dir.join(INDEX_FILE), &json).unwrap();
        let sig = crate::signing::PackSignature::sign_index(&json, &key(seed), now, day).unwrap();
        std::fs::write(dir.join(format!("{INDEX_FILE}{SIGNATURE_SUFFIX}")), sig.to_toml()).unwrap();
    }

    fn listing(dir: &Path) -> Vec<(String, Vec<u8>)> {
        let mut out: Vec<_> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| {
                let p = e.unwrap().path();
                (p.file_name().unwrap().to_string_lossy().into_owned(), std::fs::read(&p).unwrap())
            })
            .collect();
        out.sort();
        out
    }

    /// A mirror is the source, byte for byte, and nothing else; it takes
    /// only a list signed by the key it was told to hold it to; a pack whose
    /// bytes were changed in the source stops the mirror and leaves the last
    /// good copy; and a mirror of a mirror is the same copy again.
    #[tokio::test]
    async fn a_mirror_is_an_exact_one_way_copy_held_to_one_key() {
        let source = tempfile::tempdir().unwrap();
        std::fs::write(source.path().join("sven-coop.toml"), SVEN).unwrap();
        signed_repo(source.path(), 3);
        let signer = key(3).public().identity_hash();
        let out = tempfile::tempdir().unwrap();
        let dest = out.path().join("mirror");
        let src = source.path().to_string_lossy().into_owned();

        assert!(mirror(&src, &dest, key(4).public().identity_hash()).await.is_err(), "another key");
        let report = mirror(&src, &dest, signer).await.unwrap();
        assert_eq!((report.packs, report.changed.len()), (1, 1));
        assert_eq!(listing(&dest), listing(source.path()), "byte for byte, nothing added");

        // Taken again with nothing new: nothing changed.
        assert!(mirror(&src, &dest, signer).await.unwrap().changed.is_empty());

        // A mirror of the mirror, read as a file:// URL, is the same copy.
        let second = out.path().join("second");
        let url = url::Url::from_directory_path(&dest).unwrap().to_string();
        mirror(&url, &second, signer).await.unwrap();
        assert_eq!(listing(&second), listing(source.path()));

        // Someone edits a pack at the source without re-signing: refused,
        // and the mirror keeps what it had.
        std::fs::write(source.path().join("sven-coop.toml"), format!("{SVEN}\n# changed\n")).unwrap();
        assert!(mirror(&src, &dest, signer).await.is_err());
        assert_eq!(std::fs::read(dest.join("sven-coop.toml")).unwrap(), SVEN.as_bytes());

        // An unsigned list is never mirrored.
        std::fs::remove_file(source.path().join(format!("{INDEX_FILE}{SIGNATURE_SUFFIX}"))).unwrap();
        assert!(mirror(&src, &out.path().join("third"), signer).await.is_err());
    }

    /// A folder is a repository too, as a path or a `file://` URL.
    #[test]
    fn a_folder_is_a_repository_address() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().to_string_lossy().into_owned();
        let url = check_repo_url(&p).unwrap();
        assert!(url.starts_with("file://") && url.ends_with("/index.json"), "{url}");
        assert_eq!(check_repo_url(&url).unwrap(), url);
        assert!(check_repo_url("ftp://example.org/").is_err());
        assert!(check_repo_url("http://example.org/").is_err(), "plain http only to this machine");
        assert!(check_repo_url(OFFICIAL_REPOSITORY).is_ok());
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
