//! `[content]` — how a pack says where the game files come from.
//!
//! `PLAN.md` §11.2. Content provisioning is manual today: an operator puts the
//! game into `<data_root>/content/<game>/<version>/` by hand and the agent
//! checks the pack's `writable_paths` exist inside it before starting anything
//! (`platform-agent`'s `agent.rs`). That is a driver — the `manual` one — that
//! was never named. Naming it is what lets a second one exist.
//!
//! # Why this is not a hole in "a pack cannot name what runs"
//!
//! `pack.rs` says a pack has no field to put a command in, and that is the
//! property that makes a stranger's pack safe to *execute* rather than merely
//! safe to read. A content driver does not weaken it, because **the pack names
//! an enum variant the Rust code implements and hands it typed parameters** —
//! the same seam as `QueryProtocol::A2s`. `driver = "archive"` selects code
//! that is already in this binary; it does not supply code.
//!
//! The parameters are where a hostile pack would try to live, so each one is
//! bounded rather than free-form:
//!
//! - `url` must be `http`/`https`. Not `file:`, which would make a pack read
//!   the node's disk, and not a scheme some future fetcher might treat as a
//!   command.
//! - `sha256` is **mandatory** and fixed-width. The safety of `archive` is the
//!   digest, not the URL: a hijacked mirror, a stale CDN edge, or a
//!   man-in-the-middle all produce bytes that do not match, and the download is
//!   discarded. A URL with no digest would be an instruction to trust whoever
//!   currently answers that hostname.
//! - `strip_components` is a small count, not a path.
//!
//! Extraction safety — refusing entries that escape the destination, symlinks
//! that point out of it — belongs to whoever unpacks, not here; this module
//! only decides that a spec is well formed. The agent is the one that touches a
//! disk.
//!
//! `steamcmd` is the same shape one level up: the pack supplies an **app id, a
//! number**, and the agent builds the command line. Which steamcmd runs — a
//! container image — is the node operator's config, exactly like the image a
//! game runs in. A pack that could say "run steamcmd like *this*" would be
//! naming what runs, so it cannot.
//!
//! # What is not here yet
//!
//! `oci` (pull an image the *operator* allowlisted) is the last driver in
//! `PLAN.md` §11.2's order. It is an additive variant: a pack naming it today
//! fails to parse, loudly, which is the correct answer from a build that cannot
//! fetch it.

use serde::{Deserialize, Serialize};

/// Length of a hex-encoded SHA-256 digest.
const SHA256_HEX_LEN: usize = 64;

/// Upper bound on `strip_components`. Deep enough for any real archive layout,
/// shallow enough that the field cannot be used to express something odd.
const MAX_STRIP_COMPONENTS: u8 = 8;

/// Where a game's files come from.
///
/// Serialized as a `driver`-tagged table, so `[content] driver = "archive"`
/// reads as a sentence and an unknown driver is a parse error rather than a
/// silently ignored one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "driver", rename_all = "lowercase", deny_unknown_fields)]
pub enum PackContent {
    /// The operator installs the content themselves. What every pack does
    /// today, and the honest answer for any game whose files need credentials
    /// or a licence click (`GAMES.md` §5).
    Manual {
        /// Optional human-readable pointer — where to get the files, which
        /// edition. Never parsed, never fetched.
        #[serde(default)]
        note: Option<String>,
    },
    /// Fetch one archive, verify its digest, extract it.
    Archive {
        /// `http` or `https`. Trusted for availability only; see `sha256`.
        url: String,
        /// Hex-encoded SHA-256 of the archive **as downloaded**. Mandatory.
        sha256: String,
        /// Leading path components to drop, for archives wrapped in one
        /// top-level directory.
        #[serde(default)]
        strip_components: u8,
    },
    /// Download a Steam app anonymously.
    ///
    /// Only apps whose dedicated server needs no credentials can be fetched
    /// unattended (`GAMES.md` §5); anything else stays `manual`, which is the
    /// honest answer rather than a worse one. There is no `login` field on
    /// purpose: a pack is a file that gets shared, and a field for credentials
    /// is a field people put credentials in.
    Steamcmd {
        /// Steam application id. A number, not a command line.
        app_id: u32,
    },
}

