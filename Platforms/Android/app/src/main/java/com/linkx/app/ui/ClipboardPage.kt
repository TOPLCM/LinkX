/**
 * 剪贴板页：同步开关、手动补同步、最近发送与最近接收。
 *
 * 由原先的 `MainActivity.kt` 拆出（一页一个文件）。
 */

package com.linkx.app.ui

import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.Switch
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.runtime.getValue
import androidx.compose.runtime.setValue
import com.linkx.app.ClipboardSync
import com.linkx.app.EmptyHint
import com.linkx.app.LinkxRuntime
import com.linkx.app.SectionCard

// ---------- 页：剪贴板 ----------

@Composable
internal fun ClipboardPage(refresh: Int) {
    var on by remember { mutableStateOf(ClipboardSync.enabled) }
    val sent = remember(refresh) { LinkxRuntime.clipSentRecent() }
    val recv = remember(refresh) { LinkxRuntime.clipRecvRecent() }

    SectionCard(title = "剪贴板同步") {
        Row(
            modifier = Modifier.fillMaxWidth(),
            horizontalArrangement = Arrangement.SpaceBetween,
            verticalAlignment = Alignment.CenterVertically,
        ) {
            Text(if (on) "已开启" else "已关闭", style = MaterialTheme.typography.bodyMedium)
            Switch(
                checked = on,
                onCheckedChange = {
                    on = it
                    ClipboardSync.setEnabled(it)
                },
            )
        }
        Text(
            "开启后，已配对设备间的纯文本复制会自动同步。",
            style = MaterialTheme.typography.bodySmall,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
        )
        // Android 10+ 只允许持输入焦点的应用读剪贴板，后台监听不会触发：给降级说明 + 手动兜底入口。
        Text(
            "Android 10+ 限制：在其他应用里复制后，切回本页会自动补同步；也可点下方按钮立即发送。",
            style = MaterialTheme.typography.bodySmall,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
        )
        OutlinedButton(onClick = { ClipboardSync.syncNow("手动") }) {
            Text("立即同步本机剪贴板")
        }
    }

    SectionCard(title = "最近发送") {
        if (sent.isEmpty()) EmptyHint("暂无记录") else sent.forEach { Text(it, style = MaterialTheme.typography.bodyMedium) }
    }
    SectionCard(title = "最近接收") {
        if (recv.isEmpty()) EmptyHint("暂无记录") else recv.forEach { Text(it, style = MaterialTheme.typography.bodyMedium) }
    }
}
