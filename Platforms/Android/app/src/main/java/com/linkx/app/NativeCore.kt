package com.linkx.app

import android.util.Log
import java.nio.ByteBuffer

/**
 * Rust Core 桥接（JNI 三条铁律：回调走 TX 队列、谁分配谁释放、不跨边界抛异常）。
 * 对应 Crates/ffi/src/jni_bridge.rs 的 Java_com_linkx_app_NativeCore_* 导出。
 *
 * 注意：native* 方法必须是本类（com.linkx.app.NativeCore）的 @JvmStatic external，
 * JNI 符号按 `Java_com_linkx_app_NativeCore_*` 绑定，移到别的类会找不到实现。
 *
 * 二进制编解码（与本类 ByteBuffer 默认大端逐项对齐，见 jni_bridge 模块头注释）：
 * - 出站包（nativeDrain）：重复 `u16 pkt_len | pkt`；空数组 = 无待发。
 * - TCP 出站帧（nativeDrainTcp）：重复 `u32 frame_len | frame`；空数组 = 无待发。
 * - 入站分块（nativeTakeChunks）：
 *   重复 `u32 rec_len | u64 file_id | u32 index | u32 crc32 | u32 data_len | data`。
 * - 事件流（nativePollEvents）：重复 `u16 ev_len | u8 kind | payload`：
 *   1 StateChanged: `u8 state`；2 PeerHello: `u8 os | u16 name | u16 ver`；
 *   3 SasReady: `u32 sas`；4 PeerPaired: `u16 fp`；
 *   5 Notification: `i64 ts_ms | u32 key_hash | u16 pkg | u16 title | u16 text`；
 *   6 Clipboard: `u16 text`；7 Error: `i32 code | u16 ctx`；
 *   8 FileMeta: `u64 file_id | u64 size | u32 chunk_size | u32 crc32 | 32B sha256 | u16 name`
 *     （sha256 全零 = 发端流式计算，摘要改由 FILE_DONE 交付）；
 *   9 FileDone: `u64 file_id | u8 ok | u16 error | u8 sha_len | sha_len B`
 *     （空串 = 无 error；`sha_len` 0 = 本帧没带摘要；尾段可缺 = 旧 .so）；
 *   10 FileResume: `u64 file_id | u32 from_index`；
 *   11 TcpBound（无负载）；12 TcpUnbound: `u16 reason`；
 *   13 Config: `u16 count | (u16 key | u16 value | u16 scope) * count`；
 *   14 IdentityChanged: `u16 name | u16 old_fp | u16 new_fp`；
 *   15 MediaCommand: `i32 action | i32 volume | i64 delta_ms`；
 *   16 FileTaskFailed: `u64 file_id | u16 reason`（文件任务被引擎硬失败，必须可见）；
 *   17 FileTaskCancelled: `u64 file_id | u16 reason`（用户取消，**不是失败**）；
 *   18 AlbumListRequested: `u32 page | u32 per_page`（相册，手机是应答方）；
 *   19 AlbumThumbRequested: `u64 id | u32 edge`；
 *   20 AlbumFullRequested: `u32 count | count × u64 id`（引擎侧 count 上限 256）；
 *   21 NotifyReplyRequested: `u32 reply_id | i32 notification_id | i32 action_index | u16 pkg | u16 tag | u16 result_key | u16 text`。
 *
 * 21 以下已占用，新事件从 22 起加；`MediaState` / `DeviceStatus` / 回复回执 手机是生产者，故意不编码。
 * 相册的 `AlbumList` / `AlbumThumb` 同理：它们是手机**发出去**的应答，走
 * [nativeSendAlbumList] / [nativeSendAlbumThumb]，不会出现在事件流里。
 */
object NativeCore {
    private const val TAG = "LinkX.NativeCore"

    /** .so 未打包时 loadLibrary 失败，后续 native 调用会抛 UnsatisfiedLinkError，调用方须 runCatching 兜住。 */
    private val loaded: Boolean = runCatching { System.loadLibrary("linkx_core") }
        .onFailure { Log.w(TAG, "linkx_core 未加载（.so 缺失？）", it) }
        .isSuccess

