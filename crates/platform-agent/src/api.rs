//! The agent's local API.
//!
//! # What this is, and what it is not
//!
//! It is the thing a launcher, a script, or a person with `curl` uses to run a
//! game server on **this** machine. It is not a control plane: there is no
//! central service in `PLAN.md` §8 phase 3, and by `DESIGN.md` §0 there must
//! never be one the agent depends on. An index or a hosted-deploy API arrives in
//! phase 4 as a convenience *on top of* an agent that already works alone.
//!
//! # Authentication, and why loopback is still the default
//!
//! Every route here creates or destroys containers, so anyone who can reach an
//! unauthenticated one can run any image the operator configured, on this host.
//! With no token that is exactly the situation, and the only honest boundary is
//! "you are already on this machine" — `AgentConfig::validate` enforces it by
//! refusing a non-loopback `api_bind` rather than documenting the danger and
//! hoping.
//!
//! Setting `api_token_file` is what lifts that, because it puts something
//! between the network and the container runtime. The rules, all of them
//! load-bearing:
//!
//! * **A token is required on every route once one is configured**, loopback
//!   included. "Local requests are trusted" is how a browser on the same
//!   machine, or any other program on it, becomes an unauthenticated caller.
//! * **The comparison is constant-time.** A byte-by-byte `==` on a secret leaks
//!   it to anyone patient enough to measure.
//! * **The token travels in a header, never a cookie.** A cookie is attached by
//!   the browser to cross-site requests too, which would make every page on the
//!   internet able to POST to this API from an operator's browser. A header the
//!   page must set explicitly cannot be forged that way, so there is no CSRF
//!   surface to defend.
//!
//! Identity challenge/response against a Reticulum keypair is the richer answer
//! and already exists for the uplink (`uplink.rs`, `DESIGN.md` §2.4). This is
//! the shared secret a browser can carry.

use std::sync::Arc;

use axum::extract::{Path, Request, State};
use axum::http::{header, StatusCode};
use axum::middleware::{self, Next};
use axum::response::Response;
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde::Serialize;
use serde_json::json;

use crate::agent::Agent;
use crate::instance::{InstanceSpec, InstanceStatus};

#[derive(Serialize)]
struct ApiError {
    error: String,
}

/// Every failure is a 4xx/5xx with the message, because an operator debugging
/// "my server will not start" needs the sentence about the missing content
/// directory, not a bare status code.
fn fail(status: StatusCode, e: impl std::fmt::Display) -> (StatusCode, Json<ApiError>) {
    (status, Json(ApiError { error: format!("{e:#}") }))
}

type ApiResult<T> = Result<Json<T>, (StatusCode, Json<ApiError>)>;

/// What the API needs besides the agent: the token, if one is configured.
#[derive(Clone)]
pub struct ApiState {
    pub agent: Arc<Agent>,
    pub token: Option<Arc<String>>,
}

pub fn router(agent: Arc<Agent>) -> Router {
    router_with_token(agent, None)
}

pub fn router_with_token(agent: Arc<Agent>, token: Option<String>) -> Router {
    let state = ApiState { agent: agent.clone(), token: token.map(Arc::new) };
    Router::new()
        .route("/health", get(health))
        .route("/capacity", get(capacity))
        .route("/instances", get(list).post(create))
        .route("/instances/:id/stop", post(stop))
        .route("/instances/:id", delete(remove))
        .route("/orphans", get(orphans))
        .route("/content/:game", post(install_content))
        .route("/games", get(games))
        .route_layer(middleware::from_fn_with_state(state.clone(), require_token))
        .with_state(state)
}

/// Reject anything without the token, on every route, once a token exists.
///
/// Deliberately not "unless the request came from loopback": a browser on the
/// operator's own machine is a loopback client, and so is every other program
/// on it. An exemption there would mean the token protects the network and
/// nothing else.
async fn require_token(
    State(state): State<ApiState>,
    request: Request,
    next: Next,
) -> Result<Response, (StatusCode, Json<ApiError>)> {
    let Some(expected) = &state.token else {
        // No token configured: `AgentConfig::validate` has already refused to
        // bind anywhere but loopback, so this is the local-only mode.
        return Ok(next.run(request).await);
    };
    let presented = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or("");
    if !constant_time_eq(presented.as_bytes(), expected.as_bytes()) {
        return Err(fail(StatusCode::UNAUTHORIZED, "a valid API token is required"));
    }
    Ok(next.run(request).await)
}

/// Compare without leaking where the first difference is.
///
/// The length is allowed to leak — it is not the secret — but the contents are
/// not, so every byte of the shorter input is examined regardless.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b) {
        diff |= x ^ y;
    }
    diff == 0
}

