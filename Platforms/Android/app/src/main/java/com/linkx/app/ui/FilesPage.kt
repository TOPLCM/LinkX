/**
 * 文件页：SAF / 相册多选待发清单、接收目录选择、传输记录（可取消、可打开或分享）。
 *
 * 由原先的 `MainActivity.kt` 拆出（一页一个文件）。
 */

package com.linkx.app.ui

import android.content.Context
import android.content.Intent
import android.net.Uri
import android.provider.MediaStore
import android.util.Log
import android.webkit.MimeTypeMap
import android.widget.Toast
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.result.PickVisualMediaRequest
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.foundation.background
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.Button
import androidx.compose.material3.Card
import androidx.compose.material3.CardDefaults
import androidx.compose.material3.Icon
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.res.painterResource
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import androidx.core.content.FileProvider
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import java.io.File
import androidx.compose.runtime.getValue
import androidx.compose.runtime.setValue
import com.linkx.app.ActionIconSize
import com.linkx.app.CANCEL_REASON
import com.linkx.app.EmptyHint
import com.linkx.app.FileBrief
import com.linkx.app.KeyValueRow
import com.linkx.app.LinkxRuntime
import com.linkx.app.R
import com.linkx.app.SectionCard
import com.linkx.app.TAG
import com.linkx.app.TransferItem
import com.linkx.app.TransferState
import com.linkx.app.shareGrantNote
import com.linkx.app.sharedInbox

// ---------- 页：文件（SAF/相册多选 + 加密通道传输）----------

/**
 * 待发清单里一条的状态。
 *
 * `Handed` = 内容已经交给链路，**不等于**电脑已收下：回执要等「传输记录」里的行说话，
 * 这里只说"发出去了"，不替对端宣布成功。
 */
private enum class PendingState { Waiting, Sending, Handed, Done, Failed }

/** 待发清单里的一条：Uri + 展示用名称/大小 + 这一条自己当前的状态与原因。 */
private data class PendingItem(
    val uri: Uri,
    val brief: FileBrief,
    val state: PendingState = PendingState.Waiting,
    val reason: String = "",
)

/** 清单里某一条在界面上的说法：失败必须带一句原因，不让用户自己猜是链路断了还是没配对。 */
private fun pendingStateText(item: PendingItem): String = when (item.state) {
    PendingState.Waiting -> "待发送"
    PendingState.Sending -> "发送中…"
    PendingState.Handed -> item.reason.ifEmpty { "已交链路，等电脑回执" }
    PendingState.Done -> item.reason.ifEmpty { "电脑已确认收完" }
    PendingState.Failed -> item.reason.ifEmpty { "发送失败（原因未说明）" }
}

/** 把一批 Uri 并进待发清单：同一个 Uri 不重复进（用户可能连着选两批重叠的文件）。 */
private fun mergePending(
    current: List<PendingItem>,
    uris: List<Uri>,
    briefOf: (Uri) -> FileBrief,
): List<PendingItem> {
    if (uris.isEmpty()) return current
    val seen = current.map { it.uri }.toSet()
    val add = uris.filter { it !in seen }.map { PendingItem(it, briefOf(it)) }
    return if (add.isEmpty()) current else current + add
}

/**
 * 就地改某一条的状态：按 Uri 定位而不是按下标，
 * 用户在发送途中增删条目也不会把状态写到别的文件头上。
 */
private fun withPendingState(
    current: List<PendingItem>,
    uri: Uri,
    state: PendingState,
    reason: String = "",
): List<PendingItem> = current.map { if (it.uri == uri) it.copy(state = state, reason = reason) else it }