    /** Core 是否可用（供 UI 提示；不做强校验）。 */
    fun isLoaded(): Boolean = loaded

    // ---------- JNI 导出（勿改名/挪类） ----------

    @JvmStatic external fun nativeVersion(): String

    /**
     * role: 1=Initiator，0=Responder（Android 用 Responder=0）；返回句柄，0 = 失败。
     * 参数语义：
     * - `identityDer`：RSA-2048 设备身份（PKCS#8 DER，Keystore 解密后的明文）——
     *   设备识别与信任判定的唯一锚点；
     * - `trustedTsv`：信任库文本（`指纹\t名称` 逐行，由 [nativeTrustUpsert] 生成）；
     * - `skHex`：X25519 static（只做 Noise 握手，不再参与设备识别）。
     */
    @JvmStatic external fun nativeSessionNew(
        role: Int,
        name: String,
        version: String,
        skHex: String,
        identityDer: ByteArray,
        trustedTsv: String,
    ): Long

    /** 生成新的 RSA-2048 设备身份（PKCS#8 DER）；失败返回 null（调用方须显式提示）。 */
    @JvmStatic external fun nativeIdentityGenerate(): ByteArray?

    /** 由 PKCS#8 DER 计算本机指纹（16 位小写 hex）；DER 非法返回 null。 */
    @JvmStatic external fun nativeIdentityFingerprint(identityDer: ByteArray): String?

    @JvmStatic external fun nativeTcpOutDepth(handle: Long): Int

    @JvmStatic external fun nativeTcpOutWindow(handle: Long): Int

    /** 从通知标题/正文抽验证码；规则在共享核心层（`linkx_session::code_extract`），两端必须给同一个答案，所以这里只是透传。无可信结果返回 null。 */
    @JvmStatic external fun nativeExtractCode(title: String, text: String): String?

    /** 把一次配对结果并入信任库，返回新的 TSV（Kotlin 只负责持久化返回值）。 */
    @JvmStatic external fun nativeTrustUpsert(
        trustedTsv: String,
        fingerprint: String,
        peerName: String,
    ): String?

    /** 开关 Debug 模式（`dir` 为日志目录）；1 = 成功。 */
    @JvmStatic external fun nativeSetDebugEnabled(on: Boolean, dir: String): Int

    /** 导出 Debug 日志到 `dir`，返回导出路径；失败返回 null。 */
    @JvmStatic external fun nativeExportDebug(dir: String): String?

    @JvmStatic external fun nativeSessionFree(handle: Long)

    @JvmStatic external fun nativeStart(handle: Long)

    @JvmStatic external fun nativeTick(handle: Long)

    /** 喂入一个 BLE 分片包（长度 = 链路 MTU，上限 517B；见 `nativeSetBleMtu`）。 */
    @JvmStatic external fun nativeFeed(handle: Long, data: ByteArray)

    /** 待发分片包：重复 `u16 长度 | 包体`（大端）；空数组 = 无。 */
    @JvmStatic external fun nativeDrain(handle: Long): ByteArray

    @JvmStatic external fun nativePollEvents(handle: Long): ByteArray

    @JvmStatic external fun nativeConfirmSas(handle: Long)

    @JvmStatic external fun nativeRejectSas(handle: Long)

    @JvmStatic external fun nativeAcceptFingerprint(handle: Long)

    /** 推送当前播放状态（手机 → 电脑），1 = 已入队。`speedX100` 用整数倍率而非浮点：跨 JNI 传 f32 两端都要各自换算，传整数少一处能写错的地方。 */
    @JvmStatic external fun nativeSendMediaState(
        handle: Long,
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
    ): Int

    /** 手机设备状态（电量/充电态）上报。1 = 已入队（未配对时为 0，下一轮重试）。 */
    @JvmStatic external fun nativeSendDeviceStatus(
        handle: Long,
        battery: Int,
        charging: Boolean,
        tsMs: Long,
    ): Int

