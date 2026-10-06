package com.linkx.app

import android.app.ActivityManager
import android.content.Context
import android.content.Intent
import android.content.SharedPreferences
import android.net.Uri
import android.os.Build
import android.os.Handler
import android.os.Looper
import android.os.SystemClock
import android.os.StatFs
import android.provider.DocumentsContract
import android.provider.OpenableColumns
import android.util.Log
import java.io.File
import java.io.IOException
import java.io.InputStream
import java.io.RandomAccessFile
import java.net.DatagramPacket
import java.net.DatagramSocket
import java.net.InetAddress
import java.net.InetSocketAddress
import java.net.NetworkInterface
import java.net.Socket
import java.net.SocketTimeoutException
import java.net.URLDecoder
import java.security.MessageDigest
import java.security.SecureRandom
import java.util.ArrayDeque
import java.util.ArrayList
import java.util.concurrent.Executors
import java.util.concurrent.LinkedBlockingQueue
import java.util.concurrent.ScheduledExecutorService
import java.util.concurrent.TimeUnit
import java.util.zip.CRC32

/** TCP 传输端口：**必须与 Rust `Crates/lan/src/transport.rs::TRANSPORT_TCP_PORT`（55676）一致**（UDP 发现端口同号，见 `discovery.rs`）；Android 为 TCP 客户端，Windows 为服务端。 */
private const val LINKX_TCP_PORT = 55676

/** 发送侧等待对端 FILE_DONE 回执的上限（入队 ≠ 送达）。 */
private const val ACK_TIMEOUT_MS = 30_000L

/** 收到"发端说发完了、可本端有洞"之后暂缓收尾等补发的窗口（与电脑端 `RESUME_HOLD_MS` 同值）。计时口径是**"多久没有补发进展"**：每落一块就重新计时（见 `writeChunk`）。走到"收尾时才发现洞"说明洞落在最后 4 MB 之内——更早的洞在分块还在流的时候就被在途续传接管了。 */
private const val RESUME_HOLD_MS = 20_000L

/** 一条传输最多允许几轮补发（与电脑端 `MAX_RESUME_TRIES` 同口径），发端与收端共用这一个数。 */
private const val MAX_RESUME_TRIES = 8
/// 调试控制面端口：只绑 127.0.0.1，经 `adb forward` 透出；与 Windows 侧 `linkx_debugd::DEFAULT_PORT` 同值。
private const val DEBUGD_PORT = 55699

/** GATT 发送器：由 BlePeripheralService 注册，把每个待发分片包经 notify 写回 Central。 */
fun interface PacketSink {
    fun send(packet: ByteArray)
}

/** 已转发通知记录（Android 侧展示"本机已转发"的通知，最多 20 条；keyHash=0 表示无稳定 key）。 */
data class ForwardedNotification(
    val pkg: String,
    val title: String,
    val text: String,
    val tsMs: Long,
    val keyHash: Int = 0,
)

/** 文件信息（SAF Uri 解析结果，供 UI 展示）。 */
data class FileBrief(val name: String, val size: Long)

/** 传输任务状态："最后一个分块交给本地引擎" ≠ 对端收到。`AwaitingPeer` 等对端 FILE_DONE 回执才落 `Done`，回执超时落 `SentUnconfirmed`（分块确实发完了，只是对端没回话）——既不骗用户说已完成，也不谎报失败。`Cancelled` 单独一档而不并入 `Failed`：取消是用户的主动动作，报成失败就是说系统坏了。 */
enum class TransferState { Running, AwaitingPeer, SentUnconfirmed, Done, Failed, Cancelled }

