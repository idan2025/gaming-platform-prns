//! Adding a game to a running node by installing its pack (`PLAN.md` §13.3
//! step 5, node half).
//!
//! The web UI's "add a game" is this: an operator pastes a pack, or points at a
//! URL, and the node installs it into the pack directory. That is the whole
//! feature, and almost all of the code here is about what it refuses.
//!
//! # What an import may not become
//!
//! A route that writes files into a directory the agent reads at startup is a
//! route that decides what this node runs, so the rules that hold everywhere
//! else hold harder here:
//!
//! * **A pack is parsed before it is written.** An unparseable file in the pack
//!   directory would be skipped at next startup with a warning nobody reads;
//!   refusing it now puts the error in front of the person who caused it.
//! * **The filename comes from the pack's own id, never from the URL or from a
//!   caller-supplied name.** A name from outside is a path traversal waiting to
//!   happen, and `id` is already validated to be filesystem-safe.
//! * **An existing pack is never silently replaced.** Its game may be running
//!   right now, and a pack decides ports and transports. Replacing takes an
//!   explicit `replace`, exactly as content installation never replaces an
//!   existing version.
//! * **The trust tier is computed and returned**, so a UI can show what §11.4
//!   requires at the moment of import rather than in a document nobody reads.
//!   The node does not *gate* on it here — gating happens at load, under the
//!   operator's `[pack_trust]` policy, which is the one place that decision
//!   lives.
//! * **A fetched pack is bounded.** A pack is a small text file; anything large
//!   is not one, and streaming an unbounded body into memory on an unauthorised
//!   guess is a way to end a node.
//!
//! # Why this does not weaken "a pack cannot name what runs"
//!
//! An imported pack is still a pack: it names an enum this build implements and
//! hands it typed parameters. It cannot name an image, a command or an argv a
//! node executes. Importing one adds a *description*; whether this node can run
//! that description still depends on the operator writing a `[games.<id>]`
//! runtime, and `GET /games` says so plainly when they have not.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use game_bridge::signing::{PackTrust, TrustPolicy};
use game_bridge::GamePack;
use serde::{Deserialize, Serialize};

/// Largest pack this will accept. A game pack is a few kilobytes of TOML; this
/// is generous by two orders of magnitude and still small enough that a hostile
/// URL cannot exhaust the node.
pub const MAX_PACK_BYTES: usize = 256 * 1024;

/// What a caller asks for. Exactly one of `toml` or `url`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImportRequest {
    /// The pack's text, pasted.
    #[serde(default)]
    pub toml: Option<String>,
    /// Where to fetch it from. `http` or `https` only — a `file://` URL would
    /// turn this into a way to read the node's disk.
    #[serde(default)]
    pub url: Option<String>,
    /// A pack repository's address (`game_bridge::repo`), with `id`: the pack
    /// is fetched beside its index, checked against the index's digest, and
    /// its signature, if it has one, comes with it and must hold.
    #[serde(default)]
    pub repository: Option<String>,
    /// Which pack of `repository`.
    #[serde(default)]
    pub id: Option<String>,
    /// Replace a pack with this id if one is already installed.
    #[serde(default)]
    pub replace: bool,
}

/// What was installed.
#[derive(Debug, Clone, Serialize)]
pub struct ImportedPack {
    pub id: String,
    pub display_name: String,
    /// §11.4's words, for a UI to show verbatim.
    pub trust: String,
    pub trust_detail: String,
    pub file: String,
    /// True when this overwrote a pack that was already installed.
    pub replaced: bool,
    /// Whether this node can actually run it, which needs a runtime the
    /// operator configured. Importing a pack never makes a game runnable on its
    /// own, and saying so here stops that being a surprise.
    pub runnable: bool,
}

