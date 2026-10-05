package com.linkx.app

import android.bluetooth.BluetoothAdapter
import android.bluetooth.BluetoothManager
import android.content.Context
import android.content.Intent
import android.net.ConnectivityManager
import android.net.NetworkCapabilities
import android.provider.Settings

/**
 * 连接前提体检：蓝牙开没开、有没有连在局域网里。
 *
 * 这两件事 App 控制不了（Android 12+ 也不允许应用自己开蓝牙），但**用户一步就能做**。
 * 缺了又不说，界面就只剩"扫不到设备"这种沉默现象，用户会以为功能坏了——
 * 本项目真机上就被这种误读带偏过。所以缺什么讲什么，并给一个跳过去的入口。
 *
 * 只在重组时读一次（跟着 1 Hz 的 tick 走），不起线程也不注册监听：
 * 这两个状态是"用户改了就变"的慢变量，为它常驻不值得。
 */
object LinkPrereqs {

    /** 一次体检的结果。`noBluetooth` 与 `bluetoothOn=false` 是两件事，别合并。 */
    data class Status(
        val bluetoothOn: Boolean,
        val noBluetooth: Boolean,
        val lanConnected: Boolean,
        /** 只有移动数据时 true：局域网类功能在蜂窝网络下结构上不可能工作 */
        val cellularOnly: Boolean,
    )

    fun check(ctx: Context): Status {
        val adapter = runCatching {
            ctx.getSystemService(BluetoothManager::class.java)?.adapter
        }.getOrNull()
        val noBt = adapter == null
        val btOn = !noBt && runCatching { adapter!!.isEnabled }.getOrDefault(false)

        // 整段都要防护：`getActiveNetwork` 缺 ACCESS_NETWORK_STATE 时抛的是 SecurityException，
        // 而它在 Compose 的重组里抛出 = 直接闪退（真机就这么炸过一次）。
        val caps = runCatching {
            val cm = ctx.getSystemService(ConnectivityManager::class.java) ?: return@runCatching null
            cm.activeNetwork?.let { cm.getNetworkCapabilities(it) }
        }.getOrNull()
        val wifi = caps?.hasTransport(NetworkCapabilities.TRANSPORT_WIFI) == true
        val ethernet = caps?.hasTransport(NetworkCapabilities.TRANSPORT_ETHERNET) == true
        val cellular = caps?.hasTransport(NetworkCapabilities.TRANSPORT_CELLULAR) == true
        return Status(
            bluetoothOn = btOn,
            noBluetooth = noBt,
            lanConnected = wifi || ethernet,
            cellularOnly = cellular && !wifi && !ethernet,
        )
    }

    /**
     * 跳一个系统/厂商页面（NEW_TASK 从这里统一加）：部分 ROM 裁过这些页面，
     * ActivityNotFound 一律落回 false，由调用方决定下一句说什么、下一个入口给哪个。
     */
    private fun openPage(ctx: Context, page: Intent): Boolean = runCatching {
        ctx.startActivity(page.addFlags(Intent.FLAG_ACTIVITY_NEW_TASK))
        true
    }.getOrDefault(false)

    /**
     * 跳去开蓝牙。Android 12 及以下还能弹"允许应用打开蓝牙"的系统确认框，
     * 13 起该意图被废弃（点了也没对话框），只能落到蓝牙设置页——
     * 分叉写在这里，界面层不必知道版本差异。
     */
    fun openBluetooth(ctx: Context): Boolean = openPage(
        ctx,
        if (android.os.Build.VERSION.SDK_INT < android.os.Build.VERSION_CODES.TIRAMISU) {
            Intent(BluetoothAdapter.ACTION_REQUEST_ENABLE)
        } else {
            Intent(Settings.ACTION_BLUETOOTH_SETTINGS)
        },
    )

    /** 跳去连 Wi-Fi。 */
    fun openNetwork(ctx: Context): Boolean = openPage(ctx, Intent(Settings.ACTION_WIFI_SETTINGS))

    /**
     * 各家 ROM 的「自启动管理」页组件名（没有公开标准，只能按厂商机型逐个试）。
     *
     * 我们**不检测**自启动是否已允许 —— 那要反射进厂商私有 API，版本一变就静默失效，
     * 比"不知道"更坏。这里只负责把门打开，让用户自己看一眼并允许；一家都不中就退回
     * 本应用的系统详情页。
     */
    private val autoStartPages = listOf(
        "com.miui.securitycenter" to "com.miui.permcenter.autostart.AutoStartManagementActivity",
        "com.huawei.systemmanager" to "com.huawei.systemmanager.startupmgr.ui.StartupNormalAppListActivity",
        "com.huawei.systemmanager" to "com.huawei.systemmanager.appcontrol.activity.StartupAppControlActivity",
        "com.coloros.safecenter" to "com.coloros.safecenter.permission.startup.StartupAppListActivity",
        "com.vivo.permissionmanager" to "com.vivo.permissionmanager.activity.BgStartUpManagerActivity",
        "com.iqoo.secure" to "com.iqoo.secure.ui.phoneoptimize.BgStartUpManager",
        "com.samsung.android.lool" to "com.samsung.android.sm.ui.battery.BatteryActivity",
        "com.letv.android.letvsafe" to "com.letv.android.letvsafe.AutobootManageActivity",
    )

    /** 跳去「自启动管理」；找不到厂商页面就退到本应用详情页。返回是否真的跳出去了。 */
    fun openAutoStart(ctx: Context): Boolean {
        for ((pkg, cls) in autoStartPages) {
            if (openPage(ctx, Intent().setClassName(pkg, cls))) return true
        }
        return openPage(
            ctx,
            Intent(Settings.ACTION_APPLICATION_DETAILS_SETTINGS)
                .setData(android.net.Uri.fromParts("package", ctx.packageName, null)),
        )
    }
}
