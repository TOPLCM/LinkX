package com.linkx.app

import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.app.Service
import android.bluetooth.BluetoothAdapter
import android.bluetooth.BluetoothGatt
import android.bluetooth.BluetoothGattCharacteristic
import android.bluetooth.BluetoothGattDescriptor
import android.bluetooth.BluetoothGattServer
import android.bluetooth.BluetoothGattServerCallback
import android.bluetooth.BluetoothGattService
import android.bluetooth.BluetoothManager
import android.bluetooth.BluetoothProfile
import android.bluetooth.le.AdvertiseCallback
import android.bluetooth.le.BluetoothLeAdvertiser
import android.bluetooth.le.AdvertiseData
import android.bluetooth.le.AdvertiseSettings
import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.content.IntentFilter
import android.content.pm.ServiceInfo
import android.os.Build
import android.os.IBinder
import android.os.ParcelUuid
import android.util.Log
import androidx.core.app.NotificationCompat
import androidx.core.app.ServiceCompat
import androidx.core.content.ContextCompat
import java.util.ArrayDeque
import java.util.UUID

/**
 * BLE GATT Server（手机 = Peripheral = Responder）：广播自有 Service UUID +
 * 两个 Characteristic（CHAR_TX 收 / CHAR_EVT notify 发），收发分片包全部交给 LinkxRuntime。
 *
 * 健壮性：manifest 已声明 foregroundServiceType=connectedDevice，服务被拉起后必须尽快
 * startForeground，否则 Android 8+ 抛 ForegroundServiceDidNotStartInTimeException、
 * Android 14 直接崩溃；adapter / openGattServer / sendResponse / notify 在 Android 12+
 * 需 BLUETOOTH_CONNECT / BLUETOOTH_ADVERTISE 运行时权限，未授权会抛 SecurityException，
 * 全部兜住避免进程崩溃。
 */
class BlePeripheralService : Service() {

    private var gattServer: BluetoothGattServer? = null
    private var advertiser: BluetoothLeAdvertiser? = null
    private var btReceiver: BroadcastReceiver? = null
    private var notifyChar: BluetoothGattCharacteristic? = null

    /** 当前连接的 Central（仅一个，点对点）。跨线程读写（GATT 回调 / Runtime 线程）。 */
    @Volatile
    private var connectedDevice: android.bluetooth.BluetoothDevice? = null

    /** CCCD 是否已被订阅（未订阅时 notify 会丢包甚至抛异常）。跨线程读写。 */
    @Volatile
    private var subscribed = false

    /**
     * 订阅前的待发分片包（CCCD 未使能，先缓存）。
     * Runtime 线程（PacketSink → onDeliver）与 GATT 回调线程并发访问，`ArrayDeque`
     * 非线程安全 → 统一由 [pendingLock] 保护，避免并发修改导致崩溃或丢包。
     */
    private val pendingLock = Any()
    private val pendingNotify = ArrayDeque<ByteArray>()

    /**
     * 是否有**一个** notify 正在链路上传输。
     *
     * BLE 规范允许 Peripheral 同一时刻只有一个待确认的 notification：必须等
     * `onNotificationSent` 回调回来才能发下一片。背靠背连发时协议栈自认为全发出去了，
     * 实际会丢片——丢的可能正是 channel binding 这类关键分片，表现成一端已绑定、
     * 另一端永远绑不上，而两端日志都看不出错误。
     */
    @Volatile private var notifyInFlight = false
    /**
     * 保护「检查在途标志 + 取队首 + 置在途」必须是原子的。
     *
     * 只用 @Volatile 标志不够：`pumpNotify` 从 Runtime 的 linkx-tick 线程和 Binder 线程
     * 两处进来，可能同时读到 `notifyInFlight == false`，各自取一片各自发，
     * 退化成"背靠背连发"。
     */
    private val notifyPumpLock = Any()
    private val notifyRetry = android.os.Handler(android.os.Looper.getMainLooper())

