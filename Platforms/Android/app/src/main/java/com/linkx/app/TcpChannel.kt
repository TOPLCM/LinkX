/**
 * TCP 通道（Android 为客户端）：持有 [Socket] 与读线程，把 13B 帧头切分出的完整帧喂给 Core。
 *
 * 由原先的 `LinkxRuntime.kt` 拆出（原文件里它是第二个顶层 object）。
 * 帧格式见 `Crates/protocol/src/frame.rs`：`[magic(2) version(1) type(1) flags(1) seq(4) payload_len(4)] [body]`，
 * body 长度 = payload_len +（flags & ENCRYPTED ? 16B AEAD tag : 0）。
 * socket 异常/对端关闭时回调 [LinkxRuntime.tcpClosed]（复位绑定状态）。
 */

package com.linkx.app

import android.util.Log
import java.io.IOException
import java.net.InetSocketAddress
import java.net.Socket
import java.net.SocketTimeoutException

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
