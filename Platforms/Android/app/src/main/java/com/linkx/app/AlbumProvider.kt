package com.linkx.app

import android.content.Context
import android.content.pm.PackageManager
import android.graphics.Bitmap
import android.graphics.BitmapFactory
import android.net.Uri
import android.os.Build
import android.provider.MediaStore
import android.util.Log
import android.util.Size
import androidx.core.content.ContextCompat
import java.io.ByteArrayOutputStream
import java.util.ArrayDeque
import java.util.concurrent.LinkedBlockingQueue
import java.util.concurrent.atomic.AtomicInteger
import java.util.concurrent.atomic.AtomicLong

/**
 * 相册里的一条（照片或视频；只有 MediaStore 元信息，不含任何图像/视频字节）。
 */
data class AlbumPhoto(
    val id: Long,
    val name: String,
    val sizeBytes: Long,
    val mtimeMs: Long,
    val width: Int,
    val height: Int,
    val kind: Int = AlbumProvider.KIND_PHOTO,
    val durationMs: Long = 0L,
) {
    /**
     * 拼成 `id|name|size|mtime|width|height|kind|durationMs`（Rust 侧 `nativeSendAlbumList`
     * 的口径，见该函数注释与 `parse_album_rows` 的单测）。
     *
     * 名字里的 `|` **原样保留**：Rust 从右往左取数字段，剩下的整段才是名字，
     * 所以带竖线的文件名能安全往返。换行必须换掉——它是行分隔符，留着会把一行劈成两行。
     */
    fun toRow(): String =
        "$id|${name.replace('\n', ' ').replace('\r', ' ')}|$sizeBytes|$mtimeMs|$width|$height|$kind|$durationMs"
}

/**
 * 相册权限的真实形态。`SelectedOnly` 与 `Denied` 必须分开：Android 14 上用户点
 * "仅选定照片"是**给了权限**的，只是范围小，界面把它说成"未授权"就是在骗用户再点一次。
 */
enum class AlbumAccess { Full, SelectedOnly, Denied }

/**
 * 图片互传的**安卓应答方**：电脑问、手机答，手机侧没有相册浏览界面。
 *
 * ## 边界
 * 三条上行请求（清单 / 缩略图 / 原图）都在这里落地，应答分别走
 * [LinkxRuntime.sendAlbumList]、[LinkxRuntime.sendAlbumThumb]，原图复用已有的文件发送路径。
 *
 * ## 缩略图一律不落盘
 * 这是用户明确定的口径（关掉软件就没有缓存）：解码结果只在内存里压成 JPEG 直接发出，
 * 本文件不出现任何写文件的路径（`File` / `cacheDir` / `openOutputStream` 一处不许有）。
 * 唯一的 IO 是**只读**地把原图字节读进内存当兜底解码用（低版本没有系统缩略图时）。
 *
 * ## 线程
 * MediaStore 查询与缩略图解码都在本模块自己的串行线程上跑。绝不放进 [LinkxRuntime]
 * 的 `@Synchronized` 路径或事件泵：`ContentResolver.query` / `loadThumbnail` 是跨进程
 * Binder 调用，一次解码在低端机上要几十到几百毫秒，按住全局锁会让 BLE/TCP/JNI 一起停摆。
 * 原图发送单独一条线程：一张 20 MB 的照片要流式发好几秒，和"鼠标划过格子就要的缩略图"排一个队，界面就会看起来卡死。
 */
object AlbumProvider {
    private const val TAG = "LinkX.Album"

    /** 协议 `AlbumItem.kind` 的两个取值（电脑侧只认这两个，未知值按照片渲染）。 */
    const val KIND_PHOTO = 0
    const val KIND_VIDEO = 1

    /**
     * 清单与计数共用的过滤：相册页同时列**照片与视频**，音频和"其他文件"不许混进来。
     *
     * 用 `MediaStore.Files` 而不是分别查 Images / Video 两张表再合并：合并两份游标做分页
     * 要么把两表都读进内存、要么在两端各实现一遍归并排序，两种都比这一句 selection 贵，
     * 而且"按 DATE_ADDED 倒序翻到第 N 页"会在两表交界处错位。
     */
    private val MEDIA_SELECTION =
        "${MediaStore.Files.FileColumns.MEDIA_TYPE} IN (" +
            "${MediaStore.Files.FileColumns.MEDIA_TYPE_IMAGE}, " +
            "${MediaStore.Files.FileColumns.MEDIA_TYPE_VIDEO})"

