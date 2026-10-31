/**
 * 全局运行时（单例）：持有 Core 会话句柄、驱动收发、维护 UI 可见状态。
 *
 * 本文件由原先那个近 3000 行的 `LinkxRuntime.kt` 拆出：原文件里塞了四个顶层 object
 * （运行时 / TCP 通道 / UDP 发现 / 文件传输），改一处要在几千行里找。现在各占一个文件：
 *
 * - `LinkxRuntime.kt`（本文件）：运行时；
 * - `RuntimeTypes.kt`：四个 object **共用**的数据模型；
 * - `TcpChannel.kt`：TCP 通道（Android 侧是客户端）；
 * - `LanDiscovery.kt`：UDP 发现；
 * - `FileTransfer.kt`：文件传输（发送 / 接收 / 工具）。
 *
 * 拆的是**位置**，不是行为：函数体、调用关系与可见性口径一律照原样。
 */

package com.linkx.app

import android.app.ActivityManager
import android.content.Context
import android.content.SharedPreferences
import android.net.Uri
import android.os.Build
import android.os.Handler
import android.os.Looper
import android.os.SystemClock
import android.util.Log
import java.io.File
import java.net.URLDecoder
import java.util.ArrayDeque
import java.util.ArrayList
import java.util.concurrent.Executors
import java.util.concurrent.ScheduledExecutorService
import java.util.concurrent.TimeUnit

/** TCP 传输端口：**必须与 Rust `Crates/lan/src/transport.rs::TRANSPORT_TCP_PORT`（55676）一致**（UDP 发现端口同号，见 `discovery.rs`）；Android 为 TCP 客户端，Windows 为服务端。 */
internal const val LINKX_TCP_PORT = 55676

/// 调试控制面端口：只绑 127.0.0.1，经 `adb forward` 透出；与 Windows 侧 `linkx_debugd::DEFAULT_PORT` 同值。
private const val DEBUGD_PORT = 55699

/** 全局运行时（单例）：持有 Core 会话句柄、驱动收发、维护 UI 可见状态。线程安全：引擎非线程安全，所有 native 调用都在本 object 的 monitor（`@Synchronized`）下串行，锁内统一走 `feed/tick/send → drain → sink + pollEvents → 分发`。BLE 配对后启用 UDP 发现 + TCP 通道；小消息未绑定 TCP 时回退 BLE，文件传输不回退（大声失败）。 */
object LinkxRuntime {
    private const val TAG = "LinkX.Runtime"
    private const val PREFS = "linkx"
    private const val KEY_PEER_FP = "peer_fingerprint"
    /** 信任库（`指纹\t名称` 逐行 TSV；格式规则在 Core 的 `linkx_session::trust`，跨端同源） */
    private const val KEY_TRUSTED_TSV = "trusted_peers_tsv"
    /** Debug 模式开关（默认关） */
    private const val KEY_DEBUG_ENABLED = "debug_enabled"
    /** 报给对端的版本号：取 build.gradle 的 versionName（手抄字面量必然漂） */
    private val VERSION: String = BuildConfig.VERSION_NAME
    private const val ROLE_RESPONDER = 0
    private const val PENDING_MAX = 512
    private const val HISTORY_MAX = 20

    // 会话状态码（镜像 Core state_code）
    const val STATE_DISCOVER = 0
    const val STATE_HANDSHAKE = 1
    const val STATE_PAIRING = 2
    const val STATE_SAS_COMPARE = 3
    const val STATE_PAIRED = 4
    const val STATE_REPAIRED = 5

    /** 相册开关关掉时回给电脑的那句话（必须出声，不能让电脑转到超时）。 */
    private const val ALBUM_CLOSED = "相册同步已在手机端关闭"

    // 配置归属：只有 cross / per_peer 跨端同步
    private const val SCOPE_CROSS = "cross"
    private const val SCOPE_PER_PEER = "per_peer"

    private var prefs: SharedPreferences? = null
    private var appContext: Context? = null
    private var mainHandler: Handler? = null

    private var handle: Long = 0L
    /** 已下发给当前引擎的 MTU；随会话重建归零，避免新引擎沿用旧值判定「无需更新」。 */
    private var appliedMtu = 0
    /** 上一次成功上报的电量签名 `level*2 + charging`；-1 = 本次会话还没报过 */
    private var lastBatterySig = -1
    /** 本次会话是否已经把"上报成功/被拒"说过一次（只说一次，避免每秒刷日志） */
    private var batteryOutcomeLogged = false
    private var scheduler: ScheduledExecutorService? = null
    private var sink: PacketSink? = null

    /** TCP 出站帧 sink（socket 连上后注册；见 [TcpChannel] / [onTcpConnected]）。 */
    @Volatile
    private var tcpSink: ((ByteArray) -> Unit)? = null

    /** 入站分块 sink（init 时指向 [FileTransfer.onChunk]，在 pump 锁内投递）。 */
    @Volatile
    private var chunkSink: ((IncomingChunk) -> Unit)? = null

    /** 无 sink（未连接）时暂存待发包，连接建立后补发，避免丢掉 HELLO 导致握手起不来。 */
    private val pending = ArrayDeque<ByteArray>()

    // UI 可见状态（事件维护；Compose 通过 listener/onChanged 触发重组）
    @Volatile var state: Int = STATE_DISCOVER
        private set
    @Volatile var sas: Int? = null
        private set
    @Volatile var peerName: String? = null
        private set
    @Volatile var peerFp: String? = null
        private set
    @Volatile var fingerprintMismatch: Boolean = false
        private set
    /** 对端同名设备呈递了新身份、待用户决策：非空时 UI 必须弹「重新配对确认」，接受走 [acceptFingerprint]，拒绝走 [rejectSas]。 */
    @Volatile var identityChange: IdentityChangeInfo? = null
        private set
    /** 本机 RSA 身份指纹（16 位小写 hex；设置页展示，与 Windows/日志同源） */
    @Volatile var localFingerprint: String? = null
        private set
    /** Debug 模式是否开启 */
    @Volatile var debugEnabled: Boolean = false
        private set
    /** 调试控制面是否可用（= 当前是 `agent-debug` 构建）。`BlePeripheralService` 等调用前先查：交付版 .so 里 `nativeDebugCounter` 等符号不存在，裸调会抛 UnsatisfiedLinkError。 */
    @Volatile var debugdAvailable: Boolean = false
        private set
    @Volatile var lastErrorCode: Int? = null
        private set
    @Volatile var droppedPackets: Long = 0L
        private set
    /** GATT 协商到的 ATT MTU（0 = 尚未知晓）。只由 `onMtuChanged` 在 **Binder 线程**写入、[pump] 读并下发引擎：Binder 回调不受本对象 `@Synchronized` 保护，在那里直接调 JNI 就是绕过引擎互斥，属于数据竞争。 */
    @Volatile var negotiatedMtu: Int = 0

    // 局域网/TCP 状态（UI 可见）
    @Volatile var tcpBound: Boolean = false
        private set
    @Volatile var discoveredIp: String? = null
        private set
    @Volatile var manualPeerIp: String? = null
        private set
    /** TCP 连接尝试次数与最后一次失败原因（0 = 从未尝试）。 */
    @Volatile var lanAttempts: Int = 0
        private set
    @Volatile var lastLanError: String? = null
        private set

    /** 局域网通道的可读诊断，无异常时返回 null。连接失败必须能归因：用户无从判断是电脑防火墙（常见：55676 只在「专用网络」放行）还是 App 的问题。只做归因与指引，不擅自改防火墙设置。 */
    fun lanDiagnosis(): String? {
        // 读 @Volatile 快照而不是 isTcpBound()：本函数也会被 UI 主线程调用，为一次观测抢引擎锁不值得。
        if (tcpBound) return null // 已经通了就别再报旧账
        val err = lastLanError ?: return null
        if (lanAttempts < 2) return null // 首次失败可能只是对端还没起来
        val timedOut = err.contains("ETIMEDOUT") || err.contains("after 5000ms") ||
            err.contains("failed to connect")
        return if (timedOut) {
            "已发现电脑但连不上其 55676 端口（多次超时）。" +
                "最常见原因是电脑端 Windows 防火墙只在「专用网络」放行 LinkX，" +
                "而当前网络被识别为「公用网络」。请把该网络设为专用，或在防火墙里为 LinkX 放行公用网络。"
        } else {
            "TCP 通道连接失败：$err"
        }
    }

    /** 事件监听（Compose 侧收集）。在主线程回调。 */
    @Volatile var listener: ((LinkxEvent) -> Unit)? = null

    /** 通用"数据已变化"回调（连接状态、历史记录等）。在主线程回调。 */
    @Volatile var onChanged: (() -> Unit)? = null

    /** 剪贴板应用逻辑（收到对端 Clipboard 事件时调用）。在主线程回调。 */
    @Volatile var clipboardApplier: ((String) -> Unit)? = null

    private val histLock = Any()
    private val forwarded = ArrayList<ForwardedNotification>()
    private val clipSent = ArrayDeque<String>()
    private val clipRecv = ArrayDeque<String>()

