package com.pcxx.hy2box

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.content.Intent
import android.net.VpnService
import android.os.Build
import android.os.ParcelFileDescriptor

class Hy2VpnService : VpnService() {
    private var tun: ParcelFileDescriptor? = null
    private var enginePtr: Long = 0L

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
            stopSelf()
            return
        }

        // Stage 1: 建 TUN + 配置路由，验证 VpnService 权限链路。
        // Stage 2: 把 tun.fd 交给 Rust 侧 tun2socks，实现真正转发。
        val builder = Builder()
            .setSession("hy2box")
            .setMtu(1500)
            .addAddress("10.0.0.2", 32)
            .addRoute("0.0.0.0", 0)
            .addDnsServer("1.1.1.1")

        // 排除 App 自身流量，避免回环死锁
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.N) {
            builder.addDisallowedApplication(packageName)
        }

        tun = builder.establish()

        Hy2Bridge.nativeStart(enginePtr, config.server, config.auth)
    }

    private fun disconnect() {
        Hy2Bridge.nativeStop(enginePtr)
        tun?.close()
        tun = null
        stopForeground(STOP_FOREGROUND_REMOVE)
        stopSelf()
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
        return if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
            Notification.Builder(this, CHANNEL_ID)
                .setContentTitle("hy2box")
                .setContentText("Hysteria2 tunnel (Stage 1)")
                .setSmallIcon(android.R.drawable.ic_dialog_info)
                .setContentIntent(pi)
                .build()
        } else {
            @Suppress("DEPRECATION")
            Notification.Builder(this)
                .setContentTitle("hy2box")
                .setContentText("Hysteria2 tunnel (Stage 1)")
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
