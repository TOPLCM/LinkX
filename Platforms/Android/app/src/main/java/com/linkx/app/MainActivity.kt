/**
 * 单 Activity + 底部图标化导航（连接/通知/剪贴板/文件/媒体/设置/关于），
 * 卡片化信息层级、页面切换动效、明/暗主题跟随系统。
 *
 * 本文件由原先那个近 2000 行的 `MainActivity.kt` 拆出：Activity 与通用组件留在这里，
 * 七个页面各自一个文件，放在 `ui/` 下（`com.linkx.app.ui`）。
 * 拆的是**位置**，不是行为：文案、布局参数与调用关系一律照原样。
 */

package com.linkx.app

import android.Manifest
import android.content.Context
import android.content.Intent
import android.content.pm.PackageManager
import android.net.Uri
import android.os.Build
import android.os.Bundle
import android.util.Log
import android.widget.Toast
import androidx.activity.ComponentActivity
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.compose.setContent
import androidx.activity.result.contract.ActivityResultContracts
import androidx.annotation.DrawableRes
import androidx.compose.animation.AnimatedContent
import androidx.compose.animation.core.InfiniteRepeatableSpec
import androidx.compose.animation.core.RepeatMode
import androidx.compose.animation.core.animateFloat
import androidx.compose.animation.core.infiniteRepeatable
import androidx.compose.animation.core.rememberInfiniteTransition
import androidx.compose.animation.core.tween
import androidx.compose.animation.fadeIn
import androidx.compose.animation.fadeOut
import androidx.compose.animation.slideInHorizontally
import androidx.compose.animation.slideOutHorizontally
import androidx.compose.animation.togetherWith
import androidx.compose.foundation.background
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.ColumnScope
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.CardDefaults
import androidx.compose.material3.ElevatedCard
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.Icon
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.NavigationBar
import androidx.compose.material3.NavigationBarItem
import androidx.compose.material3.Scaffold
import androidx.compose.material3.SnackbarHost
import androidx.compose.material3.SnackbarHostState
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.mutableIntStateOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.res.painterResource
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.unit.dp
import androidx.core.content.ContextCompat
import com.linkx.app.ui.LinkXTheme
import kotlinx.coroutines.launch
import androidx.compose.runtime.getValue
import androidx.compose.runtime.setValue
import com.linkx.app.ui.AboutPage
import com.linkx.app.ui.ClipboardPage
import com.linkx.app.ui.ConnectPage
import com.linkx.app.ui.FilesPage
import com.linkx.app.ui.MediaPage
import com.linkx.app.ui.NotificationsPage
import com.linkx.app.ui.SettingsPage

/**
 * 单 Activity + 底部图标化导航（连接/通知/剪贴板/文件/媒体/设置），
 * 卡片化信息层级、页面切换动效、明/暗主题跟随系统。
 */
class MainActivity : ComponentActivity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        // 顺序有讲究：功能开关必须**早于**任何模块初始化（详见 LinkxRuntime.boot）
        LinkxRuntime.boot(applicationContext)
        if (Features.enabled(Module.Clipboard)) ClipboardSync.register()
        // 进程被回收后再起来，系统不会自动把通知监听绑回来（授权项仍在但一条通知都收不到）。
        // 不按功能开关分叉：组件本来就保持启用（禁用会令系统收回授权），开关只关行为。
        NlsService.requestRebindIfAuthorized(applicationContext)
        collectShared(intent)
        setContent { LinkXTheme { LinkxApp(startPage = if (sharedInbox.isEmpty()) PAGE_CONNECT else PAGE_FILES) } }
    }

    /** 从其它 App「分享 → LinkX」带进来的文件（一次性队列，互传页取走即清空）。 */
    private fun collectShared(incoming: Intent?) {
        val uris = when (incoming?.action) {
            Intent.ACTION_SEND -> listOfNotNull(
                if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
                    incoming.getParcelableExtra(Intent.EXTRA_STREAM, Uri::class.java)
                } else {
                    @Suppress("DEPRECATION") incoming.getParcelableExtra(Intent.EXTRA_STREAM)
                },
            )
            Intent.ACTION_SEND_MULTIPLE ->
                if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
                    incoming.getParcelableArrayListExtra(Intent.EXTRA_STREAM, Uri::class.java)
                } else {
                    @Suppress("DEPRECATION") incoming.getParcelableArrayListExtra<Uri>(Intent.EXTRA_STREAM)
                }.orEmpty()
            else -> return
        }
        if (uris.isEmpty()) return
        sharedInbox.addAll(uris)
        // 分享进来的 Uri 只有**临时**读授权（发送方通常不带 FLAG_GRANT_PERSISTABLE，转持久基本会失败）：
        // 失败必须说出来并显示给用户，否则它稍后变成"读不到这个文件"，看起来像用户自己的文件出了问题。
        uris.forEach { u ->
            runCatching {
                contentResolver.takePersistableUriPermission(
                    u,
                    Intent.FLAG_GRANT_READ_URI_PERMISSION,
                )
            }.onFailure {
                Log.w(TAG, "分享进来的 Uri 拿不到持久授权（${it.javaClass.simpleName}）：$u")
                shareGrantNote = "分享进来的文件只在这次界面里有效；切走再回来需要重新分享一次"
            }
        }
    }

    override fun onNewIntent(intent: Intent) {
        super.onNewIntent(intent)
        setIntent(intent)
        collectShared(intent)
    }

    /**
     * 切回前台 / 重新获得输入焦点 → 补偿同步一次剪贴板：Android 10+ 只有持输入焦点的
     * 应用能读剪贴板，在别的应用里复制时后台监听不会触发。
     */
    override fun onResume() {
        super.onResume()
        if (Features.enabled(Module.Clipboard)) ClipboardSync.syncNow("切回前台")
    }

    override fun onWindowFocusChanged(hasFocus: Boolean) {
        super.onWindowFocusChanged(hasFocus)
        if (hasFocus && Features.enabled(Module.Clipboard)) ClipboardSync.syncNow("重新获得焦点")
    }

    override fun onDestroy() {
        if (Features.enabled(Module.Clipboard)) ClipboardSync.unregister()
        super.onDestroy()
    }
}