    /** 局域网会话是否已启动（UDP 发现或手动 IP 建链）。 */
    private var lanStarted = false

    fun init(context: Context) {
        appContext = context.applicationContext
        prefs = context.applicationContext.getSharedPreferences(PREFS, Context.MODE_PRIVATE)
        mainHandler = Handler(Looper.getMainLooper())
        // 入站文件分块统一交给 FileTransfer 落盘（锁内只投递，不做磁盘 IO）
        chunkSink = { c -> FileTransfer.onChunk(c) }
        BatteryMonitor.start(context)
    }

    /** 进程级初始化。入口有两个（用户打开 Activity、系统为绑监听/开机广播直接把服务拉起），必须共用同一份顺序：功能开关**早于**任何模块，否则会出现"设置说关了、模块照样起来"。 */
    fun boot(context: Context) {
        Features.init(context)
        this.init(context)
        ClipboardSync.init(context)
        AppPrefs.init(context)
    }

    /** 应用上下文（Keystore 封装需要它；`start()` 已确保非空） */
    private fun context(): Context = checkNotNull(appContext) { "未 init(context)" }

    /** 供同文件模块（FileTransfer / TcpChannel）取应用上下文 */
    internal fun requireContext(): Context = context()

    /** 偏好存储；`init(context)` 之前是 null（调用方要能容忍"还没起来"）。 */
    internal fun prefsStore(): SharedPreferences? = prefs

    fun isPaired(): Boolean = state == STATE_PAIRED || state == STATE_REPAIRED

    // ---------- 生命周期 ----------

    /** 创建引擎（Responder）并发送本端 HELLO；重复调用幂等。 */
    @Synchronized
    fun start() {
        if (handle != 0L) return
        val p = prefs
        if (p == null) {
            Log.w(TAG, "未 init(context)，忽略 start()")
            return
        }
        // 身份私钥经 Android Keystore（AES-GCM）加密存储；旧版明文首次读取时原地迁移，私钥字节不变 → 指纹不漂移。
        val sk = IdentityStore.getOrCreateSkHex(context(), p)
        // 设备身份 RSA-2048（首次运行/旧版迁移时由 Core 生成并加密落盘）。拿不到就**不启动**引擎——宁可不连，也不能用临时身份让对端看到「新设备」。
        val identityDer = IdentityStore.getOrCreateDeviceIdentity(context(), p)
        if (identityDer == null) {
            Log.e(TAG, "设备身份不可用，拒绝启动会话引擎")
            lastErrorCode = ERR_IDENTITY_UNAVAILABLE
            notifyChanged()
            return
        }
        localFingerprint = IdentityStore.fingerprintOf(identityDer)
        val trustedTsv = p.getString(KEY_TRUSTED_TSV, null).orEmpty()
        var nativeMissing = false
        val h = runCatching {
            NativeCore.nativeSessionNew(
                ROLE_RESPONDER,
                deviceName(),
                VERSION,
                sk,
                identityDer,
                trustedTsv,
            )
        }.onFailure {
            // 铁律：runCatching 会把 UnsatisfiedLinkError 吞成 0——报成"身份不可用"是假故障，真因是 .so 里没有这个符号（Kotlin 与 Rust 版本不匹配），修复方向完全不同。
            nativeMissing = it is UnsatisfiedLinkError
            Log.w(TAG, "nativeSessionNew 抛异常：${it::class.simpleName} ${it.message}")
        }.getOrDefault(0L)
        if (h == 0L) {
            Log.w(TAG, if (nativeMissing) "Core 符号缺失（.so 与本 APK 不匹配）" else "引擎创建失败")
            lastErrorCode = if (nativeMissing) ERR_CORE_UNAVAILABLE else ERR_IDENTITY_UNAVAILABLE
            notifyChanged()
            return
        }
        handle = h
        appliedMtu = 0 // 新引擎：MTU 必须重新下发一次
        forgetBatteryReport() // 新会话：电脑那边没有上一次的电量了，必须重报
        runCatching { NativeCore.nativeStart(h) }
        // 调试控制面只在 debug 构建里尝试启动：`BuildConfig.DEBUG` 在 release 是编译期 false，R8 会把这段连同对 `nativeDebugdStart` 的引用一起摘掉——交付 APK 的 dex 里不该留着调试入口的声明与端口常量。
        debugdAvailable = if (BuildConfig.DEBUG) {
            runCatching {
                NativeCore.nativeDebugdStart(
                    context().filesDir.resolve("Logs").absolutePath,
                    DEBUGD_PORT,
                )
                true
            }.getOrDefault(false)
        } else {
            false
        }
        // Debug 模式随设置恢复（日志目录 = app 私有 files/Logs）
        debugEnabled = p.getBoolean(KEY_DEBUG_ENABLED, false)
        // 调试变体一启动就进 Debug 模式（否则控制面 /logs 永远是空的）；只在控制面可用时强制，交付版行为完全不变。
        if (debugdAvailable && !debugEnabled) {
            debugEnabled = true
            p.edit().putBoolean(KEY_DEBUG_ENABLED, true).apply()
        }
        if (debugEnabled) applyDebug(true)
        startScheduler()
        pump()
        Log.i(
            TAG,
            "运行时已启动：responder=${deviceName()} fp=${localFingerprint ?: "-"} debugd=$debugdAvailable",
        )
    }

    /** 释放句柄并停止定时器。 */
    @Synchronized
    fun stop() {
        stopLanInternal()
        scheduler?.shutdownNow()
        scheduler = null
        val h = handle
        handle = 0L
        if (h != 0L) runCatching { NativeCore.nativeSessionFree(h) }
        pending.clear()
    }

    /** 连接建立时注册 sink（并补发暂存包）；断开时传 null 清除。 */
    @Synchronized
    fun setPacketSink(s: PacketSink?) {
        sink = s
        if (s != null) {
            flushPending(s)
            pump()
        }
    }

    // ---------- 收发驱动（供 GATT/定时器调用） ----------

    /** 喂入一个 BLE 分片包，并把产生的出站包经 sink 发出。 */
    @Synchronized
    fun feed(data: ByteArray) {
        val h = handle
        if (h == 0L) return
        runCatching { NativeCore.nativeFeed(h, data) }
        pump()
    }

    /** 通知里的验证码提取（与电脑端"只复制验证码"同一规则、同一实现）。不碰引擎句柄因此无须 `@Synchronized`；但仍包 runCatching——native 抛异常会让整张通知卡片组合失败，"通知页打不开"比少一个按钮严重得多。 */
    fun noticeCode(title: String, text: String): String? =
        runCatching { NativeCore.nativeExtractCode(title, text) }.getOrNull()

    /** 等出站 TCP 队列腾出空位（最多 30 s）；false = 排空无望。**绝不抱着锁 sleep**：排空队列的 pump 在 linkx-tick 线程上等同一把锁，锁内等待 = 自锁；所以锁内只读一次状态（`tcpQueueState`），等待放锁外。 */
    fun waitForTcpWindow(): Boolean {
        val deadline = SystemClock.elapsedRealtime() + 30_000L
        while (SystemClock.elapsedRealtime() < deadline) {
            val (depth, window) = tcpQueueState()
            if (window <= 0) return false
            if (depth < window) return true
            Thread.sleep(10)
        }
        return false
    }

    /** 一次锁内读取队列深度与窗口（native 调用统一走 @Synchronized，见 JNI 铁律）。 */
    @Synchronized
    private fun tcpQueueState(): Pair<Int, Int> {
        val h = handle
        if (h == 0L) return -1 to 0
        val window = runCatching { NativeCore.nativeTcpOutWindow(h) }.getOrDefault(0)
        val depth = runCatching { NativeCore.nativeTcpOutDepth(h) }.getOrDefault(window)
        return depth to window
    }

    /** 心跳/超时推进（~1s 一次，由 linkx-tick 线程调度）。`@Synchronized` 是必须的：`nativeTick` + `pump` 直接操作引擎，而引擎非线程安全，互斥只能靠本对象的锁（`with_engine` 只锁句柄表，不锁引擎）。 */
    @Synchronized
    fun tick() {
        val h = handle
        if (h == 0L) return
        runCatching { NativeCore.nativeTick(h) }
        pump()
        publishDebugFields() // 1 Hz：观测面刷新只跟 tick，不跟帧
        // 回执超时只动 UI 侧任务表，不碰引擎；锁内调用是安全的（同一 monitor 可重入）
        if (Features.enabled(Module.FileTransfer)) FileTransfer.tickAcks()
    }

    // ---------- 业务发送 ----------

    /**
     * 小消息发送的共同形状：句柄未就绪或 native 抛异常都算「没入队」，引擎返回 1 才就地泵一次。
     * 兜底不得省：全 profile `panic = "abort"`、JNI 侧 `catch_unwind` 实际拦不住，装了旧核心时
     * 符号缺失抛的 `UnsatisfiedLinkError` 只能在这里挡。
     */
    private fun sendQueued(block: (Long) -> Int): Boolean {
        val h = handle
        if (h == 0L) return false
        if (runCatching { block(h) }.getOrDefault(0) != 1) return false
        pump()
        return true
    }

