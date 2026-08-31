//! Provisioning the shared content copy: fetch, verify, extract.
//!
//! `PLAN.md` §11.2. A pack's `[content]` block says where a game's files come
//! from (`game_bridge::content`); this is the half that touches a disk. The
//! agent stores content **once per node** at
//! `<data_root>/content/<game_id>/<version>/` and binds it read-only into every
//! instance (`store.rs`), so provisioning happens at most once per game and
//! version, not once per instance.
//!
//! # The three rules that make a stranger's archive safe to unpack
//!
//! 1. **The digest decides, not the URL.** The archive is written to a
//!    temporary file and hashed before anything is extracted. A hijacked
//!    mirror, a stale CDN edge, or a truncated download all fail the same way:
//!    the bytes are deleted and nothing reaches the content tree.
//! 2. **Every entry path is rejected, never repaired.** `..`, an absolute path,
//!    a Windows-style drive or backslash, a NUL — refused with the entry named.
//!    `store.rs` makes the same choice for `writable_paths` and for the same
//!    reason: a normalizer is the only thing standing between a hostile input
//!    and the host, and silently repairing one teaches nobody anything. A link
//!    is held to the same rule, because a symlink to `/` inside an archive is
//!    just a path escape with extra steps.
//! 3. **A half-extracted tree is never visible as installed.** Extraction goes
//!    to a staging directory beside the destination and is renamed into place
//!    only once it is complete. `plan_and_check` treats "the content directory
//!    exists" as "the content is installed", so an interrupted download that
//!    left a partial tree there would be indistinguishable from a good one.
//!
//! # What this deliberately does not do
//!
//! **It never replaces content that is already there.** An existing directory
//! is the operator's — possibly hand-installed, possibly bind-mounted read-only
//! into running containers right now — and re-fetching over it would be a
//! silent update of code that is currently executing. A new version gets a new
//! `content_version`, which is a new directory.
//!
//! It also never runs anything out of the archive. Extraction sets no
//! executable bits from the tar and no setuid bits ever; what makes a game run
//! is the operator's container image (`config.rs`), which is exactly where the
//! "a pack cannot name what runs" line is drawn.

use std::fs;
use std::io::{self, BufReader, Read, Write};
use std::path::{Component, Path, PathBuf};

use game_bridge::content::PackContent;
use sha2::{Digest, Sha256};

use crate::store::{ContentRef, StoreLayout};

/// Ceiling on the compressed bytes accepted from the network, and on the bytes
/// written during extraction. The digest already pins *which* archive is
/// fetched, so this is not the main defence — it is the one that keeps a
/// mistake (a pack pinning a 400 GB archive, a decompression bomb whose digest
/// somebody pasted in good faith) from filling a node's disk before anyone
/// notices.
pub const MAX_ARCHIVE_BYTES: u64 = 8 * 1024 * 1024 * 1024;
pub const MAX_EXTRACTED_BYTES: u64 = 32 * 1024 * 1024 * 1024;

/// What `ensure` did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Provisioned {
    /// The content was already installed. Nothing was fetched and nothing was
    /// touched — see the module docs on why this is never an update.
    AlreadyInstalled(PathBuf),
    /// The archive was fetched, verified, and extracted here.
    Installed { dir: PathBuf, bytes: u64 },
}

