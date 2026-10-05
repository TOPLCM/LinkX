package com.linkx.app

import android.Manifest
import android.content.Context
import android.content.Intent
import android.content.pm.PackageManager
import android.net.Uri
import android.os.Build
import android.os.Bundle
import android.os.PowerManager
import android.provider.DocumentsContract
import android.provider.Settings
import android.util.Log
import android.webkit.MimeTypeMap
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
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.RowScope
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.Button
import androidx.compose.material3.ButtonDefaults
import androidx.compose.material3.Card
import androidx.compose.material3.CardDefaults
import androidx.compose.material3.ElevatedCard
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.NavigationBar
import androidx.compose.material3.NavigationBarItem
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Scaffold
import androidx.compose.material3.SnackbarHost
import androidx.compose.material3.SnackbarHostState
import androidx.compose.material3.Switch
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableIntStateOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.res.painterResource
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import androidx.core.content.ContextCompat
import androidx.core.content.FileProvider
import com.linkx.app.ui.LinkXTheme
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.delay
import kotlinx.coroutines.launch
import java.io.File

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

/**
 * 关于页文案的**单一来源**（对应 Windows 侧 `Platforms/Windows/src/about.rs`）。
 * 这一页不放任何外链（「获取最新版本」「打赏」都不上 UI），所以只有三行字，也就不再是"Links"。
 * 两端文案要改一起改（门禁不校验这一对）。
 */
internal object AboutCopy {
    const val APP_NAME = "LinkX"
    const val TAGLINE = "开源的跨端协同效率革命工具"
    const val AUTHOR = "开发者：Chaoming"
}

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
private val NavIconSize = 28.dp
private val ActionIconSize = 22.dp

/** 其它 App「分享到 LinkX」带进来的文件；只在主线程读写，互传页取走即清空。 */
private val sharedInbox = mutableListOf<Uri>()

private const val TAG = "LinkX.UI"

/** 分享授权拿不住时的提示（互传页显示一次）。空串 = 没有要提醒的。 */
private var shareGrantNote = ""

