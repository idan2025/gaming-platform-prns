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
    let index_key = key(200);
    let state = Arc::new(IndexState {
        registry: Mutex::new(Registry::new()),
        auth: Mutex::new(Authenticator::new(index_key.identity_hash())),
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