    /**
     * 带稳定 key 与**回复定位三元组**的通知推送。
     * `keyHash=0` = 无稳定 key；`canReply=false` 时后三个回复字段一律不填，对端不会给回复入口。
     * 1 = 已入队。
     */
    @JvmStatic external fun nativeSendNotificationKeyed(
        handle: Long,
        pkg: String,
        title: String,
        text: String,
        tsMs: Long,
        keyHash: Int,
        tag: String,
        notificationId: Int,
        canReply: Boolean,
        replyActionIndex: Int,
        replyResultKey: String,
    ): Int

    /** 回复回执（手机 → 电脑）：`error` 非空 = 有一句要如实显示给用户的话。1 = 已入队。 */
    @JvmStatic external fun nativeSendNotifyReplyAck(
        handle: Long,
        replyId: Int,
        pkg: String,
        ok: Boolean,
        error: String,
    ): Int

    /**
     * 「这条通知已经不在了」（手机 → 电脑）：电脑据此撤掉回复入口，不删电脑上的历史记录。
     * 定位与推送同一套 pkg + tag + id。1 = 已入队。
     */
    @JvmStatic external fun nativeSendNotifyDismiss(
        handle: Long,
        pkg: String,
        tag: String,
        notificationId: Int,
        keyHash: Int,
    ): Int

    @JvmStatic external fun nativeSendClipboard(handle: Long, text: String): Int

    /**
     * 注入本链路协商到的 ATT MTU，出站分片长度随之改变。
     * 写死小 MTU（如 23）而真机协商值可达 **517** 时，一次设备身份交换要发几十片，
     * 握手时限就是这么被顶穿的。越界值由 Rust 侧忽略并保留现值，故这里无须校验。
     */
    @JvmStatic external fun nativeSetBleMtu(handle: Long, mtu: Int)

    // ---------- TCP 通道 ----------

    /** TCP 通道是否有待发帧（1 = 有）。 */
    @JvmStatic external fun nativeHasTcpOutbound(handle: Long): Int

    /** 待发 TCP 完整帧：重复 `u32 长度 | 帧`（大端）；空数组 = 无。 */
    @JvmStatic external fun nativeDrainTcp(handle: Long): ByteArray

    /** 喂入收到的 TCP 完整帧字节（由平台层按 13B 帧头切分后调用）。 */
    @JvmStatic external fun nativeFeedTcp(handle: Long, data: ByteArray)

    /** 发起 TCP 通道绑定：role 1=TcpClient（主动连接），0=TcpServer（监听）。 */
    @JvmStatic external fun nativeBeginTcpBind(handle: Long, role: Int): Int

    /** TCP socket 已关闭/出错（绑定状态复位；在途传输大声失败，不降级 BLE）。 */
    @JvmStatic external fun nativeTcpClosed(handle: Long, reason: String)

    /** TCP 通道是否已绑定（1 = 已绑定）。 */
    @JvmStatic external fun nativeIsTcpBound(handle: Long): Int

    // ---------- 文件传输 ----------

    /** 发送 FILE_META。`sha256` 传空数组 = 摘要延到 FILE_DONE（大文件不预扫）。1 = 已入队。`albumId` 非 0 = 对 `ALBUM_FULL_REQ` 的原图应答，电脑侧据此改落点；默认值由 [LinkxRuntime.sendFileMeta] 承担。 */
    @JvmStatic external fun nativeSendFileMeta(
        handle: Long,
        fileId: Long,
        name: String,
        size: Long,
        chunkSize: Int,
        crc32: Int,
        sha256: ByteArray,
        albumId: Long,
    ): Int

    /** 发送 FILE_CHUNK；`crc32` 由平台层按 `data` 计算。1 = 已入队。 */
    @JvmStatic external fun nativeSendFileChunk(
        handle: Long,
        fileId: Long,
        index: Int,
        crc32: Int,
        data: ByteArray,
    ): Int