#[derive(Debug)]
pub enum ImportError {
    /// Not exactly one of `toml`, `url`, or `repository` with `id`.
    SourceNotClear,
    /// The repository's bytes or signature are not what it promised.
    Repository(String),
    UnsupportedUrlScheme(String),
    Fetch(String),
    TooLarge { bytes: usize },
    /// The bytes are not a usable pack.
    NotAPack(String),
    /// A pack with this id is installed and `replace` was not set.
    AlreadyInstalled(String),
    Io(String),
}

impl core::fmt::Display for ImportError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::SourceNotClear => write!(
                f,
                "give exactly one of `toml`, `url`, or `repository` with `id`: two sources for \
                 one pack is a way to install the one you did not mean to"
            ),
            Self::Repository(e) => write!(f, "{e}; nothing was installed"),
            Self::UnsupportedUrlScheme(u) => write!(
                f,
                "pack url {u:?} must start with http:// or https://. A pack is fetched over \
                 the network; it is not a way to read this node's disk"
            ),
            Self::Fetch(e) => write!(f, "could not fetch the pack: {e}"),
            Self::TooLarge { bytes } => write!(
                f,
                "that is {bytes} bytes and a game pack is a few kilobytes of TOML; \
                 the limit is {MAX_PACK_BYTES}"
            ),
            Self::NotAPack(e) => write!(f, "that is not a usable game pack: {e}"),
            Self::AlreadyInstalled(id) => write!(
                f,
                "{id} is already installed. A pack decides ports and transports and its game \
                 may be running now, so replacing one is deliberate: send replace = true"
            ),
            Self::Io(e) => write!(f, "could not write the pack: {e}"),
        }
    }
}

impl std::error::Error for ImportError {}

