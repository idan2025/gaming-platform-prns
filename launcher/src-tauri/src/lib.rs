//! Tauri shell. Every command here forwards to `launcher-core` and does nothing
//! else, on purpose: the logic worth testing lives in a crate that needs no
//! webview, no display server and no platform toolchain to run its tests.
//!
//! Stack decision is `PLAN.md` §9 — Tauri v2, kept because the core is Rust and
//! Tauri links it as a plain path dependency with no IPC or serialization
//! boundary between the UI and the sessions it supervises.
//!
//! `tauri.conf.json` sets `webviewInstallMode: offlineInstaller` for the reason
//! §9 gives: Tauri needs WebView2 on Windows, and a genuinely offline mesh
//! machine that lacks it could otherwise neither start the launcher nor
//! download the runtime — precisely the situation this platform advertises.

use std::path::PathBuf;

use launcher_core::lan::{LanHelperView, RoomView};
use launcher_core::{
    error_text as fmt_err, BrowseOpts, BrowseQueryInput, BrowseStatus, GameLocationView,
    GameSummary, JoinResult, Launcher, PlayResult, ServerDetailsView, ServerRow,
};
use tauri::Manager;

struct AppState {
    launcher: Launcher,
}

#[tauri::command]
async fn browse_status(state: tauri::State<'_, AppState>) -> Result<BrowseStatus, String> {
    Ok(state.launcher.browse_status().await)
}

#[tauri::command]
async fn start_browse(state: tauri::State<'_, AppState>, opts: BrowseOpts) -> Result<(), String> {
    state.launcher.start_browse(opts).await.map_err(fmt_err)
}

#[tauri::command]
async fn stop_browse(state: tauri::State<'_, AppState>) -> Result<(), String> {
    state.launcher.stop_browse().await.map_err(fmt_err)
}

#[tauri::command]
async fn list_servers(
    state: tauri::State<'_, AppState>,
    query: BrowseQueryInput,
) -> Result<Vec<ServerRow>, String> {
    state.launcher.list_servers(query).await.map_err(fmt_err)
}

#[tauri::command]
async fn list_games(state: tauri::State<'_, AppState>) -> Result<Vec<GameSummary>, String> {
    Ok(state.launcher.list_games())
}

/// Which build the player is running, for the corner chip. The core answers, so
/// the number is the platform's rather than this shell's hand-bumped one.
#[tauri::command]
async fn app_version(state: tauri::State<'_, AppState>) -> Result<String, String> {
    Ok(state.launcher.version().to_string())
}

/// Never returns `Err` for an unreachable server: a probe that did not answer is
/// a *state* the detail pane renders, not an error banner. Mesh routing is
/// asymmetric and an allowlisted server refuses probes on purpose, so "did not
/// answer" must never be presented as "offline".
#[tauri::command]
async fn server_details(
    state: tauri::State<'_, AppState>,
    destination_hash: String,
) -> Result<ServerDetailsView, String> {
    Ok(state.launcher.server_details(&destination_hash).await)
}

/// `listen_port` is the local port the bridge binds for the game to connect to,
/// and it is remembered per game. It exists because the pack's default is the
/// port the game's *own* dedicated server uses, so a machine already running
/// one owns it and the join fails on a number the player never picked.
#[tauri::command]
async fn join_server(
    state: tauri::State<'_, AppState>,
    destination_hash: String,
    game_id: Option<String>,
    listen_port: Option<u16>,
) -> Result<JoinResult, String> {
    state
        .launcher
        .join_server(&destination_hash, game_id.as_deref(), listen_port)
        .await
        .map_err(fmt_err)
}

/// The local port a join for this game would bind right now — what the player
/// chose, else the pack's default. The UI prefills its field with it.
#[tauri::command]
async fn listen_port(
    state: tauri::State<'_, AppState>,
    game_id: String,
) -> Result<Option<u16>, String> {
    Ok(state.launcher.listen_port_for(&game_id).await)
}

/// Forget a chosen local port and go back to the pack's default.
#[tauri::command]
async fn clear_listen_port(
    state: tauri::State<'_, AppState>,
    game_id: String,
) -> Result<(), String> {
    state.launcher.set_listen_port(&game_id, None).await.map_err(fmt_err)
}

/// The servers this launcher remembers, so a person can see, refresh or forget
/// them. Remembering exists because the mesh floods an announce once and then
/// suppresses the repeats: without it, a launcher started after a server was
/// already running never hears about it.
#[tauri::command]
async fn known_servers(
    state: tauri::State<'_, AppState>,
) -> Result<Vec<launcher_core::KnownServerView>, String> {
    Ok(state.launcher.known_servers().await)
}

