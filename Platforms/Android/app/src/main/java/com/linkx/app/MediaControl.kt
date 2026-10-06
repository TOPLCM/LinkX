package com.linkx.app

import android.content.Context
import android.graphics.Bitmap
import android.media.AudioManager
import android.media.MediaMetadata
import android.media.session.MediaController
import android.media.session.MediaSessionManager
import android.media.session.PlaybackState
import android.os.SystemClock
import android.util.Log
import java.util.concurrent.Executors
import java.util.concurrent.atomic.AtomicBoolean
import java.util.concurrent.atomic.AtomicInteger

/**
 * 媒体控制：手机在放什么 → 推给电脑；电脑点按钮 → 手机执行。
 * 只读系统 MediaSession 的元数据与播放状态，**不搬运音频**、不引第三方库。
 *
 * 依赖通知监听权限：`getActiveSessions()` 要求调用方是已启用的
 * NotificationListenerService（系统硬约束）。"媒体控制用不了"多半是通知权限没给，
 * 界面必须把这句说明白，别让用户以为蓝牙坏了（[lastSkip] 就是干这个的）。
 *
 * 线程：采样与指令执行都在本模块自己的单线程 executor，绝不进 [LinkxRuntime] 的
 * `@Synchronized` 路径——`getActiveSessions` 与 `Settings.Secure` 是跨进程 Binder 调用，
 * 个别机型几十毫秒起步；锁内执行会按住全局锁，BLE 分片、TCP 帧、所有 JNI 一起停摆。
 * 同一个串行线程也顺带解决了采样与指令共写 [lastSkip] 的竞态。
 */
object MediaControl {

    private const val TAG = "LinkX.Media"

    /** 常规采样间隔。指令执行后会**立刻**补采一次（见 `submitCommand`），所以这里可以放宽。 */
    private const val POLL_MS = 3_000L

    /** 播放中即使内容没变，也至少隔这么久推一次进度（让电脑侧的进度条会动）。 */
    private const val POSITION_PUSH_MS = 5_000L

    /** 电脑没给 delta 时的快进/快退步长。 */
    private const val SEEK_STEP_MS = 10_000L

    /** 排队中的指令上限：对端疯狂刷按钮时宁可明确丢弃，也不能把队列堆到无界。 */
    private const val MAX_PENDING_CMDS = 8

    /** 封面边长与字节上限：压不进上限就当作"这首没封面"，不把窄口当文件通道用。 */
    private const val COVER_MAX_PX = 200
    private const val COVER_MAX_BYTES = 32 * 1024

    private val busy = AtomicBoolean(false)
    private val pendingCmds = AtomicInteger(0)
    private val pool by lazy {
        Executors.newSingleThreadExecutor { r -> Thread(r, "linkx-media").apply { isDaemon = true } }
    }

    private var lastAt = 0L
    private var lastPushAt = 0L
    private var lastSignature = ""
    private var coverSentKey = ""

    /** 最近一次成功推出去的内容（控制面与 UI 据此判断"到底有没有在采样"）。 */
    @Volatile
    var lastSent: String = ""
        private set

    /** 最近一次**没**推的原因。空串 = 一切正常。 */
    @Volatile
    var lastSkip: String = ""
        private set

    /** 最近一条播放指令的执行说明（控制面 `media_cmd` 与验收脚本读它）。 */
    @Volatile
    var lastCommand: String = ""
        private set

    /** 最近一次采样到的本机播放状态，供「媒体」页直接读：UI 自己查 MediaSession 等于在重组里做跨进程 Binder 调用。 */
    @Volatile
    var current: Playback? = null
        private set

    /** 一次采样的展示字段（与推给电脑的 `MediaState` 同源） */
    data class Playback(
        val pkg: String,
        val title: String,
        val artist: String,
        val album: String,
        val playing: Boolean,
        val positionMs: Long,
        val durationMs: Long,
        val volume: Int,
    )

