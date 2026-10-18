package org.idan2025.gamingplatformprns.keepalive

import android.app.Activity
import android.content.Intent
import androidx.core.content.ContextCompat
import app.tauri.annotation.Command
import app.tauri.annotation.TauriPlugin
import app.tauri.plugin.Invoke
import app.tauri.plugin.Plugin

@TauriPlugin
class KeepAlivePlugin(private val activity: Activity) : Plugin(activity) {

    @Command
    fun start(invoke: Invoke) {
        ContextCompat.startForegroundService(activity, Intent(activity, KeepAliveService::class.java))
        invoke.resolve()
    }

    @Command
    fun stop(invoke: Invoke) {
        activity.stopService(Intent(activity, KeepAliveService::class.java))
        invoke.resolve()
    }
}