@Composable
internal fun FilesPage(refresh: Int) {
    val ctx = LocalContext.current
    val scope = rememberCoroutineScope()
    // 待发清单：SAF 多选、相册多选、别的 App 分享进来的都攒在这里，攒好后整批顺序发出
    var pending by remember { mutableStateOf<List<PendingItem>>(emptyList()) }
    val transfers = remember(refresh) { LinkxRuntime.transfers() }
    // 接收目录：dirTick 让"用户刚改完目录"能立刻反映到界面，不必等下一次状态轮询
    var dirTick by remember { mutableStateOf(0) }
    val recvDir = remember(refresh, dirTick) { LinkxRuntime.receiveDir() }
    val dirCustom = remember(refresh, dirTick) { LinkxRuntime.receiveDirIsCustom() }
    val dirPicker = rememberLauncherForActivityResult(ActivityResultContracts.OpenDocumentTree()) { uri ->
        if (uri == null) return@rememberLauncherForActivityResult
        runCatching { LinkxRuntime.setReceiveDir(uri) }
            .onSuccess {
                dirTick++
                Toast.makeText(ctx, "以后收到的文件都存进「$it」", Toast.LENGTH_LONG).show()
            }
            .onFailure { Toast.makeText(ctx, "改目录失败：${it.message}", Toast.LENGTH_LONG).show() }
    }
    // 其它 App「分享到 LinkX」带进来的：进页即取走，**整批**并进待发清单（不只取第一条）
    val shared = remember { sharedInbox.toList().also { sharedInbox.clear() } }
    LaunchedEffect(shared) {
        if (shared.isNotEmpty()) pending = mergePending(pending, shared) { fileBriefOf(ctx, it) }
    }

    // SAF 多选：无需任何存储权限；持久化读权限以便后台线程再次打开同一 Uri（只选一个也在这个入口里）
    val picker = rememberLauncherForActivityResult(ActivityResultContracts.OpenMultipleDocuments()) { uris ->
        if (uris.isEmpty()) return@rememberLauncherForActivityResult
        uris.forEach { u ->
            runCatching {
                ctx.contentResolver.takePersistableUriPermission(
                    u,
                    Intent.FLAG_GRANT_READ_URI_PERMISSION,
                )
            }
        }
        pending = mergePending(pending, uris) { fileBriefOf(ctx, it) }
    }

    // 相册多选：Photo Picker 不需要任何存储权限。没有 Photo Picker 的旧 ROM 由**契约自己**回落
    //（退到系统文件选择器并允许多选），这里不自己判版本。
    // Photo Picker 给的授权不可持久化（也不该持久化），所以这些 Uri 只在这一页有效。
    val albumPicker = rememberLauncherForActivityResult(ActivityResultContracts.PickMultipleVisualMedia()) { uris ->
        if (uris.isEmpty()) return@rememberLauncherForActivityResult
        pending = mergePending(pending, uris) { photoBriefOf(ctx, it) }
    }

    SectionCard(title = "发送到电脑") {
        if (pending.isEmpty()) {
            EmptyHint("未选择文件")
        } else {
            val total = pending.sumOf { it.brief.size.coerceAtLeast(0L) }
            KeyValueRow("待发清单", "${pending.size} 个文件 · 合计 ${formatSize(total)}")
            pending.forEach { item ->
                PendingRow(
                    item,
                    onRemove = { pending = pending.filter { it.uri != item.uri } },
                )
            }
            TextButton(onClick = { pending = emptyList() }) { Text("清空清单") }
        }
        Row(horizontalArrangement = Arrangement.spacedBy(12.dp)) {
            OutlinedButton(onClick = { picker.launch(arrayOf("*/*")) }) {
                Icon(
                    painterResource(R.drawable.ic_upload),
                    contentDescription = null,
                    modifier = Modifier.size(ActionIconSize),
                )
                Spacer(Modifier.width(6.dp))
                Text("选择文件")
            }
            OutlinedButton(
                onClick = {
                    albumPicker.launch(PickVisualMediaRequest(ActivityResultContracts.PickVisualMedia.ImageOnly))
                },
            ) {
                Icon(
                    painterResource(R.drawable.ic_album),
                    contentDescription = null,
                    modifier = Modifier.size(ActionIconSize),
                )
                Spacer(Modifier.width(6.dp))
                Text("从相册选图")
            }
        }
        Button(
            enabled = pending.isNotEmpty(),
            onClick = {
                val batch = pending
                // 整批顺序发出：一个失败不打断其余，失败原因逐条回写到清单里。
                // 每个 sendFile 都是阻塞式的（摘要 + 分块 + 等回执），所以必须顺序调用，不能并发压链路。
                scope.launch(Dispatchers.IO) {
                    for (item in batch) {
                        withContext(Dispatchers.Main) {
                            pending = withPendingState(pending, item.uri, PendingState.Sending)
                        }
                        val before = LinkxRuntime.transfers().map { it.fileId }.toSet()
                        val thrown = runCatching { LinkxRuntime.sendFile(item.uri) }.exceptionOrNull()
                        if (thrown != null) {
                            val why = "发送出错：${thrown.message ?: thrown.javaClass.simpleName}"
                            Log.w(TAG, "发送协程里抛出：$why", thrown)
                            withContext(Dispatchers.Main) {
                                pending = withPendingState(pending, item.uri, PendingState.Failed, why)
                            }
                            continue
                        }
                        // sendFile 内部的失败（未配对 / 通道未就绪 / 读不到文件）**不抛异常**，
                        // 只落一条失败的任务行：据这条新行如实回报，不假装成功。
                        val row = LinkxRuntime.transfers().firstOrNull { it.fileId !in before && it.outgoing }
                        val state: PendingState
                        val reason: String
                        when {
                            row == null -> {
                                state = PendingState.Handed
                                reason = "已交链路，等电脑回执"
                            }
                            row.state == TransferState.Failed -> {
                                state = PendingState.Failed
                                reason = row.error.ifEmpty { "发送失败（原因未说明）" }
                            }
                            row.state == TransferState.Done -> {
                                state = PendingState.Done
                                reason = "电脑已确认收完"
                            }
                            else -> {
                                state = PendingState.Handed
                                reason = "已交链路，等电脑回执"
                            }
                        }
                        withContext(Dispatchers.Main) {
                            pending = withPendingState(pending, item.uri, state, reason)
                        }
                    }
                }
            },
        ) {
            Icon(
                painterResource(R.drawable.ic_send),
                contentDescription = null,
                modifier = Modifier.size(ActionIconSize),
            )
            Spacer(Modifier.width(6.dp))
            Text("发送到电脑")
        }
        if (shared.isNotEmpty()) {
            Text(
                "分享进来 ${shared.size} 个文件（进页即加入待发清单，和上面的文件一起发）：",
                style = MaterialTheme.typography.bodySmall,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
            )
            shared.forEach { u ->
                val inList = pending.any { it.uri == u }
                Row(verticalAlignment = Alignment.CenterVertically) {
                    Text(
                        u.lastPathSegment ?: "文件",
                        style = MaterialTheme.typography.bodySmall,
                        color = MaterialTheme.colorScheme.onSurfaceVariant,
                        maxLines = 1,
                        overflow = TextOverflow.Ellipsis,
                        modifier = Modifier.weight(1f),
                    )
                    if (inList) {
                        Text(
                            "已在清单",
                            style = MaterialTheme.typography.bodySmall,
                            color = MaterialTheme.colorScheme.onSurfaceVariant,
                        )
                    } else {
                        TextButton(onClick = { pending = mergePending(pending, listOf(u)) { fileBriefOf(ctx, it) } }) {
                            Text("加入清单")
                        }
                    }
                }
            }
        }
        if (shareGrantNote.isNotEmpty()) {
            Text(
                shareGrantNote,
                style = MaterialTheme.typography.bodySmall,
                color = MaterialTheme.colorScheme.tertiary,
            )
        }
        Text(
            "文件经局域网加密通道传输：256KB 分块 · 每块 CRC32 · 整文件 SHA-256 · 断点续传。",
            style = MaterialTheme.typography.bodySmall,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
        )
    }

    SectionCard(title = "从电脑收文件") {
        Text(
            "电脑上打开 LinkX 的「文件」页，选好文件点「发送到手机」即可；" +
                "手机这边自动接收，进度落在下面的传输记录里。",
            style = MaterialTheme.typography.bodySmall,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
        )
        KeyValueRow("接收目录", recvDir.ifEmpty { "—" })
        Row(horizontalArrangement = Arrangement.spacedBy(8.dp), verticalAlignment = Alignment.CenterVertically) {
            OutlinedButton(onClick = { dirPicker.launch(null) }) { Text("改到别的文件夹…") }
            if (dirCustom) {
                TextButton(onClick = {
                    LinkxRuntime.clearReceiveDir()
                    dirTick++
                }) { Text("改回默认") }
            }
        }
        Text(
            if (dirCustom) {
                "改目录只影响**之后**收到的文件；已经收下的仍留在原处。"
            } else {
                "默认存在应用私有目录里，相册和文件管理器都看不到它。" +
                    "改到「文档」这类文件夹后就能直接找到（授权是持久的，重启也认）。" +
                        "部分机型不让把「下载」「相册」授权给第三方应用，换一个目录即可。"
            },
            style = MaterialTheme.typography.bodySmall,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
        )
    }

    Text(
        "传输记录",
        style = MaterialTheme.typography.labelLarge,
        color = MaterialTheme.colorScheme.onSurfaceVariant,
        modifier = Modifier.padding(start = 4.dp, top = 4.dp),
    )
    if (transfers.isEmpty()) {
        EmptyHint("暂无传输")
    } else {
        transfers.forEach { TransferRow(it) }
    }
}