    /** 缩略图长边硬上限：再大就不是"格子图"了，而且是纯带宽浪费（协议允许电脑要更小值）。 */
    private const val MAX_EDGE = 512

    /** 缩略图字节上限：超过就沿质量阶梯下调，仍超则缩尺寸，最后如实报错。 */
    private const val MAX_THUMB_BYTES = 200 * 1024

    /** JPEG 质量阶梯（从前往后逐级下调）。 */
    private val QUALITY_LADDER = intArrayOf(85, 75, 65, 55, 45)

    /** 尺寸也降过之后仍超限，再降一轮的最大次数（每张最多解 3 次）。 */
    private const val SHRINK_ROUNDS = 2

    /** 排队中的缩略图上限：电脑快速滚动时宁可明确作废旧的，也不把队列堆到无界。 */
    private const val MAX_PENDING_THUMBS = 24

    /** 一次原图批量上限（超过就是电脑侧的问题，这里只服务前若干张并如实说明）。 */
    private const val MAX_FULL_BATCH = 64

    /** 待发的原图批次上限（每批是一条串行任务）。 */
    private const val MAX_PENDING_FULL_BATCHES = 4

    /**
     * 判"迟到"的窗口：最近 [WINDOW_PAGES] 页里给出去的 id 仍算"在屏"。
     * 取 2 而不是 1，是因为电脑翻页时上一页的缩略图请求常常还在路上，
     * 只认最新一页会把它们全部误杀成"迟到"。
     */
    private const val WINDOW_PAGES = 2

    /** 清单请求的代次：只有仍是最新一代的那条才发出去（翻走的页不必再回）。 */
    private val listGen = AtomicLong(0L)

    /** 已接受的清单请求数（调试面据此判断"请求到没到手机"）。 */
    @Volatile
    var requestsAccepted: Int = 0
        private set

    /** 最近一次应答说明（`/state.host.album_last`、相册开关旁也读它）。 */
    @Volatile
    var lastStatus: String = "尚未收到相册请求"
        private set

    /** 最近一次失败原因；空串 = 一切正常。 */
    @Volatile
    var lastError: String = ""
        private set

    /**
     * 权限形态的缓存（[access] 顺带刷新）。
     *
     * 控制面每秒读的是这个值而不是现查：`checkSelfPermission` 也要跨进程问 PackageManager，
     * 而观测面刷新跑在 [LinkxRuntime] 的 tick 锁内——为一次观测去按锁就是自找"单轮长阻塞"。
     */
    @Volatile
    var cachedAccess: AlbumAccess = AlbumAccess.Denied
        private set

    // ---------- 任务队列（清单 + 缩略图共用一条串行线程） ----------

    private interface Job {
        fun run()
    }

    private class ListJob(ctx: Context, val page: Int, val perPage: Int, val gen: Long) : Job {
        private val app = ctx.applicationContext
        override fun run() = doList(app, page, perPage, gen)
    }

    private class ThumbJob(ctx: Context, val id: Long, val edge: Int) : Job {
        private val app = ctx.applicationContext
        override fun run() = doThumb(app, id, edge)
    }

    private class FullJob(ctx: Context, val ids: List<Long>) : Job {
        private val app = ctx.applicationContext
        override fun run() = doFull(app, ids)
    }

    private val jobs = LinkedBlockingQueue<Job>()
    private val fullJobs = LinkedBlockingQueue<Job>()
    private val pendingThumbs = AtomicInteger(0)
    private val pendingFull = AtomicInteger(0)

    /** 只在第一条相册任务真正到来时才起线程（功能关掉的启动路径连线程都不该有）。 */
    private val worker by lazy { startLoop("linkx-album", jobs) }
    private val fullWorker by lazy { startLoop("linkx-album-full", fullJobs) }

    private fun startLoop(name: String, queue: LinkedBlockingQueue<Job>): Thread {
        val t = Thread({
            while (true) {
                val job = try {
                    queue.take()
                } catch (e: InterruptedException) {
                    return@Thread
                }
                // 一条坏任务不许把线程带走：解码别的 App 生成的图，什么异常都可能遇到
                runCatching { job.run() }
                    .onFailure { Log.w(TAG, "相册任务异常：${it.javaClass.simpleName} ${it.message}") }
                if (job is ThumbJob) pendingThumbs.decrementAndGet()
                if (job is FullJob) pendingFull.decrementAndGet()
            }
        }, name).apply { isDaemon = true }
        t.start()
        return t
    }

