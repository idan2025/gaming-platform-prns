//! Joining a shared instance that is still coming up.
//!
//! `serve_shared_instance` hands the server to the node's supervisor and
//! returns before the port listens, so a join straight after it used to find
//! nothing there and fail for good: on a fresh Lanthorn Host the official seed
//! did, on CI's ARM runner (`official_seed.rs`). A join waits a little instead.

use std::time::Duration;

use game_bridge::config::RelayArgs;
use game_bridge::BridgeSession;

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

fn relay(dir: &std::path::Path, name: &str) -> RelayArgs {
    let mut args = RelayArgs::new();
    args.identity = dir.join(name);
    args
}

#[tokio::test(flavor = "multi_thread")]
async fn a_join_waits_for_a_shared_instance_that_is_still_starting() {
    let dir = tempfile::tempdir().unwrap();
    let port = free_port();
    let hub = BridgeSession::start_relay(relay(dir.path(), "hub.identity")).await.unwrap();
    let member = BridgeSession::start_relay(relay(dir.path(), "member.identity")).await.unwrap();

    let join = tokio::spawn(async move { member.join_shared_instance(port).await.map(|()| member) });
    tokio::time::sleep(Duration::from_millis(500)).await;
    hub.serve_shared_instance(port).await.unwrap();
    join.await.unwrap().expect("the join waited for the hub to listen");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_join_to_nothing_still_fails() {
    let dir = tempfile::tempdir().unwrap();
    let member = BridgeSession::start_relay(relay(dir.path(), "member.identity")).await.unwrap();
    assert!(member.join_shared_instance(free_port()).await.is_err());
}
