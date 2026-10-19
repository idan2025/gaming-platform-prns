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

use launcher_core::invite::Invite;
use launcher_core::lan::{LanHelperView, RoomCheckView, RoomView};
use launcher_core::update::UpdateView;
#[cfg(desktop)]
use launcher_core::update::{install_mode, InstallFacts, InstallMode};
use launcher_core::{
    error_text as fmt_err, BrowseOpts, BrowseQueryInput, BrowseStatus, GameLocationView,
    GameSummary, JoinResult, Launcher, PathTraceView, PlayResult, ServerDetailsView, ServerRow,
};
use tauri::{Emitter, Manager};
#[cfg(desktop)]
use tauri_plugin_updater::UpdaterExt;

struct AppState {
    launcher: Launcher,
    /// An invite this launcher was opened with, or was handed while running,
    /// until the page takes it (`take_invite`).
    pending_invite: std::sync::Mutex<Option<Invite>>,
    /// The update the last check found, kept for `install_update`.
    #[cfg(desktop)]
    update: tokio::sync::Mutex<Option<tauri_plugin_updater::Update>>,
    /// The APK the last Android check found, kept for `download_update`.
    #[cfg(target_os = "android")]
    android_update: std::sync::Mutex<Option<launcher_core::update::android::Release>>,
    /// Where `download_update` put the verified APK, for `install_update_android`.
    #[cfg(target_os = "android")]
    android_apk_path: std::sync::Mutex<Option<PathBuf>>,
    /// Whether the tray icon came up. Without one (a Linux desktop with no
    /// tray, say) closing the window quits as it always did: a hidden window
    /// with no icon to bring it back would be a launcher nobody can reach.
    tray: std::sync::atomic::AtomicBool,
    /// The "still running in the tray" note is shown once a run.
    told_tray: std::sync::atomic::AtomicBool,
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
/// Keep the phone's session alive while a server or room is open: the foreground
/// service runs exactly while `keeps_running` says something is joined or
/// hosted. A failed start is logged, not returned: the session itself is fine,
/// it just may be killed in the background.
async fn sync_keepalive(app: &tauri::AppHandle, state: &AppState) {
    let on = state.launcher.keeps_running().await.is_some();
    if let Err(e) = tauri_plugin_keepalive::set(app, on) {
        tracing::warn!(error = %e, "background service");
    }
}

#[tauri::command]
async fn join_server(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    destination_hash: String,
    game_id: Option<String>,
    listen_port: Option<u16>,
) -> Result<JoinResult, String> {
    let joined = state
        .launcher
        .join_server(&destination_hash, game_id.as_deref(), listen_port)
        .await
        .map_err(fmt_err);
    sync_keepalive(&app, &state).await;
    joined
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

/// Every starred server or room, online ones first.
#[tauri::command]
async fn favorites(state: tauri::State<'_, AppState>) -> Result<Vec<launcher_core::FavoriteView>, String> {
    Ok(state.launcher.favorites().await)
}

/// Star or unstar a server or room.
#[tauri::command]
async fn set_favorite(
    state: tauri::State<'_, AppState>,
    destination_hash: String,
    on: bool,
) -> Result<(), String> {
    state.launcher.set_favorite(&destination_hash, on).await.map_err(fmt_err)
}

/// Ask the mesh for a path to one destination now, and say what came back.
#[tauri::command]
async fn trace_path(
    state: tauri::State<'_, AppState>,
    destination_hash: String,
) -> Result<PathTraceView, String> {
    state.launcher.trace_path(&destination_hash).await.map_err(fmt_err)
}

/// Announce the room this launcher hosts now. `false` when it hosts none.
#[tauri::command]
async fn announce_room(state: tauri::State<'_, AppState>) -> Result<bool, String> {
    Ok(state.launcher.announce_room().await)
}

/// Look for a newer launcher (`launcher-core/src/update.rs`). A failed check
/// is an error here, for the "Check now" button; the page's own check at
/// start swallows it, because offline is a supported way to run.
#[tauri::command]
async fn check_update(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
) -> Result<UpdateView, String> {
    #[cfg(target_os = "android")]
    return check_update_android(app, state).await;
    #[cfg(all(mobile, not(target_os = "android")))]
    {
        let _ = (app, state);
        return Err("this build updates from the release page".into());
    }
    #[cfg(desktop)]
    check_update_desktop(app, state).await
}

/// The Android build's check (`launcher_core::update::android`): the newest
/// Android release with a higher `versionCode` than this APK's. It is never
/// installed from here; the page offers its APK to download and open.
#[cfg(target_os = "android")]
async fn check_update_android(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
) -> Result<UpdateView, String> {
    use launcher_core::update::{android, InstallMode};
    let code: u32 = env!("LANTHORN_ANDROID_VERSION_CODE").parse().map_err(|_| "no versionCode in this build")?;
    let current = format!("{} (build {})", app.package_info().version, code % 1000);
    let auto = state.launcher.auto_check_updates().await;
    let previews = state.launcher.android_previews_on().await;
    let found = android::check(code, previews).await.map_err(|e| format!("could not check for updates: {e}"))?;
    let mut view = UpdateView::none(&current, InstallMode::LinkOnly, auto);
    view.page = ANDROID_RELEASES_PAGE.to_string();
    if let Some(release) = &found {
        view.available = Some(release.label());
        view.notes = release.notes.clone();
    }
    *state.android_update.lock().unwrap() = found;
    // A new check may find a different build; the APK kept for install is
    // only the one that was downloaded for the build now offered.
    *state.android_apk_path.lock().unwrap() = None;
    Ok(view)
}

/// Every release, because the Android builds are prereleases and
/// `/releases/latest` never shows one.
#[cfg(target_os = "android")]
const ANDROID_RELEASES_PAGE: &str = "https://github.com/idan2025/gaming-platform-prns/releases";

#[cfg(desktop)]
async fn check_update_desktop(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
) -> Result<UpdateView, String> {
    let current = app.package_info().version.to_string();
    let mode = install_mode(InstallFacts::here(state.launcher.is_portable()));
    let auto = state.launcher.auto_check_updates().await;
    let found = app
        .updater()
        .map_err(|e| e.to_string())?
        .check()
        .await
        .map_err(|e| format!("could not check for updates: {e}"))?;
    let mut view = UpdateView::none(&current, mode, auto);
    if let Some(update) = &found {
        view.available = Some(update.version.clone());
        view.notes = update.body.clone();
    }
    *state.update.lock().await = found;
    Ok(view)
}

/// Download the update the last check found, verify its signature, install
/// it and restart. Only where this copy can replace itself.
#[tauri::command]
async fn install_update(app: tauri::AppHandle, state: tauri::State<'_, AppState>) -> Result<(), String> {
    #[cfg(mobile)]
    {
        let _ = (app, state);
        return Err("this build updates from the release page".into());
    }
    #[cfg(desktop)]
    install_update_desktop(app, state).await
}

#[cfg(desktop)]
async fn install_update_desktop(app: tauri::AppHandle, state: tauri::State<'_, AppState>) -> Result<(), String> {
    if install_mode(InstallFacts::here(state.launcher.is_portable())) != InstallMode::InPlace {
        return Err("this copy of Lanthorn updates from the release page, not by itself".into());
    }
    let Some(update) = state.update.lock().await.take() else {
        return Err("no update to install; check again".into());
    };
    update
        .download_and_install(|_, _| {}, || {})
        .await
        .map_err(|e| format!("the update did not install: {e}"))?;
    app.restart();
}

#[tauri::command]
async fn set_auto_update(state: tauri::State<'_, AppState>, on: bool) -> Result<(), String> {
    state.launcher.set_auto_check_updates(on).await.map_err(fmt_err)
}

/// A desktop notification (`ROADMAP.md` item 5), unless the player turned
/// them off or is looking at the window already, where the page's own toast
/// says it.
#[tauri::command]
async fn notify(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    title: String,
    body: String,
) -> Result<bool, String> {
    if !state.launcher.notifications_on().await {
        return Ok(false);
    }
    if let Some(w) = app.get_webview_window("main") {
        if w.is_visible().unwrap_or(false) && w.is_focused().unwrap_or(false) {
            return Ok(false);
        }
    }
    // A fullscreen game has the screen: no banner over it.
    if launcher_core::desktop::user_is_busy() {
        return Ok(false);
    }
    Ok(send_notification(&app, &title, &body))
}

fn send_notification(app: &tauri::AppHandle, title: &str, body: &str) -> bool {
    use tauri_plugin_notification::NotificationExt;
    // Plain text, short: it comes from server names other people chose.
    let clip = |s: &str, n: usize| s.chars().filter(|c| !c.is_control()).take(n).collect::<String>();
    app.notification().builder().title(clip(title, 80)).body(clip(body, 200)).show().is_ok()
}

#[tauri::command]
async fn host_server(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    request: launcher_core::host::HostRequest,
) -> Result<launcher_core::host::HostView, String> {
    let hosted = state.launcher.host_server(request).await.map_err(fmt_err);
    sync_keepalive(&app, &state).await;
    hosted
}

#[tauri::command]
async fn stop_hosting(app: tauri::AppHandle, state: tauri::State<'_, AppState>) -> Result<(), String> {
    state.launcher.stop_hosting().await;
    sync_keepalive(&app, &state).await;
    Ok(())
}

#[tauri::command]
async fn hosting(state: tauri::State<'_, AppState>) -> Result<launcher_core::host::HostView, String> {
    Ok(state.launcher.hosting().await)
}

#[tauri::command]
async fn first_run(state: tauri::State<'_, AppState>) -> Result<bool, String> {
    Ok(state.launcher.first_run().await)
}

#[tauri::command]
async fn finish_first_run(state: tauri::State<'_, AppState>) -> Result<(), String> {
    state.launcher.finish_first_run().await.map_err(fmt_err)
}

#[tauri::command]
async fn notifications_on(state: tauri::State<'_, AppState>) -> Result<bool, String> {
    Ok(state.launcher.notifications_on().await)
}

#[tauri::command]
async fn set_notifications_on(state: tauri::State<'_, AppState>, on: bool) -> Result<(), String> {
    state.launcher.set_notifications_on(on).await.map_err(fmt_err)
}

#[cfg(desktop)]
/// Bring the window back from the tray, or from behind other windows.
fn show_main(app: &tauri::AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.show();
        let _ = w.unminimize();
        let _ = w.set_focus();
    }
}

/// The tray icon: click to show the window, and a menu with Show and Quit.
/// Quit is a real quit, exactly like closing the window used to be, so a
/// room ends with it (`CLAUDE.md`, Mode 3: nothing a room creates outlives
/// the launcher).
#[cfg(desktop)]
fn build_tray(app: &tauri::AppHandle) -> tauri::Result<()> {
    use tauri::menu::{Menu, MenuItem};
    use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
    let show = MenuItem::with_id(app, "show", "Show Lanthorn", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&show, &quit])?;
    let mut tray = TrayIconBuilder::with_id("main")
        .tooltip("Lanthorn")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id().as_ref() {
            "show" => show_main(app),
            "quit" => app.exit(0),
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } = event {
                show_main(tray.app_handle());
            }
        });
    if let Some(icon) = app.default_window_icon() {
        tray = tray.icon(icon.clone());
    }
    tray.build(app)?;
    Ok(())
}