/**
 * 页面 id（**稳定编号**）。关掉一个模块会隐藏它那一页，
 * 用列表下标当页面 id 的话，隐藏中间一页就会让所有页串位。
 */
private const val PAGE_CONNECT = 0
private const val PAGE_NOTIF = 1
private const val PAGE_CLIP = 2
private const val PAGE_FILES = 3
private const val PAGE_SETTINGS = 4
private const val PAGE_MEDIA = 5
/** 关于页：侧栏的正式一项，页面号与 Windows 侧 `TAB_ABOUT` 同一口径。 */
private const val PAGE_ABOUT = 6

/** 底部导航条目（图标与 Windows 端同一套几何）；[module] 非空 = 关掉该模块就连这一页都不建 */
private data class NavItem(
    val label: String,
    @DrawableRes val icon: Int,
    val page: Int,
    val module: Module?,
)

private val ALL_TABS = listOf(
    NavItem("连接", R.drawable.ic_nav_link, PAGE_CONNECT, null),
    NavItem("通知", R.drawable.ic_nav_bell, PAGE_NOTIF, Module.Notifications),
    NavItem("剪贴板", R.drawable.ic_nav_clipboard, PAGE_CLIP, Module.Clipboard),
    NavItem("文件", R.drawable.ic_nav_folder, PAGE_FILES, Module.FileTransfer),
    NavItem("媒体", R.drawable.ic_nav_media, PAGE_MEDIA, Module.MediaControl),
    NavItem("设置", R.drawable.ic_nav_settings, PAGE_SETTINGS, null),
    // 「关于」排在最后，图标是图标包里的「关于.svg」(ⓘ)，与其余各项同网格同笔画。
    NavItem("关于", R.drawable.ic_nav_info, PAGE_ABOUT, null),
)

/// 界面图标尺寸：全应用只有这两个值，与 Windows 侧 `render::NAV_ICON` 同一口径（大小保持一致）。
internal val NavIconSize = 28.dp
internal val ActionIconSize = 22.dp

/** 其它 App「分享到 LinkX」带进来的文件；只在主线程读写，互传页取走即清空。 */
internal val sharedInbox = mutableListOf<Uri>()

internal const val TAG = "LinkX.UI"

/** 分享授权拿不住时的提示（互传页显示一次）。空串 = 没有要提醒的。 */
internal var shareGrantNote = ""

/** 取消原因（会显示在任务行上）：写明是本机点的，对端那一侧显示的是同一句。 */
internal const val CANCEL_REASON = "手机端已取消"