    // ---------- 权限 ----------

    /**
     * 当前相册权限形态：每次现查（授权结果随时会变），并顺带把结果记进 [cachedAccess]。
     *
     * API 34+ 的"仅选定照片"= 只给了 `READ_MEDIA_VISUAL_USER_SELECTED`，
     * 此时 MediaStore 查询只会返回用户选中的那几张，**不是全量**。
     */
    fun access(ctx: Context): AlbumAccess {
        val granted: (String) -> Boolean = { name ->
            ContextCompat.checkSelfPermission(ctx, name) == PackageManager.PERMISSION_GRANTED
        }
        val now = when {
            Build.VERSION.SDK_INT >= 34 -> when {
                // 照片和视频是两条独立权限（Android 13 起）。只给其中一条也算"能读相册"：
                // MediaStore 会把没授权那一类整类过滤掉，清单不会出错，只是少一截。
                granted(android.Manifest.permission.READ_MEDIA_IMAGES) ||
                    granted(android.Manifest.permission.READ_MEDIA_VIDEO) -> AlbumAccess.Full
                granted(android.Manifest.permission.READ_MEDIA_VISUAL_USER_SELECTED) ->
                    AlbumAccess.SelectedOnly
                else -> AlbumAccess.Denied
            }
            Build.VERSION.SDK_INT >= 33 ->
                if (granted(android.Manifest.permission.READ_MEDIA_IMAGES) ||
                    granted(android.Manifest.permission.READ_MEDIA_VIDEO)
                ) {
                    AlbumAccess.Full
                } else {
                    AlbumAccess.Denied
                }
            else ->
                if (granted(android.Manifest.permission.READ_EXTERNAL_STORAGE)) AlbumAccess.Full
                else AlbumAccess.Denied
        }
        cachedAccess = now
        return now
    }

    /** 本机型上应当申请的相册权限名（一次给全，避免用户被问两次）。 */
    fun permissions(): Array<String> = when {
        Build.VERSION.SDK_INT >= 34 -> arrayOf(
            android.Manifest.permission.READ_MEDIA_IMAGES,
            android.Manifest.permission.READ_MEDIA_VIDEO,
            android.Manifest.permission.READ_MEDIA_VISUAL_USER_SELECTED,
        )
        Build.VERSION.SDK_INT >= 33 -> arrayOf(
            android.Manifest.permission.READ_MEDIA_IMAGES,
            android.Manifest.permission.READ_MEDIA_VIDEO,
        )
        else -> arrayOf(android.Manifest.permission.READ_EXTERNAL_STORAGE)
    }

    /** 给设置页的一句话状态（含降级说明；不写"全部照片"这种假话）。 */
    fun accessBrief(ctx: Context): String = when (access(ctx)) {
        AlbumAccess.Full -> "已授予：可读相册里的照片与视频"
        AlbumAccess.SelectedOnly ->
            "仅选定照片：只把你在系统选择器里选中的那些给电脑看，不是相册全量（要改范围就去系统设置重授）"
        AlbumAccess.Denied -> "未授予相册权限：清单会一直回空，电脑侧能看到这句原因"
    }

    // ---------- 请求入口（事件泵 / 调试面调用，一律立即返回） ----------

    /**
     * `ALBUM_LIST_REQ`：把请求排进相册线程，**立即返回**一句排队说明。
     *
     * 同一时刻只保留最新一条清单请求：分页翻走后上一页的结果发出去只会让电脑显示错的页。
     */
    fun requestList(ctx: Context, page: Int, perPage: Int): String {
        val gen = listGen.incrementAndGet()
        val dropped = jobs.removeAll { it is ListJob }
        if (dropped) lastStatus = "第 $page 页请求已接受（更早的一页请求作废，未发出）"
        worker // 懒起线程
        if (!jobs.offer(ListJob(ctx, page, perPage, gen))) {
            return "排队失败：相册任务队列已满"
        }
        requestsAccepted++
        if (dropped) return "已排队：第 $page 页（每页 $perPage 张），上一条清单请求已作废"
        return "已排队：第 $page 页（每页 $perPage 张）"
    }

