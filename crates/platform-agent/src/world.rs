//! Bringing a Minecraft world onto a server (`MINECRAFT.md` §6).
//!
//! The caller has already stopped the server and backed up what is there; this
//! checks the upload and swaps it in. Rules:
//!
//! - **Extracted under `content::extract`'s path checks**, zip or tar, into a
//!   staging directory, never into the server's own.
//! - **No links at all.** A world is files; a link in one is a question
//!   nobody asked.
//! - **A world is a directory holding `level.dat`**: at the top of the
//!   archive, or in folders at the top or under one wrapping folder. One such
//!   folder is the world. Two or more are refused, except the set Paper and
//!   Spigot write (`X`, `X_nether`, `X_the_end`), which is one world in three
//!   folders.
//! - Nothing in the server directory changes until all of that has passed.

use std::fs;
use std::path::{Path, PathBuf};

/// The folder names a server uses for its world, the only ones an import
/// replaces. The image sets `level-name=world`.
pub const WORLD_DIRS: [&str; 3] = ["world", "world_nether", "world_the_end"];

/// Where each part of an uploaded world goes: `(found folder, name in the
/// server directory)`.
pub type WorldParts = Vec<(PathBuf, &'static str)>;

/// Find the world in an extracted upload.
pub fn find_world(root: &Path) -> Result<WorldParts, String> {
    if root.join("level.dat").is_file() {
        return Ok(vec![(root.to_path_buf(), "world")]);
    }
    let mut base = root.to_path_buf();
    let mut found = worlds_in(&base);
    if found.is_empty() {
        // One wrapping folder, as a zip of a folder usually is.
        let entries = dirs_in(root);
        if entries.len() == 1 {
            base = entries[0].clone();
            if base.join("level.dat").is_file() {
                return Ok(vec![(base, "world")]);
            }
            found = worlds_in(&base);
        }
    }
    match found.len() {
        0 => Err("there is no world in this file: a world is a folder with level.dat in it".to_string()),
        1 => Ok(vec![(found.remove(0), "world")]),
        _ => bukkit_set(&found).ok_or_else(|| {
            let names: Vec<String> =
                found.iter().filter_map(|p| p.file_name().map(|n| n.to_string_lossy().to_string())).collect();
            format!("this file holds more than one world ({}); upload one at a time", names.join(", "))
        }),
    }
}

/// `X`, `X_nether` and `X_the_end`, as Paper and Spigot keep one world.
fn bukkit_set(found: &[PathBuf]) -> Option<WorldParts> {
    let name = |p: &PathBuf| p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    let names: Vec<String> = found.iter().map(name).collect();
    let main = names.iter().find(|n| {
        names.iter().all(|other| other == *n || *other == format!("{n}_nether") || *other == format!("{n}_the_end"))
    })?;
    let mut parts = Vec::new();
    for (p, n) in found.iter().zip(&names) {
        let to = if n == main {
            "world"
        } else if n.ends_with("_nether") {
            "world_nether"
        } else {
            "world_the_end"
        };
        parts.push((p.clone(), to));
    }
    Some(parts)
}

fn dirs_in(dir: &Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = fs::read_dir(dir)
        .map(|r| r.flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect())
        .unwrap_or_default();
    out.sort();
    out
}

fn worlds_in(dir: &Path) -> Vec<PathBuf> {
    dirs_in(dir).into_iter().filter(|d| d.join("level.dat").is_file()).collect()
}

/// Whether any link is anywhere under `dir`.
pub fn has_link(dir: &Path) -> bool {
    let Ok(entries) = fs::read_dir(dir) else { return false };
    for e in entries.flatten() {
        let Ok(kind) = e.file_type() else { continue };
        if kind.is_symlink() || (kind.is_dir() && has_link(&e.path())) {
            return true;
        }
    }
    false
}

/// Check an upload and swap it in as `server_dir`'s world. `staging` must not
/// exist and is removed afterwards, whatever happens. The server must be
/// stopped and its files backed up by the caller.
pub fn import(archive: &Path, server_dir: &Path, staging: &Path) -> Result<Vec<&'static str>, String> {
    let result = import_inner(archive, server_dir, staging);
    let _ = fs::remove_dir_all(staging);
    result
}

