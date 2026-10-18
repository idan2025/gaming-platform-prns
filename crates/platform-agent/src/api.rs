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

use axum::extract::{Path, Query, Request, State};
use axum::http::{header, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
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
        installs: agent.installs(),
        token: token.map(Arc::new),
        pack_dir: pack_dir.map(Arc::new),
        interfaces,
    };
    Router::new()
        .route("/health", get(health))
        .route("/capacity", get(capacity))
        .route("/instances", get(list).post(create))
        .route("/usage", get(usage))
        .route("/storage", get(storage))
        .route("/instances/:id/logs", get(logs))
        .route("/instances/:id/players", get(players))
        .route("/instances/:id/restart-on-crash", post(set_restart_on_crash))
        .route("/orphans/summary", get(orphans_summary))
        .route("/orphans/clean", post(orphans_clean))
        .route("/instances/:id/stop", post(stop))
        .route("/instances/:id/restart", post(restart))
        .route("/instances/:id/start", post(start))
        .route("/instances/:id/map", post(change_map))
        .route("/instances/:id/console", post(send_console))
        // Chat (ROADMAP.md §6.1): loopback only, like the console. The
        // uplink must never gain these, or an index could speak on, or
        // silence, a node's servers.
        .route("/instances/:id/chat", get(chat).post(chat_say).put(set_chat))
        .route("/instances/:id/chat/mute", post(chat_mute))
        .route("/instances/:id/restart-schedule", axum::routing::put(set_restart_schedule))
        .route("/restart-schedules", get(restart_schedules))
        .route("/instances/:id/backups", get(list_backups).post(create_backup))
        .route("/instances/:id/backups/:name", delete(delete_backup))
        .route("/instances/:id/backups/:name/restore", post(restore_backup))
        .route("/instances/:id/backup-schedule", get(backup_schedule).put(set_backup_schedule))
        .route("/instances/:id/backups/:name/file", get(download_backup))
        .route(
            "/instances/:id/backups/import",
            post(import_backup).layer(axum::extract::DefaultBodyLimit::disable()),
        )
        .route("/backups", get(all_backups))
        .route("/backups/:id", delete(delete_removed_backups))
        .route("/instances/:id/bots", post(set_bots))
        // Moderation (MINECRAFT.md §4): loopback only, like the console.
        .route("/instances/:id/moderation", get(moderation_view).post(moderate))
        .route(
            "/instances/:id/world",
            post(import_world).layer(axum::extract::DefaultBodyLimit::disable()),
        )
        .route("/resources/check", get(resource_check))
        .route("/minecraft/versions", get(minecraft_versions))
        .route("/instances/:id", delete(remove))
        .route("/orphans", get(orphans))
        .route("/content", get(installed))
        .route(
            "/content/:game",
            post(install_content).get(install_status).delete(cancel_install),
        )
        .route("/content/:game/:version", delete(delete_content))
        .route("/content/:game/:version/use", post(use_copy))
        .route("/updates", get(updates))
        .route("/updates/settings", axum::routing::put(set_update_settings))
        .route("/updates/:game/check", post(check_update))
        .route("/updates/:game/snooze", post(snooze_update).delete(unsnooze_update))
        .route("/updates/:game/policy", axum::routing::put(set_update_policy))
        .route("/instances/:id/move", post(move_instance))
        .route("/games", get(games))
        .route("/games/:game/maps", get(game_maps))
        .route("/packs", post(import_pack))
        .route("/packs/repository", get(list_repository))
        .route("/packs/official", get(official_status).put(set_official))
        .route("/packs/official/sync", post(sync_official))
        .route("/packs/:id", delete(remove_pack))
        // Configuring the mesh interfaces binds sockets and joins meshes, so it
        // rides behind the same token gate as every container-creating route
        // (`interfaces.rs` "Authorization is the API's").
        .route("/mesh", get(mesh_status))
        .route("/mesh/announce", post(mesh_announce))
        .route("/mesh/interfaces", get(mesh_interfaces).post(mesh_interface_add))
        .route("/mesh/interfaces/:id", delete(mesh_interface_remove))
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
    /// Whether this node can talk to a running server of this game, so the UI
    /// can offer a live map change instead of offering one and then explaining
    /// that the pack declares no console.
    console: bool,
    /// Whether this game has bots a node can ask for. Same reasoning as
    /// `console`: the UI hides the control rather than offering one that
    /// answers 400.
    bots: bool,
    /// The Steam app players own, from the pack's `[launch]`. The UI shows the
    /// game's artwork from it and uses it for nothing else.
    steam_app_id: Option<u32>,
    /// The picture to show (`GamePack::art_url`), or none for a lettered tile.
    art_url: Option<String>,
    /// The pack's tier under this node's `[pack_trust]` (§11.4's words), read
    /// from its file and signature now. `None` when there is no pack directory
    /// to read it from.
    trust: Option<String>,
    /// Who signed it, hex identity hash, when someone did.
    signer: Option<String>,
    /// Where the game's files stand on this node, `None` without a runtime.
    content: Option<crate::agent::ContentState>,
    /// The licence a server's owner must accept, if the pack names one.
    eula: Option<game_bridge::pack::PackEula>,
    /// A Minecraft game: the create form asks for a server type and version,
    /// memory and settings (`MINECRAFT.md`).
    minecraft: bool,
    /// For a Minecraft game, the build a server gets when none is picked: the
    /// node's own `content_version`.
    default_build: Option<String>,
}

/// What this node is announcing on the mesh, per running game server.
///
/// Distinct from `/interfaces`, which is about the `[uplink]` — the control
/// destination an index drives this node through. A node can host games on the
/// mesh with no uplink at all, and conflating the two is how an operator ends
/// up being told they have "no mesh" while their servers are announced on it.
async fn mesh_status(State(state): State<ApiState>) -> Json<serde_json::Value> {
    let mesh = state.agent.mesh();
    Json(json!({
        "enabled": mesh.enabled(),
        "servers": mesh.status().await,
        "note": if mesh.enabled() {
            "Each running server is announced under its own destination."
        } else {
            "This node runs its games LAN-only. Add a [mesh] section to its config to \
             announce them on Reticulum."
        },
    }))
}

