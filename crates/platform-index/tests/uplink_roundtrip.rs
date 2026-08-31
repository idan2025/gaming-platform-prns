//! Two-node uplink round-trip, Docker-gated.
//!
//! The test that proves multi-node hosting end to end (`PLAN.md` §8 phase 4): an
//! index on one Reticulum node drives an agent on another — create, list, stop,
//! remove — with challenge/response auth and the operator's `trusted_indexes`
//! allowlist as the only authorization. No inbound port on either side; the two
//! nodes meet over a loopback TCP interface.
//!
//! Skips itself without a Docker daemon, because `Agent::new` pings Docker on
//! the way up. The image is a tiny local busybox build, same as
//! `platform-agent/tests/docker_guard.rs`, so the test needs no network.

use std::net::TcpListener;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Result};
use personal_rns::prelude::Zeroizing;
use platform_auth::Authenticator;
use platform_agent::agent::Agent;
use platform_agent::config::{AgentConfig, UplinkConfig};
use platform_agent::instance::{InstanceState, InstanceStatus};
use platform_agent::uplink;
use platform_index::agent_client::AgentClient;
use platform_index::http::IndexState;
use platform_index::registry::Registry;
use prns_core::identity::PrivateIdentityMaterial;
use tokio::sync::Mutex;

const TEST_IMAGE: &str = "gpp-test-sleeper:latest";
/// The end-user identity stamped as `owner` / `OWNER_LABEL`. A stand-in hex hash
/// distinct from the index's own identity, so the test also pins that the owner
/// is the user, not the caller.
const USER_IDENTITY_HEX: &str = "00112233445566778899aabbccddeeff";

fn free_tcp_port() -> u16 {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    drop(l);
    port
}

/// A tiny local image that stays up, built from the busybox already on the host
/// so the test needs no network. Mirrors `docker_guard.rs`.
fn ensure_test_image() -> bool {
    let present = std::process::Command::new("docker")
        .args(["image", "inspect", TEST_IMAGE])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    if matches!(present, Ok(s) if s.success()) {
        return true;
    }
    let dir = std::env::temp_dir().join(format!("gpp-uplink-img-{}", std::process::id()));
    if std::fs::create_dir_all(&dir).is_err() {
        return false;
    }
    if std::fs::write(dir.join("Dockerfile"), "FROM busybox\nCMD [\"sleep\", \"3600\"]\n").is_err() {
        return false;
    }
    let built = std::process::Command::new("docker")
        .args(["build", "-q", "-t", TEST_IMAGE, "."])
        .current_dir(&dir)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    let _ = std::fs::remove_dir_all(&dir);
    matches!(built, Ok(s) if s.success())
}