@Composable
private fun LinkxApp(startPage: Int = PAGE_CONNECT) {
    val ctx = LocalContext.current
    var selected by rememberSaveable { mutableIntStateOf(startPage) }
    // 关掉模块 → 那一页**根本不建**（不是灰掉、不是空页）
    val tabs = remember { ALL_TABS.filter { it.module == null || Features.enabled(it.module) } }
    // 停在被隐藏的页上时（进程内刚改过开关）落回连接页。关于页现在就在 tabs 里，
    // 不再需要为它单开一条放行条件。
    val page = if (tabs.any { it.page == selected }) selected else PAGE_CONNECT
    // 任意事件/数据变化自增，作为 UI 重组与 remember 失效的 key
    var tick by remember { mutableIntStateOf(0) }
    var granted by remember { mutableStateOf(false) }
    val snackbar = remember { SnackbarHostState() }
    val scope = rememberCoroutineScope()
    val copiedTip = stringResource(R.string.msg_copied)

    val permissions = remember { allPermissions() }
    val launcher = rememberLauncherForActivityResult(
        ActivityResultContracts.RequestMultiplePermissions(),
    ) { granted = permissionsPresent(ctx) }

    // 首帧：缺**必需**权限则申请（附带申请可选的通知权限）；门禁只看必需权限，
    // 拒绝通知权限不阻塞核心功能。
    LaunchedEffect(Unit) {
        if (permissionsPresent(ctx)) granted = true else launcher.launch(permissions)
    }

    // 权限齐备 → 启动前台服务 + 运行时
    LaunchedEffect(granted) {
        if (!granted) return@LaunchedEffect
        BlePeripheralService.ensure(ctx)
        LinkxRuntime.start()
    }

    DisposableEffect(Unit) {
        LinkxRuntime.listener = { tick++ }
        LinkxRuntime.onChanged = { tick++ }
        if (Features.enabled(Module.Clipboard)) {
            LinkxRuntime.clipboardApplier = { text -> ClipboardSync.applyRemote(text) }
        }
        onDispose {
            LinkxRuntime.listener = null
            LinkxRuntime.onChanged = null
            LinkxRuntime.clipboardApplier = null
        }
    }

    // 会话就绪（首次配对完成 / 重连恢复）→ 补偿同步最新一条剪贴板
    val paired = remember(tick) { LinkxRuntime.isPaired() }
    LaunchedEffect(paired) {
        if (paired && Features.enabled(Module.Clipboard)) ClipboardSync.onSessionReady()
    }

    // 通知条目「复制」：写入本机剪贴板且不回传对端（复用防回声基准）
    val onCopy: (String) -> Unit = { text ->
        if (text.isNotEmpty()) {
            ClipboardSync.copyLocally(text)
            scope.launch { snackbar.showSnackbar(copiedTip) }
        }
    }

    Scaffold(
        containerColor = MaterialTheme.colorScheme.background,
        snackbarHost = { SnackbarHost(snackbar) },
        bottomBar = {
            NavigationBar(containerColor = MaterialTheme.colorScheme.surface) {
                tabs.forEach { item ->
                    NavigationBarItem(
                        selected = page == item.page,
                        onClick = { selected = item.page },
                        icon = {
                            Icon(
                                painterResource(item.icon),
                                contentDescription = item.label,
                                modifier = Modifier.size(NavIconSize),
                            )
                        },
                        label = { Text(item.label, maxLines = 1) },
                    )
                }
            }
        },
    ) { padding ->
        Column(modifier = Modifier.fillMaxSize().padding(padding)) {
            StatusHeader(tick)
            PrereqBanner(tick)
            HorizontalDivider(color = MaterialTheme.colorScheme.outlineVariant)

            // 页面切换动效：新页自右侧轻推入 + 淡入，旧页反向淡出
            AnimatedContent(
                targetState = page,
                transitionSpec = {
                    val dir = if (targetState > initialState) 1 else -1
                    (slideInHorizontally(tween(220)) { it / 10 * dir } + fadeIn(tween(200))) togetherWith
                        (slideOutHorizontally(tween(180)) { -it / 10 * dir } + fadeOut(tween(120)))
                },
                label = "page",
            ) { current ->
                Column(
                    modifier = Modifier
                        .fillMaxSize()
                        .verticalScroll(rememberScrollState())
                        .padding(16.dp),
                    verticalArrangement = Arrangement.spacedBy(12.dp),
                ) {
                    when (current) {
                        PAGE_NOTIF -> NotificationsPage(tick, onCopy)
                        PAGE_CLIP -> ClipboardPage(tick)
                        PAGE_FILES -> FilesPage(tick)
                        PAGE_MEDIA -> MediaPage(tick)
                        PAGE_SETTINGS -> SettingsPage(tick)
                        PAGE_ABOUT -> AboutPage()
                        else -> ConnectPage(tick)
                    }
                    Text(
                        // nativeVersion() 自带 "LinkX Core x.y.z"，前面不能再挂一个 "Core: "
                        // （每页页脚都显示，重复了两遍，真机截图上看得见）
                        runCatching { NativeCore.nativeVersion() }.getOrDefault("native 未加载"),
                        style = MaterialTheme.typography.labelSmall,
                        color = MaterialTheme.colorScheme.onSurfaceVariant,
                    )
                }
            }
        }
    }
}

