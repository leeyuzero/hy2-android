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

    private val statusPoll = object : Runnable {
        override fun run() {
            status = TunnelStatus.text
            handler.postDelayed(this, 400)
        }
    }

    private val vpnLauncher = registerForActivityResult(
        ActivityResultContracts.StartActivityForResult()
    ) { result ->
        if (result.resultCode == Activity.RESULT_OK) startVpn()
        else {
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
                    Text("hy2box · Stage 2 / Step 2", style = MaterialTheme.typography.titleLarge)
                    Spacer(Modifier.height(4.dp))
                    Text(
                        "全局流量经 hy2 隧道：TCP 由 smoltcp 还原后转发，DNS 转成 TCP 走隧道。" +
                            "点连接后用浏览器打开网页，能打开即通过。本步暂不处理 UDP 与 IPv6。",
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
                        TunnelStatus.text = "正在启动…"
                        val intent = VpnService.prepare(this@MainActivity)
                        if (intent != null) vpnLauncher.launch(intent) else startVpn()
                    }, modifier = Modifier.fillMaxWidth()) {
                        Text("连接（全局上网）")
                    }
                    Spacer(Modifier.height(8.dp))
                    Button(onClick = {
                        stopService(Intent(this@MainActivity, Hy2VpnService::class.java).apply {
                            action = Hy2VpnService.ACTION_DISCONNECT
                        })
                        TunnelStatus.text = "已断开"
                        status = "已断开"
                    }, modifier = Modifier.fillMaxWidth()) {
                        Text("断开")
                    }
                    Spacer(Modifier.height(20.dp))
                    Text("运行状态（握手 + 包数 / 连接数 / DNS）", style = MaterialTheme.typography.labelLarge)
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
                        "验收：浏览器能打开网页，且「TCP 连接」数字随访问增长。" +
                            "打不开时看「DNS 查询」是否 > 0（为 0 说明 DNS 没进隧道），" +
                            "并用 adb logcat -s hy2core 看 new tcp conn / tcp_connect 日志。",
                        style = MaterialTheme.typography.bodySmall
                    )
                }
            }
        }
    }

    override fun onResume() {
        super.onResume()
        handler.removeCallbacks(statusPoll)
        handler.post(statusPoll)
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
        TunnelStatus.text = "正在启动…"
        status = "正在启动…"
    }
}
