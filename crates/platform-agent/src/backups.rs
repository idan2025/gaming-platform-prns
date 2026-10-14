//! Backups of a server's own files (`ROADMAP.md` item 7).
//!
//! # The rules
//!
//! * **Only the writable paths.** A backup is `instances/<id>/writable`, the
//!   directories the pack declared writable (saves, configs, logs). The shared
//!   game files are read-only and never copied per server.
//! * **The name is the agent's.** A backup is `YYYYMMDD-HHMMSS.tar.gz` (UTC),
//!   made here, and a name from a request is checked against that shape before
//!   it touches a path, so `../` never reaches the disk.
//! * **Restore only while the server is stopped**, by extracting beside and
//!   swapping in, so a running game never has its files changed under it and an
//!   interrupted restore never leaves half a tree. Extraction is `content.rs`'s
//!   `extract`: its path and link checks hold here too.
//! * **Written beside and renamed**, so an interrupted backup is never listed.

use std::path::{Path, PathBuf};

use serde::Serialize;

/// Where backups live, under the data root: `backups/<instance id>/`.
pub const BACKUPS_DIR: &str = "backups";
const SUFFIX: &str = ".tar.gz";

/// One backup of one server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Backup {
    pub name: String,
    pub bytes: u64,
    pub created_unix: Option<i64>,
}

/// Whether `name` is one this module would have made.
pub fn valid_name(name: &str) -> bool {
    let Some(stem) = name.strip_suffix(SUFFIX) else { return false };
    let (stamp, extra) = match stem.split_once('-') {
        Some((date, rest)) => match rest.split_once('-') {
            Some((time, n)) => ((date, time), Some(n)),
            None => ((date, rest), None),
        },
        None => return false,
    };
    let digits = |s: &str, len: usize| s.len() == len && s.bytes().all(|b| b.is_ascii_digit());
    digits(stamp.0, 8)
        && digits(stamp.1, 6)
        && extra
            .is_none_or(|n| !n.is_empty() && n.len() <= 3 && n.bytes().all(|b| b.is_ascii_digit()))
}

/// A new name for a backup made at `now`, not yet used in `dir`.
pub fn new_name(dir: &Path, now: i64) -> String {
    let (y, m, d) = crate::agent::civil_from_days(now.div_euclid(86_400));
    let secs = now.rem_euclid(86_400);
    let stem =
        format!("{y:04}{m:02}{d:02}-{:02}{:02}{:02}", secs / 3600, secs % 3600 / 60, secs % 60);
    for n in 1..1000 {
        let name = if n == 1 { format!("{stem}{SUFFIX}") } else { format!("{stem}-{n}{SUFFIX}") };
        if !dir.join(&name).exists() {
            return name;
        }
    }
    format!("{stem}-999{SUFFIX}")
}

/// The backups in `dir`, newest first. Anything not named like one is
/// ignored, a half-written one included.
pub fn list(dir: &Path) -> Vec<Backup> {
    let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut out: Vec<Backup> = entries
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().into_string().ok()?;
            if !valid_name(&name) {
                return None;
            }
            let meta = e.metadata().ok()?;
            let created_unix = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs() as i64);
            Some(Backup { name, bytes: meta.len(), created_unix })
        })
        .collect();
    out.sort_by(|a, b| b.name.cmp(&a.name));
    out
}

/// Pack `writable` into `dir/name`: entries under `writable/`, links kept as
/// links. Written beside and renamed.
pub fn create(writable: &Path, dir: &Path, name: &str) -> std::io::Result<Backup> {
    std::fs::create_dir_all(dir)?;
    let path = dir.join(name);
    let partial = dir.join(format!(".{name}.partial"));
    let written = (|| {
        let file = std::fs::File::create(&partial)?;
        let gz = flate2::write::GzEncoder::new(file, flate2::Compression::default());
        let mut tar = tar::Builder::new(gz);
        tar.follow_symlinks(false);
        if writable.is_dir() {
            tar.append_dir_all("writable", writable)?;
        } else {
            let mut header = tar::Header::new_gnu();
            header.set_entry_type(tar::EntryType::Directory);
            header.set_mode(0o755);
            header.set_size(0);
            tar.append_data(&mut header, "writable", std::io::empty())?;
        }
        tar.into_inner()?.finish()?.sync_all()
    })();
    if let Err(e) = written {
        let _ = std::fs::remove_file(&partial);
        return Err(e);
    }
    std::fs::rename(&partial, &path)?;
    let bytes = std::fs::metadata(&path)?.len();
    Ok(Backup { name: name.to_string(), bytes, created_unix: Some(crate::agent::now_unix()) })
}