/** 取消原因（会显示在任务行上）：写明是本机点的，对端那一侧显示的是同一句。 */
private const val CANCEL_REASON = "手机端已取消"

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
private fun SectionCard(title: String? = null, content: @Composable ColumnScope.() -> Unit) {
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
private fun KeyValueRow(key: String, value: String, valueColor: Color? = null) {
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
private fun EmptyHint(text: String) {
    Text(
        text,
        style = MaterialTheme.typography.bodySmall,
        color = MaterialTheme.colorScheme.onSurfaceVariant,
    )
}

// ---------- 页：连接 ----------

@Composable
private fun ConnectPage(refresh: Int) {
    val state = remember(refresh) { LinkxRuntime.state }
    val name = remember(refresh) { LinkxRuntime.peerName }
    val sas = remember(refresh) { LinkxRuntime.sas }
    val mismatch = remember(refresh) { LinkxRuntime.fingerprintMismatch }
    val peerFp = remember(refresh) { LinkxRuntime.peerFp }

    SectionCard(title = "本机角色") {
        Text("本机作为 BLE Peripheral，等待电脑连接", style = MaterialTheme.typography.bodyMedium)
        Text(
            "状态：${stateLabel(state)}",
            style = MaterialTheme.typography.bodyMedium,
            color = MaterialTheme.colorScheme.primary,
        )
        Text(
            "对端：${name ?: "—"}",
            style = MaterialTheme.typography.bodySmall,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
        )
        // 电量上报失败也要上屏（同媒体页 lastSkip 口径）：只写 logcat 时，"电脑上没有电量"
        // 第一个被怀疑的总是蓝牙。
        val batteryNote = remember(refresh) { BatteryMonitor.note }
        if (batteryNote.isNotEmpty()) {
            Text(
                "电量上报：$batteryNote",
                style = MaterialTheme.typography.bodySmall,
                color = MaterialTheme.colorScheme.tertiary,
            )
        }
    }

    if (state == LinkxRuntime.STATE_SAS_COMPARE && sas != null) {
        Card(
            modifier = Modifier.fillMaxWidth(),
            colors = CardDefaults.cardColors(
                containerColor = MaterialTheme.colorScheme.primaryContainer,
                contentColor = MaterialTheme.colorScheme.onPrimaryContainer,
            ),
        ) {
            Column(
                Modifier.fillMaxWidth().padding(20.dp),
                horizontalAlignment = Alignment.CenterHorizontally,
                verticalArrangement = Arrangement.spacedBy(10.dp),
            ) {
                Text("请在电脑上核对同一组数字", style = MaterialTheme.typography.bodySmall)
                Text(
                    sas.toString().padStart(6, '0'),
                    style = MaterialTheme.typography.displaySmall,
                    fontWeight = FontWeight.Bold,
                )
                Row(horizontalArrangement = Arrangement.spacedBy(12.dp)) {
                    Button(onClick = { LinkxRuntime.confirmSas() }) { Text("确认一致") }
                    OutlinedButton(onClick = { LinkxRuntime.rejectSas() }) { Text("不一致") }
                }
            }
        }
    }

    val identityChange = remember(refresh) { LinkxRuntime.identityChange }
    if (identityChange != null) {
        // 对端同名设备换了身份必须显式决策：接受 → 引擎回到 Pairing 重走 SAS 复核（出现上方比对卡）；
        // 拒绝 → 断开且不更新信任库。
        Card(
            modifier = Modifier.fillMaxWidth(),
            colors = CardDefaults.cardColors(
                containerColor = MaterialTheme.colorScheme.errorContainer,
                contentColor = MaterialTheme.colorScheme.onErrorContainer,
            ),
        ) {
            Column(
                Modifier.fillMaxWidth().padding(20.dp),
                verticalArrangement = Arrangement.spacedBy(8.dp),
            ) {
                Text("对端设备身份已变化", style = MaterialTheme.typography.titleMedium)
                Text(
                    "设备「${identityChange.name.ifBlank { "对端设备" }}」呈递了新身份（可能已重装或重置）。",
                    style = MaterialTheme.typography.bodySmall,
                )
                Text("旧身份 ${identityChange.oldFingerprint}", style = MaterialTheme.typography.bodySmall)
                Text("新身份 ${identityChange.newFingerprint}", style = MaterialTheme.typography.bodySmall)
                Text(
                    "接受后将重新比对 6 位配对码；仅当确认是本人在操作时才接受。",
                    style = MaterialTheme.typography.bodySmall,
                )
                Row(horizontalArrangement = Arrangement.spacedBy(12.dp)) {
                    Button(onClick = { LinkxRuntime.acceptIdentityChange() }) { Text("信任并重新配对") }
                    OutlinedButton(onClick = { LinkxRuntime.rejectIdentityChange() }) { Text("取消") }
                }
            }
        }
    } else if (mismatch) {
        SectionCard(title = "身份指纹已变化") {
            Text(
                "对端长期身份指纹与上次记录不一致，可能存在中间人风险。",
                style = MaterialTheme.typography.bodySmall,
                color = MaterialTheme.colorScheme.error,
            )
            Button(onClick = { LinkxRuntime.acceptFingerprint() }) { Text("接受新指纹") }
        }
    }

    if (state == LinkxRuntime.STATE_PAIRED || state == LinkxRuntime.STATE_REPAIRED) {
        SectionCard(title = "已配对") {
            KeyValueRow("对端设备", name ?: "—", MaterialTheme.colorScheme.primary)
            KeyValueRow("设备指纹", peerFp ?: "—")
        }
    }

    SectionCard(title = "局域网通道（文件传输）") {
        val tcpBound = remember(refresh) { LinkxRuntime.tcpBound }
        val discovered = remember(refresh) { LinkxRuntime.discoveredIp }
        KeyValueRow(
            "TCP 通道",
            if (tcpBound) "已建立" else "未建立",
            if (tcpBound) Color(0xFF1E9E55) else null,
        )
        KeyValueRow("发现的电脑 IP", discovered ?: "—")
        var manualIp by remember { mutableStateOf(LinkxRuntime.manualPeerIp.orEmpty()) }
        OutlinedTextField(
            value = manualIp,
            onValueChange = { manualIp = it },
            label = { Text("手动输入电脑 IP") },
            singleLine = true,
            modifier = Modifier.fillMaxWidth(),
        )
        Row(horizontalArrangement = Arrangement.spacedBy(12.dp)) {
            Button(onClick = { LinkxRuntime.setManualPeerIp(manualIp) }) { Text("连接") }
            OutlinedButton(
                onClick = {
                    manualIp = ""
                    LinkxRuntime.setManualPeerIp(null)
                },
            ) { Text("自动发现") }
        }
        Text(
            "配对成功后自动在同网段发现电脑并建立 TCP 通道；搜不到时可手动填电脑 IP（跳过 UDP 发现）。",
            style = MaterialTheme.typography.bodySmall,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
        )
        // 连不上必须给出可归因的诊断：只写"未建立"，用户分不清是电脑防火墙没放行还是 App 的问题。
        val diag = remember(refresh) { LinkxRuntime.lanDiagnosis() }
        if (!tcpBound && diag != null) {
            Text(
                diag,
                style = MaterialTheme.typography.bodySmall,
                color = MaterialTheme.colorScheme.error,
            )
        }
    }

    SectionCard {
        Text(
            "首次配对需双端比对 6 位配对码；已信任设备会直接完成。",
            style = MaterialTheme.typography.bodySmall,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
        )
    }
}

// ---------- 页：通知 ----------

@Composable
private fun NotificationsPage(refresh: Int, onCopy: (String) -> Unit) {
    val ctx = LocalContext.current
    // 只读授权项，不用 NotificationManagerCompat.getEnabledListenerPackages：后者 API 30 起才有，
    // 且只回答"授没授权"。授权还在但系统没把服务绑回来，是应用被 ROM 结束后的常态，必须分开显示。
    val authorized = remember(refresh) { NlsService.authorizedComponent(ctx) != null }
    val bound = remember(refresh) { LinkxRuntime.nlsBound }
    val forwarded = remember(refresh) { LinkxRuntime.forwardedRecent() }

    fun openListenerSettings() {
        runCatching {
            ctx.startActivity(
                Intent(Settings.ACTION_NOTIFICATION_LISTENER_SETTINGS)
                    .addFlags(Intent.FLAG_ACTIVITY_NEW_TASK),
            )
        }
    }

    SectionCard(title = "通知读取权限") {
        when {
            !authorized -> {
                Text(
                    "未开启：无法把本机通知转发到电脑",
                    style = MaterialTheme.typography.bodyMedium,
                    color = MaterialTheme.colorScheme.error,
                )
                Button(onClick = { openListenerSettings() }) { Text("前往系统设置开启") }
            }

            bound -> Text(
                "已开启，手机通知会自动同步到电脑",
                style = MaterialTheme.typography.bodyMedium,
                color = Color(0xFF1E9E55),
            )

            else -> {
                Text(
                    "已授权，但系统当前没有把通知交给我们",
                    style = MaterialTheme.typography.bodyMedium,
                    color = MaterialTheme.colorScheme.error,
                )
                Text(
                    "应用被系统结束之后，多数国产 ROM 就不会再绑定通知监听了。" +
                        "允许自启动可以长期解决；眼下先重新开启一次通知使用权即可恢复。",
                    style = MaterialTheme.typography.bodySmall,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
                Row(horizontalArrangement = Arrangement.spacedBy(10.dp)) {
                    Button(onClick = {
                        if (!LinkPrereqs.openAutoStart(ctx)) openListenerSettings()
                    }) { Text("允许自启动") }
                    Button(
                        onClick = { openListenerSettings() },
                        colors = ButtonDefaults.outlinedButtonColors(),
                    ) { Text("重新开启通知使用权") }
                }
            }
        }
    }

    Text(
        "已转发通知（最多 20 条）",
        style = MaterialTheme.typography.labelLarge,
        color = MaterialTheme.colorScheme.onSurfaceVariant,
        modifier = Modifier.padding(start = 4.dp, top = 4.dp),
    )
    if (forwarded.isEmpty()) {
        EmptyHint("暂无记录")
    } else {
        forwarded.forEach { n ->
            NotificationCard(n, onCopy)
        }
    }
}

/** 单条已转发通知（右侧「复制」按钮，正文可整段复制） */
@Composable
private fun NotificationCard(n: ForwardedNotification, onCopy: (String) -> Unit) {
    Card(
        modifier = Modifier.fillMaxWidth(),
        colors = CardDefaults.cardColors(containerColor = MaterialTheme.colorScheme.surface),
    ) {
        Row(
            Modifier.padding(start = 14.dp, top = 10.dp, end = 6.dp, bottom = 10.dp),
            verticalAlignment = Alignment.Top,
        ) {
            Column(
                Modifier.weight(1f),
                verticalArrangement = Arrangement.spacedBy(3.dp),
            ) {
                Text(
                    n.title.ifEmpty { n.pkg },
                    style = MaterialTheme.typography.titleSmall,
                    maxLines = 1,
                    overflow = TextOverflow.Ellipsis,
                )
                Text(
                    n.pkg,
                    style = MaterialTheme.typography.labelSmall,
                    color = MaterialTheme.colorScheme.primary,
                    maxLines = 1,
                    overflow = TextOverflow.Ellipsis,
                )
                Text(
                    n.text.ifEmpty { "（无正文）" },
                    style = MaterialTheme.typography.bodyMedium,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                    maxLines = 3,
                    overflow = TextOverflow.Ellipsis,
                )
            }
            // 抽得出验证码时给一个"只复制码"的入口：短信通知整段粘过去还得手动删掉
            // "请勿泄露"，而用户要的往往就是那 6 个数字。
            val code = remember(n.title, n.text) { LinkxRuntime.noticeCode(n.title, n.text) }
            if (code != null) {
                TextButton(onClick = { onCopy(code) }) {
                    Text("复制验证码 $code", style = MaterialTheme.typography.labelMedium)
                }
            }
            IconButton(onClick = { onCopy(n.text.ifEmpty { n.title }) }) {
                Icon(
                    painterResource(R.drawable.ic_copy),
                    contentDescription = "复制正文",
                    modifier = Modifier.size(ActionIconSize),
                    tint = MaterialTheme.colorScheme.primary,
                )
            }
        }
    }
}

// ---------- 页：剪贴板 ----------

@Composable
private fun ClipboardPage(refresh: Int) {
    var on by remember { mutableStateOf(ClipboardSync.enabled) }
    val sent = remember(refresh) { LinkxRuntime.clipSentRecent() }
    val recv = remember(refresh) { LinkxRuntime.clipRecvRecent() }

    SectionCard(title = "剪贴板同步") {
        Row(
            modifier = Modifier.fillMaxWidth(),
            horizontalArrangement = Arrangement.SpaceBetween,
            verticalAlignment = Alignment.CenterVertically,
        ) {
            Text(if (on) "已开启" else "已关闭", style = MaterialTheme.typography.bodyMedium)
            Switch(
                checked = on,
                onCheckedChange = {
                    on = it
                    ClipboardSync.setEnabled(it)
                },
            )
        }
        Text(
            "开启后，已配对设备间的纯文本复制会自动同步。",
            style = MaterialTheme.typography.bodySmall,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
        )
        // Android 10+ 只允许持输入焦点的应用读剪贴板，后台监听不会触发：给降级说明 + 手动兜底入口。
        Text(
            "Android 10+ 限制：在其他应用里复制后，切回本页会自动补同步；也可点下方按钮立即发送。",
            style = MaterialTheme.typography.bodySmall,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
        )
        OutlinedButton(onClick = { ClipboardSync.syncNow("手动") }) {
            Text("立即同步本机剪贴板")
        }
    }

    SectionCard(title = "最近发送") {
        if (sent.isEmpty()) EmptyHint("暂无记录") else sent.forEach { Text(it, style = MaterialTheme.typography.bodyMedium) }
    }
    SectionCard(title = "最近接收") {
        if (recv.isEmpty()) EmptyHint("暂无记录") else recv.forEach { Text(it, style = MaterialTheme.typography.bodyMedium) }
    }
}

// ---------- 页：文件（SAF 选择 + 加密通道传输）----------

@Composable
private fun FilesPage(refresh: Int) {
    val ctx = LocalContext.current
    val scope = rememberCoroutineScope()
    // 已选文件（Uri + 展示用名称/大小）
    var picked by remember { mutableStateOf<Pair<Uri, FileBrief>?>(null) }
    val transfers = remember(refresh) { LinkxRuntime.transfers() }
    // 接收目录：dirTick 让"用户刚改完目录"能立刻反映到界面，不必等下一次状态轮询
    var dirTick by remember { mutableStateOf(0) }
    val recvDir = remember(refresh, dirTick) { LinkxRuntime.receiveDir() }
    val dirCustom = remember(refresh, dirTick) { LinkxRuntime.receiveDirIsCustom() }
    val dirPicker = rememberLauncherForActivityResult(ActivityResultContracts.OpenDocumentTree()) { uri ->
        if (uri == null) return@rememberLauncherForActivityResult
        runCatching { LinkxRuntime.setReceiveDir(uri) }
            .onSuccess {
                dirTick++
                Toast.makeText(ctx, "以后收到的文件都存进「$it」", Toast.LENGTH_LONG).show()
            }
            .onFailure { Toast.makeText(ctx, "改目录失败：${it.message}", Toast.LENGTH_LONG).show() }
    }
    // 其它 App「分享到 LinkX」带进来的：进页即取走，第一条直接选中
    val shared = remember { sharedInbox.toList().also { sharedInbox.clear() } }
    LaunchedEffect(shared) {
        if (picked == null && shared.isNotEmpty()) picked = shared.first() to fileBriefOf(ctx, shared.first())
    }

    // SAF 选择：无需任何存储权限；持久化读权限以便后台线程再次打开同一 Uri
    val picker = rememberLauncherForActivityResult(ActivityResultContracts.OpenDocument()) { uri ->
        if (uri != null) {
            runCatching {
                ctx.contentResolver.takePersistableUriPermission(
                    uri,
                    Intent.FLAG_GRANT_READ_URI_PERMISSION,
                )
            }
            picked = uri to fileBriefOf(ctx, uri)
        }
    }

    SectionCard(title = "发送到电脑") {
        val p = picked
        if (p == null) {
            EmptyHint("未选择文件")
        } else {
            KeyValueRow("文件名", p.second.name)
            KeyValueRow("文件大小", formatSize(p.second.size))
        }
        Row(horizontalArrangement = Arrangement.spacedBy(12.dp)) {
            OutlinedButton(onClick = { picker.launch(arrayOf("*/*")) }) {
                Icon(
                    painterResource(R.drawable.ic_upload),
                    contentDescription = null,
                    modifier = Modifier.size(ActionIconSize),
                )
                Spacer(Modifier.width(6.dp))
                Text("选择文件")
            }
            Button(
                enabled = picked != null,
                onClick = {
                    val uri = picked?.first ?: return@Button
                    // 摘要计算 + 分块发送为阻塞 IO，放 IO 线程
                    scope.launch(Dispatchers.IO) {
                        runCatching { LinkxRuntime.sendFile(uri) }
                            .onFailure { Log.w(TAG, "发送协程里抛出：${it.message}", it) }
                    }
                },
            ) {
                Icon(
                    painterResource(R.drawable.ic_send),
                    contentDescription = null,
                    modifier = Modifier.size(ActionIconSize),
                )
                Spacer(Modifier.width(6.dp))
                Text("发送到电脑")
            }
        }
        if (shared.size > 1) {
            Text(
                "分享进来 ${shared.size} 个文件，一次只能发一个，点一条换选中：",
                style = MaterialTheme.typography.bodySmall,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
            )
            shared.forEach { u ->
                TextButton(onClick = { picked = u to fileBriefOf(ctx, u) }) {
                    Text(u.lastPathSegment ?: "文件", maxLines = 1, overflow = TextOverflow.Ellipsis)
                }
            }
        }
        if (shareGrantNote.isNotEmpty()) {
            Text(
                shareGrantNote,
                style = MaterialTheme.typography.bodySmall,
                color = MaterialTheme.colorScheme.tertiary,
            )
        }
        Text(
            "文件经局域网加密通道传输：256KB 分块 · 每块 CRC32 · 整文件 SHA-256 · 断点续传。",
            style = MaterialTheme.typography.bodySmall,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
        )
    }

    SectionCard(title = "从电脑收文件") {
        Text(
            "电脑上打开 LinkX 的「文件」页，选好文件点「发送到手机」即可；" +
                "手机这边自动接收，进度落在下面的传输记录里。",
            style = MaterialTheme.typography.bodySmall,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
        )
        KeyValueRow("接收目录", recvDir.ifEmpty { "—" })
        Row(horizontalArrangement = Arrangement.spacedBy(8.dp), verticalAlignment = Alignment.CenterVertically) {
            OutlinedButton(onClick = { dirPicker.launch(null) }) { Text("改到别的文件夹…") }
            if (dirCustom) {
                TextButton(onClick = {
                    LinkxRuntime.clearReceiveDir()
                    dirTick++
                }) { Text("改回默认") }
            }
        }
        Text(
            if (dirCustom) {
                "改目录只影响**之后**收到的文件；已经收下的仍留在原处。"
            } else {
                "默认存在应用私有目录里，相册和文件管理器都看不到它。" +
                    "改到「文档」这类文件夹后就能直接找到（授权是持久的，重启也认）。" +
                        "部分机型不让把「下载」「相册」授权给第三方应用，换一个目录即可。"
            },
            style = MaterialTheme.typography.bodySmall,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
        )
    }

    Text(
        "传输记录",
        style = MaterialTheme.typography.labelLarge,
        color = MaterialTheme.colorScheme.onSurfaceVariant,
        modifier = Modifier.padding(start = 4.dp, top = 4.dp),
    )
    if (transfers.isEmpty()) {
        EmptyHint("暂无传输")
    } else {
        transfers.forEach { TransferRow(it) }
    }
}

/** SAF Uri 的名称/大小；拿不到元数据时退化成"用路径末段 + 未知大小"，不让界面空着 */
private fun fileBriefOf(ctx: Context, uri: Uri): FileBrief =
    LinkxRuntime.fileBrief(uri) ?: FileBrief(uri.lastPathSegment ?: "文件", -1L)

/** 单条传输（名称 / 方向 / 进度条 / 状态 / 失败或取消的原因 / 在途行可取消 / 收到的文件可打开或分享） */
@Composable
private fun TransferRow(t: TransferItem) {
    val ctx = LocalContext.current
    val percent = if (t.size > 0L) {
        ((t.bytes.coerceAtLeast(0L) * 100) / t.size).toInt().coerceIn(0, 100)
    } else {
        0
    }
    val color = when (t.state) {
        TransferState.Done -> Color(0xFF1E9E55)
        TransferState.Failed -> MaterialTheme.colorScheme.error
        TransferState.Running -> MaterialTheme.colorScheme.primary
        // 分块发完了但对端还没回执：用次要色区分，别让用户误以为已经成功
        TransferState.AwaitingPeer -> MaterialTheme.colorScheme.tertiary
        // 等回执超时：不是失败也不是成功，用告警色说清楚
        TransferState.SentUnconfirmed -> MaterialTheme.colorScheme.error.copy(alpha = 0.75f)
        // 取消既不是成功也不是故障：用中性色，不给告警色（用户做对的事不该被标红）
        TransferState.Cancelled -> MaterialTheme.colorScheme.onSurfaceVariant
    }
    Card(
        modifier = Modifier.fillMaxWidth(),
        colors = CardDefaults.cardColors(containerColor = MaterialTheme.colorScheme.surface),
    ) {
        Column(
            Modifier.fillMaxWidth().padding(14.dp),
            verticalArrangement = Arrangement.spacedBy(6.dp),
        ) {
            Row(Modifier.fillMaxWidth(), horizontalArrangement = Arrangement.SpaceBetween) {
                Text(
                    t.name,
                    style = MaterialTheme.typography.titleSmall,
                    maxLines = 1,
                    overflow = TextOverflow.Ellipsis,
                    modifier = Modifier.weight(1f),
                )
                Row(verticalAlignment = Alignment.CenterVertically) {
                    Icon(
                        painterResource(if (t.outgoing) R.drawable.ic_upload else R.drawable.ic_download),
                        contentDescription = if (t.outgoing) "发送" else "接收",
                        modifier = Modifier.size(ActionIconSize),
                        tint = color,
                    )
                    Spacer(Modifier.width(4.dp))
                    Text(
                        if (t.outgoing) "发送" else "接收",
                        style = MaterialTheme.typography.labelSmall,
                        color = MaterialTheme.colorScheme.primary,
                    )
                }
            }
            // 进度条：两层 Box 绘制，避免依赖特定 material3 进度条 API 版本
            Box(
                Modifier
                    .fillMaxWidth()
                    .height(6.dp)
                    .clip(RoundedCornerShape(3.dp))
                    .background(MaterialTheme.colorScheme.outlineVariant),
            ) {
                Box(
                    Modifier
                        .fillMaxWidth(percent / 100f)
                        .height(6.dp)
                        .clip(RoundedCornerShape(3.dp))
                        .background(color),
                )
            }
            Text(
                "$percent% · ${transferStateLabel(t.state)} · " +
                    "${formatSize(t.bytes)}/${formatSize(t.size)}",
                style = MaterialTheme.typography.bodySmall,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
            )
            // 失败与取消都要把原因说出来：只标一个状态等于让用户自己去猜是链路断了还是没配对
            val note = t.error.ifEmpty {
                if (t.state == TransferState.Failed || t.state == TransferState.Cancelled) {
                    "未说明原因（在「设置」里打开 Debug 模式后可导出日志细查）"
                } else {
                    // 已完成但有一句话要说（例如"没能存进你选的目录，先留在私有目录"）
                    t.note
                }
            }
            if (note.isNotEmpty()) {
                Text(
                    note,
                    style = MaterialTheme.typography.bodySmall,
                    // 取消的原因不是故障，不给告警色；只有真失败才标红
                    color = if (t.state == TransferState.Failed) MaterialTheme.colorScheme.error
                    else MaterialTheme.colorScheme.onSurfaceVariant,
                )
            }
            // 「取消」只画在点下去真的会停的行上：判据取自 `LinkxRuntime.fileCancellable`，
            // 与取消命令入口是同一个函数，不在这里另判一遍（等回执行、终态行都停不下来）
            if (LinkxRuntime.fileCancellable(t)) {
                TextButton(onClick = {
                    val why = if (t.outgoing) {
                        LinkxRuntime.cancelFileSend(t.fileId, CANCEL_REASON)
                    } else {
                        LinkxRuntime.cancelFileRecv(t.fileId, CANCEL_REASON)
                    }
                    // 返回非空 = 这一下什么都没取消，必须当场把原因讲给用户
                    if (why != null) {
                        Toast.makeText(ctx, why, Toast.LENGTH_LONG).show()
                    }
                }) { Text("取消") }
            }
            if (!t.outgoing && t.state == TransferState.Done && t.path.isNotEmpty()) {
                Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                    TextButton(onClick = { openReceived(ctx, t.path, t.name) }) { Text("打开") }
                    TextButton(onClick = { shareReceived(ctx, t.path, t.name) }) { Text("分享") }
                }
            }
        }
    }
}

/**
 * 收到的文件那份"能打开/能分享"的 Uri。
 *
 * 两种来源：应用私有目录里的绝对路径（必须经 FileProvider，直接给 file:// 系统会拒绝），
 * 以及用户自选目录里的那一份（本身就是 `content://` 文档 Uri，直接可用——
 * FileProvider 反而处理不了它，它不在我们的授权路径里）。
 */
private fun receivedUri(ctx: Context, path: String): Uri = if (path.startsWith("content://")) {
    Uri.parse(path)
} else {
    FileProvider.getUriForFile(ctx, "${ctx.packageName}.fileprovider", File(path))
}

/** 收到的文件交给系统「打开」（名字用传输记录里的那份：content 路径看不出文件名） */
private fun openReceived(ctx: Context, path: String, displayName: String) {
    val name = displayName.ifEmpty { File(path).name }
    runCatching {
        val intent = Intent(Intent.ACTION_VIEW).apply {
            setDataAndType(receivedUri(ctx, path), mimeOf(name))
            addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION)
        }
        ctx.startActivity(Intent.createChooser(intent, "打开 $name"))
    }.onFailure {
        // 只写日志等于"按钮是死的"：这是用户直接点出来的动作，必须当场回话
        Log.w(TAG, "打不开 $name：${it.message}")
        Toast.makeText(ctx, "打不开 $name：${it.message ?: "系统里没有能打开它的应用"}", Toast.LENGTH_LONG).show()
    }
}

/** 收到的文件分享出去（再发给别的 App 或另存） */
private fun shareReceived(ctx: Context, path: String, displayName: String) {
    val name = displayName.ifEmpty { File(path).name }
    runCatching {
        val intent = Intent(Intent.ACTION_SEND).apply {
            type = mimeOf(name)
            putExtra(Intent.EXTRA_STREAM, receivedUri(ctx, path))
            addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION)
        }
        ctx.startActivity(Intent.createChooser(intent, "分享 $name"))
    }.onFailure {
        Log.w(TAG, "分享不出去 $name：${it.message}")
        Toast.makeText(ctx, "分享不出去 $name：${it.message ?: "没有接收分享的应用"}", Toast.LENGTH_LONG).show()
    }
}

