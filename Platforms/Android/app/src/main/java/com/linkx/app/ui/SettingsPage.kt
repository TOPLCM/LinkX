/**
 * 设置页：后台运行白名单、功能开关、相册应答、设备管理、外观、Debug 日志导出、隐私。
 *
 * 由原先的 `MainActivity.kt` 拆出（一页一个文件）。
 */

package com.linkx.app.ui

import android.content.Context
import android.content.Intent
import android.net.Uri
import android.os.PowerManager
import android.provider.DocumentsContract
import android.provider.Settings
import android.util.Log
import android.widget.Toast
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.RowScope
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.Button
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.Switch
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.unit.dp
import kotlinx.coroutines.launch
import androidx.compose.runtime.getValue
import androidx.compose.runtime.setValue
import com.linkx.app.AlbumAccess
import com.linkx.app.AlbumProvider
import com.linkx.app.AppPrefs
import com.linkx.app.Features
import com.linkx.app.KeyValueRow
import com.linkx.app.LinkxRuntime
import com.linkx.app.Module
import com.linkx.app.NativeCore
import com.linkx.app.SectionCard
import com.linkx.app.ThemeMode

// ---------- 页：设置 ----------

@Composable
internal fun SettingsPage(refresh: Int) {
    val ctx = LocalContext.current
    val peerFp = remember(refresh) { LinkxRuntime.peerFp }
    val localFp = remember(refresh) { LinkxRuntime.localFingerprint }
    val trusted = remember(refresh) { LinkxRuntime.trustedDevices() }
    val debugOn = remember(refresh) { LinkxRuntime.debugEnabled }
    var confirmUnbind by remember { mutableStateOf(false) }

    // 省电白名单：不在名单里，ROM 会在 App 退到后台后冻结线程——"不在前台就控不了媒体、
    // 通知不及时"就是这么来的。系统不允许我们自己加白，只能把状态说清楚并一键跳过去。
    val unrestricted = remember(refresh) {
        (ctx.getSystemService(Context.POWER_SERVICE) as? PowerManager)
            ?.isIgnoringBatteryOptimizations(ctx.packageName) ?: false
    }
    SectionCard(title = "后台运行") {
        Text(
            if (unrestricted) "已允许后台运行：退到后台也能继续同步与接收电脑的控制"
            else "未允许：手机退到后台后系统可能暂停本应用，通知与媒体控制会延迟甚至失联",
            style = MaterialTheme.typography.bodySmall,
            color = if (unrestricted) Color(0xFF1E9E55) else MaterialTheme.colorScheme.error,
        )
        if (!unrestricted) {
            OutlinedButton(
                onClick = {
                    val ok = runCatching {
                        ctx.startActivity(
                            Intent(Settings.ACTION_IGNORE_BATTERY_OPTIMIZATION_SETTINGS)
                        )
                    }.isSuccess
                    if (!ok) {
                        Toast.makeText(
                            ctx,
                            "系统没有这个设置页，请在「设置 → 电池」里手动把 LinkX 设为无限制",
                            Toast.LENGTH_LONG,
                        ).show()
                    }
                },
            ) { Text("去系统设置") }
        }
    }

    SectionCard(title = "功能开关") {
        Text(
            "关掉不用的功能，重启后就不再初始化它——不起线程、不注册回调、也不建这一页。",
            style = MaterialTheme.typography.bodySmall,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
        )
        Module.entries.forEach { m ->
            Row(
                modifier = Modifier.fillMaxWidth(),
                horizontalArrangement = Arrangement.SpaceBetween,
                verticalAlignment = Alignment.CenterVertically,
            ) {
                Column(modifier = Modifier.weight(1f)) {
                    Text(m.label, style = MaterialTheme.typography.bodyMedium)
                    Text(
                        m.about,
                        style = MaterialTheme.typography.bodySmall,
                        color = MaterialTheme.colorScheme.onSurfaceVariant,
                    )
                }
                Switch(
                    checked = Features.isWanted(m),
                    onCheckedChange = { Features.setWanted(ctx, m, it) },
                )
            }
        }
        val pending = Features.changes()
        if (pending.isNotEmpty()) {
            Text(
                "以下改动要重启才生效：" +
                    pending.joinToString("、") { (m, on) -> "${m.label}${if (on) "开" else "关"}" } +
                    "。当前已加载：${Features.activeModules().joinToString("、") { it.label }.ifEmpty { "无" }}",
                style = MaterialTheme.typography.bodySmall,
                color = MaterialTheme.colorScheme.primary,
            )
        }
    }

    // 相册应答方**没有浏览界面**（电脑问、手机答），这一页只交代两件事：开关与权限状态。
    // 权限形态必须分开显示：Android 14 的「仅选定照片」是真授权，说成"未授予"会逼用户
    // 再点一次同一个框，而他想要的其实只是那几张不要的全库授权。
    var albumTick by remember { mutableStateOf(0) }
    val albumAccess = remember(albumTick) { AlbumProvider.access(ctx) }
    val albumLauncher = rememberLauncherForActivityResult(
        ActivityResultContracts.RequestMultiplePermissions(),
    ) { albumTick++ }
    SectionCard(title = "图片互传（相册应答）") {
        KeyValueRow(
            "相册权限",
            AlbumProvider.accessBrief(ctx),
            valueColor = when (albumAccess) {
                AlbumAccess.Full -> Color(0xFF1E9E55)
                AlbumAccess.SelectedOnly -> MaterialTheme.colorScheme.primary
                AlbumAccess.Denied -> MaterialTheme.colorScheme.error
            },
        )
        Text(
            if (Features.enabled(Module.Album)) {
                "手机只应答电脑发来的清单/缩略图/原图请求；缩略图现场生成、直接发出，" +
                    "不在本机留任何缓存文件（关掉就没有）。照片只会经局域网交给已配对的电脑。"
            } else {
                "已关闭：电脑再问也只会收到一句「相册同步已在手机端关闭」，不会静默。"
            },
            style = MaterialTheme.typography.bodySmall,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
        )
        if (albumAccess != AlbumAccess.Full && Features.enabled(Module.Album)) {
            OutlinedButton(onClick = { albumLauncher.launch(AlbumProvider.permissions()) }) {
                Text(if (albumAccess == AlbumAccess.Denied) "授予相册权限" else "重新选择可见照片")
            }
        }
        KeyValueRow("最近一次应答", AlbumProvider.lastStatus)
        if (AlbumProvider.lastError.isNotEmpty()) {
            KeyValueRow("待处理原因", AlbumProvider.lastError, valueColor = MaterialTheme.colorScheme.error)
        }
    }

    SectionCard(title = "设备管理") {
        KeyValueRow("已绑定设备", if (trusted.isEmpty()) "—" else trusted.joinToString("、") { it.second.ifBlank { it.first } })
        KeyValueRow("对端指纹", peerFp ?: "—")
        KeyValueRow("本机指纹", localFp ?: "—")
        Text(
            "解绑将清除本机信任的对端指纹，下次连接需重新配对（比对 6 位配对码）。",
            style = MaterialTheme.typography.bodySmall,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
        )
        OutlinedButton(
            onClick = { confirmUnbind = true },
            enabled = trusted.isNotEmpty() || peerFp != null,
        ) { Text("解绑设备") }
    }

    SectionCard(title = "外观") {
        Row(
            modifier = Modifier.fillMaxWidth(),
            horizontalArrangement = Arrangement.spacedBy(8.dp),
        ) {
            ThemeOption(ThemeMode.System, "跟随系统")
            ThemeOption(ThemeMode.Light, "浅色")
            ThemeOption(ThemeMode.Dark, "深色")
        }
        Text(
            when (AppPrefs.themeMode) {
                ThemeMode.System -> "跟随系统深色开关（Android 10+），系统切换时界面自动跟随"
                else -> "已手动指定外观；选「跟随系统」可恢复自动切换"
            },
            style = MaterialTheme.typography.bodySmall,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
        )
    }

    // Debug 模式：一键开关全栈日志，导出到任意目录（SAF 选择）
    val exportLauncher = rememberLauncherForActivityResult(
        ActivityResultContracts.OpenDocumentTree(),
    ) { uri ->
        if (uri == null) return@rememberLauncherForActivityResult
        val n = copyDebugExportToTree(ctx, uri)
        val tip = when {
            n == null -> "导出失败（日志目录不可读）"
            n == 0 -> "没有可导出的日志文件"
            else -> "已导出 $n 个日志文件"
        }
        Toast.makeText(ctx, tip, Toast.LENGTH_LONG).show()
    }
    SectionCard(title = "开发者选项") {
        Row(
            modifier = Modifier.fillMaxWidth(),
            horizontalArrangement = Arrangement.SpaceBetween,
            verticalAlignment = Alignment.CenterVertically,
        ) {
            Column(Modifier.weight(1f)) {
                Text("Debug 模式", style = MaterialTheme.typography.titleSmall)
                Text(
                    if (debugOn) {
                        "已开启：全栈运行日志落盘（app 私有 Logs 目录）"
                    } else {
                        "开启后持续记录全栈运行状态与错误，便于定位问题"
                    },
                    style = MaterialTheme.typography.bodySmall,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
            }
            Switch(checked = debugOn, onCheckedChange = { LinkxRuntime.setDebugEnabled(it) })
        }
        Text(
            "日志含配对与设备信息（不做脱敏），导出后请仅交给可信方。",
            style = MaterialTheme.typography.bodySmall,
            color = MaterialTheme.colorScheme.error,
        )
        OutlinedButton(onClick = { exportLauncher.launch(null) }) { Text("导出 Debug 日志") }
    }

    SectionCard(title = "关于") {
        KeyValueRow("Core 版本", runCatching { NativeCore.nativeVersion() }.getOrDefault("—"))
        KeyValueRow("对端设备指纹", peerFp ?: "—")
    }

    SectionCard(title = "隐私") {
        Text("数据仅在局域网内点对点传输，不经云端、不留存", style = MaterialTheme.typography.bodyMedium)
        Text(
            "通知读取仅在手机端本地进行，正文按敏感级别可选不同步",
            style = MaterialTheme.typography.bodySmall,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
        )
    }

    if (confirmUnbind) {
        AlertDialog(
            onDismissRequest = { confirmUnbind = false },
            title = { Text("解绑设备") },
            text = {
                Text("解绑后将清除本机信任的对端指纹并断开当前会话，下次连接需重新配对。")
            },
            confirmButton = {
                TextButton(onClick = {
                    confirmUnbind = false
                    LinkxRuntime.unbind()
                }) { Text("解绑") }
            },
            dismissButton = {
                TextButton(onClick = { confirmUnbind = false }) { Text("取消") }
            },
        )
    }
}

/**
 * 把 Debug 日志导出目录里的文件逐个写入用户选定的 SAF 目录，返回写入数（准备阶段失败 = null）。
 * 日志**不脱敏**：去向必须由用户显式选择（系统目录选择器），不做后台静默落盘。
 */
internal fun copyDebugExportToTree(ctx: Context, treeUri: Uri): Int? {
    val src = LinkxRuntime.prepareDebugExport() ?: return null
    val files = src.listFiles()?.filter { it.isFile } ?: return null
    if (files.isEmpty()) return 0
    val resolver = ctx.contentResolver
    val parent = DocumentsContract.buildDocumentUriUsingTree(
        treeUri,
        DocumentsContract.getTreeDocumentId(treeUri),
    )
    var written = 0
    for (f in files) {
        runCatching {
            val target = DocumentsContract.createDocument(
                resolver,
                parent,
                "application/octet-stream",
                f.name,
            ) ?: return@runCatching
            resolver.openOutputStream(target)?.use { out ->
                f.inputStream().use { input -> input.copyTo(out) }
            }
            written++
        }.onFailure { Log.w("LinkX.Export", "写入 ${f.name} 失败", it) }
    }
    return written
}

/** 主题三选一（选中态用实心按钮，未选中用描边按钮；等分宽度） */
@Composable
internal fun RowScope.ThemeOption(mode: ThemeMode, label: String) {
    val active = AppPrefs.themeMode == mode
    val modifier = Modifier.weight(1f)
    val padding = PaddingValues(horizontal = 4.dp)
    if (active) {
        Button(onClick = { AppPrefs.setTheme(mode) }, modifier = modifier, contentPadding = padding) {
            Text(label, maxLines = 1)
        }
    } else {
        OutlinedButton(onClick = { AppPrefs.setTheme(mode) }, modifier = modifier, contentPadding = padding) {
            Text(label, maxLines = 1)
        }
    }
}