/// The games this node can actually run: a pack **and** a runtime for it.
///
/// A pack with no `[games.<id>]` entry is listed as not runnable rather than
/// hidden, because "why is my game missing" is the question an operator asks
/// next, and the answer is one line of their own config.
#[derive(Serialize)]
struct GameOption {
    id: String,
    display_name: String,
    runnable: bool,
    /// Why not, when it is not.
    reason: Option<String>,
    transport: String,
    default_port: u16,
    extra_ports: usize,
}

async fn games(State(state): State<ApiState>) -> Json<Vec<GameOption>> {
    let config = state.agent.config();
    let mut out: Vec<GameOption> = state
        .agent
        .packs()
        .values()
        .map(|pack| {
            let runtime = config.runtime_for(&pack.id);
            GameOption {
                runnable: runtime.is_some(),
                reason: runtime.is_none().then(|| {
                    format!(
                        "no [games.{}] section in this node's config, so nothing says which \
                         image runs it. An image selects the code this node executes, so a \
                         pack cannot name one",
                        pack.id
                    )
                }),
                id: pack.id.clone(),
                display_name: pack.display_name.clone(),
                transport: format!("{:?}", pack.transport).to_ascii_lowercase(),
                default_port: pack.default_port,
                extra_ports: pack.extra_ports.len(),
            }
        })
        .collect();
    out.sort_by(|a, b| a.id.cmp(&b.id));
    Json(out)
}

/// What this node has room for.
///
/// The same answer the Reticulum uplink gives, from the same
/// `Agent::capacity` — an index placing an instance must not get one story over
/// loopback and another over the mesh.
async fn capacity(State(state): State<ApiState>) -> Json<crate::uplink_wire::CapacityResp> {
    Json(state.agent.capacity().await)
}

async fn health(State(state): State<ApiState>) -> Json<serde_json::Value> {
    Json(json!({
        "ok": true,
        "max_instances": state.agent.config().max_instances,
        "port_range": {
            "start": state.agent.config().port_range.start,
            "end": state.agent.config().port_range.end,
        },
    }))
}

/// Listing asks each running game how many players it has, because an index
/// reaping idle instances must not read "could not ask" as "empty".
async fn list(State(state): State<ApiState>) -> ApiResult<Vec<InstanceStatus>> {
    state.agent
        .list_detailed()
        .await
        .map(Json)
        .map_err(|e| fail(StatusCode::INTERNAL_SERVER_ERROR, e))
}

async fn create(
    State(state): State<ApiState>,
    Json(spec): Json<InstanceSpec>,
) -> ApiResult<InstanceStatus> {
    state.agent
        .create(spec)
        .await
        .map(Json)
        // A rejected spec, an unconfigured game and a missing content directory
        // are all the caller's problem to fix, so they are 400s carrying the
        // explanation rather than 500s carrying nothing.
        .map_err(|e| fail(StatusCode::BAD_REQUEST, e))
}

async fn stop(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> ApiResult<serde_json::Value> {
    state.agent
        .stop(&id)
        .await
        .map(|()| Json(json!({ "stopped": id })))
        .map_err(|e| fail(StatusCode::BAD_REQUEST, e))
}

async fn remove(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> ApiResult<serde_json::Value> {
    state.agent
        .remove(&id)
        .await
        .map(|()| Json(json!({ "removed": id })))
        .map_err(|e| fail(StatusCode::BAD_REQUEST, e))
}

/// Install a game's content from its pack's `[content]` driver.
///
/// Separate from create on purpose (`Agent::ensure_content`): this call can run
/// for a long time and moves gigabytes, and an operator asking for that should
/// be the one who asked for it. Idempotent — content that is already installed
/// is reported, never re-fetched.
async fn install_content(
    State(state): State<ApiState>,
    Path(game): Path<String>,
) -> ApiResult<serde_json::Value> {
    match state.agent.ensure_content(&game).await {
        Ok(crate::content::Provisioned::AlreadyInstalled(dir)) => Ok(Json(json!({
            "game": game,
            "installed": false,
            "dir": dir.display().to_string(),
        }))),
        Ok(crate::content::Provisioned::Installed { dir, bytes }) => Ok(Json(json!({
            "game": game,
            "installed": true,
            "dir": dir.display().to_string(),
            "bytes": bytes,
        }))),
        // A manual pack, a node that has not opted in, a digest that did not
        // match: all things the caller has to act on, all carrying the sentence
        // that says what to do.
        Err(e) => Err(fail(StatusCode::BAD_REQUEST, e)),
    }
}

/// Instance directories with no container. Reported, never deleted — that is a
/// player's saved state, and the agent does not decide to discard it.
async fn orphans(State(state): State<ApiState>) -> ApiResult<Vec<String>> {
    state.agent
        .orphan_dirs()
        .await
        .map(|dirs| Json(dirs.iter().map(|p| p.display().to_string()).collect()))
        .map_err(|e| fail(StatusCode::INTERNAL_SERVER_ERROR, e))
}
