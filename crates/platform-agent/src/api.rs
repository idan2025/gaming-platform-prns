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
//! # It has no authentication, and that is why it is loopback-only
//!
//! Every route here creates or destroys containers. Anyone who can reach it can
//! run any image the operator configured, on this host. There is no token, no
//! signature and no identity check, so the boundary is "you are already on this
//! machine" — and `AgentConfig::validate` enforces it by refusing a non-loopback
//! `api_bind` rather than documenting the danger and hoping.
//!
//! Identity challenge/response against a Reticulum keypair is the phase-4 answer
//! (`DESIGN.md` §2.4). Until it exists, an operator who wants remote access puts
//! an authenticating proxy in front; the agent will not pretend to do it.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
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

pub fn router(agent: Arc<Agent>) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/instances", get(list).post(create))
        .route("/instances/:id/stop", post(stop))
        .route("/instances/:id", delete(remove))
        .route("/orphans", get(orphans))
        .route("/content/:game", post(install_content))
        .with_state(agent)
}

async fn health(State(agent): State<Arc<Agent>>) -> Json<serde_json::Value> {
    Json(json!({
        "ok": true,
        "max_instances": agent.config().max_instances,
        "port_range": {
            "start": agent.config().port_range.start,
            "end": agent.config().port_range.end,
        },
    }))
}

async fn list(State(agent): State<Arc<Agent>>) -> ApiResult<Vec<InstanceStatus>> {
    agent
        .list()
        .await
        .map(Json)
        .map_err(|e| fail(StatusCode::INTERNAL_SERVER_ERROR, e))
}

async fn create(
    State(agent): State<Arc<Agent>>,
    Json(spec): Json<InstanceSpec>,
) -> ApiResult<InstanceStatus> {
    agent
        .create(spec)
        .await
        .map(Json)
        // A rejected spec, an unconfigured game and a missing content directory
        // are all the caller's problem to fix, so they are 400s carrying the
        // explanation rather than 500s carrying nothing.
        .map_err(|e| fail(StatusCode::BAD_REQUEST, e))
}

async fn stop(
    State(agent): State<Arc<Agent>>,
    Path(id): Path<String>,
) -> ApiResult<serde_json::Value> {
    agent
        .stop(&id)
        .await
        .map(|()| Json(json!({ "stopped": id })))
        .map_err(|e| fail(StatusCode::BAD_REQUEST, e))
}

async fn remove(
    State(agent): State<Arc<Agent>>,
    Path(id): Path<String>,
) -> ApiResult<serde_json::Value> {
    agent
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
    State(agent): State<Arc<Agent>>,
    Path(game): Path<String>,
) -> ApiResult<serde_json::Value> {
    match agent.ensure_content(&game).await {
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
async fn orphans(State(agent): State<Arc<Agent>>) -> ApiResult<Vec<String>> {
    agent
        .orphan_dirs()
        .await
        .map(|dirs| Json(dirs.iter().map(|p| p.display().to_string()).collect()))
        .map_err(|e| fail(StatusCode::INTERNAL_SERVER_ERROR, e))
}
