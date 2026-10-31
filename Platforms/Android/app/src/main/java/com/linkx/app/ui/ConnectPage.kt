/**
 * 连接页：本机角色、SAS 配对码比对、对端身份变化决策、局域网通道状态与手动填 IP。
 *
 * 由原先的 `MainActivity.kt` 拆出（一页一个文件）。
 */

package com.linkx.app.ui

import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.material3.Button
import androidx.compose.material3.Card
import androidx.compose.material3.CardDefaults
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.dp
import androidx.compose.runtime.getValue
import androidx.compose.runtime.setValue
import com.linkx.app.BatteryMonitor
import com.linkx.app.KeyValueRow
import com.linkx.app.LinkxRuntime
import com.linkx.app.SectionCard
import com.linkx.app.stateLabel

// ---------- 页：连接 ----------

@Composable
internal fun ConnectPage(refresh: Int) {
    val state = remember(refresh) { LinkxRuntime.state }
    val name = remember(refresh) { LinkxRuntime.peerName }
    val sas = remember(refresh) { LinkxRuntime.sas }
    val mismatch = remember(refresh) { LinkxRuntime.fingerprintMismatch }
    val peerFp = remember(refresh) { LinkxRuntime.peerFp }

    SectionCard(title = "本机角色") {
        Text("本机作为 BLE Peripheral，等待电脑连接", style = MaterialTheme.typography.bodyMedium)
        Text(
            "状态：${stateLabel(state)}",
            style = MaterialTheme.typography.bodyMedium,
            color = MaterialTheme.colorScheme.primary,
        )
        Text(
            "对端：${name ?: "—"}",
            style = MaterialTheme.typography.bodySmall,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
        )
        // 电量上报失败也要上屏（同媒体页 lastSkip 口径）：只写 logcat 时，"电脑上没有电量"
        // 第一个被怀疑的总是蓝牙。
        val batteryNote = remember(refresh) { BatteryMonitor.note }
        if (batteryNote.isNotEmpty()) {
            Text(
                "电量上报：$batteryNote",
                style = MaterialTheme.typography.bodySmall,
                color = MaterialTheme.colorScheme.tertiary,
            )
        }
    }

    if (state == LinkxRuntime.STATE_SAS_COMPARE && sas != null) {
        Card(
            modifier = Modifier.fillMaxWidth(),
            colors = CardDefaults.cardColors(
                containerColor = MaterialTheme.colorScheme.primaryContainer,
                contentColor = MaterialTheme.colorScheme.onPrimaryContainer,
            ),
        ) {
            Column(
                Modifier.fillMaxWidth().padding(20.dp),
                horizontalAlignment = Alignment.CenterHorizontally,
                verticalArrangement = Arrangement.spacedBy(10.dp),
            ) {
                Text("请在电脑上核对同一组数字", style = MaterialTheme.typography.bodySmall)
                Text(
                    sas.toString().padStart(6, '0'),
                    style = MaterialTheme.typography.displaySmall,
                    fontWeight = FontWeight.Bold,
                )
                Row(horizontalArrangement = Arrangement.spacedBy(12.dp)) {
                    Button(onClick = { LinkxRuntime.confirmSas() }) { Text("确认一致") }
                    OutlinedButton(onClick = { LinkxRuntime.rejectSas() }) { Text("不一致") }
                }
            }
        }
    }

    val identityChange = remember(refresh) { LinkxRuntime.identityChange }
    if (identityChange != null) {
        // 对端同名设备换了身份必须显式决策：接受 → 引擎回到 Pairing 重走 SAS 复核（出现上方比对卡）；
        // 拒绝 → 断开且不更新信任库。
        Card(
            modifier = Modifier.fillMaxWidth(),
            colors = CardDefaults.cardColors(
                containerColor = MaterialTheme.colorScheme.errorContainer,
                contentColor = MaterialTheme.colorScheme.onErrorContainer,
            ),
        ) {
            Column(
                Modifier.fillMaxWidth().padding(20.dp),
                verticalArrangement = Arrangement.spacedBy(8.dp),
            ) {
                Text("对端设备身份已变化", style = MaterialTheme.typography.titleMedium)
                Text(
                    "设备「${identityChange.name.ifBlank { "对端设备" }}」呈递了新身份（可能已重装或重置）。",
                    style = MaterialTheme.typography.bodySmall,
                )
                Text("旧身份 ${identityChange.oldFingerprint}", style = MaterialTheme.typography.bodySmall)
                Text("新身份 ${identityChange.newFingerprint}", style = MaterialTheme.typography.bodySmall)
                Text(
                    "接受后将重新比对 6 位配对码；仅当确认是本人在操作时才接受。",
                    style = MaterialTheme.typography.bodySmall,
                )
                Row(horizontalArrangement = Arrangement.spacedBy(12.dp)) {
                    Button(onClick = { LinkxRuntime.acceptIdentityChange() }) { Text("信任并重新配对") }
                    OutlinedButton(onClick = { LinkxRuntime.rejectIdentityChange() }) { Text("取消") }
                }
            }
        }
    } else if (mismatch) {
        SectionCard(title = "身份指纹已变化") {
            Text(
                "对端长期身份指纹与上次记录不一致，可能存在中间人风险。",
                style = MaterialTheme.typography.bodySmall,
                color = MaterialTheme.colorScheme.error,
            )
            Button(onClick = { LinkxRuntime.acceptFingerprint() }) { Text("接受新指纹") }
        }
    }

    if (state == LinkxRuntime.STATE_PAIRED || state == LinkxRuntime.STATE_REPAIRED) {
        SectionCard(title = "已配对") {
            KeyValueRow("对端设备", name ?: "—", MaterialTheme.colorScheme.primary)
            KeyValueRow("设备指纹", peerFp ?: "—")
        }
    }

    SectionCard(title = "局域网通道（文件传输）") {
        val tcpBound = remember(refresh) { LinkxRuntime.tcpBound }
        val discovered = remember(refresh) { LinkxRuntime.discoveredIp }
        KeyValueRow(
            "TCP 通道",
            if (tcpBound) "已建立" else "未建立",
            if (tcpBound) Color(0xFF1E9E55) else null,
        )
        KeyValueRow("发现的电脑 IP", discovered ?: "—")
        var manualIp by remember { mutableStateOf(LinkxRuntime.manualPeerIp.orEmpty()) }
        OutlinedTextField(
            value = manualIp,
            onValueChange = { manualIp = it },
            label = { Text("手动输入电脑 IP") },
            singleLine = true,
            modifier = Modifier.fillMaxWidth(),
        )
        Row(horizontalArrangement = Arrangement.spacedBy(12.dp)) {
            Button(onClick = { LinkxRuntime.setManualPeerIp(manualIp) }) { Text("连接") }
            OutlinedButton(
                onClick = {
                    manualIp = ""
                    LinkxRuntime.setManualPeerIp(null)
                },
            ) { Text("自动发现") }
        }
        Text(
            "配对成功后自动在同网段发现电脑并建立 TCP 通道；搜不到时可手动填电脑 IP（跳过 UDP 发现）。",
            style = MaterialTheme.typography.bodySmall,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
        )
        // 连不上必须给出可归因的诊断：只写"未建立"，用户分不清是电脑防火墙没放行还是 App 的问题。
        val diag = remember(refresh) { LinkxRuntime.lanDiagnosis() }
        if (!tcpBound && diag != null) {
            Text(
                diag,
                style = MaterialTheme.typography.bodySmall,
                color = MaterialTheme.colorScheme.error,
            )
        }
    }

    SectionCard {
        Text(
            "首次配对需双端比对 6 位配对码；已信任设备会直接完成。",
            style = MaterialTheme.typography.bodySmall,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
        )
    }
}