/** 单条文件传输记录（发送/接收共用；bytes = 已完成字节数）。 */
data class TransferItem(
    val fileId: Long,
    val name: String,
    val size: Long,
    val outgoing: Boolean,
    val bytes: Long,
    val state: TransferState,
    /** 原因文案（[TransferState.Failed] = 失败原因，[TransferState.Cancelled] = 取消原因）。空 = 引擎没给。 */
    val error: String = "",
    /** 接收文件的落盘位置：普通绝对路径，或 `content://…`（用户自选目录里的文档）。 */
    val path: String = "",
    /** 已完成但有一句话要说（例如"没能存进你选的目录，先留在应用私有目录"）。 */
    val note: String = "",
)

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

    /**
     * 进程级初始化。入口有两个（用户打开 Activity、系统为绑监听/开机广播直接把服务拉起），
     * 必须共用同一份顺序：功能开关**早于**任何模块，否则会出现"设置说关了、模块照样起来"。
     */
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
        // 调试控制面只在 debug 构建里尝试启动：`BuildConfig.DEBUG` 在 release 是编译期 false，
        // R8 会把这段连同对 `nativeDebugdStart` 的引用一起摘掉——交付 APK 的 dex 里不该留着调试入口的声明与端口常量。
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
    //
    // 取帧只有 `pump()` 一个出口（它先看 `tcpSink` 再决定要不要出队）；曾有"供诊断"的旁路 drain 口，
    // 出队即不可回退，等于静默丢帧路径，已删。

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
        // 先摘本机这一侧的状态：`tcpSink` 不置空，pump 会继续"从引擎出队 → 交给死 socket"，而出队不可回退，出站帧被结构性静默吞掉。
        // `tcpBound` 必须在 handle 判空**之前**复位，否则引擎句柄已清、界面仍报"已绑定"。
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
        // 目标没变、链路正健康时**不要**重启局域网：关掉已连上的 TCP 再重连，电脑侧会看到先后两条连接，
        // 已交换的 nonce 与 BLE proof 全部作废——对端永远绑不上（表现为本机 bound 而对端未 bound）。
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
        // 故障注入（只用于验证掉帧后的补发路径）：这一号分块**不入队**，其余照常——与链路真吞掉这一帧的后果一致。
        // 由 `/action/drop-chunk` 一次性设定、用掉即清；交付包起不了调试面，这个字段因此永远是 null。
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

    /**
     * 取消本端**发送**的一条在途传输（引擎停发分块并送出 `FILE_DONE{cancelled:true}`）。
     * 返回约定见 [cancelFile]。
     */
    @Synchronized
    fun cancelFileSend(fileId: Long, reason: String): String? = cancelFile(fileId, reason, outgoing = true)

    /**
     * 取消本端**接收**的一条在途传输（引擎送 `FILE_CANCEL` 让对端停手；本机半截文件由
     * [FileTransfer.onTaskCancelled] 删除）。返回约定见 [cancelFile]。
     */
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
        // 返回 0 有两种：引擎执行了但结束帧没入队（它照样会回一条取消事件），或句柄失效、命令根本没进引擎。
        // 返回值上分不开，只能泵完后看行落定了没有——没落定就是"点了没作用"，必须当场讲出来。
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

    /**
     * 解绑设备：停用局域网通道 → 清 Core TOFU 信任库 → 清本地信任库 → 复位 UI 状态。
     * 下次连接须重新配对（比对 6 位配对码）。
     */
    @Synchronized
    fun unbind() {
        stopLanInternal()
        val h = handle
        if (h != 0L) runCatching { NativeCore.nativeUnbind(h) }
        // 信任库是唯一持久化锚点（旧单指纹字段一并清除）。必须 commit 而非 apply：这是"把某台设备从信任锚里抹掉"，
        // 异步落盘一旦赶上进程被杀，旧 TSV 下次开机仍在 = 解绑没解成，而界面已经告诉用户解了。
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
        // KEY_PEER_FP 保留最近一次配对指纹：只作展示/迁移，不再参与信任判定。
        // 同上：信任库是唯一的持久化锚点，写丢一次就等于凭空多/少一台已配对设备
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

    /** Debug 日志目录（导出时作为源） */
    fun debugLogDir(): File = File(context().filesDir, "Logs")

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
        // 事件队列里是 FILE_META / FILE_DONE，分块走另一条队列；**线上顺序**是 META→分块→DONE，所以先投事件（会话建好）再投分块。
        // 反过来会让同一泵里的首块找不到会话，只能丢。
        // 每条事件单独兜底：`pollEvents` 已经把这一批从引擎队列里拿走了，第 3 条抛异常就会带走
        // 第 4..N 条（含收尾用的 FILE_DONE），而且异常会顺着调用栈逃到 Binder/tick 线程 ——
        // 那是比"丢一条通知"严重得多的形态。日志只记事件类型，正文一律不进日志。
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
                // 这条调试入口绕在 `ClipboardSync.sendOnce` 之外，所以功能开关的判定必须在这里
                // 也补一次：否则"关掉剪贴板模块"在 agent-debug 构建上是个能穿过去的口子。
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
            // 0.5.1 PoC：普查在栏通知的可操作面（谁能回复、谁能关闭）。**必须另起线程**：
            // 本函数整段跑在事件泵的锁内，而 activeNotifications 是跨进程 Binder 调用，
            // 按在锁上会让 BLE/TCP 与所有 JNI 一起停摆（MediaControl 当初就是为这条退出去的）。
            "nls-probe" -> {
                Thread({
                    Log.i("LinkX.NLS", "probe: ${NotifCapabilityProbe.summarize()}")
                }, "linkx-nls-probe").start()
                "已起线程普查在栏通知，结果看 logcat（tag=LinkX.NLS）"
            }
            // 直接驱动一次回复：与电脑下发的 NOTIFY_REPLY 走**同一个** submitReply，不另造路径。
            // 用途是在这台机器上还没有"挂了 RemoteInput 的应用"时，先把定位、失败回执与
            // 电脑侧显示这三段验活（正向发送要等真有一条可回复通知时再跑）。
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
                    // **不能 post 到主线程**：传输链路在调用线程上直接写 socket，放主线程会撞 Android 的主线程网络禁令 → 异常 → socket 被关 → "分块没到"。
                    // 调试动作必须与产品路径（Dispatchers.IO）同线程口径。
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
            // 相册三条请求：**直接调事件分支用的同一组生产方法**（连"开关关掉要回什么"都不另写一遍）。这里只投递、不等待：
            // MediaStore 查询与解码在相册线程上跑，应答结果与原因都落到 `/state.host.album_last`、`album_err`。
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
        // 剪贴板只有持输入焦点的进程读得到。没焦点还每秒去读 = 每秒一条系统拒绝日志
        // （ClipboardService: Denying clipboard access …）+ 一次跨进程调用，纯属刷屏。
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
                // 收尾前先把**这条传输自己的**分块投进落盘队列：DONE 比它的分块先被处理就会在半个文件上判"校验失败"。
                // 只取这一个 file_id——别的文件还没收到自己的 META。
                deliverChunks(NativeCore.takeChunksFor(handle, ev.fileId))
                FileTransfer.onFileDone(ev)
            }
            // 通道闸门硬失败：必须落到任务状态，不能只留日志
            is LinkxEvent.FileTaskFailed ->
                if (Features.enabled(Module.FileTransfer)) FileTransfer.onTaskFailed(ev.fileId, ev.reason)
            // 用户取消（本端点的 / 对端发来的）：落「已取消」而不是失败，收端删掉本机残留文件。就地处理而不是丢进 worker：
            // 发送循环在**另一条线程**上读盘发块，晚一步置标志就多发一块；取消入口返回时行也必须已落定，否则调用方只能让用户"稍后再看看"。
            is LinkxEvent.FileTaskCancelled ->
                if (Features.enabled(Module.FileTransfer)) FileTransfer.onTaskCancelled(ev.fileId, ev.reason)
            is LinkxEvent.FileResume -> if (Features.enabled(Module.FileTransfer)) FileTransfer.onFileResume(ev)
            is LinkxEvent.Config -> applyRemoteConfig(ev.entries)
            // 电脑下发的播放指令：这里**只排队不执行**——本函数跑在事件泵锁内，`getActiveSessions` 是跨进程 Binder 调用，直接执行会按住整轮泵。
            // 不弹 toast：电脑端连点时手机不该每跳响一次。
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
            // 电脑要回复一条通知：同媒体指令一样**只排队**，`activeNotifications` 是跨进程 Binder，
            // 在本函数（事件泵锁内）直接跑会按住整轮泵。成败都由 NlsService 回一条回执。
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