#[derive(Debug)]
pub enum ProvisionError {
    /// The pack's driver is `manual`: a human has to put the files there.
    ManualInstallRequired { dir: PathBuf, note: Option<String> },
    /// The operator has not allowed this node to fetch content over the network.
    FetchNotPermitted { url: String },
    /// The `[content]` block itself is malformed.
    BadSpec(game_bridge::content::ContentError),
    /// The layout refused the game id or version.
    BadContentRef(crate::store::PlanError),
    Http { url: String, detail: String },
    /// The download exceeded `MAX_ARCHIVE_BYTES`, or extraction exceeded
    /// `MAX_EXTRACTED_BYTES`.
    TooLarge { limit: u64 },
    /// The bytes that arrived are not the bytes the pack pinned.
    DigestMismatch { expected: String, actual: String },
    /// An archive entry tried to write outside the destination.
    UnsafeEntry { entry: String, reason: &'static str },
    Io(io::Error),
}

impl core::fmt::Display for ProvisionError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::ManualInstallRequired { dir, note } => {
                write!(
                    f,
                    "this pack's content driver is \"manual\": install the game into {} yourself",
                    dir.display()
                )?;
                if let Some(note) = note {
                    write!(f, " ({note})")?;
                }
                Ok(())
            }
            Self::FetchNotPermitted { url } => write!(
                f,
                "this pack wants to download {url}, and this node has not enabled content \
                 fetching. Set allow_content_fetch = true in [store] once you trust the pack"
            ),
            Self::BadSpec(e) => write!(f, "unusable [content] block: {e}"),
            Self::BadContentRef(e) => write!(f, "unusable content reference: {e}"),
            Self::Http { url, detail } => write!(f, "fetching {url}: {detail}"),
            Self::TooLarge { limit } => {
                write!(f, "content exceeds this node's {limit}-byte limit")
            }
            Self::DigestMismatch { expected, actual } => write!(
                f,
                "downloaded content has sha256 {actual}, but the pack pins {expected}. The \
                 download was discarded: the digest is what decides, not the URL"
            ),
            Self::UnsafeEntry { entry, reason } => write!(
                f,
                "archive entry {entry:?} refused: {reason}. Entries are rejected, not repaired"
            ),
            Self::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for ProvisionError {}

impl From<io::Error> for ProvisionError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

/// Materializes a pack's content into a node's store.
pub struct Provisioner {
    layout: StoreLayout,
    /// Whether this node may download. Off by default: a pack is a file
    /// somebody wrote, and until §11.3's signing exists the operator's own
    /// switch is the only thing that says "yes, fetch what this file names".
    allow_fetch: bool,
}

impl Provisioner {
    pub fn new(layout: StoreLayout, allow_fetch: bool) -> Self {
        Self { layout, allow_fetch }
    }

    /// Make sure `content` is installed, fetching it if the pack says how and
    /// the operator has allowed it.
    pub async fn ensure(
        &self,
        content: &ContentRef,
        spec: &PackContent,
    ) -> Result<Provisioned, ProvisionError> {
        spec.validate().map_err(ProvisionError::BadSpec)?;
        let dir = self
            .layout
            .content_dir(content)
            .map_err(ProvisionError::BadContentRef)?;
        if dir.is_dir() {
            return Ok(Provisioned::AlreadyInstalled(dir));
        }

        match spec {
            PackContent::Manual { note } => Err(ProvisionError::ManualInstallRequired {
                dir,
                note: note.clone(),
            }),
            PackContent::Archive { url, sha256, strip_components } => {
                if !self.allow_fetch {
                    return Err(ProvisionError::FetchNotPermitted { url: url.clone() });
                }
                let staging = self.staging_root()?;
                let archive = staging.join("download");
                let bytes = fetch(url, &archive).await;
                let result = bytes.and_then(|_| {
                    verify_digest(&archive, sha256)?;
                    let unpacked = staging.join("unpacked");
                    fs::create_dir_all(&unpacked)?;
                    let written = extract(&archive, &unpacked, *strip_components)?;
                    Ok(written)
                });
                let written = match result {
                    Ok(written) => written,
                    Err(e) => {
                        // Nothing half-done survives a failure: the next attempt
                        // starts from an empty staging directory, not from
                        // whatever the last one managed to write.
                        let _ = fs::remove_dir_all(&staging);
                        return Err(e);
                    }
                };
                if let Some(parent) = dir.parent() {
                    fs::create_dir_all(parent)?;
                }
                // The rename is the moment the content becomes "installed", and
                // it is one operation. Extracting straight into `dir` would let
                // an interrupted run leave a partial tree that `plan_and_check`
                // reads as a complete install.
                fs::rename(staging.join("unpacked"), &dir)?;
                let _ = fs::remove_dir_all(&staging);
                Ok(Provisioned::Installed { dir, bytes: written })
            }
        }
    }

