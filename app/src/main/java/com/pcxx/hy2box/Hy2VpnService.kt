package com.pcxx.hy2box

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.content.Intent
import android.net.VpnService
import android.os.Build
import android.os.Handler
import android.os.Looper
import android.os.ParcelFileDescriptor

class Hy2VpnService : VpnService() {
    private var tun: ParcelFileDescriptor? = null
    private var enginePtr: Long = 0L
    private val handler = Handler(Looper.getMainLooper())

    // 每 400ms 把 Rust 侧状态（握手 + TUN 计数 + 连接数）同步给界面
    private val statusPoll = object : Runnable {
        override fun run() {
            if (enginePtr != 0L) {
                try {
                    TunnelStatus.text = Hy2Bridge.nativeGetStatus(enginePtr) ?: "状态读取失败"
                } catch (t: Throwable) {
                    TunnelStatus.text = "状态读取异常：${t.message}"
                }
            }
            handler.postDelayed(this, 400)
        }
    }

    override fun onCreate() {
        super.onCreate()
        createChannel()
        startForeground(1, buildNotification())
        enginePtr = Hy2Bridge.nativeInit()
    }

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        when (intent?.action) {
            ACTION_CONNECT -> connect()
            ACTION_DISCONNECT -> disconnect()
        }
        return START_STICKY
    }

    private fun connect() {
        val config = ConfigStore(this).load()
        if (config.server.isBlank() || config.auth.isBlank()) {
            TunnelStatus.text = "请先填写服务器地址和认证密码"
            stopSelf()
            return
        }

        // 必须先建立 hy2 客户端，数据面线程要复用同一个可重连客户端
        if (!Hy2Bridge.nativeStart(enginePtr, config.server, config.auth)) {
            TunnelStatus.text = "客户端初始化失败：配置读取异常"
            return
        }

        // Stage 2 · Step 2：恢复全局路由，所有 App 流量进 TUN。
        // - 虚拟 DNS 10.0.0.1:53 由 Rust 侧转成 DNS-over-TCP 走隧道
        // - 必须排除自身，否则 hy2 的 QUIC 流量也被劫持进 TUN 造成回环死锁
        val builder = Builder()
            .setSession("hy2box")
            .setMtu(1500)
            .addAddress("10.0.0.2", 24)
            .addRoute("0.0.0.0", 0)
            .addDnsServer("10.0.0.1")
        try {
            builder.addDisallowedApplication(packageName)
        } catch (e: Exception) {
            android.util.Log.w("hy2box", "addDisallowedApplication failed", e)
        }

        val pfd = builder.establish()
        if (pfd == null) {
            TunnelStatus.text = "VPN 权限未授予或 TUN 建立失败"
            return
        }

        // detachFd() 把裸 fd 的所有权交给 Rust；之后不能再 pfd.close()
        val fd = pfd.detachFd()
        tun = null

        if (!Hy2Bridge.nativeAttachTun(enginePtr, fd)) {
            TunnelStatus.text = "TUN fd 挂载失败：客户端未就绪"
            ParcelFileDescriptor.adoptFd(fd).close()
            return
        }

        handler.removeCallbacks(statusPoll)
        handler.post(statusPoll)
    }

    private fun disconnect() {
        handler.removeCallbacks(statusPoll)
        Hy2Bridge.nativeDetachTun(enginePtr)
        Hy2Bridge.nativeStop(enginePtr)
        // fd 已交给 Rust 并由其关闭，这里不要重复 close
        tun = null
        TunnelStatus.text = "已断开"
        stopForeground(STOP_FOREGROUND_REMOVE)
        stopSelf()
    }

    override fun onRevoke() {
        disconnect()
        super.onRevoke()
    }

    override fun onDestroy() {
        disconnect()
        if (enginePtr != 0L) {
            Hy2Bridge.nativeDestroy(enginePtr)
            enginePtr = 0L
        }
        super.onDestroy()
    }

    private fun createChannel() {
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
            val ch = NotificationChannel(
                CHANNEL_ID, "hy2box VPN", NotificationManager.IMPORTANCE_LOW
            )
            getSystemService(NotificationManager::class.java).createNotificationChannel(ch)
        }
    }

    private fun buildNotification(): Notification {
        val pi = PendingIntent.getActivity(
            this, 0, Intent(this, MainActivity::class.java),
            PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT
        )
        val text = "Step 2：全局流量经 hy2 隧道"
        return if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
            Notification.Builder(this, CHANNEL_ID)
                .setContentTitle("hy2box")
                .setContentText(text)
                .setSmallIcon(android.R.drawable.ic_dialog_info)
                .setContentIntent(pi)
                .build()
        } else {
            @Suppress("DEPRECATION")
            Notification.Builder(this)
                .setContentTitle("hy2box")
                .setContentText(text)
                .setSmallIcon(android.R.drawable.ic_dialog_info)
                .setContentIntent(pi)
                .build()
        }
    }

    companion object {
        const val ACTION_CONNECT = "com.pcxx.hy2box.CONNECT"
        const val ACTION_DISCONNECT = "com.pcxx.hy2box.DISCONNECT"
        private const val CHANNEL_ID = "hy2box_vpn"
    }
}
