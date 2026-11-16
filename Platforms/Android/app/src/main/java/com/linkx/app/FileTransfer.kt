/**
 * 文件传输：
 * - 发送：读 SAF Uri → 立刻 FILE_META（不带摘要）→ 逐 256KB 分块（每块 CRC32，摘要同步增量算）→ FILE_DONE 交付整文件 SHA-256；
 * - 接收：按 FILE_META 落盘，`index * chunk_size` 随机写，逐块校验 CRC32，空洞回 MSG_RESUME 续传；收尾用本端增量摘要比对（校验源优先 FILE_DONE，缺位才回落 FILE_META）。
 * 磁盘 IO 全部在独立守护线程（[worker]）串行执行：pump 锁内只投递任务，不阻塞事件分发。
 *
 * 由原先的 `LinkxRuntime.kt` 拆出（原文件里它是第四个顶层 object，约 1100 行）。
 */

package com.linkx.app

import android.content.Intent
import android.net.Uri
import android.os.SystemClock
import android.os.StatFs
import android.provider.DocumentsContract
import android.provider.OpenableColumns
import android.util.Log
import java.io.File
import java.io.InputStream
import java.io.RandomAccessFile
import java.net.URLDecoder
import java.security.MessageDigest
import java.security.SecureRandom
import java.util.ArrayDeque
import java.util.ArrayList
import java.util.concurrent.LinkedBlockingQueue
import java.util.zip.CRC32

internal object FileTransfer {
    private const val TAG = "LinkX.FileTransfer"
    private const val CHUNK_SIZE = 256 * 1024

    /** 单个接收文件的上限：`size` 是**对端声明的**，不能由它决定本机开多大的文件。4 GiB 对手机相册与常见文档有足够余量，同时挡住"声明一个天文数字，然后慢慢写满你"；与电脑端 `MAX_RECV_FILE_BYTES` 同一常量口径。 */
    private const val MAX_RECV_FILE_BYTES = 4L * 1024 * 1024 * 1024
    private const val DIR_NAME = "LinkX"

    /** 用户自选接收目录（SAF tree Uri + 可读名）的持久化键；空 = 用默认私有目录。 */
    private const val KEY_TREE_URI = "receive_tree_uri"
    private const val KEY_TREE_LABEL = "receive_tree_label"

    /** 落地文件名的 UTF-8 字节上限：留足余量给重名时追加的 " (n)"（ext4 单段 255 B）。 */
    private const val MAX_NAME_BYTES = 200

    /** 已取消 id 的登记上限（与引擎同口径的兜底：一次会话不会取消几十次传输）。 */
    private const val CANCELLED_IDS_MAX = 32

    private val lock = Any()
    private val items = ArrayList<TransferItem>()

    private val recv = HashMap<Long, RecvState>()
    private val sends = HashMap<Long, SendState>()

    /**
     * 已取消的 `file_id`（FIFO 淘汰）。取消帧在事件泵里就地处理，而它的文件头还排在 worker 队列后面：
     * 没有这份登记，取消之后仍会落出一个「传输中」的行和一个没人要的半截文件。
     */
    private val cancelledIds = ArrayDeque<Long>()

    private val queue = LinkedBlockingQueue<() -> Unit>()
    @Volatile private var worker: Thread? = null

    private class SendState(
        val uri: Uri,
        val name: String,
        val size: Long,
        val chunkSize: Int,
        /** 非 null = 长度未知、已预扫得到整文件摘要（旧口径，摘要同时进 FILE_META） */
        val declaredSha: ByteArray?,
        /** 非 0 = 这条发送是对 `ALBUM_FULL_REQ` 的相册原图应答（值是照片 id）。 */
        val albumId: Long = 0L,
    ) {
        /** 整文件摘要累计器：只由 [sendChunks] 所在线程读写，分块真的发出去才喂一段，所以既不用预扫整文件，也不会把文件驻留内存。 */
        val digest: MessageDigest = MessageDigest.getInstance("SHA-256")

        /** 已喂进摘要的连续字节数（收尾时须等于 size，否则摘要不完整）。 */
        var hashedBytes: Long = 0

        /** 收端请求的续传起点（非 null 时发送循环会就地跳转）。 */
        @Volatile var resumeFrom: Int? = null

        /** 等待对端 FILE_DONE 回执的截止时刻（0 = 分块尚未发完，不计时）。 */
        @Volatile var ackDeadlineMs: Long = 0

        /** 对端已经把这条文件收尾（回执 ok=false）：补发途中收到它 = 再发下去只会产出孤儿块，发送循环与回执窗口都据此立刻停手。 */
        @Volatile var peerRejected: Boolean = false

        /** 用户已取消：发送循环据此停手——不补第二条 FILE_DONE，也不落「失败」。 */
        @Volatile var cancelled: Boolean = false

        /** 发送循环还在不在：有了它，`noteResume` 才能分清"记一笔等下一轮生效"和"没有人会再读了"——后者被写成前者就是一个看似处理了的静默分支。 */
        @Volatile var loopAlive: Boolean = false

        /** 第一条 `FILE_DONE` 已出口、正在等对端回执。与 [loopAlive] 一起构成"有人会读 `resumeFrom`"的判据：回执窗口里对端可能正好清点出自己的洞并发来续传请求，这一刻补发还来得及。 */
        @Volatile var awaitingAck: Boolean = false

        /** 已经为对端的洞重启过几轮补发（上限 `MAX_RESUME_TRIES`，与电脑端同口径）。 */
        @Volatile var resumeRounds: Int = 0
    }

    private class RecvState(
        val fileId: Long,
        val name: String,
        val size: Long,
        val chunkSize: Int,
        val path: String,
        val file: RandomAccessFile,
        /** FILE_META 声明的整文件 SHA-256；null = 发端流式计算，摘要改由 FILE_DONE 交付。 */
        val expectSha256: ByteArray?,
        var nextIndex: Int = 0,
        var received: Long = 0,
        /** 已经请求过、还没看到进展的续传起点（去重用，见 `askResume`） */
        var pendingResume: Int? = null,
        /** 已发出的**不同**续传轮次（上限 `MAX_RESUME_TRIES`，与电脑端同口径） */
        var resumeTries: Int = 0,
        /** 收到 FILE_DONE 但本端有洞时的暂缓收尾截止时刻（0 = 不在暂缓，见 `holdForResume`） */
        var resumeHoldMs: Long = 0L,
    ) {
        /** 本端增量摘要：落盘的每块都喂进来，收尾不再重读整个文件。 */
        val digest: MessageDigest = MessageDigest.getInstance("SHA-256")
    }

    // ---------- UI 可见 ----------

    fun transfers(): List<TransferItem> = synchronized(lock) { items.toList() }

    /** 按 id 取一条任务行（取消命令入口要用它先过 [cancellable] 这道判据）。 */
    fun itemOf(fileId: Long): TransferItem? = synchronized(lock) { items.firstOrNull { it.fileId == fileId } }

    /**
     * 这一行**点下去真的会停**吗：状态还在传输中、且本机确实还握着这条在途会话。文件页画「取消」与
     * [LinkxRuntime.cancelFileSend] / [cancelFileRecv] 受理取消都只问这一个函数——判据分两处各写一遍，
     * 就会出现"看得见点不到"或"点了什么都没发生"。等对端回执（`AwaitingPeer`）与各种终态都不算：分块已经交出去了，这里停不下来。
     */
    fun cancellable(item: TransferItem): Boolean =
        item.state == TransferState.Running && synchronized(lock) {
            if (item.outgoing) sends.containsKey(item.fileId) else recv.containsKey(item.fileId)
        }