fn docker_rm(name: &str) {
    let _ = std::process::Command::new("docker")
        .args(["rm", "-f", name])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

fn agent_config_toml(data_root: &std::path::Path) -> String {
    format!(
        r#"
data_root = "{root}"
max_instances = 2
api_bind = "127.0.0.1:0"

[port_range]
start = 27150
end = 27159

[games.sven-coop]
image = "{image}"
content_root = "/game"
content_version = "test"
"#,
        root = data_root.display(),
        image = TEST_IMAGE,
    )
}

struct TwoNodes {
    agent_node: uplink::AgentUplinkNode,
    index: platform_index::node::IndexNode,
    client: Arc<AgentClient>,
    agent_dest: personal_rns::prelude::DestinationHash,
    _dir: tempfile::TempDir,
}

/// Bring up both nodes on a loopback TCP mesh. The agent binds the TCP server;
/// the index connects to it. `trusted` receives the index's own identity-hash
/// hex and returns the agent's `trusted_indexes` — so a caller can list the
/// index (the trusted case) or a stranger (the untrusted case) from the same
/// harness. Returns `None` to skip the test when Docker is unavailable.
async fn two_nodes(trusted: impl FnOnce(&str) -> Vec<String>) -> Option<TwoNodes> {
    if !ensure_test_image() {
        eprintln!("skipping: could not build the test image");
        return None;
    }
    let dir = tempfile::tempdir().ok()?;

    // The shared content copy must carry the writable mountpoints the Sven pack
    // declares, or `plan_and_check` rejects the create with the missing-dir
    // error. A writable bind nested in a read-only one cannot make its own
    // mountpoint.
    let content = dir.path().join("content").join("sven-coop").join("test").join("svencoop");
    std::fs::create_dir_all(content.join("maps")).ok()?;
    std::fs::create_dir_all(content.join("logs")).ok()?;
    std::fs::create_dir_all(content.join("scripts")).ok()?;

    let agent_cfg = AgentConfig::parse(&agent_config_toml(dir.path())).ok()?;
    let agent = Agent::new(agent_cfg, vec![game_bridge::GamePack::sven_coop()])
        .await
        .ok()?;
    let agent = Arc::new(agent);

    // The index identity: random, ephemeral. Its hash goes into the agent's
    // trusted_indexes; the index presents the same key to the agent as to users.
    let mut idx_secret = [0u8; 64];
    if getrandom::getrandom(&mut idx_secret).is_err() {
        return None;
    }
    let idx_identity = PrivateIdentityMaterial::from_slice(&idx_secret).ok()?;
    let idx_hash_hex = hex::encode(idx_identity.identity_hash().as_bytes());

    let port = free_tcp_port();
    let agent_key_path = dir.path().join("agent.key");
    let uplink_cfg = UplinkConfig {
        identity_secret_path: agent_key_path,
        tcp: Some(format!("0.0.0.0:{port}")),
        auto: false,
        trusted_indexes: trusted(&idx_hash_hex),
    };
    let agent_node = uplink::start(agent.clone(), uplink_cfg).await.ok()?;
    let agent_dest = agent_node.destination();

    // A minimal index node: no hosting config, just a Reticulum stack the
    // AgentClient can open Links from. It connects to the agent's TCP server.
    let state = Arc::new(IndexState {
        registry: Mutex::new(Registry::new()),
        auth: Mutex::new(Authenticator::new(idx_identity.identity_hash())),
        hosting: None,
    });
    let index = platform_index::node::start(
        state,
        Zeroizing::new(idx_secret),
        Some(format!("127.0.0.1:{port}")),
        false,
    )
    .await
    .ok()?;
    let client = AgentClient::new(index.handle(), Zeroizing::new(idx_secret));

    Some(TwoNodes {
        agent_node,
        index,
        client,
        agent_dest,
        _dir: dir,
    })
}

/// The link needs the interface to come up and a path to resolve, so the first
/// attempt can lose a race that has nothing to do with the agent. Same shape as
/// `reticulum_query.rs`'s `with_retries`.
async fn with_retries<F, Fut, T>(mut f: F) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T>>,
{
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        match f().await {
            Ok(v) => return Ok(v),
            Err(e) if tokio::time::Instant::now() >= deadline => return Err(e),
            Err(_) => tokio::time::sleep(Duration::from_millis(500)).await,
        }
    }
}