/// Announce every server this node runs now, or one when `instance` is given,
/// rather than at each announcer's next tick. A request, not a broadcast
/// decision: each announcer keeps its own floor, so pressing it repeatedly
/// cannot become a storm on somebody's slow link.
#[derive(Debug, Default, Deserialize)]
struct AnnounceReq {
    #[serde(default)]
    instance: Option<String>,
}

async fn mesh_announce(
    State(state): State<ApiState>,
    body: Option<Json<AnnounceReq>>,
) -> ApiResult<serde_json::Value> {
    let req = body.map(|Json(b)| b).unwrap_or_default();
    let mesh = state.agent.mesh();
    if !mesh.enabled() {
        return Err(fail(
            StatusCode::CONFLICT,
            "this node runs its games LAN-only, so there is nothing to announce. Add a [mesh] \
             section to its config and restart",
        ));
    }
    let asked = mesh.announce_now(req.instance.as_deref()).await;
    if req.instance.is_some() && asked == 0 {
        return Err(fail(StatusCode::NOT_FOUND, "that server is not announced on the mesh"));
    }
    Ok(Json(json!({ "announced": asked })))
}

/// The interfaces every game bridge on this node attaches.
///
/// Distinct from `/interfaces`, which configures the `[uplink]`. These are what
/// carry **game** traffic, and they are the ones an operator hosting servers
/// actually needs.
async fn mesh_interfaces(State(state): State<ApiState>) -> Json<serde_json::Value> {
    let mesh = state.agent.mesh();
    Json(json!({
        "enabled": mesh.enabled(),
        "configured": mesh.interfaces().await,
    }))
}

async fn mesh_interface_add(
    State(state): State<ApiState>,
    Json(iface): Json<crate::mesh::MeshInterface>,
) -> ApiResult<serde_json::Value> {
    state
        .agent
        .mesh()
        .add_interface(iface)
        .await
        .map(|_| Json(json!({ "ok": true })))
        .map_err(|e| fail(StatusCode::BAD_REQUEST, e))
}