    /**
     * `ALBUM_THUMB_REQ`：排一张缩略图。
     *
     * 排队满时**丢最旧的那张**并当场给它回一条 error 应答（电脑格子会显示原因），
     * 而不是把请求吞掉：吞掉的请求在电脑侧是"永远转圈"，比一句"这张作废"难懂十倍。
     */
    fun requestThumb(ctx: Context, id: Long, edge: Int): String {
        if (jobs.any { it is ThumbJob && it.id == id }) return "同一张已在队列中（id=$id），本条并掉"
        if (pendingThumbs.get() >= MAX_PENDING_THUMBS) {
            val oldest = jobs.firstOrNull { it is ThumbJob } as? ThumbJob ?: return "队列状态异常（id=$id）"
            if (jobs.remove(oldest)) {
                pendingThumbs.decrementAndGet()
                sendThumbError(oldest.id, oldest.edge, "手机侧排队已满，这张已作废（重新滚到它可再要一次）")
            }
        }
        worker
        if (!jobs.offer(ThumbJob(ctx, id, edge))) return "排队失败：相册任务队列已满"
        pendingThumbs.incrementAndGet()
        requestsAccepted++
        return "已排队：缩略图 id=$id（长边 $edge）"
    }

    /**
     * `ALBUM_FULL_REQ`：把这些照片当普通文件发回去（`FileMeta.album_id` = 照片 id）。
     *
     * 批次上限 [MAX_FULL_BATCH]：超出部分**明确说没发**，不做"默默只发前几张"。
     */
    fun requestFull(ctx: Context, ids: List<Long>): String {
        if (ids.isEmpty()) return "原图清单为空，没有可发送的照片"
        if (!Features.enabled(Module.FileTransfer)) {
            lastError = "文件传输已关闭，原图无法发送"
            return lastError
        }
        val served = ids.take(MAX_FULL_BATCH)
        val skipped = ids.size - served.size
        if (pendingFull.get() >= MAX_PENDING_FULL_BATCHES) {
            lastError = "原图批次排队已满（${pendingFull.get()} 批在途），这一批没有开始"
            return lastError
        }
        fullWorker
        if (!fullJobs.offer(FullJob(ctx, served))) return "排队失败：原图队列已满"
        pendingFull.incrementAndGet()
        requestsAccepted += served.size
        return "已排队：${served.size} 张原图" +
            if (skipped > 0) "，另有 $skipped 张超出单批上限 $MAX_FULL_BATCH，未发送" else ""
    }

    /** 当前在途/排队情况（调试面取证）。 */
    fun queueBrief(): String =
        "清单+缩略图队列 ${jobs.size}（缩略图 ${pendingThumbs.get()}/$MAX_PENDING_THUMBS），" +
            "原图批次 ${pendingFull.get()}/$MAX_PENDING_FULL_BATCHES"

    /**
     * 整批原图请求被拒（相册或文件传输开关关掉）。
     *
     * 只替**第一张**发两帧（0 字节 FILE_META + 失败的 FILE_DONE，见
     * [FileTransfer.declineAlbumFull]），原因里带上"本批 N 张全部未发送"：
     * 逐张发会把一批变成几百帧噪音，而电脑要看的只是那一句话。
     */
    fun rejectFullBatch(ids: List<Long>, reason: String) {
        val id = ids.firstOrNull() ?: return
        val why = if (ids.size > 1) "$reason（本批 ${ids.size} 张全部未发送）" else reason
        lastError = why
        lastStatus = "原图请求已拒：$why"
        runCatching { FileTransfer.declineAlbumFull(id, why) }
            .onFailure { Log.w(TAG, "原图拒绝帧没能发出：${it.javaClass.simpleName} ${it.message}") }
    }

    // ---------- 工作线程上的实际动作 ----------

    private fun doList(ctx: Context, page: Int, perPage: Int, gen: Long) {
        if (gen != listGen.get()) {
            lastStatus = "第 $page 页清单已作废（电脑已经要了更新的页），未发出"
            return
        }
        val per = perPage.coerceIn(1, 500)
        val p = page.coerceAtLeast(0)
        val (rows, total, error) = queryPage(ctx, p, per)
        if (rows.isNotEmpty()) {
            synchronized(recentLock) {
                // 顺手记住这一页每条的类型：电脑回头要缩略图/原图时只给 id
                recentPages.addLast(rows.associate { it.id to it.kind })
                while (recentPages.size > WINDOW_PAGES) recentPages.removeFirst()
            }
        }
        val payload = rows.joinToString("\n") { it.toRow() }
        val sent = LinkxRuntime.sendAlbumList(
            page = p,
            total = total,
            error = error,
            rows = payload,
        )
        lastStatus = if (sent) {
            "第 $p 页已回：${rows.size}/$total 张" + if (error.isEmpty()) "" else "（附带原因：$error）"
        } else {
            "第 $p 页清单没能发出（未配对 / TCP 未绑定 / native 调用失败）"
        }
        if (!sent) lastError = lastStatus
    }

