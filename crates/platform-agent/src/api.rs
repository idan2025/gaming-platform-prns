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
use axum::response::{IntoResponse, Response};
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
    /// Content installs running in the background.
    pub installs: Arc<crate::install::Installs>,
    pub token: Option<Arc<String>>,
    /// Where packs live, so a pack can be imported into a running node. `None`
    /// disables the import route rather than guessing a directory: this route
    /// writes files the node will run games from.
    pub pack_dir: Option<Arc<std::path::PathBuf>>,
    /// Live control of the uplink node's mesh interfaces (`interfaces.rs`).
    /// `None` when the node was built with no uplink, in which case the routes
    /// answer "no uplink" rather than 404 — the difference between "this agent
    /// cannot do that" and "this agent has no mesh node to do it to".
    pub interfaces: Option<Arc<crate::interfaces::InterfaceManager>>,
}

pub fn router(agent: Arc<Agent>) -> Router {
    router_with_token(agent, None)
}

pub fn router_with_token(agent: Arc<Agent>, token: Option<String>) -> Router {
    router_full(agent, token, None, None)
}

pub fn router_full(
    agent: Arc<Agent>,
    token: Option<String>,
    pack_dir: Option<std::path::PathBuf>,
    interfaces: Option<Arc<crate::interfaces::InterfaceManager>>,
) -> Router {
    let state = ApiState {
        agent: agent.clone(),
        installs: Arc::new(crate::install::Installs::new()),
        token: token.map(Arc::new),
        pack_dir: pack_dir.map(Arc::new),
        interfaces,
    };
    Router::new()
        .route("/health", get(health))
        .route("/capacity", get(capacity))
        .route("/instances", get(list).post(create))
        .route("/instances/:id/stop", post(stop))
        .route("/instances/:id", delete(remove))
        .route("/orphans", get(orphans))
        .route("/content/:game", post(install_content).get(install_status))
        .route("/games", get(games))
        .route("/packs", post(import_pack))
        // Configuring the mesh interfaces binds sockets and joins meshes, so it
        // rides behind the same token gate as every container-creating route
        // (`interfaces.rs` "Authorization is the API's").
        .route("/interfaces", get(interfaces_status).post(interfaces_add))
        .route("/interfaces/:id", delete(interfaces_remove))
        .route("/interfaces/:id/rename", post(interfaces_rename))
        .route_layer(middleware::from_fn_with_state(state.clone(), require_token))
        // The web UI is served **outside** the auth layer, deliberately. A
        // login page that needed the token to load could never be reached, and
        // these three files are the same for every install — they contain no
        // secret and no data about this node. Everything they then ask for goes
        // through the layer above.
        //
        // Embedded rather than read from disk: the agent is meant to run in a
        // container, and a UI that depended on a directory being mounted is a
        // UI that is missing on exactly the deployment it was built for.
        .route("/", get(ui_index))
        .route("/app.js", get(ui_js))
        .route("/style.css", get(ui_css))
        .with_state(state)
}

const UI_INDEX: &str = include_str!("../webui/index.html");
const UI_JS: &str = include_str!("../webui/app.js");
const UI_CSS: &str = include_str!("../webui/style.css");

/// `no-store`, because the operator's browser holding a stale UI against a
/// freshly upgraded agent is a confusing bug to be handed.
fn asset(content_type: &'static str, body: &'static str) -> Response {
    (
        [
            (header::CONTENT_TYPE, content_type),
            (header::CACHE_CONTROL, "no-store"),
        ],
        body,
    )
        .into_response()
}

async fn ui_index() -> Response {
    asset("text/html; charset=utf-8", UI_INDEX)
}

async fn ui_js() -> Response {
    asset("text/javascript; charset=utf-8", UI_JS)
}

