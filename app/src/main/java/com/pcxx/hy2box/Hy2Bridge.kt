package com.pcxx.hy2box

object Hy2Bridge {
    init {
        System.loadLibrary("hy2core")
    }

    external fun nativeInit(): Long
    external fun nativeStart(ptr: Long, server: String, auth: String): Boolean
    external fun nativeStop(ptr: Long)
    external fun nativeDestroy(ptr: Long)
}
