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
    // 分流规则改过但还没重新建立 VPN：applyAppRules() 只在 establish() 读一次。
    // 提升为类成员，让 vpnLauncher / startVpn 也能重置它。
    private var ruleDirty by mutableStateOf(false)
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
            // 权限被拒 → 新规则没有建立成功，横幅要继续提醒重连
            ruleDirty = true
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

        // ===== 应用管控状态 =====
        var showAppControl by mutableStateOf(false)
        var showPicker by mutableStateOf(false)
        var appRule by mutableStateOf(store.loadAppRule())
        var manualPkg by mutableStateOf("")
        // 规则变化立即落盘，不需要用户额外点「保存」；
        // 同时打上 dirty 标记，横幅会一直提醒「断开 → 连接」直到新规则真正生效。
        val updateRule: (AppRule) -> Unit = { next ->
            appRule = next
            store.saveAppRule(next)
            ruleDirty = true
        }

        setContent {
            // remember 只在首次组合时枚举一次；Compose 内不引入额外协程依赖
            val installedApps = remember { loadInstalledApps(this@MainActivity) }
            MaterialTheme {
                Column(
                    Modifier
                        .padding(20.dp)
                        .verticalScroll(rememberScrollState())
                ) {
                    Text("hy2box · 配置项 + 应用管控版", style = MaterialTheme.typography.titleLarge)
                    Spacer(Modifier.height(4.dp))
                    Text(
                        "基础三项 + 展开「高级设置」可配置 insecure / 带宽 / obfs / 端口跳跃 / 证书 Pin；" +
                            "展开「应用管控」可按 App 分流。节点配置以 JSON 传给 Rust 内核；" +
                            "分流规则只作用于 Android VpnService，不传给内核。",
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

                    // ===== 应用管控 / 按 App 分流 =====
                    Spacer(Modifier.height(8.dp))
                    TextButton(onClick = { showAppControl = !showAppControl }) {
                        Text(
                            if (showAppControl) "收起应用管控 ▲"
                            else "展开应用管控 ▼  按 App 分流（${appRule.label()}）"
                        )
                    }
                    // 折叠状态下也要看见：改完规则不重连，是「选了 Google 却没生效」的最常见原因
                    if (ruleDirty) {
                        Card(
                            colors = CardDefaults.cardColors(
                                containerColor = MaterialTheme.colorScheme.errorContainer
                            ),
                            modifier = Modifier.fillMaxWidth(),
                        ) {
                            Text(
                                "分流规则已改，但只在建立 VPN 时读取一次。当前连接仍用旧规则 → 请先点「断开」，再点「连接」。",
                                style = MaterialTheme.typography.bodySmall,
                                color = MaterialTheme.colorScheme.onErrorContainer,
                                modifier = Modifier.padding(12.dp),
                            )
                        }
                        Spacer(Modifier.height(6.dp))
                    }
                    if (showAppControl) {
                        Text(
                            "规则在 VPN 建立时生效，改完需「断开 → 连接」。自身 App 永远被排除，" +
                                "避免 hy2 的 QUIC 流量被劫持进 TUN 造成回环。",
                            style = MaterialTheme.typography.bodySmall
                        )
                        Spacer(Modifier.height(6.dp))
                        listOf(
                            AppMode.GLOBAL to "全局：所有应用走隧道（默认）",
                            AppMode.BLACKLIST to "排除模式：勾选的应用直连，其余走隧道",
                            AppMode.WHITELIST to "仅允许模式：仅勾选的应用走隧道，其余直连",
                        ).forEach { (m, label) ->
                            Row(verticalAlignment = Alignment.CenterVertically) {
                                RadioButton(
                                    selected = appRule.mode == m,
                                    onClick = { updateRule(appRule.copy(mode = m)) },
                                )
                                Text(label, style = MaterialTheme.typography.bodySmall)
                            }
                        }
                        Spacer(Modifier.height(6.dp))
                        Text(
                            "已勾选 ${appRule.packages.size} 个应用 · 当前：${appRule.label()}",
                            style = MaterialTheme.typography.labelMedium
                        )
                        Spacer(Modifier.height(6.dp))
                        Button(onClick = { showPicker = true }, modifier = Modifier.fillMaxWidth()) {
                            Text("从已安装应用中选择")
                        }
                        // 仅允许模式下只勾 Play 商店必连不上：账号 / 推送 / 下载分别属于别的包
                        if (appRule.mode == AppMode.WHITELIST) {
                            Spacer(Modifier.height(6.dp))
                            Button(
                                onClick = {
                                    val next = appRule.packages.toMutableSet()
                                    next.addAll(GooglePackages.CORE)
                                    updateRule(appRule.copy(packages = next))
                                },
                                modifier = Modifier.fillMaxWidth(),
                            ) {
                                Text("一键加入 Google 全家桶（${GooglePackages.CORE.size} 个）")
                            }
                            Text(
                                "仅允许模式必须勾全：商店 + Play 服务 + 服务框架 + 下载器。" +
                                    "设备未安装的包会被自动跳过，多填无害。",
                                style = MaterialTheme.typography.labelSmall,
                            )
                        }
                        Spacer(Modifier.height(6.dp))
                        Row(verticalAlignment = Alignment.CenterVertically) {
                            OutlinedTextField(
                                value = manualPkg,
                                onValueChange = { manualPkg = it },
                                label = { Text("手动添加包名，如 com.android.vending") },
                                singleLine = true,
                                modifier = Modifier.weight(1f),
                            )
                            Spacer(Modifier.width(6.dp))
                            Button(onClick = {
                                val p = manualPkg.trim()
                                if (p.isNotBlank()) {
                                    val next = appRule.packages.toMutableSet()
                                    next.add(p)
                                    updateRule(appRule.copy(packages = next))
                                    manualPkg = ""
                                }
                            }) { Text("添加") }
                        }
                        if (appRule.packages.isNotEmpty()) {
                            Spacer(Modifier.height(6.dp))
                            Text("点击可移除：", style = MaterialTheme.typography.labelSmall)
                            // 外层已是可滚动 Column，这里用普通 Column，不能嵌套 LazyColumn
                            Column {
                                appRule.packages.sorted().forEach { pkg ->
                                    TextButton(onClick = {
                                        val next = appRule.packages.toMutableSet()
                                        next.remove(pkg)
                                        updateRule(appRule.copy(packages = next))
                                    }) {
                                        Text("✕ $pkg", style = MaterialTheme.typography.bodySmall)
                                    }
                                }
                            }
                        }
                        Spacer(Modifier.height(4.dp))
                        Text(
                            "Google 走隧道的正确配置：① 推荐「全局」，所有 App 走隧道，Google 自然在内；" +
                                "② 或「仅允许模式」点上面的一键加入 Google 全家桶（漏掉 GMS / 服务框架必连不上）。" +
                                "注意：「排除模式」里勾选 Google = 让它直连，国内网络直连 Google 必然连不上，别这样配。",
                            style = MaterialTheme.typography.bodySmall
                        )
                        Spacer(Modifier.height(4.dp))
                        Text(
                            "判断流量是否真进隧道：打开 Play 时状态卡「DNS 查询」与「TUN IP 包」应持续增长。" +
                                "若不涨，说明 DNS 被系统解析器漏出（部分 ROM 在仅允许模式下会这样）→ 改回「全局」再断开重连。",
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
                        store.saveAppRule(appRule)
                        configSummary = current.summary()
                        TunnelStatus.connected = true
                        TunnelStatus.text = "正在启动…"
                        status = "正在启动…"
                        startPolling()
                        val intent = VpnService.prepare(this@MainActivity)
                        if (intent != null) vpnLauncher.launch(intent) else startVpn()
                    }, modifier = Modifier.fillMaxWidth()) {
                        Text("连接（${appRule.label()}）")
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
                            "改分流模式或勾选应用后必须断开重连，通知栏会显示当前分流模式。" +
                            "断开后状态停在「已断开」；DNS 计数为 0 时先关系统「私人 DNS」。",
                        style = MaterialTheme.typography.bodySmall
                    )

                    if (showPicker) {
                        AppPickerDialog(
                            all = installedApps,
                            selected = appRule.packages,
                            onConfirm = { sel -> updateRule(appRule.copy(packages = sel)) },
                            onDismiss = { showPicker = false },
                        )
                    }
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
        // 新规则已随本次 establish() 生效，撤掉「需重连」横幅
        ruleDirty = false
        TunnelStatus.connected = true
        TunnelStatus.text = "正在启动…"
        status = "正在启动…"
        startPolling()
    }
}
