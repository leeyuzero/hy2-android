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

    @Volatile
    private var probing = false
    private var probeThread: Thread? = null

    @Volatile
    private var probeSocket: java.net.DatagramSocket? = null

    // 每 400ms 把 Rust 侧状态（握手 + TUN 包计数）同步给界面
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

        // Stage 2 · Step 1：只劫持保留测试网段 198.18.0.0/16，
        // 全局上网不受影响；探测包会写入 TUN，供 Rust 侧读取计数。
        // 不加 addDisallowedApplication，否则 App 自身探测包不会进 TUN。
        val builder = Builder()
            .setSession("hy2box")
            .setMtu(1500)
            .addAddress("10.0.0.2", 32)
            .addRoute("198.18.0.0", 16)

        val pfd = builder.establish()
        if (pfd == null) {
            TunnelStatus.text = "VPN 权限未授予或 TUN 建立失败"
            return
        }

        // detachFd() 把裸 fd 的所有权交给 Rust；之后不能再 pfd.close()
        val fd = pfd.detachFd()
        tun = null

        if (!Hy2Bridge.nativeAttachTun(enginePtr, fd)) {
            TunnelStatus.text = "TUN fd 挂载失败"
            ParcelFileDescriptor.adoptFd(fd).close()
            return
        }

        // 握手目标不在测试网段，仍走真实网络，不受 TUN 影响
        Hy2Bridge.nativeStart(enginePtr, config.server, config.auth)
        startProbe()
        handler.removeCallbacks(statusPoll)
        handler.post(statusPoll)
    }

    // 定时往测试网段发 UDP 包，产生确定性的 TUN 入站流量
    private fun startProbe() {
        stopProbe()
        probing = true
        val sock = java.net.DatagramSocket()
        probeSocket = sock
        probeThread = Thread {
            try {
                // 故意不调用 protect()，让这个包进入 TUN
                val addr = java.net.InetAddress.getByName("198.18.0.1")
                val payload = ByteArray(32)
                val packet = java.net.DatagramPacket(payload, payload.size, addr, 53)
                while (probing && !Thread.currentThread().isInterrupted) {
                    try {
                        sock.send(packet)
                    } catch (_: Exception) {
                    }
                    try {
                        Thread.sleep(200)
                    } catch (_: InterruptedException) {
                        break
                    }
                }
            } catch (t: Throwable) {
                android.util.Log.w("hy2box", "probe thread ended", t)
            } finally {
                try {
                    sock.close()
                } catch (_: Exception) {
                }
            }
        }.apply {
            isDaemon = true
            start()
        }
    }

    private fun stopProbe() {
        probing = false
        probeThread?.interrupt()
        probeThread = null
        try {
            probeSocket?.close()
        } catch (_: Exception) {
        }
        probeSocket = null
    }

    private fun disconnect() {
        handler.removeCallbacks(statusPoll)
        stopProbe()
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
        val text = "Step 1：TUN fd 通道验证"
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