/// Ask the mesh where every remembered server is. The answers arrive as
/// ordinary announces, so rows fill in through the normal path.
#[tauri::command]
async fn refresh_known_servers(state: tauri::State<'_, AppState>) -> Result<usize, String> {
    state.launcher.refresh_known_servers().await.map_err(fmt_err)
}

/// Forget one remembered server, or all of them when `destination_hash` is
/// absent.
#[tauri::command]
async fn forget_server(
    state: tauri::State<'_, AppState>,
    destination_hash: Option<String>,
) -> Result<(), String> {
    state.launcher.forget_server(destination_hash.as_deref()).await.map_err(fmt_err)
}

/// Indexes this launcher is willing to ask.
///
/// An index is a cache of the mesh, never the source of truth (`DESIGN.md`
/// §0): the list is empty by default, browsing works with it empty, and adding
/// one is a player deciding to trust somebody's list rather than the platform
/// acquiring a dependency.
#[tauri::command]
async fn indexes(state: tauri::State<'_, AppState>) -> Result<Vec<String>, String> {
    Ok(state.launcher.indexes().await)
}

#[tauri::command]
async fn add_index(
    state: tauri::State<'_, AppState>,
    destination_hash: String,
) -> Result<(), String> {
    state.launcher.add_index(&destination_hash).await.map_err(fmt_err)
}

#[tauri::command]
async fn remove_index(
    state: tauri::State<'_, AppState>,
    destination_hash: String,
) -> Result<(), String> {
    state.launcher.remove_index(&destination_hash).await.map_err(fmt_err)
}

#[tauri::command]
async fn leave(state: tauri::State<'_, AppState>) -> Result<(), String> {
    state.launcher.leave().await.map_err(fmt_err)
}

/// Start the game for the current join — the Play button (`PLAN.md` §13.3). All
/// the deciding and spawning is in `launcher-core`; this only forwards.
#[tauri::command]
async fn play_server(state: tauri::State<'_, AppState>) -> Result<PlayResult, String> {
    state.launcher.play().await.map_err(fmt_err)
}

/// What the launcher knows about locating one game, so the UI can choose between
/// a Play button and a "locate your game" prompt.
#[tauri::command]
async fn game_location(
    state: tauri::State<'_, AppState>,
    game_id: String,
) -> Result<GameLocationView, String> {
    Ok(state.launcher.game_location(&game_id).await)
}

/// Remember the player's own path to a game's executable. Refused if it is not a
/// file, so the UI can complain at the moment of picking.
#[tauri::command]
async fn set_game_path(
    state: tauri::State<'_, AppState>,
    game_id: String,
    path: String,
) -> Result<(), String> {
    state
        .launcher
        .set_game_path(&game_id, std::path::Path::new(&path))
        .await
        .map_err(fmt_err)
}

#[tauri::command]
async fn clear_game_path(state: tauri::State<'_, AppState>, game_id: String) -> Result<(), String> {
    state.launcher.clear_game_path(&game_id).await.map_err(fmt_err)
}

#[tauri::command]
async fn list_interfaces(
    state: tauri::State<'_, AppState>,
) -> Result<Vec<launcher_core::InterfaceEntry>, String> {
    Ok(state.launcher.interfaces().await)
}

#[tauri::command]
async fn add_interface(
    state: tauri::State<'_, AppState>,
    kind: String,
    addr: Option<String>,
) -> Result<(), String> {
    let iface = match kind.as_str() {
        "auto" => launcher_core::settings::LauncherInterface::Auto,
        "tcp" => launcher_core::settings::LauncherInterface::Tcp {
            addr: addr.unwrap_or_default(),
        },
        other => return Err(format!("unknown interface kind {other:?}")),
    };
    state.launcher.add_interface(iface).await.map_err(fmt_err)
}

#[tauri::command]
async fn remove_interface(state: tauri::State<'_, AppState>, id: String) -> Result<bool, String> {
    state.launcher.remove_interface(&id).await.map_err(fmt_err)
}

/// The saved interfaces as browse options, so the UI can start browsing with
/// what the player configured instead of asking again.
#[tauri::command]
async fn saved_browse_opts(
    state: tauri::State<'_, AppState>,
) -> Result<launcher_core::BrowseOpts, String> {
    Ok(state.launcher.saved_browse_opts().await)
}

#[tauri::command]
async fn player_name(state: tauri::State<'_, AppState>) -> Result<Option<String>, String> {
    Ok(state.launcher.player_name().await)
}

#[tauri::command]
async fn set_player_name(state: tauri::State<'_, AppState>, name: String) -> Result<(), String> {
    state.launcher.set_player_name(&name).await.map_err(fmt_err)
}