    /** [sendQueued] 的「非 0 即受理」版：文件帧与 TCP 绑定这几条一直按这个判据泵。 */
    private fun sendAccepted(block: (Long) -> Int): Boolean {
        val h = handle
        if (h == 0L) return false
        if (runCatching { block(h) }.getOrDefault(0) == 0) return false
        pump()
        return true
    }

    /** 推送当前播放状态（手机 → 电脑），true = 已入队。由 `MediaControl` 采样线程调用，和其余发送口一样走 `@Synchronized`：引擎互斥只认本对象的锁（见 [tick]）。 */
    @Synchronized
    fun sendMediaState(
        pkg: String,
        title: String,
        artist: String,
        album: String,
        playing: Boolean,
        positionMs: Long,
        durationMs: Long,
        speedX100: Int,
        volume: Int,
        tsMs: Long,
    ): Boolean {
        val h = handle
        if (h == 0L) return false
        // 异常必须落日志：`.getOrDefault(0)` 会把 UnsatisfiedLinkError 变成"发送失败"，".so 没重编"这种构建问题看起来就像"没配对"。
        val r = runCatching {
            NativeCore.nativeSendMediaState(
                h, pkg, title, artist, album, playing,
                positionMs, durationMs, speedX100, volume, tsMs,
            )
        }.onFailure { Log.w(TAG, "nativeSendMediaState 调用失败：${it.javaClass.simpleName} ${it.message}") }
            .getOrDefault(0)
        if (r == 1) pump()
        return r == 1
    }

    /**
     * 推送一条通知；`keyHash` 为通知稳定 key 哈希（0 = 无）。
     * 回复定位三元组（`tag` / `notificationId` / `canReply` + 那两个回复字段）随同一条消息上行：
     * 电脑凭它回一条通知时，手机要在"仍在通知栏"的条目里现找。`canReply=false` 时后三项不填。
     * true = 已入队。未配对时返回 false（调用方不重试）。
     */
    @Synchronized
    fun sendNotification(
        pkg: String,
        title: String,
        text: String,
        tsMs: Long,
        keyHash: Int = 0,
        tag: String = "",
        notificationId: Int = 0,
        canReply: Boolean = false,
        replyActionIndex: Int = -1,
        replyResultKey: String = "",
    ): Boolean {
        val h = handle
        if (h == 0L) return false
        val r = runCatching {
            NativeCore.nativeSendNotificationKeyed(
                h, pkg, title, text, tsMs, keyHash,
                tag, notificationId, canReply,
                if (canReply) replyActionIndex else -1,
                if (canReply) replyResultKey else "",
            )
        }.getOrDefault(0)
        if (r == 1) {
            recordForwarded(ForwardedNotification(pkg, title, text, tsMs, keyHash))
            pump()
        }
        return r == 1
    }

    /** 回复回执（手机 → 电脑）。true = 已入队；未配对时为 false，此时电脑侧靠自己的超时出声。 */
    @Synchronized
    fun sendNotifyReplyAck(replyId: Int, pkg: String, ok: Boolean, error: String): Boolean = sendQueued {
        NativeCore.nativeSendNotifyReplyAck(it, replyId, pkg, ok, error)
    }

    /** 「这条通知已经不在了」（手机 → 电脑），电脑据此撤掉回复入口。true = 已入队。 */
    @Synchronized
    fun sendNotifyDismiss(pkg: String, tag: String, notificationId: Int, keyHash: Int): Boolean = sendQueued {
        NativeCore.nativeSendNotifyDismiss(it, pkg, tag, notificationId, keyHash)
    }

    /** 推送剪贴板纯文本；true = 已入队。 */
    @Synchronized
    fun sendClipboard(text: String): Boolean {
        val h = handle
        if (h == 0L) return false
        val r = runCatching { NativeCore.nativeSendClipboard(h, text) }.getOrDefault(0)
        if (r == 1) {
            recordClipSent(text)
            pump()
            notifyChanged() // 立即刷新「最近发送」，不等 1s tick
        }
        return r == 1
    }

    @Synchronized
    fun confirmSas() {
        val h = handle
        if (h == 0L) return
        runCatching { NativeCore.nativeConfirmSas(h) }
        pump()
    }

    @Synchronized
    fun rejectSas() {
        val h = handle
        if (h == 0L) return
        runCatching { NativeCore.nativeRejectSas(h) }
        pump()
    }

    @Synchronized
    fun acceptFingerprint(): Boolean {
        val h = handle
        if (h == 0L) return false
        val ok = runCatching { NativeCore.nativeAcceptFingerprint(h) }.isSuccess
        pump()
        return ok
    }

    // ---------- TCP 通道 ----------
    // 取帧只有 `pump()` 一个出口（它先看 `tcpSink` 再决定要不要出队）；曾有"供诊断"的旁路 drain 口，出队即不可回退，等于静默丢帧路径，已删。

    /** 喂入收到的 TCP 完整帧（由 [TcpChannel] 按 13B 帧头切分后调用）。 */
    @Synchronized
    fun feedTcp(bytes: ByteArray) {
        val h = handle
        if (h == 0L) return
        runCatching { NativeCore.nativeFeedTcp(h, bytes) }
        pump()
    }

    /** 发起 TCP 通道绑定；`isClient` true = TcpClient（Android 主动连接）。 */
    @Synchronized
    fun beginTcpBind(isClient: Boolean): Boolean = sendAccepted {
        NativeCore.nativeBeginTcpBind(it, if (isClient) 1 else 0)
    }

    /** TCP socket 已关闭/出错：复位 Core 绑定状态并驱动一次事件分发。 */
    @Synchronized
    fun tcpClosed(reason: String) {
        lanAttempts++
        lastLanError = reason
        // 先摘本机这一侧的状态：`tcpSink` 不置空，pump 会继续"从引擎出队 → 交给死 socket"，而出队不可回退，出站帧被结构性静默吞掉。`tcpBound` 必须在 handle 判空**之前**复位，否则引擎句柄已清、界面仍报"已绑定"。
        tcpSink = null
        tcpBound = false
        val h = handle
        if (h == 0L) return
        runCatching { NativeCore.nativeTcpClosed(h, reason) }
        pump()
    }

    /** TCP 通道是否已完成绑定。 */
    @Synchronized
    fun isTcpBound(): Boolean {
        val h = handle
        if (h == 0L) return false
        return runCatching { NativeCore.nativeIsTcpBound(h) }.getOrDefault(0) != 0
    }

    /** TCP socket 建立成功（[TcpChannel] 回调）：注册 sink 并发起绑定。 */
    internal fun onTcpConnected() {
        setTcpSink { frame -> TcpChannel.send(frame) }
        beginTcpBind(true)
    }

    /** 注册/清除 TCP 出站 sink（内部使用）。 */
    @Synchronized
    fun setTcpSink(s: ((ByteArray) -> Unit)?) {
        tcpSink = s
        if (s != null) pump()
    }

    /** 手动指定电脑 IP（跳过 UDP 发现）；传 null 恢复自动发现。 */
    @Synchronized
    fun setManualPeerIp(ip: String?) {
        val v = ip?.trim()?.takeIf { it.isNotEmpty() }
        // 目标没变、链路正健康时**不要**重启局域网：关掉已连上的 TCP 再重连，电脑侧会看到先后两条连接，已交换的 nonce 与 BLE proof 全部作废——对端永远绑不上（表现为本机 bound 而对端未 bound）。
        if (v == manualPeerIp && TcpChannel.isActive()) {
            Log.i(TAG, "对端地址未变化（$v），保持现有 TCP 通道")
            return
        }
        manualPeerIp = v
        if (isPaired()) {
            stopLanInternal()
            startLanInternal()
        }
        notifyChanged()
    }

    // ---------- 文件传输 ----------

    /** 传输记录（UI 展示，新条目在前）。 */
    fun transfers(): List<TransferItem> = FileTransfer.transfers()

    /** 接收目录的显示值：用户选过的就是他选的那个，否则是应用私有目录。 */
    fun receiveDir(): String = runCatching { FileTransfer.receiveDir() }.getOrDefault("")

    /** 用户选定了接收目录（SAF tree）：升级为持久读写授权并记住它。返回可读名。 */
    fun setReceiveDir(uri: Uri): String = FileTransfer.setReceiveTree(uri)

    /** 改回默认的应用私有目录（同时撤销持久授权）。 */
    fun clearReceiveDir() = FileTransfer.clearReceiveTree()

    /** 当前是否用了自选目录（界面据此决定显示哪句说明）。 */
    fun receiveDirIsCustom(): Boolean = runCatching { FileTransfer.treeUri().isNotEmpty() }.getOrDefault(false)

    /** 解析 SAF Uri 的文件名与大小（供 UI 展示）。 */
    fun fileBrief(uri: Uri): FileBrief? = FileTransfer.brief(uri)