    /** 控制面取证：`名称#fileId=状态` 列表（取消命令按 id 下发，这里给出可直接复制的 id）。 */
    fun debugRows(): String = synchronized(lock) {
        items.take(8).joinToString(",") { "${it.name}#${it.fileId}=${it.state.name}" }
    }

    /**
     * 接收目录的**界面显示值**：用户选过的就给他选的那个（可读名），没选过就是应用私有路径。
     * 这与"文件先写到哪"是两件事：落盘永远先写私有目录（见 [workDir]），收完再复制进所选目录——
     * 续传与补空洞要 `RandomAccessFile.seek`，而 SAF 的文档流只能顺序写，直接往所选目录写会让整套续传语义失效。
     */
    fun receiveDir(): String {
        loadTree()
        return if (treeUri.isNotEmpty()) treeLabel.ifEmpty { treeUri } else workDir().absolutePath
    }

    /** 用户所选目录的 tree Uri（空 = 没选过，用默认私有目录）。给界面与调试面读。 */
    fun treeUri(): String {
        loadTree()
        return treeUri
    }

    /** 工作目录（应用外部私有目录 `…/files/LinkX/`，任何 API 级都无需权限）。 */
    private fun workDir(): File {
        val base = LinkxRuntime.requireContext().getExternalFilesDir(null) ?: File("/data/local/tmp")
        val dir = File(base, DIR_NAME)
        if (!dir.exists()) dir.mkdirs()
        return dir
    }

    // ---------- 用户自选接收目录：SAF tree + 持久授权 ----------

    @Volatile private var treeUri = ""
    @Volatile private var treeLabel = ""
    @Volatile private var treeLoaded = false

    /**
     * 读一次持久化的所选目录。只在真正用到时读（`LinkxRuntime.init` 之前 prefs 是 null）。
     * 顺手验一遍授权还在不在：用户在系统设置里撤了 SAF 授权时，读到的 Uri 会写出全部失败，
     * 与其每次收文件都报一句"存不进去"，不如这里就当没选过（界面回到默认目录，说的是真话）。
     * **但只改内存，不动磁盘**：这是读路径，"当前查不到授权"不等于"用户没选过"（进程刚起、
     * 授权表还没同步、系统临时抽风都会查不到）。在这里把设置删掉 = 一次瞬时失败就把用户的选择抹了。
     */
    private fun loadTree() {
        if (treeLoaded) return
        val p = LinkxRuntime.prefsStore() ?: return
        val saved = p.getString(KEY_TREE_URI, "").orEmpty()
        treeLabel = p.getString(KEY_TREE_LABEL, "").orEmpty()
        treeUri = if (saved.isEmpty() || persistableGranted(saved)) saved else {
            Log.i(TAG, "所选接收目录的授权当前查不到，本次按未选择处理（设置里仍保留）：$saved")
            ""
        }
        treeLoaded = true
    }

    /** 这个 tree Uri 现在是否真的可写（持久授权还在）。 */
    private fun persistableGranted(uriText: String): Boolean {
        val uri = runCatching { Uri.parse(uriText) }.getOrNull() ?: return false
        return LinkxRuntime.requireContext().contentResolver.persistedUriPermissions.any {
            it.uri == uri && it.isWritePermission && it.isReadPermission
        }
    }

    /** 记住用户选的接收目录，并把它升级成**跨重启有效**的读写授权，返回界面可读名。不申请 `MANAGE_EXTERNAL_STORAGE`（"所有文件访问"）：那是给文件管理器用的重权限，一个局域网传图工具要它，用户只会更不放心。 */
    fun setReceiveTree(uri: Uri): String {
        val ctx = LinkxRuntime.requireContext()
        val flags = Intent.FLAG_GRANT_READ_URI_PERMISSION or Intent.FLAG_GRANT_WRITE_URI_PERMISSION
        val granted = runCatching { ctx.contentResolver.takePersistableUriPermission(uri, flags) }
            .isSuccess
        val label = treeDisplayName(uri)
        treeUri = uri.toString()
        treeLabel = label
        treeLoaded = true
        LinkxRuntime.prefsStore()?.edit()
            ?.putString(KEY_TREE_URI, treeUri)
            ?.putString(KEY_TREE_LABEL, label)
            ?.apply()
        Log.i(TAG, "接收目录已改为 $label（持久授权=$granted）")
        return if (granted) label else "$label（注意：系统没给持久授权，重启后可能要重选）"
    }

    /** 改回默认的应用私有目录，并撤销持久授权（不留一个"用户已经不要了"的授权）。 */
    fun clearReceiveTree() {
        val ctx = LinkxRuntime.requireContext()
        val old = treeUri
        treeUri = ""
        treeLabel = ""
        LinkxRuntime.prefsStore()?.edit()?.remove(KEY_TREE_URI)?.remove(KEY_TREE_LABEL)?.apply()
        if (old.isNotEmpty()) {
            runCatching {
                ctx.contentResolver.releasePersistableUriPermission(
                    Uri.parse(old),
                    Intent.FLAG_GRANT_READ_URI_PERMISSION or Intent.FLAG_GRANT_WRITE_URI_PERMISSION,
                )
            }
        }
    }

    /** tree Uri 的可读名：问文档树自己要 DISPLAY_NAME（"下载"这种本地化名），退化成解码后的路径段。 */
    private fun treeDisplayName(uri: Uri): String {
        val ctx = LinkxRuntime.requireContext()
        val docId = runCatching { DocumentsContract.getTreeDocumentId(uri) }.getOrNull()
            ?: return "所选文件夹"
        val root = DocumentsContract.buildDocumentUriUsingTree(uri, docId)
        val fromDoc = runCatching {
            ctx.contentResolver.query(
                root,
                arrayOf(DocumentsContract.Document.COLUMN_DISPLAY_NAME),
                null,
                null,
                null,
            )?.use { c -> if (c.moveToFirst()) c.getString(0) else null }
        }.getOrNull()
        if (!fromDoc.isNullOrBlank()) return fromDoc
        val tail = docId.substringAfter(':')
        return runCatching { URLDecoder.decode(tail, "UTF-8") }.getOrDefault(tail)
    }

    /** 一条入站文件收完后的最终归宿：位置、界面上的名字、要补的那句话（空 = 没话说）。 */
    private class Landed(val path: String, val name: String, val note: String)

