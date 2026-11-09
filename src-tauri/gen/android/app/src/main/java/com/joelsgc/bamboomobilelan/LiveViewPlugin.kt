package com.joelsgc.bamboomobilelan

import android.app.Activity
import android.content.Intent
import app.tauri.annotation.Command
import app.tauri.annotation.TauriPlugin
import app.tauri.plugin.Invoke
import app.tauri.plugin.Plugin

/** Opens [LiveViewActivity]; registered from Rust as the "liveView" plugin. */
@TauriPlugin
class LiveViewPlugin(private val activity: Activity) : Plugin(activity) {

    @Command
    fun open(invoke: Invoke) {
        val args = invoke.getArgs()
        val intent = Intent(activity, LiveViewActivity::class.java)
            .putExtra(LiveViewActivity.EXTRA_URL, args.getString("url", "")!!)
            .putExtra(LiveViewActivity.EXTRA_ACCESS_CODE, args.getString("access_code", "")!!)
        activity.startActivity(intent)
        invoke.resolve()
    }
}