    /** 发送 FILE_DONE；`ok` 1 = 成功，`error` 空串 = 无。`sha256` 为发端边发边算的 32B 整文件摘要，长度非 32 视为本帧不带。1 = 已入队。 */
    @JvmStatic external fun nativeSendFileDone(
        handle: Long,
        fileId: Long,
        ok: Int,
        error: String,
        sha256: ByteArray,
    ): Int

    /** 发送 MSG_RESUME：请求对端从 `fromIndex` 续传。1 = 已入队。 */
    @JvmStatic external fun nativeSendFileResume(handle: Long, fileId: Long, fromIndex: Int): Int

    /** 本端（**发送侧**）取消：引擎停发分块、释放通道锁、发 FILE_DONE{cancelled:true}，并回一条 kind 17 事件。0 = 结束帧没发出（对端会停在"传输中"）。 */
    @JvmStatic external fun nativeCancelFileSend(handle: Long, fileId: Long, reason: String): Int

    /** 本端（**接收侧**）取消：引擎发 FILE_CANCEL 让对端停手，并回一条 kind 17 事件；残留在本机的半截文件由 Kotlin 侧删除。1 = 取消帧已入队。 */
    @JvmStatic external fun nativeCancelFileRecv(handle: Long, fileId: Long, reason: String): Int

    @JvmStatic external fun nativeTakeChunks(handle: Long): ByteArray

    /** 只取走属于 [fileId] 的入站分块，其余留在引擎队列里（收尾一条传输时用）。 */
    @JvmStatic external fun nativeTakeChunksFor(handle: Long, fileId: Long): ByteArray

    // ---------- 相册：应答出口（手机是应答方） ----------

    /** 发送 ALBUM_LIST：一页相册清单，1 = 已入队（未绑定 TCP 时引擎自己留可读错误）。`rows` 每行 `id|name|size|mtime|width|height`，行间 `\n`；Rust 侧**从右往左**取数字字段，名字里带 `|` 能安全往返，但名字里的 `\n` 必须在拼行前换掉（它是行分隔符）。 */
    @JvmStatic external fun nativeSendAlbumList(
        handle: Long,
        page: Int,
        total: Int,
        error: String,
        rows: String,
    ): Int

    /** 发送 ALBUM_THUMB：一张缩略图，JPEG 随帧走、不落盘（产品口径）。1 = 已入队。`error` 非空时 Rust 侧丢弃 jpeg 只发原因——半张图配一句"失败"比空白格子更难归因。 */
    @JvmStatic external fun nativeSendAlbumThumb(
        handle: Long,
        id: Long,
        edge: Int,
        width: Int,
        height: Int,
        jpeg: ByteArray,
        error: String,
    ): Int

    // ---------- 配置同步 / 设备管理 ----------

    /** 发送跨端配置项（三个等长字符串数组，分别对应 key/value/scope）。1 = 已入队。 */
    @JvmStatic external fun nativeSendConfig(
        handle: Long,
        keys: Array<String>,
        values: Array<String>,
        scopes: Array<String>,
    ): Int

    /** 解绑对端：清 Core TOFU 信任库并复位会话（平台层须同步清除本地持久化指纹）。 */
    @JvmStatic external fun nativeUnbind(handle: Long)

    /**
     * 启动本机回环调试控制面（只绑 127.0.0.1），返回实际监听地址。⚠ 交付版 `.so` 不带
     * `agent-debug` feature，此符号根本不参与编译，直接调用会抛 `UnsatisfiedLinkError`；
     * 调用方一律 `runCatching`，并把"调用成功"本身当作"当前是调试变体"的探测手段。
     */
    @JvmStatic external fun nativeDebugdStart(logDir: String, port: Int): String

    /** 回报计数（负数表示水位，只在更大时覆盖）。仅调试变体存在该符号。 */
    @JvmStatic external fun nativeDebugCounter(name: String, delta: Long)