#[tauri::command]
async fn auto_update(state: tauri::State<'_, AppState>) -> Result<bool, String> {
    Ok(state.launcher.auto_check_updates().await)
}

/// The invite this launcher was opened with, once.
#[tauri::command]
fn take_invite(state: tauri::State<'_, AppState>) -> Option<Invite> {
    state.pending_invite.lock().ok().and_then(|mut p| p.take())
}

/// Read an invite a player pasted: a link, or a bare address.
#[tauri::command]
fn parse_invite(text: String) -> Result<Invite, String> {
    launcher_core::invite::parse(&text)
}

/// The invite link for a server or room.
#[tauri::command]
fn invite_link(
    destination_hash: String,
    game_id: Option<String>,
    name: Option<String>,
    room: bool,
) -> String {
    launcher_core::invite::web_link(&destination_hash, game_id.as_deref(), name.as_deref(), room)
}

/// Open the release page in the player's browser. The one URL this opens:
/// the page never hands the shell a URL to visit.
#[tauri::command]
fn open_release_page(app: tauri::AppHandle) -> Result<(), String> {
    use tauri_plugin_opener::OpenerExt;
    #[cfg(target_os = "android")]
    let url = ANDROID_RELEASES_PAGE;
    #[cfg(not(target_os = "android"))]
    let url = launcher_core::update::RELEASES_PAGE;
    app.opener().open_url(url, None::<&str>).map_err(|e| e.to_string())
}