/** 对端新身份待决策的提示数据（UI 据此弹「重新配对确认」）。 */
data class IdentityChangeInfo(
    val name: String,
    val oldFingerprint: String,
    val newFingerprint: String,
)

// ==================== TCP 通道 ====================

/**
 * TCP 通道（Android 为客户端）：持有 [Socket] 与读线程，把 13B 帧头切分出的完整帧喂给 Core。
 * 帧格式见 `Crates/protocol/src/frame.rs`：`[magic(2) version(1) type(1) flags(1) seq(4) payload_len(4)] [body]`，
 * body 长度 = payload_len +（flags & ENCRYPTED ? 16B AEAD tag : 0）。
 * socket 异常/对端关闭时回调 [LinkxRuntime.tcpClosed]（复位绑定状态）。
 */
internal object TcpChannel {
    private const val TAG = "LinkX.Tcp"
    private const val FRAME_HEADER_LEN = 13
    private const val AEAD_TAG_LEN = 16
    private const val FLAG_ENCRYPTED = 0x10
    private const val MAGIC = 0x4C58
    private const val PROTOCOL_VERSION = 0x01
    private const val MAX_FRAME_PAYLOAD = 8 * 1024 * 1024

    @Volatile private var running = false
    @Volatile private var connecting = false
    @Volatile private var closing = false
    @Volatile private var socket: Socket? = null
    @Volatile private var target: String? = null
    /** 代际号：只有当前持有者才允许复位 `connecting/running/socket`（见 [connect]）。 */
    private val generation = java.util.concurrent.atomic.AtomicInteger(0)
    private val writeLock = Any()

