//! An operator's own index routes are served beside the built-in ones, with
//! the same headers on every answer.

use std::sync::Arc;

use axum::routing::get;
use platform_auth::Authenticator;
use platform_index::http::{router_with, IndexState};
use platform_index::registry::Registry;
use prns_core::identity::PrivateIdentityMaterial;
use tokio::sync::Mutex;

#[tokio::test(flavor = "multi_thread")]
async fn an_extension_route_is_served_with_the_indexs_headers() {
    let key = PrivateIdentityMaterial::from_slice(&[7u8; 64]).unwrap();
    let state = Arc::new(IndexState {
        registry: Mutex::new(Registry::new()),
        auth: Mutex::new(Authenticator::new(key.identity_hash())),
        hosting: None,
    });
    let extra = axum::Router::new().route(
        "/ext/known",
        get(|axum::extract::State(s): axum::extract::State<Arc<IndexState>>| async move {
            format!("hosting: {}", s.hosting.is_some())
        }),
    );
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, router_with(state, extra)).await;
    });
    let res = reqwest::get(format!("http://{addr}/ext/known")).await.unwrap();
    assert_eq!(res.status(), 200);
    assert!(
        res.headers().contains_key("x-content-type-options"),
        "the index's headers are missing"
    );
    assert_eq!(res.text().await.unwrap(), "hosting: false");
    // The built-in routes are still there.
    let health = reqwest::get(format!("http://{addr}/health")).await.unwrap();
    assert!(health.status().is_success());
}