/**
 * 按扩展名猜 MIME：不能用 `getFileExtensionFromUrl`（它是 URL 工具，把 `#`/`?` 之后当片段丢掉，
 * 而收到的文件名常带这些字符）。兜底 `application/octet-stream`，不用通配类型——
 * 通配作为 intent 类型匹配不到任何 `IntentFilter`，只会换来 ActivityNotFoundException。
 */
private fun mimeOf(path: String): String =
    MimeTypeMap.getSingleton()
        .getMimeTypeFromExtension(File(path).extension.lowercase())
        ?: "application/octet-stream"

private fun transferStateLabel(state: TransferState): String = when (state) {
    TransferState.Running -> "传输中"
    TransferState.AwaitingPeer -> "等待电脑确认"
    TransferState.SentUnconfirmed -> "已发送（未确认）"
    TransferState.Done -> "已完成"
    TransferState.Failed -> "失败"
    // 用户主动动作：文案不许写成失败，也不写"已中止"这种含糊词
    TransferState.Cancelled -> "已取消"
}

/** 人类可读大小（负数 = 未知，显示 —） */
private fun formatSize(bytes: Long): String {
    if (bytes < 0L) return "—"
    if (bytes < 1024L) return "$bytes B"
    val kb = bytes / 1024.0
    if (kb < 1024.0) return "%.1f KB".format(kb)
    val mb = kb / 1024.0
    if (mb < 1024.0) return "%.1f MB".format(mb)
    return "%.2f GB".format(mb / 1024.0)
}