async fn ui_css() -> Response {
    asset("text/css; charset=utf-8", UI_CSS)
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

/// Install a game pack into this running node (`pack_import.rs`).
///
/// The reload afterwards goes through `Agent::reload_packs`, which re-runs the
/// operator's `[pack_trust]` policy. Writing the file and then inserting it
/// straight into the agent's map would be an import route that bypasses the
/// trust gate — the one thing that gate exists to stop. So a pack the policy
/// refuses lands on disk and still does not run, and the response says so.
async fn import_pack(
    State(state): State<ApiState>,
    Json(request): Json<crate::pack_import::ImportRequest>,
) -> ApiResult<serde_json::Value> {
    let Some(dir) = state.pack_dir.clone() else {
        return Err(fail(
            StatusCode::NOT_IMPLEMENTED,
            "this agent was not told where packs live, so it cannot install one",
        ));
    };
    let policy = state.agent.config().pack_trust_policy();
    let now = std::time::SystemTime::now();

    let imported = crate::pack_import::import(&request, &dir, &policy, now, |id| {
        state.agent.config().runtime_for(id).is_some()
    })
    .await
    .map_err(|e| fail(StatusCode::BAD_REQUEST, e))?;

    let reloaded = state
        .agent
        .reload_packs(&dir, now)
        .await
        .map_err(|e| fail(StatusCode::INTERNAL_SERVER_ERROR, e))?;

    // A pack the trust policy will not deploy is on disk and not loaded. Saying
    // which, and why, is the difference between "it did not work" and one line
    // of config.
    let refused = reloaded
        .refused
        .iter()
        .find(|r| r.pack.pack.id == imported.id)
        .map(|r| r.why());

    Ok(Json(json!({
        "imported": imported,
        "loaded": refused.is_none(),
        "refused_reason": refused,
    })))
}

async fn games(State(state): State<ApiState>) -> Json<Vec<GameOption>> {
    let config = state.agent.config();
    let packs = state.agent.packs().await;
    let mut out: Vec<GameOption> = packs
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

/// Start a server, fetching the game's files first if they are not here yet.
///
/// **Pressing Start on a game with no content begins the download rather than
/// refusing.** "Install it first" is a step the machine can take on its own,
/// and making a person take it is making them learn the difference between a
/// pack and an install to do the obvious thing.
///
/// It cannot become a *synchronous* download — that is the request timeout this
/// endpoint's sibling was just fixed for — so the honest answer is `202
/// Accepted` with the install's state, and the caller starts the server when it
/// finishes. `PLAN.md` §11.2 keeps installing as its own step for exactly that
/// reason; what changes here is that nobody has to *ask* for it.
async fn create(
    State(state): State<ApiState>,
    Json(spec): Json<InstanceSpec>,
) -> Result<(StatusCode, Json<serde_json::Value>), (StatusCode, Json<ApiError>)> {
    // The spec is judged before anything else, and a bad one is refused here
    // rather than falling through to the content path below. Without this, a
    // hostile instance id on a game whose files happen to be missing would be
    // answered with "downloading…" instead of "no": the validation failure
    // would be swallowed by a branch that only meant to be helpful about
    // content. Caught by `a_hostile_instance_id_is_refused`.
    spec.validate()
        .map_err(|e| fail(StatusCode::BAD_REQUEST, e))?;

    match state.agent.create(spec.clone()).await {
        Ok(status) => Ok((
            StatusCode::OK,
            Json(serde_json::to_value(status).unwrap_or_else(|_| json!({}))),
        )),
        Err(e) => {
            // Only a *missing content* failure turns into a download. An
            // unconfigured game or a mistyped id is the caller's to fix, and
            // starting a multi-gigabyte download over a typo would be worse
            // than the error they got.
            if !state.agent.content_is_missing(&spec.game_id).await {
                return Err(fail(StatusCode::BAD_REQUEST, e));
            }
            let started = crate::install::spawn(
                state.installs.clone(),
                state.agent.clone(),
                spec.game_id.clone(),
            )
            .await;
            Ok((
                StatusCode::ACCEPTED,
                Json(json!({
                    "installing": spec.game_id,
                    "started": started,
                    "status": state.installs.state(&spec.game_id).await,
                    "message": "This game's files are not on this node yet, so the download \
                                started. Your server will start once it finishes.",
                })),
            ))
        }
    }
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
/// Start this game's content install, and return immediately.
///
/// The download used to run inside this request. A Sven Co-op install is 2.7 GB
/// and takes tens of minutes — longer than a browser or a proxy will hold a
/// connection — so the request would time out while the work carried on
/// invisibly. Now the work is a task and this reports its state; poll `GET` on
/// the same path.
async fn install_content(
    State(state): State<ApiState>,
    Path(game): Path<String>,
) -> ApiResult<serde_json::Value> {
    // Refuse what will obviously fail *before* claiming a task, so a mistyped
    // game id is an error rather than a "Failed" state to poll for.
    if !state.agent.packs().await.contains_key(&game) {
        return Err(fail(
            StatusCode::NOT_FOUND,
            format!("no game pack installed for {game:?}"),
        ));
    }
    let started =
        crate::install::spawn(state.installs.clone(), state.agent.clone(), game.clone()).await;
    Ok(Json(json!({
        "game": game,
        "started": started,
        "status": state.installs.state(&game).await,
    })))
}

async fn install_status(
    State(state): State<ApiState>,
    Path(game): Path<String>,
) -> Json<serde_json::Value> {
    Json(json!({ "game": game, "status": state.installs.state(&game).await }))
}

// ---- Mesh interface configuration (`interfaces.rs`, `PLAN.md` §13.5) --------
//
// The host's way to link this node to the Reticulum mesh from the web UI: add a
// TCP client (dial a relay), a TCP server (be a relay), or LAN auto-discovery,
// each optionally IFAC-protected, and have it survive a restart. Every handler
// is thin — the manager owns the engine calls and the persistence — and an
// `InterfaceError` becomes a 400 with the operator-facing sentence, the same
// shape as the rest of this API.

/// Map the manager, or a clear 501 when this agent has no uplink node at all
/// (no `[uplink]` block). `interfaces == None` means the feature is not wired,
/// which is distinct from an uplink that is present but has no interfaces yet.
fn interface_manager(
    state: &ApiState,
) -> Result<&Arc<crate::interfaces::InterfaceManager>, (StatusCode, Json<ApiError>)> {
    state.interfaces.as_ref().ok_or_else(|| {
        fail(
            StatusCode::NOT_IMPLEMENTED,
            "this agent has no Reticulum uplink, so it has no mesh interfaces to configure. \
             Add an [uplink] section to the config and restart",
        )
    })
}

/// The Interfaces tab's whole read: whether the uplink is up, this node's
/// destination hash, and the live interface list.
async fn interfaces_status(
    State(state): State<ApiState>,
) -> ApiResult<crate::interfaces::InterfaceStatus> {
    Ok(Json(interface_manager(&state)?.status()))
}

/// Add one interface (`{"kind":"tcp","addr":"host:port",...}` or
/// `{"kind":"auto"}`). A bad address or a bind failure is the caller's to fix,
/// so it is a 400 with the reason, not a 500.
async fn interfaces_add(
    State(state): State<ApiState>,
    Json(req): Json<crate::interfaces::AddInterface>,
) -> ApiResult<serde_json::Value> {
    interface_manager(&state)?
        .add(req)
        .await
        .map(|()| Json(json!({ "ok": true })))
        .map_err(|e| fail(StatusCode::BAD_REQUEST, e))
}

/// Remove an interface by its hex id.
async fn interfaces_remove(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> ApiResult<serde_json::Value> {
    interface_manager(&state)?
        .remove(&id)
        .await
        .map(|()| Json(json!({ "removed": id })))
        .map_err(|e| fail(StatusCode::BAD_REQUEST, e))
}

#[derive(serde::Deserialize)]
struct RenameReq {
    name: String,
}

/// Rename an interface by its hex id — presentation only, so an operator can
/// tell "the relay" from "the LAN" in the list.
async fn interfaces_rename(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Json(req): Json<RenameReq>,
) -> ApiResult<serde_json::Value> {
    interface_manager(&state)?
        .rename(&id, req.name)
        .map(|()| Json(json!({ "renamed": id })))
        .map_err(|e| fail(StatusCode::BAD_REQUEST, e))
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