    /**
     * 取走一个待执行调试动作，格式 `动作名\tquery串`；无待办返回空串。HTTP 线程只入队，
     * **由 `LinkxRuntime.pump()` 取走并执行**：`@Synchronized` 才是引擎互斥的来源，
     * HTTP 线程直接调 JNI 就是绕过锁裸解引用引擎。
     */
    @JvmStatic external fun nativeDebugTakeRequest(): String

    /** 把一项宿主观测值并入控制面 `/state` 的 `host` 面（剪贴板、主题、开关…）。 */
    @JvmStatic external fun nativeDebugSetField(key: String, value: String)

    // ---------- Kotlin 侧解码辅助（以下列表函数：空/异常一律返回空列表；编码见类注释） ----------

    fun drainPackets(handle: Long): List<ByteArray> {
        val raw = runCatching { nativeDrain(handle) }.getOrNull() ?: return emptyList()
        if (raw.isEmpty()) return emptyList()
        val buf = ByteBuffer.wrap(raw)
        val out = ArrayList<ByteArray>(4)
        while (buf.remaining() >= 2) {
            val len = buf.short.toInt() and 0xFFFF
            if (len <= 0) continue
            if (len > buf.remaining()) break // 畸形（长度超余量）：放弃剩余
            val pkt = ByteArray(len)
            buf.get(pkt)
            out.add(pkt)
        }
        return out
    }

    fun drainTcp(handle: Long): List<ByteArray> {
        val raw = runCatching { nativeDrainTcp(handle) }.getOrNull() ?: return emptyList()
        if (raw.isEmpty()) return emptyList()
        val buf = ByteBuffer.wrap(raw)
        val out = ArrayList<ByteArray>(4)
        while (buf.remaining() >= 4) {
            val len = buf.int.toLong() and 0xFFFFFFFFL
            if (len <= 0L || len > buf.remaining()) break // 畸形：放弃剩余
            val frame = ByteArray(len.toInt())
            buf.get(frame)
            out.add(frame)
        }
        return out
    }

    fun takeChunks(handle: Long): List<IncomingChunk> =
        parseChunks(runCatching { nativeTakeChunks(handle) }.getOrNull())

    fun takeChunksFor(handle: Long, fileId: Long): List<IncomingChunk> =
        parseChunks(runCatching { nativeTakeChunksFor(handle, fileId) }.getOrNull())

    private fun parseChunks(raw: ByteArray?): List<IncomingChunk> {
        if (raw == null || raw.isEmpty()) return emptyList()
        val buf = ByteBuffer.wrap(raw)
        val out = ArrayList<IncomingChunk>(4)
        while (buf.remaining() >= 20) {
            val recLen = buf.int.toLong() and 0xFFFFFFFFL
            if (recLen < 20L || recLen > buf.remaining()) break
            val start = buf.position()
            val fileId = buf.long
            val index = buf.int
            val crc32 = buf.int
            val dataLen = buf.int.toLong() and 0xFFFFFFFFL
            if (dataLen > buf.remaining()) break
            val data = ByteArray(dataLen.toInt())
            buf.get(data)
            if ((buf.position() - start).toLong() != recLen) break // 长度自洽校验
            out.add(IncomingChunk(fileId, index, crc32, data))
        }
        return out
    }

    fun pollEvents(handle: Long): List<LinkxEvent> {
        val raw = runCatching { nativePollEvents(handle) }.getOrNull() ?: return emptyList()
        if (raw.isEmpty()) return emptyList()
        val buf = ByteBuffer.wrap(raw)
        val out = ArrayList<LinkxEvent>(4)
        while (buf.remaining() >= 3) {
            val evLen = buf.short.toInt() and 0xFFFF
            if (evLen <= 0 || evLen > buf.remaining()) break
            val body = ByteArray(evLen)
            buf.get(body)
            runCatching { decodeEvent(body) }.getOrNull()?.let { out.add(it) }
        }
        return out
    }