    /** 周期入口，由 tick 线程调用；内部节流 + 单次在途，本身不阻塞。 */
    fun tick(ctx: Context) {
        // 节流用 elapsedRealtime：挂钟会因改系统时间/时区回跳，`now - lastAt` 变负后采样从此静默停摆。
        val now = SystemClock.elapsedRealtime()
        if (now - lastAt < POLL_MS) return
        lastAt = now
        if (!busy.compareAndSet(false, true)) return
        val app = ctx.applicationContext // 不持有 Activity：采样可能活得比它久
        pool.execute {
            try {
                sampleAndPush(app, SystemClock.elapsedRealtime())
            } catch (e: Exception) {
                // 媒体会话来自别的进程，元数据缺哪个字段都有可能；绝不让它冒到线程外
                Log.w(TAG, "采样失败：${e.message}")
                lastSkip = "采样异常：${e.message}"
            } finally {
                busy.set(false)
            }
        }
    }

    /** 把一条指令排到媒体线程执行，**立即返回**；返回值只说明"排没排上"，执行结果在 [lastCommand]。绝不能就地做 Binder 调用：调用方事件泵持有全局锁。 */
    fun submitCommand(ctx: Context, action: Int, volume: Int, deltaMs: Long): String {
        if (pendingCmds.get() >= MAX_PENDING_CMDS) {
            lastSkip = "指令排队已满（媒体线程未响应），本条已丢弃"
            return "已丢弃：媒体线程排队已满"
        }
        pendingCmds.incrementAndGet()
        val app = ctx.applicationContext
        pool.execute {
            try {
                lastCommand = handleCommand(app, action, volume, deltaMs)
                // 执行完立刻补采：电脑显示手机上报值（不自己记账）。音量同步生效必带新值；切歌异步可能仍采旧态——不假报，交下一轮。
                runCatching { sampleAndPush(app, SystemClock.elapsedRealtime()) }
            } catch (e: Exception) {
                lastCommand = "执行异常：${e.message}"
                Log.w(TAG, "指令执行失败：${e.message}")
            } finally {
                pendingCmds.decrementAndGet()
            }
        }
        return "已排队：动作 $action"
    }

    private fun sampleAndPush(ctx: Context, nowMs: Long) {
        val (controller, state) = pickController(ctx) ?: run { current = null; return }
        val md = controller.metadata
        val title = md?.getString(MediaMetadata.METADATA_KEY_TITLE).orEmpty()
        val artist = md?.getString(MediaMetadata.METADATA_KEY_ARTIST).orEmpty()
        val album = md?.getString(MediaMetadata.METADATA_KEY_ALBUM).orEmpty()
        val playing = state?.state == PlaybackState.STATE_PLAYING
        // PlaybackState.position 是"上次更新时的位置"，不会自己随时间走；原样传，平滑交给电脑侧按倍速推算。
        val position = state?.position ?: 0L
        val duration = md?.getLong(MediaMetadata.METADATA_KEY_DURATION) ?: 0L
        val speed = state?.let { if (it.playbackSpeed > 0f) it.playbackSpeed else 1.0f } ?: 1.0f
        val volume = mediaVolumePercent(ctx)
        val pkg = controller.packageName.orEmpty()
        current = Playback(pkg, title, artist, album, playing, position, duration, volume)

        // 只在"内容变了"或"到了进度心跳"时推：静止画面每 3 秒一条帧会把窄口占满。
        // 签名**必须含音量**：不含时电脑改了音量手机却判"未变化"不推，连点几下算出的目标值还是同一个数。
        val signature = "$pkg|$title|$artist|$album|$playing|$duration|${(speed * 100).toInt()}|$volume"
        val changed = signature != lastSignature
        // 心跳**不看 playing**：暂停态下一台刚重启的电脑会一直空白——手机不知道对面已是全新会话。
        val heartbeat = nowMs - lastPushAt >= POSITION_PUSH_MS
        if (!changed && !heartbeat) {
            lastSkip = "状态未变化（节流）"
            return
        }
        // 签名**只能在真的推出去之后**才更新：提前更新会把"未配对时推送失败"记成已发，配对后电脑侧永远空白。
        val sent = LinkxRuntime.sendMediaState(
            pkg = pkg,
            title = title,
            artist = artist,
            album = album,
            playing = playing,
            positionMs = position,
            durationMs = duration,
            speedX100 = (speed * 100).toInt(),
            volume = volume,
            // 协议时间戳用挂钟（对端判新旧、日志可读）；节流用 elapsedRealtime——用途不同，不能共用。
            tsMs = System.currentTimeMillis(),
        )
        if (sent) {
            lastSignature = signature
            lastPushAt = nowMs
            lastSent = "$pkg|$title"
            lastSkip = ""
            // 状态先落地再补封面：电脑拿 track_key 认图，顺序反了就会把上一首的图配这一首的歌名
            pushCover(pkg, title, artist, md)
        } else {
            // false 有两种成因：未配对，或 JNI 调用没成功（.so 缺符号）。措辞必须覆盖两者，否则排查被带偏。
            lastSkip = "推送被拒：未配对，或 native 调用失败（见 logcat nativeSendMediaState）"
        }
    }

