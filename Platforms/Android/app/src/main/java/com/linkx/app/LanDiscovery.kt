/**
 * UDP 发现（镜像 `Crates/lan/src/discovery.rs`）：绑定 UDP [LINKX_TCP_PORT]（广播使能），
 * 每 3s 广播一次 TLV 信标 `tag(1B)|len(1B)|value`：
 * TAG_ADVERT_NAME=0x01(设备名) / TAG_OS=0x02(=1 Android) / TAG_VERSION=0x03(版本)。
 * 收到对端信标即上报其 IPv4（首个/每次均可，由运行时去重）。
 *
 * 由原先的 `LinkxRuntime.kt` 拆出（原文件里它是第三个顶层 object）。
 */

package com.linkx.app

import android.util.Log
import java.net.DatagramPacket
import java.net.DatagramSocket
import java.net.InetAddress
import java.net.NetworkInterface
import java.net.SocketTimeoutException
import java.util.ArrayList

internal object LanDiscovery {
    private const val TAG = "LinkX.Lan"
    private const val BROADCAST_INTERVAL_MS = 3_000L
    private const val MAX_BEACON = 64

    @Volatile private var running = false
    @Volatile private var socket: DatagramSocket? = null
    private var thread: Thread? = null


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