    /** 发送文件（读 SAF Uri → 分块发送）；须在后台线程调用（含整文件摘要计算）。 */
    fun sendFile(uri: Uri, albumId: Long = 0L) = FileTransfer.send(uri, albumId)

    /** 这一行该不该出现「取消」。**UI 绘制与取消命令入口共用这一个判据**。 */
    fun fileCancellable(item: TransferItem): Boolean = FileTransfer.cancellable(item)

    // ---------- 文件传输 native 出口 ----------

    @Synchronized
    fun sendFileMeta(
        fileId: Long,
        name: String,
        size: Long,
        chunkSize: Int,
        crc32: Int,
        sha256: ByteArray,
        /** 非 0 = 这条 FILE_META 是对 `ALBUM_FULL_REQ` 的原图应答（值是照片 id）。 */
        albumId: Long = 0L,
    ): Boolean = sendAccepted {
        NativeCore.nativeSendFileMeta(it, fileId, name, size, chunkSize, crc32, sha256, albumId)
    }

    @Synchronized
    fun sendFileChunk(fileId: Long, index: Int, crc32: Int, data: ByteArray): Boolean {
        if (handle == 0L) return false
        // 故障注入（只用于验证掉帧后的补发路径）：这一号分块**不入队**，其余照常——与链路真吞掉这一帧的后果一致。由 `/action/drop-chunk` 一次性设定、用掉即清；交付包起不了调试面，这个字段因此永远是 null。
        if (debugDropChunkAt == index) {
            debugDropChunkAt = null
            Log.w(TAG, "故障注入：第 $index 块故意不入队（验证掉帧后续传补发）")
            return true
        }
        return sendAccepted { NativeCore.nativeSendFileChunk(it, fileId, index, crc32, data) }
    }

    /** 调试用的掉帧注入点：非 null 时，序号等于它的那一条 `FILE_CHUNK` 不交给引擎。 */
    @Volatile var debugDropChunkAt: Int? = null

    /** 发送 FILE_DONE；`error` 为空串表示无错误；`sha256` 空数组表示本帧不带整文件摘要。 */
    @Synchronized
    fun sendFileDone(
        fileId: Long,
        ok: Boolean,
        error: String,
        sha256: ByteArray = ByteArray(0),
    ): Boolean = sendAccepted {
        NativeCore.nativeSendFileDone(it, fileId, if (ok) 1 else 0, error, sha256)
    }

    @Synchronized
    fun sendFileResume(fileId: Long, fromIndex: Int): Boolean = sendAccepted {
        NativeCore.nativeSendFileResume(it, fileId, fromIndex)
    }

    // ---------- 相册应答出口 ----------

    /** 发送一页相册清单（由 [AlbumProvider] 的工作线程调用），true = 已入队。`@Synchronized` 不可省：引擎互斥靠本对象的 monitor，相册线程绕过它直进 JNI 属于越界；未绑定 TCP 时引擎自己留可读错误，这里只把 false 如实交回调用方。 */
    @Synchronized
    fun sendAlbumList(page: Int, total: Int, error: String, rows: String): Boolean {
        val h = handle
        if (h == 0L) return false
        val r = runCatching { NativeCore.nativeSendAlbumList(h, page, total, error, rows) }
            .onFailure { Log.w(TAG, "nativeSendAlbumList 调用失败：${it.javaClass.simpleName} ${it.message}") }
            .getOrDefault(0)
        if (r == 1) pump()
        return r == 1
    }

    /** 发送一张缩略图（JPEG 只在内存里，从不落盘）。true = 已入队。 */
    @Synchronized
    fun sendAlbumThumb(
        id: Long,
        edge: Int,
        width: Int,
        height: Int,
        jpeg: ByteArray,
        error: String,
    ): Boolean {
        val h = handle
        if (h == 0L) return false
        val r = runCatching { NativeCore.nativeSendAlbumThumb(h, id, edge, width, height, jpeg, error) }
            .onFailure { Log.w(TAG, "nativeSendAlbumThumb 调用失败：${it.javaClass.simpleName} ${it.message}") }
            .getOrDefault(0)
        if (r == 1) pump()
        return r == 1
    }

    /** 取消本端**发送**的一条在途传输（引擎停发分块并送出 `FILE_DONE{cancelled:true}`）。返回约定见 [cancelFile]。 */
    @Synchronized
    fun cancelFileSend(fileId: Long, reason: String): String? = cancelFile(fileId, reason, outgoing = true)

    /** 取消本端**接收**的一条在途传输（引擎送 `FILE_CANCEL` 让对端停手；本机半截文件由 [FileTransfer.onTaskCancelled] 删除）。返回约定见 [cancelFile]。 */
    @Synchronized
    fun cancelFileRecv(fileId: Long, reason: String): String? = cancelFile(fileId, reason, outgoing = false)

    /**
     * 取消命令的唯一出口：先过 [FileTransfer.cancellable] 这道判据（与文件页画「取消」同一个函数），再把命令交给引擎，
     * 最后就地泵一次——kind 17 事件在本函数返回前就已落到任务行上，调用方不必再去猜"过了一会儿状态会不会变"。
     * 返回 null = 已经落进「已取消」；非 null = 必须原样讲给用户的一句原因。native **抛异常**与**返回 0**是两件事：
     * 前者（多半是装了没有这个符号的旧核心）永远不会有事件回来，不当场说就是"按了取消却还在传"。
     */
    private fun cancelFile(fileId: Long, reason: String, outgoing: Boolean): String? {
        val item = FileTransfer.itemOf(fileId) ?: return "找不到这条传输，这一下没有取消任何东西"
        if (!FileTransfer.cancellable(item)) {
            return "「${item.name}」已经不在途了，这一下没有可取消的传输（分块早就交出去了，停不下来）"
        }
        val h = handle
        if (h == 0L) return "本机会话已不存在，这条传输没有可取消的引擎侧任务"
        val r = runCatching {
            if (outgoing) NativeCore.nativeCancelFileSend(h, fileId, reason)
            else NativeCore.nativeCancelFileRecv(h, fileId, reason)
        }.onFailure { Log.w(TAG, "调用取消 native 抛异常：${it.javaClass.simpleName}: ${it.message}") }
            .getOrDefault(-1)
        if (r < 0) return "取消命令没能交给引擎（本机核心版本过旧或调用失败），这条传输没有被取消"
        pump()
        // 返回 0 有两种：引擎执行了但结束帧没入队（它照样会回一条取消事件），或句柄失效、命令根本没进引擎。返回值上分不开，只能泵完后看行落定了没有——没落定就是"点了没作用"，必须当场讲出来。
        return if (FileTransfer.itemOf(fileId)?.state == TransferState.Cancelled) null
        else "取消命令交出去了，但这条传输没有落进「已取消」——多半会话已经断了；再点一次仍如此请从「设置」导出 Debug 日志"
    }

    // ---------- 配置同步 / 设备管理 ----------

    /** 发送跨端配置项（仅 scope = cross/per_peer；由 [sendLocalConfig] 组装）。 */
    @Synchronized
    fun sendConfig(entries: List<ConfigItem>): Boolean {
        if (entries.isEmpty()) return false
        return sendAccepted {
            NativeCore.nativeSendConfig(
                it,
                Array(entries.size) { i -> entries[i].key },
                Array(entries.size) { i -> entries[i].value },
                Array(entries.size) { i -> entries[i].scope },
            )
        }
    }

    /** 解绑设备：停用局域网通道 → 清 Core TOFU 信任库 → 清本地信任库 → 复位 UI 状态。下次连接须重新配对（比对 6 位配对码）。 */
    @Synchronized
    fun unbind() {
        stopLanInternal()
        val h = handle
        if (h != 0L) runCatching { NativeCore.nativeUnbind(h) }
        // 信任库是唯一持久化锚点（旧单指纹字段一并清除）。必须 commit 而非 apply：这是"把某台设备从信任锚里抹掉"，异步落盘一旦赶上进程被杀，旧 TSV 下次开机仍在 = 解绑没解成，而界面已经告诉用户解了。
        prefs?.edit()?.remove(KEY_TRUSTED_TSV)?.remove(KEY_PEER_FP)?.commit()
        peerFp = null
        peerName = null
        sas = null
        identityChange = null
        fingerprintMismatch = false
        manualPeerIp = null
        pump()
        notifyChanged()
    }

    // ---------- 信任库 / 身份裁决 / Debug ----------

    /** 把一次配对结果并入信任库并持久化（TSV 由 Core 生成 → 跨端同源） */
    private fun rememberTrusted(fingerprint: String, name: String) {
        val p = prefs ?: return
        val cur = p.getString(KEY_TRUSTED_TSV, null).orEmpty()
        val next = runCatching { NativeCore.nativeTrustUpsert(cur, fingerprint, name) }.getOrNull()
        if (next == null) {
            Log.w(TAG, "信任库更新失败（Core 不可用）")
            return
        }
        // KEY_PEER_FP 保留最近一次配对指纹：只作展示/迁移，不再参与信任判定。同上：信任库是唯一的持久化锚点，写丢一次就等于凭空多/少一台已配对设备
        p.edit().putString(KEY_TRUSTED_TSV, next).putString(KEY_PEER_FP, fingerprint).commit()
    }