    /// A fresh staging directory on the same filesystem as the content tree, so
    /// the final move is a rename rather than a copy.
    fn staging_root(&self) -> Result<PathBuf, ProvisionError> {
        let mut seed = [0u8; 8];
        getrandom::getrandom(&mut seed).map_err(|e| {
            ProvisionError::Io(io::Error::other(format!("no randomness for staging dir: {e}")))
        })?;
        let dir = self
            .layout
            .root
            .join("staging")
            .join(format!("content-{}", hex::encode(seed)));
        fs::create_dir_all(&dir)?;
        Ok(dir)
    }
}

/// Stream a URL to `dest`, refusing anything over `MAX_ARCHIVE_BYTES`.
async fn fetch(url: &str, dest: &Path) -> Result<u64, ProvisionError> {
    let client = reqwest::Client::builder()
        .build()
        .map_err(|e| ProvisionError::Http { url: url.to_string(), detail: e.to_string() })?;
    let mut response = client
        .get(url)
        .send()
        .await
        .map_err(|e| ProvisionError::Http { url: url.to_string(), detail: e.to_string() })?;
    if !response.status().is_success() {
        return Err(ProvisionError::Http {
            url: url.to_string(),
            detail: format!("server answered {}", response.status()),
        });
    }
    let mut file = fs::File::create(dest)?;
    let mut total: u64 = 0;
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|e| ProvisionError::Http { url: url.to_string(), detail: e.to_string() })?
    {
        total += chunk.len() as u64;
        if total > MAX_ARCHIVE_BYTES {
            return Err(ProvisionError::TooLarge { limit: MAX_ARCHIVE_BYTES });
        }
        file.write_all(&chunk)?;
    }
    file.flush()?;
    Ok(total)
}

/// Hash the file and compare with what the pack pinned.
pub fn verify_digest(path: &Path, expected_hex: &str) -> Result<(), ProvisionError> {
    let mut file = fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    let actual = hex::encode(hasher.finalize());
    if !actual.eq_ignore_ascii_case(expected_hex) {
        return Err(ProvisionError::DigestMismatch {
            expected: expected_hex.to_ascii_lowercase(),
            actual,
        });
    }
    Ok(())
}

/// Extract a tar — plain, gzip, or xz — into `dest`, returning bytes written.
///
/// The compression is detected from the file's magic rather than from the URL's
/// extension: a pack's URL is a string somebody typed, and the bytes are what
/// actually has to be unpacked.
pub fn extract(archive: &Path, dest: &Path, strip_components: u8) -> Result<u64, ProvisionError> {
    let mut magic = [0u8; 6];
    let read = {
        let mut f = fs::File::open(archive)?;
        read_up_to(&mut f, &mut magic)?
    };
    let magic = &magic[..read];

    if magic.starts_with(&[0xfd, b'7', b'z', b'X', b'Z', 0x00]) {
        // lzma-rs has no streaming reader, so xz is decompressed to a file
        // beside the download rather than into memory: game content is
        // gigabytes, and a Vec of it is a node falling over.
        let plain = archive.with_extension("tar");
        {
            let mut input = BufReader::new(fs::File::open(archive)?);
            let mut output = LimitedWriter::new(fs::File::create(&plain)?, MAX_EXTRACTED_BYTES);
            lzma_rs::xz_decompress(&mut input, &mut output).map_err(|e| {
                io::Error::new(io::ErrorKind::InvalidData, format!("xz decompress: {e}"))
            })?;
            output.inner.flush()?;
        }
        let written = unpack(fs::File::open(&plain)?, dest, strip_components);
        let _ = fs::remove_file(&plain);
        written
    } else if magic.starts_with(&[0x1f, 0x8b]) {
        unpack(
            flate2::read::GzDecoder::new(fs::File::open(archive)?),
            dest,
            strip_components,
        )
    } else {
        unpack(fs::File::open(archive)?, dest, strip_components)
    }
}

fn read_up_to(r: &mut impl Read, buf: &mut [u8]) -> io::Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        match r.read(&mut buf[filled..])? {
            0 => break,
            n => filled += n,
        }
    }
    Ok(filled)
}

