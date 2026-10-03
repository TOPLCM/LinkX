package com.linkx.app

import android.app.RemoteInput
import android.service.notification.StatusBarNotification
import android.util.Log

/**
 * 一条通知的**可操作面**：我们能不能关掉它、能不能替用户回它。
 *
 * `canReply` 只在**应用自己挂了 RemoteInput** 时为真。那是应用的选择，不是我们的能力 ——
 * 界面上"没有回复按钮"必须是真的不支持，不能是我们还没做，否则宣传与验收都会踩空。
 * 回复定位靠 `pkg + id + tag` 三元组：手机侧不缓存通知对象，收到回复请求时在"仍在通知栏"
 * 的条目里现找 —— 缓存下来的 PendingIntent 在通知被划掉后就是废的，拿它回复只会静默失败。
 */
data class NotifCapability(
    val pkg: String,
    val id: Int,
    val tag: String?,
    val clearable: Boolean,
    /** 挂了 RemoteInput 的是第几个 action；-1 = 没有可回复的 action */
    val replyActionIndex: Int,
    /** 要填的 `RemoteInput.resultKey`；空串 = 没有 */
    val replyResultKey: String,
) {
    val canReply: Boolean get() = replyActionIndex >= 0 && replyResultKey.isNotEmpty()
}

object NotifCapabilityProbe {
    private const val TAG = "LinkX.NLS"

    /** 这条通知第一个"能用"的回复入口：`(action 下标, resultKey)`，没有则 `(-1, "")`。 */
    fun replyHandle(sbn: StatusBarNotification): Pair<Int, String> {
        val actions = sbn.notification?.actions.orEmpty()
        for ((i, action) in actions.withIndex()) {
            val key = action.remoteInputs.orEmpty()
                .mapNotNull(RemoteInput::getResultKey)
                .firstOrNull { it.isNotEmpty() }
            if (key != null) return i to key
        }
        return -1 to ""
    }

    fun of(sbn: StatusBarNotification): NotifCapability {
        val (index, key) = replyHandle(sbn)
        return NotifCapability(sbn.packageName, sbn.id, sbn.tag, sbn.isClearable, index, key)
    }

    /**
     * 把在栏每条通知的可操作面打进日志，返回一行汇总。
     *
     * 只在人工测量时调用：`activeNotifications` 是跨进程 Binder 调用，不许进任何周期路径。
     * 正文与标题一概不落日志 —— 通知内容常含姓名、金额、验证码。
     */
    fun summarize(): String {
        val active = NlsService.activeSnapshot() ?: return "监听未绑定，读不到在栏通知"
        if (active.isEmpty()) return "在栏 0 条"
        val caps = active.map { of(it) }
        caps.forEach {
            Log.i(
                TAG,
                "cap pkg=${it.pkg} id=${it.id} tag=${it.tag ?: "-"} clearable=${it.clearable} " +
                        "reply=${it.canReply} action=${it.replyActionIndex} key=${it.replyResultKey}",
            )
        }
        return "在栏 ${caps.size} 条｜可回复 ${caps.count { it.canReply }} 条｜可关闭 ${caps.count { it.clearable }} 条（明细看 logcat）"
    }
}