    /** 信任库里的设备条目（设置页展示；`指纹 to 名称`） */
    fun trustedDevices(): List<Pair<String, String>> {
        val raw = prefs?.getString(KEY_TRUSTED_TSV, null).orEmpty()
        if (raw.isBlank()) return emptyList()
        return raw.lines().mapNotNull { line ->
            if (line.isBlank() || line.startsWith("#")) return@mapNotNull null
            val fp = line.substringBefore('\t').trim()
            if (fp.length != 16) return@mapNotNull null
            val name = line.substringAfter('\t', "").trim()
            fp to name
        }
    }

    /** 用户接受「对端新身份」→ 引擎重走配对 + SAS 复核。 */
    @Synchronized
    fun acceptIdentityChange() {
        // 先确认 Core 真的接受了再关提示：反过来会把"身份漂移需人工确认"这道窗口静默关掉而信任库没更新——用户以为批了，下次连的还是那个陌生身份。
        if (acceptFingerprint()) identityChange = null
        else notifyChanged()
    }

    /** 用户拒绝对端新身份 → 断开（不更新信任库）。 */
    @Synchronized
    fun rejectIdentityChange() {
        identityChange = null
        rejectSas()
    }

    /** 切换 Debug 模式（写入设置并立即生效；日志目录 = app 私有 files/Logs）。 */
    @Synchronized
    fun setDebugEnabled(on: Boolean) {
        debugEnabled = on
        prefs?.edit()?.putBoolean(KEY_DEBUG_ENABLED, on)?.apply()
        applyDebug(on)
        notifyChanged()
    }

    private fun applyDebug(on: Boolean) {
        val dir = File(context().filesDir, "Logs")
        val ok = runCatching { NativeCore.nativeSetDebugEnabled(on, dir.absolutePath) }.getOrDefault(0)
        if (ok != 1) Log.w(TAG, "Debug 模式切换失败（目录不可写？）：${dir.absolutePath}")
    }

    /** 把 Debug 日志导出到 app 私有缓存目录，返回导出目录（null = 失败）。日志不脱敏：最终去向必须由用户在 UI 显式选定（转存 SAF 树），绝不静默写到用户不知道的位置。 */
    fun prepareDebugExport(): File? {
        val tmp = File(context().cacheDir, "debug-export")
        val exported = runCatching { NativeCore.nativeExportDebug(tmp.absolutePath) }.getOrNull()
        if (exported.isNullOrBlank()) return null
        return File(exported)
    }

    // ---------- 历史记录（UI 展示，最多 20 条） ----------

    fun forwardedRecent(): List<ForwardedNotification> = synchronized(histLock) { forwarded.toList() }

    fun clipSentRecent(): List<String> = synchronized(histLock) { clipSent.toList() }

    fun clipRecvRecent(): List<String> = synchronized(histLock) { clipRecv.toList() }

    // ---------- 内部 ----------

    private fun deviceName(): String = (Build.MODEL ?: "").ifBlank { "Android" }

    private fun startScheduler() {
        if (scheduler != null) return
        val s = Executors.newSingleThreadScheduledExecutor { r ->
            Thread(r, "linkx-tick").apply { isDaemon = true }
        }
        s.scheduleWithFixedDelay({ runCatching { tick() } }, 1_000L, 1_000L, TimeUnit.MILLISECONDS)
        scheduler = s
    }

    /** 给"在发送线程里等对端回执/续传"这条路用：自己泵一次，别把在途帧留在 socket 里。平时的泵在 linkx-tick 上跑；可调试动作是把整条发送直接跑在 tick 线程上的（见 `dispatchDebugAction` 的 `send-file`），那一刻没人泵数据面——等中的线程等的那条 `RESUME` 正是被它自己卡住的。 */
    @Synchronized
    fun pumpWaiting() = pump()

    /** 必须在持有本对象锁时调用：drain 出站包/TCP 帧/入站分块 → sink；pollEvents → 分发。 */
    private fun pump() {
        val h = handle
        if (h == 0L) return
        // MTU 只在真正变化时下发：onMtuChanged 在 Binder 线程（不受 @Synchronized 保护）只传一个 int，真正的 nativeSetBleMtu 在本函数（锁内）发出。
        val mtu = negotiatedMtu
        if (mtu != 0 && mtu != appliedMtu) {
            runCatching { NativeCore.nativeSetBleMtu(h, mtu) }
            appliedMtu = mtu
        }
        // 电量：放在 drain 之前，本轮排上的帧就在本轮发出去，不用等下一次泵
        reportBattery(h)
        deliver(NativeCore.drainPackets(h))
        // 没有 sink 就**连取都不取**：`drainTcp` 是从引擎队列里拿走，拿走就还得不到"写不出去就退回去"的语义。留一轮等下一次泵，最多是慢，不是丢。
        if (tcpSink != null &&
            runCatching { NativeCore.nativeHasTcpOutbound(h) }.getOrDefault(0) != 0
        ) {
            deliverTcp(NativeCore.drainTcp(h))
        }
        // 事件队列里是 FILE_META / FILE_DONE，分块走另一条队列；**线上顺序**是 META→分块→DONE，所以先投事件（会话建好）再投分块。反过来会让同一泵里的首块找不到会话，只能丢。每条事件单独兜底：`pollEvents` 已经把这一批从引擎队列里拿走了，第 3 条抛异常就会带走第 4..N 条（含收尾用的 FILE_DONE），而且异常会顺着调用栈逃到 Binder/tick 线程——那是比"丢一条通知"严重得多的形态。日志只记事件类型，正文一律不进日志。
        for (ev in NativeCore.pollEvents(h)) {
            runCatching { handleEvent(ev) }
                .onFailure { Log.w(TAG, "事件分发失败：${ev::class.simpleName}", it) }
        }
        deliverChunks(NativeCore.takeChunks(h))

        // 媒体采样：tick 内部节流且丢到独立线程，不拖慢本轮泵；关掉媒体控制后连线程都不起（首次 tick 才懒建 `linkx-media`）。
        if (Features.enabled(Module.MediaControl)) MediaControl.tick(requireContext())
        // 调试动作在本线程执行（HTTP 线程只入队），并把宿主观测面回填给 `/state`。
        drainDebugRequests()
        // **不在这里刷观测面**：publishDebugFields() 要读本机剪贴板，安卓只允许持焦点的应用读——每泵一次 = 一次系统拒绝 + 跨进程调用，实测把电脑→手机收包压到 ~9 MB/s。观测面跟 tick 1 Hz 刷新就够，调试面不是数据面。
    }

    /** 让下一次泵无论如何都重报一次电量（新会话 / 断开后重连共用这一处口径）。 */
    private fun forgetBatteryReport() {
        lastBatterySig = -1
        batteryOutcomeLogged = false
    }

    /** 电量/充电态变化时上报电脑（同一签名不重复发）。未配对时引擎返回 0、签名不记账，下一轮自动重试——配对完成 ≤1 s 电脑就能看到电量，无需额外的"连上就推一次"逻辑。 */
    private fun reportBattery(h: Long) {
        val level = BatteryMonitor.level
        if (level < 0) return
        val charging = BatteryMonitor.charging
        val sig = level * 2 + (if (charging) 1 else 0)
        if (sig == lastBatterySig) return
        val outcome = runCatching {
            NativeCore.nativeSendDeviceStatus(h, level, charging, System.currentTimeMillis())
        }
        if (outcome.getOrDefault(0) == 1) lastBatterySig = sig
        // 第一次上报的成败要说出来：这一路全静默时，"电脑不显示电量"会被一路误诊到蓝牙/配对上。
        if (!batteryOutcomeLogged && isPaired()) {
            batteryOutcomeLogged = true
            outcome.onSuccess { sent ->
                if (sent == 1) {
                    Log.i(TAG, "电量已上报：$level%${if (charging) " 充电中" else ""}")
                } else {
                    // 引擎拒收的两种可能：通道满，或状态不允许（isPaired 只是 Kotlin 侧的近似）
                    BatteryMonitor.markNote("电量未入队：引擎返回 $sent")
                    Log.w(TAG, "电量未入队：native 返回 $sent")
                }
            }.onFailure {
                // 抛异常和"返回 0"是两件事：前者多半是 .so 里缺符号（装了旧核心）
                BatteryMonitor.markNote("电量上报调用失败：${it.javaClass.simpleName}")
                Log.w(TAG, "调用 nativeSendDeviceStatus 抛异常：${it.javaClass.simpleName}: ${it.message}")
            }
        }
    }