/** SAF Uri 的名称/大小；拿不到元数据时退化成"用路径末段 + 未知大小"，不让界面空着 */
internal fun fileBriefOf(ctx: Context, uri: Uri): FileBrief =
    LinkxRuntime.fileBrief(uri) ?: FileBrief(uri.lastPathSegment ?: "文件", -1L)

/** 待发清单里的一条：文件名 + 大小 + 当前状态（失败带一句原因）+ 移除按钮。 */
@Composable
private fun PendingRow(item: PendingItem, onRemove: () -> Unit) {
    Row(
        modifier = Modifier.fillMaxWidth(),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Column(Modifier.weight(1f)) {
            Text(
                item.brief.name,
                style = MaterialTheme.typography.bodyMedium,
                maxLines = 1,
                overflow = TextOverflow.Ellipsis,
            )
            Text(
                "${formatSize(item.brief.size)} · ${pendingStateText(item)}",
                style = MaterialTheme.typography.bodySmall,
                color = if (item.state == PendingState.Failed) {
                    MaterialTheme.colorScheme.error
                } else {
                    MaterialTheme.colorScheme.onSurfaceVariant
                },
            )
        }
        TextButton(onClick = onRemove) { Text("移除") }
    }
}

/**
 * 相册图片的展示名与大小。
 *
 * Photo Picker 给的 Uri 不需要任何存储权限，但不一定带 `OpenableColumns` 的名字与大小：
 * 查得到就直接用；查不到（或名字其实是一串媒体 id）就按 Uri 末段的 id 回 MediaStore 兜底。
 * 这条回落要看相册权限，没有权限时如实显示为未知大小，而不是编一个像样的假名字。
 */
