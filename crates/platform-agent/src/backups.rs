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
//! * **A schedule prunes only what it made.** It remembers the names it wrote
//!   and deletes the oldest of those beyond `keep`; a backup the operator made
//!   by hand is never deleted by anything automatic. A schedule file that is
//!   lost turns its backups into hand-made ones, which is the safe direction.
//! * **An imported backup is checked before it is kept**: it is extracted
//!   into staging with `content.rs`'s `extract` (its path and link checks) and
//!   must hold a `writable` directory and nothing beside it, then it is given
//!   an agent-made name. A file from outside never names itself, and restore
//!   checks it all again.
//! * **A removed server's backups stay** until the operator deletes them, and
//!   only a server that no longer exists can have all of its backups deleted
//!   in one go.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

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
/// links when restore would take them back. Written beside and renamed.
///
/// A link that is absolute or climbs out with `..` is left out: restore
/// extracts under `content::extract`'s link rules and refuses one, so a backup
/// holding it could never be restored. The Bedrock image links its server's
/// own files in from the shared copy that way, and makes the links again at
/// every start.
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
            append_tree(&mut tar, writable, Path::new("writable"))?;
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

/// `append_dir_all`, minus the links restore would refuse.
fn append_tree<W: std::io::Write>(tar: &mut tar::Builder<W>, dir: &Path, name: &Path) -> std::io::Result<()> {
    tar.append_dir(name, dir)?;
    let mut entries: Vec<_> = std::fs::read_dir(dir)?.flatten().collect();
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        let path = e.path();
        let inner = name.join(e.file_name());
        let kind = e.file_type()?;
        if kind.is_symlink() {
            let target = std::fs::read_link(&path)?;
            let restorable = target.is_relative()
                && !target.components().any(|c| matches!(c, std::path::Component::ParentDir));
            if restorable {
                tar.append_path_with_name(&path, &inner)?;
            }
        } else if kind.is_dir() {
            append_tree(tar, &path, &inner)?;
        } else if kind.is_file() {
            tar.append_path_with_name(&path, &inner)?;
        }
    }
    Ok(())
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

/// The file in an instance's folder that holds its backup schedule.
pub const SCHEDULE_FILE: &str = "backup-schedule.json";

/// Back a server up every `every_hours`, keeping the newest `keep` of the
/// backups the schedule made.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackupSchedule {
    pub every_hours: u32,
    pub keep: u32,
}

impl BackupSchedule {
    pub fn validate(&self) -> Result<(), String> {
        if !(1..=24 * 30).contains(&self.every_hours) {
            return Err("a backup schedule runs every 1 hour to 30 days".into());
        }
        if !(1..=100).contains(&self.keep) {
            return Err("a backup schedule keeps 1 to 100 backups".into());
        }
        Ok(())
    }

    /// Whether a backup is due at `now`, the last one having been made at
    /// `last`.
    pub fn due(&self, now: i64, last: Option<i64>) -> bool {
        last.is_none_or(|at| now - at >= i64::from(self.every_hours) * 3600)
    }
}

/// A backup schedule as stored: when it last ran, and the backups it made,
/// oldest first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredSchedule {
    pub schedule: BackupSchedule,
    #[serde(default)]
    pub last_unix: Option<i64>,
    #[serde(default)]
    pub made: Vec<String>,
}

impl StoredSchedule {
    /// Record a backup the schedule made, and return the ones to delete: the
    /// oldest it made beyond `keep`.
    pub fn record(&mut self, name: String) -> Vec<String> {
        self.made.push(name);
        let extra = self.made.len().saturating_sub(self.schedule.keep as usize);
        self.made.drain(..extra).collect()
    }
}

