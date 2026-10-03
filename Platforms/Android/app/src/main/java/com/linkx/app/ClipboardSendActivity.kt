package com.linkx.app

import android.app.Activity
import android.os.Bundle
import android.os.Handler
import android.os.Looper
import android.util.Log
import android.widget.Toast

/**
 * 常驻通知「发送剪贴板」动作的落地页。
 *
 * Android 10+ 只允许「当前持输入焦点」的应用读剪贴板，从下拉栏进来时 LinkX 不在前台，
 * 所以这里借一个什么都不画的透明 Activity 去拿一次焦点：拿到就读、读完就退。
 * 走的是与剪贴板页「立即同步本机剪贴板」完全相同的代码路径，只是入口不同 ——
 * 用户主动点这一下即可，不需要 adb、不需要无障碍、也不需要常驻焦点。
 *
 * 页面不可见由清单里的 `Theme.LinkX.Transparent` 负责。**不要**调 `Activity.setVisible`：
 * 它是 API 30 才有的方法，而本产品 minSdk = 26，Android 8/9/10 上点这个动作会直接
 * `NoSuchMethodError` 崩掉（lintVital 不报 NewApi，交付包照出，只有真机才会暴露）。
 */
class ClipboardSendActivity : Activity() {

    private val handler = Handler(Looper.getMainLooper())
    private var done = false
    private val settle = Runnable { send(WAY_FALLBACK) }

    override fun onCreate(saved: Bundle?) {
        super.onCreate(saved)
        Log.i(TAG, "opened way=${intent?.getStringExtra(EXTRA_WAY)}")
        // 个别 ROM 上焦点回调可能不来，给它一个兜底窗口，否则这个透明页会挂着不散。
        handler.postDelayed(settle, FOCUS_TIMEOUT_MS)
    }

    override fun onWindowFocusChanged(hasFocus: Boolean) {
        super.onWindowFocusChanged(hasFocus)
        if (hasFocus) send(intent?.getStringExtra(EXTRA_WAY) ?: WAY_FALLBACK)
    }

    private fun send(way: String) {
        if (done) return
        done = true
        handler.removeCallbacks(settle)
        val outcome = ClipboardSync.sendOnce(if (way == WAY_NOTIF) "通知动作" else "焦点兜底")
        Log.i(TAG, "result=$outcome way=$way")
        Toast.makeText(this, message(outcome), Toast.LENGTH_SHORT).show()
        finishAndRemoveTask()
    }

    private fun message(r: ClipboardSync.Result): Int = when (r) {
        ClipboardSync.Result.Sent -> R.string.clip_send_sent
        ClipboardSync.Result.Already -> R.string.clip_send_already
        ClipboardSync.Result.Empty -> R.string.clip_send_empty
        ClipboardSync.Result.NotPaired -> R.string.clip_send_not_paired
        ClipboardSync.Result.Disabled -> R.string.clip_send_disabled
        ClipboardSync.Result.Failed -> R.string.clip_send_failed
    }

    companion object {
        private const val TAG = "LinkX.ClipSend"
        const val EXTRA_WAY = "way"
        /** 没标明来源时按"焦点没来、由定时器兜底"记（QS 磁贴那条路在 HyperOS 上判死，APK 里没有） */
        const val WAY_FALLBACK = "fallback"
        const val WAY_NOTIF = "notif"
        private const val FOCUS_TIMEOUT_MS = 1_200L
    }
}