/// Where game packs live: next to the executable in a shipped build, and at the
/// repo's `packs/` when running from a checkout.
// ---- Mode 3 LAN rooms (`PLAN.md` §14, step 5). All of it is in
// `launcher-core/src/lan.rs`; these only forward. ----

#[tauri::command]
async fn lan_helper(state: tauri::State<'_, AppState>) -> Result<LanHelperView, String> {
    Ok(state.launcher.lan_helper().await)
}

#[tauri::command]
async fn grant_lan_helper(state: tauri::State<'_, AppState>) -> Result<LanHelperView, String> {
    state.launcher.grant_lan_helper().await.map_err(fmt_err)
}

#[tauri::command]
async fn revoke_lan_helper(state: tauri::State<'_, AppState>) -> Result<LanHelperView, String> {
    state.launcher.revoke_lan_helper().await.map_err(fmt_err)
}

#[tauri::command]
async fn host_room(
    state: tauri::State<'_, AppState>,
    game_id: String,
    name: Option<String>,
) -> Result<RoomView, String> {
    state.launcher.host_room(&game_id, name).await.map_err(fmt_err)
}

#[tauri::command]
async fn join_room(
    state: tauri::State<'_, AppState>,
    destination_hash: String,
    game_id: String,
) -> Result<RoomView, String> {
    state.launcher.join_room(&destination_hash, &game_id).await.map_err(fmt_err)
}

#[tauri::command]
async fn leave_room(state: tauri::State<'_, AppState>) -> Result<(), String> {
    state.launcher.leave_room().await.map_err(fmt_err)
}

#[tauri::command]
async fn room_status(state: tauri::State<'_, AppState>) -> Result<RoomView, String> {
    Ok(state.launcher.room_status().await)
}

fn pack_dir(app: &tauri::AppHandle) -> PathBuf {
    // Beside the executable first: that is the portable layout, and on Linux
    // `resource_dir` never points there outside a cargo `target/` — it resolves
    // to `../lib/<name>` or `/usr/lib/<name>`, so a portable launcher would
    // otherwise find no packs and offer only the built-in game. An installed
    // launcher has no `packs/` beside its binary, so it falls through.
    if let Some(dir) = std::env::current_exe().ok().and_then(|e| e.parent().map(|d| d.join("packs"))) {
        if dir.is_dir() {
            return dir;
        }
    }
    if let Ok(dir) = app.path().resource_dir() {
        let candidate = dir.join("packs");
        if candidate.is_dir() {
            return candidate;
        }
    }
    PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../../packs"))
}

pub fn run() {
    // Portable mode (`launcher-core/src/portable.rs`): every per-user
    // directory is pointed inside the portable folder *before* anything else
    // runs — the web view, GTK, fonts and shaders all read those once, early.
    let portable = launcher_core::portable::detect();
    if let Some(p) = &portable {
        launcher_core::portable::confine(p);
    }

    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "launcher=info,launcher_core=info,game_bridge=info".into()),
        )
        .try_init();

    tauri::Builder::default()
        .setup(move |app| {
            let packs = pack_dir(app.handle());
            let launcher = Launcher::from_pack_dir(&packs).with_portable(portable.is_some());
            tracing::info!(
                packs = %packs.display(),
                games = launcher.list_games().len(),
                portable = ?portable.as_ref().map(|p| p.data_dir.display().to_string()),
                "launcher starting"
            );
            app.manage(AppState { launcher });
            // The window is made here rather than from the config alone
            // (`"create": false`), so a portable run can give the web view a
            // storage folder inside the portable one.
            let config = app
                .config()
                .app
                .windows
                .first()
                .cloned()
                .ok_or("tauri.conf.json declares no window")?;
            let mut window = tauri::WebviewWindowBuilder::from_config(app.handle(), &config)?;
            if let Some(p) = &portable {
                window = window.data_directory(launcher_core::portable::webview_dir(p));
            }
            window.build()?;
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            browse_status,
            start_browse,
            stop_browse,
            list_servers,
            list_games,
            app_version,
            server_details,
            join_server,
            listen_port,
            clear_listen_port,
            known_servers,
            refresh_known_servers,
            forget_server,
            indexes,
            add_index,
            remove_index,
            leave,
            play_server,
            game_location,
            set_game_path,
            clear_game_path,
            player_name,
            list_interfaces,
            add_interface,
            remove_interface,
            saved_browse_opts,
            set_player_name,
            lan_helper,
            grant_lan_helper,
            revoke_lan_helper,
            host_room,
            join_room,
            leave_room,
            room_status,
        ])
        .run(tauri::generate_context!())
        .expect("error while running the launcher");
}