    /** 单条事件解码：任何字段越界/畸形都返回 null（绝不抛异常出解码器）。 */
    private fun decodeEvent(body: ByteArray): LinkxEvent? {
        val b = ByteBuffer.wrap(body)
        if (!b.hasRemaining()) return null
        val kind = b.get().toInt() and 0xFF
        return when (kind) {
            1 -> LinkxEvent.StateChanged(u8(b) ?: return null)
            2 -> {
                val os = u8(b) ?: return null
                val name = str(b) ?: return null
                val ver = str(b) ?: return null
                LinkxEvent.PeerHello(os, name, ver)
            }
            3 -> LinkxEvent.SasReady(i32(b) ?: return null)
            4 -> LinkxEvent.PeerPaired(str(b) ?: return null)
            5 -> {
                val ts = i64(b) ?: return null
                val keyHash = i32(b) ?: return null
                val pkg = str(b) ?: return null
                val title = str(b) ?: return null
                val text = str(b) ?: return null
                LinkxEvent.Notification(ts, keyHash, pkg, title, text)
            }
            6 -> LinkxEvent.Clipboard(str(b) ?: return null)
            7 -> {
                val code = i32(b) ?: return null
                val ctx = str(b) ?: return null
                LinkxEvent.Error(code, ctx)
            }
            8 -> {
                val fileId = i64(b) ?: return null
                val size = i64(b) ?: return null
                val chunkSize = u32(b) ?: return null
                val crc32 = u32(b) ?: return null
                val sha256 = bytes(b, 32) ?: return null
                val name = str(b) ?: return null
                LinkxEvent.FileMeta(fileId, size, chunkSize.toInt(), crc32, sha256, name)
            }
            9 -> {
                val fileId = i64(b) ?: return null
                val ok = u8(b) ?: return null
                val error = str(b) ?: return null
                // 摘要尾段：u8 长度 + 字节；旧 .so 不写 → 按「本帧没带摘要」处理
                val sha = if (b.remaining() >= 1) {
                    val n = u8(b) ?: return null
                    bytes(b, n) ?: return null
                } else {
                    ByteArray(0)
                }
                LinkxEvent.FileDone(fileId, ok != 0, error, sha)
            }
            10 -> {
                val fileId = i64(b) ?: return null
                val from = u32(b) ?: return null
                LinkxEvent.FileResume(fileId, from.toInt())
            }
            11 -> LinkxEvent.TcpBound
            12 -> LinkxEvent.TcpUnbound(str(b) ?: return null)
            13 -> {
                val count = u16(b) ?: return null
                val entries = ArrayList<ConfigItem>(count)
                for (i in 0 until count) {
                    val k = str(b) ?: break
                    val v = str(b) ?: break
                    val sc = str(b) ?: break
                    entries.add(ConfigItem(k, v, sc))
                }
                LinkxEvent.Config(entries)
            }
            14 -> {
                val name = str(b) ?: return null
                val oldFp = str(b) ?: return null
                val newFp = str(b) ?: return null
                LinkxEvent.IdentityChanged(name, oldFp, newFp)
            }
            // 电脑下发的播放控制指令（action/volume/delta 全是大端定长）
            15 -> {
                val action = i32(b) ?: return null
                val volume = i32(b) ?: return null
                val delta = i64(b) ?: return null
                LinkxEvent.MediaCommand(action, volume, delta)
            }
            // 通道闸门把一次文件传输判为硬失败 → 必须变成用户看得见的状态
            16 -> {
                val fileId = i64(b) ?: return null
                val reason = str(b) ?: return null
                LinkxEvent.FileTaskFailed(fileId, reason)
            }
            // 用户取消（本端点的、对端发来的、收到对端 cancelled 收尾都归到这一条）：
            // 布局与 16 相同，但**不是失败**——把用户动作显示成故障就是谎报系统坏了
            17 -> {
                val fileId = i64(b) ?: return null
                val reason = str(b) ?: return null
                LinkxEvent.FileTaskCancelled(fileId, reason)
            }
            // 相册：电脑问、手机答。三条都只可能落到手机侧。
            18 -> {
                val page = u32(b) ?: return null
                val perPage = u32(b) ?: return null
                LinkxEvent.AlbumListRequested(page.toInt(), perPage.toInt())
            }
            19 -> {
                val id = i64(b) ?: return null
                val edge = u32(b) ?: return null
                LinkxEvent.AlbumThumbRequested(id, edge.toInt())
            }
            20 -> {
                val count = u32(b) ?: return null
                // 条数按编码上限 256 收口：畸形 count 若直接拿去分配，一条事件就能撑爆内存
                val n = count.coerceAtMost(256L).toInt()
                val ids = ArrayList<Long>(n)
                for (i in 0 until n) {
                    val id = i64(b) ?: break
                    ids.add(id)
                }
                LinkxEvent.AlbumFullRequested(ids)
            }
            // 电脑要回复某条通知：定位三元组 + 要填的 key + 正文（字段序见 jni_bridge 模块头 kind 21）
            21 -> {
                val replyId = u32(b) ?: return null
                val notificationId = i32(b) ?: return null
                val actionIndex = i32(b) ?: return null
                val pkg = str(b) ?: return null
                val tag = str(b) ?: return null
                val resultKey = str(b) ?: return null
                val text = str(b) ?: return null
                LinkxEvent.NotifyReplyRequested(
                    replyId.toInt(), pkg, tag, notificationId, actionIndex, resultKey, text,
                )
            }
            else -> null
        }
    }

