//! The ban list a node keeps for a game that cannot keep its own
//! (`MINECRAFT.md` §4).
//!
//! GoldSrc and Source write `banned.cfg` into the game's own directory, which
//! on a node is the shared, read-only copy, so `writeid` saves nothing and a
//! restart forgets every ban. The node keeps the list instead, as
//! `bans.json` beside the instance (never on the container: a ban added later
//! must survive a recreate), and sends `banid` for each entry after every
//! start. Minecraft keeps its own list in its server directory, and the node
//! only reads that one.

use std::path::Path;

use serde::{Deserialize, Serialize};

pub const FILE: &str = "bans.json";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ban {
    /// What the ban names: a Steam ID here, a Minecraft name when read from
    /// the game's own file.
    pub target: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Seconds since the epoch, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub at: Option<u64>,
}

/// The node's list for one instance. A missing file is an empty list; a file
/// that does not parse is an error, so a damaged list is never silently
/// replaced by an empty one on the next ban.
pub fn read(instance_dir: &Path) -> std::io::Result<Vec<Ban>> {
    match std::fs::read_to_string(instance_dir.join(FILE)) {
        Ok(text) => serde_json::from_str(&text).map_err(std::io::Error::other),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(e),
    }
}

/// Write the list beside and rename it into place, so a crash never leaves a
/// half-written list.
pub fn write(instance_dir: &Path, bans: &[Ban]) -> std::io::Result<()> {
    let tmp = instance_dir.join(format!(".{FILE}.tmp"));
    std::fs::write(&tmp, serde_json::to_vec_pretty(bans).map_err(std::io::Error::other)?)?;
    std::fs::rename(tmp, instance_dir.join(FILE))
}

/// Add a ban, replacing any earlier one for the same target.
pub fn add(instance_dir: &Path, ban: Ban) -> std::io::Result<()> {
    let mut bans = read(instance_dir)?;
    bans.retain(|b| b.target != ban.target);
    bans.push(ban);
    write(instance_dir, &bans)
}

/// Remove a ban. Removing one that is not there is not an error.
pub fn remove(instance_dir: &Path, target: &str) -> std::io::Result<()> {
    let mut bans = read(instance_dir)?;
    let before = bans.len();
    bans.retain(|b| b.target != target);
    if bans.len() != before {
        write(instance_dir, &bans)?;
    }
    Ok(())
}

/// Minecraft's own `banned-players.json` or `ops.json`, as names. A file that
/// is not there is an empty list (a new server has none yet).
pub fn read_minecraft_list(path: &Path) -> std::io::Result<Vec<Ban>> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let v: serde_json::Value = serde_json::from_str(&text).map_err(std::io::Error::other)?;
    Ok(v.as_array()
        .map(|list| {
            list.iter()
                .filter_map(|e| {
                    Some(Ban {
                        target: e["name"].as_str()?.to_string(),
                        reason: e["reason"].as_str().map(str::to_string),
                        at: None,
                    })
                })
                .collect()
        })
        .unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_record_adds_replaces_and_removes_and_a_damaged_one_is_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path();
        assert!(read(d).unwrap().is_empty());
        add(d, Ban { target: "STEAM_0:1:1".into(), reason: Some("cheat".into()), at: Some(1) }).unwrap();
        add(d, Ban { target: "STEAM_0:1:2".into(), reason: None, at: None }).unwrap();
        add(d, Ban { target: "STEAM_0:1:1".into(), reason: Some("again".into()), at: Some(2) }).unwrap();
        let bans = read(d).unwrap();
        assert_eq!(bans.len(), 2);
        assert_eq!(bans.iter().find(|b| b.target == "STEAM_0:1:1").unwrap().reason.as_deref(), Some("again"));
        remove(d, "STEAM_0:1:2").unwrap();
        remove(d, "STEAM_0:1:9").unwrap();
        assert_eq!(read(d).unwrap().len(), 1);

        std::fs::write(d.join(FILE), b"{ not json").unwrap();
        assert!(read(d).is_err());
        assert!(add(d, Ban { target: "STEAM_0:1:3".into(), reason: None, at: None }).is_err(), "never overwritten as empty");
    }

    #[test]
    fn minecrafts_own_lists_are_read_as_names() {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("banned-players.json");
        assert!(read_minecraft_list(&f).unwrap().is_empty());
        std::fs::write(&f, r#"[{"uuid":"x","name":"Griefer","created":"2026-10-18","source":"Server","expires":"forever","reason":"Banned by an operator."}]"#).unwrap();
        let list = read_minecraft_list(&f).unwrap();
        assert_eq!(list[0].target, "Griefer");
        assert_eq!(list[0].reason.as_deref(), Some("Banned by an operator."));
    }
}