/// Read the backup schedule in an instance folder. Missing or unreadable is
/// none.
pub fn load_schedule(instance_dir: &Path) -> Option<StoredSchedule> {
    let bytes = std::fs::read(instance_dir.join(SCHEDULE_FILE)).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Write it beside and rename over; `None` removes it.
pub fn save_schedule(instance_dir: &Path, stored: Option<&StoredSchedule>) -> std::io::Result<()> {
    let path = instance_dir.join(SCHEDULE_FILE);
    match stored {
        None => match std::fs::remove_file(&path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        },
        Some(stored) => {
            std::fs::create_dir_all(instance_dir)?;
            let tmp = path.with_extension("json.tmp");
            std::fs::write(&tmp, serde_json::to_vec_pretty(stored).map_err(std::io::Error::other)?)?;
            std::fs::rename(&tmp, &path)
        }
    }
}

/// Whether `archive` is a backup this module could restore: a tar.gz that
/// extracts cleanly under `content.rs`'s checks into `writable/` and nothing
/// else. `staging` is used and removed.
pub fn check_archive(archive: &Path, staging: &Path) -> Result<(), String> {
    let done = (|| {
        std::fs::create_dir_all(staging).map_err(|e| format!("{e}"))?;
        crate::content::extract(archive, staging, 0)
            .map_err(|e| format!("this is not a backup the node can read: {e}"))?;
        let mut names = Vec::new();
        for e in std::fs::read_dir(staging).map_err(|e| format!("{e}"))?.flatten() {
            names.push(e.file_name().to_string_lossy().into_owned());
        }
        if names != ["writable"] || !staging.join("writable").is_dir() {
            return Err("this is not a server backup: it must hold one writable folder and nothing else".into());
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

    #[cfg(unix)]
    #[test]
    fn a_link_restore_would_refuse_is_left_out_and_the_backup_restores() {
        let tmp = tempfile::tempdir().unwrap();
        let w = tmp.path().join("writable");
        std::fs::create_dir_all(w.join("server/worlds/world")).unwrap();
        std::fs::write(w.join("server/worlds/world/level.dat"), b"lvl").unwrap();
        std::os::unix::fs::symlink("/game/bedrock_server", w.join("server/bedrock_server")).unwrap();
        std::os::unix::fs::symlink("../escape", w.join("server/up")).unwrap();
        std::os::unix::fs::symlink("worlds", w.join("server/w")).unwrap();
        let dir = tmp.path().join("backups");
        let b = create(&w, &dir, "20261018-000000.tar.gz").unwrap();
        let back = tmp.path().join("restored");
        std::fs::create_dir_all(&back).unwrap();
        restore(&dir.join(&b.name), &back.join("writable"), &tmp.path().join("staging")).unwrap();
        let r = back.join("writable/server");
        assert_eq!(std::fs::read(r.join("worlds/world/level.dat")).unwrap(), b"lvl");
        assert!(r.join("w").symlink_metadata().unwrap().file_type().is_symlink(), "a link inside the tree is kept");
        assert!(r.join("bedrock_server").symlink_metadata().is_err() && r.join("up").symlink_metadata().is_err());
    }

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

    #[test]
    fn only_an_archive_shaped_like_a_backup_is_accepted() {
        let root = tempfile::tempdir().unwrap();
        let writable = root.path().join("writable");
        std::fs::create_dir_all(writable.join("save")).unwrap();
        std::fs::write(writable.join("save/a"), b"x").unwrap();
        let dir = root.path().join("b");
        let made = create(&writable, &dir, "20261014-000000.tar.gz").unwrap();
        check_archive(&dir.join(&made.name), &root.path().join("s1")).expect("its own backups pass");
        assert!(!root.path().join("s1").exists(), "staging removed");

        // Something else beside `writable`.
        let other = dir.join("other.tar.gz");
        {
            let gz = flate2::write::GzEncoder::new(
                std::fs::File::create(&other).unwrap(),
                flate2::Compression::default(),
            );
            let mut tar = tar::Builder::new(gz);
            tar.append_dir_all("writable", &writable).unwrap();
            tar.append_dir_all("extra", &writable).unwrap();
            tar.into_inner().unwrap().finish().unwrap();
        }
        assert!(check_archive(&other, &root.path().join("s2")).is_err());
        std::fs::write(&other, b"not an archive").unwrap();
        assert!(check_archive(&other, &root.path().join("s3")).is_err());
        assert!(!root.path().join("s3").exists());
    }

    #[test]
    fn a_schedule_is_due_by_its_interval_and_prunes_only_what_it_made() {
        let s = BackupSchedule { every_hours: 6, keep: 2 };
        assert!(s.due(100, None));
        assert!(!s.due(6 * 3600 - 1, Some(0)));
        assert!(s.due(6 * 3600, Some(0)));
        let mut stored = StoredSchedule { schedule: s, last_unix: None, made: Vec::new() };
        assert!(stored.record("a".into()).is_empty());
        assert!(stored.record("b".into()).is_empty());
        assert_eq!(stored.record("c".into()), vec!["a".to_string()]);
        assert_eq!(stored.made, vec!["b".to_string(), "c".to_string()]);
        assert!(BackupSchedule { every_hours: 0, keep: 2 }.validate().is_err());
        assert!(BackupSchedule { every_hours: 6, keep: 0 }.validate().is_err());

        let dir = tempfile::tempdir().unwrap();
        assert_eq!(load_schedule(dir.path()), None);
        save_schedule(dir.path(), Some(&stored)).unwrap();
        assert_eq!(load_schedule(dir.path()), Some(stored));
        save_schedule(dir.path(), None).unwrap();
        assert_eq!(load_schedule(dir.path()), None);
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