    // 大端定长读取器家族：剩余字节不足一律返回 null、绝不抛异常；u32/i64/u64 按 Long 承载。
    private fun u8(b: ByteBuffer): Int? {
        if (b.remaining() < 1) return null
        return b.get().toInt() and 0xFF
    }

    private fun u16(b: ByteBuffer): Int? {
        if (b.remaining() < 2) return null
        return b.short.toInt() and 0xFFFF
    }

    private fun u32(b: ByteBuffer): Long? {
        if (b.remaining() < 4) return null
        return b.int.toLong() and 0xFFFFFFFFL
    }

    private fun i32(b: ByteBuffer): Int? {
        if (b.remaining() < 4) return null
        return b.int
    }

    private fun i64(b: ByteBuffer): Long? {
        if (b.remaining() < 8) return null
        return b.long
    }

    private fun bytes(b: ByteBuffer, n: Int): ByteArray? {
        if (b.remaining() < n) return null
        val arr = ByteArray(n)
        b.get(arr)
        return arr
    }

    /** 读 `u16 长度 | utf8 字节`；空串合法，越界返回 null（不抛异常）。 */
    private fun str(b: ByteBuffer): String? {
        if (b.remaining() < 2) return null
        val n = b.short.toInt() and 0xFFFF
        if (b.remaining() < n) return null
        if (n == 0) return ""
        val arr = ByteArray(n)
        b.get(arr)
        return String(arr, Charsets.UTF_8)
    }
}