// ---------- 页：媒体（本机播放状态 + 与 Windows 同一套图标）----------

@Composable
private fun MediaPage(refresh: Int) {
    val ctx = LocalContext.current
    // 采样在后台每 3 s 跑一次，页面按 1 s 回看它的缓存：进度条会动，
    // 而 UI 自己不去查 MediaSession（那是重组里的 Binder 调用）。
    var beat by remember { mutableIntStateOf(0) }
    LaunchedEffect(Unit) {
        while (true) {
            delay(1_000)
            beat++
        }
    }
    val now = remember(refresh, beat) { MediaControl.current }

    if (now == null) {
        SectionCard(title = "正在播放") {
            Row(verticalAlignment = Alignment.CenterVertically) {
                Icon(
                    painterResource(R.drawable.ic_music),
                    contentDescription = null,
                    modifier = Modifier.size(NavIconSize),
                    tint = MaterialTheme.colorScheme.onSurfaceVariant,
                )
                Spacer(Modifier.width(10.dp))
                Text(
                    "手机现在没有在播放任何东西",
                    style = MaterialTheme.typography.bodyMedium,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
            }
            Text(
                "打开任意音乐 / 播客 / 视频 App，这里几秒内就会出现曲目信息，" +
                    "电脑上也能看到同一份状态。",
                style = MaterialTheme.typography.bodySmall,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
            )
            val skip = MediaControl.lastSkip
            if (skip.isNotEmpty()) {
                Text(
                    "读不到播放状态：$skip",
                    style = MaterialTheme.typography.bodySmall,
                    color = MaterialTheme.colorScheme.tertiary,
                )
            }
        }
        return
    }

    SectionCard(title = "正在播放") {
        Text(
            now.title.ifEmpty { "（该应用未提供曲目名）" },
            style = MaterialTheme.typography.titleMedium,
            maxLines = 2,
            overflow = TextOverflow.Ellipsis,
        )
        val sub = listOf(now.artist, now.album).filter { it.isNotEmpty() }.joinToString(" · ")
        if (sub.isNotEmpty()) {
            Text(
                sub,
                style = MaterialTheme.typography.bodyMedium,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
            )
        }
        Text(
            (if (now.playing) "播放中" else "已暂停") +
                " · ${fmtMs(now.positionMs)}/${fmtMs(now.durationMs)}" +
                " · 来源 ${now.pkg.ifEmpty { "未知" }}",
            style = MaterialTheme.typography.bodySmall,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
        )
        if (now.durationMs > 0L) {
            Bar((now.positionMs * 100 / now.durationMs).toInt().coerceIn(0, 100))
        }
        // 音量：手机读不到（-1）时不显示数字，也不给 +/- —— 拿 0 当基准会把本机静音
        if (now.volume >= 0) {
            Text(
                "媒体音量 ${now.volume}%",
                style = MaterialTheme.typography.bodySmall,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
            )
            Bar(now.volume.coerceIn(0, 100))
        } else {
            Text(
                "媒体音量 —（当前无活动播放，读到的 0 不代表静音）",
                style = MaterialTheme.typography.bodySmall,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
            )
        }
        val vol = now.volume
        // 音量以"本地累加"为准：采样每 3 s 才刷新一次，连点两下如果都按采样值算，
        // 第二次会发出同一个绝对值 —— 用户看到的就是"按钮点不动"。
        var volLocal by remember(now.volume) { mutableIntStateOf(vol.coerceAtLeast(0)) }
        val feedback: (String) -> Unit = { msg ->
            if (msg.startsWith("已丢弃")) Toast.makeText(ctx, msg, Toast.LENGTH_SHORT).show()
        }
        Row(
            Modifier.fillMaxWidth(),
            horizontalArrangement = Arrangement.spacedBy(4.dp),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            MediaButton(R.drawable.ic_prev, "上一首") {
                feedback(MediaControl.submitCommand(ctx, MediaControl.ACTION_PREV, 0, 0))
            }
            MediaButton(
                if (now.playing) R.drawable.ic_pause else R.drawable.ic_play,
                if (now.playing) "暂停" else "播放",
            ) {
                feedback(MediaControl.submitCommand(ctx, MediaControl.ACTION_PLAY_PAUSE, 0, 0))
            }
            MediaButton(R.drawable.ic_next, "下一首") {
                feedback(MediaControl.submitCommand(ctx, MediaControl.ACTION_NEXT, 0, 0))
            }
            MediaButton(R.drawable.ic_vol_down, "音量减", enabled = vol >= 0) {
                volLocal = (volLocal - 5).coerceIn(0, 100)
                feedback(
                    MediaControl.submitCommand(ctx, MediaControl.ACTION_SET_VOLUME, volLocal, 0)
                )
            }
            MediaButton(R.drawable.ic_vol_up, "音量加", enabled = vol >= 0) {
                volLocal = (volLocal + 5).coerceIn(0, 100)
                feedback(
                    MediaControl.submitCommand(ctx, MediaControl.ACTION_SET_VOLUME, volLocal, 0)
                )
            }
        }
        Text(
            "LinkX 只同步状态与控制指令，不搬运音频；这里点的动作与电脑上的按钮走同一条通道。",
            style = MaterialTheme.typography.bodySmall,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
        )
    }
}

@Composable
private fun RowScope.MediaButton(
    @DrawableRes icon: Int,
    label: String,
    enabled: Boolean = true,
    onClick: () -> Unit,
) {
    Column(horizontalAlignment = Alignment.CenterHorizontally, modifier = Modifier.weight(1f)) {
        IconButton(onClick = onClick, enabled = enabled) {
            Icon(
                painterResource(icon),
                contentDescription = label,
                modifier = Modifier.size(NavIconSize),
            )
        }
        Text(
            label,
            style = MaterialTheme.typography.labelSmall,
            maxLines = 1,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
        )
    }
}

/** 细进度条（与传输记录同一画法，不依赖特定 material3 版本） */
@Composable
private fun Bar(percent: Int) {
    Box(
        Modifier
            .fillMaxWidth()
            .height(6.dp)
            .clip(RoundedCornerShape(3.dp))
            .background(MaterialTheme.colorScheme.outlineVariant),
    ) {
        Box(
            Modifier
                .fillMaxWidth(percent / 100f)
                .height(6.dp)
                .clip(RoundedCornerShape(3.dp))
                .background(MaterialTheme.colorScheme.primary),
        )
    }
}

private fun fmtMs(ms: Long): String {
    val s = ms.coerceAtLeast(0L) / 1000
    val h = s / 3600
    val m = (s % 3600) / 60
    val sec = s % 60
    return if (h > 0) "%d:%02d:%02d".format(h, m, sec) else "%d:%02d".format(m, sec)
}

// ---------- 页：设置 ----------

/**
 * 关于页：三行字、没有外链。文案取自 [AboutCopy]，这里只排版——改一个字不该动界面代码。
 */
@Composable
private fun AboutPage() {
    Column(
        modifier = Modifier.fillMaxWidth(),
        verticalArrangement = Arrangement.spacedBy(10.dp),
    ) {
        Text(
            AboutCopy.APP_NAME,
            style = MaterialTheme.typography.displaySmall,
            fontWeight = FontWeight.Bold,
        )
        Text(AboutCopy.TAGLINE, style = MaterialTheme.typography.titleMedium)
        Text(
            AboutCopy.AUTHOR,
            style = MaterialTheme.typography.bodyMedium,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
        )
    }
}

@Composable
private fun SettingsPage(refresh: Int) {
    val ctx = LocalContext.current
    val peerFp = remember(refresh) { LinkxRuntime.peerFp }
    val localFp = remember(refresh) { LinkxRuntime.localFingerprint }
    val trusted = remember(refresh) { LinkxRuntime.trustedDevices() }
    val debugOn = remember(refresh) { LinkxRuntime.debugEnabled }
    var confirmUnbind by remember { mutableStateOf(false) }

    // 省电白名单：不在名单里，ROM 会在 App 退到后台后冻结线程——"不在前台就控不了媒体、
    // 通知不及时"就是这么来的。系统不允许我们自己加白，只能把状态说清楚并一键跳过去。
    val unrestricted = remember(refresh) {
        (ctx.getSystemService(Context.POWER_SERVICE) as? PowerManager)
            ?.isIgnoringBatteryOptimizations(ctx.packageName) ?: false
    }
    SectionCard(title = "后台运行") {
        Text(
            if (unrestricted) "已允许后台运行：退到后台也能继续同步与接收电脑的控制"
            else "未允许：手机退到后台后系统可能暂停本应用，通知与媒体控制会延迟甚至失联",
            style = MaterialTheme.typography.bodySmall,
            color = if (unrestricted) Color(0xFF1E9E55) else MaterialTheme.colorScheme.error,
        )
        if (!unrestricted) {
            OutlinedButton(
                onClick = {
                    val ok = runCatching {
                        ctx.startActivity(
                            Intent(Settings.ACTION_IGNORE_BATTERY_OPTIMIZATION_SETTINGS)
                        )
                    }.isSuccess
                    if (!ok) {
                        Toast.makeText(
                            ctx,
                            "系统没有这个设置页，请在「设置 → 电池」里手动把 LinkX 设为无限制",
                            Toast.LENGTH_LONG,
                        ).show()
                    }
                },
            ) { Text("去系统设置") }
        }
    }

    SectionCard(title = "功能开关") {
        Text(
            "关掉不用的功能，重启后就不再初始化它——不起线程、不注册回调、也不建这一页。",
            style = MaterialTheme.typography.bodySmall,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
        )
        Module.entries.forEach { m ->
            Row(
                modifier = Modifier.fillMaxWidth(),
                horizontalArrangement = Arrangement.SpaceBetween,
                verticalAlignment = Alignment.CenterVertically,
            ) {
                Column(modifier = Modifier.weight(1f)) {
                    Text(m.label, style = MaterialTheme.typography.bodyMedium)
                    Text(
                        m.about,
                        style = MaterialTheme.typography.bodySmall,
                        color = MaterialTheme.colorScheme.onSurfaceVariant,
                    )
                }
                Switch(
                    checked = Features.isWanted(m),
                    onCheckedChange = { Features.setWanted(ctx, m, it) },
                )
            }
        }
        val pending = Features.changes()
        if (pending.isNotEmpty()) {
            Text(
                "以下改动要重启才生效：" +
                    pending.joinToString("、") { (m, on) -> "${m.label}${if (on) "开" else "关"}" } +
                    "。当前已加载：${Features.activeModules().joinToString("、") { it.label }.ifEmpty { "无" }}",
                style = MaterialTheme.typography.bodySmall,
                color = MaterialTheme.colorScheme.primary,
            )
        }
    }

    // 相册应答方**没有浏览界面**（电脑问、手机答），这一页只交代两件事：开关与权限状态。
    // 权限形态必须分开显示：Android 14 的「仅选定照片」是真授权，说成"未授予"会逼用户
    // 再点一次同一个框，而他想要的其实只是那几张不要的全库授权。
    var albumTick by remember { mutableStateOf(0) }
    val albumAccess = remember(albumTick) { AlbumProvider.access(ctx) }
    val albumLauncher = rememberLauncherForActivityResult(
        ActivityResultContracts.RequestMultiplePermissions(),
    ) { albumTick++ }
    SectionCard(title = "图片互传（相册应答）") {
        KeyValueRow(
            "相册权限",
            AlbumProvider.accessBrief(ctx),
            valueColor = when (albumAccess) {
                AlbumAccess.Full -> Color(0xFF1E9E55)
                AlbumAccess.SelectedOnly -> MaterialTheme.colorScheme.primary
                AlbumAccess.Denied -> MaterialTheme.colorScheme.error
            },
        )
        Text(
            if (Features.enabled(Module.Album)) {
                "手机只应答电脑发来的清单/缩略图/原图请求；缩略图现场生成、直接发出，" +
                    "不在本机留任何缓存文件（关掉就没有）。照片只会经局域网交给已配对的电脑。"
            } else {
                "已关闭：电脑再问也只会收到一句「相册同步已在手机端关闭」，不会静默。"
            },
            style = MaterialTheme.typography.bodySmall,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
        )
        if (albumAccess != AlbumAccess.Full && Features.enabled(Module.Album)) {
            OutlinedButton(onClick = { albumLauncher.launch(AlbumProvider.permissions()) }) {
                Text(if (albumAccess == AlbumAccess.Denied) "授予相册权限" else "重新选择可见照片")
            }
        }
        KeyValueRow("最近一次应答", AlbumProvider.lastStatus)
        if (AlbumProvider.lastError.isNotEmpty()) {
            KeyValueRow("待处理原因", AlbumProvider.lastError, valueColor = MaterialTheme.colorScheme.error)
        }
    }

    SectionCard(title = "设备管理") {
        KeyValueRow("已绑定设备", if (trusted.isEmpty()) "—" else trusted.joinToString("、") { it.second.ifBlank { it.first } })
        KeyValueRow("对端指纹", peerFp ?: "—")
        KeyValueRow("本机指纹", localFp ?: "—")
        Text(
            "解绑将清除本机信任的对端指纹，下次连接需重新配对（比对 6 位配对码）。",
            style = MaterialTheme.typography.bodySmall,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
        )
        OutlinedButton(
            onClick = { confirmUnbind = true },
            enabled = trusted.isNotEmpty() || peerFp != null,
        ) { Text("解绑设备") }
    }

    SectionCard(title = "外观") {
        Row(
            modifier = Modifier.fillMaxWidth(),
            horizontalArrangement = Arrangement.spacedBy(8.dp),
        ) {
            ThemeOption(ThemeMode.System, "跟随系统")
            ThemeOption(ThemeMode.Light, "浅色")
            ThemeOption(ThemeMode.Dark, "深色")
        }
        Text(
            when (AppPrefs.themeMode) {
                ThemeMode.System -> "跟随系统深色开关（Android 10+），系统切换时界面自动跟随"
                else -> "已手动指定外观；选「跟随系统」可恢复自动切换"
            },
            style = MaterialTheme.typography.bodySmall,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
        )
    }

    // Debug 模式：一键开关全栈日志，导出到任意目录（SAF 选择）
    val exportLauncher = rememberLauncherForActivityResult(
        ActivityResultContracts.OpenDocumentTree(),
    ) { uri ->
        if (uri == null) return@rememberLauncherForActivityResult
        val n = copyDebugExportToTree(ctx, uri)
        val tip = when {
            n == null -> "导出失败（日志目录不可读）"
            n == 0 -> "没有可导出的日志文件"
            else -> "已导出 $n 个日志文件"
        }
        Toast.makeText(ctx, tip, Toast.LENGTH_LONG).show()
    }
    SectionCard(title = "开发者选项") {
        Row(
            modifier = Modifier.fillMaxWidth(),
            horizontalArrangement = Arrangement.SpaceBetween,
            verticalAlignment = Alignment.CenterVertically,
        ) {
            Column(Modifier.weight(1f)) {
                Text("Debug 模式", style = MaterialTheme.typography.titleSmall)
                Text(
                    if (debugOn) {
                        "已开启：全栈运行日志落盘（app 私有 Logs 目录）"
                    } else {
                        "开启后持续记录全栈运行状态与错误，便于定位问题"
                    },
                    style = MaterialTheme.typography.bodySmall,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
            }
            Switch(checked = debugOn, onCheckedChange = { LinkxRuntime.setDebugEnabled(it) })
        }
        Text(
            "日志含配对与设备信息（不做脱敏），导出后请仅交给可信方。",
            style = MaterialTheme.typography.bodySmall,
            color = MaterialTheme.colorScheme.error,
        )
        OutlinedButton(onClick = { exportLauncher.launch(null) }) { Text("导出 Debug 日志") }
    }

    SectionCard(title = "关于") {
        KeyValueRow("Core 版本", runCatching { NativeCore.nativeVersion() }.getOrDefault("—"))
        KeyValueRow("对端设备指纹", peerFp ?: "—")
    }

    SectionCard(title = "隐私") {
        Text("数据仅在局域网内点对点传输，不经云端、不留存", style = MaterialTheme.typography.bodyMedium)
        Text(
            "通知读取仅在手机端本地进行，正文按敏感级别可选不同步",
            style = MaterialTheme.typography.bodySmall,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
        )
    }

    if (confirmUnbind) {
        AlertDialog(
            onDismissRequest = { confirmUnbind = false },
            title = { Text("解绑设备") },
            text = {
                Text("解绑后将清除本机信任的对端指纹并断开当前会话，下次连接需重新配对。")
            },
            confirmButton = {
                TextButton(onClick = {
                    confirmUnbind = false
                    LinkxRuntime.unbind()
                }) { Text("解绑") }
            },
            dismissButton = {
                TextButton(onClick = { confirmUnbind = false }) { Text("取消") }
            },
        )
    }
}

/**
 * 把 Debug 日志导出目录里的文件逐个写入用户选定的 SAF 目录，返回写入数（准备阶段失败 = null）。
 * 日志**不脱敏**：去向必须由用户显式选择（系统目录选择器），不做后台静默落盘。
 */
private fun copyDebugExportToTree(ctx: Context, treeUri: Uri): Int? {
    val src = LinkxRuntime.prepareDebugExport() ?: return null
    val files = src.listFiles()?.filter { it.isFile } ?: return null
    if (files.isEmpty()) return 0
    val resolver = ctx.contentResolver
    val parent = DocumentsContract.buildDocumentUriUsingTree(
        treeUri,
        DocumentsContract.getTreeDocumentId(treeUri),
    )
    var written = 0
    for (f in files) {
        runCatching {
            val target = DocumentsContract.createDocument(
                resolver,
                parent,
                "application/octet-stream",
                f.name,
            ) ?: return@runCatching
            resolver.openOutputStream(target)?.use { out ->
                f.inputStream().use { input -> input.copyTo(out) }
            }
            written++
        }.onFailure { Log.w("LinkX.Export", "写入 ${f.name} 失败", it) }
    }
    return written
}

/** 主题三选一（选中态用实心按钮，未选中用描边按钮；等分宽度） */
@Composable
private fun RowScope.ThemeOption(mode: ThemeMode, label: String) {
    val active = AppPrefs.themeMode == mode
    val modifier = Modifier.weight(1f)
    val padding = PaddingValues(horizontal = 4.dp)
    if (active) {
        Button(onClick = { AppPrefs.setTheme(mode) }, modifier = modifier, contentPadding = padding) {
            Text(label, maxLines = 1)
        }
    } else {
        OutlinedButton(onClick = { AppPrefs.setTheme(mode) }, modifier = modifier, contentPadding = padding) {
            Text(label, maxLines = 1)
        }
    }
}

// ---------- 文案 / 权限 ----------

private fun stateLabel(state: Int): String = when (state) {
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