    fun isActive(): Boolean = running

    /** 建立连接；同目标幂等，**换目标时抢占**在途的那一次：失败的尝试会占住 `connecting` 直到 5 s 超时，不抢占则期间的新目标（如用户手动 IP）会被静默丢弃。 */
    fun connect(host: String, port: Int) {
        val dest = "$host:$port"
        if (running || connecting) {
            if (target == null || target == dest) return
            Log.i(TAG, "切换 TCP 目标 $target -> $dest，中止在途尝试")
            close()
        }
        target = dest
        connecting = true
        closing = false
        val mine = generation.incrementAndGet()
        Thread({ connectAndRead(host, port, dest, mine) }, "linkx-tcp")
            .apply { isDaemon = true }
            .start()
    }

    private fun connectAndRead(host: String, port: Int, dest: String, mine: Int) {
        var reason = "TCP 连接已断开"
        var established = false
        // 本线程自己的 socket 引用：被抢占时共享字段 `socket` 已归新目标，只能关这一个，否则会误关新连接的句柄。
        var own: Socket? = null
        try {
            val s = Socket()
            own = s
            s.tcpNoDelay = true
            s.soTimeout = 1_000 // 读超时仅用于周期性检查 running，非致命
            // 写侧**没有**超时可用：Android 公开的 `java.net.Socket` 既没有 `setSendTimeout`、也拿不到
            // `FileDescriptor`，`SO_SNDTIMEO` 这条路走不通。后果与边界：电脑侧现在会对入站流量做背压，
            // 本机磁盘真跟不上时 `out.write` 可能长阻，而 `deliverTcp` 是在 `@Synchronized pump()` 里被调的，
            // 冻住的是心跳与 BLE 外设。兜底：电脑最多 park 15 秒就主动判死这条连接，socket 一关这里的写就抛异常
            // → 读线程上报断开 → 这次传输**大声失败**。最坏是一次 ≤15 秒的界面停顿 + 一次明确报错，不是静默丢数据。
            // 要彻底拿掉这个停顿窗口，得把 TCP 写出挪到独立写线程 + 有界队列（尚未做）。
            // 先登记 socket 再 connect：`close()` 必须能打断一个**正在进行**的 connect，否则被抢占的那次仍会占满 5 s。
            socket = s
            s.connect(InetSocketAddress(host, port), 5_000)
            running = true
            established = true
            Log.i(TAG, "TCP 已连接 $host:$port")
            LinkxRuntime.onTcpConnected() // 注册 sink + 发起 TCP 通道绑定
            readLoop(s)
        } catch (e: Exception) {
            reason = e.message ?: e.javaClass.simpleName
            // 连接失败必须落日志，否则无从判断是没试、超时还是被拒（静默失败最难排查）。
            Log.w(TAG, "TCP 连接失败 $host:$port → $reason")
        } finally {
            if (generation.get() == mine) {
                connecting = false
                running = false
                runCatching { socket?.close() }
                socket = null
                target = null
                if (!closing) {
                    if (!established) reason = "TCP 连接失败：$reason"
                    runCatching { LinkxRuntime.tcpClosed(reason) }
                }
            } else {
                // 被抢占：只清理自己，绝不复位共享状态、也不上报断开——那会把新目标的 `connecting` 抹成 false 并让 Core 误判通道刚断。
                runCatching { own?.close() }
                Log.i(TAG, "TCP 尝试已被新目标抢占，丢弃结果：$dest")
            }
        }
    }