    /** 取走并执行调试动作队列，每轮最多取 4 条：动作里可能含 `sendFile` 这类会排队的重活，一次取空会把本 tick 拖长——调试路径不许饿死事件泵。 */
    private fun drainDebugRequests() {
        if (!BuildConfig.DEBUG || !debugdAvailable) return
        repeat(4) {
            val raw = runCatching { NativeCore.nativeDebugTakeRequest() }.getOrDefault("")
            if (raw.isEmpty()) return
            val (name, query) = raw.split('\t', limit = 2)
            runCatching { dispatchDebugAction(name, parseQuery(query)) }
        }
    }

    private fun parseQuery(q: String): Map<String, String> = q.split('&')
        .filter { it.contains('=') }
        .associate { it.substringBefore('=').trim() to it.substringAfter('=') }
        // 控制面是手写的 HTTP，没有解码环节：curl 的百分号编码参数必须在这里还原，否则中文内容会被当成字面 "%E4%B8%AD" 送进产品路径。
        .mapValues { runCatching { URLDecoder.decode(it.value, "UTF-8") }.getOrDefault(it.value) }

    /** 动作分发：**只调既有生产方法**，不写调试专用逻辑。本函数跑在 linkx-tick 线程，会碰 Compose 状态的（主题等）一律 post 回主线程——UI 状态只在主线程改，直接改 `mutableStateOf` 是竞态写。 */
    private fun dispatchDebugAction(name: String, p: Map<String, String>) {
        val reply: String = when (name) {
            "connect" -> "Android 是 Peripheral，由 Central 发起连接；可用动作：manual-ip / send-clip / toggle-clip / set-theme / confirm-sas / reject-sas / accept-fingerprint / unbind / send-file / cancel-file / album-list / album-thumb / album-full"
            "confirm-sas" -> { confirmSas(); "已确认 SAS 一致" }
            "reject-sas" -> { rejectSas(); "已判定 SAS 不一致" }
            "accept-fingerprint" -> { acceptFingerprint(); "已接受身份变更" }
            "unbind" -> { unbind(); "已解绑并清空信任库" }
            "send-clip" -> {
                val t = p["text"]
                // 这条调试入口绕在 `ClipboardSync.sendOnce` 之外，所以功能开关的判定必须在这里也补一次：否则"关掉剪贴板模块"在 agent-debug 构建上是个能穿过去的口子。
                if (!Features.enabled(Module.Clipboard)) "剪贴板同步模块已关闭（重启后生效）"
                else if (t.isNullOrEmpty()) "缺 ?text="
                // 只回长度：正文一律不许进日志、进 `/state`、进 HTTP 响应（同一条纪律的三个出口）
                else if (sendClipboard(t)) "已下发剪贴板（${t.length} 字）"
                else "剪贴板发送失败（未配对或引擎拒绝）"
            }
            "toggle-clip" -> {
                val on = p["on"] == "1" || p["on"] == "true"
                ClipboardSync.setEnabled(on)
                "剪贴板同步 = ${if (on) "开" else "关"}"
            }
            "set-theme" -> {
                val mode = when (p["mode"]) {
                    "light" -> ThemeMode.Light
                    "dark" -> ThemeMode.Dark
                    "system" -> ThemeMode.System
                    else -> null
                }
                if (mode == null) "缺或错 ?mode=light|dark|system"
                else {
                    mainHandler?.post { AppPrefs.setTheme(mode) }
                    "主题 = $mode"
                }
            }
            "manual-ip" -> {
                val ip = p["ip"]
                if (ip.isNullOrEmpty()) "缺 ?ip=" else { setManualPeerIp(ip); "已设置手动对端 IP：$ip" }
            }
            // 直接下一条播放指令。**同样只排队**：本分支在事件泵的锁内跑，同步执行等于把锁按在跨进程调用上。
            "media-cmd" -> {
                val a = (p["action"] ?: "").toIntOrNull()
                if (a == null) "缺 ?action=0..8"
                else MediaControl.submitCommand(
                    requireContext(),
                    a,
                    (p["volume"] ?: "0").toIntOrNull() ?: 0,
                    (p["delta_ms"] ?: "0").toLongOrNull() ?: 0L,
                )
            }
            // 0.5.1 PoC：普查在栏通知的可操作面（谁能回复、谁能关闭）。**必须另起线程**：本函数整段跑在事件泵的锁内，而 activeNotifications 是跨进程 Binder 调用，按在锁上会让 BLE/TCP 与所有 JNI 一起停摆（MediaControl 当初就是为这条退出去的）。
            "nls-probe" -> {
                Thread({
                    Log.i("LinkX.NLS", "probe: ${NotifCapabilityProbe.summarize()}")
                }, "linkx-nls-probe").start()
                "已起线程普查在栏通知，结果看 logcat（tag=LinkX.NLS）"
            }
            // 直接驱动一次回复：与电脑下发的 NOTIFY_REPLY 走**同一个** submitReply，不另造路径。用途是在这台机器上还没有"挂了 RemoteInput 的应用"时，先把定位、失败回执与电脑侧显示这三段验活（正向发送要等真有一条可回复通知时再跑）。
            "reply-probe" -> {
                val pkg = p["pkg"]
                if (pkg.isNullOrEmpty()) "缺 ?pkg=" else {
                    NlsService.submitReply(
                        replyId = (p["reply"] ?: "9001").toIntOrNull() ?: 9001,
                        pkg = pkg,
                        tag = p["tag"].orEmpty(),
                        id = (p["id"] ?: "").toIntOrNull() ?: 0,
                        actionIndex = (p["action"] ?: "-1").toIntOrNull() ?: -1,
                        resultKey = p["key"].orEmpty(),
                        text = p["text"].orEmpty().ifBlank { "探针回复" },
                    )
                    "已提交回复（回执看电脑 /state 的 reply_hint 与 logcat tag=LinkX.NLS）"
                }
            }
            "send-file" -> {
                val path = p["path"]
                if (path.isNullOrEmpty()) "缺 ?path=" else {
                    // **不能 post 到主线程**：传输链路在调用线程上直接写 socket，放主线程会撞 Android 的主线程网络禁令 → 异常 → socket 被关 → "分块没到"。调试动作必须与产品路径（Dispatchers.IO）同线程口径。
                    FileTransfer.send(Uri.fromFile(File(path)))
                    "已排队发送文件：$path"
                }
            }
            // 故障注入（只用于验证掉帧后的补发路径）：指定序号的那一条 FILE_CHUNK 不交给引擎，其余一概照常走生产路径。
            "drop-chunk" -> {
                val at = (p["at"] ?: "").toIntOrNull()
                if (at == null) "缺 ?at=<分块序号，一次性>"
                else {
                    LinkxRuntime.debugDropChunkAt = at
                    "已登记：第 $at 块 FILE_CHUNK 不会交给引擎（一次性）"
                }
            }
            // 取消一条在途传输：与文件页行上「取消」**同一个生产入口**（[cancelFileSend]/[cancelFileRecv]），不另造调试专用逻辑；id 与每行状态从 `/state.host.file_rows` 取。
            "cancel-file" -> {
                val id = (p["file_id"] ?: "").toLongOrNull()
                if (id == null) "缺 ?file_id=<数字>（见 /state.host.file_rows）&dir=send|recv"
                else {
                    val why = if (p["dir"] == "recv") cancelFileRecv(id, "调试面取消")
                    else cancelFileSend(id, "调试面取消")
                    // 返回 null = 行已经落进「已取消」；非 null = 这一下没生效的原因，原样回出去
                    why ?: "已取消：#${id}（dir=${p["dir"] ?: "send"}），落定状态见 /state.host.file_rows"
                }
            }
            // 相册三条请求：**直接调事件分支用的同一组生产方法**（连"开关关掉要回什么"都不另写一遍）。这里只投递、不等待：MediaStore 查询与解码在相册线程上跑，应答结果与原因都落到 `/state.host.album_last`、`album_err`。
            "album-list" -> {
                val page = (p["page"] ?: "0").toIntOrNull() ?: 0
                val per = (p["per"] ?: "60").toIntOrNull() ?: 60
                onAlbumListRequested(page, per)
                "已投递：第 $page 页清单（每页 $per 张），应答见 /state.host.album_last"
            }
            "album-thumb" -> {
                val id = (p["id"] ?: "").toLongOrNull()
                if (id == null) "缺 ?id=<相册照片 id，来自 ALBUM_LIST>"
                else {
                    onAlbumThumbRequested(id, (p["edge"] ?: "256").toIntOrNull() ?: 256)
                    "已投递：缩略图 id=$id，应答见 /state.host.album_last"
                }
            }
            "album-full" -> {
                val ids = (p["ids"] ?: "").split(',').mapNotNull { it.trim().toLongOrNull() }
                if (ids.isEmpty()) "缺 ?ids=1,2,3（逗号分隔的照片 id）"
                else {
                    onAlbumFullRequested(ids)
                    "已投递：${ids.size} 张原图（进度见 /state.host.file_rows），排队见 /state.host.album_queue"
                }
            }
            else -> "未知动作 $name"
        }
        Log.i(TAG, "debug action $name -> $reply")
        if (BuildConfig.DEBUG && debugdAvailable) {
            runCatching { NativeCore.nativeDebugSetField("last_action", "$name: $reply") }
        }
    }

