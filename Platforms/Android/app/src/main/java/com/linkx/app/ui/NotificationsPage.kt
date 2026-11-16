/**
 * 通知页：通知读取授权状态 + 已转发通知列表（可整段复制，也可只复制验证码）。
 *
 * 由原先的 `MainActivity.kt` 拆出（一页一个文件）。
 */

package com.linkx.app.ui

import android.content.Intent
import android.provider.Settings
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.material3.Button
import androidx.compose.material3.ButtonDefaults
import androidx.compose.material3.Card
import androidx.compose.material3.CardDefaults
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.remember
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.res.painterResource
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import com.linkx.app.ActionIconSize
import com.linkx.app.EmptyHint
import com.linkx.app.ForwardedNotification
import com.linkx.app.LinkPrereqs
import com.linkx.app.LinkxRuntime
import com.linkx.app.NlsService
import com.linkx.app.R
import com.linkx.app.SectionCard

// ---------- 页：通知 ----------

@Composable
internal fun NotificationsPage(refresh: Int, onCopy: (String) -> Unit) {
    val ctx = LocalContext.current
    // 只读授权项，不用 NotificationManagerCompat.getEnabledListenerPackages：后者 API 30 起才有，
    // 且只回答"授没授权"。授权还在但系统没把服务绑回来，是应用被 ROM 结束后的常态，必须分开显示。
    val authorized = remember(refresh) { NlsService.authorizedComponent(ctx) != null }
    val bound = remember(refresh) { LinkxRuntime.nlsBound }
    val forwarded = remember(refresh) { LinkxRuntime.forwardedRecent() }

    fun openListenerSettings() {
        runCatching {
            ctx.startActivity(
                Intent(Settings.ACTION_NOTIFICATION_LISTENER_SETTINGS)
                    .addFlags(Intent.FLAG_ACTIVITY_NEW_TASK),
            )
        }
    }

    SectionCard(title = "通知读取权限") {
        when {
            !authorized -> {
                Text(
                    "未开启：无法把本机通知转发到电脑",
                    style = MaterialTheme.typography.bodyMedium,
                    color = MaterialTheme.colorScheme.error,
                )
                Button(onClick = { openListenerSettings() }) { Text("前往系统设置开启") }
            }

            bound -> Text(
                "已开启，手机通知会自动同步到电脑",
                style = MaterialTheme.typography.bodyMedium,
                color = Color(0xFF1E9E55),
            )

            else -> {
                Text(
                    "已授权，但系统当前没有把通知交给我们",
                    style = MaterialTheme.typography.bodyMedium,
                    color = MaterialTheme.colorScheme.error,
                )
                Text(
                    "应用被系统结束之后，多数国产 ROM 就不会再绑定通知监听了。" +
                        "允许自启动可以长期解决；眼下先重新开启一次通知使用权即可恢复。",
                    style = MaterialTheme.typography.bodySmall,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
                Row(horizontalArrangement = Arrangement.spacedBy(10.dp)) {
                    Button(onClick = {
                        if (!LinkPrereqs.openAutoStart(ctx)) openListenerSettings()
                    }) { Text("允许自启动") }
                    Button(
                        onClick = { openListenerSettings() },
                        colors = ButtonDefaults.outlinedButtonColors(),
                    ) { Text("重新开启通知使用权") }
                }
            }
        }
    }

    Text(
        "已转发通知（最多 20 条）",
        style = MaterialTheme.typography.labelLarge,
        color = MaterialTheme.colorScheme.onSurfaceVariant,
        modifier = Modifier.padding(start = 4.dp, top = 4.dp),
    )
    if (forwarded.isEmpty()) {
        EmptyHint("暂无记录")
    } else {
        forwarded.forEach { n ->
            NotificationCard(n, onCopy)
        }
    }
}

/** 单条已转发通知（右侧「复制」按钮，正文可整段复制） */
@Composable
internal fun NotificationCard(n: ForwardedNotification, onCopy: (String) -> Unit) {
    Card(
        modifier = Modifier.fillMaxWidth(),
        colors = CardDefaults.cardColors(containerColor = MaterialTheme.colorScheme.surface),
    ) {
        Row(
            Modifier.padding(start = 14.dp, top = 10.dp, end = 6.dp, bottom = 10.dp),
            verticalAlignment = Alignment.Top,
        ) {
            Column(
                Modifier.weight(1f),
                verticalArrangement = Arrangement.spacedBy(3.dp),
            ) {
                Text(
                    n.title.ifEmpty { n.pkg },
                    style = MaterialTheme.typography.titleSmall,
                    maxLines = 1,
                    overflow = TextOverflow.Ellipsis,
                )
                Text(
                    n.pkg,
                    style = MaterialTheme.typography.labelSmall,
                    color = MaterialTheme.colorScheme.primary,
                    maxLines = 1,
                    overflow = TextOverflow.Ellipsis,
                )
                Text(
                    n.text.ifEmpty { "（无正文）" },
                    style = MaterialTheme.typography.bodyMedium,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                    maxLines = 3,
                    overflow = TextOverflow.Ellipsis,
                )
            }
            // 抽得出验证码时给一个"只复制码"的入口：短信通知整段粘过去还得手动删掉
            // "请勿泄露"，而用户要的往往就是那 6 个数字。
            val code = remember(n.title, n.text) { LinkxRuntime.noticeCode(n.title, n.text) }
            if (code != null) {
                TextButton(onClick = { onCopy(code) }) {
                    Text("复制验证码 $code", style = MaterialTheme.typography.labelMedium)
                }
            }
            IconButton(onClick = { onCopy(n.text.ifEmpty { n.title }) }) {
                Icon(
                    painterResource(R.drawable.ic_copy),
                    contentDescription = "复制正文",
                    modifier = Modifier.size(ActionIconSize),
                    tint = MaterialTheme.colorScheme.primary,
                )
            }
        }
    }
}