/// Put a backup back: extract into `staging`, then swap it in for
/// `writable`. The directories `writable` has now and the backup lacks are
/// made empty, so every mount the server's container has still has a source.
pub fn restore(archive: &Path, writable: &Path, staging: &Path) -> Result<(), String> {
    let unpacked = staging.join("unpacked");
    std::fs::create_dir_all(&unpacked).map_err(|e| format!("{e}"))?;
    let done = (|| {
        crate::content::extract(archive, &unpacked, 0)
            .map_err(|e| format!("the backup could not be read: {e}"))?;
        let restored = unpacked.join("writable");
        if !restored.is_dir() {
            return Err("the backup holds no writable directory".to_string());
        }
        if let Ok(entries) = std::fs::read_dir(writable) {
            for e in entries.flatten() {
                if e.file_type().is_ok_and(|t| t.is_dir()) && !restored.join(e.file_name()).exists()
                {
                    std::fs::create_dir_all(restored.join(e.file_name()))
                        .map_err(|e| format!("{e}"))?;
                }
            }
        }
        let old = staging.join("previous");
        if writable.exists() {
            std::fs::rename(writable, &old)
                .map_err(|e| format!("moving the current files aside: {e}"))?;
        }
        if let Err(e) = std::fs::rename(&restored, writable) {
            // Put the old files back rather than leave the server with none.
            let _ = std::fs::rename(&old, writable);
            return Err(format!("putting the backup in place: {e}"));
        }
        Ok(())
    })();
    let _ = std::fs::remove_dir_all(staging);
    done
}

/// The path of a named backup in `dir`, or `None` for a name this module
/// would not have made.
pub fn path_of(dir: &Path, name: &str) -> Option<PathBuf> {
    valid_name(name).then(|| dir.join(name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_name_this_module_makes_names_a_file() {
        assert!(valid_name("20261014-051002.tar.gz"));
        assert!(valid_name("20261014-051002-2.tar.gz"));
        for bad in [
            "../20261014-051002.tar.gz",
            "20261014-051002.tar.gz/..",
            "x.tar.gz",
            "20261014-05100.tar.gz",
            "20261014-051002-.tar.gz",
            "20261014-051002-1234.tar.gz",
            "20261014-051002.tar",
            "",
        ] {
            assert!(!valid_name(bad), "{bad:?} must be refused");
            assert!(path_of(Path::new("/b"), bad).is_none());
        }
        let dir = tempfile::tempdir().unwrap();
        let first = new_name(dir.path(), 1_791_000_000);
        assert!(valid_name(&first), "{first}");
        std::fs::write(dir.path().join(&first), b"").unwrap();
        let second = new_name(dir.path(), 1_791_000_000);
        assert!(valid_name(&second) && second != first, "{second}");
    }

    /// A backup comes back exactly, a directory the backup lacks is made
    /// empty so its mount still has a source, and a half-written one is
    /// never listed.
    #[test]
    fn a_backup_restores_what_it_took() {
        let root = tempfile::tempdir().unwrap();
        let writable = root.path().join("writable");
        std::fs::create_dir_all(writable.join("save")).unwrap();
        std::fs::write(writable.join("save/slot1.sav"), b"level 3").unwrap();
        let dir = root.path().join("backups");
        let name = new_name(&dir, 1_791_000_000);
        let made = create(&writable, &dir, &name).unwrap();
        assert!(made.bytes > 0);
        std::fs::write(dir.join(".20261014-000000.tar.gz.partial"), b"half").unwrap();
        assert_eq!(list(&dir).len(), 1, "only finished backups are listed");

        std::fs::write(writable.join("save/slot1.sav"), b"level 9").unwrap();
        std::fs::create_dir_all(writable.join("added-later")).unwrap();
        restore(&dir.join(&name), &writable, &root.path().join("staging")).unwrap();
        assert_eq!(std::fs::read(writable.join("save/slot1.sav")).unwrap(), b"level 3");
        assert!(writable.join("added-later").is_dir(), "a mount source is never left missing");
        assert!(!root.path().join("staging").exists(), "nothing left in staging");
    }
}