    /** 回填 `/state.host`：剪贴板真值、开关、主题、LAN 状态——双端比对不再靠眼睛。 */
    private fun publishDebugFields() {
        if (!BuildConfig.DEBUG || !debugdAvailable) return
        // 剪贴板只有持输入焦点的进程读得到。没焦点还每秒去读 = 每秒一条系统拒绝日志 （ClipboardService: Denying clipboard access …）+ 一次跨进程调用，纯属刷屏。
        val focused = inForeground()
        val clip = if (focused) ClipboardSync.peekForDebug() else null
        setDebugField("clip", clip ?: if (focused) "<empty>" else "<no-focus>")
        setDebugField("clip_sync", if (ClipboardSync.enabled) "on" else "off")
        setDebugField("theme", AppPrefs.themeMode.name)
        setDebugField("tcp_bound", if (isTcpBound()) "yes" else "no")
        setDebugField("peer_ip", discoveredIp ?: "")
        setDebugField("lan_err", lanDiagnosis() ?: "")
        setDebugField("lan_attempts", lanAttempts.toString())
        // 媒体链路两端各记一半——"手机有没有在采"与"为什么没推"
        setDebugField("media_last", MediaControl.lastSent)
        setDebugField("media_skip", MediaControl.lastSkip)
        setDebugField("media_cmd", MediaControl.lastCommand)
        // 相册：请求到没到、权限形态、上一条应答、队列积压。权限只读缓存——checkSelfPermission 是跨进程调用，本函数在 tick 锁内，不在这里现查。
        setDebugField("album_on", if (Features.enabled(Module.Album)) "on" else "off")
        setDebugField("album_perm", AlbumProvider.cachedAccess.name)
        setDebugField("album_last", AlbumProvider.lastStatus)
        setDebugField("album_err", AlbumProvider.lastError)
        setDebugField("album_queue", AlbumProvider.queueBrief())
        setDebugField("album_reqs", AlbumProvider.requestsAccepted.toString())
        // 通知链路的两个断点要能分开看："系统没绑定监听器"与"绑上了但一条没转发"是完全不同的故障。
        setDebugField("nls_bound", if (nlsBound) "yes" else "no")
        // BLE 侧丢包必须是可观测数字，否则「分块掉进蓝牙」只能靠推断
        setDebugField("ble_drops", droppedPackets.toString())
        setDebugField("file_sending", FileTransfer.sendingCount().toString())
        // 取消动作按 file_id 下发，观测面必须先能看见 id 与每行的状态
        setDebugField("file_rows", FileTransfer.debugRows())
        val fwd = forwardedRecent()
        setDebugField("nls_fwd", fwd.size.toString())
        setDebugField(
            "nls_last",
            fwd.firstOrNull()?.let { "${it.pkg}/${it.title}" } ?: ""
        )
    }

    /** 通知监听（NLS）是否已被系统**绑定**：授权 ≠ 绑定——应用更新后 MIUI/HyperOS 常留着授权项却不再绑定服务，表现为"手机通知死活不到电脑"。由 NlsService 回调写，UI 与控制面只读。 */
    @Volatile
    var nlsBound = false
        private set

    fun markNls(bound: Boolean) {
        nlsBound = bound
    }

    /** 进程当前是不是"用户正在用的那个"。读缓存值，不跨进程，可以每秒调。 */
    private fun inForeground(): Boolean {
        val info = ActivityManager.RunningAppProcessInfo()
        ActivityManager.getMyMemoryState(info)
        return info.importance == ActivityManager.RunningAppProcessInfo.IMPORTANCE_FOREGROUND
    }

    private fun setDebugField(k: String, v: String) =
        runCatching { NativeCore.nativeDebugSetField(k, v) }

    private fun deliver(packets: List<ByteArray>) {
        if (packets.isEmpty()) return
        val s = sink
        if (s != null) {
            flushPending(s)
            for (p in packets) runCatching { s.send(p) }
        } else {
            // 未连接：暂存待发；超出上限才真正丢弃并计数
            for (p in packets) {
                if (pending.size >= PENDING_MAX) {
                    droppedPackets++
                } else {
                    pending.addLast(p)
                }
            }
        }
    }

    private fun deliverTcp(frames: List<ByteArray>) {
        if (frames.isEmpty()) return
        val s = tcpSink ?: run {
            // 走到这里说明泵没挡住，而帧已经从引擎队列取走了——**取走就回不去**。传输钉死在一条链路上（256 KB 分块 BLE 结构上带不动），丢掉就是在可靠字节流上自己造一个洞。留痕，别再静默。
            Log.w(TAG, "TCP 通道未就绪，${frames.size} 帧已出队却写不出去")
            return
        }
        for (f in frames) runCatching { s(f) }
    }

    private fun deliverChunks(chunks: List<IncomingChunk>) {
        if (chunks.isEmpty()) return
        val s = chunkSink ?: return
        for (c in chunks) runCatching { s(c) }
    }

    private fun flushPending(s: PacketSink) {
        while (pending.isNotEmpty()) {
            val p = pending.removeFirst()
            runCatching { s.send(p) }
        }
    }

    private fun handleEvent(ev: LinkxEvent) {
        when (ev) {
            is LinkxEvent.StateChanged -> {
                val wasPaired = isPaired()
                state = ev.state
                if (isPaired() && !wasPaired) {
                    sendLocalConfig()
                    // 未配对期间到过的通知已经丢了，把还在通知栏里的补发给对端
                    NlsService.resyncActive()
                }
                // 断开即忘掉"上次报过的电量"：重连后对面是新会话，电量没变也得重报，否则电脑永远空白。
                if (wasPaired && !isPaired()) forgetBatteryReport()
                syncLan()
            }
            is LinkxEvent.PeerHello -> peerName = ev.name
            is LinkxEvent.SasReady -> sas = ev.sas
            is LinkxEvent.PeerPaired -> {
                peerFp = ev.fingerprint
                fingerprintMismatch = false
                identityChange = null
                // 信任库入册（TSV 由 Core 生成，两端格式同源）→ 下次连接免弹窗
                rememberTrusted(ev.fingerprint, peerName.orEmpty())
            }
            // 对端新身份 → 必须派发到 UI 决策，不能静默
            is LinkxEvent.IdentityChanged -> {
                identityChange = IdentityChangeInfo(
                    ev.name.ifBlank { peerName.orEmpty() },
                    ev.oldFingerprint,
                    ev.newFingerprint,
                )
                Log.w(TAG, "对端身份已变化（${ev.oldFingerprint} → ${ev.newFingerprint}），等待用户确认")
            }
            is LinkxEvent.Clipboard -> {
                // 关掉剪贴板同步后连"记录一条"都不做：模块未加载 = 这条链路不存在
                if (Features.enabled(Module.Clipboard)) {
                    recordClipRecv(ev.text)
                    clipboardApplier?.let { apply -> postMain { apply(ev.text) } }
                }
            }
            is LinkxEvent.Error -> {
                lastErrorCode = ev.code
                if (ev.code == ERR_TOFU_MISMATCH) fingerprintMismatch = true
                Log.w(TAG, "Core 错误 code=${ev.code} ctx=${ev.context}")
            }
            is LinkxEvent.Notification -> Unit // Android 侧一般收不到对端通知
            is LinkxEvent.TcpBound -> {
                tcpBound = true
                Log.i(TAG, "TCP 通道绑定完成")
                sendLocalConfig() // TCP 绑定后再同步一次（此时大载荷走 TCP）
            }
            is LinkxEvent.TcpUnbound -> {
                tcpBound = false
                Log.i(TAG, "TCP 通道断开：${ev.reason}")
            }
            is LinkxEvent.FileMeta -> if (Features.enabled(Module.FileTransfer)) {
                FileTransfer.onFileMeta(ev)
            } else {
                // 关掉模块也**必须回话**：电脑侧在 EOF 时自己判"已完成"，收不到 FileDone 就会把根本没落盘的传输报成成功。
                Log.w(TAG, "文件传输已关闭，拒收 ${ev.name}")
                runCatching { sendFileDone(ev.fileId, false, "接收端已关闭文件传输") }
            }
            is LinkxEvent.FileDone -> if (Features.enabled(Module.FileTransfer)) {
                // 收尾前先把**这条传输自己的**分块投进落盘队列：DONE 比它的分块先被处理就会在半个文件上判"校验失败"。只取这一个 file_id——别的文件还没收到自己的 META。
                deliverChunks(NativeCore.takeChunksFor(handle, ev.fileId))
                FileTransfer.onFileDone(ev)
            }
            // 通道闸门硬失败：必须落到任务状态，不能只留日志
            is LinkxEvent.FileTaskFailed ->
                if (Features.enabled(Module.FileTransfer)) FileTransfer.onTaskFailed(ev.fileId, ev.reason)
            // 用户取消（本端点的 / 对端发来的）：落「已取消」而不是失败，收端删掉本机残留文件。就地处理而不是丢进 worker：发送循环在**另一条线程**上读盘发块，晚一步置标志就多发一块；取消入口返回时行也必须已落定，否则调用方只能让用户"稍后再看看"。
            is LinkxEvent.FileTaskCancelled ->
                if (Features.enabled(Module.FileTransfer)) FileTransfer.onTaskCancelled(ev.fileId, ev.reason)
            is LinkxEvent.FileResume -> if (Features.enabled(Module.FileTransfer)) FileTransfer.onFileResume(ev)
            is LinkxEvent.Config -> applyRemoteConfig(ev.entries)
            // 电脑下发的播放指令：这里**只排队不执行**——本函数跑在事件泵锁内，`getActiveSessions` 是跨进程 Binder 调用，直接执行会按住整轮泵。不弹 toast：电脑端连点时手机不该每跳响一次。
            is LinkxEvent.MediaCommand -> if (!Features.enabled(Module.MediaControl)) {
                Log.i(TAG, "媒体控制已关闭，忽略指令 ${ev.action}")
            } else {
                val queued = MediaControl.submitCommand(
                    requireContext(), ev.action, ev.volume, ev.deltaMs
                )
                Log.i(TAG, "媒体指令 ${ev.action} → $queued")
            }
            // 相册三条都**只排队**：MediaStore 查询与缩略图解码在 AlbumProvider 自己的线程上做——在这里直接跑就是按住全局锁做跨进程调用。
            is LinkxEvent.AlbumListRequested -> onAlbumListRequested(ev.page, ev.perPage)
            is LinkxEvent.AlbumThumbRequested -> onAlbumThumbRequested(ev.id, ev.edge)
            is LinkxEvent.AlbumFullRequested -> onAlbumFullRequested(ev.ids)
            // 电脑要回复一条通知：同媒体指令一样**只排队**，`activeNotifications` 是跨进程 Binder，在本函数（事件泵锁内）直接跑会按住整轮泵。成败都由 NlsService 回一条回执。
            is LinkxEvent.NotifyReplyRequested -> NlsService.submitReply(
                ev.replyId, ev.pkg, ev.tag, ev.notificationId, ev.actionIndex, ev.resultKey, ev.text
            )
        }
        notifyChanged()
        listener?.let { l -> postMain { l(ev) } }
    }