/// Android: download the APK the last check found into the app's cache, with
/// progress sent to the page as `update-progress` events. The file is held to
/// the SHA-256 GitHub reported for it and to its size, and only then kept under
/// its final name; a mismatch is deleted and nothing is installed. The
/// download is from `update::android::DOWNLOAD_PREFIX` only, checked when the
/// release was picked.
#[tauri::command]
async fn download_update(app: tauri::AppHandle, state: tauri::State<'_, AppState>) -> Result<(), String> {
    #[cfg(target_os = "android")]
    {
        use std::io::Write;
        use tauri::Emitter;
        let release = state
            .android_update
            .lock()
            .unwrap()
            .clone()
            .ok_or("no update is known; check for updates first")?;
        let dir = app.path().app_cache_dir().map_err(|e| e.to_string())?.join("updates");
        std::fs::create_dir_all(&dir).map_err(|e| format!("could not make {}: {e}", dir.display()))?;
        let part = dir.join("lanthorn-update.apk.part");
        let apk = dir.join("lanthorn-update.apk");
        let _ = std::fs::remove_file(&part);

        let client = game_bridge::repo::http_client(std::time::Duration::from_secs(900)).map_err(|e| e.to_string())?;
        let mut response = client
            .get(&release.apk_url)
            .send()
            .await
            .map_err(|e| format!("could not reach the download: {e}"))?;
        if !response.status().is_success() {
            return Err(format!("the download answered {}", response.status()));
        }
        let total = if release.size > 0 { Some(release.size) } else { None };
        let mut file = std::fs::File::create(&part).map_err(|e| format!("could not write {}: {e}", part.display()))?;
        let mut hasher = <sha2::Sha256 as sha2::Digest>::new();
        let mut received: u64 = 0;
        let mut last_sent = std::time::Instant::now();
        let _ = app.emit("update-progress", serde_json::json!({ "received": 0, "total": total }));
        loop {
            let chunk = match response.chunk().await {
                Ok(Some(chunk)) => chunk,
                Ok(None) => break,
                Err(e) => {
                    let _ = std::fs::remove_file(&part);
                    return Err(format!("the download stopped: {e}"));
                }
            };
            sha2::Digest::update(&mut hasher, &chunk);
            file.write_all(&chunk).map_err(|e| format!("could not write the download: {e}"))?;
            received += chunk.len() as u64;
            if last_sent.elapsed() >= std::time::Duration::from_millis(200) {
                last_sent = std::time::Instant::now();
                let _ = app.emit("update-progress", serde_json::json!({ "received": received, "total": total }));
            }
        }
        file.sync_all().map_err(|e| format!("could not finish the download: {e}"))?;
        drop(file);
        let _ = app.emit("update-progress", serde_json::json!({ "received": received, "total": total }));

        let digest = sha2::Digest::finalize(hasher);
        let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
        let size_ok = release.size == 0 || received == release.size;
        let digest_ok = release.sha256.as_deref().is_none_or(|expected| expected == hex);
        if !size_ok || !digest_ok {
            let _ = std::fs::remove_file(&part);
            return Err(
                "the download does not match the checksum GitHub published for it; nothing was installed".into(),
            );
        }
        std::fs::rename(&part, &apk).map_err(|e| format!("could not keep the download: {e}"))?;
        *state.android_apk_path.lock().unwrap() = Some(apk);
        return Ok(());
    }
    #[cfg(not(target_os = "android"))]
    {
        let _ = (app, state);
        Err("this build updates from the release page".into())
    }
}

