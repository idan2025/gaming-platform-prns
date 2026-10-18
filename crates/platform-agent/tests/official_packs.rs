//! The official pack repository on a node (`official.rs`), against a real
//! Docker daemon: a new game arrives by itself, a pack is never replaced
//! while a server of its game exists, and a list signed by anyone but the
//! repository's key changes nothing. Skips itself without Docker. Ports
//! 27236–27238 are its own.

use std::path::Path;
use std::time::{Duration, SystemTime};

use game_bridge::repo::{RepoIndex, INDEX_FILE};
use game_bridge::signing::{PackSignature, SIGNATURE_SUFFIX};
use platform_agent::agent::Agent;
use platform_agent::config::AgentConfig;
use platform_agent::instance::InstanceSpec;
use platform_agent::official::Official;
use prns_core::identity::PrivateIdentityMaterial;

const GAME: &str = "gpp-test-official:latest";
const GAME_DOCKERFILE: &str =
    "FROM busybox\nCMD [\"sh\", \"-c\", \"trap 'exit 0' TERM; while true; do sleep 1; done\"]\n";

fn ensure_image() -> bool {
    let present = std::process::Command::new("docker")
        .args(["image", "inspect", GAME])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    if matches!(present, Ok(s) if s.success()) {
        return true;
    }
    let dir = std::env::temp_dir().join(format!("gpp-official-{}", std::process::id()));
    if std::fs::create_dir_all(&dir).is_err()
        || std::fs::write(dir.join("Dockerfile"), GAME_DOCKERFILE).is_err()
    {
        return false;
    }
    let built = std::process::Command::new("docker")
        .args(["build", "-q", "-t", GAME, "."])
        .current_dir(&dir)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    let _ = std::fs::remove_dir_all(&dir);
    matches!(built, Ok(s) if s.success())
}

struct Container(String);

impl Drop for Container {
    fn drop(&mut self) {
        let _ = std::process::Command::new("docker")
            .args(["rm", "-f", &format!("gpp-{}", self.0)])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
}

fn key(seed: u8) -> PrivateIdentityMaterial {
    PrivateIdentityMaterial::from_slice(&[seed; 64]).unwrap()
}

/// A repository in `dir`: every pack and the list signed by `seed`.
fn publish(dir: &Path, seed: u8) {
    let now = SystemTime::now() - Duration::from_secs(60);
    for e in std::fs::read_dir(dir).unwrap() {
        let p = e.unwrap().path();
        if p.extension().and_then(|e| e.to_str()) == Some("toml") {
            let days = Duration::from_secs(game_bridge::repo::PACK_SIGNATURE_DAYS * 86_400);
            let sig = PackSignature::sign(&std::fs::read(&p).unwrap(), &key(seed), now, days).unwrap();
            std::fs::write(format!("{}{SIGNATURE_SUFFIX}", p.display()), sig.to_toml()).unwrap();
        }
    }
    sign_list(dir, seed);
}

fn sign_list(dir: &Path, seed: u8) {
    let index = RepoIndex::build(dir, "Official").unwrap();
    let json = serde_json::to_vec(&index).unwrap();
    std::fs::write(dir.join(INDEX_FILE), &json).unwrap();
    let now = SystemTime::now() - Duration::from_secs(60);
    let sig = PackSignature::sign_index(&json, &key(seed), now, Duration::from_secs(86_400)).unwrap();
    std::fs::write(dir.join(format!("{INDEX_FILE}{SIGNATURE_SUFFIX}")), sig.to_toml()).unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn official_packs_arrive_and_never_change_under_a_server() {
    if !ensure_image() {
        eprintln!("skipping: no Docker daemon");
        return;
    }
    let shipped = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packs");
    let root = tempfile::tempdir().unwrap();
    let repo = root.path().join("repo");
    let packs = root.path().join("packs");
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::create_dir_all(&packs).unwrap();
    for f in ["sven-coop.toml", "half-life.toml"] {
        std::fs::copy(shipped.join(f), repo.join(f)).unwrap();
    }
    publish(&repo, 5);
    // The node already has Sven Co-op, unsigned, as every node did before.
    std::fs::copy(shipped.join("sven-coop.toml"), packs.join("sven-coop.toml")).unwrap();
    let content = root.path().join("content/sven-coop/test/svencoop");
    for sub in ["maps/soundcache", "logs", "scripts"] {
        std::fs::create_dir_all(content.join(sub)).unwrap();
    }
    let toml = format!(
        r#"
data_root = "{}"
max_instances = 2
api_bind = "127.0.0.1:0"

[port_range]
start = 27236
end = 27238

[games.sven-coop]
image = "{GAME}"
content_root = "/game"
content_version = "test"
"#,
        root.path().display()
    );
    let config = AgentConfig::parse(&toml).unwrap();
    let mut agent = Agent::new(config, vec![game_bridge::GamePack::sven_coop()]).await.unwrap();
    let url = game_bridge::repo::check_repo_url(&repo.to_string_lossy()).unwrap();
    agent.set_official(Official::with_source(root.path(), &url, key(5).public().identity_hash()));
    let agent = std::sync::Arc::new(agent);
    assert!(agent.official().on(), "on by default");

    let id = format!("official{}", std::process::id());
    let _cleanup = Container(id.clone());
    agent
        .create(InstanceSpec {
            instance_id: id.clone(),
            game_id: "sven-coop".to_string(),
            name: "official test".to_string(),
            max_players: 8,
            port: None,
            extra_ports: Default::default(),
            map: None,
            bots: None,
            restart_on_crash: false,
            owner: None,
            options: Default::default(),
        })
        .await
        .unwrap();
    let before = std::fs::read(packs.join("sven-coop.toml")).unwrap();

    let first = agent.official().sync(&agent, &packs).await;
    assert!(first.error.is_none(), "{first:?}");
    assert_eq!(first.added, vec!["half-life".to_string()], "a new game arrives by itself");
    assert_eq!(first.waiting, vec!["sven-coop".to_string()], "a server of it exists: the update waits");
    assert!(packs.join("half-life.toml.sig").exists());
    assert_eq!(std::fs::read(packs.join("sven-coop.toml")).unwrap(), before);
    assert!(!packs.join("sven-coop.toml.sig").exists(), "nothing changed under the server");

    agent.remove(&id).await.unwrap();
    let second = agent.official().sync(&agent, &packs).await;
    assert_eq!(second.updated, vec!["sven-coop".to_string()], "{second:?}");
    assert!(packs.join("sven-coop.toml.sig").exists(), "the signed copy, once no server uses it");
    let third = agent.official().sync(&agent, &packs).await;
    assert!(third.added.is_empty() && third.updated.is_empty(), "{third:?}");

    // Someone else signs the list: refused, nothing taken.
    std::fs::write(repo.join("sven-coop.toml"), "# changed\n".to_string() + &String::from_utf8(before).unwrap()).unwrap();
    sign_list(&repo, 6);
    let refused = agent.official().sync(&agent, &packs).await;
    assert!(refused.error.is_some() && refused.updated.is_empty(), "{refused:?}");

    agent.official().set_on(false).unwrap();
    assert!(!agent.official().sync(&agent, &packs).await.on);
}
