//! The index's HTTP front door.
//!
//! `DESIGN.md` §2.4 wants the index served over **both** a Reticulum destination
//! and HTTPS. This is the HTTPS half — or rather the HTTP half, since TLS
//! belongs to whatever terminates it: an index is a cache anyone can run, and
//! baking in certificate management would make running one harder than running
//! the game server it lists.
//!
//! # Nothing here is authoritative
//!
//! Every route is a convenience. A launcher that cannot reach this service falls
//! back to listening for announces itself and loses nothing but reach — which is
//! the property `DESIGN.md` §0 exists to protect, and the reason the deploy
//! routes are separate from the listing routes: listing must keep working for
//! someone with no account, no session and no interest in ever having one.

use std::sync::Arc;

use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};
use axum::{Json, Router};
use game_bridge::browse::{BrowseFilter, BrowseQuery, SortBy};
use platform_auth::{Authenticator, ChallengeResponse};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::sync::Mutex;

use crate::registry::Registry;

pub struct IndexState {
    pub registry: Mutex<Registry>,
    pub auth: Mutex<Authenticator>,
    /// `None` when this index does not offer hosting, which is the default and
    /// the common case: most indexes are just directories.
    pub hosting: Option<crate::hosting::Hosting>,
}

#[derive(Serialize)]
struct ApiError {
    error: String,
}

fn fail(status: StatusCode, e: impl std::fmt::Display) -> (StatusCode, Json<ApiError>) {
    (status, Json(ApiError { error: format!("{e:#}") }))
}

type ApiResult<T> = Result<Json<T>, (StatusCode, Json<ApiError>)>;

pub fn router(state: Arc<IndexState>) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/servers", get(servers))
        .route("/auth/challenge", post(challenge))
        .route("/auth/verify", post(verify))
        .route("/me", get(me))
        // Hosting. Everything above works with these switched off.
        .route("/hosting", get(hosting_info))
        .route("/instances", get(my_instances).post(deploy))
        .route("/instances/:id", axum::routing::delete(destroy))
        .with_state(state)
}

async fn health(State(state): State<Arc<IndexState>>) -> Json<serde_json::Value> {
    let registry = state.registry.lock().await;
    Json(json!({
        "ok": true,
        // Said plainly, in the health endpoint, because somebody will build on
        // this and should be told what it is before they do.
        "authoritative": false,
        "note": "an index is a cache of the mesh; a launcher can hear these servers itself",
        "servers_known": registry.len(),
        "retention_secs": registry.retention().as_secs(),
    }))
}

/// The listing query, as URL parameters.
///
/// Mirrors `game_bridge::browse::BrowseFilter` field for field on purpose: an
/// index that filtered differently from a launcher would give a different answer
/// about the same mesh.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct ServersQuery {
    pub game_id: Option<String>,
    pub text: Option<String>,
    pub max_hops: Option<u8>,
    pub max_link_class: Option<u8>,
    pub has_players: bool,
    pub not_full: bool,
    pub exclude_passworded: bool,
    pub exclude_allowlisted: bool,
    pub dedicated_only: bool,
    pub include_legacy: Option<bool>,
    pub sort: Option<String>,
    pub descending: bool,
    pub limit: Option<usize>,
}

#[derive(Debug, Serialize)]
pub struct ServerRow {
    pub destination_hash: String,
    pub name: Option<String>,
    pub game_id: Option<String>,
    pub map: Option<String>,
    pub players: Option<u8>,
    pub max_players: Option<u8>,
    pub hops: u8,
    pub min_link_class: Option<u8>,
    pub passworded: Option<bool>,
    pub allowlisted: Option<bool>,
    pub dedicated: Option<bool>,
    pub last_seen_secs: u64,
    pub legacy: bool,
}

/// A hard ceiling on how much one request can ask for.
///
/// Not politeness: an index is a service anyone can run and anyone can call, and
/// an unbounded list is a free amplifier. A caller who wants everything can page
/// through it.
const MAX_LIMIT: usize = 500;

async fn servers(
    State(state): State<Arc<IndexState>>,
    Query(q): Query<ServersQuery>,
) -> ApiResult<Vec<ServerRow>> {
    let query = BrowseQuery {
        filter: BrowseFilter {
            game_id: q.game_id.filter(|s| !s.is_empty()),
            text: q.text.filter(|s| !s.trim().is_empty()),
            max_hops: q.max_hops,
            max_link_class: q.max_link_class,
            has_players: q.has_players,
            not_full: q.not_full,
            exclude_passworded: q.exclude_passworded,
            exclude_allowlisted: q.exclude_allowlisted,
            transport_modes: None,
            dedicated_only: q.dedicated_only,
            // Legacy rows are listed by default. They are real servers, and an
            // index that quietly hid everything predating its own metadata
            // format would be making the mesh look like the platform.
            include_legacy: q.include_legacy.unwrap_or(true),
        },
        sort: match q.sort.as_deref() {
            Some("players") => SortBy::Players,
            Some("name") => SortBy::Name,
            Some("last_seen") => SortBy::LastSeen,
            _ => SortBy::Hops,
        },
        descending: q.descending,
        max_age: None,
    };

    let now = std::time::Instant::now();
    let registry = state.registry.lock().await;
    let limit = q.limit.unwrap_or(MAX_LIMIT).min(MAX_LIMIT);
    let rows: Vec<ServerRow> = registry
        .query(&query, now)
        .into_iter()
        .take(limit)
        .map(|s| {
            let record = s.record();
            ServerRow {
                destination_hash: hex::encode(s.destination_hash.as_bytes()),
                name: s.name().map(str::to_string),
                game_id: s.game_id().map(str::to_string),
                map: record.map(|r| r.map.clone()).filter(|m| !m.is_empty()),
                players: record.map(|r| r.players),
                max_players: record.map(|r| r.max_players),
                hops: s.hops,
                min_link_class: record.map(|r| r.min_link_class),
                passworded: record.map(|r| r.flags.passworded),
                allowlisted: record.map(|r| r.flags.allowlisted),
                dedicated: record.map(|r| r.flags.dedicated),
                last_seen_secs: now.saturating_duration_since(s.last_seen).as_secs(),
                legacy: s.game_id().is_none(),
            }
        })
        .collect();
    Ok(Json(rows))
}

