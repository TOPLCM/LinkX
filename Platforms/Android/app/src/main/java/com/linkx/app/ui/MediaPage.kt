/**
 * 媒体页：本机正在播放什么 + 上一首/播放/下一首/音量控制（与 Windows 端同一套图标）。
 *
 * 由原先的 `MainActivity.kt` 拆出（一页一个文件）。
 */

package com.linkx.app.ui

import android.widget.Toast
import androidx.annotation.DrawableRes
import androidx.compose.foundation.background
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.RowScope
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.mutableIntStateOf
import androidx.compose.runtime.remember
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.res.painterResource
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import kotlinx.coroutines.delay
import androidx.compose.runtime.getValue
import androidx.compose.runtime.setValue
import com.linkx.app.MediaControl
import com.linkx.app.NavIconSize
import com.linkx.app.R
import com.linkx.app.SectionCard

// ---------- 页：媒体（本机播放状态 + 与 Windows 同一套图标）----------

@Composable
internal fun MediaPage(refresh: Int) {
    val ctx = LocalContext.current
    // 采样在后台每 3 s 跑一次，页面按 1 s 回看它的缓存：进度条会动，
    // 而 UI 自己不去查 MediaSession（那是重组里的 Binder 调用）。
    var beat by remember { mutableIntStateOf(0) }
    LaunchedEffect(Unit) {
        while (true) {
            delay(1_000)
            beat++
        }
    }
    val now = remember(refresh, beat) { MediaControl.current }

    if (now == null) {
        SectionCard(title = "正在播放") {
            Row(verticalAlignment = Alignment.CenterVertically) {
                Icon(
                    painterResource(R.drawable.ic_music),
                    contentDescription = null,
                    modifier = Modifier.size(NavIconSize),
                    tint = MaterialTheme.colorScheme.onSurfaceVariant,
                )
                Spacer(Modifier.width(10.dp))
                Text(
                    "手机现在没有在播放任何东西",
                    style = MaterialTheme.typography.bodyMedium,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
            }
            Text(
                "打开任意音乐 / 播客 / 视频 App，这里几秒内就会出现曲目信息，" +
                    "电脑上也能看到同一份状态。",
                style = MaterialTheme.typography.bodySmall,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
            )
            val skip = MediaControl.lastSkip
            if (skip.isNotEmpty()) {
                Text(
                    "读不到播放状态：$skip",
                    style = MaterialTheme.typography.bodySmall,
                    color = MaterialTheme.colorScheme.tertiary,
                )
            }
        }
        return
    }

    SectionCard(title = "正在播放") {
        Text(
            now.title.ifEmpty { "（该应用未提供曲目名）" },
            style = MaterialTheme.typography.titleMedium,
            maxLines = 2,
            overflow = TextOverflow.Ellipsis,
        )
        val sub = listOf(now.artist, now.album).filter { it.isNotEmpty() }.joinToString(" · ")
        if (sub.isNotEmpty()) {
            Text(
                sub,
                style = MaterialTheme.typography.bodyMedium,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
            )
        }
        Text(
            (if (now.playing) "播放中" else "已暂停") +
                " · ${fmtMs(now.positionMs)}/${fmtMs(now.durationMs)}" +
                " · 来源 ${now.pkg.ifEmpty { "未知" }}",
            style = MaterialTheme.typography.bodySmall,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
        )
        if (now.durationMs > 0L) {
            Bar((now.positionMs * 100 / now.durationMs).toInt().coerceIn(0, 100))
        }
        // 音量：手机读不到（-1）时不显示数字，也不给 +/- —— 拿 0 当基准会把本机静音
        if (now.volume >= 0) {
            Text(
                "媒体音量 ${now.volume}%",
                style = MaterialTheme.typography.bodySmall,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
            )
            Bar(now.volume.coerceIn(0, 100))
        } else {
            Text(
                "媒体音量 —（当前无活动播放，读到的 0 不代表静音）",
                style = MaterialTheme.typography.bodySmall,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
            )
        }
        val vol = now.volume
        // 音量以"本地累加"为准：采样每 3 s 才刷新一次，连点两下如果都按采样值算，
        // 第二次会发出同一个绝对值 —— 用户看到的就是"按钮点不动"。
        var volLocal by remember(now.volume) { mutableIntStateOf(vol.coerceAtLeast(0)) }
        val feedback: (String) -> Unit = { msg ->
            if (msg.startsWith("已丢弃")) Toast.makeText(ctx, msg, Toast.LENGTH_SHORT).show()
        }
        Row(
            Modifier.fillMaxWidth(),
            horizontalArrangement = Arrangement.spacedBy(4.dp),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            MediaButton(R.drawable.ic_prev, "上一首") {
                feedback(MediaControl.submitCommand(ctx, MediaControl.ACTION_PREV, 0, 0))
            }
            MediaButton(
                if (now.playing) R.drawable.ic_pause else R.drawable.ic_play,
                if (now.playing) "暂停" else "播放",
            ) {
                feedback(MediaControl.submitCommand(ctx, MediaControl.ACTION_PLAY_PAUSE, 0, 0))
            }
            MediaButton(R.drawable.ic_next, "下一首") {
                feedback(MediaControl.submitCommand(ctx, MediaControl.ACTION_NEXT, 0, 0))
            }
            MediaButton(R.drawable.ic_vol_down, "音量减", enabled = vol >= 0) {
                volLocal = (volLocal - 5).coerceIn(0, 100)
                feedback(
                    MediaControl.submitCommand(ctx, MediaControl.ACTION_SET_VOLUME, volLocal, 0)
                )
            }
            MediaButton(R.drawable.ic_vol_up, "音量加", enabled = vol >= 0) {
                volLocal = (volLocal + 5).coerceIn(0, 100)
                feedback(
                    MediaControl.submitCommand(ctx, MediaControl.ACTION_SET_VOLUME, volLocal, 0)
                )
            }
        }
        // 指令的真实结果要看得见：submitCommand 当场只能说"排没排上"，播放器给不给这个能力
        // 是执行那一刻才知道的，而它丢了也不回执。这里下一拍把原因摆出来。
        val cmd = MediaControl.lastCommand
        if (cmd.isNotEmpty()) {
            Text(
                "上一次指令：$cmd",
                style = MaterialTheme.typography.bodySmall,
                color = MaterialTheme.colorScheme.tertiary,
            )
        }
        Text(
            "LinkX 只同步状态与控制指令，不搬运音频；这里点的动作与电脑上的按钮走同一条通道。",
            style = MaterialTheme.typography.bodySmall,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
        )
    }
}

@Composable
private fun RowScope.MediaButton(
    @DrawableRes icon: Int,
    label: String,
    enabled: Boolean = true,
    onClick: () -> Unit,
) {
    Column(horizontalAlignment = Alignment.CenterHorizontally, modifier = Modifier.weight(1f)) {
        IconButton(onClick = onClick, enabled = enabled) {
            Icon(
                painterResource(icon),
                contentDescription = label,
                modifier = Modifier.size(NavIconSize),
            )
        }
        Text(
            label,
            style = MaterialTheme.typography.labelSmall,
            maxLines = 1,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
        )
    }
}

/** 细进度条（与传输记录同一画法，不依赖特定 material3 版本） */
@Composable
private fun Bar(percent: Int) {
    Box(
        Modifier
            .fillMaxWidth()
            .height(6.dp)
            .clip(RoundedCornerShape(3.dp))
            .background(MaterialTheme.colorScheme.outlineVariant),
    ) {
        Box(
            Modifier
                .fillMaxWidth(percent / 100f)
                .height(6.dp)
                .clip(RoundedCornerShape(3.dp))
                .background(MaterialTheme.colorScheme.primary),
        )
    }
}

internal fun fmtMs(ms: Long): String {
    val s = ms.coerceAtLeast(0L) / 1000
    val h = s / 3600
    val m = (s % 3600) / 60
    val sec = s % 60
    return if (h > 0) "%d:%02d:%02d".format(h, m, sec) else "%d:%02d".format(m, sec)
}
