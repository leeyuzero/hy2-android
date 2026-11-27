package com.pcxx.hy2box

// VpnService 与界面之间的轻量状态桥（避免引入广播/绑定服务的复杂度）
object TunnelStatus {
    @Volatile
    var text: String = "未连接"

    // 断开后必须置 false：防止 Service 里迟到的状态轮询把「已断开」覆盖回连接态。
    // 这也是「断开按钮无效」的根因之一（界面状态被 400ms 轮询写回去）。
    @Volatile
    var connected: Boolean = false
}

object Hy2Bridge {
    init {
        System.loadLibrary("hy2core")
    }

    external fun nativeInit(): Long
    external fun nativeAttachTun(ptr: Long, fd: Int): Boolean
    external fun nativeDetachTun(ptr: Long)
    external fun nativeStart(ptr: Long, server: String, auth: String): Boolean
    external fun nativeGetStatus(ptr: Long): String?
    external fun nativeStop(ptr: Long)
    external fun nativeDestroy(ptr: Long)
}
