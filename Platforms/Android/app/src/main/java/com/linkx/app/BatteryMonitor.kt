package com.linkx.app

import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.content.IntentFilter
import android.os.BatteryManager
import android.os.Build
import android.util.Log

/**
 * 手机电量与充电态 → 缓存，供 [LinkxRuntime] 在事件泵里"变了才上报"。
 *
 * 只注册一次 `ACTION_BATTERY_CHANGED`（系统粘性广播，无需任何权限），之后不再轮询、
 * 不再起线程：读电量的 Binder 调用发生在系统投递广播时，我们的热路径只读两个 volatile。
 */
object BatteryMonitor {

    private const val TAG = "LinkX.Battery"

    /** 0-100；-1 = 还没收到过可用读数（此时不上报，避免把"不知道"显示成 0%） */
    @Volatile
    var level = -1
        private set

    @Volatile
    var charging = false
        private set

    /** 为什么没有读数 / 为什么报不出去。空串 = 正常。连接页与日志都读它。 */
    @Volatile
    var note = ""
        private set

    private var registered = false

    /** 上报侧遇到问题时写这里；空串 = 正常。连接页与日志共用这一个口径。 */
    fun markNote(text: String) {
        note = text
    }

    private val receiver = object : BroadcastReceiver() {
        override fun onReceive(context: Context, intent: Intent) {
            val raw = intent.getIntExtra(BatteryManager.EXTRA_LEVEL, -1)
            val scale = intent.getIntExtra(BatteryManager.EXTRA_SCALE, -1)
            if (raw < 0 || scale <= 0 || raw > scale) {
                // 广播到了但读数不可用，与"还没注册"是两件事，必须分开记，否则这条路径只会
                // 表现为"电脑上没有电量"。scale=0 尤其危险：不挡就会凭空算出一个满电。
                level = -1
                note = "系统电量广播没给可用读数（level=$raw scale=$scale），电脑侧不会显示电量"
                Log.w(TAG, note)
                return
            }
            level = (raw * 100 / scale).coerceIn(0, 100)
            // PLUGGED 是位掩码（AC/USB/无线），非 0 即"接着电"
            charging = intent.getIntExtra(BatteryManager.EXTRA_PLUGGED, 0) != 0
            note = ""
        }
    }

    /** 注册即拿到当前值（粘性广播），所以启动后第一轮泵就能报出电量。 */
    fun start(ctx: Context) {
        if (registered) return
        registered = true
        val app = ctx.applicationContext
        val filter = IntentFilter(Intent.ACTION_BATTERY_CHANGED)
        // 注册失败必须出声，否则症状会被误读成"蓝牙没连上"
        runCatching {
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
                // 只收系统广播，不接受本机其它 App 伪造 → NOT_EXPORTED
                app.registerReceiver(receiver, filter, Context.RECEIVER_NOT_EXPORTED)
            } else {
                app.registerReceiver(receiver, filter)
            }
        }.onFailure {
            registered = false
            note = "电量广播注册失败：${it.message}"
            Log.w(TAG, note)
        }
    }
}