/// Fetch or take the pack's text, parse it, and install it into `dir`.
///
/// `runtime_configured` answers "can this node run it", which the importer
/// cannot know on its own — that is the operator's `[games.<id>]` config.
pub async fn import(
    request: &ImportRequest,
    dir: &Path,
    policy: &TrustPolicy,
    now: SystemTime,
    runtime_configured: impl Fn(&str) -> bool,
) -> Result<ImportedPack, ImportError> {
    let (src, sig) = match (&request.toml, &request.url, &request.repository, &request.id) {
        (Some(t), None, None, None) => (t.clone(), None),
        (None, Some(u), None, None) => (fetch_pack(u).await?, None),
        (None, None, Some(r), Some(id)) => fetch_from_repository(r, id, now).await?,
        _ => return Err(ImportError::SourceNotClear),
    };
    if src.len() > MAX_PACK_BYTES {
        return Err(ImportError::TooLarge { bytes: src.len() });
    }

    // Parsed before written. A file that only fails at the next startup is a
    // file whose error nobody sees.
    let pack = GamePack::parse(&src).map_err(|e| ImportError::NotAPack(format!("{e}")))?;
    // And held to the review's refusals (`game_bridge::review`) against the
    // packs already here: nothing a node's operator could want, and a pack
    // from a repository may never have been reviewed.
    let others: Vec<GamePack> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("toml"))
        .filter_map(|p| std::fs::read_to_string(p).ok())
        .filter_map(|s| GamePack::parse(&s).ok())
        .filter(|p| p.id != pack.id)
        .collect();
    if let Some(why) = game_bridge::review::refusal(&game_bridge::review::review(&pack, &others)) {
        return Err(ImportError::NotAPack(why));
    }

    // The name is the pack's own id, which `validate` has already constrained to
    // filesystem-safe characters. Nothing from the URL or the caller reaches the
    // path.
    let path = dir.join(format!("{}.toml", pack.id));
    let existed = path.exists();
    if existed && !request.replace {
        return Err(ImportError::AlreadyInstalled(pack.id.clone()));
    }

    if let Some(wanted) = &request.id {
        if wanted != &pack.id {
            return Err(ImportError::Repository(format!(
                "the repository lists {wanted:?} but the pack inside is {:?}",
                pack.id
            )));
        }
    }

    std::fs::create_dir_all(dir).map_err(|e| ImportError::Io(e.to_string()))?;
    // A repository pack's signature goes in first, so the pack never sits
    // beside a signature that is not its own. Without one, a signature left
    // from the pack this replaces goes if it does not hold for the new bytes:
    // left there, it would make the new pack fail to load as a forgery.
    let sig_path = dir.join(format!("{}.toml{}", pack.id, game_bridge::signing::SIGNATURE_SUFFIX));
    match &sig {
        Some(text) => {
            let staging = dir.join(format!(".{}.toml.sig.partial", pack.id));
            std::fs::write(&staging, text).map_err(|e| ImportError::Io(e.to_string()))?;
            std::fs::rename(&staging, &sig_path).map_err(|e| ImportError::Io(e.to_string()))?;
        }
        None => {
            let holds = std::fs::read_to_string(&sig_path)
                .ok()
                .and_then(|t| game_bridge::signing::PackSignature::parse(&t).ok())
                .is_some_and(|s| s.verify(src.as_bytes(), now).is_ok());
            if !holds {
                let _ = std::fs::remove_file(&sig_path);
            }
        }
    }
    // Write beside and rename, so an interrupted import cannot leave a
    // half-written pack where the loader will find it — the same reason content
    // extraction stages and renames.
    let staging = dir.join(format!(".{}.toml.partial", pack.id));
    std::fs::write(&staging, src.as_bytes()).map_err(|e| ImportError::Io(e.to_string()))?;
    std::fs::rename(&staging, &path).map_err(|e| {
        let _ = std::fs::remove_file(&staging);
        ImportError::Io(e.to_string())
    })?;

    // A signature that arrived beside an existing file still decides the tier;
    // an import over HTTP carries none, which reads as unsigned rather than as
    // an error.
    let trust = match GamePack::load_verified(&path, policy, now) {
        Ok(verified) => verified.trust,
        // The pack parsed a moment ago, so a failure here is about the
        // signature beside it, not the pack. Unsigned is the honest reading of
        // "there is nothing to verify".
        Err(_) => PackTrust::UnsignedLocal,
    };

    Ok(ImportedPack {
        runnable: runtime_configured(&pack.id),
        id: pack.id.clone(),
        display_name: pack.display_name.clone(),
        trust: trust.label().to_string(),
        trust_detail: trust.explanation().to_string(),
        file: path.display().to_string(),
        replaced: existed,
    })
}

/// Fetch pack `id` from the repository at `repository`: hashed against the
/// index before it is read, and its signature, when the index says it has
/// one, verified before anything is written. Returns the pack's text and the
/// signature's.
async fn fetch_from_repository(
    repository: &str,
    id: &str,
    now: SystemTime,
) -> Result<(String, Option<String>), ImportError> {
    use game_bridge::repo::{beside, sha256_hex};
    let index = read_repository(repository).await?;
    let url = game_bridge::repo::check_repo_url(repository)
        .map_err(|e| ImportError::Repository(e.to_string()))?;
    let entry = index
        .entry(id)
        .ok_or_else(|| ImportError::Repository(format!("the repository does not list {id:?}")))?;
    let bytes = fetch_bytes(&beside(&url, &entry.file), MAX_PACK_BYTES).await?;
    if sha256_hex(&bytes) != entry.sha256 {
        return Err(ImportError::Repository(
            "the pack's bytes are not the ones the repository lists".into(),
        ));
    }
    let src = String::from_utf8(bytes)
        .map_err(|_| ImportError::NotAPack("the bytes are not UTF-8 text".to_string()))?;
    let sig = if entry.signed {
        let sig_url = beside(&url, &format!("{}{}", entry.file, game_bridge::signing::SIGNATURE_SUFFIX));
        let text = String::from_utf8(fetch_bytes(&sig_url, 16 * 1024).await?)
            .map_err(|_| ImportError::Repository("the pack's signature is not text".into()))?;
        let parsed = game_bridge::signing::PackSignature::parse(&text)
            .map_err(|e| ImportError::Repository(format!("the pack's signature is unreadable: {e}")))?;
        // A signature that fails is a refusal, never an unsigned install.
        parsed
            .verify(src.as_bytes(), now)
            .map_err(|e| ImportError::Repository(format!("the pack's signature does not hold: {e}")))?;
        Some(text)
    } else {
        None
    };
    Ok((src, sig))
}