/// Android: hand the downloaded APK to the system installer, which asks the
/// player to confirm and refuses it unless the same key signed it.
#[tauri::command]
fn install_update_android(state: tauri::State<'_, AppState>) -> Result<(), String> {
    #[cfg(target_os = "android")]
    {
        let path = state
            .android_apk_path
            .lock()
            .unwrap()
            .clone()
            .ok_or("the update is not downloaded yet")?;
        return install_apk(&path);
    }
    #[cfg(not(target_os = "android"))]
    {
        let _ = state;
        Err("this build updates from the release page".into())
    }
}

/// Start the system package installer on `path` through the app's FileProvider.
/// The provider's authority is `${applicationId}.fileprovider` (AndroidManifest.xml),
/// and it serves the cache folder (`res/xml/file_paths.xml`).
#[cfg(target_os = "android")]
fn install_apk(path: &std::path::Path) -> Result<(), String> {
    use jni::objects::JObject;
    // The window library set both up before any command runs.
    let ctx = ndk_context::android_context();
    // SAFETY: the VM and the activity are the ones the Android runtime gave the
    // window library. The activity is a reference the runtime owns, so it is
    // never released here.
    let vm = unsafe { jni::JavaVM::from_raw(ctx.vm().cast()) }.map_err(|e| e.to_string())?;
    let mut env = vm.attach_current_thread().map_err(|e| e.to_string())?;
    let activity = std::mem::ManuallyDrop::new(unsafe { JObject::from_raw(ctx.context().cast()) });
    start_installer(&mut env, &activity, path).map_err(|e| {
        let _ = env.exception_clear();
        format!("could not open the installer: {e}")
    })
}

