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
use axum::response::Html;
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
        .route("/", get(status_page))
        .route("/health", get(health))
        .route("/servers", get(servers))
        .route("/auth/challenge", post(challenge))
        .route("/auth/verify", post(verify))
        .route("/me", get(me))
        // Hosting. Everything above works with these switched off.
        .route("/hosting", get(hosting_info))
        .route("/instances", get(my_instances).post(deploy))
        .route("/instances/:id", axum::routing::delete(destroy))
        .route("/instances/:id/stop", post(stop))
        .route("/instances/:id/start", post(start))
        .route("/instances/:id/relocate", post(relocate))
        .route("/instances/:id/move-targets", get(move_targets))
        .route("/instances/:id/region-request", post(region_request))
        .route("/region-requests", get(my_region_requests))
        .route("/region-requests/:rid", axum::routing::delete(cancel_region_request))
        .route("/admin/region-requests", get(admin_region_requests))
        .route("/admin/region-requests/:rid/approve", post(approve_region_request))
        .route("/admin/region-requests/:rid/deny", post(deny_region_request))
        .route("/admin-requests", post(ask_admin))
        .route("/admin/admin-requests", get(admin_requests))
        .route("/admin/admin-requests/:rid/approve", post(approve_admin_request))
        .route("/admin/admin-requests/:rid/deny", post(deny_admin_request))
        .route("/admin/admins", get(admins_list))
        .route("/admin/admins/:identity", axum::routing::delete(revoke_admin))
        .route("/admin/workers", get(workers))
        .route("/instances/:id/logs", get(logs))
        .route("/instances/:id/backups", get(backups).post(backup_create))
        .route("/instances/:id/backups/:name", axum::routing::delete(backup_delete))
        .route("/instances/:id/backups/:name/restore", post(backup_restore))
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