    /** `ALBUM_LIST_REQ` 的落点。关掉功能也要**出声**：电脑在等一页清单，沉默只会让它转到超时。 */
    private fun onAlbumListRequested(page: Int, perPage: Int) {
        if (!Features.enabled(Module.Album)) {
            sendAlbumList(page, 0, ALBUM_CLOSED, "")
            return
        }
        Log.i(TAG, AlbumProvider.requestList(requireContext(), page, perPage))
    }

    private fun onAlbumThumbRequested(id: Long, edge: Int) {
        if (!Features.enabled(Module.Album)) {
            sendAlbumThumb(id, edge, 0, 0, ByteArray(0), ALBUM_CLOSED)
            return
        }
        Log.i(TAG, AlbumProvider.requestThumb(requireContext(), id, edge))
    }

    /**
     * `ALBUM_FULL_REQ` 的落点：原图复用已有的文件发送通道（`FileMeta.album_id` 非 0）。
     * 被开关拒绝时没有"相册专用的错误帧"可用，所以用**已有的两帧**把原因送到电脑面前：
     * 一帧 0 字节的 FILE_META（带 album_id）+ 一帧失败的 FILE_DONE。只发一张就够说明问题，逐张发会把 256 张的批次变成 512 帧噪音。
     */
    private fun onAlbumFullRequested(ids: List<Long>) {
        if (ids.isEmpty()) return
        if (!Features.enabled(Module.Album) || !Features.enabled(Module.FileTransfer)) {
            val why = if (Features.enabled(Module.Album)) "文件传输已在手机端关闭，原图无法发送" else ALBUM_CLOSED
            AlbumProvider.rejectFullBatch(ids, why)
            return
        }
        Log.i(TAG, AlbumProvider.requestFull(requireContext(), ids))
    }

    private fun recordForwarded(n: ForwardedNotification) {
        synchronized(histLock) {
            // 带稳定 key 的通知就地合并（同包名 + 同 keyHash），避免刷屏
            if (n.keyHash != 0) {
                val idx = forwarded.indexOfFirst { it.pkg == n.pkg && it.keyHash == n.keyHash }
                if (idx >= 0) {
                    forwarded[idx] = n
                    return
                }
            }
            forwarded.add(0, n)
            while (forwarded.size > HISTORY_MAX) forwarded.removeAt(forwarded.size - 1)
        }
    }

    private fun recordClipSent(text: String) {
        synchronized(histLock) {
            clipSent.addFirst(text)
            while (clipSent.size > HISTORY_MAX) clipSent.removeLast()
        }
    }

    private fun recordClipRecv(text: String) {
        synchronized(histLock) {
            clipRecv.addFirst(text)
            while (clipRecv.size > HISTORY_MAX) clipRecv.removeLast()
        }
    }

    // ---------- 局域网生命周期 ----------

    /** 配对状态变化时启停局域网通道（UDP 发现 + TCP）。 */
    private fun syncLan() {
        if (isPaired()) startLanInternal() else stopLanInternal()
    }

    /** 已配对后启动：手动 IP 优先（跳过 UDP 发现），否则广播信标并等待发现。 */
    private fun startLanInternal() {
        if (lanStarted) return
        if (!isPaired()) return
        lanStarted = true
        val manual = manualPeerIp
        if (manual != null) {
            Log.i(TAG, "局域网直连（手动 IP）：$manual")
            TcpChannel.connect(manual, LINKX_TCP_PORT)
        } else {
            LanDiscovery.start(deviceName(), VERSION) { ip -> onPeerDiscovered(ip) }
        }
    }

    /** 断开/解绑时停用局域网通道并通知 Core socket 已关闭。 */
    private fun stopLanInternal() {
        lanStarted = false
        LanDiscovery.stop()
        TcpChannel.close()
        discoveredIp = null
        val h = handle
        if (h != 0L) runCatching { NativeCore.nativeTcpClosed(h, "lan teardown") }
        tcpBound = false
    }

    /** UDP 发现回调：记录对端 IP，若 TCP 未建立则尝试连接（供重连重试）。 */
    private fun onPeerDiscovered(ip: String) {
        discoveredIp = ip
        if (!TcpChannel.isActive()) {
            Log.i(TAG, "发现电脑：$ip")
            TcpChannel.connect(ip, LINKX_TCP_PORT)
        }
        notifyChanged()
    }

    // ---------- 配置同步 ----------

    /** 组装并发送本端跨端配置（cross/per_peer 才出本机）。 */
    private fun sendLocalConfig() {
        if (!isPaired()) return
        val clip = if (ClipboardSync.enabled) "1" else "0"
        val theme = when (AppPrefs.themeMode) {
            ThemeMode.System -> "system"
            ThemeMode.Light -> "light"
            ThemeMode.Dark -> "dark"
        }
        sendConfig(
            listOf(
                ConfigItem("clipboard.enabled", clip, SCOPE_CROSS),
                ConfigItem("theme.mode", theme, SCOPE_PER_PEER),
            ),
        )
    }

    /** 应用对端配置（仅 cross / per_peer 生效；local 不出本机）。 */
    private fun applyRemoteConfig(entries: List<ConfigItem>) {
        // 这两项会落到 Compose 的 mutableStateOf（主题）与系统剪贴板开关，而本函数跑在泵线程上——一律 post 回主线程，和"UI 状态只在主线程改"同一条口径。
        postMain { applyRemoteConfigOnMain(entries) }
    }

    private fun applyRemoteConfigOnMain(entries: List<ConfigItem>) {
        for (e in entries) {
            if (e.scope != SCOPE_CROSS && e.scope != SCOPE_PER_PEER) continue
            when (e.key) {
                "clipboard.enabled" -> ClipboardSync.setEnabled(e.value == "1" || e.value == "true")
                "theme.mode" -> AppPrefs.setTheme(
                    when (e.value) {
                        "light" -> ThemeMode.Light
                        "dark" -> ThemeMode.Dark
                        else -> ThemeMode.System
                    },
                )
                else -> {}
            }
        }
    }

    internal fun notifyChanged() {
        onChanged?.let { c -> postMain(c) }
    }

    private fun postMain(block: () -> Unit) {
        val h = mainHandler
        if (h != null) h.post(block) else block()
    }

    private const val ERR_TOFU_MISMATCH = -213

    /** 对端错误码 -212（Core 的 channel-mismatch 类）：设备身份不可用/验签失败 */
    private const val ERR_IDENTITY_UNAVAILABLE = -212

    /** 本机码（不来自 Rust）：`.so` 里根本没有那个 JNI 符号 = Kotlin 与 Core 版本不匹配 */
    private const val ERR_CORE_UNAVAILABLE = -900
}
