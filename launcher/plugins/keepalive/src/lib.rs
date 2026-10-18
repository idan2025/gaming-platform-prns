//! A foreground service on Android, so a session survives the app going to the
//! background (`launcher/src-tauri` starts it when a join or room opens).
//!
//! Android kills a background process within minutes, and the game takes the
//! screen while Lanthorn's link must stay up. A service with a notification is
//! the one thing the OS keeps running. On desktop this plugin does nothing: no
//! desktop OS needs it.

use tauri::{
    plugin::{Builder, TauriPlugin},
    Runtime,
};
#[cfg(target_os = "android")]
use tauri::Manager;

#[cfg(target_os = "android")]
pub struct KeepAlive<R: Runtime>(tauri::plugin::PluginHandle<R>);

/// Start or stop the foreground service. Mobile only; desktop is a no-op.
#[cfg(target_os = "android")]
pub fn set<R: Runtime>(app: &tauri::AppHandle<R>, on: bool) -> Result<(), String> {
    let keep = app.state::<KeepAlive<R>>();
    let command = if on { "start" } else { "stop" };
    keep.0
        .run_mobile_plugin::<()>(command, ())
        .map_err(|e| format!("could not {command} the background service: {e}"))
}

#[cfg(not(target_os = "android"))]
pub fn set<R: Runtime>(_app: &tauri::AppHandle<R>, _on: bool) -> Result<(), String> {
    Ok(())
}

pub fn init<R: Runtime>() -> TauriPlugin<R> {
    Builder::new("keepalive")
        .setup(|app, api| {
            #[cfg(target_os = "android")]
            {
                let handle = api.register_android_plugin("org.idan2025.gamingplatformprns.keepalive", "KeepAlivePlugin")?;
                app.manage(KeepAlive(handle));
            }
            #[cfg(not(target_os = "android"))]
            {
                let _ = (app, api);
            }
            Ok(())
        })
        .build()
}