    /** 把收完的文件交送到用户选的目录。复制失败**不算接收失败**：字节已经完整到手，删掉它就是把成功变没——所以留在私有目录，并把原因如实写在行上，让用户能改目录后重试。 */
    private fun deliverToChosenDir(src: File, name: String): Landed {
        loadTree()
        if (treeUri.isEmpty()) return Landed(src.absolutePath, name, "")
        // **整段**都放进 runCatching，连 `requireContext()` / `Uri.parse` 这种"看起来不会炸"的前置语句也算在内：
        // 漏出来会顺着 worker 的兜底 catch 把 `updateState(Done)` 一起跳过——电脑显示"已完成"、手机永远停在"传输中"，正是最不能出现的那类谎。
        val outcome = runCatching {
            val ctx = LinkxRuntime.requireContext()
            val tree = Uri.parse(treeUri)
            val parent = DocumentsContract.buildDocumentUriUsingTree(
                tree,
                DocumentsContract.getTreeDocumentId(tree),
            )
            val created = DocumentsContract.createDocument(
                ctx.contentResolver,
                parent,
                mimeOf(name),
                name,
            ) ?: error("系统拒绝在这个目录里建文件")
            runCatching {
                ctx.contentResolver.openOutputStream(created, "w").use { out ->
                    requireNotNull(out).use { o -> src.inputStream().use { it.copyTo(o, 128 * 1024) } }
                }
            }.onFailure {
                // 建好了却没写完 = 用户目录里躺着一个 0 字节的假文件。删掉它：留着比"没写成"更坏，文件管理器会告诉用户"东西在这儿"，而它是空的。
                runCatching { DocumentsContract.deleteDocument(ctx.contentResolver, created) }
                    .onFailure { d -> Log.w(TAG, "半截文档也没能删掉：${d.message}") }
            }.getOrThrow()
            // 重名时系统会自己改成 "xxx (1).ext"：以最终名字回给界面，别说谎
            val finalName = runCatching {
                ctx.contentResolver.query(created, null, null, null, null)?.use { c ->
                    val i = c.getColumnIndex(OpenableColumns.DISPLAY_NAME)
                    if (i >= 0 && c.moveToFirst()) c.getString(i) else null
                }
            }.getOrNull() ?: name
            if (!src.delete()) Log.w(TAG, "已复制到所选目录，但私有目录里的副本没删掉：${src.absolutePath}")
            Landed(created.toString(), finalName, "")
        }
        return outcome.getOrElse { e ->
            Log.w(TAG, "复制进所选目录失败：${e.javaClass.simpleName} ${e.message}")
            Landed(
                src.absolutePath,
                name,
                "已收到，但没能存进「$treeLabel」：${e.message ?: "未知原因"}。文件先留在应用私有目录，别的 App 看不到它",
            )
        }
    }

    /** 按扩展名给一个 MIME（SAF 建文档要用；认不出来就给通用二进制类型）。 */
    private fun mimeOf(name: String): String = when(name.substringAfterLast('.', "").lowercase()) {
        "jpg", "jpeg" -> "image/jpeg"
        "png" -> "image/png"
        "gif" -> "image/gif"
        "webp" -> "image/webp"
        "heic" -> "image/heic"
        "mp4" -> "video/mp4"
        "mov" -> "video/quicktime"
        "mkv" -> "video/x-matroska"
        "avi" -> "video/avi"
        "mp3" -> "audio/mpeg"
        "m4a" -> "audio/mp4"
        "pdf" -> "application/pdf"
        "txt" -> "text/plain"
        "zip" -> "application/zip"
        "apk" -> "application/vnd.android.package-archive"
        else -> "application/octet-stream"
    }

    /** 解析 SAF Uri 的文件名/大小。 */
    fun brief(uri: Uri): FileBrief? = runCatching {
        val ctx = LinkxRuntime.requireContext()
        var name: String? = null
        var size = -1L
        ctx.contentResolver.query(uri, null, null, null, null)?.use { c ->
            val nameIdx = c.getColumnIndex(OpenableColumns.DISPLAY_NAME)
            val sizeIdx = c.getColumnIndex(OpenableColumns.SIZE)
            if (c.moveToFirst()) {
                if (nameIdx >= 0) name = c.getString(nameIdx)
                if (sizeIdx >= 0 && !c.isNull(sizeIdx)) size = c.getLong(sizeIdx)
            }
        }
        FileBrief(name ?: uri.lastPathSegment ?: "file", size)
    }.getOrNull()

    // ---------- 事件投递（pump 锁内调用） ----------

    /** 在途发送任务数（含等待对端回执的）——调试面 `/state` 取证用。 */
    fun sendingCount(): Int = synchronized(lock) { sends.size }

    fun onChunk(chunk: IncomingChunk) = submit { writeChunk(chunk) }

    fun onFileMeta(ev: LinkxEvent.FileMeta) = submit { beginReceive(ev) }

    fun onFileDone(ev: LinkxEvent.FileDone) = submit { onPeerFileDone(ev) }

    fun onFileResume(ev: LinkxEvent.FileResume) = submit { noteResume(ev.fileId, ev.fromIndex) }

    /** 对端 FILE_DONE：既可能是**本端发送**任务的确认、也可能是**本端接收**任务的结束，两个方向都必须处理——漏掉发送侧的确认会把失败显示成"已完成"。 */
    private fun onPeerFileDone(ev: LinkxEvent.FileDone) {
        val sending = synchronized(lock) { sends.remove(ev.fileId) }
        if (sending != null) {
            // 对端说"我没收全"：正在补发的循环据此停手——再发下去每一块都是孤儿
            if (!ev.ok) sending.peerRejected = true
            val name = sending.name
            if (ev.ok) {
                updateState(ev.fileId, TransferState.Done, forceFull = true)
                Log.i(TAG, "对端已确认收完 $name")
            } else {
                updateState(
                    ev.fileId,
                    TransferState.Failed,
                    error = ev.error.ifEmpty { "电脑那边没有收下这个文件" },
                )
                Log.w(TAG, "对端拒绝/未完成 $name：${ev.error}")
            }
            return
        }
        finishReceive(ev)
    }

    /** 引擎报文件传输硬失败（TCP 未绑定 / 传输中 TCP 断开）→ 落到任务状态，不许静默只留日志。 */
    fun onTaskFailed(fileId: Long, reason: String) = submit {
        val sending = synchronized(lock) { sends.remove(fileId) }
        // 接收侧的失败也要在这里落：引擎会在 TCP 断开时把本机在途接收一并判失败；这里若不 closeRecv，
        // 文件句柄与会话登记会一直留着，`cancellable()` 也会继续说"这条还在途"。
        val rcv = closeRecv(fileId)
        updateState(fileId, TransferState.Failed, error = reason)
        when {
            sending != null -> Log.w(TAG, "文件任务失败（${sending.name}）：$reason")
            rcv != null -> Log.w(TAG, "文件接收中断（${rcv.name}）：$reason")
            else -> Log.w(TAG, "文件任务失败（#$fileId）：$reason")
        }
    }

