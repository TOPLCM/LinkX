package com.linkx.app

import android.content.Context
import android.media.AudioManager
import android.media.MediaMetadata
import android.media.session.MediaController
import android.media.session.MediaSessionManager
import android.media.session.PlaybackState
import android.os.SystemClock
import android.util.Log
import java.util.concurrent.Executors
import java.util.concurrent.TimeUnit
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

    /**
     * 指令之后追采的节拍（毫秒）。安卓的 `MediaSession` 改播放状态是**异步**的：立刻采一次
     * 经常还读到改之前那个值，于是电脑要等下一轮常规采样（最长 [POLL_MS]）才知道"其实已经
     * 暂停了"，表现就是"按下去半秒又弹回播放"。采到真的变了就停；一次都没变就交回常规轮询，
     * 播放器可能就是不执行，不能替它假报。
     */
    private val SETTLE_DELAYS_MS = longArrayOf(150, 400, 900)


    private val busy = AtomicBoolean(false)
    private val pendingCmds = AtomicInteger(0)
    private val pool by lazy {
        Executors.newSingleThreadScheduledExecutor { r ->
            Thread(r, "linkx-media").apply { isDaemon = true }
        }
    }

    private var lastAt = 0L
    private var lastPushAt = 0L
    private var lastSignature = ""

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
                val before = snapshot(app)?.sig ?: ""
                lastCommand = handleCommand(app, action, volume, deltaMs)
                // 执行完立刻补采（音量这类生效即有新值的），再按节拍追采：播放态与切歌是异步落地的，
                // 一次采不到就会把"已经按了"拖到下一轮常规采样才让电脑知道
                runCatching { sampleAndPush(app, SystemClock.elapsedRealtime()) }
                settleAfter(app, before, 0)
            } catch (e: Exception) {
                lastCommand = "执行异常：${e.message}"
                Log.w(TAG, "指令执行失败：${e.message}")
            } finally {
                pendingCmds.decrementAndGet()
            }
        }
        return "已排队：动作 $action"
    }

    /** 指令后追采：状态真的变了就收工；一次都没变就交回常规轮询（播放器可能就是不执行，不替它假报）。 */
    private fun settleAfter(app: Context, before: String, attempt: Int) {
        if (attempt >= SETTLE_DELAYS_MS.size) return
        pool.schedule({
            val now = SystemClock.elapsedRealtime()
            val after = runCatching { sampleAndPush(app, now) }.getOrDefault(before)
            // 追采也算一次采样，别让常规 tick 紧接着再采一遍
            lastAt = now
            if (after == before) settleAfter(app, before, attempt + 1)
        }, SETTLE_DELAYS_MS[attempt], TimeUnit.MILLISECONDS)
    }

    /** 一次采样的材料：签名（判"变没变"）+ 要推出去的字段 + 原始元数据（封面从这里取）。 */
    private data class Snap(val sig: String, val p: Playback, val speedX100: Int)

    /** 读一遍本机媒体会话并算出签名。**不推送、不动任何"上次"记账**，所以指令前后各调一次是安全的。 */
    private fun snapshot(ctx: Context): Snap? {
        val (controller, state) = pickController(ctx) ?: return null
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
        // 签名**必须含音量**：不含时电脑改了音量手机却判"未变化"不推。不含进度：进度每时每刻
        // 都在变，含进去等于每轮都"变了"，窄口会被心跳自己挤满。
        val sig = "$pkg|$title|$artist|$album|$playing|$duration|${(speed * 100).toInt()}|$volume"
        return Snap(
            sig,
            Playback(pkg, title, artist, album, playing, position, duration, volume),
            (speed * 100).toInt(),
        )
    }

    /** 采一次并（内容变了或到了心跳）推给电脑，返回这一轮的签名；没有可采的会话时返回空串。 */
    private fun sampleAndPush(ctx: Context, nowMs: Long): String {
        val s = snapshot(ctx) ?: run { current = null; return "" }
        val p = s.p
        current = p
        // 只在"内容变了"或"到了进度心跳"时推：静止画面每 3 秒一条帧会把窄口占满。
        val changed = s.sig != lastSignature
        // 心跳**不看 playing**：暂停态下一台刚重启的电脑会一直空白——手机不知道对面已是全新会话。
        val heartbeat = nowMs - lastPushAt >= POSITION_PUSH_MS
        if (!changed && !heartbeat) {
            lastSkip = "状态未变化（节流）"
            return s.sig
        }
        // 签名**只能在真的推出去之后**才更新：提前更新会把"未配对时推送失败"记成已发，配对后电脑侧永远空白。
        val sent = LinkxRuntime.sendMediaState(
            pkg = p.pkg,
            title = p.title,
            artist = p.artist,
            album = p.album,
            playing = p.playing,
            positionMs = p.positionMs,
            durationMs = p.durationMs,
            speedX100 = s.speedX100,
            volume = p.volume,
            // 协议时间戳用挂钟（对端判新旧、日志可读）；节流用 elapsedRealtime——用途不同，不能共用。
            tsMs = System.currentTimeMillis(),
        )
        if (sent) {
            lastSignature = s.sig
            lastPushAt = nowMs
            lastSent = "${p.pkg}|${p.title}"
            lastSkip = ""
        } else {
            // false 有两种成因：未配对，或 JNI 调用没成功（.so 缺符号）。措辞必须覆盖两者，否则排查被带偏。
            lastSkip = "推送被拒：未配对，或 native 调用失败（见 logcat nativeSendMediaState）"
        }
        return s.sig
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