/// A page for a person who opens the index's address in a browser: what this
/// index is, its identity (what a worker's `--index` and a node's
/// `[controller]` name), and what `/health` and `/hosting` already say. No
/// worker's name or address, and nothing that asks a worker, so opening it
/// costs the index nothing and tells a stranger nothing new.
async fn status_page(State(state): State<Arc<IndexState>>) -> Html<String> {
    let identity = hex::encode(state.auth.lock().await.audience().as_bytes());
    let (known, retention) = {
        let registry = state.registry.lock().await;
        (registry.len(), registry.retention().as_secs())
    };
    let hosting = match &state.hosting {
        Some(h) if h.config().enabled() => {
            let config = h.config();
            let games = config.games.iter().map(|g| escape(g)).collect::<Vec<_>>().join(", ");
            let quota = format!("up to {} per account", config.quota.max_instances_per_account);
            let regions = config.regions();
            let regions = if regions.is_empty() {
                String::new()
            } else {
                let list = regions.iter().map(|r| escape(r)).collect::<Vec<_>>().join(", ");
                format!("<tr><th>Regions</th><td>{list}</td></tr>")
            };
            format!(
                "<tr><th>Hosting</th><td>on</td></tr><tr><th>Games</th><td>{games}</td></tr>\
                 <tr><th>Workers</th><td>{}</td></tr>{regions}<tr><th>Servers</th><td>{quota}</td></tr>",
                config.nodes.len()
            )
        }
        _ => "<tr><th>Hosting</th><td>off: this index is a directory only</td></tr>".to_string(),
    };
    Html(format!(
        r#"<!doctype html>
<html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1">
<title>Lanthorn index</title>
<style>
:root {{ color-scheme: light dark; --bg: #f7f7f5; --fg: #1d1d1b; --muted: #66665f; --line: #deded8; }}
@media (prefers-color-scheme: dark) {{ :root {{ --bg: #171716; --fg: #ecece8; --muted: #a3a39b; --line: #33332f; }} }}
body {{ margin: 0; padding: 32px 16px; background: var(--bg); color: var(--fg); font: 16px/1.5 system-ui, sans-serif; }}
main {{ max-width: 640px; margin: 0 auto; }}
h1 {{ font-size: 1.4rem; margin: 0 0 4px; }}
p {{ color: var(--muted); margin: 0 0 24px; }}
table {{ width: 100%; border-collapse: collapse; }}
th, td {{ text-align: left; padding: 10px 0; border-top: 1px solid var(--line); vertical-align: top; }}
th {{ width: 34%; font-weight: 500; color: var(--muted); }}
code {{ font: 0.95em ui-monospace, monospace; overflow-wrap: anywhere; }}
a {{ color: inherit; }}
</style></head>
<body><main>
<h1>Lanthorn index</h1>
<p>A cache of the mesh, not an authority: a launcher can hear every server listed here by itself.</p>
<table>
<tr><th>Identity</th><td><code>{identity}</code></td></tr>
<tr><th>Version</th><td>{version}</td></tr>
<tr><th>Servers heard</th><td>{known}, each kept {retention_min} minutes after it was last heard</td></tr>
{hosting}
</table>
<p style="margin-top:24px">Machine-readable: <a href="/health">/health</a>, <a href="/servers">/servers</a>, <a href="/hosting">/hosting</a>.</p>
</main></body></html>
"#,
        version = env!("CARGO_PKG_VERSION"),
        retention_min = retention / 60,
    ))
}

/// Text for an HTML page. Every value the status page shows goes through it,
/// operator config included.
fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
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
    let identity = hex::encode(identity.as_bytes());
    drop(auth);
    // Whether this account is an admin, and its latest request to become one,
    // where this index hosts: a page needs both to offer the right thing.
    let (admin, admin_request) = match &state.hosting {
        Some(h) => h.admin_status(&crate::quota::AccountId(identity.clone())),
        None => (false, None),
    };
    Ok(Json(json!({ "identity": identity, "admin": admin, "admin_request": admin_request })))
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
            "regions": h.config().regions(),
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

/// Stop one of the caller's own servers. Its files and address stay.
async fn stop(
    State(state): State<Arc<IndexState>>,
    headers: HeaderMap,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> ApiResult<serde_json::Value> {
    let hosting = hosting_of(&state)?;
    let account = account_of(&state, &headers).await?;
    hosting
        .stop(&account, &id)
        .await
        .map(|()| Json(json!({ "stopped": id })))
        .map_err(owner_op_failure)
}

/// Move one of the caller's own servers to another node, with its files
/// (`Hosting::relocate`). The reply is the server as it now stands, under its
/// new id.
/// A move's body: `{"node": "<worker>"}` picks the worker; absent or empty,
/// the worker with the most room in the server's region.
#[derive(Deserialize, Default)]
struct RelocateBody {
    #[serde(default)]
    node: Option<String>,
}

async fn relocate(
    State(state): State<Arc<IndexState>>,
    headers: HeaderMap,
    axum::extract::Path(id): axum::extract::Path<String>,
    body: Option<Json<RelocateBody>>,
) -> ApiResult<crate::hosting::HostedInstance> {
    let hosting = hosting_of(&state)?;
    let account = account_of(&state, &headers).await?;
    let body = body.map(|Json(b)| b).unwrap_or_default();
    let target = match body.node.as_deref().filter(|n| !n.is_empty()) {
        Some(node) => crate::hosting::MoveTarget::Worker(node),
        None => crate::hosting::MoveTarget::Auto,
    };
    hosting.relocate_to(&account, &id, target).await.map(Json).map_err(owner_op_failure)
}

async fn move_targets(
    State(state): State<Arc<IndexState>>,
    headers: HeaderMap,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> ApiResult<crate::hosting::MoveTargets> {
    let hosting = hosting_of(&state)?;
    let account = account_of(&state, &headers).await?;
    hosting.move_targets(&account, &id).await.map(Json).map_err(owner_op_failure)
}

#[derive(Deserialize)]
struct RegionRequestBody {
    region: String,
}

async fn region_request(
    State(state): State<Arc<IndexState>>,
    headers: HeaderMap,
    axum::extract::Path(id): axum::extract::Path<String>,
    Json(body): Json<RegionRequestBody>,
) -> ApiResult<crate::regions::RegionRequest> {
    let hosting = hosting_of(&state)?;
    let account = account_of(&state, &headers).await?;
    hosting
        .request_region(&account, &id, &body.region, std::time::SystemTime::now())
        .await
        .map(Json)
        .map_err(owner_op_failure)
}

async fn my_region_requests(
    State(state): State<Arc<IndexState>>,
    headers: HeaderMap,
) -> ApiResult<Vec<crate::regions::RegionRequest>> {
    let hosting = hosting_of(&state)?;
    let account = account_of(&state, &headers).await?;
    Ok(Json(hosting.region_requests_for(&account)))
}

/// A request id from a path is checked as one this index made before it is
/// looked up: "no such request" either way, so asking reveals nothing.
fn request_id(rid: &str) -> Result<&str, (StatusCode, Json<ApiError>)> {
    if crate::regions::valid_request_id(rid) {
        Ok(rid)
    } else {
        Err(fail(StatusCode::NOT_FOUND, "no such request"))
    }
}

/// "Only an admin" is 403, "no such request" 404, anything else 400.
fn region_failure(e: anyhow::Error) -> (StatusCode, Json<ApiError>) {
    let text = format!("{e:#}");
    if text.starts_with("only an admin") {
        fail(StatusCode::FORBIDDEN, e)
    } else if text == "no such request" || text == "no such instance" {
        fail(StatusCode::NOT_FOUND, e)
    } else {
        fail(StatusCode::BAD_REQUEST, e)
    }
}

async fn cancel_region_request(
    State(state): State<Arc<IndexState>>,
    headers: HeaderMap,
    axum::extract::Path(rid): axum::extract::Path<String>,
) -> ApiResult<crate::regions::RegionRequest> {
    let hosting = hosting_of(&state)?;
    let account = account_of(&state, &headers).await?;
    let rid = request_id(&rid)?;
    hosting.cancel_region_request(&account, rid).map(Json).map_err(region_failure)
}

async fn admin_region_requests(
    State(state): State<Arc<IndexState>>,
    headers: HeaderMap,
) -> ApiResult<Vec<crate::regions::RegionRequest>> {
    let hosting = hosting_of(&state)?;
    let account = account_of(&state, &headers).await?;
    hosting.all_region_requests(&account).map(Json).map_err(region_failure)
}

async fn approve_region_request(
    State(state): State<Arc<IndexState>>,
    headers: HeaderMap,
    axum::extract::Path(rid): axum::extract::Path<String>,
) -> ApiResult<crate::regions::RegionRequest> {
    let hosting = hosting_of(&state)?;
    let account = account_of(&state, &headers).await?;
    let rid = request_id(&rid)?;
    hosting
        .approve_region_request(&account, rid, std::time::SystemTime::now())
        .await
        .map(Json)
        .map_err(region_failure)
}

/// A note or reason from a request body, held to one short line of text.
fn short_text(text: Option<String>) -> Option<String> {
    text.map(|n| n.chars().filter(|c| !c.is_control()).take(300).collect::<String>())
        .filter(|n| !n.trim().is_empty())
}

#[derive(Deserialize, Default)]
struct AskAdminBody {
    #[serde(default)]
    note: Option<String>,
}

/// Any account asks to become an admin of this index; an admin decides.
async fn ask_admin(
    State(state): State<Arc<IndexState>>,
    headers: HeaderMap,
    body: Option<Json<AskAdminBody>>,
) -> ApiResult<crate::admins::AdminRequest> {
    let hosting = hosting_of(&state)?;
    let account = account_of(&state, &headers).await?;
    let note = short_text(body.and_then(|Json(b)| b.note));
    hosting.request_admin(&account, note, std::time::SystemTime::now()).map(Json).map_err(region_failure)
}

async fn admin_requests(
    State(state): State<Arc<IndexState>>,
    headers: HeaderMap,
) -> ApiResult<Vec<crate::admins::AdminRequest>> {
    let hosting = hosting_of(&state)?;
    let account = account_of(&state, &headers).await?;
    hosting.admin_requests(&account).map(Json).map_err(region_failure)
}

async fn approve_admin_request(
    State(state): State<Arc<IndexState>>,
    headers: HeaderMap,
    axum::extract::Path(rid): axum::extract::Path<String>,
) -> ApiResult<crate::admins::AdminRequest> {
    let hosting = hosting_of(&state)?;
    let account = account_of(&state, &headers).await?;
    let rid = request_id(&rid)?;
    hosting.approve_admin_request(&account, rid, std::time::SystemTime::now()).map(Json).map_err(region_failure)
}

async fn deny_admin_request(
    State(state): State<Arc<IndexState>>,
    headers: HeaderMap,
    axum::extract::Path(rid): axum::extract::Path<String>,
    body: Option<Json<DenyBody>>,
) -> ApiResult<crate::admins::AdminRequest> {
    let hosting = hosting_of(&state)?;
    let account = account_of(&state, &headers).await?;
    let rid = request_id(&rid)?;
    let reason = short_text(body.and_then(|Json(b)| b.note));
    hosting.deny_admin_request(&account, rid, reason, std::time::SystemTime::now()).map(Json).map_err(region_failure)
}

async fn admins_list(
    State(state): State<Arc<IndexState>>,
    headers: HeaderMap,
) -> ApiResult<Vec<crate::hosting::AdminView>> {
    let hosting = hosting_of(&state)?;
    let account = account_of(&state, &headers).await?;
    hosting.admins(&account).map(Json).map_err(region_failure)
}

async fn revoke_admin(
    State(state): State<Arc<IndexState>>,
    headers: HeaderMap,
    axum::extract::Path(identity): axum::extract::Path<String>,
) -> ApiResult<serde_json::Value> {
    let hosting = hosting_of(&state)?;
    let account = account_of(&state, &headers).await?;
    if !(identity.len() == 32 && identity.bytes().all(|b| b.is_ascii_hexdigit())) {
        return Err(fail(StatusCode::NOT_FOUND, "no such admin"));
    }
    hosting.revoke_admin(&account, &identity).map(|()| Json(json!({ "revoked": identity }))).map_err(region_failure)
}

async fn workers(
    State(state): State<Arc<IndexState>>,
    headers: HeaderMap,
) -> ApiResult<Vec<crate::hosting::WorkerView>> {
    let hosting = hosting_of(&state)?;
    let account = account_of(&state, &headers).await?;
    hosting.workers(&account).await.map(Json).map_err(region_failure)
}

#[derive(Deserialize, Default)]
struct DenyBody {
    #[serde(default)]
    note: Option<String>,
}

async fn deny_region_request(
    State(state): State<Arc<IndexState>>,
    headers: HeaderMap,
    axum::extract::Path(rid): axum::extract::Path<String>,
    body: Option<Json<DenyBody>>,
) -> ApiResult<crate::regions::RegionRequest> {
    let hosting = hosting_of(&state)?;
    let account = account_of(&state, &headers).await?;
    let rid = request_id(&rid)?;
    // A note is shown to the owner as text; held to one short line.
    let note = short_text(body.and_then(|Json(b)| b.note));
    hosting
        .deny_region_request(&account, rid, note, std::time::SystemTime::now())
        .map(Json)
        .map_err(region_failure)
}

#[derive(Deserialize)]
struct LogsQuery {
    lines: Option<u32>,
}

/// The last lines one of the caller's own servers wrote. Read-only: it types
/// nothing at the server.
async fn logs(
    State(state): State<Arc<IndexState>>,
    headers: HeaderMap,
    axum::extract::Path(id): axum::extract::Path<String>,
    Query(q): Query<LogsQuery>,
) -> ApiResult<platform_agent::uplink_wire::LogsResp> {
    let hosting = hosting_of(&state)?;
    let account = account_of(&state, &headers).await?;
    hosting
        .logs(&account, &id, q.lines.unwrap_or(200).clamp(1, 2000))
        .await
        .map(Json)
        .map_err(owner_op_failure)
}

async fn backups(
    State(state): State<Arc<IndexState>>,
    headers: HeaderMap,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> ApiResult<platform_agent::uplink_wire::BackupsResp> {
    let hosting = hosting_of(&state)?;
    let account = account_of(&state, &headers).await?;
    hosting.backups(&account, &id).await.map(Json).map_err(owner_op_failure)
}

/// Back up one of the caller's own servers now.
async fn backup_create(
    State(state): State<Arc<IndexState>>,
    headers: HeaderMap,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> ApiResult<platform_agent::backups::Backup> {
    let hosting = hosting_of(&state)?;
    let account = account_of(&state, &headers).await?;
    hosting.backup_create(&account, &id).await.map(Json).map_err(owner_op_failure)
}

async fn backup_restore(
    State(state): State<Arc<IndexState>>,
    headers: HeaderMap,
    axum::extract::Path((id, name)): axum::extract::Path<(String, String)>,
) -> ApiResult<serde_json::Value> {
    let hosting = hosting_of(&state)?;
    let account = account_of(&state, &headers).await?;
    hosting
        .backup_restore(&account, &id, &name)
        .await
        .map(|()| Json(json!({ "instance_id": id, "restored": name })))
        .map_err(owner_op_failure)
}

async fn backup_delete(
    State(state): State<Arc<IndexState>>,
    headers: HeaderMap,
    axum::extract::Path((id, name)): axum::extract::Path<(String, String)>,
) -> ApiResult<serde_json::Value> {
    let hosting = hosting_of(&state)?;
    let account = account_of(&state, &headers).await?;
    hosting
        .backup_delete(&account, &id, &name)
        .await
        .map(|()| Json(json!({ "instance_id": id, "deleted": name })))
        .map_err(owner_op_failure)
}

/// Start one of the caller's own stopped servers again.
async fn start(
    State(state): State<Arc<IndexState>>,
    headers: HeaderMap,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> ApiResult<serde_json::Value> {
    let hosting = hosting_of(&state)?;
    let account = account_of(&state, &headers).await?;
    hosting
        .start(&account, &id)
        .await
        .map(|()| Json(json!({ "started": id })))
        .map_err(owner_op_failure)
}

/// "no such instance" covers both missing and not-yours and is a 404, so
/// these routes cannot be used to enumerate other people's instances either;
/// anything else (already running, a node that refused) is the caller's to
/// read.
fn owner_op_failure(e: anyhow::Error) -> (StatusCode, Json<ApiError>) {
    if format!("{e:#}") == "no such instance" {
        fail(StatusCode::NOT_FOUND, e)
    } else {
        fail(StatusCode::BAD_REQUEST, e)
    }
}

fn bearer(headers: &HeaderMap) -> Option<String> {
    let value = headers.get(axum::http::header::AUTHORIZATION)?.to_str().ok()?;
    let token = value.strip_prefix("Bearer ").or_else(|| value.strip_prefix("bearer "))?;
    Some(token.trim().to_string())
}