#[cfg(target_os = "android")]
fn start_installer(
    env: &mut jni::JNIEnv,
    activity: &jni::objects::JObject,
    path: &std::path::Path,
) -> jni::errors::Result<()> {
    use jni::objects::{JObject, JValue};
    let authority = env.new_string("org.idan2025.gamingplatformprns.launcher.fileprovider")?;
    let name = env.new_string(path.to_string_lossy().as_ref())?;
    let file = env.new_object("java/io/File", "(Ljava/lang/String;)V", &[JValue::Object(&name)])?;
    let uri = env
        .call_static_method(
            "androidx/core/content/FileProvider",
            "getUriForFile",
            "(Landroid/content/Context;Ljava/lang/String;Ljava/io/File;)Landroid/net/Uri;",
            &[JValue::Object(activity), JValue::Object(&authority), JValue::Object(&file)],
        )?
        .l()?;
    let action = env.new_string("android.intent.action.VIEW")?;
    let intent: JObject = env.new_object("android/content/Intent", "(Ljava/lang/String;)V", &[JValue::Object(&action)])?;
    let mime = env.new_string("application/vnd.android.package-archive")?;
    env.call_method(
        &intent,
        "setDataAndType",
        "(Landroid/net/Uri;Ljava/lang/String;)Landroid/content/Intent;",
        &[JValue::Object(&uri), JValue::Object(&mime)],
    )?;
    // FLAG_GRANT_READ_URI_PERMISSION: the installer reads the file through the provider.
    env.call_method(&intent, "addFlags", "(I)Landroid/content/Intent;", &[JValue::Int(0x0000_0001)])?;
    env.call_method(activity, "startActivity", "(Landroid/content/Intent;)V", &[JValue::Object(&intent)])?;
    Ok(())
}

/// Whether an Android launcher offers prerelease builds (`update::android`).
#[tauri::command]
async fn android_previews(state: tauri::State<'_, AppState>) -> Result<bool, String> {
    Ok(state.launcher.android_previews_on().await)
}

#[tauri::command]
async fn set_android_previews(state: tauri::State<'_, AppState>, on: bool) -> Result<(), String> {
    state.launcher.set_android_previews_on(on).await.map_err(|e| e.to_string())
}