    /** 注册给 Runtime 的发送器：把每个分片包经 notify 写回 Central。 */
    private val packetSink = PacketSink { packet -> onDeliver(packet) }

    private val advertiseCallback = object : AdvertiseCallback() {
        override fun onStartSuccess(settingsInEffect: AdvertiseSettings?) {
            Log.i(TAG, "BLE 广播已启动")
        }

        override fun onStartFailure(errorCode: Int) {
            Log.w(TAG, "BLE 广播启动失败: $errorCode")
        }
    }

    private val callback = object : BluetoothGattServerCallback() {
        override fun onConnectionStateChange(device: android.bluetooth.BluetoothDevice, status: Int, newState: Int) {
            Log.i(TAG, "connection: ${device.address} -> $newState")
            when (newState) {
                BluetoothProfile.STATE_CONNECTED -> {
                    connectedDevice = device
                    // 连接建立即注册 sink：分片包会先缓存，待 CCCD 订阅后补发
                    LinkxRuntime.setPacketSink(packetSink)
                }
                BluetoothProfile.STATE_DISCONNECTED -> {
                    connectedDevice = null
                    subscribed = false
                    // 在途标志必须复位：否则重连后 pumpNotify 永远等不到那次
                    // onNotificationSent（它属于上一条连接），整条出站队列会永久卡死。
                    notifyInFlight = false
                    notifyRetry.removeCallbacksAndMessages(null)
                    synchronized(pendingLock) { pendingNotify.clear() }
                    LinkxRuntime.setPacketSink(null)
                }
            }
        }

        /**
         * 把协商到的 ATT MTU 交给 Runtime，由 `pump()` 下发给引擎。
         *
         * 必须走这条路而不是在这里直接调 JNI：本回调在 Binder 线程，不经过
         * `LinkxRuntime` 的 `@Synchronized`，直接调会绕过引擎互斥。
         */
        override fun onMtuChanged(device: android.bluetooth.BluetoothDevice, mtu: Int) {
            Log.i(TAG, "MTU 协商: ${device.address} -> $mtu")
            LinkxRuntime.negotiatedMtu = mtu
        }

        override fun onCharacteristicWriteRequest(
            device: android.bluetooth.BluetoothDevice,
            requestId: Int,
            characteristic: BluetoothGattCharacteristic,
            preparedWrite: Boolean,
            responseNeeded: Boolean,
            offset: Int,
            value: ByteArray,
        ) {
            // 仅当请求方要求响应时才回写，避免对 preparedWrite 组包流程误回。
            if (responseNeeded) {
                try {
                    gattServer?.sendResponse(device, requestId, BluetoothGatt.GATT_SUCCESS, 0, null)
                } catch (e: SecurityException) {
                    Log.w(TAG, "sendResponse 被拒（缺 BLUETOOTH_CONNECT？）", e)
                }
            }
            // 诊断（-212 方向性丢包）：Central→Peripheral 的入站此前**完全无计数**。
            // 出站有 ble_notify_*，入站一个都没有，于是「对端一个字节都没到我这」
            // 与「我收到了但 Core 没认」在日志里长得一模一样。
            dbgCounter("ble_write_in")
            if (offset != 0 || preparedWrite) {
                dbgCounter("ble_write_prepared")
                Log.i(TAG, "write offset=$offset prepared=$preparedWrite len=${value.size}")
            }
            if (preparedWrite) return // 不用长写组包
            // BLE 分片包 → Core 会话；出站包由 Runtime 经 sink 写回
            LinkxRuntime.feed(value)
        }

        override fun onDescriptorWriteRequest(
            device: android.bluetooth.BluetoothDevice,
            requestId: Int,
            descriptor: BluetoothGattDescriptor,
            preparedWrite: Boolean,
            responseNeeded: Boolean,
            offset: Int,
            value: ByteArray,
        ) {
            if (responseNeeded) {
                try {
                    gattServer?.sendResponse(device, requestId, BluetoothGatt.GATT_SUCCESS, 0, null)
                } catch (e: SecurityException) {
                    Log.w(TAG, "sendResponse(descriptor) 被拒", e)
                }
            }
            if (descriptor.uuid != CCCD_UUID) return
            when {
                value.contentEquals(BluetoothGattDescriptor.ENABLE_NOTIFICATION_VALUE) -> {
                    subscribed = true
                    connectedDevice = device
                    flushPendingNotify(device)
                }
                value.contentEquals(BluetoothGattDescriptor.DISABLE_NOTIFICATION_VALUE) -> {
                    subscribed = false
                }
            }
        }

        override fun onNotificationSent(device: android.bluetooth.BluetoothDevice, status: Int) {
            val ok = status == BluetoothGatt.GATT_SUCCESS
            dbgCounter("ble_notify_sent_total")
            dbgCounter(if (ok) "ble_notify_sent_ok" else "ble_notify_sent_fail")
            if (!ok) {
                Log.w(TAG, "notify 完成状态异常: status=$status")
            }
            // 这一句是"单片在途"节律的另一半：链路确认一片完成后才放行下一片；
            // 这里若为空实现，发送侧就退化成背靠背连发、每轮丢一片。
            synchronized(notifyPumpLock) { notifyInFlight = false }
            pumpNotify()
        }
    }