    /** 读循环：按 13B 帧头切分完整帧（TCP 是字节流，帧可能跨包/粘包）。缓冲 = **可增长缓冲 + 消费偏移**，读直接落到尾部；若每读一次整段重新分配再拷贝（`acc + 新数据`），一个 262KB 帧要白拷约 400KB，电脑→手机速度减半。 */
    private fun readLoop(s: Socket) {
        val input = s.getInputStream()
        var buf = ByteArray(128 * 1024)
        var len = 0 // buf 中有效字节数
        var off = 0 // 已消费（已交给 Core）的位置
        while (running && !closing) {
            if (len == buf.size) {
                if (off > 0) { // 先压实已消费的前缀，绝大多数情况下就不用扩容了
                    val remain = len - off
                    System.arraycopy(buf, off, buf, 0, remain)
                    len = remain
                    off = 0
                } else {
                    buf = buf.copyOf(buf.size * 2)
                }
            }
            val n = try {
                input.read(buf, len, buf.size - len)
            } catch (e: SocketTimeoutException) {
                continue
            }
            if (n < 0) throw IOException("对端关闭连接")
            len += n
            while (len - off >= FRAME_HEADER_LEN) {
                if (!validHeader(buf, off)) throw IOException("TCP 帧头非法")
                val flags = buf[off + 4].toInt() and 0xFF
                val payloadLen = readU32(buf, off + 9)
                if (payloadLen > MAX_FRAME_PAYLOAD) throw IOException("TCP 帧长超限")
                val need = FRAME_HEADER_LEN + payloadLen.toInt() +
                    if (flags and FLAG_ENCRYPTED != 0) AEAD_TAG_LEN else 0
                if (len - off < need) break
                val frame = buf.copyOfRange(off, off + need)
                LinkxRuntime.feedTcp(frame)
                off += need
            }
            if (off == len) { // 一帧没剩，回到起点
                off = 0
                len = 0
            } else if (off > 0) {
                val remain = len - off
                System.arraycopy(buf, off, buf, 0, remain)
                len = remain
                off = 0
            }
        }
    }

