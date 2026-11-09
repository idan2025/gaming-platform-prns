//! The node's controller client, against an index that does the real login.
//!
//! The fake index runs `platform_auth::Authenticator` exactly as the index's
//! HTTP front door does, so the signature, the pinned audience and the session
//! are the real ones. Only the routes behind the login are faked. Needs no
//! Docker.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use axum::extract::State;
use axum::http::{HeaderMap, Method, StatusCode};
use axum::routing::{get, post};
use axum::{Json, Router};
use platform_agent::config::ControllerConfig;
use platform_agent::controller::{allowed_path, Controller};
use platform_auth::{Authenticator, ChallengeResponse};
use prns_core::identity::PrivateIdentityMaterial;

struct FakeIndex {
    auth: Mutex<Authenticator>,
    /// Tokens this index issued and has not revoked, and whose they are.
    live: Mutex<HashMap<String, String>>,
    /// Every request path that got past the login, so a test can see what the
    /// node actually asked for.
    seen: Mutex<Vec<String>>,
}

async fn challenge(State(s): State<Arc<FakeIndex>>) -> Json<platform_auth::Challenge> {
    Json(s.auth.lock().unwrap().issue_challenge(std::time::SystemTime::now()).unwrap())
}

async fn verify(
    State(s): State<Arc<FakeIndex>>,
    Json(response): Json<ChallengeResponse>,
) -> Result<Json<platform_auth::Session>, StatusCode> {
    let session = s
        .auth
        .lock()
        .unwrap()
        .verify(&response, std::time::SystemTime::now())
        .map_err(|_| StatusCode::UNAUTHORIZED)?;
    s.live.lock().unwrap().insert(session.token.clone(), session.identity.clone());
    Ok(Json(session))
}

/// The identity the request's token was issued to.
fn authorised(s: &FakeIndex, headers: &HeaderMap) -> Result<String, StatusCode> {
    let token = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .ok_or(StatusCode::UNAUTHORIZED)?;
    s.live.lock().unwrap().get(token).cloned().ok_or(StatusCode::UNAUTHORIZED)
}

async fn instances(State(s): State<Arc<FakeIndex>>, headers: HeaderMap) -> Result<Json<serde_json::Value>, StatusCode> {
    let owner = authorised(&s, &headers)?;
    s.seen.lock().unwrap().push("/instances".into());
    Ok(Json(serde_json::json!([{"instance_id": "sven-coop-1", "owner": owner}])))
}

async fn me(State(s): State<Arc<FakeIndex>>, headers: HeaderMap) -> Result<Json<serde_json::Value>, StatusCode> {
    let identity = authorised(&s, &headers)?;
    Ok(Json(serde_json::json!({ "identity": identity })))
}

async fn erase_me(
    State(s): State<Arc<FakeIndex>>,
    headers: HeaderMap,
    Json(body): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let identity = authorised(&s, &headers)?;
    if body["confirm"] != serde_json::json!(identity) {
        return Err(StatusCode::BAD_REQUEST);
    }
    s.seen.lock().unwrap().push(format!("erase {identity}"));
    Ok(Json(serde_json::json!({ "erased": {}, "sessions_ended": 1 })))
}

