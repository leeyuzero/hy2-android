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
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.text.input.KeyboardType
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
        var sniInput by mutableStateOf(cfg.serverName)
        var insecureInput by mutableStateOf(cfg.insecure)
        var txInput by mutableStateOf(cfg.txMbps)
        var rxInput by mutableStateOf(cfg.rxMbps)
        var obfsInput by mutableStateOf(cfg.obfsPassword)
        var hopInput by mutableStateOf(cfg.hopPorts)
        var hopMinInput by mutableStateOf(cfg.hopMin)
        var hopMaxInput by mutableStateOf(cfg.hopMax)
        var fastOpenInput by mutableStateOf(cfg.fastOpen)
        var pinInput by mutableStateOf(cfg.pinSha256)
        var showAdvanced by mutableStateOf(false)
        var configSummary by mutableStateOf(cfg.summary())

        setContent {
            MaterialTheme {
                Column(
                    Modifier
                        .padding(20.dp)
                        .verticalScroll(rememberScrollState())
                ) {
                    Text("hy2box · 配置项版", style = MaterialTheme.typography.titleLarge)
                    Spacer(Modifier.height(4.dp))
                    Text(
                        "基础三项 + 展开「高级设置」可配置 insecure / 带宽 / obfs / 端口跳跃 / 证书 Pin。" +
                            "所有配置以 JSON 传给 Rust 内核，状态卡会回显实际生效值。",
                        style = MaterialTheme.typography.bodySmall
                    )
                    Spacer(Modifier.height(16.dp))
                    OutlinedTextField(
                        value = serverInput,
                        onValueChange = { serverInput = it },
                        label = { Text("服务器地址 (host:port)") },
                        singleLine = true,
                        modifier = Modifier.fillMaxWidth()
                    )
                    Spacer(Modifier.height(8.dp))
                    OutlinedTextField(
                        value = authInput,
                        onValueChange = { authInput = it },
                        label = { Text("认证密码") },
                        singleLine = true,
                        modifier = Modifier.fillMaxWidth()
                    )
                    Spacer(Modifier.height(8.dp))
                    OutlinedTextField(
                        value = sniInput,
                        onValueChange = { sniInput = it },
                        label = { Text("TLS SNI / server_name（留空 = 取域名）") },
                        singleLine = true,
                        modifier = Modifier.fillMaxWidth()
                    )
                    Spacer(Modifier.height(8.dp))
                    TextButton(onClick = { showAdvanced = !showAdvanced }) {
                        Text(
                            if (showAdvanced) "收起高级设置 ▲"
                            else "展开高级设置 ▼  insecure / 带宽 / obfs / 端口跳跃"
                        )
                    }
                    if (showAdvanced) {
                        Row(verticalAlignment = Alignment.CenterVertically) {
                            Text("跳过证书校验 insecure（自签证书节点）", Modifier.weight(1f))
                            Switch(checked = insecureInput, onCheckedChange = { insecureInput = it })
                        }
                        Spacer(Modifier.height(6.dp))
                        OutlinedTextField(
                            value = txInput,
                            onValueChange = { txInput = it },
                            label = { Text("上传带宽 Mbps（0 = BBR）") },
                            singleLine = true,
                            keyboardOptions = KeyboardOptions(keyboardType = KeyboardType.Number),
                            modifier = Modifier.fillMaxWidth()
                        )
                        Spacer(Modifier.height(6.dp))
                        OutlinedTextField(
                            value = rxInput,
                            onValueChange = { rxInput = it },
                            label = { Text("下载带宽 Mbps（0 = 服务端测速）") },
                            singleLine = true,
                            keyboardOptions = KeyboardOptions(keyboardType = KeyboardType.Number),
                            modifier = Modifier.fillMaxWidth()
                        )
                        Spacer(Modifier.height(6.dp))
                        OutlinedTextField(
                            value = obfsInput,
                            onValueChange = { obfsInput = it },
                            label = { Text("Salamander 混淆密码（空 = 关闭，至少 4 字节）") },
                            singleLine = true,
                            modifier = Modifier.fillMaxWidth()
                        )
                        Spacer(Modifier.height(6.dp))
                        OutlinedTextField(
                            value = hopInput,
                            onValueChange = { hopInput = it },
                            label = { Text("端口跳跃（空 = 关闭；20000-50000 / 443,8443 / all）") },
                            singleLine = true,
                            modifier = Modifier.fillMaxWidth()
                        )
                        Spacer(Modifier.height(6.dp))
                        OutlinedTextField(
                            value = hopMinInput,
                            onValueChange = { hopMinInput = it },
                            label = { Text("跳跃最小间隔秒（0 = 默认 30）") },
                            singleLine = true,
                            keyboardOptions = KeyboardOptions(keyboardType = KeyboardType.Number),
                            modifier = Modifier.fillMaxWidth()
                        )
                        Spacer(Modifier.height(6.dp))
                        OutlinedTextField(
                            value = hopMaxInput,
                            onValueChange = { hopMaxInput = it },
                            label = { Text("跳跃最大间隔秒（0 = 固定为最小间隔）") },
                            singleLine = true,
                            keyboardOptions = KeyboardOptions(keyboardType = KeyboardType.Number),
                            modifier = Modifier.fillMaxWidth()
                        )
                        Spacer(Modifier.height(6.dp))
                        Row(verticalAlignment = Alignment.CenterVertically) {
                            Text("Fast Open（默认开）", Modifier.weight(1f))
                            Switch(checked = fastOpenInput, onCheckedChange = { fastOpenInput = it })
                        }
                        Spacer(Modifier.height(6.dp))
                        OutlinedTextField(
                            value = pinInput,
                            onValueChange = { pinInput = it },
                            label = { Text("证书 SHA-256 Pin（64 位 hex，可带冒号；空 = 不 pin）") },
                            singleLine = true,
                            modifier = Modifier.fillMaxWidth()
                        )
                        Spacer(Modifier.height(4.dp))
                        Text(
                            "换算：1 Mbps = 125000 bytes/sec。tx>0 启用 Brutal 拥塞控制，0 走 BBR。" +
                                "ca_pem（PEM 根证书）与 QUIC 调参暂不暴露，保持库默认。",
                            style = MaterialTheme.typography.bodySmall
                        )
                    }
                    Spacer(Modifier.height(12.dp))
                    Button(onClick = {
                        val current = Hy2Config(
                            server = serverInput,
                            auth = authInput,
                            serverName = sniInput,
                            insecure = insecureInput,
                            txMbps = txInput,
                            rxMbps = rxInput,
                            obfsPassword = obfsInput,
                            hopPorts = hopInput,
                            hopMin = hopMinInput,
                            hopMax = hopMaxInput,
                            fastOpen = fastOpenInput,
                            pinSha256 = pinInput,
                        )
                        store.save(current)
                        configSummary = current.summary()
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
                    Text(
                        "运行状态（握手 + 生效配置 + TCP / DNS / UDP / IPv6）",
                        style = MaterialTheme.typography.labelLarge
                    )
                    Spacer(Modifier.height(6.dp))
                    Card(Modifier.fillMaxWidth()) {
                        Text(
                            status,
                            style = MaterialTheme.typography.bodyMedium,
                            modifier = Modifier.padding(14.dp)
                        )
                    }
                    Spacer(Modifier.height(8.dp))
                    Text("已保存配置：$configSummary", style = MaterialTheme.typography.bodySmall)
                    Spacer(Modifier.height(12.dp))
                    Text(
                        "验收：状态卡「配置：…」一行应与界面填写一致；改 insecure / 带宽 / obfs 后重连能握手成功即生效。" +
                            "断开后状态停在「已断开」；DNS 计数为 0 时先关系统「私人 DNS」。",
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