fn unpack(
    reader: impl Read,
    dest: &Path,
    strip_components: u8,
) -> Result<u64, ProvisionError> {
    let mut archive = tar::Archive::new(reader);
    let mut written: u64 = 0;
    for entry in archive.entries()? {
        let mut entry = entry?;
        let raw = entry.path()?.to_path_buf();
        let name = raw.display().to_string();
        let Some(relative) = strip(&safe_relative(&raw, &name)?, strip_components) else {
            // Wholly consumed by strip_components: the wrapper directory the
            // operator asked to drop.
            continue;
        };
        let target = dest.join(&relative);

        match entry.header().entry_type() {
            tar::EntryType::Directory => {
                fs::create_dir_all(&target)?;
            }
            tar::EntryType::Regular | tar::EntryType::Continuous => {
                if let Some(parent) = target.parent() {
                    fs::create_dir_all(parent)?;
                }
                let size = entry.header().size().unwrap_or(0);
                written = written
                    .checked_add(size)
                    .ok_or(ProvisionError::TooLarge { limit: MAX_EXTRACTED_BYTES })?;
                if written > MAX_EXTRACTED_BYTES {
                    return Err(ProvisionError::TooLarge { limit: MAX_EXTRACTED_BYTES });
                }
                let mut out = LimitedWriter::new(fs::File::create(&target)?, MAX_EXTRACTED_BYTES);
                io::copy(&mut entry, &mut out)?;
                out.inner.flush()?;
            }
            tar::EntryType::Symlink | tar::EntryType::Link => {
                // A link's *target* is a path the archive supplies, so it gets
                // the same treatment as the entry's own name. One that points
                // out of the tree is a path escape wearing a different hat.
                let link = entry
                    .link_name()?
                    .ok_or(ProvisionError::UnsafeEntry {
                        entry: name.clone(),
                        reason: "a link entry with no target",
                    })?
                    .to_path_buf();
                safe_relative(&link, &name)?;
                if let Some(parent) = target.parent() {
                    fs::create_dir_all(parent)?;
                }
                #[cfg(unix)]
                std::os::unix::fs::symlink(&link, &target)?;
            }
            other => {
                // Devices, fifos, sockets. Game content is files; anything else
                // in an archive from the internet is a question nobody asked.
                let _ = other;
                return Err(ProvisionError::UnsafeEntry {
                    entry: name,
                    reason: "only files, directories, and links are extracted",
                });
            }
        }
    }
    Ok(written)
}

/// Reject — never repair — a path that could leave the destination.
fn safe_relative(path: &Path, entry: &str) -> Result<PathBuf, ProvisionError> {
    let text = path.to_string_lossy();
    if text.is_empty() {
        return Err(ProvisionError::UnsafeEntry { entry: entry.to_string(), reason: "empty path" });
    }
    if text.contains('\0') {
        return Err(ProvisionError::UnsafeEntry {
            entry: entry.to_string(),
            reason: "contains a NUL byte",
        });
    }
    if text.contains('\\') {
        return Err(ProvisionError::UnsafeEntry {
            entry: entry.to_string(),
            reason: "contains a backslash, which is a separator on some hosts",
        });
    }
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => out.push(part),
            Component::CurDir => {
                return Err(ProvisionError::UnsafeEntry {
                    entry: entry.to_string(),
                    reason: "contains a '.' component",
                })
            }
            Component::ParentDir => {
                return Err(ProvisionError::UnsafeEntry {
                    entry: entry.to_string(),
                    reason: "contains a '..' component",
                })
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err(ProvisionError::UnsafeEntry {
                    entry: entry.to_string(),
                    reason: "is an absolute path",
                })
            }
        }
    }
    Ok(out)
}

/// Drop `n` leading components, or `None` if the path has no more than that.
fn strip(path: &Path, n: u8) -> Option<PathBuf> {
    let mut components = path.components();
    for _ in 0..n {
        components.next()?;
    }
    let rest: PathBuf = components.collect();
    if rest.as_os_str().is_empty() {
        None
    } else {
        Some(rest)
    }
}

/// A writer that stops rather than filling a disk.
struct LimitedWriter<W: Write> {
    inner: W,
    remaining: u64,
}

impl<W: Write> LimitedWriter<W> {
    fn new(inner: W, limit: u64) -> Self {
        Self { inner, remaining: limit }
    }
}

