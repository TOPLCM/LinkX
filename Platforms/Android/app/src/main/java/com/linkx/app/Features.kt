package com.linkx.app

import android.content.Context
import android.content.SharedPreferences
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.setValue

/**
 * 可独立关闭的功能模块（安卓端，与 Windows `Platforms/Windows/src/features.rs` 同一套语义）。
 */
enum class Module(
    val label: String,
    val about: String,
    val consequence: String,
    val key: String,
) {
    Notifications("通知同步", "接收手机通知并在应用内列出", "通知将不再被接收", "notifications"),
    Clipboard("剪贴板同步", "两端复制的文本互相同步", "两端复制内容将不再互相同步", "clipboard"),
    FileTransfer("文件传输", "局域网收发文件", "将无法收发文件", "file_transfer"),
    MediaControl("媒体控制", "把本机播放状态报给对端并接受其控制", "对端将不再显示和控制本机播放", "media_control"),
    Album("图片互传", "应答电脑对本机相册的清单/缩略图/原图请求", "电脑将无法读取本机相册", "album"),
}

/**
 * 运行期功能开关。
 *
 * 需求：关掉的功能**下次启动不再初始化**——不起线程、不注册回调、不建页面。
 *
 * 关键设计：「想要的状态」与「本次已加载」必须分开。
 * - [wantedSnapshot] 是用户想要的，改完立刻落盘；
 * - [active] 是本次启动实际加载的，只在进程启动时由 [init] 固化一次，之后只读。
 *
 * 之所以不当场热卸载：BLE / 剪贴板 / 通知回调都在途，就地拆对象是悬垂引用风险面
 * （Windows 端曾在 BLE 回调生命周期上栽过）。用户要的语义本来就是"重启后不加载"。
 *
 * 通知监听（`NlsService`）**只关行为、不关组件**：把组件 disable 掉会让系统收回
 * 已授予的"通知使用权"，用户下次开启要重新去系统设置里授权，代价远大于省下的那点内存。
 */
object Features {
    private const val PREFS = "linkx"
    private const val KEY_PREFIX = "feature_"
    private val ALL = Module.entries

    private var prefs: SharedPreferences? = null

    /** 本次启动已加载的模块（由 [init] 固化一次，之后只读） */
    // 采样/转发跑在各自的线程上，`init` 只在主线程固化一次；不加 @Volatile，后起的线程
    // 可能一直读不到那份快照（服务与 Activity 谁先到都可能）。
    @Volatile
    private var active: Set<Module> = ALL.toSet()
    private var frozen = false

    /** 设置页据此重组：[setWanted] 之后必须让"待重启生效"提示刷新 */
    var wantedSnapshot by mutableStateOf(ALL.toSet())
        private set

    /** 幂等：进程内**第一个**用到它的组件（Activity / NLS / BLE 服务）负责固化。 */
    fun init(context: Context) {
        if (frozen) return
        frozen = true
        val p = context.applicationContext.getSharedPreferences(PREFS, Context.MODE_PRIVATE)
        prefs = p
        active = read(p)
        wantedSnapshot = active
    }

    fun enabled(m: Module): Boolean = m in active

    fun activeModules(): Set<Module> = active

    fun setWanted(context: Context, m: Module, value: Boolean) {
        // getSharedPreferences 按名字返回**进程内同一实例**：init() 取过就直接复用，不必再查一遍
        val p = prefs ?: context.applicationContext.getSharedPreferences(PREFS, Context.MODE_PRIVATE)
        // apply() 会同步更新内存里的那份，所以下一行的 read 立刻能读到刚写入的值
        p.edit().putBoolean(KEY_PREFIX + m.key, value).apply()
        wantedSnapshot = read(p)
    }

    fun isWanted(m: Module): Boolean = m in wantedSnapshot

    /** 待重启条目：（模块，用户想要的状态） */
    fun changes(): List<Pair<Module, Boolean>> =
        ALL.filter { enabled(it) != (it in wantedSnapshot) }.map { it to (it in wantedSnapshot) }

    private fun read(p: SharedPreferences): Set<Module> =
        ALL.filterTo(mutableSetOf()) { p.getBoolean(KEY_PREFIX + it.key, true) }
}
