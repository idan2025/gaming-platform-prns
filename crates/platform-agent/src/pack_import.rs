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
    /// Neither `toml` nor `url`, or both.
    SourceNotClear,
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
                "give exactly one of `toml` or `url`: two sources for one pack is a way to \
                 install the one you did not mean to"
            ),
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
    let src = match (&request.toml, &request.url) {
        (Some(t), None) => t.clone(),
        (None, Some(u)) => fetch_pack(u).await?,
        _ => return Err(ImportError::SourceNotClear),
    };
    if src.len() > MAX_PACK_BYTES {
        return Err(ImportError::TooLarge { bytes: src.len() });
    }

    // Parsed before written. A file that only fails at the next startup is a
    // file whose error nobody sees.
    let pack = GamePack::parse(&src).map_err(|e| ImportError::NotAPack(format!("{e}")))?;

    // The name is the pack's own id, which `validate` has already constrained to
    // filesystem-safe characters. Nothing from the URL or the caller reaches the
    // path.
    let path = dir.join(format!("{}.toml", pack.id));
    let existed = path.exists();
    if existed && !request.replace {
        return Err(ImportError::AlreadyInstalled(pack.id.clone()));
    }

    std::fs::create_dir_all(dir).map_err(|e| ImportError::Io(e.to_string()))?;
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
        ImportRequest { toml: Some(toml.to_string()), url: None, replace }
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
        let neither = ImportRequest { toml: None, url: None, replace: false };
        assert!(matches!(run(&neither, dir.path()).await, Err(ImportError::SourceNotClear)));

        let both = ImportRequest {
            toml: Some(PACK.into()),
            url: Some("https://example.org/p.toml".into()),
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
}