    /** 发送一个完整帧（写失败时关闭 socket，由读线程统一上报断开）。 */
    fun send(frame: ByteArray) {
        val s = socket ?: return
        synchronized(writeLock) {
            try {
                val out = s.getOutputStream()
                out.write(frame)
                out.flush()
            } catch (e: Exception) {
                Log.w(TAG, "TCP 写失败：${e.message}")
                runCatching { s.close() }
            }
        }
    }

    /** 主动关闭（不回调 tcpClosed，调用方负责通知 Core）。 */
    fun close() {
        closing = true
        running = false
        runCatching { socket?.close() }
        socket = null
    }

    private fun validHeader(b: ByteArray, off: Int): Boolean {
        val magic = ((b[off].toInt() and 0xFF) shl 8) or (b[off + 1].toInt() and 0xFF)
        val ver = b[off + 2].toInt() and 0xFF
        return magic == MAGIC && ver == PROTOCOL_VERSION
    }

    private fun readU32(b: ByteArray, off: Int): Long =
        ((b[off].toLong() and 0xFF) shl 24) or
            ((b[off + 1].toLong() and 0xFF) shl 16) or
            ((b[off + 2].toLong() and 0xFF) shl 8) or
            (b[off + 3].toLong() and 0xFF)
}

// ==================== UDP 发现 ====================

/**
 * UDP 发现（镜像 `Crates/lan/src/discovery.rs`）：绑定 UDP [LINKX_TCP_PORT]（广播使能），
 * 每 3s 广播一次 TLV 信标 `tag(1B)|len(1B)|value`：
 * TAG_ADVERT_NAME=0x01(设备名) / TAG_OS=0x02(=1 Android) / TAG_VERSION=0x03(版本)。
 * 收到对端信标即上报其 IPv4（首个/每次均可，由运行时去重）。
 */
internal object LanDiscovery {
    private const val TAG = "LinkX.Lan"
    private const val BROADCAST_INTERVAL_MS = 3_000L
    private const val MAX_BEACON = 64

    @Volatile private var running = false
    @Volatile private var socket: DatagramSocket? = null
    private var thread: Thread? = null

    fun isRunning(): Boolean = running

    /** 启动广播 + 监听（幂等）；回调在后台线程触发。 */
    fun start(deviceName: String, version: String, onPeer: (String) -> Unit) {
        if (running) return
        running = true
        thread = Thread({ runLoop(deviceName, version, onPeer) }, "linkx-lan")
            .apply { isDaemon = true }
            .also { it.start() }
    }

    fun stop() {
        running = false
        runCatching { socket?.close() }
        socket = null
        thread = null
    }

    private fun runLoop(deviceName: String, version: String, onPeer: (String) -> Unit) {
        val sock = try {
            DatagramSocket(LINKX_TCP_PORT).apply {
                broadcast = true
                soTimeout = 500
            }
        } catch (e: Exception) {
            Log.w(TAG, "UDP 绑定 $LINKX_TCP_PORT 失败：${e.message}")
            running = false
            return
        }
        socket = sock
        val beacon = encodeBeacon(deviceName, version)
        val broadcastAddr = try {
            InetAddress.getByName("255.255.255.255")
        } catch (e: Exception) {
            null
        }
        val buf = ByteArray(MAX_BEACON + 1)
        var lastBroadcast = 0L
        Log.i(TAG, "UDP 发现已启动（端口 $LINKX_TCP_PORT）")
        try {
            while (running) {
                val now = System.currentTimeMillis()
                if (broadcastAddr != null && now - lastBroadcast >= BROADCAST_INTERVAL_MS) {
                    lastBroadcast = now
                    runCatching {
                        sock.send(DatagramPacket(beacon, beacon.size, broadcastAddr, LINKX_TCP_PORT))
                    }
                }
                val pkt = DatagramPacket(buf, buf.size)
                try {
                    sock.receive(pkt)
                } catch (e: SocketTimeoutException) {
                    continue
                } catch (e: Exception) {
                    if (!running) break else continue
                }
                val os = if (pkt.length in 1..MAX_BEACON) beaconOs(buf, pkt.length) else null
                if (os != null) {
                    val ip = pkt.address?.hostAddress
                    when {
                        ip.isNullOrEmpty() -> {}
                        isLocalAddress(pkt.address) -> Log.d(TAG, "忽略本机信标 $ip")
                        // LAN 通道只有「手机 → 电脑」一个方向（Windows 是 55676 唯一监听方），连另一台安卓没有意义，同 OS 一律忽略。
                        os == Tlv.OS_ANDROID -> {}
                        else -> runCatching { onPeer(ip) }
                    }
                }
            }
        } finally {
            runCatching { sock.close() }
            socket = null
        }
        Log.i(TAG, "UDP 发现已停止")
    }