internal fun photoBriefOf(ctx: Context, uri: Uri): FileBrief {
    val brief = LinkxRuntime.fileBrief(uri)
    // 名字是一串数字 = 拿到的其实是媒体 id 而不是文件名，这种情况要走下面的 MediaStore 兜底
    val nameIsId = brief != null && brief.name.trim().toLongOrNull() != null
    if (brief != null && brief.size >= 0L && !nameIsId) return brief
    val id = uri.lastPathSegment?.toLongOrNull() ?: -1L
    // 只对**相册类** Uri 回 MediaStore 查：文档 Uri 的末段也可能是一串数字，
    // 拿它当媒体 id 去查会捡到另一个文件的名字（张冠李戴比显示未知更糟）。
    val looksLikeMedia = uri.path?.contains("/media/") == true ||
        uri.authority?.contains("photopicker") == true
    if (id > 0L && looksLikeMedia) {
        val fromStore = runCatching {
            ctx.contentResolver.query(
                MediaStore.Files.getContentUri("external"),
                arrayOf(MediaStore.Files.FileColumns.DISPLAY_NAME, MediaStore.Files.FileColumns.SIZE),
                "${MediaStore.Files.FileColumns._ID} = ?",
                arrayOf(id.toString()),
                null,
            )?.use { c ->
                if (!c.moveToFirst()) {
                    null
                } else {
                    val name = if (c.isNull(0)) null else c.getString(0)
                    val size = if (c.isNull(1)) -1L else c.getLong(1)
                    FileBrief(name ?: "图片", size)
                }
            }
        }.getOrNull()
        if (fromStore != null) return fromStore
    }
    return brief ?: FileBrief(uri.lastPathSegment ?: "图片", -1L)
}