async fn mesh_interface_remove(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> ApiResult<serde_json::Value> {
    let removed = state
        .agent
        .mesh()
        .remove_interface(&id)
        .await
        .map_err(|e| fail(StatusCode::INTERNAL_SERVER_ERROR, e))?;
    Ok(Json(json!({
        "removed": removed,
        // The engine cannot detach a live interface, and saying otherwise would
        // have an operator watch traffic keep flowing and conclude the button
        // is broken.
        "note": "Forgotten for future bridges. Servers already running keep this interface until they restart.",
    })))
}

#[derive(Deserialize)]
struct RepositoryQuery {
    url: String,
}

/// What a pack repository lists, and which of it this node has: `available`,
/// `installed` (the same bytes) or `update` (other bytes). Read by the node,
/// not the browser, so the page needs no cross-origin fetch; installing one
/// is `POST /packs` with `repository` and `id`.
async fn list_repository(
    State(state): State<ApiState>,
    Query(q): Query<RepositoryQuery>,
) -> ApiResult<serde_json::Value> {
    node_repository_allowed(&state, &q.url)?;
    let index = crate::pack_import::read_repository(&q.url)
        .await
        .map_err(|e| fail(StatusCode::BAD_GATEWAY, e))?;
    let dir = state.pack_dir.clone();
    let packs: Vec<_> = index
        .packs
        .iter()
        .map(|e| {
            let have = dir
                .as_ref()
                .and_then(|d| std::fs::read(crate::pack_import::pack_path(d, &e.id)).ok());
            let status = match have {
                None => "available",
                Some(b) if game_bridge::repo::sha256_hex(&b) == e.sha256 => "installed",
                Some(_) => "update",
            };
            json!({
                "id": e.id,
                "display_name": e.display_name,
                "signed": e.signed,
                "lan": e.lan,
                "status": status,
            })
        })
        .collect();
    Ok(Json(json!({ "name": index.name, "packs": packs })))
}

/// Install a game pack into this running node (`pack_import.rs`).
///
/// The reload afterwards goes through `Agent::reload_packs`, which re-runs the
/// operator's `[pack_trust]` policy. Writing the file and then inserting it
/// straight into the agent's map would be an import route that bypasses the
/// trust gate — the one thing that gate exists to stop. So a pack the policy
/// refuses lands on disk and still does not run, and the response says so.
async fn official_status(State(state): State<ApiState>) -> Json<crate::official::OfficialStatus> {
    Json(state.agent.official().status())
}

/// Either switch, or both: taking the official packs, and seeding them on
/// the mesh. A seeding change takes effect at the next pass, which the page
/// asks for right after.
#[derive(Deserialize)]
struct OfficialReq {
    #[serde(default)]
    on: Option<bool>,
    #[serde(default)]
    seed: Option<bool>,
}

async fn set_official(
    State(state): State<ApiState>,
    Json(req): Json<OfficialReq>,
) -> ApiResult<crate::official::OfficialStatus> {
    let official = state.agent.official();
    if let Some(on) = req.on {
        official.set_on(on).map_err(|e| fail(StatusCode::INTERNAL_SERVER_ERROR, e))?;
    }
    if let Some(seed) = req.seed {
        official.set_seeding(seed).map_err(|e| fail(StatusCode::INTERNAL_SERVER_ERROR, e))?;
    }
    Ok(Json(official.status()))
}

/// Read the official list now, as the timer would.
async fn sync_official(State(state): State<ApiState>) -> ApiResult<crate::official::OfficialStatus> {
    let Some(dir) = state.pack_dir.clone() else {
        return Err(fail(
            StatusCode::NOT_IMPLEMENTED,
            "this agent was not told where packs live, so it cannot install one",
        ));
    };
    Ok(Json(state.agent.official().sync(&state.agent, &dir).await))
}

/// A folder repository on a node must be under `<data_root>/mirrors/`: a
/// repository address from a request would otherwise be a way to read the
/// node's disk. An https one is unchanged.
fn node_repository_allowed(state: &ApiState, repository: &str) -> Result<(), (StatusCode, Json<ApiError>)> {
    let Ok(url) = game_bridge::repo::check_repo_url(repository) else { return Ok(()) };
    if !url.starts_with("file://") {
        return Ok(());
    }
    let mirrors = state.agent.config().data_root.join("mirrors");
    let path = url::Url::parse(&url).ok().and_then(|u| u.to_file_path().ok());
    let inside = path
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        .and_then(|d| std::fs::canonicalize(d).ok())
        .zip(std::fs::canonicalize(&mirrors).ok())
        .is_some_and(|(d, m)| d.starts_with(m));
    if inside {
        Ok(())
    } else {
        Err(fail(
            StatusCode::BAD_REQUEST,
            format!(
                "a folder repository on this node must be inside {} (put a mirror there with `game-bridge repo mirror`)",
                mirrors.display()
            ),
        ))
    }
}

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
    if let Some(repository) = &request.repository {
        node_repository_allowed(&state, repository)?;
    }
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

/// Take a game pack off this node (`pack_import::remove`). Refused while any
/// server of the game exists, running or stopped (a stopped one could not be
/// started again without its pack), or while its files download. The game's
/// files stay; they show on the Installed page as unused.
async fn remove_pack(State(state): State<ApiState>, Path(id): Path<String>) -> ApiResult<serde_json::Value> {
    let Some(dir) = state.pack_dir.clone() else {
        return Err(fail(
            StatusCode::NOT_IMPLEMENTED,
            "this agent was not told where packs live, so it cannot remove one",
        ));
    };
    let servers: Vec<String> = state
        .agent
        .docker()
        .list_managed()
        .await
        .map_err(|e| fail(StatusCode::INTERNAL_SERVER_ERROR, e))?
        .into_iter()
        .filter(|c| c.game_id.as_deref() == Some(id.as_str()))
        .map(|c| c.instance_id)
        .collect();
    if !servers.is_empty() {
        return Err(fail(
            StatusCode::CONFLICT,
            format!(
                "{id} still has servers on this node ({}); remove them first. A stopped one counts: it could not be started again without its pack",
                servers.join(", ")
            ),
        ));
    }
    if state.installs.is_running(&id).await {
        return Err(fail(StatusCode::CONFLICT, format!("{id} is downloading; cancel it first")));
    }
    let removed = crate::pack_import::remove(&dir, &id)
        .map_err(|e| fail(StatusCode::INTERNAL_SERVER_ERROR, format!("removing the pack: {e}")))?
        .ok_or_else(|| fail(StatusCode::NOT_FOUND, format!("no pack with the id {id:?} in this node's pack directory")))?;
    state
        .agent
        .reload_packs(&dir, std::time::SystemTime::now())
        .await
        .map_err(|e| fail(StatusCode::INTERNAL_SERVER_ERROR, e))?;
    Ok(Json(json!({
        "removed": removed,
        "note": "Its game files are kept; the Installed page lists them as unused.",
    })))
}

async fn games(State(state): State<ApiState>) -> Json<Vec<GameOption>> {
    let config = state.agent.config();
    let packs = state.agent.packs().await;
    let policy = config.pack_trust_policy();
    let now = std::time::SystemTime::now();
    let tier = |id: &str| {
        let dir = state.pack_dir.as_ref()?;
        let path = crate::pack_import::pack_path(dir, id);
        // A pack loaded from a file named otherwise is not guessed at.
        game_bridge::GamePack::load_verified(&path, &policy, now).ok().map(|p| p.trust)
    };
    let mut contents = std::collections::HashMap::new();
    for id in packs.keys() {
        contents.insert(id.clone(), state.agent.content_state(id).await);
    }
    let mut out: Vec<GameOption> = packs
        .values()
        .map(|pack| {
            let runtime = config.runtime_for(&pack.id);
            // The node's own declaration wins, which is what makes a bots
            // control appear for Counter-Strike 1.6 on a node that installed
            // YaPB and stay absent on one that did not.
            let bots = runtime.and_then(|r| r.bots).is_some() || pack.bots.is_some();
            let trust = tier(&pack.id);
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
                console: pack.console.is_some(),
                bots: bots && pack.console.is_some(),
                steam_app_id: pack.launch.as_ref().and_then(|l| l.steam_app_id),
                art_url: pack.art_url(),
                signer: trust.as_ref().and_then(|t| t.signer()).map(|s| hex::encode(s.as_bytes())),
                trust: trust.map(|t| t.label().to_string()),
                content: contents.remove(&pack.id).flatten(),
                eula: pack.eula.clone(),
                minecraft: matches!(pack.content, game_bridge::content::PackContent::Minecraft {}),
                default_build: matches!(pack.content, game_bridge::content::PackContent::Minecraft {})
                    .then(|| runtime.map(|r| r.content_version.clone()))
                    .flatten(),
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
        // Which build is answering. The UI shows it in a corner, because
        // "did my redeploy land" is otherwise a question only `docker inspect`
        // can answer, and the person asking it is usually looking at a browser.
        "version": env!("CARGO_PKG_VERSION"),
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

/// Docker's resource counters for every running server. Rates are the
/// caller's to work out from two readings (`InstanceUsage`).
async fn usage(State(state): State<ApiState>) -> ApiResult<Vec<crate::instance::InstanceUsage>> {
    state.agent
        .usage()
        .await
        .map(Json)
        .map_err(|e| fail(StatusCode::INTERNAL_SERVER_ERROR, e))
}

#[derive(Deserialize)]
struct LogsQuery {
    lines: Option<usize>,
}

/// The tail of one server's log, as plain text.
async fn logs(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Query(q): Query<LogsQuery>,
) -> Result<String, (StatusCode, Json<ApiError>)> {
    state.agent
        .logs(&id, q.lines.unwrap_or(200))
        .await
        .map_err(|e| fail(StatusCode::NOT_FOUND, e))
}

/// Who is on a server, from the game. 404 for an unknown instance; `null` for
/// a game that could not be asked, which the page must not show as empty.
async fn players(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> ApiResult<Option<Vec<crate::agent::PlayerRow>>> {
    state.agent
        .players(&id)
        .await
        .map(Json)
        .map_err(|e| fail(StatusCode::NOT_FOUND, e))
}

#[derive(Deserialize)]
struct RestartOnCrashReq {
    on: bool,
}

async fn set_restart_on_crash(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Json(req): Json<RestartOnCrashReq>,
) -> ApiResult<serde_json::Value> {
    state.agent
        .set_restart_on_crash(&id, req.on)
        .await
        .map(|()| Json(json!({ "instance_id": id, "restart_on_crash": req.on })))
        .map_err(|e| fail(StatusCode::NOT_FOUND, e))
}

async fn orphans_summary(State(state): State<ApiState>) -> ApiResult<crate::agent::OrphanReport> {
    state.agent
        .orphan_report()
        .await
        .map(Json)
        .map_err(|e| fail(StatusCode::INTERNAL_SERVER_ERROR, e))
}

/// Deletes leftover instance directories. Only on request: see `clean_orphans`.
async fn orphans_clean(State(state): State<ApiState>) -> ApiResult<crate::agent::OrphanReport> {
    state.agent
        .clean_orphans()
        .await
        .map(Json)
        .map_err(|e| fail(StatusCode::INTERNAL_SERVER_ERROR, e))
}

async fn storage(State(state): State<ApiState>) -> ApiResult<crate::agent::Storage> {
    state.agent
        .storage()
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
            let version = spec.options.content_version.clone();
            if !state.agent.content_is_missing(&spec.game_id, version.as_deref()).await {
                return Err(fail(StatusCode::BAD_REQUEST, e));
            }
            let started = crate::install::spawn_version(
                state.installs.clone(),
                state.agent.clone(),
                spec.game_id.clone(),
                version,
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

/// Turn an instance off and on again, keeping its ports and its mesh identity.
async fn restart(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> ApiResult<serde_json::Value> {
    state
        .agent
        .restart(&id)
        .await
        .map(|_| Json(json!({ "instance_id": id, "restarted": true })))
        .map_err(|e| fail(StatusCode::BAD_REQUEST, e))
}

/// Start a stopped instance; refused while it runs (`Agent::start`).
async fn start(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> ApiResult<serde_json::Value> {
    state
        .agent
        .start(&id)
        .await
        .map(|_| Json(json!({ "instance_id": id, "started": true })))
        .map_err(|e| fail(StatusCode::BAD_REQUEST, e))
}

/// The maps this node has installed for one game, so a person picks from a
/// list instead of remembering a name.
///
/// Read off the content copy, not the pack: what is installed is a fact about
/// this machine. An empty list means the content is not here yet.
async fn game_maps(
    State(state): State<ApiState>,
    Path(game): Path<String>,
) -> ApiResult<serde_json::Value> {
    state
        .agent
        .available_maps(&game)
        .await
        .map(|maps| Json(json!({ "game_id": game, "maps": maps })))
        .map_err(|e| fail(StatusCode::BAD_REQUEST, e))
}

/// What `POST /instances/:id/map` carries. A map name and nothing else: the
/// command is this build's, never the caller's (`console.rs`).
#[derive(serde::Deserialize)]
struct ChangeMapReq {
    map: String,
}

#[derive(serde::Deserialize)]
struct BotsReq {
    count: u8,
}

/// Set how many bots a running server holds. Not a restart, and idempotent: the
/// number sent is a quota, so asking twice changes nothing the second time and
/// asking for zero empties the server.
///
/// A 400 with the reason for every way this fails — an unknown instance, a game
/// with no bots, a pack with no console to ask over, a count past the build's
/// ceiling — because each one is something the caller can act on.
async fn set_bots(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Json(req): Json<BotsReq>,
) -> ApiResult<serde_json::Value> {
    state
        .agent
        .set_bots(&id, req.count)
        .await
        .map(|lines| Json(json!({ "instance_id": id, "bots": req.count, "commands": lines })))
        .map_err(|e| fail(StatusCode::BAD_REQUEST, e))
}

/// Change a running server's map in place. Not a restart — see
/// [`Agent::change_map`]; the players stay connected.
///
/// A refusal is a 400 with the reason, because every way this fails is
/// something the caller can act on: an unknown instance, a game whose pack
/// declares no console, a map name that is not one, or a server that was
/// started by a build that left it no console to talk to.
async fn change_map(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Json(req): Json<ChangeMapReq>,
) -> ApiResult<serde_json::Value> {
    state
        .agent
        .change_map(&id, &req.map)
        .await
        .map(|command| Json(json!({ "instance_id": id, "map": req.map, "command": command })))
        .map_err(|e| fail(StatusCode::BAD_REQUEST, e))
}

/// A server's backups, newest first (`backups.rs`).
async fn list_backups(State(state): State<ApiState>, Path(id): Path<String>) -> ApiResult<Vec<crate::backups::Backup>> {
    state.agent.backups(&id).await.map(Json).map_err(|e| fail(StatusCode::NOT_FOUND, e))
}

/// Back up a server's writable files now.
async fn create_backup(State(state): State<ApiState>, Path(id): Path<String>) -> ApiResult<crate::backups::Backup> {
    state.agent.create_backup(&id).await.map(Json).map_err(|e| fail(StatusCode::CONFLICT, e))
}

/// Put a backup back over a stopped server's files.
async fn restore_backup(
    State(state): State<ApiState>,
    Path((id, name)): Path<(String, String)>,
) -> ApiResult<serde_json::Value> {
    state
        .agent
        .restore_backup(&id, &name)
        .await
        .map(|()| Json(json!({ "instance_id": id, "restored": name })))
        .map_err(|e| fail(StatusCode::CONFLICT, e))
}

async fn delete_backup(
    State(state): State<ApiState>,
    Path((id, name)): Path<(String, String)>,
) -> ApiResult<serde_json::Value> {
    state
        .agent
        .delete_backup(&id, &name)
        .await
        .map(|()| Json(json!({ "instance_id": id, "deleted": name })))
        .map_err(|e| fail(StatusCode::NOT_FOUND, e))
}

/// The largest backup the node takes in. A server's own files (saves,
/// settings, logs) are megabytes; this is a bound on a mistake, not a quota.
const MAX_BACKUP_UPLOAD: u64 = 4 * 1024 * 1024 * 1024;

/// A backup's file, for the operator to keep elsewhere. Streamed, never read
/// whole into memory.
async fn download_backup(
    State(state): State<ApiState>,
    Path((id, name)): Path<(String, String)>,
) -> Result<Response, (StatusCode, Json<ApiError>)> {
    let path = state.agent.backup_file(&id, &name).await.map_err(|e| fail(StatusCode::NOT_FOUND, e))?;
    let file = tokio::fs::File::open(&path).await.map_err(|e| fail(StatusCode::NOT_FOUND, e))?;
    let len = file.metadata().await.map(|m| m.len()).ok();
    let stream = futures_util::stream::unfold(file, |mut file| async move {
        use tokio::io::AsyncReadExt;
        let mut buf = vec![0u8; 64 * 1024];
        match file.read(&mut buf).await {
            Ok(0) => None,
            Ok(n) => {
                buf.truncate(n);
                Some((Ok::<_, std::io::Error>(axum::body::Bytes::from(buf)), file))
            }
            Err(e) => Some((Err(e), file)),
        }
    });
    let mut resp = Response::new(axum::body::Body::from_stream(stream));
    let headers = resp.headers_mut();
    headers.insert(header::CONTENT_TYPE, header::HeaderValue::from_static("application/gzip"));
    // The name is the agent's own shape (`backups::valid_name`): digits, `-`
    // and `.tar.gz`, so it is safe in a header as is.
    let file_name = format!("{id}-{name}");
    if let Ok(v) = header::HeaderValue::from_str(&format!("attachment; filename=\"{file_name}\"")) {
        headers.insert(header::CONTENT_DISPOSITION, v);
    }
    if let Some(len) = len {
        headers.insert(header::CONTENT_LENGTH, header::HeaderValue::from(len));
    }
    Ok(resp)
}

/// Take in a backup file for a server, as the request body. It needs a
/// `Content-Length`, is held to the disk reserve and `MAX_BACKUP_UPLOAD`,
/// and is kept only if it is a backup (`Agent::import_backup`).
async fn import_backup(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    headers: axum::http::HeaderMap,
    body: axum::body::Body,
) -> ApiResult<crate::backups::Backup> {
    use futures_util::StreamExt;
    use tokio::io::AsyncWriteExt;
    let declared = headers
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok())
        .ok_or_else(|| fail(StatusCode::LENGTH_REQUIRED, anyhow::anyhow!("send the file with its length")))?;
    if declared > MAX_BACKUP_UPLOAD {
        return Err(fail(StatusCode::PAYLOAD_TOO_LARGE, anyhow::anyhow!("a backup is at most 4 GB")));
    }
    let upload = state
        .agent
        .backup_upload_path(&id, Some(declared))
        .await
        .map_err(|e| fail(StatusCode::CONFLICT, e))?;
    let written = async {
        let mut file = tokio::fs::File::create(&upload).await?;
        let mut stream = body.into_data_stream();
        let mut total = 0u64;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(std::io::Error::other)?;
            total += chunk.len() as u64;
            if total > declared {
                return Err(std::io::Error::other("the upload is longer than it said"));
            }
            file.write_all(&chunk).await?;
        }
        file.flush().await?;
        Ok::<_, std::io::Error>(())
    }
    .await;
    if let Err(e) = written {
        let _ = std::fs::remove_file(&upload);
        return Err(fail(StatusCode::BAD_REQUEST, anyhow::anyhow!("the upload did not arrive: {e}")));
    }
    state.agent.import_backup(&id, &upload).await.map(Json).map_err(|e| fail(StatusCode::BAD_REQUEST, e))
}

/// A server's backup schedule, or `null`.
async fn backup_schedule(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> ApiResult<Option<crate::backups::StoredSchedule>> {
    state.agent.backup_schedule(&id).await.map(Json).map_err(|e| fail(StatusCode::NOT_FOUND, e))
}

/// `{"schedule": {"every_hours": 24, "keep": 7}}` backs a server up on a
/// schedule; `{"schedule": null}` stops it.
#[derive(Deserialize)]
struct BackupScheduleReq {
    schedule: Option<crate::backups::BackupSchedule>,
}

async fn set_backup_schedule(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Json(req): Json<BackupScheduleReq>,
) -> ApiResult<serde_json::Value> {
    state
        .agent
        .set_backup_schedule(&id, req.schedule)
        .await
        .map(|()| Json(json!({ "instance_id": id, "schedule": req.schedule })))
        .map_err(|e| fail(StatusCode::BAD_REQUEST, e))
}

/// Every server's backups, removed servers' included.
async fn all_backups(State(state): State<ApiState>) -> ApiResult<Vec<crate::agent::BackupSet>> {
    state.agent.all_backups().await.map(Json).map_err(|e| fail(StatusCode::INTERNAL_SERVER_ERROR, e))
}

/// Delete every backup of a server that was removed.
async fn delete_removed_backups(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> ApiResult<serde_json::Value> {
    state
        .agent
        .delete_removed_backups(&id)
        .await
        .map(|n| Json(json!({ "instance_id": id, "deleted": n })))
        .map_err(|e| fail(StatusCode::CONFLICT, e))
}

/// Every server's daily restart window, by id (`schedule.rs`).
async fn restart_schedules(
    State(state): State<ApiState>,
) -> ApiResult<std::collections::BTreeMap<String, crate::agent::ScheduleStatus>> {
    state.agent.restart_schedules().await.map(Json).map_err(|e| fail(StatusCode::INTERNAL_SERVER_ERROR, e))
}

/// `{"schedule": {"start_utc": 4, "end_utc": 6, "with_players": false}}` gives
/// a server a daily restart window; `{"schedule": null}` takes it away.
#[derive(Deserialize)]
struct ScheduleReq {
    schedule: Option<crate::schedule::RestartSchedule>,
}

async fn set_restart_schedule(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Json(req): Json<ScheduleReq>,
) -> ApiResult<serde_json::Value> {
    state
        .agent
        .set_restart_schedule(&id, req.schedule)
        .await
        .map(|()| Json(json!({ "instance_id": id, "schedule": req.schedule })))
        .map_err(|e| fail(StatusCode::BAD_REQUEST, e))
}

#[derive(Deserialize)]
struct ConsoleReq {
    line: String,
}

#[derive(Deserialize)]
struct ModerateReq {
    action: game_bridge::console::PlayerAction,
    target: String,
    #[serde(default)]
    reason: Option<String>,
}

/// Kick, ban, unban, op or deop (`Agent::moderate`).
async fn moderate(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Json(req): Json<ModerateReq>,
) -> ApiResult<serde_json::Value> {
    state
        .agent
        .moderate(&id, req.action, &req.target, req.reason.as_deref())
        .await
        .map(|lines| Json(json!({ "instance_id": id, "sent": lines })))
        .map_err(|e| fail(StatusCode::BAD_REQUEST, e))
}

async fn moderation_view(State(state): State<ApiState>, Path(id): Path<String>) -> ApiResult<serde_json::Value> {
    state.agent.moderation_view(&id).await.map(Json).map_err(|e| fail(StatusCode::BAD_REQUEST, e))
}

#[derive(Deserialize)]
struct ResourceQuery {
    game: String,
    #[serde(default)]
    min_mb: Option<u32>,
    #[serde(default)]
    max_mb: Option<u32>,
    #[serde(default)]
    cpus: Option<u16>,
}

/// Whether a size fits this host (`resources.rs`), for the create form to ask
/// while a person picks; `create` asks the same question again.
async fn resource_check(
    State(state): State<ApiState>,
    Query(q): Query<ResourceQuery>,
) -> Json<crate::resources::ResourceVerdict> {
    let memory = match (q.min_mb, q.max_mb) {
        (Some(min_mb), Some(max_mb)) => Some(crate::instance::MemorySpec { min_mb, max_mb }),
        (None, Some(max_mb)) => Some(crate::instance::MemorySpec { min_mb: max_mb, max_mb }),
        _ => None,
    };
    Json(state.agent.resource_check(&q.game, memory, q.cpus).await)
}

#[derive(Deserialize)]
struct VersionsQuery {
    server: String,
}

async fn minecraft_versions(
    State(state): State<ApiState>,
    Query(q): Query<VersionsQuery>,
) -> ApiResult<Vec<crate::minecraft::VersionChoice>> {
    let server = game_bridge::minecraft::MinecraftServer::from_word(&q.server)
        .ok_or_else(|| fail(StatusCode::BAD_REQUEST, anyhow::anyhow!("{:?} is not a Minecraft server type", q.server)))?;
    state.agent.minecraft_versions(server).await.map(Json).map_err(|e| fail(StatusCode::BAD_GATEWAY, e))
}

/// A world upload for a stopped Minecraft server (`Agent::import_world`),
/// streamed like a backup upload and held to the same limits.
async fn import_world(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    headers: axum::http::HeaderMap,
    body: axum::body::Body,
) -> ApiResult<serde_json::Value> {
    use futures_util::StreamExt;
    use tokio::io::AsyncWriteExt;
    let declared = headers
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok())
        .ok_or_else(|| fail(StatusCode::LENGTH_REQUIRED, anyhow::anyhow!("send the file with its length")))?;
    if declared > MAX_BACKUP_UPLOAD {
        return Err(fail(StatusCode::PAYLOAD_TOO_LARGE, anyhow::anyhow!("a world is at most 4 GB")));
    }
    let upload = state
        .agent
        .world_upload_path(&id, Some(declared))
        .await
        .map_err(|e| fail(StatusCode::CONFLICT, e))?;
    let written = async {
        let mut file = tokio::fs::File::create(&upload).await?;
        let mut stream = body.into_data_stream();
        let mut total = 0u64;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(std::io::Error::other)?;
            total += chunk.len() as u64;
            if total > declared {
                return Err(std::io::Error::other("the upload is longer than it said"));
            }
            file.write_all(&chunk).await?;
        }
        file.flush().await?;
        Ok::<_, std::io::Error>(())
    }
    .await;
    if let Err(e) = written {
        let _ = std::fs::remove_file(&upload);
        return Err(fail(StatusCode::BAD_REQUEST, anyhow::anyhow!("the upload did not arrive: {e}")));
    }
    state.agent.import_world(&id, &upload).await.map(Json).map_err(|e| fail(StatusCode::BAD_REQUEST, e))
}

/// Type one line at a running server's console (`Agent::send_console`). The
/// answer, if the game gives one, shows in the server's log.
async fn send_console(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Json(req): Json<ConsoleReq>,
) -> ApiResult<serde_json::Value> {
    state
        .agent
        .send_console(&id, &req.line)
        .await
        .map(|()| Json(json!({ "instance_id": id, "sent": req.line })))
        .map_err(|e| fail(StatusCode::BAD_REQUEST, e))
}

#[derive(Deserialize)]
struct ChatQuery {
    #[serde(default)]
    since: u64,
}

async fn chat(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Query(q): Query<ChatQuery>,
) -> ApiResult<crate::agent::NodeChat> {
    state.agent.chat(&id, q.since).await.map(Json).map_err(|e| fail(StatusCode::NOT_FOUND, e))
}

#[derive(Deserialize)]
struct ChatSayReq {
    text: String,
}

async fn chat_say(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Json(req): Json<ChatSayReq>,
) -> ApiResult<serde_json::Value> {
    state
        .agent
        .chat_say(&id, &req.text)
        .await
        .map(|()| Json(json!({ "instance_id": id })))
        .map_err(|e| fail(StatusCode::BAD_REQUEST, e))
}

#[derive(Deserialize)]
struct ChatSetReq {
    on: bool,
}

async fn set_chat(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Json(req): Json<ChatSetReq>,
) -> ApiResult<serde_json::Value> {
    state
        .agent
        .set_chat(&id, req.on)
        .await
        .map(|()| Json(json!({ "instance_id": id, "on": req.on })))
        .map_err(|e| fail(StatusCode::NOT_FOUND, e))
}

#[derive(Deserialize)]
struct ChatMuteReq {
    identity: String,
    muted: bool,
}

async fn chat_mute(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Json(req): Json<ChatMuteReq>,
) -> ApiResult<serde_json::Value> {
    state
        .agent
        .chat_mute(&id, &req.identity, req.muted)
        .await
        .map(|()| Json(json!({ "instance_id": id, "identity": req.identity, "muted": req.muted })))
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
/// Start this game's content install, and return immediately.
///
/// The download used to run inside this request. A Sven Co-op install is 2.7 GB
/// and takes tens of minutes — longer than a browser or a proxy will hold a
/// connection — so the request would time out while the work carried on
/// invisibly. Now the work is a task and this reports its state; poll `GET` on
/// the same path.
/// `POST /content/:game` with `{"update": true}` updates an installed game
/// instead (`Agent::update_content`); `remove_old` also deletes the previous
/// copy, now or once the servers using it are gone. `{"verify": true}` checks
/// the files against Valve's (`Agent::verify_content`). No body is an install.
#[derive(Deserialize, Default)]
struct InstallRequest {
    #[serde(default)]
    update: bool,
    #[serde(default)]
    verify: bool,
    #[serde(default)]
    remove_old: bool,
    /// For a game whose copies are picked per server: which one (a
    /// Minecraft build id).
    #[serde(default)]
    version: Option<String>,
}

async fn install_content(
    State(state): State<ApiState>,
    Path(game): Path<String>,
    body: Option<Json<InstallRequest>>,
) -> ApiResult<serde_json::Value> {
    let req = body.map(|Json(b)| b).unwrap_or_default();
    // Refuse what will obviously fail *before* claiming a task, so a mistyped
    // game id is an error rather than a "Failed" state to poll for.
    if !state.agent.packs().await.contains_key(&game) {
        return Err(fail(
            StatusCode::NOT_FOUND,
            format!("no game pack installed for {game:?}"),
        ));
    }
    if req.update && req.verify {
        return Err(fail(StatusCode::BAD_REQUEST, "an update or a check of the files, not both"));
    }
    let started = if req.verify {
        crate::install::spawn_verify(state.installs.clone(), state.agent.clone(), game.clone()).await
    } else if req.update {
        crate::install::spawn_update(state.installs.clone(), state.agent.clone(), game.clone(), req.remove_old)
            .await
    } else {
        if let Some(v) = &req.version {
            crate::instance::validate_id(v).map_err(|e| fail(StatusCode::BAD_REQUEST, e))?;
        }
        crate::install::spawn_version(state.installs.clone(), state.agent.clone(), game.clone(), req.version.clone()).await
    };
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

/// The Installed page's one read: every installed copy of every game's files
/// (`Agent::installed`), and every download running now, so the page polls
/// one route rather than one per game.
async fn installed(State(state): State<ApiState>) -> ApiResult<serde_json::Value> {
    let copies = state.agent
        .installed()
        .await
        .map_err(|e| fail(StatusCode::INTERNAL_SERVER_ERROR, e))?;
    let mut downloads = Vec::new();
    for game in state.installs.running().await {
        let status = state.installs.state(&game).await;
        downloads.push(json!({ "game": game, "status": status }));
    }
    Ok(Json(json!({ "copies": copies, "downloads": downloads })))
}

/// Delete one installed copy. Refused with 409 and the servers using it while
/// any container mounts it, and while the game is downloading.
async fn delete_content(
    State(state): State<ApiState>,
    Path((game, version)): Path<(String, String)>,
) -> Response {
    use crate::agent::DeleteContentError;
    let held = state
        .installs
        .exclusive(&game, state.agent.delete_content(&game, &version))
        .await;
    match held {
        Err(crate::install::NotRunning) => fail(
            StatusCode::CONFLICT,
            format!("{game} is downloading; cancel the download or wait for it to finish"),
        )
        .into_response(),
        Ok(Ok(bytes)) => Json(json!({ "game": game, "version": version, "bytes": bytes })).into_response(),
        Ok(Err(e)) => {
            let status = match &e {
                DeleteContentError::BadId(_) => StatusCode::BAD_REQUEST,
                DeleteContentError::NotInstalled => StatusCode::NOT_FOUND,
                DeleteContentError::InUse(_) => StatusCode::CONFLICT,
                DeleteContentError::Other(_) => StatusCode::INTERNAL_SERVER_ERROR,
            };
            let servers = match &e {
                DeleteContentError::InUse(ids) => ids.clone(),
                _ => Vec::new(),
            };
            (status, Json(json!({ "error": e.to_string(), "servers": servers }))).into_response()
        }
    }
}

/// Cancel this game's download. The task stops its container, discards what
/// it fetched and ends as `Failed { error: "cancelled" }`; poll `GET` on the
/// same path to see it land.
async fn cancel_install(
    State(state): State<ApiState>,
    Path(game): Path<String>,
) -> ApiResult<serde_json::Value> {
    let container = state.installs.cancel(&game).await.map_err(|_| {
        fail(StatusCode::NOT_FOUND, format!("{game} is not downloading"))
    })?;
    if let Some(name) = container {
        state.agent
            .docker()
            .remove_task(&name)
            .await
            .map_err(|e| fail(StatusCode::INTERNAL_SERVER_ERROR, e))?;
    }
    Ok(Json(json!({ "game": game, "cancelled": true })))
}

/// Make an installed copy the one new servers use, such as the repaired copy
/// a check of the files kept (`Agent::use_copy`). Held to the game's slot, so
/// it cannot land in the middle of a download of the same game.
async fn use_copy(
    State(state): State<ApiState>,
    Path((game, version)): Path<(String, String)>,
) -> ApiResult<serde_json::Value> {
    if !state.agent.packs().await.contains_key(&game) {
        return Err(fail(StatusCode::NOT_FOUND, format!("no game pack installed for {game:?}")));
    }
    let agent = state.agent.clone();
    let outcome = state
        .installs
        .exclusive(&game, async { agent.use_copy(&game, &version).await })
        .await
        .map_err(|_| fail(StatusCode::CONFLICT, format!("{game} is downloading; wait for it to finish")))?;
    outcome.map_err(|e| fail(StatusCode::CONFLICT, e))?;
    Ok(Json(json!({ "game": game, "active": version })))
}

// ---- Update checks and the disk reserve (`updates.rs`, `ROADMAP.md` §12) ----
//
// Their own resource rather than more routes under `/content/:game`: a static
// segment there (`/content/:game/check`) would shadow a version of that name
// for `DELETE /content/:game/:version`, as `install` would have.

/// Settings, the reserve as it stands on this disk, and every game's notice.
async fn updates(State(state): State<ApiState>) -> Json<serde_json::Value> {
    let (storage, reserve) = match state.agent.reserve() {
        Some((s, r)) => (Some(s), Some(r)),
        None => (None, None),
    };
    Json(json!({
        "settings": state.agent.update_settings(),
        "reserve_bytes": reserve,
        "default_reserve_bytes": storage.as_ref().map(|s| crate::updates::default_reserve(s.total_bytes)),
        "storage": storage,
        "games": state.agent.update_notices().await,
        "moves": state.agent.moves(),
    }))
}

async fn set_update_settings(
    State(state): State<ApiState>,
    Json(settings): Json<crate::updates::Settings>,
) -> ApiResult<serde_json::Value> {
    state.agent.set_update_settings(settings).map_err(|e| fail(StatusCode::BAD_REQUEST, e))?;
    Ok(updates(State(state)).await)
}

/// Check one game now. Holds the request for the check (about 25 s against
/// Steam, measured 2026-10-13), and answers with the game's notice.
async fn check_update(State(state): State<ApiState>, Path(game): Path<String>) -> ApiResult<serde_json::Value> {
    if !state.agent.packs().await.contains_key(&game) {
        return Err(fail(StatusCode::NOT_FOUND, format!("no game pack installed for {game:?}")));
    }
    let notice = state.agent.check_for_update(&game).await.map_err(|e| fail(StatusCode::CONFLICT, e))?;
    // A game set to download a found build starts now, not on the next tick.
    state.agent.start_automatic_downloads().await;
    Ok(Json(json!(notice)))
}

/// A game's choices (`updates::PolicyPatch`; what is absent is kept): what a
/// found build does (§12.3), `{"on_new_build": "download"}` or `"notify"`,
/// and when its servers move onto the new copy (§12.4), `move_servers`
/// `never`, `when_empty` or `window`, with `window_start_utc`,
/// `window_end_utc` and `force_in_window`. Answers like `GET /updates`.
async fn set_update_policy(
    State(state): State<ApiState>,
    Path(game): Path<String>,
    Json(policy): Json<crate::updates::PolicyPatch>,
) -> ApiResult<serde_json::Value> {
    if !state.agent.packs().await.contains_key(&game) {
        return Err(fail(StatusCode::NOT_FOUND, format!("no game pack installed for {game:?}")));
    }
    state.agent.set_update_policy(&game, policy).await.map_err(|e| fail(StatusCode::CONFLICT, e))?;
    state.agent.start_automatic_downloads().await;
    Ok(updates(State(state)).await)
}

/// Move a running server onto the copy new servers use now, whoever is on
/// it (`Agent::move_instance`). Holds the request for the move and the
/// watch after it, about half a minute.
async fn move_instance(State(state): State<ApiState>, Path(id): Path<String>) -> ApiResult<serde_json::Value> {
    match state.agent.move_instance(&id, false).await {
        Ok(crate::agent::MoveOutcome::Moved { from, to }) => {
            Ok(Json(json!({ "instance_id": id, "state": "moved", "from": from, "to": to })))
        }
        Ok(crate::agent::MoveOutcome::RolledBack { from, to, why }) => {
            Ok(Json(json!({ "instance_id": id, "state": "rolled_back", "from": from, "to": to, "why": why })))
        }
        Err(e) => Err(fail(StatusCode::CONFLICT, e)),
    }
}

/// `{"hours": 24}` hides a notice for a day; `{"skip_build": true}` hides it
/// while the build it names is the newest.
#[derive(Deserialize)]
struct SnoozeRequest {
    #[serde(default)]
    hours: Option<u32>,
    #[serde(default)]
    skip_build: bool,
}

async fn snooze_update(
    State(state): State<ApiState>,
    Path(game): Path<String>,
    Json(req): Json<SnoozeRequest>,
) -> ApiResult<serde_json::Value> {
    if !state.agent.packs().await.contains_key(&game) {
        return Err(fail(StatusCode::NOT_FOUND, format!("no game pack installed for {game:?}")));
    }
    if req.hours.is_none() && !req.skip_build {
        return Err(fail(StatusCode::BAD_REQUEST, "say how long (hours) or skip_build"));
    }
    if req.hours.is_some_and(|h| h == 0 || h > crate::updates::MAX_CHECK_EVERY_HOURS) {
        return Err(fail(StatusCode::BAD_REQUEST, "a snooze is between one hour and 30 days"));
    }
    state.agent.snooze_update(&game, req.hours, req.skip_build).map_err(|e| fail(StatusCode::CONFLICT, e))?;
    Ok(updates(State(state)).await)
}

async fn unsnooze_update(State(state): State<ApiState>, Path(game): Path<String>) -> ApiResult<serde_json::Value> {
    if !state.agent.packs().await.contains_key(&game) {
        return Err(fail(StatusCode::NOT_FOUND, format!("no game pack installed for {game:?}")));
    }
    state.agent.snooze_update(&game, None, false).map_err(|e| fail(StatusCode::INTERNAL_SERVER_ERROR, e))?;
    Ok(updates(State(state)).await)
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
            "this agent has no [uplink], so there are no uplink interfaces to configure here. \
             That is a different thing from hosting games on the mesh: [uplink] is the \
             control destination an index drives this node through, and [mesh] is what \
             announces the games this node runs. Games can be on the mesh with no uplink \
             at all — see the Servers tab for their destinations.",
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