    /** 组信标（总量 ≤64B：name 上限 40B + os + version 16B + 开销 7B = 63B）。 */
    private fun encodeBeacon(deviceName: String, version: String): ByteArray {
        val name = truncateUtf8(deviceName, 40)
        val ver = truncateUtf8(version, 16)
        val os = byteArrayOf(Tlv.OS_ANDROID.toByte())
        val out = ArrayList<Byte>(64)
        fun put(tag: Int, value: ByteArray) {
            out.add(tag.toByte())
            out.add(value.size.toByte())
            for (b in value) out.add(b)
        }
        put(Tlv.TAG_ADVERT_NAME, name)
        put(Tlv.TAG_OS, os)
        put(Tlv.TAG_VERSION, ver)
        return out.toByteArray()
    }

    /** 校验是否为合法 TLV 信标并取出 `TAG_OS`；非法返回 null（任意输入不得抛异常）。"是不是本机"光看 IP 不可靠（多网卡/IPv6 回环），OS 才是稳定判据。 */
    private fun beaconOs(buf: ByteArray, len: Int): Int? {
        var off = 0
        var os: Int? = null
        while (off < len) {
            if (off + 2 > len) return null
            val tag = buf[off].toInt() and 0xFF
            val l = buf[off + 1].toInt() and 0xFF
            off += 2
            if (off + l > len) return null
            if (tag == Tlv.TAG_OS && l >= 1) os = buf[off].toInt() and 0xFF
            off += l
        }
        return os
    }

    /** 该地址是否属于本机某个网络接口（本机广播回灌的判定）。 */
    private fun isLocalAddress(addr: InetAddress?): Boolean {
        if (addr == null) return false
        return runCatching { NetworkInterface.getByInetAddress(addr) != null }.getOrDefault(false)
    }

    /** 在 UTF-8 字符边界上截断到不超过 `maxBytes` 字节。 */
    private fun truncateUtf8(s: String, maxBytes: Int): ByteArray {
        val bytes = s.toByteArray(Charsets.UTF_8)
        if (bytes.size <= maxBytes) return bytes
        var end = maxBytes
        while (end > 0 && (bytes[end].toInt() and 0xC0) == 0x80) end--
        return bytes.copyOf(end)
    }
}

// ==================== 文件传输 ====================

/**
 * 文件传输：
 * - 发送：读 SAF Uri → 立刻 FILE_META（不带摘要）→ 逐 256KB 分块（每块 CRC32，摘要同步增量算）→ FILE_DONE 交付整文件 SHA-256；
 * - 接收：按 FILE_META 落盘，`index * chunk_size` 随机写，逐块校验 CRC32，空洞回 MSG_RESUME 续传；收尾用本端增量摘要比对（校验源优先 FILE_DONE，缺位才回落 FILE_META）。
 * 磁盘 IO 全部在独立守护线程（[worker]）串行执行：pump 锁内只投递任务，不阻塞事件分发。
 */
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