    private fun doThumb(ctx: Context, id: Long, edge: Int) {
        // 迟到判定：id 不在最近两页的清单里 = 电脑已经翻走了，这张发出去也没人画
        if (!isRecent(id)) {
            lastStatus = "缩略图 id=$id 不在最近 $WINDOW_PAGES 页里，判为迟到，已丢弃（不发）"
            return
        }
        val e = edge.coerceIn(1, MAX_EDGE)
        val outcome = runCatching { buildThumb(ctx, id, e) }
        val thumb = outcome.getOrNull()
        val error = if (outcome.isSuccess) "" else "缩略图生成失败：${outcome.exceptionOrNull()?.message}"
        if (thumb == null) {
            sendThumbError(id, e, error.ifEmpty { "缩略图生成失败（原因未知）" })
            lastError = error
        } else {
            val sent = LinkxRuntime.sendAlbumThumb(id, thumb.edge, thumb.width, thumb.height, thumb.jpeg, thumb.error)
            lastStatus = when {
                sent && thumb.error.isEmpty() ->
                    "缩略图 #$id 已回：${thumb.width}×${thumb.height}，${thumb.jpeg.size} 字节"
                sent -> "缩略图 #$id 已回（带原因：${thumb.error}）"
                else -> "缩略图 #$id 没能发出（未配对 / TCP 未绑定）"
            }
            if (!sent) lastError = lastStatus
        }
    }

    private fun doFull(ctx: Context, ids: List<Long>) {
        for (id in ids) {
            val uri = contentUri(ctx, id)
            // 复用文件发送的生产路径：流式、背压、逐块校验、FILE_DONE 摘要一概不另发明
            runCatching { FileTransfer.send(uri, albumId = id) }
                .onFailure {
                    lastError = "原图 id=$id 发送失败：${it.javaClass.simpleName} ${it.message}"
                    Log.w(TAG, lastError)
                }
        }
        lastStatus = "原图批次已处理：${ids.size} 张（逐张走文件通道，进度在「文件」页）"
    }

    private fun sendThumbError(id: Long, edge: Int, reason: String) {
        LinkxRuntime.sendAlbumThumb(id, edge, 0, 0, ByteArray(0), reason)
    }

    // ---------- MediaStore 查询 ----------

    private val recentPages = ArrayDeque<Map<Long, Int>>()
    private val recentLock = Any()

    private fun isRecent(id: Long): Boolean = synchronized(recentLock) {
        // 一次清单都没发过就无从判"迟到"（电脑可能重启后拿旧 id 直接要图）：放行
        recentPages.isEmpty() || recentPages.any { id in it }
    }

    /**
     * 这一条是照片还是视频（协议 `kind`）。
     *
     * 首选最近两页清单里记下的映射——清单是唯一知道类型的地方，而电脑只把 id 传回来。
     * 缓存里没有（电脑重启后拿旧 id 直接要原图）就现查一次：猜错类型的代价是
     * "视频按图片表去 openInputStream 读不到"，一句错误提示换一次查询不亏。
     */
    private fun kindOf(ctx: Context, id: Long): Int {
        synchronized(recentLock) { recentPages.firstNotNullOfOrNull { it[id] } }?.let { return it }
        return runCatching {
            ctx.contentResolver.query(
                MediaStore.Files.getContentUri("external"),
                arrayOf(MediaStore.Files.FileColumns.MEDIA_TYPE),
                "${MediaStore.Files.FileColumns._ID} = ?",
                arrayOf(id.toString()),
                null,
            )?.use { c ->
                if (!c.moveToFirst()) return@runCatching KIND_PHOTO
                if (c.getInt(0) == MediaStore.Files.FileColumns.MEDIA_TYPE_VIDEO) KIND_VIDEO
                else KIND_PHOTO
            } ?: KIND_PHOTO
        }.getOrDefault(KIND_PHOTO)
    }

