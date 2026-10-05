package com.linkx.app

import android.app.Notification
import android.app.PendingIntent
import android.app.RemoteInput
import android.content.ComponentName
import android.content.Context
import android.content.Intent
import android.os.Bundle
import android.provider.Settings
import android.service.notification.NotificationListenerService
import android.service.notification.StatusBarNotification
import android.util.Log
import java.util.concurrent.Executors
import java.util.concurrent.atomic.AtomicInteger

/**
 * 通知监听：过滤后组装 package/title/text/ts 交给 LinkxRuntime 转发（经 Core 加密成 NOTIFY_PUSH）。
 * 敏感来源（验证码等）默认不传正文，对端只显示"有新通知"。
 */
class NlsService : NotificationListenerService() {

    // 绑定/解绑必须留痕：授权项还在但服务没被绑上，是"通知同步失效"最常见的形态，
    // 而它在设置界面上完全看不出来。
    override fun onListenerConnected() {
        super.onListenerConnected()
        Features.init(applicationContext) // 本组件可能由系统直接拉起，谁先到谁固化开关（幂等）
        current = this
        LinkxRuntime.markNls(true)
        Log.i(TAG, "NLS 已绑定")
        BlePeripheralService.ensure(applicationContext) // 不广播，电脑就拨不回来，通知抓到也没处送
        if (LinkxRuntime.isPaired()) resyncActive() // 链路先起、监听后绑时，"配对完成"那个钩子已过去
    }

    override fun onListenerDisconnected() {
        super.onListenerDisconnected()
        clearCurrent()
        Log.i(TAG, "NLS 已解绑")
        // 不主动请求重绑，进程被回收后授权项仍在、`onNotificationPosted` 却永远不来（实测 HyperOS）
        requestRebindIfAuthorized(applicationContext)
    }

    override fun onDestroy() {
        clearCurrent()
        super.onDestroy()
    }

    /** 系统重绑时旧实例的解绑/销毁回调可能后到，无条件清会把新实例抹掉。 */
    private fun clearCurrent() {
        if (current === this) current = null
        if (current == null) LinkxRuntime.markNls(false)
    }

    override fun onNotificationPosted(sbn: StatusBarNotification) {
        forward(sbn)
    }

    /** 过滤 + 组装 + 转发；`onNotificationPosted` 与重连补发共用这一份口径。 */
    private fun forward(sbn: StatusBarNotification) {
        // 关掉通知同步只关行为，**不把组件 disable 掉**：禁用会让系统收回已授予的通知使用权，
        // 用户下次开启要重新去系统设置里授权，代价远大于省下的那点内存。
        if (!Features.enabled(Module.Notifications)) return
        val pkg = sbn.packageName
        if (pkg == SELF_PACKAGE) return // 防自回声
        if (sbn.isOngoing) return // 通话/音乐等常驻通知不是"新事件"
        // 只转新鲜窗口内的：绑定瞬间系统会把整栏补投一遍（实测一次 35 条），全转会打爆蓝牙
        // "单片在途"的发送队列 —— 超限丢的是最旧的，真正该到的那条反而到不了电脑。
        val ageMs = System.currentTimeMillis() - sbn.postTime
        if (ageMs > FRESH_WINDOW_MS) {
            Log.i(TAG, "skip stale: $pkg age=${ageMs / 1000}s")
            return
        }

        val extras = sbn.notification?.extras
        val title = extras?.getCharSequence(Notification.EXTRA_TITLE)?.toString().orEmpty()
        var text = extras?.getCharSequence(Notification.EXTRA_TEXT)?.toString().orEmpty()
        if (title.isEmpty() && text.isEmpty()) return
        // "android.isSensitive" 在 SDK 里没有公开常量，只能写字面量
        val sensitive = extras?.getBoolean(EXTRA_IS_SENSITIVE, false) ?: false
        if (sensitive) text = ""

        // 标题与正文不进 logcat：里面常是姓名/金额/验证码，而日志本机任何采集面都读得到
        Log.i(TAG, "posted: $pkg titleLen=${title.length} textLen=${text.length} sensitive=$sensitive")
        val keyHash = runCatching { sbn.key?.hashCode() ?: 0 }.getOrDefault(0) // 对端据此就地合并
        val cap = NotifCapabilityProbe.of(sbn)
        // 补投与补发会撞在同一条上：对端列表按 key 合并看不出来，但电脑的系统弹窗会响两次。内容变了才算
        // 新事件（同 key 追发必须放行）。可回复状态也要算进去：应用常在下一次推送里才挂上 RemoteInput。
        val fingerprint = "$keyHash|$title|$text|${cap.canReply}"
        if (alreadyForwarded(fingerprint)) return
        // 发不出去（未配对/引擎未就绪）不重试，但必须出声 —— 否则"抓到了"和"扔了"在日志里长得一样
        val sent = runCatching {
            LinkxRuntime.sendNotification(
                pkg, title, text, sbn.postTime, keyHash,
                tag = cap.tag.orEmpty(),
                notificationId = cap.id,
                canReply = cap.canReply,
                replyActionIndex = cap.replyActionIndex,
                replyResultKey = cap.replyResultKey,
            )
        }.getOrDefault(false)
        if (!sent) {
            Log.i(TAG, "dropped: $pkg（等重连补发）titleLen=${title.length}")
            return
        }
        markForwarded(fingerprint)
        // 只有"电脑上有回复入口"的那几条需要后续报消失；其余条目留在电脑列表里点不出错
        if (cap.canReply) markReplyable(cap.pkg, cap.tag, cap.id, keyHash)
        else clearReplyable(cap.pkg, cap.tag, cap.id)
    }

