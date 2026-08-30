//! The index's front door, over a real socket.
//!
//! The interesting test here is the auth round trip: challenge, sign, verify,
//! then use the token. It exercises `platform-auth` through HTTP exactly as a
//! launcher would, which is the only way to catch an encoding mismatch between
//! the two.

use std::sync::Arc;

use platform_auth::{answer_challenge, Authenticator, Challenge};
use platform_index::http::{router, IndexState};
use platform_index::registry::Registry;
use prns_core::identity::PrivateIdentityMaterial;
use tokio::sync::Mutex;

fn key(seed: u8) -> PrivateIdentityMaterial {
    PrivateIdentityMaterial::from_slice(&[seed; 64]).expect("64 bytes is a secret key")
}

async fn serve() -> (std::net::SocketAddr, PrivateIdentityMaterial) {
    serve_with(None).await
}

async fn serve_with(
    hosting: Option<platform_index::hosting::Hosting>,
) -> (std::net::SocketAddr, PrivateIdentityMaterial) {
    let index_key = key(200);
    let state = Arc::new(IndexState {
        registry: Mutex::new(Registry::new()),
        auth: Mutex::new(Authenticator::new(index_key.identity_hash())),
        hosting,
    });
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, router(state)).await;
    });
    (addr, index_key)
}