async fn challenge(State(state): State<Arc<IndexState>>) -> ApiResult<platform_auth::Challenge> {
    let mut auth = state.auth.lock().await;
    auth.issue_challenge(std::time::SystemTime::now())
        .map(Json)
        .map_err(|e| fail(StatusCode::INTERNAL_SERVER_ERROR, e))
}

async fn verify(
    State(state): State<Arc<IndexState>>,
    Json(response): Json<ChallengeResponse>,
) -> ApiResult<platform_auth::Session> {
    let mut auth = state.auth.lock().await;
    auth.verify(&response, std::time::SystemTime::now())
        .map(Json)
        // Every auth failure is a 401 with the same shape, so the response does
        // not tell an attacker which of "unknown nonce", "expired" and "bad
        // signature" they hit.
        .map_err(|e| fail(StatusCode::UNAUTHORIZED, e))
}

/// Who the caller is, per their bearer token. The smallest possible route that
/// proves the whole auth path works end to end.
async fn me(
    State(state): State<Arc<IndexState>>,
    headers: HeaderMap,
) -> ApiResult<serde_json::Value> {
    let token = bearer(&headers)
        .ok_or_else(|| fail(StatusCode::UNAUTHORIZED, "no bearer token"))?;
    let auth = state.auth.lock().await;
    let identity = auth
        .authenticate(&token, std::time::SystemTime::now())
        .map_err(|e| fail(StatusCode::UNAUTHORIZED, e))?;
    Ok(Json(json!({ "identity": hex::encode(identity.as_bytes()) })))
}

/// What this index will host, if anything. **Unauthenticated on purpose**: a
/// launcher has to be able to decide whether signing in is even worth it, and
/// an operator's list of hosted games is not a secret.
async fn hosting_info(State(state): State<Arc<IndexState>>) -> Json<serde_json::Value> {
    match &state.hosting {
        None => Json(json!({ "enabled": false, "games": [] })),
        Some(h) => Json(json!({
            "enabled": h.config().enabled(),
            "games": h.config().games,
            "nodes": h.config().nodes.len(),
            "max_instances_per_account": h.config().quota.max_instances_per_account,
        })),
    }
}

/// Resolve a bearer token to the account it proves, or fail with 401.
async fn account_of(
    state: &IndexState,
    headers: &HeaderMap,
) -> Result<crate::quota::AccountId, (StatusCode, Json<ApiError>)> {
    let token = bearer(headers).ok_or_else(|| fail(StatusCode::UNAUTHORIZED, "no bearer token"))?;
    let auth = state.auth.lock().await;
    let identity = auth
        .authenticate(&token, std::time::SystemTime::now())
        .map_err(|e| fail(StatusCode::UNAUTHORIZED, e))?;
    Ok(crate::quota::AccountId(hex::encode(identity.as_bytes())))
}

fn hosting_of(state: &IndexState) -> Result<&crate::hosting::Hosting, (StatusCode, Json<ApiError>)> {
    state
        .hosting
        .as_ref()
        .ok_or_else(|| fail(StatusCode::NOT_FOUND, "this index does not offer hosting"))
}

async fn my_instances(
    State(state): State<Arc<IndexState>>,
    headers: HeaderMap,
) -> ApiResult<Vec<crate::hosting::HostedInstance>> {
    let hosting = hosting_of(&state)?;
    let account = account_of(&state, &headers).await?;
    hosting
        .instances_for(&account)
        .await
        .map(Json)
        .map_err(|e| fail(StatusCode::BAD_GATEWAY, e))
}

async fn deploy(
    State(state): State<Arc<IndexState>>,
    headers: HeaderMap,
    Json(request): Json<crate::hosting::DeployRequest>,
) -> ApiResult<crate::hosting::HostedInstance> {
    let hosting = hosting_of(&state)?;
    let account = account_of(&state, &headers).await?;
    hosting
        .deploy(&account, &request, std::time::SystemTime::now())
        .await
        .map(Json)
        // A quota denial, an unhosted game and a node that will not take it are
        // all things the caller can act on, so they carry their sentence.
        .map_err(|e| fail(StatusCode::BAD_REQUEST, e))
}

async fn destroy(
    State(state): State<Arc<IndexState>>,
    headers: HeaderMap,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> ApiResult<serde_json::Value> {
    let hosting = hosting_of(&state)?;
    let account = account_of(&state, &headers).await?;
    hosting
        .destroy(&account, &id)
        .await
        .map(|()| Json(json!({ "removed": id })))
        // "no such instance" covers both missing and not-yours, so this cannot
        // be used to enumerate other people's instances.
        .map_err(|e| fail(StatusCode::NOT_FOUND, e))
}

fn bearer(headers: &HeaderMap) -> Option<String> {
    let value = headers.get(axum::http::header::AUTHORIZATION)?.to_str().ok()?;
    let token = value.strip_prefix("Bearer ").or_else(|| value.strip_prefix("bearer "))?;
    Some(token.trim().to_string())
}