    /** 按类型挑表：`content://media/external/{images|video}/media/<id>`，读原文件与缩略图都用它。 */
    private fun contentUri(ctx: Context, id: Long): Uri {
        val base = if (kindOf(ctx, id) == KIND_VIDEO) {
            MediaStore.Video.Media.EXTERNAL_CONTENT_URI
        } else {
            MediaStore.Images.Media.EXTERNAL_CONTENT_URI
        }
        return Uri.withAppendedPath(base, id.toString())
    }

    /**
     * 取一页清单。返回（本页行, 相册总张数, 错误说明）；错误说明非空 = 这一页有要告诉电脑的事。
     *
     * 只取元信息列，**不碰图像字节**。分页用 `moveToPosition` 跳过偏移：
     * `MediaStore` 的游标是分窗取回的，比把整表拉进内存再切便宜，也不依赖各版本
     * 行为不一致的 queryArgs limit/offset。
     */
    private fun queryPage(ctx: Context, page: Int, per: Int): Triple<List<AlbumPhoto>, Int, String> {
        val access = access(ctx)
        if (access == AlbumAccess.Denied) {
            // 权限没给是最常见的一次空应答，原因必须原样送到电脑侧，而不是显示"相册为空"
            return Triple(emptyList(), 0, "手机未授予相册权限（设置 → 应用 → LinkX → 图片和视频）")
        }
        val total = countTotal(ctx)
        if (total < 0) {
            return Triple(emptyList(), 0, "读取相册数量失败（MediaStore 查询异常，稍后重试）")
        }
        val offset = page.toLong() * per.toLong()
        if (offset >= total) {
            return Triple(
                emptyList(), total,
                "第 $page 页没有内容：相册共 $total 项、每页 $per 项",
            )
        }
        val projection = arrayOf(
            MediaStore.Files.FileColumns._ID,
            MediaStore.Files.FileColumns.DISPLAY_NAME,
            MediaStore.Files.FileColumns.SIZE,
            MediaStore.Files.FileColumns.DATE_MODIFIED,
            MediaStore.Files.FileColumns.WIDTH,
            MediaStore.Files.FileColumns.HEIGHT,
            MediaStore.Files.FileColumns.MEDIA_TYPE,
            MediaStore.Video.Media.DURATION,
        )
        val rows = ArrayList<AlbumPhoto>(per)
        var readError = ""
        runCatching {
            ctx.contentResolver.query(
                MediaStore.Files.getContentUri("external"),
                projection,
                MEDIA_SELECTION,
                null,
                "${MediaStore.Files.FileColumns.DATE_ADDED} DESC",
            )?.use { c ->
                if (!c.moveToPosition(offset.toInt())) return@use
                val idIx = c.getColumnIndexOrThrow(MediaStore.Files.FileColumns._ID)
                val nameIx = c.getColumnIndex(MediaStore.Files.FileColumns.DISPLAY_NAME)
                val sizeIx = c.getColumnIndex(MediaStore.Files.FileColumns.SIZE)
                val mtimeIx = c.getColumnIndex(MediaStore.Files.FileColumns.DATE_MODIFIED)
                val wIx = c.getColumnIndex(MediaStore.Files.FileColumns.WIDTH)
                val hIx = c.getColumnIndex(MediaStore.Files.FileColumns.HEIGHT)
                val typeIx = c.getColumnIndex(MediaStore.Files.FileColumns.MEDIA_TYPE)
                val durIx = c.getColumnIndex(MediaStore.Video.Media.DURATION)
                while (rows.size < per) {
                    val id = c.getLong(idIx)
                    val isVideo = typeIx >= 0 && !c.isNull(typeIx) &&
                        c.getInt(typeIx) == MediaStore.Files.FileColumns.MEDIA_TYPE_VIDEO
                    val name = if (nameIx >= 0) c.getString(nameIx)
                        else if (isVideo) "VID_$id.mp4" else "IMG_$id.jpg"
                    // 名字兜底也必须带类型：电脑侧导出时用的就是这个文件名
                    val safeName = name?.takeIf { it.isNotEmpty() }
                        ?: if (isVideo) "VID_$id.mp4" else "IMG_$id.jpg"
                    val size = if (sizeIx >= 0 && !c.isNull(sizeIx)) c.getLong(sizeIx) else 0L
                    // DATE_MODIFIED 是秒；清单按毫秒回给电脑，两端口径要一致
                    val mtime = if (mtimeIx >= 0 && !c.isNull(mtimeIx)) c.getLong(mtimeIx) * 1000L else 0L
                    val w = if (wIx >= 0 && !c.isNull(wIx)) c.getInt(wIx) else 0
                    val h = if (hIx >= 0 && !c.isNull(hIx)) c.getInt(hIx) else 0
                    // DURATION 是毫秒，与协议字段同单位；照片这一列是 NULL/0
                    val dur = if (isVideo && durIx >= 0 && !c.isNull(durIx)) c.getLong(durIx) else 0L
                    rows.add(
                        AlbumPhoto(
                            id,
                            safeName,
                            size,
                            mtime,
                            w,
                            h,
                            if (isVideo) KIND_VIDEO else KIND_PHOTO,
                            dur,
                        ),
                    )
                    if (!c.moveToNext()) break
                }
            }
        }.onFailure {
            readError = "读取相册清单失败：${it.javaClass.simpleName} ${it.message}"
            Log.w(TAG, readError)
        }
        if (readError.isNotEmpty()) return Triple(emptyList(), total.coerceAtLeast(0), readError)
        val note = when {
            rows.isEmpty() && total > 0 -> "第 $page 页取不到行（相册在变动中，请刷新）"
            total == 0 -> "本机相册里没有照片或视频"
            // 仅选定照片：这一页是真的，但范围不是全量。合同上 error 非空表示"这页没取到"，
            // 这里仍然带上它——因为"假装是全量"的代价更大，电脑侧宁可多显示一句提示。
            access == AlbumAccess.SelectedOnly ->
                "仅选定照片授权：只列出你选中的这些（可见 $total 项），不是相册全部"
            else -> ""
        }
        return Triple(rows, total, note)
    }