/// A repository's index, read now.
pub async fn read_repository(repository: &str) -> Result<game_bridge::repo::RepoIndex, ImportError> {
    let url = game_bridge::repo::check_repo_url(repository)
        .map_err(|e| ImportError::Repository(e.to_string()))?;
    let bytes = fetch_bytes(&url, game_bridge::repo::MAX_INDEX_BYTES).await?;
    game_bridge::repo::RepoIndex::parse(&bytes).map_err(|e| ImportError::Repository(e.to_string()))
}

/// A repository's files, `https` or a folder on this machine
/// (`game_bridge::repo::fetch`, the one fetch the launcher uses too).
async fn fetch_bytes(url: &str, max: usize) -> Result<Vec<u8>, ImportError> {
    game_bridge::repo::fetch(url, max).await.map_err(|e| ImportError::Fetch(format!("{e:#}")))
}

async fn fetch_pack(url: &str) -> Result<String, ImportError> {
    if !(url.starts_with("https://") || url.starts_with("http://")) {
        return Err(ImportError::UnsupportedUrlScheme(url.to_string()));
    }
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|e| ImportError::Fetch(e.to_string()))?;
    let response = client
        .get(url)
        .send()
        .await
        .map_err(|e| ImportError::Fetch(e.to_string()))?;
    if !response.status().is_success() {
        return Err(ImportError::Fetch(format!("{} said {}", url, response.status())));
    }
    // Refuse on the declared length before reading, when the server declares
    // one — the cheap check first.
    if let Some(len) = response.content_length() {
        if len as usize > MAX_PACK_BYTES {
            return Err(ImportError::TooLarge { bytes: len as usize });
        }
    }
    let bytes = response
        .bytes()
        .await
        .map_err(|e| ImportError::Fetch(e.to_string()))?;
    if bytes.len() > MAX_PACK_BYTES {
        return Err(ImportError::TooLarge { bytes: bytes.len() });
    }
    String::from_utf8(bytes.to_vec())
        .map_err(|_| ImportError::NotAPack("the bytes are not UTF-8 text".to_string()))
}

/// Where packs live for a given pack directory, for a caller that has only the
/// agent's config.
pub fn pack_path(dir: &Path, id: &str) -> PathBuf {
    dir.join(format!("{id}.toml"))
}

/// The files a removal took out of the pack directory.
#[derive(Debug, Clone, Serialize)]
pub struct RemovedPack {
    pub id: String,
    pub files: Vec<String>,
}

/// Take a pack out of `dir` (ROADMAP §11 "Remove a pack"), with the signature
/// beside it. **The file is found by reading the packs there, never by
/// building a path from `id`**: an id from a URL is a path traversal waiting
/// to happen, and only a file that parses as the pack with this id is ever
/// touched. Every file carrying the id goes, or the loader would find it
/// again. `Ok(None)` when no pack in `dir` has this id.
///
/// Whether the pack may go, with servers or a download of its game on the
/// node, is the caller's to decide first; the game's files are not touched.
pub fn remove(dir: &Path, id: &str) -> std::io::Result<Option<RemovedPack>> {
    let mut found = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if path.extension().and_then(|e| e.to_str()) != Some("toml") {
            continue;
        }
        let Ok(src) = std::fs::read_to_string(&path) else { continue };
        if GamePack::parse(&src).is_ok_and(|p| p.id == id) {
            found.push(path);
        }
    }
    if found.is_empty() {
        return Ok(None);
    }
    let mut files = Vec::new();
    for path in found {
        let mut sig = path.clone().into_os_string();
        sig.push(game_bridge::signing::SIGNATURE_SUFFIX);
        let sig = PathBuf::from(sig);
        if sig.exists() {
            std::fs::remove_file(&sig)?;
            files.push(sig.display().to_string());
        }
        std::fs::remove_file(&path)?;
        files.push(path.display().to_string());
    }
    Ok(Some(RemovedPack { id: id.to_string(), files }))
}