    /**
     * 用户取消一条传输的落点（本端点的、对端发来的、收到对端取消收尾的都归一到这一条）。三件事缺一不可，而且都不许写成"失败"：
     * 1) 置 `cancelled` 掐断本机发送循环——它就不会补发第二条结束帧、也不会把行落回「失败」；
     * 2) 接收侧删掉半截文件（留着就像传成功了），删不掉必须把路径如实报进原因里；
     * 3) 登记这个 id，挡掉仍排在 worker 队列后面的文件头（见 [cancelledIds]）。
     */
    fun onTaskCancelled(fileId: Long, reason: String) {
        val send = synchronized(lock) { sends.remove(fileId)?.also { it.cancelled = true } }
        val rcv = closeRecv(fileId)
        markCancelled(fileId)
        var text = reason
        if (rcv != null && !deleteLeftover(rcv.path)) {
            text = "$reason（残留文件没删掉，请手动清理：${rcv.path}）"
            Log.w(TAG, "取消后删除残留文件失败：${rcv.path}")
        }
        if (send == null && rcv == null) {
            // 本机已经没有这条在途会话（多半是刚收尾完）：不静默吞——两台机器各显示一种状态时，这句是唯一能对上的线索。行还在就把话写在行上，但状态不动（更不能凭空造一行）。
            text = "$reason（本机已无对应的在途任务，这条取消收尾没有可落的对象）"
            noteOnRow(fileId, text)
            Log.w(TAG, "收到对不上的取消收尾（#$fileId）：$text")
            return
        }
        updateState(fileId, TransferState.Cancelled, error = text)
        Log.i(TAG, "文件任务已取消（${send?.name ?: rcv?.name ?: "#$fileId"}）：$text")
    }

    /** 1 Hz：等待对端回执超时的发送任务落到「已发送（未确认）」，不永远转圈。 */
    fun tickAcks() {
        val now = SystemClock.elapsedRealtime()
        val expired = synchronized(lock) {
            sends.entries.filter { (_, s) -> s.ackDeadlineMs != 0L && now > s.ackDeadlineMs }
                .map { (id, s) -> id to s.name }
        }
        for ((id, name) in expired) {
            synchronized(lock) { sends.remove(id) }
            updateState(id, TransferState.SentUnconfirmed)
            Log.w(TAG, "未收到对端回执，$name 标记为「已发送（未确认）」而不是已完成")
        }
        // 暂缓收尾的补发窗口到点也得有人收掉。收尾要落盘、要交送，不能在这个 1 Hz 调度线程上做——交给 file worker（与 writeChunk 同一线程口径）。
        val holds = synchronized(lock) {
            recv.entries.filter { (_, s) -> s.resumeHoldMs != 0L && now > s.resumeHoldMs }
                .map { (id, _) -> id }
        }
        for (id in holds) submit { expireResumeHold(id) }
    }

    // ---------- 发送 ----------

    /** 发送文件（阻塞式，须在后台线程调用）。`albumId` 非 0 = 相册原图应答。 */
    fun send(uri: Uri, albumId: Long = 0L) {
        val brief = brief(uri) ?: FileBrief(uri.lastPathSegment ?: "file", -1L)

        // 闸门，与电脑端同口径：TCP 未绑定就不发。256KB 分块在 BLE 上要拆成约 18,700 个 14 字节包，
        // 而 notify 队列上限 512 且丢最旧，硬发只会得到「META 到了、分块没到、UI 显示已完成」。
        if (!LinkxRuntime.isPaired()) {
            val id = newFileId()
            upsert(
                TransferItem(
                    id, brief.name, brief.size.coerceAtLeast(0L), true, 0L,
                    TransferState.Failed, "还没和电脑配对，文件没有发出",
                )
            )
            Log.w(TAG, "未配对，文件未发送：${brief.name}")
            return
        }
        if (!LinkxRuntime.isTcpBound()) {
            val id = newFileId()
            upsert(
                TransferItem(
                    id, brief.name, brief.size.coerceAtLeast(0L), true, 0L,
                    TransferState.Failed, "局域网通道还没就绪（电脑要开着 LinkX 并在同一 Wi-Fi）",
                )
            )
            Log.w(TAG, "TCP 通道尚未就绪，文件未发送：${brief.name}")
            return
        }

        // 1) 大小优先用 ContentResolver 报的值；报不出来（≤0）才退化为「预扫一遍拿长度+摘要」——只有这种少见情况才付预扫的代价。
        var size = brief.size
        var declared: ByteArray? = null
        var crc = 0
        if (size <= 0L) {
            val ctx = LinkxRuntime.requireContext()
            val pre = runCatching {
                val md = MessageDigest.getInstance("SHA-256")
                val c = CRC32()
                var total = 0L
                val input = ctx.contentResolver.openInputStream(uri) ?: return@runCatching null
                input.use { ins ->
                    val scan = ByteArray(64 * 1024)
                    while (true) {
                        val n = ins.read(scan)
                        if (n <= 0) break
                        md.update(scan, 0, n)
                        c.update(scan, 0, n)
                        total += n
                    }
                }
                Triple(total, md.digest(), c.value.toInt())
            }.getOrNull()
            if (pre == null) {
                val id = newFileId()
                upsert(
                    TransferItem(
                        id, brief.name, 0L, true, 0L,
                        TransferState.Failed, "读不到这个文件（可能已被移动、删除，或授权已过期）",
                    )
                )
                Log.w(TAG, "读取文件失败：${brief.name}")
                return
            }
            size = pre.first
            declared = pre.second
            crc = pre.third
        }

        val fileId = newFileId()
        val state = SendState(uri, brief.name, size, CHUNK_SIZE, declared, albumId)
        synchronized(lock) { sends[fileId] = state }
        upsert(TransferItem(fileId, brief.name, size, true, 0L, TransferState.Running))

        // 2) FILE_META 立即发出；流式路径摘要留空（长度 0 → 摘要由 FILE_DONE 交付，不预扫整文件）。
        val metaSha = declared ?: ByteArray(0)
        if (!LinkxRuntime.sendFileMeta(fileId, brief.name, size, CHUNK_SIZE, crc, metaSha, albumId)) {
            Log.w(TAG, "FILE_META 未入队（未配对？）：${brief.name}")
            updateState(fileId, TransferState.Failed, error = "发送中断：与电脑的连接已断开")
            synchronized(lock) { sends.remove(fileId) }
            return
        }
        sendChunks(fileId, 0)
    }

    /** 相册原图请求被拒时的一次"有头有尾"：0 字节 FILE_META（带 album_id）+ 失败 FILE_DONE。相册方向没有专用错误帧，而电脑在等这条文件的头——两帧齐全地把原因送到它面前，比让它对着一个空转盘等超时诚实。 */
    fun declineAlbumFull(albumId: Long, reason: String) {
        val fileId = newFileId()
        if (!LinkxRuntime.sendFileMeta(fileId, "相册原图 #$albumId", 0L, CHUNK_SIZE, 0, ByteArray(0), albumId)) {
            Log.w(TAG, "相册拒绝帧未入队（未配对？）：album_id=$albumId")
            return
        }
        LinkxRuntime.sendFileDone(fileId, false, reason, ByteArray(0))
    }

    /**
     * 顺序流式发送：整条传输**只打开一次输入流**，一路读到底；整文件 SHA-256 随分块增量算出，由 FILE_DONE 交付。
     * 每块重开流 + `skip()` 定位是 O(N²) 读放大（100 MB 要白读约 20 GB）；续传跳转（`resumeFrom`）才重开一次——那是罕见事件，不是每块。
     * 外层这一圈是补发兜底：`FILE_DONE` 出口之后对端仍可能清点出自己的洞并请求补发，收到就按它给的起点再来一轮，直到对端回执、窗口超时或补发轮次用尽。
     */
    private fun sendChunks(fileId: Long, from: Int) {
        var start = from
        while (true) {
            val restart = sendOnce(fileId, start) ?: return
            val st = synchronized(lock) { sends[fileId] } ?: return
            if (st.resumeRounds >= MAX_RESUME_TRIES) {
                // 补发轮次用尽还不收敛：不再重发，让对端的收尾把这次传输判失败（并说清原因）
                Log.w(TAG, "补发轮次已用尽（fileId=$fileId，第 $restart 块），不再重发")
                return
            }
            st.resumeRounds++
            start = restart
            Log.i(TAG, "回执窗口里收到续传请求，第 ${st.resumeRounds} 轮从第 $restart 块补发")
            updateState(fileId, TransferState.Running, note = "对端请求从第 $restart 块补发")
        }
    }

    /** 发一轮分块直到 `FILE_DONE` 出口，然后在回执窗口里等；返回非 null = 要从这个起点再来一轮。 */
    private fun sendOnce(fileId: Long, from: Int): Int? {
        val state = synchronized(lock) { sends[fileId] } ?: return null
        // 从这一刻起 noteResume 的登记有人读了；退出必须清掉，否则迟到的续传被当成"已受理"而没人重发。
        state.loopAlive = true
        // 正在真发东西就不是"等回执"：上一轮的截止时刻必须清掉，否则补发到一半会被 tickAcks 当超时摘掉会话（行落到「已发送（未确认）」，而对端随后回执时已找不到发送会话）。
        state.ackDeadlineMs = 0L
        val total = chunkCount(state.size, state.chunkSize)
        val ctx = LinkxRuntime.requireContext()
        var index = from
        var ok = true
        var err = ""
        // 读缓冲在循环外分配一次、复用到底：每块新建 256KB 数组会把大文件传输的内存峰值翻倍（小内存机型上是要命的）。
        val buf = ByteArray(state.chunkSize)
        var stream: java.io.InputStream? = try {
            ctx.contentResolver.openInputStream(state.uri)
        } catch (e: Exception) {
            null
        }
        if (stream == null) {
            state.loopAlive = false
            LinkxRuntime.sendFileDone(fileId, false, "无法读取文件")
            updateState(fileId, TransferState.Failed, error = "读不到这个文件（授权已过期或被移动）")
            synchronized(lock) { sends.remove(fileId) }
            return null
        }
        try {
            var input = stream!!
            // 定位到起始块（只在开始/续传时发生一次）：跳过的前缀不重发，但要喂进摘要
            if (!foldPrefix(input, state, index.toLong() * state.chunkSize, buf)) {
                ok = false
                err = "续传起点超过文件长度"
            }
            while (ok && index < total && !state.cancelled && !state.peerRejected) {
                val resumeTo = state.resumeFrom
                if (resumeTo != null) {
                    state.resumeFrom = null
                    if (resumeTo in 0 until total && resumeTo != index) {
                        index = resumeTo
                        runCatching { input.close() }
                        val reopened = ctx.contentResolver.openInputStream(state.uri)
                        if (reopened == null) {
                            ok = false
                            err = "续传时无法重开文件"
                            break
                        }
                        input = reopened
                        // 摘要口径：重开流后累计器清零，再把 [0, 起点) 前缀读回来喂进去——每个字节恰好算一次，与从未中断的路径逐字节等价。
                        state.digest.reset()
                        state.hashedBytes = 0
                        if (!foldPrefix(input, state, index.toLong() * state.chunkSize, buf)) {
                            ok = false
                            err = "续传前缀读不出来"
                        }
                    }
                }
                if (!ok) break
                // 背压：在途帧数达到窗口就等链路排空。缺这一步，发送快过链路时队列会一路涨到"剩余文件大小"，100 MB 传输就能吃光内存。
                val waited = LinkxRuntime.waitForTcpWindow()
                if (!waited) {
                    ok = false
                    err = "TCP 通道长时间无法排空，文件未完成"
                    break
                }
                val expect = minOf(
                    state.chunkSize.toLong(),
                    state.size - index.toLong() * state.chunkSize,
                ).toInt()
                val n = readUpTo(input, buf, expect)
                if (n < expect) {
                    // 文件比 FILE_META 声明的小：不能当成"正常发完"
                    ok = false
                    err = "文件比声明的小（第 $index 块读到 $n/$expect 字节）"
                    break
                }
                val data = if (n == state.chunkSize) buf.copyOf() else buf.copyOf(n)
                if (!LinkxRuntime.sendFileChunk(fileId, index, crc32(data).toInt(), data)) {
                    ok = false
                    err = "通道不可用（未配对 / TCP 断开 / 未先发送文件头）"
                    break
                }
                // 摘要只覆盖真的交出去的那一段
                state.digest.update(data)
                state.hashedBytes += n
                index++
                updateProgress(fileId, minOf(index.toLong() * state.chunkSize, state.size))
            }
            if (ok && state.hashedBytes != state.size) {
                ok = false
                err = "发出 ${state.hashedBytes} 字节，与声明的 ${state.size} 字节不符"
            }
        } catch (e: Exception) {
            ok = false
            err = e.message ?: "读取失败"
        } finally {
            // 从这里出去就再没有人读 `resumeFrom` 了，必须让 noteResume 知道。
            state.loopAlive = false
            runCatching { stream?.close() }
        }
        // 用户取消：带 cancelled 的结束帧已由引擎在收到取消命令那一刻发出，这里既不许补第二条，也不许把状态落回「失败」——那等于把用户的主动动作说成系统故障。
        if (state.cancelled) {
            Log.i(TAG, "发送循环已停手（已取消）：${state.name}")
            synchronized(lock) { sends.remove(fileId) }
            return null
        }
        // 对端已经给这条文件收尾（回执 ok=false）：结果已由 `onPeerFileDone` 落到界面，这里不该再发一条 FILE_DONE，也不该再来一轮补发——那是对着一个不存在的人说话。
        if (state.peerRejected) {
            Log.i(TAG, "发送循环收到对端收尾，停手不再补发：${state.name}")
            return null
        }
        // FILE_DONE：成功才交付整文件摘要（长度 0 = 没算出来，收端据此判失败而不是猜）；预扫路径用现成的，流式路径用发块时增量攒出的那份。
        val doneSha = if (ok) state.declaredSha ?: state.digest.digest() else ByteArray(0)
        LinkxRuntime.sendFileDone(fileId, ok, if (ok) "" else err, doneSha)
        if (!ok) {
            updateState(fileId, TransferState.Failed, error = err.ifEmpty { "分块发送失败" })
            synchronized(lock) { sends.remove(fileId) }
            return null
        }
        // 分块全部交给本地引擎 ≠ 对端收到：这里标 AwaitingPeer，等电脑侧 FILE_DONE 回执才落 Done；超时由 tickAcks 落「已发送（未确认）」。
        val st = synchronized(lock) { sends[fileId] } ?: return null
        st.ackDeadlineMs = SystemClock.elapsedRealtime() + ACK_TIMEOUT_MS
        updateState(fileId, TransferState.AwaitingPeer)
        // 第一条 FILE_DONE 出口不等于尘埃落定——对端正是收到它的那一刻才清点出自己手里的洞。
        // 留在本线程把这个回执窗口盯完：等到续传请求就再来一轮，等到回执（会话被摘掉）或窗口到点就收工。
        st.awaitingAck = true
        val restart = try {
            waitForLateResume(fileId)
        } finally {
            st.awaitingAck = false
        }
        if (restart == null) return null
        // 摘要口径与循环内续传一致：累计器清零，下一轮把 [0, 起点) 前缀重读喂回去。
        st.digest.reset()
        st.hashedBytes = 0
        return restart
    }

    /** 回执窗口里只盯两件事：对端的续传请求（要补发）和会话被摘掉（回执已到 / 已取消 / 已超时）。只由发送线程调用。返回非 null = 要从这个起点再来一轮。 */
    private fun waitForLateResume(fileId: Long): Int? {
        val deadline = synchronized(lock) { sends[fileId]?.ackDeadlineMs } ?: return null
        while (SystemClock.elapsedRealtime() < deadline) {
            // 泵一下：对端的续传请求与回执都是入站帧，没人泵就永远到不了 noteResume。发送线程若是被 linkx-tick 直接拉起来的（调试动作就是这条路），这里不泵就死等。
            LinkxRuntime.pumpWaiting()
            val st = synchronized(lock) { sends[fileId] } ?: return null
            val from = st.resumeFrom
            if (from != null) {
                st.resumeFrom = null
                return from
            }
            if (st.cancelled || st.peerRejected) return null
            Thread.sleep(50)
        }
        return null
    }

    /** 把 `[0, bytes)` 前缀读出来喂进摘要（不发帧）：续传后 FILE_DONE 的摘要仍须覆盖全文件，而 `InputStream.skip()` 给不出被跳过的字节，所以这段只能真读。返回 false = 文件比 `bytes` 短（起点越界）。只由发送循环所在线程调用。 */
    private fun foldPrefix(
        input: InputStream,
        state: SendState,
        bytes: Long,
        buf: ByteArray,
    ): Boolean {
        var left = bytes
        while (left > 0) {
            val want = minOf(buf.size.toLong(), left).toInt()
            val n = readUpTo(input, buf, want)
            if (n <= 0) return false
            state.digest.update(buf, 0, n)
            state.hashedBytes += n
            left -= n
        }
        return true
    }

    private fun noteResume(fileId: Long, fromIndex: Int) {
        val state = synchronized(lock) { sends[fileId] } ?: run {
            Log.w(TAG, "续传请求找不到在途发送 fileId=$fileId from=$fromIndex：本机已经没有这条发送，不会重发")
            return
        }
        if (state.loopAlive || state.awaitingAck) {
            state.resumeFrom = fromIndex
            val where = if (state.loopAlive) "发送循环在途，下一轮生效" else "在回执窗口里，发送线程接手补发"
            Log.i(TAG, "收到续传请求 fileId=$fileId from=$fromIndex（$where）")
            return
        }
        // 走到这里说明发送线程已经彻底收工：回执已到手、窗口过期、或这条会话被摘掉。此时重发的每一块只会变成对端孤儿，
        // 所以**只留痕、不假装办事**——一个"看起来处理了"的静默分支比报错更难查。
        Log.w(
            TAG,
            "收到续传请求 fileId=$fileId from=$fromIndex，但发送侧已收工：本机不重发，" +
                "这次传输由对端的收尾判定（回执窗口已在本线程盯完）",
        )
    }

    // ---------- 接收 ----------

    /** 接收前置校验不通过：当场拒收，并且**一定回一条 FILE_DONE(ok=false)**——不回执的话发端只能等到自己的超时才判死，用户看到的是一句和真实原因无关的错误；原因同时落到任务行上，界面上说得出为什么。 */
    private fun refuseReceive(ev: LinkxEvent.FileMeta, reason: String) {
        Log.w(TAG, "拒收 ${ev.name}：$reason")
        upsert(
            TransferItem(
                ev.fileId,
                sanitize(ev.name),
                ev.size,
                false,
                0L,
                TransferState.Failed,
                reason,
            )
        )
        LinkxRuntime.sendFileDone(ev.fileId, false, reason, ByteArray(0))
    }

    private fun beginReceive(ev: LinkxEvent.FileMeta) {
        // 取消收尾在事件泵里就地处理，这条文件头却可能还排在 worker 队列里：不挡掉就会在取消之后落出一个「传输中」的行和一个没人要的半截文件。
        if (isCancelled(ev.fileId)) {
            Log.w(TAG, "取消之后才排到的文件头，已丢弃：${ev.name}")
            return
        }
        val dir = workDir()
        if (!dir.exists()) dir.mkdirs()
        // 三道前置闸门（与电脑端 `begin_recv` 同口径）：分块大小、声明长度、本机剩余空间都是对端说了算的输入，
        // 必须在**建文件之前**问清楚，否则一次传输就能把手机写爆。
        if (ev.chunkSize <= 0 || ev.chunkSize > CHUNK_SIZE) {
            refuseReceive(ev, "文件分块大小非法（${ev.chunkSize}，上限 $CHUNK_SIZE）")
            return
        }
        if (ev.size > MAX_RECV_FILE_BYTES) {
            refuseReceive(ev, "文件超过本机接收上限（${ev.size} B > $MAX_RECV_FILE_BYTES B）")
            return
        }
        when (val free = runCatching { StatFs(dir.absolutePath).availableBytes }.getOrNull()) {
            null -> Log.w(TAG, "读不到本机剩余空间（StatFs 失败），只做兜底：写入失败时再报")
            else -> if (free < ev.size) {
                refuseReceive(ev, "本机空间不足：需要 ${ev.size} B，可用 $free B")
                return
            }
        }
        val target = uniqueFile(dir, sanitize(ev.name))
            ?: return refuseReceive(ev, "同名文件过多（已试 1000 个序号），不覆盖已有文件")
        val raf = try {
            RandomAccessFile(target, "rw").apply { setLength(0) }
        } catch (e: Exception) {
            Log.w(TAG, "创建接收文件失败：${e.message}")
            upsert(
                TransferItem(
                    ev.fileId, target.name, ev.size, false, 0L,
                    TransferState.Failed, "本机放不下这个文件：${e.message ?: "未知原因"}",
                )
            )
            return
        }
        val state = RecvState(
            fileId = ev.fileId,
            name = target.name,
            size = ev.size,
            chunkSize = ev.chunkSize, // 已在 `beginReceive` 闸门里挡过非法值（≤0 与 >CHUNK_SIZE 都拒收），这里直接采信
            path = target.absolutePath,
            file = raf,
            expectSha256 = digestOrNull(ev.sha256),
        )
        // 同一个 file_id 又来一条 FILE_META：先把旧会话关干净。不关就是漏一个 `RandomAccessFile` 句柄，而且旧文件永远没人收尾。
        val stale = synchronized(lock) { recv.remove(ev.fileId) }
        if (stale != null) {
            Log.w(TAG, "fileId=${ev.fileId} 已有在途接收，旧会话先关掉：${stale.name}")
            runCatching { stale.file.close() }
        }
        synchronized(lock) { recv[ev.fileId] = state }
        upsert(TransferItem(ev.fileId, target.name, ev.size, false, 0L, TransferState.Running))
        Log.i(
            TAG,
            "开始接收 ${target.name}（${ev.size}B，" +
                "摘要来源=${if (state.expectSha256 == null) "FILE_DONE" else "FILE_META"}）",
        )
    }

    private fun writeChunk(c: IncomingChunk) {
        val state = synchronized(lock) { recv[c.fileId] }
            ?: run {
                // 没有会话 = FILE_META 没到（或会话已收尾）。丢是唯一的处置，但**必须留痕**：否则"显示已完成、文件却是 0 字节"两边都查不出来。
                Log.w(TAG, "分块找不到收件会话，已丢弃：file=${c.fileId} idx=${c.index}")
                return
            }
        if (c.index < state.nextIndex) return // 重复块：忽略
        // 同一个起点只问一次：一个空洞之后的每一块都会走到这里，不去重的话一次缺块能发出几百条一模一样的续传请求（实测 5 ms 内 8 条），
        // 对端还没来得及重传就被自己的预算判死。
        if (c.index > state.nextIndex) {
            // 空洞：请求对端从期望位置续传
            askResume(state, c.fileId, state.nextIndex)
            return
        }
        if (crc32(c.data).toInt() != c.crc32) {
            askResume(state, c.fileId, c.index)
            return
        }
        // 随机写的偏移必须由**声明长度**兜住（与电脑端 `land_chunk` 同一条判据）：对端算错或在撒谎时，
        // `index × chunkSize` 可以把文件撑到它自己说的 size 之外，收件目录不该被一次传输写得比它自己声明的还大。
        val offset = c.index.toLong() * state.chunkSize
        if (offset + c.data.size > state.size) {
            val msg = "分块越出声明长度：${state.name} 第 ${c.index} 块 @$offset +${c.data.size} B > ${state.size} B"
            Log.w(TAG, msg)
            closeRecv(c.fileId)
            updateState(c.fileId, TransferState.Failed, error = "分块越出声明长度，文件未收全")
            LinkxRuntime.sendFileDone(c.fileId, false, "分块越出声明长度", ByteArray(0))
            return
        }
        try {
            state.file.seek(offset)
            state.file.write(c.data)
        } catch (e: Exception) {
            Log.w(TAG, "落盘失败：${e.message}")
            closeRecv(c.fileId)
            updateState(c.fileId, TransferState.Failed, error = "写入本机失败：${e.message ?: "未知原因"}")
            return
        }
        state.nextIndex++
        state.received += c.data.size
        // 落了一块 = 上一次续传起效了，下一次再缺块算新的一轮
        state.pendingResume = null
        // 补发有进展 → 暂缓窗口重新计时：窗口量的是"多久没动静"，不是一共等了多久，否则一次整段重发会被自己的窗口掐死。
        if (state.resumeHoldMs != 0L) {
            state.resumeHoldMs = SystemClock.elapsedRealtime() + RESUME_HOLD_MS
        }
        // 落成功的块才进摘要：收尾时不必再把整个文件读一遍
        state.digest.update(c.data)
        updateProgress(c.fileId, state.received)
    }

    /** 发一条续传请求；与上一条起点相同就跳过（见 [writeChunk] 的去重口径），轮次有界。 */
    private fun askResume(state: RecvState, fileId: Long, fromIndex: Int) {
        if (state.pendingResume == fromIndex) return
        if (state.resumeTries >= MAX_RESUME_TRIES) {
            // 与电脑端同口径：预算用尽就不再刷请求，由收尾把这次传输判失败并说清原因
            Log.w(TAG, "续传轮次已用尽（${state.resumeTries} 轮）fileId=$fileId，不再请求第 $fromIndex 块")
            return
        }
        state.resumeTries++
        state.pendingResume = fromIndex
        LinkxRuntime.sendFileResume(fileId, fromIndex)
    }

    /** 收到 FILE_DONE 但本端有洞 → **暂缓收尾**，再要一次补发。不许先摘会话再回执"不完整"——摘了会话，发端那条迟到的续传请求就再也没有人读，重发出去的每一块也只会变成孤儿。 */
    private fun holdForResume(state: RecvState, fileId: Long) {
        state.resumeHoldMs = SystemClock.elapsedRealtime() + RESUME_HOLD_MS
        state.resumeTries++
        state.pendingResume = state.nextIndex
        LinkxRuntime.sendFileResume(fileId, state.nextIndex)
        updateState(fileId, TransferState.Running, note = "对端说发完了，等它补发第 ${state.nextIndex} 块")
        Log.w(TAG, "接收 #$fileId 缺第 ${state.nextIndex} 块，暂缓收尾等补发（第 ${state.resumeTries} 轮）")
    }

    /** 暂缓窗口到点仍缺：大声收尾，不许永远挂着「等补发」。 */
    private fun expireResumeHold(fileId: Long) {
        val state = synchronized(lock) { recv[fileId] } ?: return
        if (state.resumeHoldMs == 0L) return // 窗口里已经正常收尾
        state.resumeHoldMs = 0L
        val expected = chunkCount(state.size, state.chunkSize)
        val complete = state.nextIndex >= expected && state.received >= state.size
        if (complete) {
            // 补发都到了，只是发端第二条 FILE_DONE 没落进来：按老路校验（摘要回落 FILE_META 那份）
            settleReceive(fileId, peerOk = true, doneSha = null)
            return
        }
        val why = "等对端从第 ${state.nextIndex} 块补发超时，文件不完整"
        Log.w(TAG, "接收 #$fileId：$why")
        settleReceive(fileId, peerOk = false, doneSha = null, forcedReason = why)
    }

    private fun finishReceive(ev: LinkxEvent.FileDone) {
        val state = synchronized(lock) { recv[ev.fileId] } ?: run {
            // 本端没有这条在途接收（重复/迟到的 DONE，或会话早已收尾）。静默丢掉的话，"发端还在补发、收端已经收摊"这类分歧在日志里看不见。
            Log.w(TAG, "结束帧找不到在途接收会话，已忽略：file=${ev.fileId} ok=${ev.ok}")
            return
        }
        val expectedChunks = chunkCount(state.size, state.chunkSize)
        val complete = ev.ok && state.nextIndex >= expectedChunks && state.received >= state.size
        // 判据里**不看** `resumeHoldMs` 是否已置过：第二个洞同样该再救一次，真正的上界是 `resumeTries`（与发端补发轮次同口径）。
        if (!complete && ev.ok && state.resumeTries < MAX_RESUME_TRIES) {
            holdForResume(state, ev.fileId)
            return
        }
        settleReceive(ev.fileId, ev.ok, digestOrNull(ev.sha256), ev.error.ifEmpty { null })
    }

    /** 校验收尾：核对字节数与整文件摘要，回执一条 FILE_DONE，再把结果落到界面；会话到这一步才真正摘掉。`forcedReason` = 本端自己判出的原因（如等补发超时），直接进回执与 UI，别让它被泛化成一句「不完整」。 */
    private fun settleReceive(
        fileId: Long,
        peerOk: Boolean,
        doneSha: ByteArray?,
        forcedReason: String? = null,
    ) {
        val state = synchronized(lock) { recv[fileId] } ?: return
        val expectedChunks = chunkCount(state.size, state.chunkSize)
        val complete = peerOk && state.nextIndex >= expectedChunks && state.received >= state.size
        // 本端增量摘要：分块落盘时逐块喂入，收尾不再重读整个文件（那是 GB 级的几十秒死等）。
        val sha = runCatching { state.digest.digest() }.getOrNull()
        // 校验源到收尾才定：FILE_DONE 带来的摘要优先（新端边发边算的那份），缺位才回落 FILE_META。
        val expected = doneSha ?: state.expectSha256
        val digestOk = expected != null && sha != null && expected.contentEquals(sha)
        val name = state.name
        val got = state.received
        closeRecv(fileId)
        // 收端**无论成败都回一条 FILE_DONE 作为回执**：只在失败时回话会让发送侧永远等不到确认、只能 EOF 时自判"已完成"，把假成功端给用户。
        val ackReason = forcedReason ?: when {
            complete && digestOk -> null
            expected == null -> "接收端没有可比对的整文件摘要（对端两帧都没给）"
            complete -> "接收端摘要不符"
            !peerOk -> "对端报告失败"
            else -> "接收端不完整（$got/${state.size} 字节）"
        }
        LinkxRuntime.sendFileDone(fileId, ackReason == null, ackReason ?: "", ByteArray(0))
        if (ackReason == null) {
            Log.i(TAG, "接收完成 $name（${got}B，sha256=${sha?.toHex() ?: "—"}），已回执")
            // 交送到用户选的目录（没选过就留在私有目录）。这一步在 file worker 线程上跑：200 MB 的复制要一两秒，绝不在事件泵锁里做。
            val landed = deliverToChosenDir(File(state.path), name)
            updateState(
                fileId,
                TransferState.Done,
                forceFull = true,
                path = landed.path,
                note = landed.note,
                name = landed.name,
            )
        } else {
            Log.w(TAG, "接收失败 $name：$ackReason，已回执")
            updateState(fileId, TransferState.Failed, error = ackReason)
        }
    }

    /** 关掉并摘掉本端接收会话；返回被摘掉的那条（null = 本机根本没有这条在途接收）。 */
    private fun closeRecv(fileId: Long): RecvState? {
        val state = synchronized(lock) { recv.remove(fileId) } ?: return null
        // 先放手再删：句柄还开着时删除会失败，而取消收尾要的就是"删干净"
        runCatching { state.file.close() }
        return state
    }

    // ---------- 工具 ----------

    private fun submit(op: () -> Unit) {
        ensureWorker()
        queue.offer(op)
    }

    private fun ensureWorker() {
        if (worker != null) return
        val t = Thread({
            while (true) {
                val op = try {
                    queue.take()
                } catch (e: InterruptedException) {
                    break
                }
                runCatching { op() }.onFailure { Log.w(TAG, "传输任务失败", it) }
            }
        }, "linkx-file").apply { isDaemon = true }
        worker = t
        t.start()
    }

    private fun newFileId(): Long {
        // 非 0 的正随机 id（与对端无协商，仅本次会话内定位传输）
        var id = SecureRandom().nextLong() and Long.MAX_VALUE
        if (id == 0L) id = 1L
        return id
    }

    /** 登记一个已取消的 id（FIFO 淘汰，只留最近 [CANCELLED_IDS_MAX] 个）。 */
    private fun markCancelled(fileId: Long) {
        synchronized(lock) {
            if (cancelledIds.contains(fileId)) return
            cancelledIds.addLast(fileId)
            while (cancelledIds.size > CANCELLED_IDS_MAX) cancelledIds.removeFirst()
        }
    }

    private fun isCancelled(fileId: Long): Boolean = synchronized(lock) { cancelledIds.contains(fileId) }

    /** 删掉取消后残留的半截文件；本机根本没有这个文件也算删干净了（不为它编一句"删不掉"）。 */
    private fun deleteLeftover(path: String): Boolean {
        val f = File(path)
        return !f.exists() || runCatching { f.delete() }.getOrDefault(false)
    }

    private fun chunkCount(size: Long, chunkSize: Int): Int {
        val cs = if (chunkSize > 0) chunkSize else CHUNK_SIZE
        return if (size <= 0L) 0 else ((size + cs - 1) / cs).toInt()
    }

    /** 读满 `len` 字节（不足则读到 EOF 为止）；返回实际读到的字节数。 */
    private fun readUpTo(input: InputStream, buf: ByteArray, len: Int): Int {
        var off = 0
        while (off < len) {
            val n = input.read(buf, off, len - off)
            if (n < 0) break
            off += n
        }
        return off
    }

    private fun crc32(data: ByteArray): Long {
        val c = CRC32()
        c.update(data)
        return c.value
    }

    /** 摘要归一：JNI 把「这一帧没带摘要」编码成长度 0 或全零 32B（SHA-256 不会命中全零原像），一律归一成 null，交给调用方决定回落顺序（FILE_DONE 优先、FILE_META 兜底）。 */
    private fun digestOrNull(sha: ByteArray): ByteArray? =
        if (sha.size == 32 && sha.any { it != 0.toByte() }) sha else null

    private fun ByteArray.toHex(): String = buildString(size * 2) {
        for (b in this@toHex) {
            val v = b.toInt() and 0xFF
            append("0123456789abcdef"[v ushr 4])
            append("0123456789abcdef"[v and 0x0F])
        }
    }

    /**
     * 文件名净化：取基名消掉目录穿越，其余只去 NUL/控制字符并 trim。**不**照抄 Windows 那张非法字符表：
     * `<>:"\|?*` 在 ext4 与 MediaStore 上是合法文件名字符，改写了只会破坏用户本该看到的名字（NTFS 才真的不接受）。
     * 两端的共同点是"落盘名必须单段、不可穿越、字节数有界"，而不是逐字符一致。
     */
    private fun sanitize(name: String): String {
        val base = name.substringAfterLast('/').substringAfterLast('\\')
        val cleaned = buildString(base.length) {
            for (ch in base) if (!ch.isISOControl()) append(ch)
        }.trim().trimEnd('.', ' ')
        val cut = truncateUtf8(cleaned, MAX_NAME_BYTES)
        if (cut.isEmpty() || cut == "." || cut == "..") return "received.bin"
        return cut
    }

    /** 按 **UTF-8 字节数**截断且不切断代理对：按 UTF-16 代码单元数截（如 `take(120)`）可能留半个 surrogate（文件名从此无法编码），且 120 个汉字 = 360 字节，超过 ext4 单段 255B 上限，建文件直接失败。 */
    private fun truncateUtf8(s: String, maxBytes: Int): String {
        var bytes = 0
        var i = 0
        while (i < s.length) {
            val cp = s.codePointAt(i)
            // UTF-8 编码长度由码点范围决定（1/2/3/4 字节）
            val size = when {
                cp < 0x80 -> 1
                cp < 0x800 -> 2
                cp < 0x10000 -> 3
                else -> 4
            }
            if (bytes + size > maxBytes) break
            bytes += size
            i += Character.charCount(cp)
        }
        return s.substring(0, i)
    }

    /** 目标目录内重名时追加 " (n)"。 */
    private fun uniqueFile(dir: File, name: String): File? {
        var target = File(dir, name)
        if (!target.exists()) return target
        val dot = name.lastIndexOf('.')
        val stem = if (dot > 0) name.substring(0, dot) else name
        val ext = if (dot > 0) name.substring(dot) else ""
        var i = 1
        while (target.exists()) {
            target = File(dir, "$stem ($i)$ext")
            i++
            if (i > 1000) {
                // 1000 个同名还在撞：返回 null 让调用方拒收。返回那个已存在的路径 = 把用户的旧文件截断重写。
                return null
            }
        }
        return target
    }

    private fun upsert(item: TransferItem) {
        synchronized(lock) {
            val i = items.indexOfFirst { it.fileId == item.fileId }
            if (i >= 0) items[i] = item else items.add(0, item)
        }
        LinkxRuntime.notifyChanged()
    }

    private fun updateProgress(fileId: Long, bytes: Long) {
        synchronized(lock) {
            val i = items.indexOfFirst { it.fileId == fileId }
            if (i >= 0) items[i] = items[i].copy(bytes = bytes)
        }
        LinkxRuntime.notifyChanged()
    }

    /** 把一句话写进行上而**不动状态**（给"对不上号的取消收尾"用：行已是终态、状态改不得，但话必须让用户看见，否则就是一条被静默吞掉的事件）。 */
    private fun noteOnRow(fileId: Long, text: String) {
        synchronized(lock) {
            val i = items.indexOfFirst { it.fileId == fileId }
            if (i >= 0) items[i] = items[i].copy(error = text)
        }
        LinkxRuntime.notifyChanged()
    }

    private fun updateState(
        fileId: Long,
        state: TransferState,
        forceFull: Boolean = false,
        error: String = "",
        path: String = "",
        note: String = "",
        name: String = "",
    ) {
        synchronized(lock) {
            val i = items.indexOfFirst { it.fileId == fileId }
            if (i >= 0) {
                val cur = items[i]
                // 「已取消」是用户动作的终态：迟到的收尾（发送循环退出 / 晚到的结束帧）不许把它改写回失败或完成。
                val keepCancelled = cur.state == TransferState.Cancelled && state != TransferState.Cancelled
                if (!keepCancelled) {
                    items[i] = cur.copy(
                        state = state,
                        bytes = if (forceFull || state == TransferState.Done) cur.size else cur.bytes,
                        error = error,
                        path = path.ifEmpty { cur.path },
                        name = name.ifEmpty { cur.name }, // 名字与补充说明只在"文件落到哪"确定那一刻才有值，无关的状态更新不该抹回空
                        note = note.ifEmpty { cur.note },
                    )
                }
            }
        }
        LinkxRuntime.notifyChanged()
    }
}

/** 发送侧等待对端 FILE_DONE 回执的上限（入队 ≠ 送达）。 */
private const val ACK_TIMEOUT_MS = 30_000L

/** 收到"发端说发完了、可本端有洞"之后暂缓收尾等补发的窗口（与电脑端 `RESUME_HOLD_MS` 同值）。计时口径是**"多久没有补发进展"**：每落一块就重新计时（见 `writeChunk`）。走到"收尾时才发现洞"说明洞落在最后 4 MB 之内——更早的洞在分块还在流的时候就被在途续传接管了。 */
private const val RESUME_HOLD_MS = 20_000L

/** 一条传输最多允许几轮补发（与电脑端 `MAX_RESUME_TRIES` 同口径），发端与收端共用这一个数。 */
private const val MAX_RESUME_TRIES = 8