    /** 相册总项数（照片 + 视频）；`COUNT(*)` 读不出来时回落成逐行计数。返回 -1 = 查询失败。 */
    private fun countTotal(ctx: Context): Int {
        val uri = MediaStore.Files.getContentUri("external")
        runCatching {
            ctx.contentResolver.query(uri, arrayOf("COUNT(*)"), MEDIA_SELECTION, null, null)
                ?.use { c -> if (c.moveToFirst()) return c.getInt(0) }
        }
        return runCatching {
            ctx.contentResolver.query(
                uri,
                arrayOf(MediaStore.Files.FileColumns._ID),
                MEDIA_SELECTION,
                null,
                null,
            )?.use { it.count } ?: -1
        }.getOrDefault(-1)
    }

    // ---------- 缩略图：现场生成，只在内存里 ----------

    private class Thumb(
        val edge: Int,
        val width: Int,
        val height: Int,
        val jpeg: ByteArray,
        val error: String,
    )

    /**
     * 生成一张缩略图（长边夹到 ≤[MAX_EDGE]，JPEG 压到 ≤[MAX_THUMB_BYTES]）。
     *
     * 一次只解一张（本函数只被串行线程调用），并且**从不**把整批原图读进内存。
     * 全程不落盘：`ByteArrayOutputStream` 只是内存缓冲，压完直接经 JNI 发出。
     */
    private fun buildThumb(ctx: Context, id: Long, edge: Int): Thumb {
        val uri = contentUri(ctx, id)
        val src = decodeSource(ctx, uri, edge)
            ?: return Thumb(edge, 0, 0, ByteArray(0), "取不到这张缩略图（权限已收回，或文件已不在相册里）")
        var img: Bitmap = scaleToLongEdge(src, edge)
        if (img !== src) src.recycle()
        var qualityIdx = 0
        var round = 0
        while (true) {
            val out = ByteArrayOutputStream(32 * 1024)
            img.compress(Bitmap.CompressFormat.JPEG, QUALITY_LADDER[qualityIdx], out)
            val bytes = out.toByteArray()
            if (bytes.size <= MAX_THUMB_BYTES) {
                val w = img.width
                val h = img.height
                img.recycle()
                val longEdge = maxOf(w, h)
                return Thumb(longEdge, w, h, bytes, "")
            }
            qualityIdx++
            if (qualityIdx >= QUALITY_LADDER.size) {
                qualityIdx = 0
                round++
                if (round > SHRINK_ROUNDS) {
                    val w = img.width
                    val h = img.height
                    img.recycle()
                    // 压到 200 KB 之下做不到就说做不到，别把超大帧硬发出去
                    return Thumb(
                        edge, w, h, ByteArray(0),
                        "这张压不进 200 KB（最低质量后仍有 ${bytes.size} 字节），未发送",
                    )
                }
                val shrunk = runCatching {
                    Bitmap.createScaledBitmap(img, (img.width * 3 / 4).coerceAtLeast(1), (img.height * 3 / 4).coerceAtLeast(1), true)
                }.getOrNull() ?: break
                if (shrunk !== img) img.recycle()
                img = shrunk
            }
        }
        val w = img.width
        val h = img.height
        img.recycle()
        return Thumb(edge, w, h, ByteArray(0), "缩略图压缩失败（图片尺寸异常）")
    }

