package com.pcxx.hy2box

// VpnService 与界面之间的轻量状态桥（避免引入广播/绑定服务的复杂度）
object TunnelStatus {
    @Volatile
    var text: String = "未连接"
}

object Hy2Bridge {
    init {
        System.loadLibrary("hy2core")
    }

    external fun nativeInit(): Long
    external fun nativeStart(ptr: Long, server: String, auth: String): Boolean
    external fun nativeGetStatus(ptr: Long): String?
    external fun nativeStop(ptr: Long)
    external fun nativeDestroy(ptr: Long)
}