    /**
     * 局域网刚通：清掉"这首已经交代过"的记号，下一轮采样就会把当前曲目的封面补发一次。
     * 电脑重启或重新配对之后手上是没有封面的，不补就要等用户切歌。
     */
    fun onLanUp() {
        coverSentKey = ""
    }

    /**
     * 推当前曲目的封面。**只在局域网通时推**（纯蓝牙传几十 KB 会挤掉通知与剪贴板，引擎也会直接拒收），
     * 同一首只推一次：`coverSentKey` 记的是"已经交代过的那首"，无论它当时有没有封面。
     */
    private fun pushCover(pkg: String, title: String, artist: String, md: MediaMetadata?) {
        val key = "$pkg|$title|$artist"
        if (key == coverSentKey || !LinkxRuntime.tcpBound) return
        val bmp = md?.getBitmap(MediaMetadata.METADATA_KEY_ALBUM_ART)
            ?: md?.getBitmap(MediaMetadata.METADATA_KEY_ART)
        val jpeg = bmp?.let { encodeCover(it) }
        if (jpeg == null) {
            // 这首就是没封面（或压不进上限）：记下来，别每轮重编一次
            coverSentKey = key
            return
        }
        if (LinkxRuntime.sendMediaCover(key, jpeg, System.currentTimeMillis())) {
            Log.i(TAG, "cover.sent ${bmp.width}x${bmp.height} ${jpeg.size}B")
            coverSentKey = key
        }
    }

    /** 缩到 [COVER_MAX_PX] 边长以内再压 JPEG，质量逐级降直到不超过 [COVER_MAX_BYTES]。 */
    private fun encodeCover(src: Bitmap): ByteArray? {
        val side = maxOf(src.width, src.height)
        val scaled = if (side <= COVER_MAX_PX) src else {
            val k = COVER_MAX_PX.toFloat() / side
            Bitmap.createScaledBitmap(src, (src.width * k).toInt().coerceAtLeast(1),
                (src.height * k).toInt().coerceAtLeast(1), true)
        }
        // 缩放出来那张是 native 内存（一首 1000×1000 的图 ≈ 4 MB），不回收就得等 GC 才肯放手
        val recycled = scaled !== src
        try {
            val out = java.io.ByteArrayOutputStream(COVER_MAX_BYTES)
            for (q in intArrayOf(80, 60, 40)) {
                out.reset()
                scaled.compress(Bitmap.CompressFormat.JPEG, q, out)
                if (out.size() <= COVER_MAX_BYTES) return out.toByteArray()
            }
            return if (out.size() <= COVER_MAX_BYTES) out.toByteArray() else null
        } finally {
            if (recycled) scaled.recycle()
        }
    }


    /** 目标应用没实现某个动作时，安卓是**静默忽略**的：先查 `PlaybackState.actions` 能力位，
     *  做不到就直说，不能报"已下发"而手机什么都没做。 */
    private fun supports(state: PlaybackState?, action: Long): Boolean =
        state != null && state.actions and action != 0L