fn import_inner(archive: &Path, server_dir: &Path, staging: &Path) -> Result<Vec<&'static str>, String> {
    let unpacked = staging.join("unpacked");
    fs::create_dir_all(&unpacked).map_err(|e| e.to_string())?;
    crate::content::extract_any(archive, &unpacked).map_err(|e| format!("this file cannot be read as a world: {e}"))?;
    if has_link(&unpacked) {
        return Err("this file holds a link; a world is plain files".to_string());
    }
    let parts = find_world(&unpacked)?;
    fs::create_dir_all(server_dir).map_err(|e| e.to_string())?;
    // Everything has passed; only now does the server's directory change.
    for name in WORLD_DIRS {
        let old = server_dir.join(name);
        if old.exists() {
            fs::remove_dir_all(&old).map_err(|e| format!("removing the old {name}: {e}"))?;
        }
    }
    let mut placed = Vec::new();
    for (from, to) in parts {
        fs::rename(&from, server_dir.join(to)).map_err(|e| format!("placing {to}: {e}"))?;
        // A lock file copied from a running game makes the server refuse the
        // world; the server writes a fresh one.
        let _ = fs::remove_file(server_dir.join(to).join("session.lock"));
        placed.push(to);
    }
    Ok(placed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn zip_of(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut buf = std::io::Cursor::new(Vec::new());
        {
            let mut z = zip::ZipWriter::new(&mut buf);
            let opts = zip::write::SimpleFileOptions::default();
            for (name, body) in entries {
                if name.ends_with('/') {
                    z.add_directory(name.trim_end_matches('/'), opts).unwrap();
                } else {
                    z.start_file(*name, opts).unwrap();
                    z.write_all(body).unwrap();
                }
            }
            z.finish().unwrap();
        }
        buf.into_inner()
    }

    fn setup(tmp: &tempfile::TempDir, archive: &[u8]) -> (PathBuf, PathBuf, PathBuf) {
        let a = tmp.path().join("upload");
        fs::write(&a, archive).unwrap();
        let server = tmp.path().join("server");
        fs::create_dir_all(server.join("world")).unwrap();
        fs::write(server.join("world/level.dat"), b"old").unwrap();
        fs::write(server.join("server.properties"), b"keep").unwrap();
        (a, server, tmp.path().join("staging"))
    }

    #[test]
    fn a_world_in_a_wrapping_folder_replaces_the_old_one_and_nothing_else() {
        let tmp = tempfile::tempdir().unwrap();
        let (a, server, staging) = setup(
            &tmp,
            &zip_of(&[("My World/", b""), ("My World/level.dat", b"new"), ("My World/region/r.0.0.mca", b"x"), ("My World/session.lock", b"l")]),
        );
        assert_eq!(import(&a, &server, &staging).unwrap(), ["world"]);
        assert_eq!(fs::read(server.join("world/level.dat")).unwrap(), b"new");
        assert!(server.join("world/region/r.0.0.mca").is_file());
        assert!(!server.join("world/session.lock").exists());
        assert_eq!(fs::read(server.join("server.properties")).unwrap(), b"keep");
        assert!(!staging.exists());
    }

    #[test]
    fn papers_three_folders_are_one_world() {
        let tmp = tempfile::tempdir().unwrap();
        let (a, server, staging) = setup(
            &tmp,
            &zip_of(&[
                ("smp/level.dat", b"main"),
                ("smp_nether/level.dat", b"n"),
                ("smp_the_end/level.dat", b"e"),
            ]),
        );
        let mut placed = import(&a, &server, &staging).unwrap();
        placed.sort();
        assert_eq!(placed, ["world", "world_nether", "world_the_end"]);
        assert_eq!(fs::read(server.join("world/level.dat")).unwrap(), b"main");
    }

    #[test]
    fn no_world_two_worlds_or_an_escape_change_nothing() {
        for archive in [
            zip_of(&[("readme.txt", b"hi")]),
            zip_of(&[("a/level.dat", b"1"), ("b/level.dat", b"2")]),
            zip_of(&[("../evil/level.dat", b"x")]),
            zip_of(&[("/abs/level.dat", b"x")]),
            b"not an archive at all".to_vec(),
        ] {
            let tmp = tempfile::tempdir().unwrap();
            let (a, server, staging) = setup(&tmp, &archive);
            assert!(import(&a, &server, &staging).is_err());
            assert_eq!(fs::read(server.join("world/level.dat")).unwrap(), b"old", "the old world is untouched");
            assert!(!tmp.path().join("evil").exists() && !staging.exists());
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_link_inside_a_world_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let mut b = tar::Builder::new(Vec::new());
        let mut h = tar::Header::new_gnu();
        h.set_size(3);
        h.set_cksum();
        b.append_data(&mut h, "w/level.dat", &b"abc"[..]).unwrap();
        let mut l = tar::Header::new_gnu();
        l.set_entry_type(tar::EntryType::Symlink);
        l.set_size(0);
        b.append_link(&mut l, "w/link", "level.dat").unwrap();
        let (a, server, staging) = setup(&tmp, &b.into_inner().unwrap());
        let err = import(&a, &server, &staging).unwrap_err();
        assert!(err.contains("link"), "{err}");
        assert_eq!(fs::read(server.join("world/level.dat")).unwrap(), b"old");
    }
}