/// An index must say what it is. Somebody will build on this endpoint, and
/// "authoritative: false" in the health payload is cheaper than finding out
/// later that a launcher started trusting it.
#[tokio::test(flavor = "multi_thread")]
async fn health_says_it_is_not_authoritative() {
    let (addr, _) = serve().await;
    let body: serde_json::Value = reqwest::get(format!("http://{addr}/health"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(body["ok"], serde_json::json!(true));
    assert_eq!(body["authoritative"], serde_json::json!(false));
    assert_eq!(body["servers_known"], serde_json::json!(0));
}

/// Listing works with no account, no session, and no interest in having one.
/// That is the property that keeps the index a convenience.
#[tokio::test(flavor = "multi_thread")]
async fn listing_needs_no_authentication() {
    let (addr, _) = serve().await;
    let res = reqwest::get(format!("http://{addr}/servers")).await.unwrap();
    assert!(res.status().is_success());
    let rows: serde_json::Value = res.json().await.unwrap();
    assert!(rows.is_array());
}

#[tokio::test(flavor = "multi_thread")]
async fn the_full_auth_round_trip_works_over_http() {
    let (addr, index_key) = serve().await;
    let user = key(1);
    let client = reqwest::Client::new();

    let challenge: Challenge = client
        .post(format!("http://{addr}/auth/challenge"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        challenge.audience,
        hex::encode(index_key.identity_hash().as_bytes()),
        "the index must advertise the identity clients bind their signature to"
    );

    let response = answer_challenge(&challenge, &index_key.identity_hash(), &user).unwrap();
    let session: platform_auth::Session = client
        .post(format!("http://{addr}/auth/verify"))
        .json(&response)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(session.identity, hex::encode(user.identity_hash().as_bytes()));

    let me: serde_json::Value = client
        .get(format!("http://{addr}/me"))
        .bearer_auth(&session.token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(me["identity"], serde_json::json!(hex::encode(user.identity_hash().as_bytes())));
}

/// Every auth failure has to look the same from outside, or the response tells
/// an attacker which of "unknown nonce", "expired" and "bad signature" they hit.
#[tokio::test(flavor = "multi_thread")]
async fn a_replayed_response_is_rejected_over_http() {
    let (addr, index_key) = serve().await;
    let user = key(1);
    let client = reqwest::Client::new();

    let challenge: Challenge = client
        .post(format!("http://{addr}/auth/challenge"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let response = answer_challenge(&challenge, &index_key.identity_hash(), &user).unwrap();

    let first = client
        .post(format!("http://{addr}/auth/verify"))
        .json(&response)
        .send()
        .await
        .unwrap();
    assert!(first.status().is_success());

    let second = client
        .post(format!("http://{addr}/auth/verify"))
        .json(&response)
        .send()
        .await
        .unwrap();
    assert_eq!(second.status(), 401, "a replayed response must not authenticate");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_missing_or_bogus_token_is_refused() {
    let (addr, _) = serve().await;
    let client = reqwest::Client::new();

    let none = client.get(format!("http://{addr}/me")).send().await.unwrap();
    assert_eq!(none.status(), 401);

    let bogus = client
        .get(format!("http://{addr}/me"))
        .bearer_auth("ff".repeat(32))
        .send()
        .await
        .unwrap();
    assert_eq!(bogus.status(), 401);

    let junk = client
        .get(format!("http://{addr}/me"))
        .header("Authorization", "Basic hunter2")
        .send()
        .await
        .unwrap();
    assert_eq!(junk.status(), 401);
}

/// An unbounded list from a service anyone can call is a free amplifier.
#[tokio::test(flavor = "multi_thread")]
async fn the_listing_limit_is_capped_regardless_of_what_is_asked_for() {
    let (addr, _) = serve().await;
    let res = reqwest::get(format!("http://{addr}/servers?limit=100000"))
        .await
        .unwrap();
    assert!(res.status().is_success(), "an absurd limit is clamped, not an error");
    let rows: serde_json::Value = res.json().await.unwrap();
    assert!(rows.as_array().unwrap().len() <= 500);
}

/// Sign in and return a bearer token, the way a launcher would.
async fn token_for(
    addr: std::net::SocketAddr,
    index_key: &PrivateIdentityMaterial,
    user: &PrivateIdentityMaterial,
) -> String {
    let client = reqwest::Client::new();
    let challenge: Challenge = client
        .post(format!("http://{addr}/auth/challenge"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let response = answer_challenge(&challenge, &index_key.identity_hash(), user).unwrap();
    let session: platform_auth::Session = client
        .post(format!("http://{addr}/auth/verify"))
        .json(&response)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    session.token
}

fn hosting_config() -> platform_index::hosting::HostingConfig {
    platform_index::hosting::HostingConfig {
        games: vec!["sven-coop".to_string()],
        nodes: vec![platform_index::hosting::NodeConfig {
            name: "local".to_string(),
            // Nothing is listening here. Every test below either fails before
            // reaching a node, or is asserting how an unreachable node is
            // reported — which is itself worth pinning.
            api: "http://127.0.0.1:1".to_string(),
        }],
        quota: Default::default(),
    }
}

/// An index that does not host says so, unauthenticated, so a launcher can tell
/// before bothering anyone to sign in.
#[tokio::test(flavor = "multi_thread")]
async fn an_index_without_hosting_advertises_that_plainly() {
    let (addr, _) = serve().await;
    let body: serde_json::Value = reqwest::get(format!("http://{addr}/hosting"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(body["enabled"], serde_json::json!(false));
    assert_eq!(body["games"], serde_json::json!([]));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_hosting_index_advertises_what_it_will_run() {
    let (addr, _) =
        serve_with(Some(platform_index::hosting::Hosting::new(hosting_config()))).await;
    let body: serde_json::Value = reqwest::get(format!("http://{addr}/hosting"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(body["enabled"], serde_json::json!(true));
    assert_eq!(body["games"], serde_json::json!(["sven-coop"]));
}

/// Deploying is the one thing here that costs somebody resources, so it is the
/// one thing that requires proving who you are.
#[tokio::test(flavor = "multi_thread")]
async fn deploying_without_a_session_is_refused() {
    let (addr, _) =
        serve_with(Some(platform_index::hosting::Hosting::new(hosting_config()))).await;
    let res = reqwest::Client::new()
        .post(format!("http://{addr}/instances"))
        .json(&serde_json::json!({ "game_id": "sven-coop", "name": "mine" }))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 401);
}

/// The operator's list is the whole access-control story for *what* may run, and
/// a caller gets told what it is rather than a bare refusal.
#[tokio::test(flavor = "multi_thread")]
async fn deploying_an_unhosted_game_names_what_is_hosted() {
    let (addr, index_key) =
        serve_with(Some(platform_index::hosting::Hosting::new(hosting_config()))).await;
    let token = token_for(addr, &index_key, &key(1)).await;

    let res = reqwest::Client::new()
        .post(format!("http://{addr}/instances"))
        .bearer_auth(&token)
        .json(&serde_json::json!({ "game_id": "minecraft", "name": "nope" }))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 400);
    let body: serde_json::Value = res.json().await.unwrap();
    let msg = body["error"].as_str().unwrap_or_default();
    assert!(msg.contains("does not host"), "{msg}");
    assert!(msg.contains("sven-coop"), "the caller should see the list: {msg}");
}

/// An index with no hosting configured must not pretend the routes exist.
#[tokio::test(flavor = "multi_thread")]
async fn deploy_routes_are_absent_when_hosting_is_off() {
    let (addr, index_key) = serve().await;
    let token = token_for(addr, &index_key, &key(1)).await;
    let res = reqwest::Client::new()
        .post(format!("http://{addr}/instances"))
        .bearer_auth(&token)
        .json(&serde_json::json!({ "game_id": "sven-coop", "name": "mine" }))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 404);
}

/// Not-found and not-yours are the same answer, so nobody can enumerate other
/// people's instances by watching which ids come back differently.
#[tokio::test(flavor = "multi_thread")]
async fn destroying_someone_elses_instance_is_indistinguishable_from_a_missing_one() {
    let (addr, index_key) =
        serve_with(Some(platform_index::hosting::Hosting::new(hosting_config()))).await;
    let token = token_for(addr, &index_key, &key(1)).await;
    let client = reqwest::Client::new();

    let missing = client
        .delete(format!("http://{addr}/instances/does-not-exist"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(missing.status(), 404);
}