    private fun unsupported(what: String): String =
        "手机上的播放器不支持$what（当前媒体会话没有这个能力）"

    /** 执行指令，返回给控制面/日志的说明。动作码与 `Proto/linkx/v1/media.proto` 的 `MediaCommand.Action` 一一对应。 */
    private fun handleCommand(ctx: Context, action: Int, volume: Int, deltaMs: Long): String {
        val picked = pickController(ctx) ?: return "手机当前没有可控制的媒体会话（$lastSkip）"
        val (controller, state) = picked
        val tc = controller.transportControls
            ?: return "该应用未提供传输控制接口（只能读状态，不能控制）"
        val pos = currentPosition(state)
        return when (action) {
            // 用精确的 play/pause 而非盲发 toggle：对端已在目标态时 toggle 会把它反过来。
            ACTION_PLAY_PAUSE ->
                if (state?.state == PlaybackState.STATE_PLAYING) {
                    if (!supports(state, PlaybackState.ACTION_PAUSE)) return unsupported("暂停")
                    tc.pause(); "已下发：暂停"
                } else {
                    if (!supports(state, PlaybackState.ACTION_PLAY)) return unsupported("播放")
                    tc.play(); "已下发：播放"
                }
            ACTION_PLAY -> {
                if (!supports(state, PlaybackState.ACTION_PLAY)) return unsupported("播放")
                tc.play(); "已下发：播放"
            }
            ACTION_PAUSE -> {
                if (!supports(state, PlaybackState.ACTION_PAUSE)) return unsupported("暂停")
                tc.pause(); "已下发：暂停"
            }
            // 安卓对"没实现的动作"是静默忽略：不查能力位就会报"已下发"而手机什么都没做
            ACTION_NEXT -> {
                if (!supports(state, PlaybackState.ACTION_SKIP_TO_NEXT)) return unsupported("下一首")
                tc.skipToNext(); "已下发：下一首"
            }
            ACTION_PREV -> {
                if (!supports(state, PlaybackState.ACTION_SKIP_TO_PREVIOUS)) return unsupported("上一首")
                tc.skipToPrevious(); "已下发：上一首"
            }
            ACTION_STOP -> {
                if (!supports(state, PlaybackState.ACTION_STOP)) return unsupported("停止")
                tc.stop(); "已下发：停止"
            }
            ACTION_SEEK_FWD -> {
                if (!supports(state, PlaybackState.ACTION_SEEK_TO)) return unsupported("快进")
                tc.seekTo((pos + seekDelta(deltaMs)).coerceAtLeast(0L))
                "已下发：快进"
            }
            ACTION_SEEK_BACK -> {
                if (!supports(state, PlaybackState.ACTION_SEEK_TO)) return unsupported("快退")
                tc.seekTo((pos - seekDelta(deltaMs)).coerceAtLeast(0L))
                "已下发：快退"
            }
            ACTION_SET_VOLUME -> {
                setMediaVolume(ctx, volume.coerceIn(0, 100))
                val now = mediaVolumePercent(ctx)
                // 报**实际落到的百分比**：安卓音量分级（常见 15 档），请求 30% 可能落在 33%；now<0 是"读不到"而非 0%，只说未确认。
                if (now < 0) "请求设为 $volume%：音量读不到，未确认"
                else "媒体音量已设为 $now%（请求 $volume%）"
            }
            else -> "未知播放指令动作 $action（未执行）"
        }
    }

    private fun seekDelta(deltaMs: Long): Long = if (deltaMs > 0) deltaMs else SEEK_STEP_MS