impl<W: Write> Write for LimitedWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if buf.len() as u64 > self.remaining {
            return Err(io::Error::other(format!(
                "content exceeds this node's {MAX_EXTRACTED_BYTES}-byte limit"
            )));
        }
        let n = self.inner.write(buf)?;
        self.remaining -= n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn tar_with(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut builder = tar::Builder::new(Vec::new());
        for (name, body) in entries {
            let mut header = tar::Header::new_gnu();
            header.set_size(body.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            builder.append_data(&mut header, name, Cursor::new(*body)).unwrap();
        }
        builder.into_inner().unwrap()
    }

    /// A tar entry whose name the `tar` builder would refuse to write; built by
    /// hand so the extractor is tested against what a hostile archive contains,
    /// not against what a well-behaved writer produces.
    fn tar_with_raw_name(name: &str, body: &[u8], kind: tar::EntryType, link: Option<&str>) -> Vec<u8> {
        let mut header = tar::Header::new_gnu();
        header.set_size(if kind == tar::EntryType::Regular { body.len() as u64 } else { 0 });
        header.set_mode(0o644);
        header.set_entry_type(kind);
        header.as_gnu_mut().unwrap().name[..name.len()].copy_from_slice(name.as_bytes());
        if let Some(link) = link {
            header.as_gnu_mut().unwrap().linkname[..link.len()].copy_from_slice(link.as_bytes());
        }
        header.set_cksum();
        let mut out = Vec::new();
        out.extend_from_slice(header.as_bytes());
        if kind == tar::EntryType::Regular {
            out.extend_from_slice(body);
            out.resize(out.len().div_ceil(512) * 512, 0);
        }
        out.resize(out.len() + 1024, 0);
        out
    }

    fn write(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
        let path = dir.join(name);
        fs::write(&path, bytes).unwrap();
        path
    }

    fn digest_of(bytes: &[u8]) -> String {
        hex::encode(Sha256::digest(bytes))
    }

    #[test]
    fn a_digest_that_matches_passes_and_one_that_does_not_names_both() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write(tmp.path(), "a.bin", b"hello");
        verify_digest(&path, &digest_of(b"hello")).unwrap();
        // Case does not matter; the pack is a file a human typed.
        verify_digest(&path, &digest_of(b"hello").to_uppercase()).unwrap();

        let wrong = digest_of(b"goodbye");
        match verify_digest(&path, &wrong) {
            Err(ProvisionError::DigestMismatch { expected, actual }) => {
                assert_eq!(expected, wrong);
                assert_eq!(actual, digest_of(b"hello"));
            }
            other => panic!("expected a digest mismatch, got {other:?}"),
        }
    }

    #[test]
    fn a_plain_tar_extracts_and_strip_components_drops_the_wrapper() {
        let tmp = tempfile::tempdir().unwrap();
        let archive = write(
            tmp.path(),
            "a.tar",
            &tar_with(&[("game-5.9.0/svencoop/maps/one.bsp", b"map")]),
        );
        let dest = tmp.path().join("out");
        fs::create_dir_all(&dest).unwrap();
        extract(&archive, &dest, 1).unwrap();
        assert_eq!(fs::read(dest.join("svencoop/maps/one.bsp")).unwrap(), b"map");
    }

    #[test]
    fn gzip_and_xz_are_detected_from_the_bytes_not_the_name() {
        let tmp = tempfile::tempdir().unwrap();
        let plain = tar_with(&[("data/one.txt", b"one")]);

        let mut encoder =
            flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        encoder.write_all(&plain).unwrap();
        // Deliberately misnamed: the magic decides.
        let gz = write(tmp.path(), "misnamed.bin", &encoder.finish().unwrap());
        let dest = tmp.path().join("gz");
        fs::create_dir_all(&dest).unwrap();
        extract(&gz, &dest, 0).unwrap();
        assert_eq!(fs::read(dest.join("data/one.txt")).unwrap(), b"one");

        let mut xz = Vec::new();
        lzma_rs::xz_compress(&mut Cursor::new(plain.clone()), &mut xz).unwrap();
        let xz = write(tmp.path(), "also-misnamed.bin", &xz);
        let dest = tmp.path().join("xz");
        fs::create_dir_all(&dest).unwrap();
        extract(&xz, &dest, 0).unwrap();
        assert_eq!(fs::read(dest.join("data/one.txt")).unwrap(), b"one");
    }

    /// The load-bearing test for unpacking a stranger's file: an entry that
    /// climbs out of the destination is refused, not cleaned up and written.
    #[test]
    fn an_entry_that_escapes_the_destination_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("out");
        for (name, expect) in [
            ("../escape.txt", "contains a '..' component"),
            ("a/../../escape.txt", "contains a '..' component"),
            ("/etc/cron.d/evil", "is an absolute path"),
            ("a\\b.txt", "contains a backslash, which is a separator on some hosts"),
        ] {
            fs::create_dir_all(&dest).unwrap();
            let archive = write(
                tmp.path(),
                "bad.tar",
                &tar_with_raw_name(name, b"x", tar::EntryType::Regular, None),
            );
            match extract(&archive, &dest, 0) {
                Err(ProvisionError::UnsafeEntry { reason, .. }) => assert_eq!(reason, expect),
                other => panic!("{name} must be refused, got {other:?}"),
            }
            assert!(!tmp.path().join("escape.txt").exists());
            fs::remove_dir_all(&dest).unwrap();
        }
    }

    /// A symlink is a path escape with extra steps, so its target is checked
    /// exactly like an entry name.
    #[test]
    fn a_link_pointing_out_of_the_tree_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("out");
        fs::create_dir_all(&dest).unwrap();
        let archive = write(
            tmp.path(),
            "link.tar",
            &tar_with_raw_name("cfg", b"", tar::EntryType::Symlink, Some("/etc/passwd")),
        );
        assert!(matches!(
            extract(&archive, &dest, 0),
            Err(ProvisionError::UnsafeEntry { reason: "is an absolute path", .. })
        ));

        // A link that stays inside is ordinary game content and is kept.
        let archive = write(
            tmp.path(),
            "ok.tar",
            &tar_with_raw_name("cfg", b"", tar::EntryType::Symlink, Some("real/config.cfg")),
        );
        extract(&archive, &dest, 0).unwrap();
        assert_eq!(
            fs::read_link(dest.join("cfg")).unwrap(),
            PathBuf::from("real/config.cfg")
        );
    }

    #[test]
    fn a_device_entry_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("out");
        fs::create_dir_all(&dest).unwrap();
        let archive = write(
            tmp.path(),
            "dev.tar",
            &tar_with_raw_name("dev/sda", b"", tar::EntryType::Block, None),
        );
        assert!(matches!(
            extract(&archive, &dest, 0),
            Err(ProvisionError::UnsafeEntry {
                reason: "only files, directories, and links are extracted",
                ..
            })
        ));
    }

    fn layout(tmp: &tempfile::TempDir) -> StoreLayout {
        StoreLayout::new(tmp.path().to_path_buf())
    }

    fn sven() -> ContentRef {
        ContentRef { game_id: "sven-coop".to_string(), version: "5.26".to_string() }
    }

    #[tokio::test]
    async fn a_manual_pack_names_the_directory_a_human_has_to_fill() {
        let tmp = tempfile::tempdir().unwrap();
        let provisioner = Provisioner::new(layout(&tmp), true);
        let err = provisioner
            .ensure(&sven(), &PackContent::Manual { note: Some("buy it on Steam".into()) })
            .await
            .unwrap_err();
        match err {
            ProvisionError::ManualInstallRequired { dir, note } => {
                assert!(dir.ends_with("content/sven-coop/5.26"), "{}", dir.display());
                assert_eq!(note.as_deref(), Some("buy it on Steam"));
            }
            other => panic!("expected a manual-install error, got {other:?}"),
        }
    }

    /// Content that is already there is never re-fetched and never replaced:
    /// it may be bind-mounted read-only into containers running right now.
    #[tokio::test]
    async fn existing_content_is_left_alone_even_when_a_driver_could_fetch_it() {
        let tmp = tempfile::tempdir().unwrap();
        let l = layout(&tmp);
        let dir = l.content_dir(&sven()).unwrap();
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("hand-installed"), b"mine").unwrap();

        let provisioner = Provisioner::new(l, true);
        let spec = PackContent::Archive {
            // Unreachable on purpose: reaching the network at all would be the bug.
            url: "http://127.0.0.1:1/never.tar".to_string(),
            sha256: digest_of(b"whatever"),
            strip_components: 0,
        };
        assert_eq!(
            provisioner.ensure(&sven(), &spec).await.unwrap(),
            Provisioned::AlreadyInstalled(dir.clone())
        );
        assert_eq!(fs::read(dir.join("hand-installed")).unwrap(), b"mine");
    }

    /// Until §11.3's signing exists, the operator's switch is the only thing
    /// that says "yes, download what this file names".
    #[tokio::test]
    async fn a_node_that_has_not_opted_in_does_not_fetch() {
        let tmp = tempfile::tempdir().unwrap();
        let provisioner = Provisioner::new(layout(&tmp), false);
        let spec = PackContent::Archive {
            url: "http://127.0.0.1:1/never.tar".to_string(),
            sha256: digest_of(b"whatever"),
            strip_components: 0,
        };
        assert!(matches!(
            provisioner.ensure(&sven(), &spec).await,
            Err(ProvisionError::FetchNotPermitted { .. })
        ));
    }

    /// Serve one archive on loopback, so the fetch path is exercised for real
    /// without reaching the internet.
    async fn serve(body: Vec<u8>) -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = axum::Router::new().route(
            "/content.tar",
            axum::routing::get(move || {
                let body = body.clone();
                async move { body }
            }),
        );
        let handle = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (format!("http://{addr}/content.tar"), handle)
    }

    #[tokio::test]
    async fn an_archive_is_fetched_verified_and_installed() {
        let tmp = tempfile::tempdir().unwrap();
        let body = tar_with(&[("sven-5.26/svencoop/maps/one.bsp", b"map")]);
        let (url, server) = serve(body.clone()).await;

        let l = layout(&tmp);
        let provisioner = Provisioner::new(l.clone(), true);
        let spec = PackContent::Archive {
            url,
            sha256: digest_of(&body),
            strip_components: 1,
        };
        let out = provisioner.ensure(&sven(), &spec).await.unwrap();
        let dir = l.content_dir(&sven()).unwrap();
        assert_eq!(out, Provisioned::Installed { dir: dir.clone(), bytes: 3 });
        assert_eq!(fs::read(dir.join("svencoop/maps/one.bsp")).unwrap(), b"map");

        // Second call is a no-op, so a create that races another never refetches.
        assert_eq!(
            provisioner.ensure(&sven(), &spec).await.unwrap(),
            Provisioned::AlreadyInstalled(dir)
        );
        server.abort();
    }

    /// The digest decides. Bytes that do not match it never reach the content
    /// tree, and nothing partial is left behind for the next attempt to trust.
    #[tokio::test]
    async fn bytes_that_fail_the_digest_never_reach_the_content_tree() {
        let tmp = tempfile::tempdir().unwrap();
        let body = tar_with(&[("game/one.txt", b"one")]);
        let (url, server) = serve(body).await;

        let l = layout(&tmp);
        let provisioner = Provisioner::new(l.clone(), true);
        let spec = PackContent::Archive {
            url,
            sha256: digest_of(b"a different archive entirely"),
            strip_components: 0,
        };
        assert!(matches!(
            provisioner.ensure(&sven(), &spec).await,
            Err(ProvisionError::DigestMismatch { .. })
        ));
        assert!(!l.content_dir(&sven()).unwrap().exists());
        let staging = tmp.path().join("staging");
        let leftovers: Vec<_> = fs::read_dir(&staging)
            .map(|d| d.flatten().map(|e| e.path()).collect())
            .unwrap_or_default();
        assert!(leftovers.is_empty(), "a failed fetch left {leftovers:?} behind");
        server.abort();
    }

    #[tokio::test]
    async fn an_http_error_is_reported_with_the_url() {
        let tmp = tempfile::tempdir().unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = axum::Router::new();
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        let provisioner = Provisioner::new(layout(&tmp), true);
        let spec = PackContent::Archive {
            url: format!("http://{addr}/missing.tar"),
            sha256: digest_of(b"x"),
            strip_components: 0,
        };
        match provisioner.ensure(&sven(), &spec).await {
            Err(ProvisionError::Http { url, detail }) => {
                assert!(url.contains("missing.tar"));
                assert!(detail.contains("404"), "{detail}");
            }
            other => panic!("expected an HTTP error, got {other:?}"),
        }
        server.abort();
    }
}