/// The full multi-node round-trip: create over the uplink, see it listed, stop
/// it, see it stopped, remove it, see it gone. The owner stamped by the index
/// is the user's identity, not the index's own — the create carries it through
/// `spec.owner` to `OWNER_LABEL`.
#[tokio::test(flavor = "multi_thread")]
async fn an_index_creates_lists_stops_and_removes_on_a_remote_agent() {
    let Some(mut tn) = two_nodes(|idx| vec![idx.to_string()]).await else {
        eprintln!("skipping: no Docker daemon");
        return;
    };

    let id = format!("uplink{}", std::process::id());
    let name = format!("gpp-{id}");
    docker_rm(&name);

    let outcome: Result<()> = async {
        let spec = platform_agent::instance::InstanceSpec {
            instance_id: id.clone(),
            game_id: "sven-coop".to_string(),
            name: "Remote".to_string(),
            max_players: 8,
            port: None,
            extra_ports: Default::default(),
            owner: None,
        };

        // Create over the uplink. Retries cover the interface/path race.
        let created: InstanceStatus = with_retries(|| {
            let spec = spec.clone();
            let client = tn.client.clone();
            let dest = tn.agent_dest;
            async move {
                client
                    .create(dest, spec, Some(USER_IDENTITY_HEX.to_string()))
                    .await
            }
        })
        .await?;
        assert_eq!(created.instance_id, id);
        assert_eq!(created.state, InstanceState::Running, "the instance did not come up remotely");
        assert_eq!(
            created.owner.as_deref(),
            Some(USER_IDENTITY_HEX),
            "the user's identity is stamped as owner, not the index's"
        );

        // List over the uplink sees it.
        let listed: Vec<InstanceStatus> = with_retries(|| {
            let client = tn.client.clone();
            let dest = tn.agent_dest;
            async move { client.list(dest).await }
        })
        .await?;
        assert!(
            listed.iter().any(|r| r.instance_id == id),
            "the remote agent did not list the instance we just created"
        );

        // Stop over the uplink, then list confirms it stopped.
        tn.client.stop(tn.agent_dest, &id).await?;
        let listed: Vec<InstanceStatus> = tn.client.list(tn.agent_dest).await?;
        let row = listed
            .iter()
            .find(|r| r.instance_id == id)
            .ok_or_else(|| anyhow!("the instance vanished after stop"))?;
        assert_eq!(row.state, InstanceState::Stopped, "stop over the uplink did not stop it");

        // Remove over the uplink, then list confirms it is gone.
        tn.client.remove(tn.agent_dest, &id).await?;
        let listed: Vec<InstanceStatus> = tn.client.list(tn.agent_dest).await?;
        assert!(
            !listed.iter().any(|r| r.instance_id == id),
            "remove over the uplink did not remove it"
        );

        Ok(())
    }
    .await;

    docker_rm(&name);
    tn.agent_node.stop().await;
    tn.index.stop().await;
    outcome.expect("the uplink round-trip should complete");
}

/// An index the operator did not list is refused on its very first request — it
/// cannot even list. This is the allowlist gate, live over the wire: a stranger
/// with a valid keypair and no path to the allowlist gets nothing.
#[tokio::test(flavor = "multi_thread")]
async fn an_untrusted_index_cannot_even_list_over_the_uplink() {
    // Same harness, but the agent's trusted_indexes names a stranger, not the
    // index's own identity. The index still has a valid keypair; the signature
    // verifies. The allowlist gate is what refuses it.
    let stranger_hex = hex::encode([0xa1u8; 16]);
    let Some(tn) = two_nodes(|_| vec![stranger_hex]).await else {
        eprintln!("skipping: no Docker daemon");
        return;
    };
    let mut agent_node = tn.agent_node;
    let mut index = tn.index;
    let client = tn.client;
    let agent_dest = tn.agent_dest;

    // The index is not in trusted_indexes. Even list — the least privileged op —
    // must be refused, and the refusal must be an auth error, not a timeout or a
    // silent empty list. Retry only while the wire is still coming up; once the
    // agent answers, an auth refusal is immediate and stable, so stop on it.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    let mut refusal: Option<anyhow::Error> = None;
    loop {
        match client.list(agent_dest).await {
            Ok(_) => break, // handled below as a failure
            Err(e) => {
                let msg = format!("{e:#}").to_ascii_lowercase();
                if msg.contains("untrusted") || msg.contains("refused") || msg.contains("verify") {
                    refusal = Some(e);
                    break;
                }
                if tokio::time::Instant::now() >= deadline {
                    refusal = Some(e);
                    break;
                }
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        }
    }
    let _ = index.stop().await;
    let _ = agent_node.stop().await;

    let err = refusal.expect("an untrusted index was allowed to list");
    let msg = format!("{err:#}").to_ascii_lowercase();
    assert!(
        msg.contains("untrusted") || msg.contains("refused") || msg.contains("verify"),
        "the refusal should name auth, got: {msg}"
    );
}