/** 单条传输（名称 / 方向 / 进度条 / 状态 / 失败或取消的原因 / 在途行可取消 / 收到的文件可打开或分享） */
@Composable
private fun TransferRow(t: TransferItem) {
    val ctx = LocalContext.current
    val percent = if (t.size > 0L) {
        ((t.bytes.coerceAtLeast(0L) * 100) / t.size).toInt().coerceIn(0, 100)
    } else {
        0
    }
    val color = when (t.state) {
        TransferState.Done -> Color(0xFF1E9E55)
        TransferState.Failed -> MaterialTheme.colorScheme.error
        TransferState.Running -> MaterialTheme.colorScheme.primary
        // 分块发完了但对端还没回执：用次要色区分，别让用户误以为已经成功
        TransferState.AwaitingPeer -> MaterialTheme.colorScheme.tertiary
        // 等回执超时：不是失败也不是成功，用告警色说清楚
        TransferState.SentUnconfirmed -> MaterialTheme.colorScheme.error.copy(alpha = 0.75f)
        // 取消既不是成功也不是故障：用中性色，不给告警色（用户做对的事不该被标红）
        TransferState.Cancelled -> MaterialTheme.colorScheme.onSurfaceVariant
    }
    Card(
        modifier = Modifier.fillMaxWidth(),
        colors = CardDefaults.cardColors(containerColor = MaterialTheme.colorScheme.surface),
    ) {
        Column(
            Modifier.fillMaxWidth().padding(14.dp),
            verticalArrangement = Arrangement.spacedBy(6.dp),
        ) {
            Row(Modifier.fillMaxWidth(), horizontalArrangement = Arrangement.SpaceBetween) {
                Text(
                    t.name,
                    style = MaterialTheme.typography.titleSmall,
                    maxLines = 1,
                    overflow = TextOverflow.Ellipsis,
                    modifier = Modifier.weight(1f),
                )
                Row(verticalAlignment = Alignment.CenterVertically) {
                    Icon(
                        painterResource(if (t.outgoing) R.drawable.ic_upload else R.drawable.ic_download),
                        contentDescription = if (t.outgoing) "发送" else "接收",
                        modifier = Modifier.size(ActionIconSize),
                        tint = color,
                    )
                    Spacer(Modifier.width(4.dp))
                    Text(
                        if (t.outgoing) "发送" else "接收",
                        style = MaterialTheme.typography.labelSmall,
                        color = MaterialTheme.colorScheme.primary,
                    )
                }
            }
            // 进度条：两层 Box 绘制，避免依赖特定 material3 进度条 API 版本
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
                        .background(color),
                )
            }
            Text(
                "$percent% · ${transferStateLabel(t.state)} · " +
                    "${formatSize(t.bytes)}/${formatSize(t.size)}",
                style = MaterialTheme.typography.bodySmall,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
            )
            // 失败与取消都要把原因说出来：只标一个状态等于让用户自己去猜是链路断了还是没配对
            val note = t.error.ifEmpty {
                if (t.state == TransferState.Failed || t.state == TransferState.Cancelled) {
                    "未说明原因（在「设置」里打开 Debug 模式后可导出日志细查）"
                } else {
                    // 已完成但有一句话要说（例如"没能存进你选的目录，先留在私有目录"）
                    t.note
                }
            }
            if (note.isNotEmpty()) {
                Text(
                    note,
                    style = MaterialTheme.typography.bodySmall,
                    // 取消的原因不是故障，不给告警色；只有真失败才标红
                    color = if (t.state == TransferState.Failed) MaterialTheme.colorScheme.error
                    else MaterialTheme.colorScheme.onSurfaceVariant,
                )
            }
            // 「取消」只画在点下去真的会停的行上：判据取自 `LinkxRuntime.fileCancellable`，
            // 与取消命令入口是同一个函数，不在这里另判一遍（等回执行、终态行都停不下来）
            if (LinkxRuntime.fileCancellable(t)) {
                TextButton(onClick = {
                    val why = if (t.outgoing) {
                        LinkxRuntime.cancelFileSend(t.fileId, CANCEL_REASON)
                    } else {
                        LinkxRuntime.cancelFileRecv(t.fileId, CANCEL_REASON)
                    }
                    // 返回非空 = 这一下什么都没取消，必须当场把原因讲给用户
                    if (why != null) {
                        Toast.makeText(ctx, why, Toast.LENGTH_LONG).show()
                    }
                }) { Text("取消") }
            }
            if (!t.outgoing && t.state == TransferState.Done && t.path.isNotEmpty()) {
                Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                    TextButton(onClick = { openReceived(ctx, t.path, t.name) }) { Text("打开") }
                    TextButton(onClick = { shareReceived(ctx, t.path, t.name) }) { Text("分享") }
                }
            }
        }
    }
}

