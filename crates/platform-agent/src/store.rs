//! Where an instance's files live, and which of them a container may write.
//!
//! A game install is large — Sven Co-op is 2.74 GB — and a node runs many
//! instances of the same game. One copy per instance does not scale
//! (`DESIGN.md` §4), so content is stored **once per node, read-only**, and each
//! instance gets writable space only where the game says it needs it.
//!
//! # Why bind mounts and not overlayfs
//!
//! The obvious answer is an overlayfs upper layer per instance. Mounting
//! overlayfs *inside* a container needs `CAP_SYS_ADMIN`, and a game server —
//! third-party code, exposed to strangers on the internet, historically full of
//! memory-safety bugs — is the last process that should have it. So the plan is
//! a read-only bind of the shared content plus one writable bind per declared
//! path, nested inside it. That is why a pack has to declare `writable_paths`
//! rather than the planner guessing: the alternative is privileges nobody
//! should hand a game server.
//!
//! # Everything here is untrusted input
//!
//! `instance_id`, `game_id`, `version` and every writable path may come from a
//! pack a user dropped in a directory or from an API request. A planner that let
//! any of them escape the data root would produce a bind mount exposing an
//! arbitrary host directory to a container, writable — a node takeover, not a
//! bug. So every one is rejected rather than normalized, and the result is
//! additionally asserted to still sit under its expected parent: one check that
//! reasons about strings, one that reasons about the built path.
//!
//! Rejecting rather than normalizing is the deliberate half. `a/../b` could be
//! "cleaned" to `b`, but a pack that writes `a/../b` is either broken or
//! probing, and silently repairing it teaches nobody anything while leaving the
//! normalizer as the only thing between a hostile pack and the host.
//!
//! # The `logs/` directory is the agent's, not the game's
//!
//! It is created for every instance and deliberately **not** mounted into the
//! container. A game's own log directory is one of its `writable_paths` (Sven
//! declares `svencoop/logs`); this one is where the agent keeps what *it*
//! records about the instance, which a compromised game server must not be able
//! to rewrite.
//!
//! Drafted with GLM 5.2 against the layout spec, then reviewed and corrected.

use std::fmt;
use std::path::{Path, PathBuf};