#[cfg(test)]
mod tests {
    use super::*;

    const PACK: &str = r#"
schema_version = 1
id = "test-game"
display_name = "Test Game"
app_name = "test-game"
default_port = 27015
transport = "udp"
min_link_class = 1
query = "a2s"
"#;

    fn paste(toml: &str, replace: bool) -> ImportRequest {
        ImportRequest { toml: Some(toml.to_string()), url: None, repository: None, id: None, replace }
    }

    /// Serve `dir` over plain HTTP on loopback.
    fn serve(dir: std::path::PathBuf) -> u16 {
        use std::io::{BufRead, BufReader, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for mut stream in listener.incoming().flatten() {
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                let _ = reader.read_line(&mut line);
                loop {
                    let mut h = String::new();
                    if reader.read_line(&mut h).unwrap_or(0) == 0 || h == "\r\n" {
                        break;
                    }
                }
                let path = line.split_whitespace().nth(1).unwrap_or("/").trim_start_matches('/');
                let body = std::fs::read(dir.join(path));
                let head = match &body {
                    Ok(b) => format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", b.len()),
                    Err(_) => "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".into(),
                };
                let _ = stream.write_all(head.as_bytes());
                if let Ok(b) = body {
                    let _ = stream.write_all(&b);
                }
            }
        });
        port
    }

    /// From a repository: the digest decides, a signature comes along and
    /// must hold, and a pack whose id is not the listed one is refused.
    #[tokio::test]
    async fn a_repository_pack_installs_only_as_listed_and_signed() {
        let served = tempfile::tempdir().unwrap();
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(served.path().join("test-game.toml"), PACK).unwrap();
        let key = prns_core::identity::PrivateIdentityMaterial::from_slice(&[9; 64]).unwrap();
        let sig = game_bridge::signing::PackSignature::sign(
            PACK.as_bytes(),
            &key,
            now() - std::time::Duration::from_secs(60),
            std::time::Duration::from_secs(86_400),
        )
        .unwrap();
        std::fs::write(served.path().join("test-game.toml.sig"), sig.to_toml()).unwrap();
        let build = || {
            let index = game_bridge::repo::RepoIndex::build(served.path(), "t").unwrap();
            std::fs::write(served.path().join("index.json"), serde_json::to_vec(&index).unwrap())
                .unwrap();
        };
        build();
        let url = format!("http://127.0.0.1:{}/", serve(served.path().to_path_buf()));
        let req = |id: &str, replace| ImportRequest {
            toml: None,
            url: None,
            repository: Some(url.clone()),
            id: Some(id.to_string()),
            replace,
        };

        let out = run(&req("test-game", false), dir.path()).await.unwrap();
        assert_eq!(out.trust, "signed by an unknown key");
        assert!(dir.path().join("test-game.toml.sig").exists());
        assert!(run(&req("absent", false), dir.path()).await.is_err());

        // Other bytes than listed: refused, nothing changes.
        std::fs::write(served.path().join("test-game.toml"), format!("{PACK}\n# changed\n")).unwrap();
        let e = run(&req("test-game", true), dir.path()).await.unwrap_err().to_string();
        assert!(e.contains("not the ones the repository lists"), "{e}");
        assert_eq!(std::fs::read_to_string(dir.path().join("test-game.toml")).unwrap(), PACK);

        // Listed, but the signature is for the old bytes: refused, never unsigned.
        build();
        let e = run(&req("test-game", true), dir.path()).await.unwrap_err().to_string();
        assert!(e.contains("does not hold"), "{e}");
        assert_eq!(std::fs::read_to_string(dir.path().join("test-game.toml")).unwrap(), PACK);
    }

    fn now() -> SystemTime {
        SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_800_000_000)
    }

    async fn run(req: &ImportRequest, dir: &Path) -> Result<ImportedPack, ImportError> {
        import(req, dir, &TrustPolicy::allowing_unsigned(), now(), |_| true).await
    }

    #[tokio::test]
    async fn a_pasted_pack_lands_in_the_directory_under_its_own_id() {
        let dir = tempfile::tempdir().unwrap();
        let out = run(&paste(PACK, false), dir.path()).await.unwrap();
        assert_eq!(out.id, "test-game");
        assert!(!out.replaced);
        assert_eq!(out.trust, "unsigned local");
        // The name is the pack's, not the caller's.
        assert!(dir.path().join("test-game.toml").exists());
        // And the loader can read what was written.
        let loaded = GamePack::load_dir(dir.path()).unwrap();
        assert_eq!(loaded.packs.len(), 1);
        assert!(loaded.errors.is_empty());
    }

    /// Parsed before written: an unparseable file in the pack directory would be
    /// skipped at the next startup with a warning nobody reads.
    #[tokio::test]
    async fn a_file_that_is_not_a_pack_is_refused_and_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        assert!(run(&paste("this is not a pack", false), dir.path()).await.is_err());
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    /// A pack the review refuses is not written, here as at a repository's
    /// build: one passing itself off as a game already here, and one whose
    /// launch block would run a config file on a player's machine.
    #[tokio::test]
    async fn a_pack_the_review_refuses_is_never_written() {
        let dir = tempfile::tempdir().unwrap();
        run(&paste(PACK, false), dir.path()).await.unwrap();
        let imitation = PACK.replace("id = \"test-game\"", "id = \"other\"").replace(
            "app_name = \"test-game\"",
            "app_name = \"other\"",
        );
        let e = run(&paste(&imitation, false), dir.path()).await.unwrap_err().to_string();
        assert!(e.contains("display_name"), "{e}");
        let hostile = format!(
            "{}\n[launch]\nkind = \"goldsrc\"\nargs = [\"+exec evil.cfg\"]\n",
            PACK.replace("id = \"test-game\"", "id = \"third\"")
                .replace("app_name = \"test-game\"", "app_name = \"third\"")
                .replace("Test Game", "Third Game")
        );
        let e = run(&paste(&hostile, false), dir.path()).await.unwrap_err().to_string();
        assert!(e.contains("+exec"), "{e}");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    /// A pack decides ports and transports and its game may be running right
    /// now, so replacing one is deliberate.
    #[tokio::test]
    async fn an_installed_pack_is_not_replaced_by_accident() {
        let dir = tempfile::tempdir().unwrap();
        run(&paste(PACK, false), dir.path()).await.unwrap();

        let err = run(&paste(PACK, false), dir.path()).await.expect_err("refused");
        assert!(format!("{err}").contains("replace = true"), "{err}");

        let out = run(&paste(PACK, true), dir.path()).await.unwrap();
        assert!(out.replaced);
    }

    /// A `file://` URL would make this a way to read the node's disk, and the
    /// reply carries the file's contents.
    #[tokio::test]
    async fn a_non_http_url_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let req = ImportRequest {
            toml: None,
            url: Some("file:///etc/passwd".into()),
            repository: None,
            id: None,
            replace: false,
        };
        assert!(matches!(
            run(&req, dir.path()).await,
            Err(ImportError::UnsupportedUrlScheme(_))
        ));
    }

    #[tokio::test]
    async fn a_request_with_no_source_or_two_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let neither = ImportRequest { toml: None, url: None, repository: None, id: None, replace: false };
        assert!(matches!(run(&neither, dir.path()).await, Err(ImportError::SourceNotClear)));

        let both = ImportRequest {
            toml: Some(PACK.into()),
            url: Some("https://example.org/p.toml".into()),
            repository: None,
            id: None,
            replace: false,
        };
        assert!(matches!(run(&both, dir.path()).await, Err(ImportError::SourceNotClear)));
    }

    #[tokio::test]
    async fn an_oversized_pack_is_refused_before_it_is_parsed() {
        let dir = tempfile::tempdir().unwrap();
        let big = "#".repeat(MAX_PACK_BYTES + 1);
        assert!(matches!(
            run(&paste(&big, false), dir.path()).await,
            Err(ImportError::TooLarge { .. })
        ));
    }

    /// Importing a pack adds a description. Whether the node can run it is the
    /// operator's `[games.<id>]` config, and saying so at import stops that
    /// being a surprise at start.
    #[tokio::test]
    async fn an_imported_pack_says_whether_this_node_can_actually_run_it() {
        let dir = tempfile::tempdir().unwrap();
        let out = import(
            &paste(PACK, false),
            dir.path(),
            &TrustPolicy::allowing_unsigned(),
            now(),
            |_| false,
        )
        .await
        .unwrap();
        assert!(!out.runnable);
    }

    /// A removal finds the pack by its contents: a hostile id names no file,
    /// and the signature goes with the pack.
    #[tokio::test]
    async fn a_pack_is_removed_by_what_it_says_never_by_a_path_from_outside() {
        let dir = tempfile::tempdir().unwrap();
        run(&paste(PACK, false), dir.path()).await.unwrap();
        let file = dir.path().join("test-game.toml");
        std::fs::write(dir.path().join("test-game.toml.sig"), "not checked here").unwrap();
        std::fs::write(dir.path().join("notes.txt"), "keep me").unwrap();

        for hostile in ["../test-game", "test-game.toml", "/etc/passwd", ""] {
            assert!(remove(dir.path(), hostile).unwrap().is_none(), "{hostile:?} matched something");
        }
        assert!(file.exists());

        let removed = remove(dir.path(), "test-game").unwrap().expect("removed");
        assert_eq!(removed.files.len(), 2, "{removed:?}");
        assert!(!file.exists() && !dir.path().join("test-game.toml.sig").exists());
        assert!(dir.path().join("notes.txt").exists(), "nothing else is touched");
        assert!(remove(dir.path(), "test-game").unwrap().is_none(), "gone means gone");
    }

    /// Replacing a signed pack with other bytes takes its old signature away,
    /// or the new pack would not load; the same bytes keep it.
    #[tokio::test]
    async fn a_replaced_pack_never_keeps_a_signature_that_is_not_its_own() {
        let dir = tempfile::tempdir().unwrap();
        run(&paste(PACK, false), dir.path()).await.unwrap();
        let key = prns_core::identity::PrivateIdentityMaterial::from_slice(&[5; 64]).unwrap();
        let sig = game_bridge::signing::PackSignature::sign(
            PACK.as_bytes(),
            &key,
            now() - std::time::Duration::from_secs(60),
            std::time::Duration::from_secs(86_400),
        )
        .unwrap();
        let sig_path = dir.path().join("test-game.toml.sig");
        std::fs::write(&sig_path, sig.to_toml()).unwrap();
        assert_eq!(run(&paste(PACK, true), dir.path()).await.unwrap().trust, "signed by an unknown key");
        assert!(sig_path.exists(), "the same bytes keep their signature");
        let out = run(&paste(&format!("{PACK}\n# edited\n"), true), dir.path()).await.unwrap();
        assert_eq!(out.trust, "unsigned local");
        assert!(!sig_path.exists());
    }
}
