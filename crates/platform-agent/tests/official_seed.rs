//! A node seeding the official packs over Reticulum (`official.rs`): the
//! pass mirrors the list exactly into `<data_root>/mirrors/official` and
//! serves it through the node's own hub, so a launcher on the mesh reads the
//! same signed bytes with no internet. Skips itself without Docker, because
//! `Agent::new` asks the daemon.

use std::path::Path;
use std::time::{Duration, SystemTime};

use game_bridge::config::BrowserArgs;
use game_bridge::repo::{RepoIndex, INDEX_FILE};
use game_bridge::signing::{PackSignature, SIGNATURE_SUFFIX};
use game_bridge::BridgeSession;
use platform_agent::agent::Agent;
use platform_agent::config::AgentConfig;
use platform_agent::official::{Official, MIRROR_DIR};
use prns_core::identity::PrivateIdentityMaterial;

fn key() -> PrivateIdentityMaterial {
    PrivateIdentityMaterial::from_slice(&[9; 64]).unwrap()
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

fn publish(dir: &Path) {
    let shipped = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packs");
    let now = SystemTime::now() - Duration::from_secs(60);
    for f in ["sven-coop.toml", "half-life.toml"] {
        std::fs::copy(shipped.join(f), dir.join(f)).unwrap();
        let days = Duration::from_secs(game_bridge::repo::PACK_SIGNATURE_DAYS * 86_400);
        let sig = PackSignature::sign(&std::fs::read(dir.join(f)).unwrap(), &key(), now, days).unwrap();
        std::fs::write(dir.join(format!("{f}{SIGNATURE_SUFFIX}")), sig.to_toml()).unwrap();
    }
    let json = serde_json::to_vec(&RepoIndex::build(dir, "Official").unwrap()).unwrap();
    std::fs::write(dir.join(INDEX_FILE), &json).unwrap();
    let sig = PackSignature::sign_index(&json, &key(), now, Duration::from_secs(86_400)).unwrap();
    std::fs::write(dir.join(format!("{INDEX_FILE}{SIGNATURE_SUFFIX}")), sig.to_toml()).unwrap();
}

fn listing(dir: &Path) -> Vec<(String, Vec<u8>)> {
    let mut out: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| {
            let p = e.unwrap().path();
            (p.file_name().unwrap().to_string_lossy().into_owned(), std::fs::read(&p).unwrap())
        })
        .collect();
    out.sort();
    out
}

#[tokio::test(flavor = "multi_thread")]
async fn a_node_seeds_the_official_packs_through_its_own_hub() {
    let root = tempfile::tempdir().unwrap();
    let repo = root.path().join("repo");
    let packs = root.path().join("packs");
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::create_dir_all(&packs).unwrap();
    publish(&repo);
    let mesh_port = free_port();
    let toml = format!(
        r#"
data_root = "{}"
max_instances = 1
api_bind = "127.0.0.1:0"

[port_range]
start = 27240
end = 27241

[mesh]
tcp = "0.0.0.0:{mesh_port}"
shared_instance_port = {}
"#,
        root.path().display(),
        free_port(),
    );
    let config = AgentConfig::parse(&toml).unwrap();
    let Ok(mut agent) = Agent::new(config, vec![]).await else {
        eprintln!("skipping: no Docker daemon");
        return;
    };
    let url = game_bridge::repo::check_repo_url(&repo.to_string_lossy()).unwrap();
    agent.set_official(Official::with_source(root.path(), &url, key().public().identity_hash()));
    let agent = std::sync::Arc::new(agent);
    assert!(!agent.official().seeding(), "off unless the operator turns it on");
    let quiet = agent.official().sync(&agent, &packs).await;
    assert!(quiet.seed.address.is_none() && !root.path().join(MIRROR_DIR).exists());

    agent.official().set_seeding(true).unwrap();
    let status = agent.official().sync(&agent, &packs).await;
    assert!(status.seed.error.is_none(), "{status:?}");
    assert_eq!(status.seed.packs, Some(2));
    let address = status.seed.address.clone().expect("the seed is up");
    assert_eq!(listing(&root.path().join(MIRROR_DIR)), listing(&repo), "an exact copy");

    // A launcher on the mesh, connected to the node's interface, hears the
    // seed and reads the same signed bytes from it.
    let mut args = BrowserArgs::new();
    args.tcp = Some(format!("127.0.0.1:{mesh_port}"));
    let browser = BridgeSession::start_browser(args).await.expect("the browser starts");
    game_bridge::repo_mesh::set_mesh_handle(Some(browser.handle().clone()));
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    loop {
        if game_bridge::repo_mesh::heard().iter().any(|h| h.address() == address) {
            break;
        }
        assert!(tokio::time::Instant::now() < deadline, "the node's seed was never heard");
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    let copy = root.path().join("over-the-mesh");
    let report = game_bridge::repo::mirror(&address, &copy, key().public().identity_hash())
        .await
        .expect("read over the mesh");
    assert_eq!(report.packs, 2);
    assert_eq!(listing(&copy), listing(&repo));

    agent.official().set_seeding(false).unwrap();
    let off = agent.official().sync(&agent, &packs).await;
    assert!(!off.seed.on && off.seed.address.is_none());
}
