package com.linkx.app

import android.content.ClipData
import android.content.ClipboardManager
import android.content.Context
import android.util.Log

/**
 * 剪贴板同步（仅纯文本）。
 * - 本机复制（开关开启且已配对）→ 经 Core 推给对端；用 lastSent 防重复回传。
 * - 收到对端 Clipboard 事件 → 写入本机剪贴板；用 lastApplied 避免再次触发监听回传（防回声）。
 * 开关状态持久化在 SharedPreferences（文件名 "linkx"）。
 *
 * **Android 10+ 边界**：系统只允许「当前获得输入焦点的应用」读取剪贴板，
 * 后台监听 `onPrimaryClipChanged` 在切到别的应用后**不会再触发**（AOSP 隐私变更，无法绕过）。
 * 故本模块同时提供**切回前台补偿同步**（`syncNow`）与**手动「立即同步」**入口，
 * 保证「在别的应用里复制 → 切回 LinkX」即完成同步。
 */
object ClipboardSync {
    private const val TAG = "LinkX.Clipboard"
    private const val PREFS = "linkx"
    private const val KEY_ENABLED = "clipboard_sync_enabled"

    private var appContext: Context? = null
    private var clipboard: ClipboardManager? = null
    private var prefs: android.content.SharedPreferences? = null
    private var registered = false
    private var lastSent: String? = null
    private var lastApplied: String? = null

    @Volatile
    var enabled: Boolean = true
        private set

    private val listener = ClipboardManager.OnPrimaryClipChangedListener { onPrimaryClipChanged() }

    fun init(context: Context) {
        val app = context.applicationContext
        appContext = app
        clipboard = app.getSystemService(ClipboardManager::class.java)
        prefs = app.getSharedPreferences(PREFS, Context.MODE_PRIVATE)
        enabled = prefs?.getBoolean(KEY_ENABLED, true) ?: true
    }

    fun setEnabled(value: Boolean) {
        enabled = value
        prefs?.edit()?.putBoolean(KEY_ENABLED, value)?.apply()
    }

    /** 监听本机剪贴板变化（幂等）。 */
    fun register() {
        if (registered) return
        try {
            clipboard?.addPrimaryClipChangedListener(listener)
            registered = true
        } catch (e: SecurityException) {
            Log.w(TAG, "注册剪贴板监听被拒", e)
        }
    }

    fun unregister() {
        if (!registered) return
        try {
            clipboard?.removePrimaryClipChangedListener(listener)
        } catch (e: SecurityException) {
            Log.w(TAG, "移除剪贴板监听被拒", e)
        }
        registered = false
    }

    private fun onPrimaryClipChanged() {
        syncNow("前台监听")
    }

    /**
     * 读取本机剪贴板，若有变化则推给对端（幂等）。
     *
     * 供四处调用：① 前台剪贴板变更监听；② 切回前台/重新获得焦点时补偿同步；
     * ③ 剪贴板页「立即同步」按钮；④ 常驻通知的「发送剪贴板」动作（见 [ClipboardSendActivity]）。
     */
    fun syncNow(reason: String = "手动") {
        sendOnce(reason)
    }

    /** 用户主动发起的一次发送，返回能直接显示给用户的结论。 */
    fun sendOnce(reason: String = "主动发送"): Result {
        if (!enabled) return Result.Disabled
        // 功能开关的隐私边界必须在这里也成立：常驻通知上的「发送剪贴板」动作是无条件挂上去的
        // （前台服务通知建一次就不重建），所以"模块已关闭"这一路只能在这里拦。
        // 少了这一句，用户关掉剪贴板模块并重启后，点通知动作照样能把正文推给对端。
        if (!Features.enabled(Module.Clipboard)) return Result.Disabled
        if (!LinkxRuntime.isPaired()) return Result.NotPaired
        val text = currentClipText()
        if (text.isNullOrEmpty()) return Result.Empty // 读不到与真空在这里同形：Android 只让持焦点的应用读
        if (text == lastApplied || text == lastSent) return Result.Already // 防回声/去重
        val ok = runCatching { LinkxRuntime.sendClipboard(text) }.getOrDefault(false)
        if (!ok) return Result.Failed
        lastSent = text
        Log.i(TAG, "已同步剪贴板（$reason，${text.length} 字）")
        return Result.Sent
    }

    /** 主动发送的结论。名字就是用户能看到的差别，不要把系统内部端出来。 */
    enum class Result { Sent, Already, Empty, NotPaired, Disabled, Failed }

    /** 会话建立/重连复原：清去重基准并立即补偿同步最新一条（重连只同步最新一条，不回放历史）。 */
    fun onSessionReady() {
        lastSent = null
        syncNow("会话就绪补偿")
    }

    /** 应用对端推来的剪贴板文本（由 LinkxRuntime.clipboardApplier 在主线程调用）。 */
    fun applyRemote(text: String) {
        if (text.isEmpty()) return
        lastApplied = text
        // 只记长度：手机剪贴板被对端改写这件事，事后要靠它解释"为什么手动发送说已经发过了"。
        Log.i(TAG, "已写入本机剪贴板（来自对端，${text.length} 字）")
        try {
            clipboard?.setPrimaryClip(ClipData.newPlainText("LinkX", text))
        } catch (e: SecurityException) {
            Log.w(TAG, "写入剪贴板被拒", e)
        } catch (e: RuntimeException) {
            Log.w(TAG, "写入剪贴板失败", e)
        }
    }

    /**
     * 当前剪贴板文本，仅供调试控制面 `/state` 的 `host.clip` 观测，产品路径不经过这里。
     * 单独导出的原因：`POST /action/send-clip` 之后，「手机确实写进了剪贴板」和
     * 「协议跑通了但 UI 层没落」在日志里长得一样，只有读出真值才算验证。
     * Android 只在应用持焦点时允许读，所以这里可能为 null——那本身就是有效的观测，不是错误。
     */
    fun peekForDebug(): String? = currentClipText()

    /**
     * 把一段文本写入**本机**剪贴板，且**不回传对端**（通知条目的「复制」按钮用）。
     *
     * 与 [applyRemote] 同口径：登记为「已应用内容」，随后的 `onPrimaryClipChanged`
     * 会因命中去重基准而跳过，避免刚复制的内容又推回对端（防回声）。
     */
    fun copyLocally(text: String) {
        if (text.isEmpty()) return
        lastApplied = text
        try {
            clipboard?.setPrimaryClip(ClipData.newPlainText("LinkX", text))
        } catch (e: SecurityException) {
            Log.w(TAG, "写入剪贴板被拒", e)
        } catch (e: RuntimeException) {
            Log.w(TAG, "写入剪贴板失败", e)
        }
    }

    private fun currentClipText(): String? {
        val cm = clipboard ?: return null
        val ctx = appContext
        return runCatching {
            if (!cm.hasPrimaryClip()) return null
            val item = cm.primaryClip?.getItemAt(0) ?: return null
            if (ctx != null) item.coerceToText(ctx)?.toString() else item.text?.toString()
        }.getOrNull()
    }
}