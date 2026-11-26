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

    // 每 400ms 把 Rust 侧的握手状态同步给界面
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

        // Stage 1.5 握手验证版：建立 TUN 仅为验证 VPN 权限链路，
        // 故意不 addRoute("0.0.0.0", 0)，避免劫持全局流量导致手机断网。
        // Stage 2 做数据面时再恢复全局路由。
        val builder = Builder()
            .setSession("hy2box")
            .setMtu(1500)
            .addAddress("10.0.0.2", 32)

        tun = builder.establish()
        if (tun == null) {
            TunnelStatus.text = "VPN 权限未授予或 TUN 建立失败"
            return
        }

        Hy2Bridge.nativeStart(enginePtr, config.server, config.auth)
        handler.removeCallbacks(statusPoll)
        handler.post(statusPoll)
    }

    private fun disconnect() {
        handler.removeCallbacks(statusPoll)
        Hy2Bridge.nativeStop(enginePtr)
        tun?.close()
        tun = null
        TunnelStatus.text = "已断开"
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
                .setContentText("Hysteria2 握手验证 (Stage 1.5)")
                .setSmallIcon(android.R.drawable.ic_dialog_info)
                .setContentIntent(pi)
                .build()
        } else {
            @Suppress("DEPRECATION")
            Notification.Builder(this)
                .setContentTitle("hy2box")
                .setContentText("Hysteria2 握手验证 (Stage 1.5)")
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