/**
 * 收到的文件那份"能打开/能分享"的 Uri。
 *
 * 两种来源：应用私有目录里的绝对路径（必须经 FileProvider，直接给 file:// 系统会拒绝），
 * 以及用户自选目录里的那一份（本身就是 `content://` 文档 Uri，直接可用——
 * FileProvider 反而处理不了它，它不在我们的授权路径里）。
 */
internal fun receivedUri(ctx: Context, path: String): Uri = if (path.startsWith("content://")) {
    Uri.parse(path)
} else {
    FileProvider.getUriForFile(ctx, "${ctx.packageName}.fileprovider", File(path))
}

/** 收到的文件交给系统「打开」（名字用传输记录里的那份：content 路径看不出文件名） */
internal fun openReceived(ctx: Context, path: String, displayName: String) {
    val name = displayName.ifEmpty { File(path).name }
    runCatching {
        val intent = Intent(Intent.ACTION_VIEW).apply {
            setDataAndType(receivedUri(ctx, path), mimeOf(name))
            addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION)
        }
        ctx.startActivity(Intent.createChooser(intent, "打开 $name"))
    }.onFailure {
        // 只写日志等于"按钮是死的"：这是用户直接点出来的动作，必须当场回话
        Log.w(TAG, "打不开 $name：${it.message}")
        Toast.makeText(ctx, "打不开 $name：${it.message ?: "系统里没有能打开它的应用"}", Toast.LENGTH_LONG).show()
    }
}

/** 收到的文件分享出去（再发给别的 App 或另存） */
internal fun shareReceived(ctx: Context, path: String, displayName: String) {
    val name = displayName.ifEmpty { File(path).name }
    runCatching {
        val intent = Intent(Intent.ACTION_SEND).apply {
            type = mimeOf(name)
            putExtra(Intent.EXTRA_STREAM, receivedUri(ctx, path))
            addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION)
        }
        ctx.startActivity(Intent.createChooser(intent, "分享 $name"))
    }.onFailure {
        Log.w(TAG, "分享不出去 $name：${it.message}")
        Toast.makeText(ctx, "分享不出去 $name：${it.message ?: "没有接收分享的应用"}", Toast.LENGTH_LONG).show()
    }
}

/**
 * 按扩展名猜 MIME：不能用 `getFileExtensionFromUrl`（它是 URL 工具，把 `#`/`?` 之后当片段丢掉，
 * 而收到的文件名常带这些字符）。兜底 `application/octet-stream`，不用通配类型——
 * 通配作为 intent 类型匹配不到任何 `IntentFilter`，只会换来 ActivityNotFoundException。
 */
internal fun mimeOf(path: String): String =
    MimeTypeMap.getSingleton()
        .getMimeTypeFromExtension(File(path).extension.lowercase())
        ?: "application/octet-stream"

internal fun transferStateLabel(state: TransferState): String = when (state) {
    TransferState.Running -> "传输中"
    TransferState.AwaitingPeer -> "等待电脑确认"
    TransferState.SentUnconfirmed -> "已发送（未确认）"
    TransferState.Done -> "已完成"
    TransferState.Failed -> "失败"
    // 用户主动动作：文案不许写成失败，也不写"已中止"这种含糊词
    TransferState.Cancelled -> "已取消"
}

/** 人类可读大小（负数 = 未知，显示 —） */
internal fun formatSize(bytes: Long): String {
    if (bytes < 0L) return "—"
    if (bytes < 1024L) return "$bytes B"
    val kb = bytes / 1024.0
    if (kb < 1024.0) return "%.1f KB".format(kb)
    val mb = kb / 1024.0
    if (mb < 1024.0) return "%.1f MB".format(mb)
    return "%.2f GB".format(mb / 1024.0)
}