// ---------- 通用组件 ----------

/** 顶部状态条：状态圆点 + 状态文案 + 对端名（协商中圆点呼吸） */
@Composable
private fun StatusHeader(refresh: Int) {
    val state = remember(refresh) { LinkxRuntime.state }
    val name = remember(refresh) { LinkxRuntime.peerName }
    val paired = state == LinkxRuntime.STATE_PAIRED || state == LinkxRuntime.STATE_REPAIRED
    // 未配对是常驻态，呼吸会让整窗按 120Hz 逐帧重绘；只在有 20s 时限的协商态动
    val negotiating = state == LinkxRuntime.STATE_HANDSHAKE || state == LinkxRuntime.STATE_PAIRING
    Row(
        modifier = Modifier.fillMaxWidth().padding(horizontal = 20.dp, vertical = 14.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        StatusDot(
            active = paired,
            busy = state == LinkxRuntime.STATE_DISCOVER || negotiating,
            pulse = negotiating,
        )
        Spacer(Modifier.width(10.dp))
        Column(Modifier.weight(1f)) {
            Text("LinkX · ${stateLabel(state)}", style = MaterialTheme.typography.titleMedium)
            Text(
                name?.let { "对端：$it" } ?: "对端：—",
                style = MaterialTheme.typography.bodySmall,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
            )
        }
    }
}

/**
 * 缺前提时的一行提示 + 一步跳转。蓝牙没开、没连局域网，都是"用户一改就好"而"程序做不了"的事，
 * 与其让界面沉默地扫不到设备，不如把缺的东西写在脸上。
 *
 * 跟着 1 Hz 的 [refresh] 读一次，不自建轮询：这两个状态是慢变量。
 */
@Composable
private fun PrereqBanner(refresh: Int) {
    val ctx = LocalContext.current
    val st = remember(refresh) { LinkPrereqs.check(ctx) }
    // (说明, 按钮文字, 跳转动作)；按钮文字为 null = 这台机器上做不了，只能告知
    val rows: List<Triple<String, String?, (() -> Boolean)?>> = buildList {
        when {
            st.noBluetooth ->
                add(Triple("本机没有蓝牙，配对与同步类功能不可用", null, null))
            !st.bluetoothOn ->
                add(Triple("蓝牙未开启：手机与电脑连不上", "去开启") { LinkPrereqs.openBluetooth(ctx) })
        }
        if (!st.lanConnected) {
            val why = if (st.cellularOnly) "正在用移动数据：文件互传与相册需要同一个局域网"
                      else "未连接局域网：文件互传与相册走局域网"
            add(Triple(why, "去连接") { LinkPrereqs.openNetwork(ctx) })
        }
    }
    if (rows.isEmpty()) return
    Column(
        Modifier.fillMaxWidth().padding(horizontal = 20.dp),
        verticalArrangement = Arrangement.spacedBy(4.dp),
    ) {
        rows.forEach { (text, action, jump) ->
            Row(verticalAlignment = Alignment.CenterVertically) {
                Text(
                    text,
                    style = MaterialTheme.typography.bodySmall,
                    color = MaterialTheme.colorScheme.error,
                    modifier = Modifier.weight(1f),
                )
                if (jump != null) {
                    TextButton(onClick = {
                        if (!jump()) {
                            Toast.makeText(ctx, "这个系统页面在本机不可用，请手动到设置里开启", Toast.LENGTH_LONG).show()
                        }
                    }) { Text(action ?: "") }
                }
            }
        }
    }
}

/** 状态圆点（配对=绿，忙碌=品牌色，其余=灰）；[pulse] 时品牌色圆点呼吸 */
@Composable
private fun StatusDot(active: Boolean, busy: Boolean, pulse: Boolean) {
    val color = when {
        active -> Color(0xFF1E9E55)
        busy -> MaterialTheme.colorScheme.primary
        else -> MaterialTheme.colorScheme.outline
    }
    val scale = if (pulse) {
        val transition = rememberInfiniteTransition(label = "pulse")
        // 规格必须 remember：这个分支每帧重组，新建对象 = 每帧一次分配
        val spec: InfiniteRepeatableSpec<Float> =
            remember { infiniteRepeatable(tween(900), RepeatMode.Reverse) }
        val s by transition.animateFloat(
            initialValue = 0.65f,
            targetValue = 1.25f,
            animationSpec = spec,
            label = "pulseScale",
        )
        s
    } else {
        1f
    }
    Box(Modifier.size(16.dp), contentAlignment = Alignment.Center) {
        Box(
            Modifier
                .size((9 * scale).dp)
                .clip(CircleShape)
                .background(color)
        )
    }
}

/** 统一卡片：可选小节标题 + 间距一致的内容列 */
@Composable
internal fun SectionCard(title: String? = null, content: @Composable ColumnScope.() -> Unit) {
    ElevatedCard(
        modifier = Modifier.fillMaxWidth(),
        colors = CardDefaults.elevatedCardColors(
            containerColor = MaterialTheme.colorScheme.surface,
            contentColor = MaterialTheme.colorScheme.onSurface,
        ),
    ) {
        Column(
            Modifier.padding(16.dp),
            verticalArrangement = Arrangement.spacedBy(8.dp),
        ) {
            if (title != null) {
                Text(
                    title,
                    style = MaterialTheme.typography.labelLarge,
                    color = MaterialTheme.colorScheme.primary,
                )
            }
            content()
        }
    }
}

/** 键值行（左标签 + 右强调值） */
@Composable
internal fun KeyValueRow(key: String, value: String, valueColor: Color? = null) {
    Column(verticalArrangement = Arrangement.spacedBy(2.dp)) {
        Text(key, style = MaterialTheme.typography.labelSmall, color = MaterialTheme.colorScheme.onSurfaceVariant)
        Text(
            value,
            style = MaterialTheme.typography.bodyMedium,
            color = valueColor ?: MaterialTheme.colorScheme.onSurface,
        )
    }
}

@Composable
internal fun EmptyHint(text: String) {
    Text(
        text,
        style = MaterialTheme.typography.bodySmall,
        color = MaterialTheme.colorScheme.onSurfaceVariant,
    )
}

// ---------- 文案 / 权限 ----------

internal fun stateLabel(state: Int): String = when (state) {
    LinkxRuntime.STATE_DISCOVER -> "未配对"
    LinkxRuntime.STATE_HANDSHAKE -> "握手中"
    LinkxRuntime.STATE_PAIRING -> "配对中"
    LinkxRuntime.STATE_SAS_COMPARE -> "待比对"
    LinkxRuntime.STATE_PAIRED, LinkxRuntime.STATE_REPAIRED -> "已配对"
    6 -> "重连中"
    7 -> "已断开"
    else -> "未就绪"
}

/**
 * **必需**运行时权限（Android 12+ 用新的蓝牙权限）：只含蓝牙——核心 BLE 功能与
 * `POST_NOTIFICATIONS` 无关，它进必需集会让用户拒绝通知权限直接卡死整个应用（可选集见 `optionalPermissions()`）。
 */
private fun requiredPermissions(): Array<String> {
    val list = mutableListOf<String>()
    if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) {
        list += Manifest.permission.BLUETOOTH_CONNECT
        list += Manifest.permission.BLUETOOTH_ADVERTISE
    } else {
        list += Manifest.permission.BLUETOOTH
        list += Manifest.permission.BLUETOOTH_ADMIN
    }
    return list.toTypedArray()
}

/** 可选权限（Android 13+ 通知）：拒绝不影响核心功能，仅少一条前台服务通知 */
private fun optionalPermissions(): Array<String> {
    val list = mutableListOf<String>()
    if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
        list += Manifest.permission.POST_NOTIFICATIONS
    }
    // 图片互传：只在相册开关开着时才问（关掉的功能不该在启动时弹一张它用不到的授权框）。
    // Android 14 上必须连 READ_MEDIA_VISUAL_USER_SELECTED 一起问，否则系统弹窗里没有
    // 「仅选定照片」这一档，用户想要小范围授权就只能去系统设置里改。
    if (Features.enabled(Module.Album)) {
        list += AlbumProvider.permissions()
    }
    return list.toTypedArray()
}

/** 启动时一次性申请的全部权限（必需在前，可选在后） */
private fun allPermissions(): Array<String> = requiredPermissions() + optionalPermissions()

private fun permissionsPresent(ctx: Context): Boolean =
    requiredPermissions().all {
        ContextCompat.checkSelfPermission(ctx, it) == PackageManager.PERMISSION_GRANTED
    }
