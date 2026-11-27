package com.pcxx.hy2box

import android.content.Context
import android.content.pm.ApplicationInfo
import android.content.pm.PackageManager
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.Checkbox
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Switch
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.unit.dp

data class AppInfo(
    val pkg: String,
    val label: String,
    val system: Boolean,
)

// 需要 Manifest 声明 QUERY_ALL_PACKAGES（API 30+ 应用可见性限制）。
// 不过滤无启动图标的系统服务（如 com.google.android.gms）——按包名分流时它们同样有效。
fun loadInstalledApps(ctx: Context): List<AppInfo> {
    val pm = ctx.packageManager
    val self = ctx.packageName
    val list = try {
        @Suppress("DEPRECATION")
        pm.getInstalledApplications(PackageManager.GET_META_DATA)
    } catch (e: Exception) {
        return emptyList()
    }
    return list.mapNotNull { ai ->
        if (ai.packageName == self) return@mapNotNull null
        val label = try {
            pm.getApplicationLabel(ai).toString()
        } catch (e: Exception) {
            ai.packageName
        }
        AppInfo(
            pkg = ai.packageName,
            label = if (label.isBlank()) ai.packageName else label,
            system = (ai.flags and ApplicationInfo.FLAG_SYSTEM) != 0,
        )
    }.sortedWith(
        compareBy<AppInfo> { it.label.lowercase() }
            .thenBy { it.pkg }
    )
}

@Composable
fun AppPickerDialog(
    all: List<AppInfo>,
    selected: Set<String>,
    onConfirm: (Set<String>) -> Unit,
    onDismiss: () -> Unit,
) {
    var query by remember { mutableStateOf("") }
    var showSystem by remember { mutableStateOf(false) }
    // 每次都赋新 Set，确保 Compose 能观测到变化
    var checked by remember { mutableStateOf(selected) }

    val filtered = remember(query, showSystem, all) {
        val q = query.trim()
        all.filter { ai ->
            (showSystem || !ai.system) &&
                (q.isEmpty() || ai.label.contains(q, ignoreCase = true) ||
                    ai.pkg.contains(q, ignoreCase = true))
        }
    }

    AlertDialog(
        onDismissRequest = onDismiss,
        title = { Text("选择应用 · 已选 ${checked.size}") },
        text = {
            Column(Modifier.fillMaxWidth()) {
                OutlinedTextField(
                    value = query,
                    onValueChange = { query = it },
                    label = { Text("搜索应用名 / 包名") },
                    singleLine = true,
                    modifier = Modifier.fillMaxWidth(),
                )
                Row(verticalAlignment = Alignment.CenterVertically) {
                    Text(
                        "显示系统应用（如 Play 服务）",
                        modifier = Modifier.weight(1f),
                        style = MaterialTheme.typography.bodySmall,
                    )
                    Switch(checked = showSystem, onCheckedChange = { showSystem = it })
                }
                if (filtered.isEmpty()) {
                    Text(
                        if (all.isEmpty()) "未读取到应用列表（检查 QUERY_ALL_PACKAGES 权限）"
                        else "没有匹配的应用",
                        style = MaterialTheme.typography.bodySmall,
                        modifier = Modifier.padding(vertical = 12.dp),
                    )
                } else {
                    Text("共 ${filtered.size} 个", style = MaterialTheme.typography.labelSmall)
                    Spacer(Modifier.height(4.dp))
                    // 固定高度：AlertDialog 的 text 区不能承载无限高度的可滚动组件
                    LazyColumn(Modifier.fillMaxWidth().height(360.dp)) {
                        items(filtered, key = { it.pkg }) { ai ->
                            Row(
                                verticalAlignment = Alignment.CenterVertically,
                                modifier = Modifier.fillMaxWidth(),
                            ) {
                                Checkbox(
                                    checked = ai.pkg in checked,
                                    onCheckedChange = { on ->
                                        val next = checked.toMutableSet()
                                        if (on) next.add(ai.pkg) else next.remove(ai.pkg)
                                        checked = next
                                    },
                                )
                                Column(Modifier.weight(1f)) {
                                    Text(
                                        ai.label,
                                        style = MaterialTheme.typography.bodyMedium,
                                        maxLines = 1,
                                    )
                                    Text(
                                        ai.pkg,
                                        style = MaterialTheme.typography.labelSmall,
                                        maxLines = 1,
                                    )
                                }
                            }
                        }
                    }
                }
            }
        },
        confirmButton = {
            TextButton(onClick = { onConfirm(checked); onDismiss() }) { Text("确定") }
        },
        dismissButton = {
            TextButton(onClick = onDismiss) { Text("取消") }
        },
    )
}
