package com.pcxx.hy2box

import android.app.Activity
import android.content.Intent
import android.net.VpnService
import android.os.Bundle
import android.os.Handler
import android.os.Looper
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.foundation.layout.*
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.ui.Modifier
import androidx.compose.ui.unit.dp

class MainActivity : ComponentActivity() {
    private lateinit var store: ConfigStore
    private var status by mutableStateOf("未连接")
    private val handler = Handler(Looper.getMainLooper())
    private var polling = false

    private val statusPoll = object : Runnable {
        override fun run() {
            status = TunnelStatus.text
            if (polling) handler.postDelayed(this, 400)
        }
    }

    private fun startPolling() {
        polling = true
        handler.removeCallbacks(statusPoll)
        handler.post(statusPoll)
    }

    private fun stopPolling() {
        polling = false
        handler.removeCallbacks(statusPoll)
    }

    private val vpnLauncher = registerForActivityResult(
        ActivityResultContracts.StartActivityForResult()
    ) { result ->
        if (result.resultCode == Activity.RESULT_OK) startVpn()
        else {
            TunnelStatus.connected = false
            TunnelStatus.text = "VPN 权限被拒绝"
            status = TunnelStatus.text
        }
    }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        store = ConfigStore(this)
        val cfg = store.load()
        var serverInput by mutableStateOf(cfg.server)
        var authInput by mutableStateOf(cfg.auth)

        setContent {
            MaterialTheme {
                Column(Modifier.padding(20.dp)) {
                    Text("hy2box · Stage 2 / Step 4", style = MaterialTheme.typography.titleLarge)
                    Spacer(Modifier.height(4.dp))
                    Text(
                        "TCP / DNS / UDP 全部经 hy2 隧道；IPv6 已劫持并丢弃，不再绕隧道泄漏。" +
                            "本步修复断开按钮，并加入流表超时清理与重连计数。" +
                            "分片重组、MSS 钳制仍待后续。",
                        style = MaterialTheme.typography.bodySmall
                    )
                    Spacer(Modifier.height(16.dp))
                    OutlinedTextField(
                        value = serverInput,
                        onValueChange = { serverInput = it },
                        label = { Text("服务器地址 (host:port)") },
                        modifier = Modifier.fillMaxWidth()
                    )
                    Spacer(Modifier.height(8.dp))
                    OutlinedTextField(
                        value = authInput,
                        onValueChange = { authInput = it },
                        label = { Text("认证密码") },
                        modifier = Modifier.fillMaxWidth()
                    )
                    Spacer(Modifier.height(16.dp))
                    Button(onClick = {
                        store.save(Hy2Config(serverInput, authInput))
                        TunnelStatus.connected = true
                        TunnelStatus.text = "正在启动…"
                        status = "正在启动…"
                        startPolling()
                        val intent = VpnService.prepare(this@MainActivity)
                        if (intent != null) vpnLauncher.launch(intent) else startVpn()
                    }, modifier = Modifier.fillMaxWidth()) {
                        Text("连接（全局上网）")
                    }
                    Spacer(Modifier.height(8.dp))
                    Button(onClick = {
                        // 先停掉界面轮询并写死「已断开」，再显式发 ACTION_DISCONNECT。
                        // 不能用 stopService：它不会把 action 交给 onStartCommand，
                        // 且前台服务在部分 ROM 上不能被可靠停止。
                        stopPolling()
                        TunnelStatus.connected = false
                        TunnelStatus.text = "已断开"
                        status = "已断开"
                        startService(Intent(this@MainActivity, Hy2VpnService::class.java).apply {
                            action = Hy2VpnService.ACTION_DISCONNECT
                        })
                    }, modifier = Modifier.fillMaxWidth()) {
                        Text("断开")
                    }
                    Spacer(Modifier.height(20.dp))
                    Text("运行状态（握手 + TCP / DNS / UDP / IPv6 / 重连）", style = MaterialTheme.typography.labelLarge)
                    Spacer(Modifier.height(6.dp))
                    Card(Modifier.fillMaxWidth()) {
                        Text(
                            status,
                            style = MaterialTheme.typography.bodyMedium,
                            modifier = Modifier.padding(14.dp)
                        )
                    }
                    Spacer(Modifier.height(12.dp))
                    Text(
                        "验收：点「断开」后状态必须停在「已断开」不再跳动，VPN 图标消失、网页回到直连；" +
                            "再点「连接」能重新握手。IPv6 丢弃计数在涨说明封堵生效。" +
                            "DNS 计数为 0 时先关系统「私人 DNS」。",
                        style = MaterialTheme.typography.bodySmall
                    )
                }
            }
        }
    }

    override fun onResume() {
        super.onResume()
        handler.removeCallbacks(statusPoll)
        // 只有连接态才恢复轮询，否则断回到前台又会把「已断开」覆盖掉
        if (TunnelStatus.connected) startPolling()
    }

    override fun onPause() {
        handler.removeCallbacks(statusPoll)
        super.onPause()
    }

    private fun startVpn() {
        val intent = Intent(this, Hy2VpnService::class.java).apply {
            action = Hy2VpnService.ACTION_CONNECT
        }
        startForegroundService(intent)
        TunnelStatus.connected = true
        TunnelStatus.text = "正在启动…"
        status = "正在启动…"
        startPolling()
    }
}