async fn logs(
    State(s): State<Arc<FakeIndex>>,
    headers: HeaderMap,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Result<String, StatusCode> {
    authorised(&s, &headers)?;
    s.seen.lock().unwrap().push(format!("/instances/{id}/logs"));
    Ok("a line\n".into())
}

async fn stop(
    State(s): State<Arc<FakeIndex>>,
    headers: HeaderMap,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Result<StatusCode, StatusCode> {
    authorised(&s, &headers)?;
    s.seen.lock().unwrap().push(format!("/instances/{id}/stop"));
    Ok(StatusCode::NO_CONTENT)
}

/// Start an index with its own identity. Returns its address and identity hash.
async fn serve_index(dir: &std::path::Path) -> (String, String, Arc<FakeIndex>) {
    let secret = personal_rns::load_or_create_identity_secret(&dir.join("index.identity")).unwrap();
    let identity = PrivateIdentityMaterial::from_slice(&secret[..]).unwrap();
    let hash = identity.identity_hash();
    let index_hash = hex::encode(hash.as_bytes());
    let state = Arc::new(FakeIndex {
        auth: Mutex::new(Authenticator::new(hash)),
        live: Mutex::new(HashMap::new()),
        seen: Mutex::new(Vec::new()),
    });
    let app = Router::new()
        .route("/auth/challenge", post(challenge))
        .route("/auth/verify", post(verify))
        .route("/instances", get(instances))
        .route("/me", get(me).delete(erase_me))
        .route("/instances/:id/logs", get(logs))
        .route("/instances/:id/stop", post(stop))
        .with_state(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("http://{addr}"), index_hash, state)
}

fn node_identity(dir: &std::path::Path) -> std::path::PathBuf {
    dir.join("node.identity")
}

#[tokio::test]
async fn the_node_logs_in_with_its_own_identity_and_forwards_what_the_allowlist_names() {
    let dir = tempfile::tempdir().unwrap();
    let (url, index_hash, index) = serve_index(dir.path()).await;
    let controller = Controller::new(
        &ControllerConfig { url: url.clone(), identity: index_hash.clone() },
        &node_identity(dir.path()),
    )
    .unwrap();

    // First request logs in, then lists.
    let reply = controller.forward(&Method::GET, "/instances", None).await.unwrap();
    assert_eq!(reply.status, 200);
    let list: serde_json::Value = serde_json::from_slice(&reply.body).unwrap();
    assert_eq!(list[0]["instance_id"], "sven-coop-1");

    // The next one reuses the session: no second login is needed to read logs.
    let path = allowed_path(&Method::GET, "instances/sven-coop-1/logs?lines=20").unwrap();
    let reply = controller.forward(&Method::GET, &path, None).await.unwrap();
    assert_eq!(reply.status, 200);
    assert_eq!(reply.body, b"a line\n");

    assert_eq!(*index.seen.lock().unwrap(), vec!["/instances", "/instances/sven-coop-1/logs"]);
}

/// The session is dropped on the index's side (a restart, an expiry). The node
/// notices the refusal, logs in again and tries once more.
#[tokio::test]
async fn a_revoked_session_is_renewed_once_and_the_request_goes_through() {
    let dir = tempfile::tempdir().unwrap();
    let (url, index_hash, index) = serve_index(dir.path()).await;
    let controller = Controller::new(
        &ControllerConfig { url, identity: index_hash },
        &node_identity(dir.path()),
    )
    .unwrap();
    assert_eq!(controller.forward(&Method::GET, "/instances", None).await.unwrap().status, 200);

    index.live.lock().unwrap().clear();
    // Logs, not the list: the list is shared for a few seconds and would come
    // from the node without asking the index at all.
    let reply = controller.forward(&Method::GET, "/instances/sven-coop-1/logs", None).await.unwrap();
    assert_eq!(reply.status, 200, "the request must succeed after a fresh login");
}

/// The login is bound to the index it was made for. Pinned to a different index,
/// the node's signature is refused, so the login cannot be used there.
#[tokio::test]
async fn a_login_is_refused_by_an_index_the_node_is_not_pinned_to() {
    let dir = tempfile::tempdir().unwrap();
    let (url, _real, _index) = serve_index(dir.path()).await;
    let other_dir = tempfile::tempdir().unwrap();
    let (_other_url, other_hash, _other) = serve_index(other_dir.path()).await;
    let controller = Controller::new(
        &ControllerConfig { url, identity: other_hash },
        &node_identity(dir.path()),
    )
    .unwrap();
    assert!(controller.forward(&Method::GET, "/instances", None).await.is_err());
}

#[tokio::test]
async fn a_plain_http_address_off_this_machine_is_refused_before_any_connection() {
    let dir = tempfile::tempdir().unwrap();
    let hash = "a1b2c3d4e5f6a7b8c9d0e1f2a3b4c5d6".to_string();
    let err = Controller::new(
        &ControllerConfig { url: "http://192.168.1.5:4760".into(), identity: hash },
        &node_identity(dir.path()),
    );
    assert!(err.is_err());
}

/// Every page open on a node polls the server list. Through a controller each
/// poll used to be one listing at the index, which asks every worker; pages
/// polling together now cost one. A change sent through the controller makes
/// the next list a fresh one, so a page never shows a stop it just made undone.
#[tokio::test]
async fn pages_polling_together_cost_the_index_one_listing_and_a_change_is_seen_at_once() {
    let dir = tempfile::tempdir().unwrap();
    let (url, index_hash, index) = serve_index(dir.path()).await;
    let controller = Arc::new(
        Controller::new(&ControllerConfig { url, identity: index_hash }, &node_identity(dir.path())).unwrap(),
    );
    let lists = |index: &FakeIndex| index.seen.lock().unwrap().iter().filter(|p| *p == "/instances").count();

    let pages: Vec<_> = (0..6)
        .map(|_| {
            let c = controller.clone();
            tokio::spawn(async move { c.forward(&Method::GET, "/instances", None).await.unwrap().status })
        })
        .collect();
    for page in pages {
        assert_eq!(page.await.unwrap(), 200);
    }
    assert_eq!(lists(&index), 1, "six pages polling together asked the index more than once");

    assert_eq!(controller.forward(&Method::POST, "/instances/sven-coop-1/stop", None).await.unwrap().status, 204);
    controller.forward(&Method::GET, "/instances", None).await.unwrap();
    assert_eq!(lists(&index), 2, "the list after a stop came from before it");
}

/// Each account with the user role is its own identity at the index, derived
/// from the node's key and the account's salt, so the index keeps their
/// servers apart; the node's admins are the node. Lists are cached per
/// identity (one account never sees another's), and erasing an account sends
/// that account's own identity back as confirmation and is refused for the
/// node.
#[tokio::test]
async fn each_user_account_is_its_own_identity_at_the_controller() {
    use platform_agent::controller::Acting;
    let dir = tempfile::tempdir().unwrap();
    let (url, index_hash, index) = serve_index(dir.path()).await;
    let controller =
        Controller::new(&ControllerConfig { url, identity: index_hash }, &node_identity(dir.path()))
            .unwrap();
    let account = |name: &str, salt: &str| Acting::Account { username: name.into(), salt: salt.into() };
    let alice = account("alice", "00112233445566778899aabbccddeeff");

    let node_id = controller.identity_of(&Acting::Node).unwrap();
    let alice_id = controller.identity_of(&alice).unwrap();
    assert_ne!(alice_id, node_id);
    assert_eq!(controller.identity_of(&alice).unwrap(), alice_id, "the same account is the same identity");
    let others = [account("alice", "ffeeddccbbaa99887766554433221100"), account("bob", "00112233445566778899aabbccddeeff")];
    for other in &others {
        let id = controller.identity_of(other).unwrap();
        assert!(id != alice_id && id != node_id, "{other:?}");
    }

    let who = |reply: platform_agent::controller::Forwarded| -> serde_json::Value { serde_json::from_slice(&reply.body).unwrap() };
    assert_eq!(who(controller.forward_as(&alice, &Method::GET, "/me", None).await.unwrap())["identity"], serde_json::json!(alice_id));
    assert_eq!(who(controller.forward(&Method::GET, "/me", None).await.unwrap())["identity"], serde_json::json!(node_id));

    let alice_list = who(controller.forward_as(&alice, &Method::GET, "/instances", None).await.unwrap());
    let bob_list = who(controller.forward_as(&others[1], &Method::GET, "/instances", None).await.unwrap());
    assert_eq!(alice_list[0]["owner"], serde_json::json!(alice_id));
    assert_ne!(bob_list[0]["owner"], serde_json::json!(alice_id), "one account was handed another's cached list");

    assert!(controller.erase(&Acting::Node).await.is_err());
    controller.erase(&alice).await.unwrap();
    assert!(index.seen.lock().unwrap().contains(&format!("erase {alice_id}")));
}