/** Core 上行事件（Kotlin 侧镜像 Crates/session EngineEvent）。 */
sealed interface LinkxEvent {
    /** 0..7 = DISCOVER/HANDSHAKE/PAIRING/SAS_COMPARE/PAIRED/REPAIRED/RECONNECTING/CLOSED */
    data class StateChanged(val state: Int) : LinkxEvent
    data class PeerHello(val os: Int, val name: String, val version: String) : LinkxEvent
    data class SasReady(val sas: Int) : LinkxEvent
    data class PeerPaired(val fingerprint: String) : LinkxEvent
    /** keyHash=0 表示无稳定 key；非 0 时同应用同 key 的通知应就地合并更新。 */
    data class Notification(
        val tsMs: Long,
        val keyHash: Int,
        val pkg: String,
        val title: String,
        val text: String,
    ) : LinkxEvent
    data class Clipboard(val text: String) : LinkxEvent
    data class Error(val code: Int, val context: String) : LinkxEvent
    data class FileMeta(
        val fileId: Long,
        val size: Long,
        val chunkSize: Int,
        /** 整文件 CRC32（发送端声明值，收端快速预检）。 */
        val crc32: Long,
        /** 整文件 SHA-256（32B）；**全零 = 发端没预先算，摘要改由 [FileDone] 交付**。 */
        val sha256: ByteArray,
        val name: String,
    ) : LinkxEvent
    /** `sha256`：发端随结束帧交付的整文件摘要（32B），长度 0 = 本帧没带（旧端）。 */
    data class FileDone(
        val fileId: Long,
        val ok: Boolean,
        val error: String,
        val sha256: ByteArray,
    ) : LinkxEvent
    data class FileResume(val fileId: Long, val fromIndex: Int) : LinkxEvent
    /** 文件任务被引擎硬失败（TCP 未绑定就想发大文件 / 传输中 TCP 断开）。与 [Error] 分开建模是有意的：这类失败曾只进日志、UI 照样显示"已完成"——平台侧必须把它落到任务状态上。 */
    data class FileTaskFailed(val fileId: Long, val reason: String) : LinkxEvent
    /** 文件任务被**用户取消**（任一端发起、以及收到对端的取消收尾都归一到这一条）。取消是用户的主动动作、不是系统故障：必须落到「已取消」并显示原因；本端是接收方时还要删掉残留的半截文件。 */
    data class FileTaskCancelled(val fileId: Long, val reason: String) : LinkxEvent
    data object TcpBound : LinkxEvent
    data class TcpUnbound(val reason: String) : LinkxEvent
    data class Config(val entries: List<ConfigItem>) : LinkxEvent
    /** 同名设备呈递了**新身份**（典型场景：对方重装）。信任锚是对端 RSA 指纹，不是设备名——UI 必须弹「重新配对确认」交人工裁决；接受后调 [LinkxRuntime.acceptFingerprint]，拒绝则调 [LinkxRuntime.rejectSas]（引擎 → CLOSED）。 */
    data class IdentityChanged(
        val name: String,
        val oldFingerprint: String,
        val newFingerprint: String,
    ) : LinkxEvent

    /** 电脑下发的播放控制指令。`action` 取值见 `Proto/linkx/v1/media.proto` 的 `MediaCommand.Action`。 */
    data class MediaCommand(val action: Int, val volume: Int, val deltaMs: Long) : LinkxEvent

    /** 相册：电脑要第 [page] 页、每页 [perPage] 张（0 页起）。应答经 [AlbumProvider] 生成后走 [LinkxRuntime.sendAlbumList]。 */
    data class AlbumListRequested(val page: Int, val perPage: Int) : LinkxEvent

    /** 相册：电脑要 [id] 这张的缩略图，[edge] = 长边像素（手机侧还会再夹一次）。 */
    data class AlbumThumbRequested(val id: Long, val edge: Int) : LinkxEvent

    /** 相册：电脑要这些照片的**原图**，回包走 FILE_META / FILE_CHUNK / FILE_DONE（`FileMeta.album_id` 非 0 = 相册应答），不是另一条消息。 */
    data class AlbumFullRequested(val ids: List<Long>) : LinkxEvent

    /**
     * 电脑要回复一条通知（NOTIFY_REPLY 0x11）。手机侧凭 `pkg + tag + notificationId` 在**仍在通知栏**
     * 的条目里现找，再按 `actionIndex` / `resultKey` 填 RemoteInput；无论成败都必须回一条回执。
     * `tag` 为空串 = 这条通知没有 tag（Android 侧 tag 可为 null）。
     */
    data class NotifyReplyRequested(
        val replyId: Int,
        val pkg: String,
        val tag: String,
        val notificationId: Int,
        val actionIndex: Int,
        val resultKey: String,
        val text: String,
    ) : LinkxEvent
}

/** 跨端配置项（镜像 Crates/session ConfigEntryItem；scope ∈ local/cross/per_peer）。 */
data class ConfigItem(val key: String, val value: String, val scope: String)

/** 入站文件分块（镜像 Crates/session IncomingChunk；大载荷独立于事件流）。 */
data class IncomingChunk(val fileId: Long, val index: Int, val crc32: Int, val data: ByteArray)
