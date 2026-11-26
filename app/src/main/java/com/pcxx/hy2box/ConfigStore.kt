package com.pcxx.hy2box

import android.content.Context

data class Hy2Config(val server: String, val auth: String)

class ConfigStore(ctx: Context) {
    private val sp = ctx.getSharedPreferences("hy2box", Context.MODE_PRIVATE)

    fun load(): Hy2Config = Hy2Config(
        server = sp.getString("server", "") ?: "",
        auth = sp.getString("auth", "") ?: "",
    )

    fun save(config: Hy2Config) {
        sp.edit()
            .putString("server", config.server)
            .putString("auth", config.auth)
            .apply()
    }
}