/// An invite link reached this launcher — at start, from a second launch, or
/// from the OS while running. Parsed here, kept for the page, and announced
/// to it; never acted on. A link only says which server to show.
fn receive_invite(app: &tauri::AppHandle, url: &str) {
    match launcher_core::invite::parse(url) {
        Ok(invite) => {
            if let Some(state) = app.try_state::<AppState>() {
                if let Ok(mut p) = state.pending_invite.lock() {
                    *p = Some(invite.clone());
                }
            }
            let _ = app.emit("invite", invite);
            if let Some(w) = app.get_webview_window("main") {
                #[cfg(desktop)]
                let _ = w.unminimize();
                let _ = w.set_focus();
            }
        }
        Err(e) => tracing::info!(error = %e, "ignored a link that is not an invite"),
    }
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
async fn leave(app: tauri::AppHandle, state: tauri::State<'_, AppState>) -> Result<(), String> {
    let left = state.launcher.leave().await.map_err(fmt_err);
    sync_keepalive(&app, &state).await;
    left
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

/// Let the player pick their game's program in the system's own file
/// dialog, then remember it like `set_game_path`. `None` when they cancelled.
/// The dialog runs here rather than from the page, so the page needs no
/// file-system permission at all.
#[tauri::command]
async fn pick_game_path(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    game_id: String,
) -> Result<Option<String>, String> {
    use tauri_plugin_dialog::DialogExt;
    let (tx, rx) = tokio::sync::oneshot::channel();
    app.dialog().file().set_title("Pick your copy of the game").pick_file(move |picked| {
        let _ = tx.send(picked);
    });
    let Some(picked) = rx.await.map_err(|_| "the file dialog closed unexpectedly".to_string())? else {
        return Ok(None);
    };
    let path = picked.into_path().map_err(|e| format!("that file cannot be used: {e}"))?;
    state.launcher.set_game_path(&game_id, &path).await.map_err(fmt_err)?;
    Ok(Some(path.display().to_string()))
}

/// Open Steam's install page for a game, or its store page without Steam.
#[tauri::command]
async fn get_game(app: tauri::AppHandle, state: tauri::State<'_, AppState>, game_id: String) -> Result<(), String> {
    use tauri_plugin_opener::OpenerExt;
    let url = state.launcher.get_game_url(&game_id).await.ok_or("this game has no Steam page to open")?;
    app.opener().open_url(url, None::<&str>).map_err(|e| e.to_string())
}

/// Every game with a pack, and whether it is on this machine.
#[tauri::command]
async fn installed_games(state: tauri::State<'_, AppState>) -> Result<Vec<launcher_core::InstalledGameView>, String> {
    Ok(state.launcher.installed_games().await)
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
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    game_id: String,
    name: Option<String>,
) -> Result<RoomView, String> {
    let hosted = state.launcher.host_room(&game_id, name).await.map_err(fmt_err);
    sync_keepalive(&app, &state).await;
    hosted
}

#[tauri::command]
async fn join_room(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    destination_hash: String,
    game_id: String,
) -> Result<RoomView, String> {
    let joined = state.launcher.join_room(&destination_hash, &game_id).await.map_err(fmt_err);
    sync_keepalive(&app, &state).await;
    joined
}

#[tauri::command]
async fn leave_room(app: tauri::AppHandle, state: tauri::State<'_, AppState>) -> Result<(), String> {
    let left = state.launcher.leave_room().await.map_err(fmt_err);
    sync_keepalive(&app, &state).await;
    left
}

#[tauri::command]
async fn room_status(state: tauri::State<'_, AppState>) -> Result<RoomView, String> {
    Ok(state.launcher.room_status().await)
}

#[tauri::command]
async fn heard_pack_repos(
    state: tauri::State<'_, AppState>,
) -> Result<Vec<launcher_core::repo::HeardRepoView>, String> {
    Ok(state.launcher.heard_pack_repos().await)
}

#[tauri::command]
async fn sync_official_packs(
    state: tauri::State<'_, AppState>,
) -> Result<launcher_core::repo::OfficialSyncView, String> {
    Ok(state.launcher.sync_official_packs().await)
}

#[tauri::command]
async fn set_official_packs(state: tauri::State<'_, AppState>, on: bool) -> Result<(), String> {
    state.launcher.set_official_packs(on).await.map_err(fmt_err)
}

#[tauri::command]
async fn official_packs(
    state: tauri::State<'_, AppState>,
) -> Result<launcher_core::repo::OfficialSyncView, String> {
    let mut view = state.launcher.official_sync();
    view.on = state.launcher.official_packs_on().await;
    Ok(view)
}

#[tauri::command]
async fn pack_repos(
    state: tauri::State<'_, AppState>,
) -> Result<Vec<launcher_core::repo::PackRepoView>, String> {
    Ok(state.launcher.browse_pack_repos().await)
}

#[tauri::command]
async fn add_pack_repo(
    state: tauri::State<'_, AppState>,
    url: String,
) -> Result<launcher_core::repo::PackRepoView, String> {
    state.launcher.add_pack_repo(&url).await.map_err(fmt_err)
}

#[tauri::command]
async fn remove_pack_repo(state: tauri::State<'_, AppState>, url: String) -> Result<(), String> {
    state.launcher.remove_pack_repo(&url).await.map_err(fmt_err)
}

#[tauri::command]
async fn install_repo_pack(
    state: tauri::State<'_, AppState>,
    url: String,
    id: String,
) -> Result<launcher_core::repo::InstalledPackView, String> {
    state.launcher.install_repo_pack(&url, &id).await.map_err(fmt_err)
}

#[tauri::command]
async fn remove_installed_pack(state: tauri::State<'_, AppState>, id: String) -> Result<(), String> {
    state.launcher.remove_installed_pack(&id).await.map_err(fmt_err)
}

#[tauri::command]
async fn room_chat(
    state: tauri::State<'_, AppState>,
    since: u64,
) -> Result<Vec<launcher_core::lan::RoomChatLineView>, String> {
    Ok(state.launcher.room_chat(since).await)
}

#[tauri::command]
async fn send_room_chat(state: tauri::State<'_, AppState>, text: String) -> Result<(), String> {
    state.launcher.send_room_chat(&text).await.map_err(fmt_err)
}

#[tauri::command]
async fn server_chat_view(
    state: tauri::State<'_, AppState>,
    hosted: bool,
) -> Result<launcher_core::server_chat::ServerChatView, String> {
    Ok(if hosted {
        state.launcher.hosted_chat_view().await
    } else {
        state.launcher.server_chat_view().await
    })
}

#[tauri::command]
async fn server_chat(
    state: tauri::State<'_, AppState>,
    hosted: bool,
    since: u64,
) -> Result<Vec<launcher_core::server_chat::ServerChatLineView>, String> {
    Ok(if hosted {
        state.launcher.hosted_chat(since).await
    } else {
        state.launcher.server_chat(since).await
    })
}

#[tauri::command]
async fn send_server_chat(
    state: tauri::State<'_, AppState>,
    hosted: bool,
    text: String,
) -> Result<(), String> {
    if hosted {
        state.launcher.send_hosted_chat(&text).await.map_err(fmt_err)
    } else {
        state.launcher.send_server_chat(&text).await.map_err(fmt_err)
    }
}

#[tauri::command]
async fn set_hosted_chat(state: tauri::State<'_, AppState>, on: bool) -> Result<(), String> {
    state.launcher.set_hosted_chat(on).await.map_err(fmt_err)
}

#[tauri::command]
async fn mute_in_hosted_chat(
    state: tauri::State<'_, AppState>,
    identity: String,
    muted: bool,
) -> Result<(), String> {
    state.launcher.mute_in_hosted_chat(&identity, muted).await.map_err(fmt_err)
}

#[tauri::command]
async fn check_room(state: tauri::State<'_, AppState>) -> Result<RoomCheckView, String> {
    state.launcher.check_room().await.map_err(fmt_err)
}

#[tauri::command]
async fn fix_room_firewall(state: tauri::State<'_, AppState>) -> Result<RoomView, String> {
    state.launcher.fix_room_firewall().await.map_err(fmt_err)
}

#[tauri::command]
async fn allow_room_ports(state: tauri::State<'_, AppState>) -> Result<RoomView, String> {
    state.launcher.allow_room_ports().await.map_err(fmt_err)
}

#[tauri::command]
async fn set_room_wide_open(state: tauri::State<'_, AppState>, on: bool) -> Result<RoomView, String> {
    state.launcher.set_room_wide_open(on).await.map_err(fmt_err)
}

#[tauri::command]
async fn reset_room_ports(state: tauri::State<'_, AppState>) -> Result<RoomView, String> {
    state.launcher.reset_room_ports().await.map_err(fmt_err)
}

#[tauri::command]
async fn unblock_room_programs(state: tauri::State<'_, AppState>) -> Result<RoomView, String> {
    state.launcher.unblock_room_programs().await.map_err(fmt_err)
}

#[cfg(not(target_os = "android"))]
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

#[cfg(target_os = "android")]
mod bundled {
    include!(concat!(env!("OUT_DIR"), "/bundled_packs.rs"));
}

/// Where an Android launcher keeps its packs and its settings: the app's own
/// folder, the only place it may write. There is no `$HOME` to derive one
/// from (writing under `/` is what failed with "Permission denied"), and the
/// shipped packs are APK assets no file call can read, so they are written
/// out from the binary on every start, replacing the last start's copy.
/// Packs from a repository live beside the settings file, as on desktop.
#[cfg(target_os = "android")]
fn android_dirs(app: &tauri::AppHandle) -> Result<(PathBuf, Option<PathBuf>), Box<dyn std::error::Error>> {
    let root = app.path().app_data_dir()?;
    let packs = root.join("bundled-packs");
    let _ = std::fs::remove_dir_all(&packs);
    std::fs::create_dir_all(&packs)?;
    for (name, bytes) in bundled::BUNDLED_PACKS {
        std::fs::write(packs.join(name), bytes)?;
    }
    Ok((packs, Some(root.join("gaming-platform-prns").join("launcher.json"))))
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
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

    let is_portable = portable.is_some();
    let builder = tauri::Builder::default();
    // First, so a second launch — which is how Windows and Linux hand a
    // clicked invite link to an app — passes its link here and exits
    // instead of starting a second launcher with a second mesh node. Phones
    // have no second launch to speak of, so this is desktop only.
    #[cfg(desktop)]
    let builder = builder.plugin(tauri_plugin_single_instance::init(|app, argv, _cwd| {
        for arg in argv.iter().filter(|a| a.starts_with("lanthorn://")) {
            receive_invite(app, arg);
        }
        show_main(app);
    }));
    #[cfg(desktop)]
    let builder = builder.plugin(tauri_plugin_updater::Builder::new().build());
    builder
        .plugin(tauri_plugin_keepalive::init())
        .plugin(tauri_plugin_deep_link::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_dialog::init())
        .setup(move |app| {
            #[cfg(not(target_os = "android"))]
            let (packs, settings) = (pack_dir(app.handle()), launcher_core::settings::default_settings_path());
            #[cfg(target_os = "android")]
            let (packs, settings) = android_dirs(app.handle())?;
            let launcher = Launcher::from_pack_dir_at(&packs, settings).with_portable(portable.is_some());
            tracing::info!(
                packs = %packs.display(),
                games = launcher.list_games().len(),
                portable = ?portable.as_ref().map(|p| p.data_dir.display().to_string()),
                "launcher starting"
            );
            app.manage(AppState {
                launcher,
                pending_invite: std::sync::Mutex::new(None),
                #[cfg(desktop)]
                update: tokio::sync::Mutex::new(None),
                #[cfg(target_os = "android")]
                android_update: std::sync::Mutex::new(None),
                #[cfg(target_os = "android")]
                android_apk_path: std::sync::Mutex::new(None),
                tray: std::sync::atomic::AtomicBool::new(false),
                told_tray: std::sync::atomic::AtomicBool::new(false),
            });
            // Gamepads for big screen mode, read here on Linux because the
            // webview may not (`launcher_core::gamepad`). The page merges
            // these with its own Gamepad API.
            {
                let handle = app.handle().clone();
                launcher_core::gamepad::watch(move |report| {
                    let _ = handle.emit("pad", report);
                });
            }
            // On Linux the tray loads libayatana-appindicator at run time and
            // panics when the desktop has none; that must cost the tray, not
            // the launcher.
            #[cfg(desktop)]
            {
                let handle = app.handle().clone();
                match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| build_tray(&handle))) {
                    Ok(Ok(())) => app.state::<AppState>().tray.store(true, std::sync::atomic::Ordering::Relaxed),
                    Ok(Err(e)) => tracing::info!(error = %e, "no tray icon; closing the window quits"),
                    Err(_) => tracing::info!("no tray library on this desktop; closing the window quits"),
                }
            }
            {
                use tauri_plugin_deep_link::DeepLinkExt;
                // The OS learns the scheme from the installer (Windows), the
                // app bundle (macOS) or the package's .desktop file (Linux).
                // An AppImage has no installer, so it registers itself; a
                // portable launcher registers nothing, because it writes
                // nothing outside its folder.
                #[cfg(target_os = "linux")]
                if !is_portable && std::env::var_os("APPIMAGE").is_some() {
                    let _ = app.deep_link().register_all();
                }
                let _ = is_portable;
                let handle = app.handle().clone();
                app.deep_link().on_open_url(move |event| {
                    for url in event.urls() {
                        receive_invite(&handle, url.as_str());
                    }
                });
                if let Ok(Some(urls)) = app.deep_link().get_current() {
                    for url in urls {
                        receive_invite(app.handle(), url.as_str());
                    }
                }
            }
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
        // Closing the window while a room is open or a server is joined keeps
        // the launcher in the tray, because both end with it; otherwise it
        // quits, as it always did.
        .on_window_event(|window, event| {
            let tauri::WindowEvent::CloseRequested { api, .. } = event else { return };
            if window.label() != "main" {
                return;
            }
            let app = window.app_handle().clone();
            let state = app.state::<AppState>();
            if !state.tray.load(std::sync::atomic::Ordering::Relaxed) {
                return;
            }
            api.prevent_close();
            let window = window.clone();
            tauri::async_runtime::spawn(async move {
                let state = app.state::<AppState>();
                match state.launcher.keeps_running().await {
                    Some(what) => {
                        let _ = window.hide();
                        if !state.told_tray.swap(true, std::sync::atomic::Ordering::Relaxed) {
                            let (title, body) = if what == "room" {
                                ("Your LAN room is still open", "Lanthorn keeps it open from the tray. Quit from the tray icon to end it.")
                            } else {
                                ("Still connected to the server", "Lanthorn keeps the connection from the tray. Quit from the tray icon to end it.")
                            };
                            send_notification(&app, title, body);
                        }
                    }
                    None => app.exit(0),
                }
            });
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
            favorites,
            set_favorite,
            trace_path,
            announce_room,
            check_update,
            install_update,
            set_auto_update,
            auto_update,
            take_invite,
            parse_invite,
            invite_link,
            open_release_page,
            download_update,
            install_update_android,
            android_previews,
            set_android_previews,
            indexes,
            add_index,
            remove_index,
            leave,
            play_server,
            game_location,
            set_game_path,
            clear_game_path,
            notify,
            notifications_on,
            first_run,
            finish_first_run,
            host_server,
            stop_hosting,
            hosting,
            set_notifications_on,
            pick_game_path,
            installed_games,
            get_game,
            player_name,
            list_interfaces,
            add_interface,
            remove_interface,
            saved_browse_opts,
            set_player_name,
            room_chat,
            send_room_chat,
            server_chat_view,
            server_chat,
            send_server_chat,
            set_hosted_chat,
            mute_in_hosted_chat,
            pack_repos,
            sync_official_packs,
            heard_pack_repos,
            set_official_packs,
            official_packs,
            add_pack_repo,
            remove_pack_repo,
            install_repo_pack,
            remove_installed_pack,
            lan_helper,
            grant_lan_helper,
            revoke_lan_helper,
            host_room,
            join_room,
            leave_room,
            room_status,
            check_room,
            fix_room_firewall,
            unblock_room_programs,
            allow_room_ports,
            reset_room_ports,
            set_room_wide_open,
        ])
        .run(tauri::generate_context!())
        .expect("error while running the launcher");
}
