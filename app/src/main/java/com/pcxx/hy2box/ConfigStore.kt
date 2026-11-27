package com.pcxx.hy2box

import android.content.Context
import org.json.JSONObject

// ===== 应用管控 / 按 App 分流 =====
// 这些规则只作用于 Android VpnService.Builder（addAllowedApplication /
// addDisallowedApplication），**不进 JNI JSON、不传给 Rust 内核**，数据面零改动。
// 规则只在 establish() 时读一次，改完必须断开重连才生效。
object AppMode {
    const val GLOBAL = 0      // 全部走隧道（仅自身排除）
    const val BLACKLIST = 1   // 勾选的应用直连，其余走隧道
    const val WHITELIST = 2   // 仅勾选的应用走隧道，其余直连

    fun label(m: Int): String = when (m) {
        BLACKLIST -> "排除模式（勾选直连）"
        WHITELIST -> "仅允许模式（勾选走隧道）"
        else -> "全局"
    }
}

data class AppRule(
    val mode: Int = AppMode.GLOBAL,
    val packages: Set<String> = emptySet(),
) {
    fun label(): String = when (mode) {
        AppMode.BLACKLIST -> "排除 ${packages.size} 个应用"
        AppMode.WHITELIST ->
            if (packages.isEmpty()) "仅允许模式（空，回退全局）"
            else "仅允许 ${packages.size} 个应用"
        else -> "全局"
    }
}

// 与 Rust 侧 UiConfig 的字段一一对应；toJson() 是 JNI 唯一的传参入口。
// 应用管控规则刻意独立存放，不塞进这里，避免改变 JNI JSON 契约。
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

    // ===== 应用管控规则（独立于 Hy2Config / JNI JSON）=====
    // 旧版本没有这两个 key → 回落到「全局 + 空集合」，行为与升级前完全一致。
    fun loadAppRule(): AppRule {
        val rawMode = sp.getInt(KEY_APP_MODE, AppMode.GLOBAL)
        val mode = if (rawMode in AppMode.GLOBAL..AppMode.WHITELIST) rawMode else AppMode.GLOBAL
        val packages = sp.getStringSet(KEY_APP_PACKAGES, emptySet())
            ?.filter { it.isNotBlank() }
            ?.toSet() ?: emptySet()
        return AppRule(mode, packages)
    }

    fun saveAppRule(r: AppRule) {
        sp.edit()
            .putInt(KEY_APP_MODE, r.mode)
            // getStringSet 返回的集合不可原地修改，这里每次都换成新 Set
            .putStringSet(KEY_APP_PACKAGES, r.packages.filter { it.isNotBlank() }.toSet())
            .apply()
    }

    private companion object {
        const val KEY_APP_MODE = "appMode"
        const val KEY_APP_PACKAGES = "appPackages"
    }
}
