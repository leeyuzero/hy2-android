package com.pcxx.hy2box

import android.content.Context
import org.json.JSONObject

// 与 Rust 侧 UiConfig 的字段一一对应；toJson() 是 JNI 唯一的传参入口。
data class Hy2Config(
    val server: String = "",
    val auth: String = "",
    val serverName: String = "",
    val insecure: Boolean = false,
    val txMbps: String = "0",
    val rxMbps: String = "0",
    val obfsPassword: String = "",
    val hopPorts: String = "",
    val hopMin: String = "0",
    val hopMax: String = "0",
    val fastOpen: Boolean = true,
    val pinSha256: String = "",
) {
    fun toJson(): String = JSONObject().apply {
        put("server", server.trim())
        put("auth", auth)
        put("server_name", serverName.trim())
        put("insecure", insecure)
        put("tx_mbps", num(txMbps))
        put("rx_mbps", num(rxMbps))
        put("obfs_password", obfsPassword.trim())
        put("hop_ports", hopPorts.trim())
        put("hop_interval_min_secs", num(hopMin))
        put("hop_interval_max_secs", num(hopMax))
        put("fast_open", fastOpen)
        put("pin_sha256", pinSha256.trim())
    }.toString()

    fun summary(): String = buildString {
        append(if (serverName.isBlank()) "SNI=自动" else "SNI=$serverName")
        append(" · insecure=").append(if (insecure) "开" else "关")
        append(" · tx=").append(num(txMbps)).append("Mbps")
        append(" / rx=").append(num(rxMbps)).append("Mbps")
        append(" · obfs=").append(if (obfsPassword.isBlank()) "关" else "开")
        append(" · hop=").append(if (hopPorts.isBlank()) "关" else hopPorts.trim())
        append(" · fastOpen=").append(if (fastOpen) "开" else "关")
        append(" · pin=").append(if (pinSha256.isBlank()) "关" else "开")
    }

    companion object {
        // 一律收敛为非负整数：负数 / 非法输入会让 Rust 侧 u64 反序列化失败。
        private fun num(s: String): Long = s.trim().toLongOrNull()?.coerceAtLeast(0L) ?: 0L
    }
}

class ConfigStore(ctx: Context) {
    private val sp = ctx.getSharedPreferences("hy2box", Context.MODE_PRIVATE)

    // 旧版本只存了 server / auth；新增 key 读不到时走默认值，覆盖安装不会丢配置。
    fun load(): Hy2Config = Hy2Config(
        server = sp.getString("server", "") ?: "",
        auth = sp.getString("auth", "") ?: "",
        serverName = sp.getString("serverName", "") ?: "",
        insecure = sp.getBoolean("insecure", false),
        txMbps = sp.getString("txMbps", "0") ?: "0",
        rxMbps = sp.getString("rxMbps", "0") ?: "0",
        obfsPassword = sp.getString("obfsPassword", "") ?: "",
        hopPorts = sp.getString("hopPorts", "") ?: "",
        hopMin = sp.getString("hopMin", "0") ?: "0",
        hopMax = sp.getString("hopMax", "0") ?: "0",
        fastOpen = sp.getBoolean("fastOpen", true),
        pinSha256 = sp.getString("pinSha256", "") ?: "",
    )

    fun save(c: Hy2Config) {
        sp.edit()
            .putString("server", c.server)
            .putString("auth", c.auth)
            .putString("serverName", c.serverName)
            .putBoolean("insecure", c.insecure)
            .putString("txMbps", c.txMbps)
            .putString("rxMbps", c.rxMbps)
            .putString("obfsPassword", c.obfsPassword)
            .putString("hopPorts", c.hopPorts)
            .putString("hopMin", c.hopMin)
            .putString("hopMax", c.hopMax)
            .putBoolean("fastOpen", c.fastOpen)
            .putString("pinSha256", c.pinSha256)
            .apply()
    }
}