    override fun onCreate() {
        super.onCreate()
        // 服务可能由系统直接拉起（开机广播、绑通知监听），那时 MainActivity.onCreate 还没跑过。
        // 光把 GATT 服务架起来不够：引擎句柄还是 0，对端连上来拿不到 HELLO，通知也一条发不出去
        // ——"广播一个没有大脑的 GATT server"。所以进程级初始化与 start() 必须在这里也走一遍
        //（两处入口共用同一份 boot，重复调用幂等）。
        LinkxRuntime.boot(applicationContext)
        ensureChannel()
        registerGattServer()
        watchBluetoothAdapter()
        LinkxRuntime.start()
    }

    /**
     * 盯蓝牙开关：手机关掉再打开蓝牙后，GATT server 与广播已被系统收回，而本服务
     * 还以为自己在广播——电脑再也扫不到。用运行时注册的接收器（隐式广播禁令只管
     * 清单声明的接收器），收到 STATE_ON 就整条重注册。
     */
    private fun watchBluetoothAdapter() {
        if (btReceiver != null) return
        val receiver = object : BroadcastReceiver() {
            override fun onReceive(context: Context?, intent: Intent?) {
                if (intent?.action != BluetoothAdapter.ACTION_STATE_CHANGED) return
                when (intent.getIntExtra(BluetoothAdapter.EXTRA_STATE, BluetoothAdapter.STATE_OFF)) {
                    BluetoothAdapter.STATE_ON -> {
                        Log.i(TAG, "蓝牙重新打开：重注册 GATT server 与广播")
                        teardownGatt()
                        registerGattServer()
                    }
                    BluetoothAdapter.STATE_OFF -> {
                        // 旧句柄必须丢掉：留着后面的 notify 就是往死对象上写
                        Log.w(TAG, "蓝牙已关闭：BLE Peripheral 下线，等它回来")
                        teardownGatt()
                    }
                }
            }
        }
        val filter = IntentFilter(BluetoothAdapter.ACTION_STATE_CHANGED)
        runCatching {
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
                applicationContext.registerReceiver(receiver, filter, Context.RECEIVER_NOT_EXPORTED)
            } else {
                applicationContext.registerReceiver(receiver, filter)
            }
            btReceiver = receiver
        }.onFailure {
            // 注册不上 = 蓝牙回来后不会自愈，这必须出声，否则症状又被读成"蓝牙坏了"
            Log.w(TAG, "蓝牙状态广播注册失败，蓝牙重开后需要重启 App 才能恢复广播", it)
        }
    }

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        promoteToForeground()
        return START_STICKY
    }

    private fun registerGattServer() {
        try {
            val bm = getSystemService(BluetoothManager::class.java)
            val adapter = bm?.adapter
            if (adapter == null) {
                Log.w(TAG, "本机无蓝牙适配器，BLE Peripheral 不可用")
                return
            }
            val server = bm.openGattServer(this, callback)
            if (server == null) {
                Log.w(TAG, "openGattServer 返回 null（蓝牙关闭或权限不足）")
                return
            }
            gattServer = server
            val write = BluetoothGattCharacteristic(
                UUID.fromString(Tlv.Ble.CHAR_TX),
                BluetoothGattCharacteristic.PROPERTY_WRITE,
                BluetoothGattCharacteristic.PERMISSION_WRITE,
            )
            val notify = BluetoothGattCharacteristic(
                UUID.fromString(Tlv.Ble.CHAR_EVT),
                BluetoothGattCharacteristic.PROPERTY_NOTIFY,
                BluetoothGattCharacteristic.PERMISSION_READ,
            )
            // notify 特征必须挂 CCCD，Central 才能使能订阅
            notify.addDescriptor(
                BluetoothGattDescriptor(
                    CCCD_UUID,
                    BluetoothGattDescriptor.PERMISSION_READ or BluetoothGattDescriptor.PERMISSION_WRITE,
                ),
            )
            notifyChar = notify
            val service = BluetoothGattService(
                UUID.fromString(Tlv.Ble.SERVICE),
                BluetoothGattService.SERVICE_TYPE_PRIMARY,
            ).apply {
                addCharacteristic(write)
                addCharacteristic(notify)
            }
            if (!server.addService(service)) {
                Log.w(TAG, "addService 失败，GATT 服务未注册")
                return
            }
            Log.i(TAG, "GATT server ready")
            startAdvertising(adapter)
        } catch (e: SecurityException) {
            Log.w(TAG, "缺少 BLUETOOTH_CONNECT（Android 12+ 运行时权限），BLE 未启动", e)
        }
    }

    private fun startAdvertising(adapter: BluetoothAdapter) {
        try {
            val adv = adapter.bluetoothLeAdvertiser
            if (adv == null) {
                Log.w(TAG, "bluetoothLeAdvertiser 不可用（蓝牙关闭？）")
                return
            }
            advertiser = adv
            val settings = AdvertiseSettings.Builder()
                .setAdvertiseMode(AdvertiseSettings.ADVERTISE_MODE_LOW_LATENCY)
                .setTxPowerLevel(AdvertiseSettings.ADVERTISE_TX_POWER_MEDIUM)
                .setConnectable(true)
                .build()
            // 广播必须带系统蓝牙名：电脑侧的设备列表与「自动连接已绑定设备」都按名字认领，
            // 名字只在加密通道里给就永远来不及（广播里没有它，列表就是空的无名项，
            // 旁边任何一台带名的设备都会被当成目标连过去）。不想暴露姓名的用户在系统蓝牙
            // 设置里把设备名改成中性名即可，本产品不自造另一个标识符。
            val data = AdvertiseData.Builder()
                .addServiceUuid(ParcelUuid(UUID.fromString(Tlv.Ble.SERVICE)))
                .setIncludeDeviceName(true)
                .build()
            adv.startAdvertising(settings, data, advertiseCallback)
        } catch (e: SecurityException) {
            Log.w(TAG, "startAdvertising 被拒（缺 BLUETOOTH_ADVERTISE？）", e)
        }
    }

    /** 把 Runtime 交来的分片包排入 notify 队列；订阅前先缓存，订阅后由 [pumpNotify] 逐片发。 */
    private fun onDeliver(packet: ByteArray) {
        if (connectedDevice == null) return // 无连接：丢弃（Runtime 侧通常已暂存）
        val dropped = synchronized(pendingLock) {
            val overflow = pendingNotify.size >= PENDING_MAX
            if (overflow) pendingNotify.removeFirst()
            pendingNotify.addLast(packet)
            overflow
        }
        // 丢最旧必须计数：这条路径就是「文件分块进了蓝牙然后消失」的现场，静默发生时取证只能靠推断。
        if (dropped) dbgCounter("ble_notify_drop_oldest")
        if (subscribed) pumpNotify()
    }

    /**
     * 单片在途的 notify 发送器：只有上一片收到 `onNotificationSent` 才发下一片。
     *
     * 被拒（链路忙）时把该片放回队首并延迟重试，**绝不丢弃**——分片组少一片就永远
     * 凑不齐，接收侧 5 s 超时后整组作废，比慢更糟。
     */
    private fun pumpNotify() {
        val device = connectedDevice ?: return
        if (!subscribed) return
        // 「查在途 → 取队首 → 置在途」必须在同一把锁里完成，见 notifyPumpLock 注释。
        // notifyPacket 本身是 Binder 调用，放在锁外，避免占锁跨进程。
        val next = synchronized(notifyPumpLock) {
            if (notifyInFlight) return
            val p = synchronized(pendingLock) {
                if (pendingNotify.isEmpty()) null else pendingNotify.removeFirst()
            } ?: return
            notifyInFlight = true
            p
        }
        if (!notifyPacket(device, next)) {
            val requeued = synchronized(pendingLock) {
                if (pendingNotify.size >= PENDING_MAX) false
                else {
                    pendingNotify.addFirst(next)
                    true
                }
            }
            synchronized(notifyPumpLock) { notifyInFlight = false }
            dbgCounter(if (requeued) "ble_notify_retry_scheduled" else "ble_notify_queue_full")
            if (requeued) notifyRetry.postDelayed({ pumpNotify() }, NOTIFY_RETRY_MS)
        }
    }

    private fun flushPendingNotify(device: android.bluetooth.BluetoothDevice) {
        // CCCD 刚使能：此前排队的分片从这一片开始按在途节律发出。
        // （device 参数保留给调用点表达"给谁发"，实际发送走 connectedDevice。）
        if (device.address == connectedDevice?.address) pumpNotify()
    }

    /** 发一片 notify；返回"是否成功排入发送队列"（不代表已送达）。 */
    private fun notifyPacket(device: android.bluetooth.BluetoothDevice, packet: ByteArray): Boolean {
        val server = gattServer ?: return false
        val ch = notifyChar ?: return false
        dbgCounter("ble_notify_attempt")
        return try {
            // ⚠ 两个重载的返回类型**不一样**：3 参（已废弃）→ boolean；
            //   4 参（API 33+）→ int 状态码（BluetoothStatusCodes.SUCCESS = 0）。
            //   丢弃 4 参的返回码等于扔掉一整条错误反馈通道，这里按各自类型取回并计入控制面。
            val queued: Boolean = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
                server.notifyCharacteristicChanged(device, ch, false, packet) ==
                    BluetoothGatt.GATT_SUCCESS
            } else {
                @Suppress("DEPRECATION")
                run {
                    ch.value = packet
                    server.notifyCharacteristicChanged(device, ch, false)
                }
            }
            dbgCounter(if (queued) "ble_notify_queued" else "ble_notify_rejected")
            queued
        } catch (e: SecurityException) {
            dbgCounter("ble_notify_security_denied")
            Log.w(TAG, "notify 被拒（缺 BLUETOOTH_CONNECT？）", e)
            false
        }
    }

    /**
     * 回报计数到调试控制面。交付版 `.so` 不带 agent-debug、`nativeDebugCounter` 符号不存在，
     * 故先查 [LinkxRuntime.debugdAvailable] 再调，且整体 `runCatching`——
     * 调试仪表绝不能变成产品的崩溃点。
     */
    private fun dbgCounter(name: String, delta: Long = 1L) {
        if (!LinkxRuntime.debugdAvailable) return
        runCatching { NativeCore.nativeDebugCounter(name, delta) }
    }

    private fun promoteToForeground() {
        // 常驻通知上带一个「发送剪贴板」动作：透明落地页 + 一次焦点，用户点一下就把正文推过去。
        val sendClip = PendingIntent.getActivity(
            this,
            REQ_CLIP_SEND,
            Intent(this, ClipboardSendActivity::class.java)
                .putExtra(ClipboardSendActivity.EXTRA_WAY, ClipboardSendActivity.WAY_NOTIF),
            PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT,
        )
        val notification = NotificationCompat.Builder(this, CHANNEL_ID)
            .setContentTitle(getString(R.string.ble_fgs_title))
            .setContentText(getString(R.string.ble_fgs_text))
            // 状态栏小图标只取 alpha 通道：品牌图（渐变启动图标）会被压成一团白，
            // 必须用单色线性图标 —— 这里用与「连接」页同一枚手机⇄电脑字形
            .setSmallIcon(R.drawable.ic_nav_link)
            .addAction(R.drawable.ic_nav_clipboard, getString(R.string.clip_send_action), sendClip)
            .setOngoing(true)
            .setPriority(NotificationCompat.PRIORITY_LOW)
            .build()
        // API < 29 忽略 type；API 34 起 type 必须与 manifest 声明一致，否则抛异常。
        val type = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) {
            ServiceInfo.FOREGROUND_SERVICE_TYPE_CONNECTED_DEVICE
        } else {
            0
        }
        ServiceCompat.startForeground(this, NOTIF_ID, notification, type)
    }

    private fun ensureChannel() {
        if (Build.VERSION.SDK_INT < Build.VERSION_CODES.O) return
        val nm = getSystemService(NotificationManager::class.java) ?: return
        nm.createNotificationChannel(
            NotificationChannel(
                CHANNEL_ID,
                getString(R.string.ble_fgs_channel_name),
                NotificationManager.IMPORTANCE_LOW,
            ),
        )
    }

    /** 拆掉广播与 GATT server（蓝牙关掉、服务销毁、重注册前都走这里，只留一份实现）。 */
    private fun teardownGatt() {
        try {
            advertiser?.stopAdvertising(advertiseCallback)
        } catch (e: SecurityException) {
            Log.w(TAG, "stopAdvertising 被拒", e)
        }
        advertiser = null
        try {
            gattServer?.close()
        } catch (e: SecurityException) {
            Log.w(TAG, "close gattServer 被拒", e)
        }
        gattServer = null
    }

    override fun onDestroy() {
        btReceiver?.let { runCatching { applicationContext.unregisterReceiver(it) } }
        btReceiver = null
        teardownGatt()
        LinkxRuntime.setPacketSink(null)
        super.onDestroy()
    }

    override fun onBind(intent: Intent?): IBinder? = null

    companion object {
        private const val TAG = "LinkX.BlePeripheral"
        private const val CHANNEL_ID = "linkx.ble.peripheral"
        private const val NOTIF_ID = 0x4C5801
        private const val REQ_CLIP_SEND = 0x4C58
        private const val PENDING_MAX = 512
        /** notify 被链路拒绝时的重试间隔（毫秒）。 */
        private const val NOTIFY_RETRY_MS = 50L
        private val CCCD_UUID: UUID = UUID.fromString("00002902-0000-1000-8000-00805f9b34fb")

        /**
         * 进程只要活着就该在广播：手机是 BLE Peripheral，不广播电脑就拨不回来，
         * 于是"系统把 App 救活了、电脑却连不上"。三处都要用到 —— 打开 App、开机广播、
         * 以及系统重新绑定通知监听（那正是 ROM 自己把进程拉起来的时刻）。
         */
        fun ensure(ctx: Context) {
            runCatching {
                ContextCompat.startForegroundService(ctx, Intent(ctx, BlePeripheralService::class.java))
            }.onFailure {
                // Android 12+ 后台起前台服务受限；起不来就说一句，别静默
                Log.i(TAG, "拉起链路服务未成功：${it.message}")
            }
        }
    }
}