    override fun onNotificationRemoved(sbn: StatusBarNotification) {
        Log.i(TAG, "removed: ${sbn.packageName}")
        val keyHash = takeReplyable(sbn.packageName, sbn.tag, sbn.id) ?: return
        // 应用自己把通知清掉是常态（回完短信就撤通知）；不报这一声，电脑上的入口会一直留到用户点出失败
        val sent = runCatching {
            LinkxRuntime.sendNotifyDismiss(sbn.packageName, sbn.tag.orEmpty(), sbn.id, keyHash)
        }.getOrDefault(false)
        Log.i(TAG, "dismiss pkg=${sbn.packageName} id=${sbn.id} tag=${sbn.tag ?: "-"} queued=$sent")
        // 同一条通知下次再挂上时指纹可能与这次完全一样，不清掉就永久补投不上去
        dropForwarded(keyHash)
    }

    companion object {
        private const val TAG = "LinkX.NLS"
        private const val SELF_PACKAGE = "com.linkx.app"

        private const val EXTRA_IS_SENSITIVE = "android.isSensitive"
        private const val FRESH_WINDOW_MS = 10 * 60 * 1000L
        private const val RECENT_MAX = 64 // 只挡"同一批被投了两次"，不挡几小时后的同一条更新

        private val forwarded = ArrayDeque<String>()

        private fun alreadyForwarded(fingerprint: String): Boolean =
            synchronized(forwarded) { fingerprint in forwarded }

        /** 发成功才登记：在发送前登记会把断链期间那条永久误吞，重连也补不回来。 */
        private fun markForwarded(fingerprint: String) {
            synchronized(forwarded) {
                forwarded.addLast(fingerprint)
                if (forwarded.size > RECENT_MAX) forwarded.removeFirst()
            }
        }

        /** 通知消失后放开这条的转发指纹：同一条重新挂上时内容可以一字不差。 */
        private fun dropForwarded(keyHash: Int) {
            val prefix = "$keyHash|"
            synchronized(forwarded) { forwarded.removeAll { it.startsWith(prefix) } }
        }

        private const val REPLYABLE_MAX = 32

        /** `pkg|tag|id` → 当初上报的 keyHash。挤掉最旧一条的代价只是"少报一次消失"。 */
        private val replyable = LinkedHashMap<String, Int>()

        private fun handleOf(pkg: String, tag: String?, id: Int) = "$pkg|${tag.orEmpty()}|$id"

        private fun markReplyable(pkg: String, tag: String?, id: Int, keyHash: Int) {
            synchronized(replyable) {
                replyable[handleOf(pkg, tag, id)] = keyHash
                while (replyable.size > REPLYABLE_MAX) replyable.remove(replyable.keys.first())
            }
        }

        private fun clearReplyable(pkg: String, tag: String?, id: Int) {
            synchronized(replyable) { replyable.remove(handleOf(pkg, tag, id)) }
        }

        /** 取出并摘掉：没登记过的（当初就不可回复、或压根没转发过）返回 null。 */
        private fun takeReplyable(pkg: String, tag: String?, id: Int): Int? =
            synchronized(replyable) { replyable.remove(handleOf(pkg, tag, id)) }

        @Volatile
        private var current: NlsService? = null

        /** 链路恢复后把「仍在通知栏里的」条目再过一遍 [forward]：未配对期间到达的通知原本是丢弃的，手机被杀重开那一段就永远到不了电脑；
         * 补发安全是因为对端按 key 合并，重复不会变成刷屏。 */
        fun resyncActive() {
            val svc = current ?: return
            if (!Features.enabled(Module.Notifications)) return
            val active = runCatching { svc.activeNotifications.toList() }
                .onFailure { Log.w(TAG, "读取在栏通知失败：${it.message}") }
                .getOrDefault(emptyList())
            if (active.isEmpty()) return
            Log.i(TAG, "链路恢复，把在栏 ${active.size} 条过一遍新鲜窗口")
            active.forEach { svc.forward(it) }
        }

        /** 在栏通知快照（能力普查用）；监听没绑定时返回 null，由调用方决定怎么说。 */
        fun activeSnapshot(): List<StatusBarNotification>? =
            current?.let { runCatching { it.activeNotifications.toList() }.getOrNull() }

        // ---------- 电脑 → 手机：回复一条通知（RemoteInput，Android 官方机制） ----------

        /** 排队上限：电脑侧连点时宁可明确回一句"太忙"，也不能把队列堆到无界（同媒体指令口径）。 */
        private const val MAX_PENDING_REPLIES = 8

        private val pendingReplies = AtomicInteger(0)

        /** 回复专用单线程：`activeNotifications` 是跨进程 Binder，绝不许在事件泵锁里跑。 */
        private val replyPool by lazy {
            Executors.newSingleThreadExecutor { r -> Thread(r, "linkx-reply").apply { isDaemon = true } }
        }

        /**
         * 收到一条回复请求：这里**只判前置条件并排队**，执行与回执都在 `linkx-reply` 线程上。
         * 无论成败都必须回一条回执 —— 电脑在等，沉默只会让它停在"点了没反应"。
         */
        fun submitReply(
            replyId: Int,
            pkg: String,
            tag: String,
            id: Int,
            actionIndex: Int,
            resultKey: String,
            text: String,
        ) {
            val svc = current ?: return ack(replyId, pkg, "手机侧通知监听未连接")
            if (!Features.enabled(Module.Notifications)) return ack(replyId, pkg, "通知同步已关闭")
            if (pkg == SELF_PACKAGE) return ack(replyId, pkg, "不能回复 LinkX 自己的通知")
            if (text.isBlank()) return ack(replyId, pkg, "回复内容为空")
            if (pendingReplies.get() >= MAX_PENDING_REPLIES) return ack(replyId, pkg, "回复太频繁，请稍候再试")
            pendingReplies.incrementAndGet()
            replyPool.execute {
                // `doReply` 的 null = 成功，`getOrNull()` 会把成功与异常塌成同一个 null，真发出去的回复会被报成"回复时出错"，故两路必须 fold 分开。
                val failure = runCatching {
                    doReply(svc, pkg, tag, id, actionIndex, resultKey, text)
                }.fold(onSuccess = { it }, onFailure = { "回复时出错：${it.javaClass.simpleName}" })
                try {
                    ack(replyId, pkg, failure)
                } finally {
                    pendingReplies.decrementAndGet()
                }
            }
        }

        /** 成功回一句"没有理由"（`reason = null`），失败回一句要如实显示给用户的话。 */
        private fun ack(replyId: Int, pkg: String, reason: String?) {
            val ok = reason == null
            val sent = runCatching {
                LinkxRuntime.sendNotifyReplyAck(replyId, pkg, ok, reason.orEmpty())
            }.getOrDefault(false)
            Log.i(TAG, "reply.ack id=$replyId pkg=$pkg ok=$ok queued=$sent reason=${reason ?: "-"}")
        }

        /** 返回 null = 已把文字交给应用；非 null = 失败原因。 */
        private fun doReply(
            svc: NlsService,
            pkg: String,
            tag: String,
            id: Int,
            actionIndex: Int,
            resultKey: String,
            text: String,
        ): String? {
            // 现找，不用缓存：通知被划掉后缓存里的 PendingIntent 就是废的，拿它回复只会静默失败
            val sbn = runCatching { svc.activeNotifications.toList() }
                .getOrNull()
                ?.firstOrNull { it.packageName == pkg && it.id == id && it.tag.orEmpty() == tag }
                ?: return "这条通知已经不在了"
            val action = sbn.notification?.actions?.getOrNull(actionIndex)
                ?: return "这条通知没有可用的回复按钮"
            val reply = action.actionIntent ?: return "该应用不支持回复"
            // 只填电脑点名的那个 key；同一 action 上的其它输入框留给应用自己处理
            val inputs = action.remoteInputs.orEmpty().filter { !it.resultKey.isNullOrEmpty() }
            if (inputs.none { it.resultKey == resultKey }) return "该应用不支持回复"
            val results = Bundle().apply { putCharSequence(resultKey, text) }
            val intent = Intent()
            RemoteInput.addResultsToIntent(inputs.toTypedArray(), intent, results)
            return runCatching { reply.send(svc, 0, intent) }.fold(
                onSuccess = {
                    Log.i(TAG, "reply pkg=$pkg id=$id action=$actionIndex key=$resultKey len=${text.length}")
                    null
                },
                // 应用自己撤销了 PendingIntent（通知已被处理掉）与"这台机器不让发"是两回事，得分开说
                onFailure = { e ->
                    if (e is PendingIntent.CanceledException) "回复已失效（应用撤掉了这条通知）"
                    else "回复失败：${e.javaClass.simpleName}"
                },
            )
        }

        /** 本应用的监听器组件名，仅当系统确实登记了它才返回非空。读 `Settings.Secure` 而不用
         * `getEnabledListenerPackages`（后者 API 30 起才有，本应用 minSdk 26）。 */
        fun authorizedComponent(ctx: Context): ComponentName? {
            val flat = runCatching {
                Settings.Secure.getString(ctx.contentResolver, "enabled_notification_listeners")
            }.getOrNull() ?: return null
            val mine = flat.split(":")
                .firstOrNull { it.startsWith("${ctx.packageName}/") && it.endsWith(".NlsService") }
                ?: return null
            return ComponentName.unflattenFromString(mine)
        }

        /**
         * 授权还在、服务却没被绑上时的恢复动作。未授权时不调用（那会绕过用户的意图）；
         * 已在绑定中调用是无害的。**这台 ROM 会收下请求但不绑定**，所以它不是交付保证。
         */
        fun requestRebindIfAuthorized(ctx: Context) {
            val name = authorizedComponent(ctx)
            if (name == null) {
                Log.i(TAG, "未授予通知监听权限，不请求重新绑定")
                return
            }
            runCatching { NotificationListenerService.requestRebind(name) }
                .onSuccess { Log.i(TAG, "已请求系统重新绑定监听：$name") }
                .onFailure { Log.w(TAG, "请求重新绑定监听失败：${it.message}") }
        }
    }
}
