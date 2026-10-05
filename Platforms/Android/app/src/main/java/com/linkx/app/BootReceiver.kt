package com.linkx.app

import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent

/**
 * 开机 / 覆盖安装后把链路服务拉回来。
 *
 * 没有这一步，手机重启后 App 不在运行，通知监听也就没人去绑 —— 用户得手动打开一次 App
 * 才能恢复，而"打开 App"并不在他对「通知同步」的预期里。这两条广播也是 Android 12+
 * 少数允许从后台拉起前台服务的场景，用它们不额外要权限。
 */
class BootReceiver : BroadcastReceiver() {

    override fun onReceive(ctx: Context, intent: Intent) {
        // 这个 receiver 在清单里是 exported（开机广播要求如此），不校验 action 就等于
        // 任何第三方 App 发一条显式 Intent 就能反复把我们的前台服务与 BLE 广播拉起来。
        val action = intent.action
        if (action != Intent.ACTION_BOOT_COMPLETED && action != Intent.ACTION_MY_PACKAGE_REPLACED) return
        BlePeripheralService.ensure(ctx)
    }
}