impl Default for PackContent {
    fn default() -> Self {
        Self::Manual { note: None }
    }
}

/// Why a `[content]` block is not usable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContentError {
    /// A scheme other than `http`/`https`, or no scheme at all.
    UnsupportedUrlScheme(String),
    /// The digest is not 64 hex characters.
    MalformedDigest(String),
    /// `strip_components` is implausible.
    StripTooDeep(u8),
    /// App id zero is not an app.
    BadAppId(u32),
}

impl core::fmt::Display for ContentError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::UnsupportedUrlScheme(url) => write!(
                f,
                "content url {url:?} must start with http:// or https://: a pack fetches over \
                 the network, it does not read the node's disk"
            ),
            Self::MalformedDigest(d) => write!(
                f,
                "content sha256 {d:?} is not {SHA256_HEX_LEN} hex characters — the digest is \
                 what makes an archive safe to run, so a missing or malformed one is refused \
                 rather than skipped"
            ),
            Self::StripTooDeep(n) => write!(
                f,
                "content strip_components {n} is over the limit of {MAX_STRIP_COMPONENTS}"
            ),
            Self::BadAppId(n) => write!(f, "steamcmd app_id {n} is not a Steam application id"),
        }
    }
}

impl std::error::Error for ContentError {}

impl PackContent {
    /// Check the spec is well formed. Called when a pack is loaded, so a broken
    /// `[content]` block names the file rather than surfacing at deploy.
    pub fn validate(&self) -> Result<(), ContentError> {
        match self {
            Self::Manual { .. } => Ok(()),
            Self::Archive { url, sha256, strip_components } => {
                if !(url.starts_with("https://") || url.starts_with("http://")) {
                    return Err(ContentError::UnsupportedUrlScheme(url.clone()));
                }
                if sha256.len() != SHA256_HEX_LEN
                    || !sha256.bytes().all(|b| b.is_ascii_hexdigit())
                {
                    return Err(ContentError::MalformedDigest(sha256.clone()));
                }
                if *strip_components > MAX_STRIP_COMPONENTS {
                    return Err(ContentError::StripTooDeep(*strip_components));
                }
                Ok(())
            }
            Self::Steamcmd { app_id } => {
                if *app_id == 0 {
                    return Err(ContentError::BadAppId(*app_id));
                }
                Ok(())
            }
        }
    }

    /// True when a node can fetch this content without a human. `manual` cannot,
    /// which is why an agent must tell an operator what is missing rather than
    /// wait for it to appear.
    pub fn is_automatic(&self) -> bool {
        matches!(self, Self::Archive { .. } | Self::Steamcmd { .. })
    }