    /**
     * 解出一张小尺寸位图（照片取首帧意义上的缩略图，视频取代表帧）。
     *
     * - API 29+：`ContentResolver.loadThumbnail` —— 照片与视频都支持，系统内部走缓存与
     *   采样，代价最低，也是唯一能对视频"就地取帧"的正规入口。
     * - 更低版本：查系统已生成的缩略图表（照片/视频是**两张表**，选错表只会返回 null），
     *   照片再回落到**只读**流 + `inSampleSize` 就地降采样；视频不回落——把 mp4 字节流喂给
     *   `BitmapFactory` 只会白读几十 MB 然后返回 null。
     */
    private fun decodeSource(ctx: Context, uri: Uri, edge: Int): Bitmap? {
        if (Build.VERSION.SDK_INT >= 29) {
            runCatching { ctx.contentResolver.loadThumbnail(uri, Size(edge, edge), null) }
                .getOrNull()?.let { return it }
            // loadThumbnail 在个别机型上对刚插入/已移动的照片抛异常，继续走下面的兜底
        }
        val id = idOf(uri)
        val isVideo = uri.pathSegments.contains("video")
        val sys = runCatching {
            @Suppress("DEPRECATION")
            if (isVideo) {
                MediaStore.Video.Thumbnails.getThumbnail(
                    ctx.contentResolver,
                    id,
                    MediaStore.Video.Thumbnails.MINI_KIND,
                    null,
                )
            } else {
                MediaStore.Images.Thumbnails.getThumbnail(
                    ctx.contentResolver,
                    id,
                    MediaStore.Images.Thumbnails.MINI_KIND,
                    null,
                )
            }
        }.getOrNull()
        if (sys != null) return sys
        return if (isVideo) null else decodeSampled(ctx, uri, edge)
    }

    /** `content://media/external/images/media/<id>` → id；解不出来返回 -1（查询随之失败，如实报）。 */
    private fun idOf(uri: Uri): Long = uri.lastPathSegment?.toLongOrNull() ?: -1L

    /** 只读地按边界尺寸降采样解码（不落盘、不驻留整张原图）。 */
    private fun decodeSampled(ctx: Context, uri: Uri, edge: Int): Bitmap? {
        val bounds = BitmapFactory.Options().apply { inJustDecodeBounds = true }
        runCatching { ctx.contentResolver.openInputStream(uri)?.use { BitmapFactory.decodeStream(it, null, bounds) } }
        val w = bounds.outWidth
        val h = bounds.outHeight
        if (w <= 0 || h <= 0) return null
        var sample = 1
        while (maxOf(w, h) / (sample * 2) >= edge) sample *= 2
        val opts = BitmapFactory.Options().apply { inSampleSize = sample }
        return runCatching {
            ctx.contentResolver.openInputStream(uri)?.use { BitmapFactory.decodeStream(it, null, opts) }
        }.getOrNull()
    }

    /** 把长边夹到 [edge]（等比），尺寸已合规则原样返回（不复制、不白扔内存）。 */
    private fun scaleToLongEdge(src: Bitmap, edge: Int): Bitmap {
        val longEdge = maxOf(src.width, src.height)
        if (longEdge <= edge || longEdge <= 0) return src
        val ratio = edge.toFloat() / longEdge
        val w = (src.width * ratio).toInt().coerceAtLeast(1)
        val h = (src.height * ratio).toInt().coerceAtLeast(1)
        return runCatching { Bitmap.createScaledBitmap(src, w, h, true) }.getOrDefault(src)
    }
}