pub const MAX_ID_LEN: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoreLayout {
    pub root: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContentRef {
    pub game_id: String,
    pub version: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstancePlan {
    pub instance_id: String,
    pub content: ContentRef,
    pub mounts: Vec<Mount>,
    pub host_dirs_to_create: Vec<PathBuf>,
    /// Directories that must already exist **inside the shared content copy**,
    /// because a writable mount is nested inside a read-only one.
    ///
    /// Found the hard way, by an integration test against a real daemon: runc
    /// creates a mountpoint if it is missing, and it cannot create one under a
    /// read-only bind. The failure is a 500 from Docker quoting `mkdirat ...
    /// read-only file system`, which says nothing about game content. So the
    /// agent checks these first and says which path a game install is missing.
    ///
    /// A real install already has them — Sven Co-op ships `svencoop/maps` and
    /// `svencoop/logs`. An install that does not is either incomplete or the
    /// pack is describing a different game's layout, and both are worth being
    /// told about plainly.
    pub required_content_dirs: Vec<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mount {
    pub host_path: PathBuf,
    pub container_path: PathBuf,
    pub read_only: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanError {
    EmptyId,
    IdTooLong,
    InvalidId,
    EmptyWritablePath,
    AbsoluteWritablePath,
    NulInPath,
    BackslashInPath,
    EmptyPathComponent,
    DotComponent,
    DotDotComponent,
    DuplicateWritablePath,
    NestedWritablePath,
    SlotCollision,
    EscapeAttempt,
    InvalidContainerRoot,
}

impl fmt::Display for PlanError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PlanError::EmptyId => write!(f, "id is empty"),
            PlanError::IdTooLong => write!(f, "id exceeds maximum length of {}", MAX_ID_LEN),
            PlanError::InvalidId => {
                write!(f, "id contains invalid characters or has invalid form")
            }
            PlanError::EmptyWritablePath => write!(f, "writable path is empty"),
            PlanError::AbsoluteWritablePath => {
                write!(f, "writable path must be relative, not absolute")
            }
            PlanError::NulInPath => write!(f, "writable path contains a NUL byte"),
            PlanError::BackslashInPath => write!(f, "writable path contains a backslash"),
            PlanError::EmptyPathComponent => {
                write!(f, "writable path contains an empty component")
            }
            PlanError::DotComponent => write!(f, "writable path contains a '.' component"),
            PlanError::DotDotComponent => write!(f, "writable path contains a '..' component"),
            PlanError::DuplicateWritablePath => write!(f, "duplicate writable path"),
            PlanError::NestedWritablePath => {
                write!(f, "a writable path is a prefix of another writable path")
            }
            PlanError::SlotCollision => {
                write!(f, "two distinct writable paths produce the same slot name")
            }
            PlanError::EscapeAttempt => write!(f, "resolved path escapes its target directory"),
            PlanError::InvalidContainerRoot => write!(f, "container content root is invalid"),
        }
    }
}

impl std::error::Error for PlanError {}

/// Delegates to `instance::validate_id` so the rule lives in exactly one place.
///
/// It previously had its own copy of the alphabet. Two implementations of "what
/// is a safe id" is precisely the thing that drifts, and the drift would be
/// silent: the store would accept an id the container namer rejects, or worse,
/// the other way round.
fn validate_id(id: &str) -> Result<(), PlanError> {
    use crate::instance::SpecError;
    crate::instance::validate_id(id).map_err(|e| match e {
        SpecError::EmptyId => PlanError::EmptyId,
        SpecError::IdTooLong(_) => PlanError::IdTooLong,
        _ => PlanError::InvalidId,
    })
}

fn validate_writable_path(p: &str) -> Result<(), PlanError> {
    if p.is_empty() {
        return Err(PlanError::EmptyWritablePath);
    }
    if p.contains('\0') {
        return Err(PlanError::NulInPath);
    }
    if p.contains('\\') {
        return Err(PlanError::BackslashInPath);
    }
    if p.starts_with('/') {
        return Err(PlanError::AbsoluteWritablePath);
    }
    for part in p.split('/') {
        if part.is_empty() {
            return Err(PlanError::EmptyPathComponent);
        }
        if part == "." {
            return Err(PlanError::DotComponent);
        }
        if part == ".." {
            return Err(PlanError::DotDotComponent);
        }
    }
    Ok(())
}

fn slot_name(path: &str) -> String {
    path.split('/').collect::<Vec<_>>().join("_")
}

impl StoreLayout {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        StoreLayout { root: root.into() }
    }

    pub fn content_dir(&self, c: &ContentRef) -> Result<PathBuf, PlanError> {
        validate_id(&c.game_id)?;
        validate_id(&c.version)?;
        Ok(self.root.join("content").join(&c.game_id).join(&c.version))
    }

    pub fn instance_dir(&self, instance_id: &str) -> Result<PathBuf, PlanError> {
        validate_id(instance_id)?;
        Ok(self.root.join("instances").join(instance_id))
    }

    pub fn plan_instance(
        &self,
        instance_id: &str,
        content: &ContentRef,
        container_content_root: &Path,
        writable_paths: &[String],
    ) -> Result<InstancePlan, PlanError> {
        validate_id(instance_id)?;
        let content_host = self.content_dir(content)?;

        if !container_content_root.is_absolute() {
            return Err(PlanError::InvalidContainerRoot);
        }
        if container_content_root.to_string_lossy().contains('\0') {
            return Err(PlanError::InvalidContainerRoot);
        }

        let instance_dir = self.root.join("instances").join(instance_id);
        let writable_root = instance_dir.join("writable");
        let logs_dir = instance_dir.join("logs");

        let mut parsed: Vec<Vec<String>> = Vec::with_capacity(writable_paths.len());
        for p in writable_paths {
            validate_writable_path(p)?;
            parsed.push(p.split('/').map(|s| s.to_string()).collect());
        }

        for i in 0..parsed.len() {
            for j in (i + 1)..parsed.len() {
                if parsed[i] == parsed[j] {
                    return Err(PlanError::DuplicateWritablePath);
                }
                let (shorter, longer) = if parsed[i].len() <= parsed[j].len() {
                    (&parsed[i], &parsed[j])
                } else {
                    (&parsed[j], &parsed[i])
                };
                if shorter.len() < longer.len() && longer[..shorter.len()] == *shorter {
                    return Err(PlanError::NestedWritablePath);
                }
            }
        }

        let mut slots: Vec<String> = Vec::with_capacity(writable_paths.len());
        for p in writable_paths {
            let s = slot_name(p);
            if slots.contains(&s) {
                return Err(PlanError::SlotCollision);
            }
            slots.push(s);
        }

        let mut mounts = Vec::with_capacity(1 + writable_paths.len());
        mounts.push(Mount {
            host_path: content_host,
            container_path: container_content_root.to_path_buf(),
            read_only: true,
        });

        for (p, s) in writable_paths.iter().zip(slots.iter()) {
            let host_path = writable_root.join(s);
            if !host_path.starts_with(&writable_root) {
                return Err(PlanError::EscapeAttempt);
            }
            let container_path = container_content_root.join(p);
            if !container_path.starts_with(container_content_root) {
                return Err(PlanError::EscapeAttempt);
            }
            mounts.push(Mount {
                host_path,
                container_path,
                read_only: false,
            });
        }

        // Parents before children: a caller creating these in order never has
        // to think about `create_dir_all` versus `create_dir`.
        let mut host_dirs_to_create = Vec::new();
        host_dirs_to_create.push(instance_dir.clone());
        host_dirs_to_create.push(writable_root.clone());
        for s in &slots {
            host_dirs_to_create.push(writable_root.join(s));
        }
        host_dirs_to_create.push(logs_dir.clone());

        let content_host_dir = self.content_dir(content)?;
        let required_content_dirs = writable_paths
            .iter()
            .map(|p| content_host_dir.join(p))
            .collect();

        Ok(InstancePlan {
            instance_id: instance_id.to_string(),
            content: content.clone(),
            mounts,
            host_dirs_to_create,
            required_content_dirs,
        })
    }

    /// Which of a plan's `required_content_dirs` are not on disk.
    ///
    /// The only filesystem-touching function in this module, and it only reads.
    /// It deliberately does **not** create the missing directories: they live in
    /// the shared content copy that every instance of this game reads from, and
    /// an instance start is the wrong moment to be writing there. Provisioning
    /// content is a separate, explicit act.
    pub fn missing_content_dirs(plan: &InstancePlan) -> Vec<PathBuf> {
        plan.required_content_dirs
            .iter()
            .filter(|p| !p.is_dir())
            .cloned()
            .collect()
    }

    pub fn orphan_instance_dirs(&self, keep: &[String], entries: &[String]) -> Vec<PathBuf> {
        entries
            .iter()
            .filter(|e| !keep.iter().any(|k| k == *e))
            .filter_map(|e| self.instance_dir(e).ok())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layout() -> StoreLayout {
        StoreLayout::new("/data")
    }

    fn sven_content() -> ContentRef {
        ContentRef {
            game_id: "sven-coop".into(),
            version: "1.0".into(),
        }
    }

    #[test]
    fn valid_plan() {
        let layout = layout();
        let content = sven_content();
        let plan = layout
            .plan_instance(
                "inst1",
                &content,
                Path::new("/game"),
                &["svencoop/maps".into(), "svencoop/logs".into()],
            )
            .unwrap();

        assert_eq!(plan.mounts.len(), 3);
        assert_eq!(
            plan.mounts[0].host_path,
            PathBuf::from("/data/content/sven-coop/1.0")
        );
        assert_eq!(plan.mounts[0].container_path, PathBuf::from("/game"));
        assert!(plan.mounts[0].read_only);
        assert_eq!(
            plan.mounts[1].host_path,
            PathBuf::from("/data/instances/inst1/writable/svencoop_maps")
        );
        assert_eq!(
            plan.mounts[1].container_path,
            PathBuf::from("/game/svencoop/maps")
        );
        assert!(!plan.mounts[1].read_only);
        assert_eq!(
            plan.mounts[2].host_path,
            PathBuf::from("/data/instances/inst1/writable/svencoop_logs")
        );
        assert_eq!(
            plan.mounts[2].container_path,
            PathBuf::from("/game/svencoop/logs")
        );
        assert!(!plan.mounts[2].read_only);
    }

    #[test]
    fn content_read_only_writable_not() {
        let layout = layout();
        let plan = layout
            .plan_instance(
                "inst1",
                &sven_content(),
                Path::new("/game"),
                &["svencoop/maps".into()],
            )
            .unwrap();
        assert!(plan.mounts[0].read_only, "content must be read-only");
        for m in &plan.mounts[1..] {
            assert!(!m.read_only, "writable mount must not be read-only: {:?}", m);
        }
    }

    #[test]
    fn host_dirs_parents_before_children_and_logs() {
        let layout = layout();
        let plan = layout
            .plan_instance(
                "inst1",
                &sven_content(),
                Path::new("/game"),
                &["svencoop/maps".into()],
            )
            .unwrap();
        let dirs = &plan.host_dirs_to_create;
        for i in 0..dirs.len() {
            for j in (i + 1)..dirs.len() {
                if dirs[i].starts_with(&dirs[j]) && dirs[i] != dirs[j] {
                    panic!("parent {:?} appears after child {:?}", dirs[j], dirs[i]);
                }
            }
        }
        assert!(dirs.contains(&PathBuf::from("/data/instances/inst1")));
        assert!(dirs.contains(&PathBuf::from("/data/instances/inst1/writable")));
        assert!(dirs.contains(&PathBuf::from(
            "/data/instances/inst1/writable/svencoop_maps"
        )));
        assert!(dirs.contains(&PathBuf::from("/data/instances/inst1/logs")));
    }

    #[test]
    fn logs_always_planned() {
        let layout = layout();
        let plan = layout
            .plan_instance("inst1", &sven_content(), Path::new("/game"), &[])
            .unwrap();
        assert!(plan
            .host_dirs_to_create
            .contains(&PathBuf::from("/data/instances/inst1/logs")));
        assert_eq!(plan.mounts.len(), 1);
    }

    #[test]
    fn rejects_invalid_ids() {
        let layout = layout();
        let content = sven_content();
        let long_id = "a".repeat(65);
        let cases: Vec<(&str, PlanError)> = vec![
            ("", PlanError::EmptyId),
            (&long_id, PlanError::IdTooLong),
            ("UPPER", PlanError::InvalidId),
            ("has space", PlanError::InvalidId),
            ("has/slash", PlanError::InvalidId),
            ("..", PlanError::InvalidId),
            (".", PlanError::InvalidId),
            (".hidden", PlanError::InvalidId),
        ];
        for (id, expected) in &cases {
            let result = layout.plan_instance(id, &content, Path::new("/game"), &[]);
            assert_eq!(result, Err(expected.clone()), "id {:?}", id);
        }
    }

    #[test]
    fn rejects_invalid_writable_paths() {
        let layout = layout();
        let content = sven_content();
        let cases: Vec<(&str, PlanError)> = vec![
            ("/etc", PlanError::AbsoluteWritablePath),
            ("", PlanError::EmptyWritablePath),
            ("a/../../etc", PlanError::DotDotComponent),
            ("../x", PlanError::DotDotComponent),
            ("a//b", PlanError::EmptyPathComponent),
            ("a/./b", PlanError::DotComponent),
            ("a\\b", PlanError::BackslashInPath),
        ];
        for (p, expected) in &cases {
            let result =
                layout.plan_instance("inst1", &content, Path::new("/game"), &[p.to_string()]);
            assert_eq!(result, Err(expected.clone()), "path {:?}", p);
        }
        let result =
            layout.plan_instance("inst1", &content, Path::new("/game"), &["a\0b".to_string()]);
        assert_eq!(result, Err(PlanError::NulInPath));
    }

    #[test]
    fn rejects_prefix_and_duplicate() {
        let layout = layout();
        let content = sven_content();
        let r1 = layout.plan_instance(
            "inst1",
            &content,
            Path::new("/game"),
            &["a".to_string(), "a/b".to_string()],
        );
        assert_eq!(r1, Err(PlanError::NestedWritablePath));
        let r2 = layout.plan_instance(
            "inst1",
            &content,
            Path::new("/game"),
            &["a/b".to_string(), "a/b".to_string()],
        );
        assert_eq!(r2, Err(PlanError::DuplicateWritablePath));
    }

    #[test]
    fn rejects_slot_collision() {
        let layout = layout();
        let content = sven_content();
        let r = layout.plan_instance(
            "inst1",
            &content,
            Path::new("/game"),
            &["a_b".to_string(), "a/b".to_string()],
        );
        assert_eq!(r, Err(PlanError::SlotCollision));
    }

    fn normalize(path: &Path) -> PathBuf {
        let mut stack: Vec<&std::ffi::OsStr> = Vec::new();
        let mut is_absolute = false;
        for c in path.components() {
            match c {
                std::path::Component::RootDir => {
                    is_absolute = true;
                    stack.clear();
                }
                std::path::Component::Normal(s) => stack.push(s),
                std::path::Component::ParentDir => {
                    stack.pop();
                }
                std::path::Component::CurDir => {}
                std::path::Component::Prefix(_) => {}
            }
        }
        let mut result = PathBuf::new();
        if is_absolute {
            result.push("/");
        }
        for s in &stack {
            result.push(s);
        }
        result
    }

    fn is_strictly_within(path: &Path, base: &Path) -> bool {
        let norm = normalize(path);
        let base_norm = normalize(base);
        norm != base_norm && norm.starts_with(&base_norm)
    }

    #[test]
    fn traversal_cannot_escape() {
        let layout = layout();
        let content = sven_content();
        let base = Path::new("/data/instances/inst1/writable");
        let traversal_inputs = ["/etc", "a/../../etc", "../x", "../../x"];
        for input in &traversal_inputs {
            let joined = base.join(input);
            assert!(
                !is_strictly_within(&joined, base),
                "input {:?} joined = {:?} should escape base {:?}",
                input,
                joined,
                base
            );
            let result =
                layout.plan_instance("inst1", &content, Path::new("/game"), &[input.to_string()]);
            assert!(result.is_err(), "input {:?} should be rejected, got {:?}", input, result);
        }
    }

    #[test]
    fn two_instances_share_content() {
        let layout = layout();
        let content = sven_content();
        let p1 = layout
            .plan_instance(
                "inst1",
                &content,
                Path::new("/game"),
                &["svencoop/maps".into()],
            )
            .unwrap();
        let p2 = layout
            .plan_instance(
                "inst2",
                &content,
                Path::new("/game"),
                &["svencoop/maps".into()],
            )
            .unwrap();
        assert_eq!(p1.mounts[0].host_path, p2.mounts[0].host_path);
        assert_ne!(p1.mounts[1].host_path, p2.mounts[1].host_path);
    }

    #[test]
    fn same_instance_stable() {
        let layout = layout();
        let content = sven_content();
        let p1 = layout
            .plan_instance(
                "inst1",
                &content,
                Path::new("/game"),
                &["a/b".into(), "c/d".into()],
            )
            .unwrap();
        let p2 = layout
            .plan_instance(
                "inst1",
                &content,
                Path::new("/game"),
                &["a/b".into(), "c/d".into()],
            )
            .unwrap();
        assert_eq!(p1.mounts, p2.mounts);
    }

    #[test]
    fn reorder_preserves_slot_host_paths() {
        let layout = layout();
        let content = sven_content();
        let p1 = layout
            .plan_instance(
                "inst1",
                &content,
                Path::new("/game"),
                &["a/b".into(), "c/d".into()],
            )
            .unwrap();
        let p2 = layout
            .plan_instance(
                "inst1",
                &content,
                Path::new("/game"),
                &["c/d".into(), "a/b".into()],
            )
            .unwrap();
        let find = |plan: &InstancePlan, suffix: &str| {
            let target = Path::new("/game").join(suffix);
            plan.mounts
                .iter()
                .find(|m| m.container_path == target)
                .map(|m| m.host_path.clone())
                .expect("mount not found")
        };
        assert_eq!(find(&p1, "a/b"), find(&p2, "a/b"));
        assert_eq!(find(&p1, "c/d"), find(&p2, "c/d"));
    }

    #[test]
    fn orphan_instance_dirs_basic() {
        let layout = layout();
        let keep = vec!["a".to_string(), "b".to_string()];
        let entries = vec![
            "a".to_string(),
            "b".to_string(),
            "c".to_string(),
            "d".to_string(),
        ];
        let orphans = layout.orphan_instance_dirs(&keep, &entries);
        assert_eq!(
            orphans,
            vec![
                PathBuf::from("/data/instances/c"),
                PathBuf::from("/data/instances/d"),
            ]
        );
    }

    #[test]
    fn orphan_instance_dirs_all_kept() {
        let layout = layout();
        let entries = vec!["a".to_string(), "b".to_string()];
        let orphans = layout.orphan_instance_dirs(&entries, &entries);
        assert!(orphans.is_empty());
    }
}