    /**
     * 当前播放位置（按倍速外推到"现在"）。
     *
     * `PlaybackState.position` 是**上次进度更新时**的值，不会自己往前走。直接拿它做
     * 快进基准，连点两次会算出同一个目标位置 → 用户看到的就是"点了没反应"。
     * 外推基准用 `lastPositionUpdateTime`（同样以 elapsedRealtime 计），与本端时钟同源。
     */
    private fun currentPosition(state: PlaybackState?): Long {
        val pos = state?.position ?: return 0L
        val updated = state.lastPositionUpdateTime
        if (state.state != PlaybackState.STATE_PLAYING || updated <= 0L) return pos
        val elapsed = SystemClock.elapsedRealtime() - updated
        if (elapsed <= 0L) return pos
        val speed = if (state.playbackSpeed > 0f) state.playbackSpeed else 1.0f
        return pos + (elapsed * speed).toLong()
    }

    /** 挑"当前该被控制/展示"的会话：优先正在播放的，没有则取状态非 NONE 的第一个（用户暂停后会话还在）；返回 null 时 [lastSkip] 已写好原因。 */
    private fun pickController(ctx: Context): Pair<MediaController, PlaybackState?>? {
        val listener = NlsService.authorizedComponent(ctx) ?: run {
            // 媒体控制最常见的"用不了"原因：getActiveSessions 要求已启用的 NotificationListenerService，
            // 没给通知权限 = 读不到正在播放，跟蓝牙无关。必须说人话。
            lastSkip = "未授予通知监听权限（媒体控制依赖它读取正在播放）"
            return null
        }
        val msm = ctx.getSystemService(Context.MEDIA_SESSION_SERVICE) as? MediaSessionManager
            ?: run { lastSkip = "系统不支持 MediaSessionManager"; return null }
        val sessions = runCatching { msm.getActiveSessions(listener) }
            .onFailure { lastSkip = "读取媒体会话失败：${it.message}" }
            .getOrDefault(emptyList())
        if (sessions.isEmpty()) {
            lastSkip = "没有活动的媒体会话"
            return null
        }
        val picked = sessions.firstOrNull { it.playbackState?.state == PlaybackState.STATE_PLAYING }
            ?: sessions.firstOrNull { it.playbackState?.state != PlaybackState.STATE_NONE }
            ?: sessions.firstOrNull()
            ?: run { lastSkip = "没有活动的媒体会话"; return null }
        return picked to picked.playbackState
    }

    private fun mediaVolumePercent(ctx: Context): Int {
        val am = ctx.getSystemService(Context.AUDIO_SERVICE) as? AudioManager ?: return -1
        val max = am.getStreamMaxVolume(AudioManager.STREAM_MUSIC)
        if (max <= 0) return -1
        // -1 专门表示"读不到"，与"真的是 0%"分开。音乐流未激活时的假 0 只在没播放时出现，
        // 电脑端用 `volume > 0 || playing` 判掉即可，不必在手机上添一个猜得更差的判据。
        return (am.getStreamVolume(AudioManager.STREAM_MUSIC) * 100 + max / 2) / max
    }

    /** 按百分比设置媒体音量，返回**是否真的变了**：安卓音量分级（常见 15 档），小步长可能落在同一档，如实返回、不假装成功。 */
    private fun setMediaVolume(ctx: Context, percent: Int): Boolean {
        val am = ctx.getSystemService(Context.AUDIO_SERVICE) as? AudioManager ?: return false
        val max = am.getStreamMaxVolume(AudioManager.STREAM_MUSIC)
        if (max <= 0) return false
        val target = (percent * max + 50) / 100
        if (target == am.getStreamVolume(AudioManager.STREAM_MUSIC)) return false
        return runCatching {
            am.setStreamVolume(AudioManager.STREAM_MUSIC, target, 0)
            true
        }.onFailure { Log.w(TAG, "设置音量失败：${it.message}") }.getOrDefault(false)
    }

    // 与 Proto/linkx/v1/media.proto 的 MediaCommand.Action 对齐
    const val ACTION_PLAY_PAUSE = 0
    const val ACTION_PLAY = 1
    const val ACTION_PAUSE = 2
    const val ACTION_NEXT = 3
    const val ACTION_PREV = 4
    const val ACTION_STOP = 5
    const val ACTION_SEEK_FWD = 6
    const val ACTION_SEEK_BACK = 7
    const val ACTION_SET_VOLUME = 8
}