    /// Driver name as it appears in the pack, for messages a human reads.
    pub fn driver_name(&self) -> &'static str {
        match self {
            Self::Manual { .. } => "manual",
            Self::Archive { .. } => "archive",
            Self::Steamcmd { .. } => "steamcmd",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DIGEST: &str = "9f2c00000000000000000000000000000000000000000000000000000000abcd";

    fn archive(url: &str) -> PackContent {
        PackContent::Archive {
            url: url.to_string(),
            sha256: DIGEST.to_string(),
            strip_components: 1,
        }
    }

    #[test]
    fn a_pack_with_no_content_block_is_manual() {
        assert_eq!(PackContent::default(), PackContent::Manual { note: None });
        assert!(!PackContent::default().is_automatic());
    }

    #[test]
    fn an_archive_spec_parses_from_the_plan_s_example() {
        let src = r#"
            driver = "archive"
            url = "https://example.org/minetest-server-5.9.0.tar.xz"
            sha256 = "9f2c00000000000000000000000000000000000000000000000000000000abcd"
            strip_components = 1
        "#;
        let content: PackContent = toml::from_str(src).unwrap();
        assert_eq!(content, archive("https://example.org/minetest-server-5.9.0.tar.xz"));
        content.validate().unwrap();
        assert!(content.is_automatic());
    }

    /// The digest is the whole safety story for `archive`. A pack that leaves
    /// it out must fail to parse, not default to "trust the URL".
    #[test]
    fn an_archive_without_a_digest_does_not_parse() {
        let src = r#"
            driver = "archive"
            url = "https://example.org/x.tar.xz"
        "#;
        assert!(toml::from_str::<PackContent>(src).is_err());
    }

    #[test]
    fn a_malformed_digest_is_refused() {
        for bad in ["", "deadbeef", &"z".repeat(SHA256_HEX_LEN)] {
            let content = PackContent::Archive {
                url: "https://example.org/x.tar.xz".to_string(),
                sha256: bad.to_string(),
                strip_components: 0,
            };
            assert!(
                matches!(content.validate(), Err(ContentError::MalformedDigest(_))),
                "digest {bad:?} must be refused"
            );
        }
    }

    /// `file:` would turn a pack into a request that the node read its own
    /// disk, which is exactly the class of thing a pack may not ask for.
    #[test]
    fn a_non_http_url_is_refused() {
        for bad in ["file:///etc/passwd", "/etc/passwd", "ftp://example.org/x.tar"] {
            assert!(
                matches!(archive(bad).validate(), Err(ContentError::UnsupportedUrlScheme(_))),
                "url {bad:?} must be refused"
            );
        }
    }

    #[test]
    fn an_absurd_strip_depth_is_refused() {
        let content = PackContent::Archive {
            url: "https://example.org/x.tar.xz".to_string(),
            sha256: DIGEST.to_string(),
            strip_components: 200,
        };
        assert!(matches!(content.validate(), Err(ContentError::StripTooDeep(200))));
    }

    #[test]
    fn a_steamcmd_spec_parses_from_the_plan_s_example() {
        let src = r#"
            driver = "steamcmd"
            app_id = 276060
        "#;
        let content: PackContent = toml::from_str(src).unwrap();
        assert_eq!(content, PackContent::Steamcmd { app_id: 276060 });
        content.validate().unwrap();
        assert!(content.is_automatic());
    }

    /// There is no `login` field, and adding one would make every shared pack a
    /// place people put credentials. A pack that asks for one must not parse.
    #[test]
    fn steamcmd_has_nowhere_to_put_credentials() {
        let src = r#"
            driver = "steamcmd"
            app_id = 276060
            login = "someone"
            password = "hunter2"
        "#;
        assert!(toml::from_str::<PackContent>(src).is_err());
    }

    #[test]
    fn app_id_zero_is_refused() {
        assert!(matches!(
            PackContent::Steamcmd { app_id: 0 }.validate(),
            Err(ContentError::BadAppId(0))
        ));
    }

    /// A driver this build cannot run must fail loudly at load. `oci` is last in
    /// PLAN.md §11.2's order; until it exists, a pack naming it is refused
    /// rather than half-understood.
    #[test]
    fn an_unimplemented_driver_is_refused_not_ignored() {
        let src = r#"
            driver = "oci"
            image = "example.org/game:1"
        "#;
        assert!(toml::from_str::<PackContent>(src).is_err());
    }

    /// A driver's own fields are closed too: `manual` must not become a place
    /// to smuggle an archive's parameters past review.
    #[test]
    fn unknown_fields_inside_a_driver_are_rejected() {
        let src = r#"
            driver = "manual"
            url = "https://example.org/x.tar.xz"
        "#;
        assert!(toml::from_str::<PackContent>(src).is_err());
    }
}
