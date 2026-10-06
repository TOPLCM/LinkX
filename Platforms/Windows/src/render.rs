//! 自绘渲染：Win32 + GDI 逐帧画到后备缓冲，只有一条渲染路径。
//!
//! 资源约定：画刷 / 画笔 / 字体一律缓存，**绝不每帧新建 GDI 对象**（GDI 句柄上限 ~10k，逐帧新建必泄漏）；
//! 设备上下文由 `window.rs` 通过 `BeginPaint/EndPaint` 取得。
//! 版式常量全是逻辑像素（96dpi 基准），绘制与命中判定都经 [`Env::px`] 换算成物理像素（Per-Monitor V2），
//! 并共用同一组几何函数——"画这里、点那里"的错位就是这么防住的。
//! 颜色全部取自 [`Palette`]（明/暗两套）；字体名跟随系统 UI 字体（[theme::system_font_face]）；图标矢量描边（见 [crate::icons]）。
//! 动效（悬停高亮、强调条生长、呼吸脉冲）由 `window.rs` 定时器驱动重绘，[wants_animation] 决定是否需要继续刷帧。

#[cfg(windows)]
use std::cell::{Cell, RefCell};
#[cfg(windows)]
use std::ptr::null_mut;

#[cfg(windows)]
#[cfg(windows)]
use linkx_session::engine::state_code;
#[cfg(windows)]
use windows::core::PCWSTR;
#[cfg(windows)]
use windows::Win32::Foundation::{COLORREF, HWND, RECT, SIZE};
#[cfg(windows)]
use windows::Win32::Graphics::Gdi::{
    Arc, CreateCompatibleDC, CreateFontW, DeleteObject, Ellipse, FillRect, GetTextExtentPoint32W,
    RoundRect, SelectObject, SetBkMode, SetStretchBltMode, SetTextColor, StretchBlt, TextOutW,
    CLEARTYPE_QUALITY, CLIP_DEFAULT_PRECIS, DEFAULT_CHARSET, DEFAULT_PITCH, FF_DONTCARE, FW_BOLD,
    FW_NORMAL, FW_SEMIBOLD, HALFTONE, HBITMAP, HDC, HFONT, HGDIOBJ, OUT_TT_PRECIS, SRCCOPY,
    TRANSPARENT,
};

#[cfg(windows)]
use crate::icons::{self, Icon};
#[cfg(windows)]
use crate::settings::Theme;
#[cfg(windows)]
use crate::state::{UiState, COPIED_HINT_TTL, REPLY_HINT_TTL};
#[cfg(windows)]
use crate::theme::{self, Env};

// ---------- 逻辑版式常量（96dpi 基准；绘制/命中共用，经 Env::px 缩放）----------

#[cfg(windows)]
const NAV_W: i32 = 180;
#[cfg(windows)]
const NAV_ITEM_H: i32 = 44;
#[cfg(windows)]
const NAV_Y0: i32 = 84;
#[cfg(windows)]
/// 全应用统一的界面图标边长（逻辑像素）：所有图标同尺寸、不要小——任何地方都不得再自带别的尺寸。
const NAV_ICON: i32 = 28;
#[cfg(windows)]
const NAV_ICON_X: i32 = 20;
#[cfg(windows)]
const NAV_TEXT_X: i32 = 54;
/// 导航项：`(展示名, 图标, 页签号)`。**数组顺序 = 侧栏显示顺序**，第三项才是 `TAB_*` 号。
/// 页签号一经分配就不再改动（截图脚本与文档按号取页）。图标尺寸一律走 `NAV_ICON`，各项同源
/// 同网格，不会出现某项比别人大/小一圈。
#[cfg(windows)]
const NAV_ITEMS: [(&str, Icon, usize); 9] = [
    ("连接", Icon::Link, TAB_CONNECT),
    ("通知", Icon::Bell, TAB_NOTIFY),
    ("剪贴板", Icon::Clipboard, TAB_CLIP),
    ("文件", Icon::Folder, TAB_FILES),
    ("相册", Icon::Album, TAB_ALBUM),
    ("媒体", Icon::Media, TAB_MEDIA),
    ("功能", Icon::Blocks, TAB_FEATURES),
    ("设置", Icon::Settings, TAB_SETTINGS),
    ("关于", Icon::Info, TAB_ABOUT),
];
/// 页签下标（与 `NAV_ITEMS` 的第三项一一对应；改动必须同步 `hit_test` / `hover_at` / `paint_gdi`）
#[cfg(windows)]
pub(crate) const TAB_CONNECT: usize = 0;
#[cfg(windows)]
pub(crate) const TAB_NOTIFY: usize = 1;
#[cfg(windows)]
pub(crate) const TAB_CLIP: usize = 2;
#[cfg(windows)]
pub(crate) const TAB_FILES: usize = 3;
#[cfg(windows)]
pub(crate) const TAB_MEDIA: usize = 4;
#[cfg(windows)]
pub(crate) const TAB_FEATURES: usize = 5;
#[cfg(windows)]
pub(crate) const TAB_SETTINGS: usize = 6;
/// 相册页（图片互传）页签号
#[cfg(windows)]
pub(crate) const TAB_ALBUM: usize = 7;
/// 关于页页签号：必须参与 `hit_test` / `hover_at` / `paint_gdi` 三处同源分派，且 `state.rs` 的页签钳制要认它（漏了会出现"点得到、画不出"）。
#[cfg(windows)]
pub(crate) const TAB_ABOUT: usize = 8;

#[cfg(windows)]
const CONTENT_L: i32 = NAV_W + 28;
#[cfg(windows)]
const CONTENT_R_PAD: i32 = 32;
#[cfg(windows)]
const TITLE_Y: i32 = 26;
#[cfg(windows)]
const SUBTITLE_Y: i32 = 58;
#[cfg(windows)]
const DEVICE_Y0: i32 = 92;
#[cfg(windows)]
const DEVICE_ROW_H: i32 = 38;
#[cfg(windows)]
const NOTIFY_Y0: i32 = 92;
#[cfg(windows)]
const NOTIFY_ITEM_H: i32 = 66;
/// 列表行的设计宽度（文件页操作行/传输行、设置页已绑定设备行共用这一档：要放得下路径 + 两个按钮）。
/// **通知页不用它**——那页只有一个右边界，见 `notify_right`
#[cfg(windows)]
const LIST_ROW_W: i32 = 700;
/// 连接页设备行的设计宽度（比文件页那一档窄一档：它只放名称+地址）
#[cfg(windows)]
const DEVICE_ROW_W: i32 = 600;
/// 版本条占掉的高度（画在 `h - 30`）：可见行数由客户区高度算（`notify_visible_rows`）——原来写死 9 行，第 9 行被它裁掉
#[cfg(windows)]
const NOTIFY_FOOT_H: i32 = 30;
/// 通知行右侧三个文字按钮的槽位高与宽。按钮上**不印验证码数字**：把 8 位码写进按钮会把正文
/// 挤掉半行，而按钮要说清楚的只有"这颗是复制什么的"
#[cfg(windows)]
const NOTIFY_CHIP_H: i32 = 26;
#[cfg(windows)]
const NOTIFY_CODE_W: i32 = 84;
#[cfg(windows)]
const NOTIFY_ALL_W: i32 = 70;
#[cfg(windows)]
const NOTIFY_REPLY_W: i32 = 64;
/// 通知行右侧控件之间的统一间隙（逻辑像素）：两个按钮之间、最右那个与行末时间带之间都是它
#[cfg(windows)]
const NOTIFY_CHIP_GAP: i32 = 6;
/// 行末时间（`09:08:03`）要留出的宽度：按钮排在它左边，标题截断也按这条算
#[cfg(windows)]
const NOTIFY_TIME_W: i32 = 100;
/// 通知页唯一的右边界 = 内容区右沿。**不用 `row_right(LIST_ROW_W)`**：那会把这页钉在 700 逻辑像素上，
/// 窗口拉大后滚动条浮在半空、时间画在窗口边，两个"右界"之间的空隙正好让按钮和文字互相压住。整页只认这一个值。
#[cfg(windows)]
fn notify_right(e: &Env) -> i32 {
    e.logical_w() - CONTENT_R_PAD
}
/// 通知行右侧那一片控件共用的排布右端（两个文字按钮 + 悬停复制图标都从这条线往左摆）：
/// 整片都要让开行末的时间带。以前这两个数在两个函数里各减一遍，漏一项就是用户截图里
/// "按钮压在时间上"
#[cfg(windows)]
fn notify_chips_right(e: &Env) -> i32 {
    notify_right(e) - NOTIFY_TIME_W - NOTIFY_CHIP_GAP
}
#[cfg(windows)]
const REPLY_BAR_Y: i32 = 56;
#[cfg(windows)]
const NOTIFY_SEND_BTN_W: i32 = 68;
#[cfg(windows)]
const _: () = assert!(
    REPLY_BAR_Y + INPUT_H < NOTIFY_Y0,
    "回复条必须落在标题与首行之间，压到列表就要改几何"
);
#[cfg(windows)]
const CLIP_TOGGLE_Y: i32 = 92;
#[cfg(windows)]
const CLIP_SEND_Y: i32 = 140;
#[cfg(windows)]
const CLIP_BTN_W: i32 = 240;
#[cfg(windows)]
const CLIP_BTN_H: i32 = 38;
#[cfg(windows)]
const SET_THEME_Y: i32 = 96;
#[cfg(windows)]
const SET_SEG_W: i32 = 104;
#[cfg(windows)]
const SET_SEG_H: i32 = 34;
#[cfg(windows)]
const SET_TOGGLE_Y0: i32 = 142;
/// 开关行数（**唯一真源**：绘制、命中、以及下方各节的纵位都由它推导）
#[cfg(windows)]
const SET_TOGGLES: usize = 6;
/// 「开机自启动」所在行：数组顺序、命中与悬停都按这个号，改行序只改这一处
#[cfg(windows)]
const SET_AUTOSTART_ROW: usize = 4;
/// 「Debug 模式」所在行（右侧同行还有「导出日志」）
#[cfg(windows)]
const SET_DEBUG_ROW: usize = SET_TOGGLES - 1;
/// 「关闭按钮行为」那一行：与开关同一套行几何，右侧换成一个循环取值的按钮
#[cfg(windows)]
const SET_BEHAVIOR_ROW: usize = SET_TOGGLES;
/// 开关区总行数（绘制、命中、悬停按它遍历）
#[cfg(windows)]
const SET_ROWS: usize = SET_TOGGLES + 1;
/// 行高 = 标题 + 说明 + 呼吸。设置页要在**最小客户区**里画得完，行高与下方各节的间隙一起受
/// 文件末尾那组 `const _` 断言约束——加一行就要动这里，别只改一处
#[cfg(windows)]
const SET_ROW_H: i32 = 42;
/// 说明行相对标题行的下移量
#[cfg(windows)]
const SET_DESC_DY: i32 = 21;
/// 开关列左边界：说明文字一律不得越过这条线（靠人眼保证的话，加一行就叠字）
#[cfg(windows)]
const SET_SWITCH_L: i32 = 380;
/// 「关闭按钮行为」按钮宽度（要装得下最长的一个取值）
#[cfg(windows)]
const SET_BEHAVIOR_BTN_W: i32 = 116;
#[cfg(windows)]
const SW_W: i32 = 46;
#[cfg(windows)]
const SW_H: i32 = 26;
/// 文本输入框高度（文件页 / 设置页共用）
#[cfg(windows)]
const INPUT_H: i32 = 32;
#[cfg(windows)]
const FILE_SEND_LABEL_Y: i32 = 86;
#[cfg(windows)]
const FILE_FIELD_Y: i32 = 106;
#[cfg(windows)]
const FILE_SEND_W: i32 = 112;
#[cfg(windows)]
const FILE_BROWSE_W: i32 = 92;
/// 操作行三个控件之间的间距（输入框吃掉剩余宽度，窗口再窄也不重叠）
#[cfg(windows)]
const FILE_BTN_GAP: i32 = 10;
#[cfg(windows)]
const FILE_INBOX_LABEL_Y: i32 = 150;
#[cfg(windows)]
const FILE_INBOX_Y: i32 = 170;
#[cfg(windows)]
const FILE_LIST_LABEL_Y: i32 = 198;
#[cfg(windows)]
const FILE_ROW_Y0: i32 = 216;
#[cfg(windows)]
/// 行高要同时容下"图标 + 进度条"两层；8 行 × 46 是 606 高客户区不溢出的上限
const FILE_ROW_H: i32 = 46;
#[cfg(windows)]
pub(crate) const FILE_MAX_ROWS: usize = 8;
/// 设置页：设备管理区 / 信息区的纵位**全部由开关区推导**（字面量排布加一行就叠字）；整页按"最小客户区 `LAYOUT_MIN_H` + 页脚 30"倒推，越界由本文件末尾的 `const _` 断言编译期拦下。
/// 开关区每加一行，这里的间隙与 `SET_ROW_H` 就要一起重算——编译器会拦住，但拦不住"挤成一团"。
#[cfg(windows)]
const SET_MANUAL_HINT_Y: i32 = SET_TOGGLE_Y0 + SET_ROWS as i32 * SET_ROW_H + 4;
#[cfg(windows)]
const SET_MANUAL_Y: i32 = SET_MANUAL_HINT_Y + 18;
#[cfg(windows)]
const SET_MANUAL_W: i32 = 220;
#[cfg(windows)]
const SET_MANUAL_BTN_W: i32 = 84;
#[cfg(windows)]
const SET_BOUND_LABEL_Y: i32 = SET_MANUAL_Y + INPUT_H + 12;
#[cfg(windows)]
const SET_BOUND_Y0: i32 = SET_BOUND_LABEL_Y + 18;
#[cfg(windows)]
const SET_BOUND_ROW_H: i32 = 26;
#[cfg(windows)]
const SET_BOUND_MAX: usize = 2;
/// 设置页：底部信息区（版本 + 本机指纹 / 隐私，两行）
#[cfg(windows)]
const SET_INFO_Y: i32 = SET_BOUND_Y0 + SET_BOUND_MAX as i32 * SET_BOUND_ROW_H + 12;
/// 信息区底部（用于"不得压到页脚"的单测）
#[cfg(windows)]
const SET_INFO_BOTTOM: i32 = SET_INFO_Y + 20 + 16;
/// 媒体页：正在播放 + 控制按钮
#[cfg(windows)]
const MEDIA_CARD_Y: i32 = 100;
#[cfg(windows)]
const MEDIA_CARD_H: i32 = 150;
#[cfg(windows)]
const MEDIA_BTN_Y: i32 = 276;
#[cfg(windows)]
const MEDIA_BTN_H: i32 = 44;
#[cfg(windows)]
const MEDIA_BTN_W: i32 = 84;
#[cfg(windows)]
const MEDIA_BTN_GAP: i32 = 12;
#[cfg(windows)]
const MEDIA_VOL_Y: i32 = 352;
/// 媒体页：控制按钮个数（**唯一真源**：绘制用它、命中与悬停按它遍历、window.rs 的下标映射也按它穷尽——多处各写各的，加一个按钮就会错位）
#[cfg(windows)]
const MEDIA_BTN_COUNT: usize = 5;
/// 功能页：模块开关区
#[cfg(windows)]
const FEAT_Y0: i32 = 104;
#[cfg(windows)]
const FEAT_ROW_H: i32 = 62;
/// 功能页布局一律由模块个数推导：手工预留的行数跟不上增删时，多出的行标题会直接压在摘要行上（真机自绘才会这样叠字，单测抓不到）。
#[cfg(windows)]
const FEAT_ROWS_END: i32 = FEAT_Y0 + crate::features::ALL.len() as i32 * FEAT_ROW_H;
#[cfg(windows)]
const FEAT_SUMMARY_Y: i32 = FEAT_ROWS_END - 6;
/// 待重启提示条：高度按**最多 ALL.len() 条**改动预留，条目少时只是留白——几何就此与 `lines` 无关，命中判定、悬停、绘制不会算出三种尺寸。
#[cfg(windows)]
const FEAT_BANNER_Y: i32 = FEAT_SUMMARY_Y + 40;
#[cfg(windows)]
const FEAT_BANNER_H: i32 = 38 + crate::features::ALL.len() as i32 * 18 + 14;
#[cfg(windows)]
const FEAT_BANNER_W: i32 = 470;
#[cfg(windows)]
const FEAT_BTN_W: i32 = 104;
#[cfg(windows)]
const FEAT_BTN_H: i32 = 32;
/// 重启确认弹窗（自绘模态：整窗只有这两个按钮可点）
#[cfg(windows)]
const MODAL_W: i32 = 460;
/// 弹窗高度同样按最多 ALL.len() 条改动预留（标题 20 + 正文 62 + 条目 + 间距 + 按钮 36 + 下边距 20）：写死高度时，多出的条目会盖到按钮上。
#[cfg(windows)]
const MODAL_H: i32 = 82 + crate::features::ALL.len() as i32 * 20 + 10 + 36 + 20;
/// 关闭询问弹窗（自绘模态：三个按钮 + 一个勾选，整窗只有这四处分派命中）
#[cfg(windows)]
const CLOSE_MODAL_W: i32 = 460;
/// 上距 20 + 标题 26 + 8 + 正文 20 + 10 + 按钮 36 + 14 + 勾选 22 + 下距 20
#[cfg(windows)]
const CLOSE_MODAL_H: i32 = 176;
/// 相册页纵向：工具行 → 状态行 → 网格 → 详情行 → 页脚。
/// 网格一律由「格子边长 + 间距」推导，行数列数按客户区算出来——三处（绘制、命中、悬停）共用
/// `album_geom`，加一列不会变成"画得下点不到"。
#[cfg(windows)]
const ALBUM_TOOLBAR_Y: i32 = 92;
#[cfg(windows)]
const ALBUM_BTN_H: i32 = 32;
/// 工具行按钮宽度（**按槽位固定**：标签文字长短不一，宽度若随文字变，命中矩形就会在"全选/取消全选"切换那一刻搬家）。
#[cfg(windows)]
const ALBUM_BTN_W: [i32; 5] = [64, 72, 72, 96, 124];
#[cfg(windows)]
const ALBUM_BTN_GAP: i32 = 8;
#[cfg(windows)]
const ALBUM_STATUS_Y: i32 = ALBUM_TOOLBAR_Y + ALBUM_BTN_H + 12;
#[cfg(windows)]
const ALBUM_GRID_Y: i32 = ALBUM_STATUS_Y + 26;
#[cfg(windows)]
const ALBUM_CELL: i32 = 108;
#[cfg(windows)]
const ALBUM_GAP: i32 = 14;
#[cfg(windows)]
const ALBUM_DETAIL_H: i32 = 22;
#[cfg(windows)]
const ALBUM_FOOT_H: i32 = 30;

#[cfg(windows)]
const ANIM_MS: u128 = 220;

/// 命中目标（由 `hit_test` 返回，window.rs 据此分派）
#[cfg(windows)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HitTarget {
    Nav(usize),
    ConnectDevice(u64),
    SasConfirm,
    SasReject,
    SendLocalClip,
    ToggleClipSync,
    CopyNotification(usize),
    /// 通知行上的「复制验证码」小按钮（只复制抽出来的数字码）；与整行点击的
    /// `CopyNotification`（复制整条正文）是两颗并排的按钮
    CopyNotificationCode(usize),
    /// 通知行上的「回复」小按钮：选中这条，顶部回复条出现
    ReplyNotification(usize),
    /// 回复条上的输入框 / 「发送」按钮
    FocusReplyInput,
    SendReply,
    ThemeSet(Theme),
    ToggleToast,
    ToggleToastContent,
    /// 开关「开机后自动连接」
    ToggleAutoConnect,
    /// 开关「开机自启动」：注册表读写由调用方**出锁后**做
    ToggleAutostart,
    /// 「关闭按钮行为」：点一下在三个取值间循环
    CycleCloseBehavior,
    FocusSendPath,
    SendFile,
    BrowseFile,
    CancelFile(usize),
    ChooseInbox,
    FocusManualIp,
    ManualIpConnect,
    UnbindDevice(usize),
    /// 信任对端新身份并重走配对 + SAS 复核
    AcceptNewIdentity,
    /// 拒绝对端新身份（断开）
    RejectNewIdentity,
    ToggleDebug,
    /// 导出 Debug 日志（系统「选择文件夹」）
    ExportDebug,
    /// 媒体控制按钮（0=上一首 1=播放/暂停 2=下一首 3=音量− 4=音量+）
    MediaControl(usize),
    /// 切换某个功能模块的"想要"状态（功能页）
    ToggleFeature(crate::features::Module),
    /// 功能页上的「重新启动」入口（等价于弹窗里的立即重启）
    RestartApp,
    /// 重启确认弹窗「重新启动」
    ModalRestartNow,
    /// 重启确认弹窗「稍后启动」
    ModalRestartLater,
    /// 关闭询问弹窗的三个按钮（回车/Esc 也走同一批目标）
    ModalCloseMinimize,
    ModalCloseExit,
    ModalCloseCancel,
    /// 关闭询问弹窗上的「记住我的选择，不再询问」勾选
    ModalCloseRemember,
    /// 相册工具行按钮（0=刷新 1=上一页 2=下一页 3=全选/取消全选 4=导出）
    AlbumTool(usize),
    /// 点某一格（列表下标；切换选中态）：只在按下时记下、**抬起时才切换**——同一次按住拖动是"拖出"，按下就改选中会让拖拽结束时多选状态莫名其妙地变一位。
    AlbumCell(usize),
    /// 相册滚动条：按下轨道 / 拖动滑块 → 新的基准行
    AlbumScroll(usize),
    /// 通知列表滚动条：按下轨道 / 拖动滑块 → 新的起始条下标
    NotifyScroll(usize),
}

// ---------- 几何（绘制与命中同源；入参为逻辑坐标，返回物理像素矩形）----------

#[cfg(windows)]
fn rect(l: i32, t: i32, r: i32, b: i32) -> RECT {
    RECT {
        left: l,
        top: t,
        right: r,
        bottom: b,
    }
}

/// 逻辑矩形 → 物理像素矩形
#[cfg(windows)]
fn lrect(e: &Env, l: i32, t: i32, r: i32, b: i32) -> RECT {
    rect(e.px(l), e.px(t), e.px(r), e.px(b))
}

/// 内容行至少要有这么宽（逻辑像素），否则通知行右侧的「复制验证码」按钮会挤掉正文。
#[cfg(windows)]
const MIN_ROW_W: i32 = 520;
/// 布局能正常绘制的最小客户区（**逻辑**像素）。`window.rs` 用它挡 `WM_GETMINMAXINFO`：不挡住的话
/// 用户能把窗口压到侧栏最后一项消失、内容画到客户区外。
/// 高度取设置页那套纵向排布到底部页脚所需（与下方 const 断言同口径）。
#[cfg(windows)]
pub(crate) const LAYOUT_MIN_W: i32 = CONTENT_L + MIN_ROW_W + CONTENT_R_PAD;
#[cfg(windows)]
pub(crate) const LAYOUT_MIN_H: i32 = 658;

/// 最小宽度必须真能装下 `MIN_ROW_W` 一档内容行，否则挡了个错的数
#[cfg(windows)]
const _: () = assert!(LAYOUT_MIN_W - CONTENT_L - CONTENT_R_PAD >= MIN_ROW_W);
#[cfg(windows)]
const _: () = assert!(
    LAYOUT_MIN_H >= NAV_Y0 + 8 * NAV_ITEM_H + 24,
    "最小高度装不下 8 个导航项"
);

/// 内容行的右边界（逻辑像素）：跟随客户区宽度，但不超过该页的设计宽度 `cap`。
/// 行宽若写成 `CONTENT_L + 常量`，窗口比常量窄时行**画到客户区外**、更宽时又不长——"元素不跟着
/// 窗口调整"就是它。下限只兜住 `Env` 尺寸异常（win_w=0）：宁可留白也不能算出反向矩形。
#[cfg(windows)]
fn row_right(e: &Env, cap: i32) -> i32 {
    let client_w = e.logical_w();
    (CONTENT_L + cap)
        .min(client_w - CONTENT_R_PAD)
        .max(CONTENT_L + 240)
}

/// 页签对应的功能模块（`None` = 恒显示）。关掉模块 → 这一页**根本不建**，而不是灰掉或留一张空页。
/// 按**页签号**而不是数组下标归属：显示顺序与号码解耦后，插一个导航项不会把归属错位到别的页上
/// （那种错法是"关掉 A 却隐藏了 B"）。
#[cfg(windows)]
fn nav_module(tab: usize) -> Option<crate::features::Module> {
    use crate::features::Module;
    match tab {
        TAB_NOTIFY => Some(Module::Notifications),
        TAB_CLIP => Some(Module::Clipboard),
        TAB_FILES => Some(Module::FileTransfer),
        TAB_MEDIA => Some(Module::MediaControl),
        TAB_ALBUM => Some(Module::Album),
        _ => None,
    }
}

/// 本次启动可见的导航项（值是 `TAB_*` 页签号，顺序即侧栏展示顺序）。命中判定、悬停判定、绘制三处必须共用它，否则"点第 3 格选中的是第 4 页"。
#[cfg(windows)]
fn nav_visible() -> Vec<usize> {
    NAV_ITEMS
        .iter()
        .map(|(_, _, tab)| *tab)
        .filter(|tab| match nav_module(*tab) {
            Some(m) => crate::features::enabled(m),
            None => true,
        })
        .collect()
}

/// 页签号 → 导航项（显示顺序无关的查找；绘制用，命中与悬停用 `nav_visible` 的行号）
#[cfg(windows)]
fn nav_entry(tab: usize) -> Option<(&'static str, Icon)> {
    NAV_ITEMS
        .iter()
        .find(|(_, _, t)| *t == tab)
        .map(|(l, i, _)| (*l, *i))
}

#[cfg(windows)]
fn nav_item_rect(e: &Env, i: usize) -> RECT {
    let y = NAV_Y0 + i as i32 * NAV_ITEM_H;
    lrect(e, 0, y, NAV_W, y + NAV_ITEM_H)
}

#[cfg(windows)]
fn device_row_rect(e: &Env, i: usize) -> RECT {
    let y = DEVICE_Y0 + i as i32 * DEVICE_ROW_H;
    let right = row_right(e, DEVICE_ROW_W);
    lrect(e, CONTENT_L, y, right, y + DEVICE_ROW_H)
}

#[cfg(windows)]
/// 通知行右侧的小按钮（「复制验证码」/「复制全文」/「回复」）：全部按钮共用一条"从右往左排"的
/// 算法，且**绘制、命中、悬停三处都只调这里** —— 分成两处各算一遍坐标就是"看着能点、点不到"的根因。
/// 返回 `(验证码, 回复, 复制全文)`；不该出现的那颗为 `None`，不占位。
/// 有码时给两颗复制（只复制数字 / 复制整条），没码时只剩「复制全文」
fn notification_chips(
    e: &Env,
    i: usize,
    has_code: bool,
    can_reply: bool,
) -> (Option<RECT>, Option<RECT>, RECT) {
    let y = NOTIFY_Y0 + i as i32 * NOTIFY_ITEM_H;
    let top = y + (NOTIFY_ITEM_H - NOTIFY_CHIP_H) / 2;
    let mut x = notify_chips_right(e);
    let reply = if can_reply {
        x -= NOTIFY_REPLY_W;
        Some(lrect(e, x, top, x + NOTIFY_REPLY_W, top + NOTIFY_CHIP_H))
    } else {
        None
    };
    if reply.is_some() {
        x -= NOTIFY_CHIP_GAP;
    }
    let code = if has_code {
        x -= NOTIFY_CODE_W;
        Some(lrect(e, x, top, x + NOTIFY_CODE_W, top + NOTIFY_CHIP_H))
    } else {
        None
    };
    if code.is_some() {
        x -= NOTIFY_CHIP_GAP;
    }
    let all = lrect(e, x - NOTIFY_ALL_W, top, x, top + NOTIFY_CHIP_H);
    (code, reply, all)
}

#[cfg(windows)]
/// 两行文字的右界：最左那颗按钮之前。按钮锚在行右界、窗口右界在它右边，"截到窗口边"会从按钮
/// 底下穿过 —— 谁后画谁赢，所以正文必须先让位
fn notify_text_right(e: &Env, leftmost_chip: i32, right: i32) -> i32 {
    (leftmost_chip - e.px(8)).min(right)
}

#[cfg(windows)]
/// 通知页顶部回复条：输入框 + 「发送」。只在选中某条可回复通知时出现；压在标题与首行的空档上，
/// 列表几何不动。复制那颗按钮**不放在这里** —— 它是"这条通知"的动作，不是"正在写的这句回复"的动作
fn notify_reply_rects(e: &Env) -> (RECT, RECT) {
    let right = notify_right(e);
    let btn_l = right - NOTIFY_SEND_BTN_W;
    (
        lrect(e, CONTENT_L, REPLY_BAR_Y, btn_l - 8, REPLY_BAR_Y + INPUT_H),
        lrect(e, btn_l, REPLY_BAR_Y, right, REPLY_BAR_Y + INPUT_H),
    )
}

/// 这条通知里能不能抽出验证码；两端共用核心层规则（`linkx_session::code_extract`）。
#[cfg(windows)]
fn notify_code_of(item: &crate::state::NotificationItem) -> Option<String> {
    linkx_session::code_extract::extract_code(&item.title, &item.text).map(|c| c.digits)
}

#[cfg(windows)]
fn notification_row_rect(e: &Env, i: usize) -> RECT {
    let y = NOTIFY_Y0 + i as i32 * NOTIFY_ITEM_H;
    lrect(e, CONTENT_L, y, notify_right(e), y + NOTIFY_ITEM_H)
}

/// 这一帧画得下的行数：窗口矮就少画几行，画不下的靠滚动条到（版本条占掉的高度与它的来历见 `NOTIFY_FOOT_H`）。
#[cfg(windows)]
fn notify_visible_rows(e: &Env) -> usize {
    let lh = e.logical_h();
    let avail = (lh - NOTIFY_Y0 - NOTIFY_FOOT_H - 6).max(NOTIFY_ITEM_H);
    (avail / NOTIFY_ITEM_H) as usize
}

/// 能滚到的最大起始下标（最后一屏仍要贴着列表末尾）
#[cfg(windows)]
fn notify_max_scroll(e: &Env, st: &UiState) -> usize {
    st.notifications
        .len()
        .saturating_sub(notify_visible_rows(e))
}

/// 这一帧的起始条目：把界面里的滚动值夹进合法区间（改窗口大小、列表变短都靠它兜住）
#[cfg(windows)]
fn notify_base(e: &Env, st: &UiState) -> usize {
    st.notify_scroll.min(notify_max_scroll(e, st))
}

#[cfg(windows)]
fn notify_bar_track(e: &Env) -> RECT {
    let lh = e.logical_h();
    let bottom = (lh - NOTIFY_FOOT_H - 6).max(NOTIFY_Y0 + NOTIFY_ITEM_H);
    // 轨道贴在内容右沿之外的留白里：跟着窗口走，不会被 700 的封顶钉在窗口中间
    let x = notify_right(e) + 8;
    lrect(e, x, NOTIFY_Y0, x + ALBUM_BAR_W, bottom)
}

/// 滚动条滑块：高度按"可见行 / 总条数"，位置按当前起始下标。画得下就返回 `None`。
#[cfg(windows)]
fn notify_bar_thumb(e: &Env, st: &UiState) -> Option<RECT> {
    let rows = notify_visible_rows(e);
    if st.notifications.len() <= rows {
        return None;
    }
    let track = notify_bar_track(e);
    let track_h = (track.bottom - track.top).max(1);
    let knob_h = ((track_h as f32) * (rows as f32 / st.notifications.len() as f32)).round() as i32;
    let knob_h = knob_h.clamp(e.px(24), track_h);
    let max_scroll = notify_max_scroll(e, st).max(1) as f32;
    let pos = (notify_base(e, st) as f32 / max_scroll).clamp(0.0, 1.0);
    let top = track.top + ((track_h - knob_h) as f32 * pos).round() as i32;
    // 轨道已经是物理像素，这里不能再过一遍 `lrect`（同相册那条坑：再乘一次缩放就画到客户区外）
    Some(rect(
        track.left + e.px(2),
        top,
        track.right - e.px(2),
        top + knob_h,
    ))
}

/// 把轨道内的 y 换算成起始下标（点轨道与拖滑块共用）
#[cfg(windows)]
fn notify_scroll_at(e: &Env, st: &UiState, y: i32) -> usize {
    let max_scroll = notify_max_scroll(e, st);
    let track = notify_bar_track(e);
    let track_h = (track.bottom - track.top).max(1) as f32;
    let frac = ((y - track.top) as f32 / track_h).clamp(0.0, 1.0);
    (frac * max_scroll as f32).round() as usize
}

/// 拖滚动条：把轨道内的 y 落到**当前这一页**的滚动字段上，返回画面是否真的变了。
/// 相册与通知共用一条路由：各写一遍"按下/拖动该改哪个字段"迟早漂成"拖 A 页滚 B 页"。
#[cfg(windows)]
pub(crate) fn drag_scroll_bar(hwnd: HWND, st: &mut UiState, y: i32) -> bool {
    let e = theme::detect(hwnd, st.theme);
    match st.active_tab {
        TAB_ALBUM => {
            let row = album_row_at_bar_y(&e, st, y);
            st.album.scroll_row != row && {
                st.album.scroll_row = row;
                true
            }
        }
        TAB_NOTIFY => {
            let row = notify_scroll_at(&e, st, y);
            st.notify_scroll != row && {
                st.notify_scroll = row;
                true
            }
        }
        _ => false,
    }
}

/// 滚轮一格：返回滚动字段是否变了
#[cfg(windows)]
pub(crate) fn wheel_scroll_bar(hwnd: HWND, st: &mut UiState, up: bool) -> bool {
    let e = theme::detect(hwnd, st.theme);
    let step = if up { -1i32 } else { 1 };
    match st.active_tab {
        TAB_ALBUM => {
            if st.album.items.is_empty() {
                return false;
            }
            let max = album_max_scroll_row(&e, st) as i32;
            let next = (st.album.scroll_row as i32 + step).clamp(0, max) as usize;
            st.album.scroll_row != next && {
                st.album.scroll_row = next;
                true
            }
        }
        TAB_NOTIFY => {
            if st.notifications.is_empty() {
                return false;
            }
            let max = notify_max_scroll(&e, st) as i32;
            let next = (st.notify_scroll as i32 + step).clamp(0, max) as usize;
            st.notify_scroll != next && {
                st.notify_scroll = next;
                true
            }
        }
        _ => false,
    }
}

/// SAS 区 [确认一致][不一致] 两按钮矩形（随设备行数下移；绘制与命中同源）
#[cfg(windows)]
fn sas_buttons(e: &Env, device_count: usize) -> (RECT, RECT) {
    let y = sas_btns_y(device_count);
    (
        lrect(e, CONTENT_L, y, CONTENT_L + 112, y + 38),
        lrect(e, CONTENT_L + 128, y, CONTENT_L + 240, y + 38),
    )
}

#[cfg(windows)]
fn sas_block_y(device_count: usize) -> i32 {
    DEVICE_Y0 + device_count.max(1) as i32 * DEVICE_ROW_H + 28
}

#[cfg(windows)]
fn sas_hint_y(device_count: usize) -> i32 {
    sas_block_y(device_count) + 6
}

#[cfg(windows)]
fn sas_card_y(device_count: usize) -> i32 {
    sas_block_y(device_count) + 30
}

#[cfg(windows)]
fn sas_btns_y(device_count: usize) -> i32 {
    sas_block_y(device_count) + 112
}

#[cfg(windows)]
fn clip_toggle_rect(e: &Env) -> RECT {
    lrect(
        e,
        CONTENT_L,
        CLIP_TOGGLE_Y,
        CONTENT_L + CLIP_BTN_W,
        CLIP_TOGGLE_Y + CLIP_BTN_H,
    )
}

#[cfg(windows)]
fn clip_send_rect(e: &Env) -> RECT {
    lrect(
        e,
        CONTENT_L,
        CLIP_SEND_Y,
        CONTENT_L + CLIP_BTN_W,
        CLIP_SEND_Y + CLIP_BTN_H,
    )
}

#[cfg(windows)]
fn theme_seg_rects(e: &Env) -> [RECT; 3] {
    let mut out = [rect(0, 0, 0, 0); 3];
    for (i, r) in out.iter_mut().enumerate() {
        let x = CONTENT_L + i as i32 * (SET_SEG_W + 8);
        *r = lrect(e, x, SET_THEME_Y, x + SET_SEG_W, SET_THEME_Y + SET_SEG_H);
    }
    out
}

/// 设置页第 `row` 个开关矩形
#[cfg(windows)]
fn set_switch_rect(e: &Env, row: usize) -> RECT {
    let y = SET_TOGGLE_Y0 + row as i32 * SET_ROW_H + 2;
    lrect(
        e,
        CONTENT_L + SET_SWITCH_L,
        y,
        CONTENT_L + SET_SWITCH_L + SW_W,
        y + SW_H,
    )
}

/// 文件页操作行的右界：与列表行同一自适应口径（窗口再窄也不越出内容区）
#[cfg(windows)]
fn file_ops_right(e: &Env) -> i32 {
    row_right(e, LIST_ROW_W)
}

/// 文件页：路径输入框（吃掉「选择…」「发送到手机」两个按钮之外的剩余宽度）
#[cfg(windows)]
fn file_path_rect(e: &Env) -> RECT {
    lrect(
        e,
        CONTENT_L,
        FILE_FIELD_Y,
        file_ops_right(e) - FILE_SEND_W - FILE_BROWSE_W - FILE_BTN_GAP * 2,
        FILE_FIELD_Y + INPUT_H,
    )
}

/// 文件页：「选择…」按钮（弹系统文件对话框）
#[cfg(windows)]
fn file_browse_rect(e: &Env) -> RECT {
    let r = file_ops_right(e) - FILE_SEND_W - FILE_BTN_GAP;
    lrect(
        e,
        r - FILE_BROWSE_W,
        FILE_FIELD_Y,
        r,
        FILE_FIELD_Y + INPUT_H,
    )
}

#[cfg(windows)]
fn file_send_rect(e: &Env) -> RECT {
    let r = file_ops_right(e);
    lrect(e, r - FILE_SEND_W, FILE_FIELD_Y, r, FILE_FIELD_Y + INPUT_H)
}

#[cfg(windows)]
fn file_row_rect(e: &Env, i: usize) -> RECT {
    let y = FILE_ROW_Y0 + i as i32 * FILE_ROW_H;
    lrect(e, CONTENT_L, y, row_right(e, LIST_ROW_W), y + FILE_ROW_H)
}

/// 文件页：在途行上的「取消」按钮尺寸（行高 46 里居中，不压进度条）
#[cfg(windows)]
const FILE_CANCEL_W: i32 = 56;
#[cfg(windows)]
const FILE_CANCEL_H: i32 = 24;

/// 文件页：第 i 行的「取消」按钮矩形。单独成函数让**绘制与命中判定共用同一份几何**——行内控件
/// 各处各算坐标，就是"看得见点不到 / 点到了看不见"这一类返工的根因。只有 `is_cancellable()` 的行才画、才应答。
#[cfg(windows)]
fn file_cancel_rect(e: &Env, i: usize) -> RECT {
    let y = FILE_ROW_Y0 + i as i32 * FILE_ROW_H;
    let r = row_right(e, LIST_ROW_W) - 12;
    let top = y + (FILE_ROW_H - FILE_CANCEL_H) / 2;
    lrect(e, r - FILE_CANCEL_W, top, r, top + FILE_CANCEL_H)
}

#[cfg(windows)]
fn inbox_pick_rect(e: &Env) -> RECT {
    // lrect 吃**逻辑**坐标（内部再乘 DPI 缩放），这里不能再 e.px() 一次
    let right = row_right(e, LIST_ROW_W) - 12;
    let top = FILE_INBOX_Y - 3;
    lrect(e, right - 82, top, right, top + 21)
}

/// 文件页可见行数：绘制、命中、悬停三处必须取同一个数，否则第 9 行能点到却看不见。
#[cfg(windows)]
fn file_rows_visible(st: &UiState) -> usize {
    st.file_tasks.len().min(FILE_MAX_ROWS)
}

/// 第 i 行「取消」的可点矩形；`None` = 这一行不该出现「取消」（终态行 / 等回执行 / 无此行）
#[cfg(windows)]
fn file_cancel_hit_rect(e: &Env, st: &UiState, i: usize) -> Option<RECT> {
    st.file_tasks
        .get(i)
        .filter(|t| t.is_cancellable())
        .map(|_| file_cancel_rect(e, i))
}

/// 命中的「取消」行下标：hit_test 与 hover_at 共用同一个入口，三处同源
#[cfg(windows)]
fn file_cancel_row_at(e: &Env, st: &UiState, x: i32, y: i32) -> Option<usize> {
    (0..file_rows_visible(st))
        .find(|i| file_cancel_hit_rect(e, st, *i).is_some_and(|r| point_in(&r, x, y)))
}

#[cfg(windows)]
fn manual_ip_rect(e: &Env) -> RECT {
    lrect(
        e,
        CONTENT_L,
        SET_MANUAL_Y,
        CONTENT_L + SET_MANUAL_W,
        SET_MANUAL_Y + INPUT_H,
    )
}

#[cfg(windows)]
fn manual_ip_btn_rect(e: &Env) -> RECT {
    let l = CONTENT_L + SET_MANUAL_W + 12;
    lrect(
        e,
        l,
        SET_MANUAL_Y,
        l + SET_MANUAL_BTN_W,
        SET_MANUAL_Y + INPUT_H,
    )
}

/// 设置页：第 i 行已绑定设备（整行可点 = 解绑，行内另有「解绑」按钮提示）
#[cfg(windows)]
fn bound_row_rect(e: &Env, i: usize) -> RECT {
    let y = SET_BOUND_Y0 + i as i32 * SET_BOUND_ROW_H;
    lrect(
        e,
        CONTENT_L,
        y,
        row_right(e, LIST_ROW_W),
        y + SET_BOUND_ROW_H,
    )
}

/// 身份变化确认块 [信任并重新配对][取消] 两按钮矩形（复用 SAS 区的纵向位置：两者是同一个「配对决策」交互位）。
#[cfg(windows)]
fn identity_buttons(e: &Env, device_count: usize) -> (RECT, RECT) {
    let y = sas_btns_y(device_count);
    (
        lrect(e, CONTENT_L, y, CONTENT_L + 196, y + 38),
        lrect(e, CONTENT_L + 212, y, CONTENT_L + 324, y + 38),
    )
}

/// 身份变化卡片（设备名 + 旧/新指纹三行）
#[cfg(windows)]
fn identity_card_rect(e: &Env, device_count: usize) -> RECT {
    lrect(
        e,
        CONTENT_L,
        sas_card_y(device_count),
        CONTENT_L + 470,
        sas_card_y(device_count) + 92,
    )
}

/// 设置页 Debug 行末的「导出日志」按钮（与 Debug 开关同行）
#[cfg(windows)]
fn debug_export_rect(e: &Env) -> RECT {
    let y = SET_TOGGLE_Y0 + SET_DEBUG_ROW as i32 * SET_ROW_H + 2;
    let l = CONTENT_L + SET_SWITCH_L + SW_W + 14;
    lrect(e, l, y, l + SET_INLINE_BTN_W, y + SW_H + 4)
}

/// 设置页同一行上小按钮的宽度（「导出日志」与「关于 LinkX」共用这一档）
#[cfg(windows)]
const SET_INLINE_BTN_W: i32 = 104;

/// 关于页三行的基线（逻辑像素）：大字与副标题之间留一行呼吸，作者行紧跟副标题。
#[cfg(windows)]
const ABOUT_TITLE_Y: i32 = 92;
#[cfg(windows)]
const ABOUT_TAGLINE_Y: i32 = 152;
#[cfg(windows)]
const ABOUT_AUTHOR_Y: i32 = 182;

#[cfg(windows)]
fn unbind_btn_rect(e: &Env, i: usize) -> RECT {
    let row = bound_row_rect(e, i);
    RECT {
        left: row.right - e.px(76),
        top: row.top + e.px(2),
        right: row.right,
        bottom: row.bottom - e.px(2),
    }
}

/// 媒体页：第 i 个控制按钮（0=上一首 1=播放/暂停 2=下一首 3=音量- 4=音量+）
#[cfg(windows)]
fn media_btn_rect(e: &Env, i: usize) -> RECT {
    // 整排按钮居中；音量两个放在同一行的两侧
    let n = MEDIA_BTN_COUNT as i32;
    let total = (MEDIA_BTN_W * n) + MEDIA_BTN_GAP * (n - 1);
    let lw = e.logical_w();
    let x0 = ((lw - total) / 2).max(CONTENT_L);
    let x = x0 + (i as i32) * (MEDIA_BTN_W + MEDIA_BTN_GAP);
    lrect(
        e,
        x,
        MEDIA_BTN_Y,
        x + MEDIA_BTN_W,
        MEDIA_BTN_Y + MEDIA_BTN_H,
    )
}

/// 功能页：某个模块的开关矩形（与设置页开关同一 x，保持列对齐）
#[cfg(windows)]
fn feat_switch_rect(e: &Env, row: usize) -> RECT {
    let y = FEAT_Y0 + row as i32 * FEAT_ROW_H + 2;
    lrect(e, CONTENT_L + 380, y, CONTENT_L + 380 + SW_W, y + SW_H)
}

/// 功能页：「重新启动」按钮（提示条右侧，**必须落在 banner 之内**——右缘超出 banner 看起来就像贴错了容器）。
#[cfg(windows)]
fn feat_restart_rect(e: &Env) -> RECT {
    let y = FEAT_BANNER_Y + (FEAT_BANNER_H - FEAT_BTN_H) / 2;
    let r = CONTENT_L + FEAT_BANNER_W - 16;
    lrect(e, r - FEAT_BTN_W, y, r, y + FEAT_BTN_H)
}

/// 重启确认弹窗的卡片矩形（按客户区居中；高度见 `MODAL_H`，随改动条目数增长）
#[cfg(windows)]
fn modal_card_rect(e: &Env) -> RECT {
    let lw = e.logical_w();
    let lh = e.logical_h();
    let l = (lw - MODAL_W) / 2;
    let t = (lh - MODAL_H) / 2 - 12;
    lrect(e, l, t, l + MODAL_W, t + MODAL_H)
}

/// 弹窗两个按钮（左「稍后启动」次行动作，右「重新启动」主行动作）
#[cfg(windows)]
fn modal_btn_rects(e: &Env) -> (RECT, RECT) {
    let card = modal_card_rect(e);
    let bw = e.px(150);
    let bh = e.px(36);
    let gap = e.px(12);
    let bottom = card.bottom - e.px(20);
    let now = RECT {
        left: card.right - e.px(24) - bw,
        top: bottom - bh,
        right: card.right - e.px(24),
        bottom,
    };
    let later = RECT {
        left: now.left - gap - bw,
        top: now.top,
        right: now.left - gap,
        bottom: now.bottom,
    };
    (later, now)
}

/// 设置页：「关闭按钮行为」按钮（与开关同一行几何、同一列——那一栏本来就是放可点控件的）
#[cfg(windows)]
fn set_behavior_rect(e: &Env) -> RECT {
    let y = SET_TOGGLE_Y0 + SET_BEHAVIOR_ROW as i32 * SET_ROW_H + 2;
    let l = CONTENT_L + SET_SWITCH_L;
    lrect(e, l, y, l + SET_BEHAVIOR_BTN_W, y + SW_H + 4)
}

/// 关闭询问弹窗的每一块：文字位与可点位都出自这一份，绘制 / 命中 / 悬停不许各写一遍坐标
#[cfg(windows)]
struct ClosePromptRects {
    card: RECT,
    title: RECT,
    body: RECT,
    /// 主行动作（也是最右的一颗，回车就是它）
    minimize: RECT,
    exit: RECT,
    cancel: RECT,
    remember: RECT,
}

/// 三颗按钮的宽度（逻辑像素，左→右：取消 / 退出程序 / 最小化到托盘）。**按取值定宽**：
/// 宽度若随文字变，鼠标停在原地就会在两次重绘之间换一颗按钮
#[cfg(windows)]
const CLOSE_BTN_W: [i32; 3] = [72, 96, 130];
#[cfg(windows)]
const CLOSE_BTN_GAP: i32 = 12;

#[cfg(windows)]
const _: () = assert!(
    CLOSE_BTN_W[0] + CLOSE_BTN_W[1] + CLOSE_BTN_W[2] + CLOSE_BTN_GAP * 2 <= CLOSE_MODAL_W - 48,
    "关闭询问弹窗的三个按钮排不进卡片"
);

#[cfg(windows)]
fn close_prompt_rects(e: &Env) -> ClosePromptRects {
    let l = (e.logical_w() - CLOSE_MODAL_W) / 2;
    let t = (e.logical_h() - CLOSE_MODAL_H) / 2 - 12;
    let card = lrect(e, l, t, l + CLOSE_MODAL_W, t + CLOSE_MODAL_H);
    let slot = |dy: i32, h: i32| RECT {
        left: card.left + e.px(24),
        top: card.top + e.px(dy),
        right: card.right - e.px(24),
        bottom: card.top + e.px(dy) + e.px(h),
    };
    let bottom = card.bottom - e.px(20 + 22 + 14);
    // 三颗按钮与标题、正文同侧左对齐：正文靠左而按钮靠右，一张卡上出现两套对齐（真机反馈）
    let mut left = card.left + e.px(24);
    let mut mk = |w: i32| {
        let r = RECT {
            left,
            top: bottom - e.px(36),
            right: left + e.px(w),
            bottom,
        };
        left = r.right + e.px(CLOSE_BTN_GAP);
        r
    };
    let cancel = mk(CLOSE_BTN_W[0]);
    let exit = mk(CLOSE_BTN_W[1]);
    let minimize = mk(CLOSE_BTN_W[2]);
    let remember = RECT {
        left: card.left + e.px(24),
        right: card.right - e.px(24),
        top: bottom + e.px(14),
        bottom: bottom + e.px(14 + 22),
    };
    ClosePromptRects {
        card,
        title: slot(20, 26),
        body: slot(54, 20),
        minimize,
        exit,
        cancel,
        remember,
    }
}

#[cfg(windows)]
fn point_in(rc: &RECT, x: i32, y: i32) -> bool {
    x >= rc.left && x < rc.right && y >= rc.top && y < rc.bottom
}

// ---------- 几何：相册（绘制 / 命中 / 悬停 / 下载窗口四处同一份）----------

/// 相册网格的设计宽度：照片格要跟着窗口长大，但文字行那套自适应上限用在网格上，窄窗口会算出 0 列。
#[cfg(windows)]
const ALBUM_GRID_CAP_W: i32 = 1180;

/// 滚动条占的宽度（逻辑像素）。网格列数按"扣掉这条"来算，绘制与命中因此永远同一份几何。
#[cfg(windows)]
const ALBUM_BAR_W: i32 = 12;

/// 当前客户区能摆下多少格：`(列数, 可见行数)`
#[cfg(windows)]
fn album_geom(e: &Env) -> (i32, i32) {
    let lh = e.logical_h();
    let usable = row_right(e, ALBUM_GRID_CAP_W) - ALBUM_BAR_W - CONTENT_L + ALBUM_GAP;
    let cols = (usable / (ALBUM_CELL + ALBUM_GAP)).clamp(1, 16);
    // 网格可用底边：页脚之上还要留得下详情行
    let bottom = (lh - ALBUM_FOOT_H - ALBUM_DETAIL_H - 6).max(ALBUM_GRID_Y + ALBUM_CELL);
    let rows = ((bottom - ALBUM_GRID_Y + ALBUM_GAP) / (ALBUM_CELL + ALBUM_GAP)).max(1);
    (cols, rows)
}

/// 第 `slot` 个**可见格**的矩形（`cols` 由 `album_geom` 给出，不再各算一遍）。
/// 传的是"这一格里排第几个"，不是条目下标 —— 滚动时两者差一个基准行。
#[cfg(windows)]
fn album_cell_rect(e: &Env, slot: usize, cols: i32) -> RECT {
    let (col, row) = (slot as i32 % cols, slot as i32 / cols);
    let x = CONTENT_L + col * (ALBUM_CELL + ALBUM_GAP);
    let y = ALBUM_GRID_Y + row * (ALBUM_CELL + ALBUM_GAP);
    lrect(e, x, y, x + ALBUM_CELL, y + ALBUM_CELL)
}

/// 当前能滚到的最大行偏移（可见窗口的最后一行仍要贴着列表末尾）
#[cfg(windows)]
fn album_max_scroll_row(e: &Env, st: &UiState) -> usize {
    let (cols, rows) = album_geom(e);
    let total_rows = st.album.items.len().div_ceil(cols.max(1) as usize);
    total_rows.saturating_sub(rows.max(1) as usize)
}

/// 这一帧的基准行：把界面上的滚动值夹进合法区间（改窗口大小、清单变短都靠它兜住）
#[cfg(windows)]
fn album_base_row(e: &Env, st: &UiState) -> usize {
    st.album.scroll_row.min(album_max_scroll_row(e, st))
}

/// 相册页这一帧画得下的格数：绘制、命中、悬停、以及"该给谁下载缩略图"必须取同一个数
#[cfg(windows)]
fn album_cell_count(e: &Env, st: &UiState) -> usize {
    let (cols, rows) = album_geom(e);
    let base = album_base_row(e, st) * cols.max(1) as usize;
    st.album
        .items
        .len()
        .saturating_sub(base)
        .min((cols * rows) as usize)
}

/// 坐标落在哪一格（返回**条目下标**；只有画得出来的格子才应答）
#[cfg(windows)]
fn album_cell_at(e: &Env, st: &UiState, x: i32, y: i32) -> Option<usize> {
    let (cols, _) = album_geom(e);
    let base = album_base_row(e, st) * cols.max(1) as usize;
    (0..album_cell_count(e, st))
        .find(|s| point_in(&album_cell_rect(e, *s, cols), x, y))
        .map(|s| base + s)
}

#[cfg(windows)]
fn album_bar_track(e: &Env) -> RECT {
    let lh = e.logical_h();
    let bottom = (lh - ALBUM_FOOT_H - ALBUM_DETAIL_H - 6).max(ALBUM_GRID_Y + ALBUM_CELL);
    let x = row_right(e, ALBUM_GRID_CAP_W) - ALBUM_BAR_W;
    lrect(e, x, ALBUM_GRID_Y, x + ALBUM_BAR_W, bottom)
}

/// 滚动条滑块：高度按"可见行 / 总行"比例，位置按当前基准行
#[cfg(windows)]
fn album_bar_thumb(e: &Env, st: &UiState) -> Option<RECT> {
    let (cols, rows) = album_geom(e);
    let total_rows = st.album.items.len().div_ceil(cols.max(1) as usize);
    if total_rows <= rows as usize {
        return None; // 画得下就不该出现滚动条
    }
    let track = album_bar_track(e);
    let track_h = (track.bottom - track.top).max(1);
    let knob_h = ((track_h as f32) * (rows as f32 / total_rows as f32)).round() as i32;
    let knob_h = knob_h.clamp(e.px(24), track_h);
    let max_row = album_max_scroll_row(e, st).max(1) as f32;
    let pos = (album_base_row(e, st) as f32 / max_row).clamp(0.0, 1.0);
    let top = track.top + ((track_h - knob_h) as f32 * pos).round() as i32;
    // 轨道四边已经是**物理**像素（`lrect` 算出来的），这里不能再过一遍 `lrect`——再乘一次缩放，滑块会画到客户区外，看起来就是"滚动条根本没出现"。
    Some(rect(
        track.left + e.px(2),
        top,
        track.right - e.px(2),
        top + knob_h,
    ))
}

/// 把轨道内的 y 坐标换算成滚动行偏移（点轨道/拖滑块共用）
#[cfg(windows)]
fn album_row_at_bar_y(e: &Env, st: &UiState, y: i32) -> usize {
    let max_row = album_max_scroll_row(e, st);
    let track = album_bar_track(e);
    let track_h = (track.bottom - track.top).max(1) as f32;
    let frac = ((y - track.top) as f32 / track_h).clamp(0.0, 1.0);
    (frac * max_row as f32).round() as usize
}

#[cfg(all(windows, feature = "agent-debug"))]
pub(crate) fn album_scroll_row_max(hwnd: HWND, st: &UiState) -> usize {
    let e = theme::detect(hwnd, st.theme);
    album_max_scroll_row(&e, st)
}

/// 调试面：轨道的物理矩形 + 这一帧的可见格数（点轨道不响应时靠它一次看清是谁错）
#[cfg(all(windows, feature = "agent-debug"))]
pub(crate) fn album_track_dbg(hwnd: HWND, st: &UiState) -> [i32; 4] {
    let e = theme::detect(hwnd, st.theme);
    let t = album_bar_track(&e);
    [t.left, t.top, t.right, t.bottom]
}

#[cfg(all(windows, feature = "agent-debug"))]
pub(crate) fn album_visible_dbg(hwnd: HWND, st: &UiState) -> usize {
    let e = theme::detect(hwnd, st.theme);
    album_cell_count(&e, st)
}

/// 调试面：可见格的矩形表（物理、客户区坐标）——自动化"按住第一格拖出去"要靠几何真值，拿截图像素反推坐标太脆。
/// **不在相册页就返回空**：几何函数不看页签，照样能算出一堆矩形——那会让自动化拿着"屏幕上根本没有的格子"的坐标去点。
#[cfg(all(windows, feature = "agent-debug"))]
pub(crate) fn album_cells_dbg(hwnd: HWND, st: &UiState) -> Vec<[i32; 4]> {
    if st.active_tab != TAB_ALBUM {
        return Vec::new();
    }
    let e = theme::detect(hwnd, st.theme);
    let (cols, _) = album_geom(&e);
    (0..album_cell_count(&e, st))
        .map(|s| {
            let r = album_cell_rect(&e, s, cols);
            [r.left, r.top, r.right, r.bottom]
        })
        .collect()
}

/// 工具行第 `slot` 个按钮（0=刷新 1=上一页 2=下一页 3=全选 4=导出）
#[cfg(windows)]
fn album_btn_rect(e: &Env, slot: usize) -> RECT {
    debug_assert!(slot < ALBUM_BTN_W.len());
    let mut x = CONTENT_L;
    for w in &ALBUM_BTN_W[..slot] {
        x += w + ALBUM_BTN_GAP;
    }
    let w = ALBUM_BTN_W[slot];
    lrect(e, x, ALBUM_TOOLBAR_Y, x + w, ALBUM_TOOLBAR_Y + ALBUM_BTN_H)
}

/// 详情行左端（悬停格子的文件名/大小/日期、以及"本页还有几张没显示"）
#[cfg(windows)]
fn album_detail_rect(e: &Env) -> RECT {
    let lh = e.logical_h();
    let y = (lh - ALBUM_FOOT_H - ALBUM_DETAIL_H).max(ALBUM_GRID_Y);
    lrect(
        e,
        CONTENT_L,
        y,
        row_right(e, ALBUM_GRID_CAP_W),
        y + ALBUM_DETAIL_H,
    )
}

#[cfg(windows)]
fn sas_pending(st: &UiState) -> bool {
    st.conn_state == state_code::SAS_COMPARE
}

/// 「想要」与「本次启动已加载」不一致的模块 → 功能页提示条与重启弹窗的条目
#[cfg(windows)]
fn pending_modules(st: &UiState) -> Vec<(crate::features::Module, bool)> {
    crate::features::changes(st)
}

/// 命中判定：坐标（物理像素）→ 目标（无命中返回 None）
#[cfg(windows)]
pub(crate) fn hit_test(st: &UiState, e: &Env, x: i32, y: i32) -> Option<HitTarget> {
    // 模态优先：弹窗打开时，除弹窗自己的控件外一律不给命中（含导航区）
    if st.restart_prompt {
        let (later, now) = modal_btn_rects(e);
        return if point_in(&now, x, y) {
            Some(HitTarget::ModalRestartNow)
        } else if point_in(&later, x, y) {
            Some(HitTarget::ModalRestartLater)
        } else {
            None
        };
    }
    if st.close_prompt {
        let r = close_prompt_rects(e);
        return if point_in(&r.minimize, x, y) {
            Some(HitTarget::ModalCloseMinimize)
        } else if point_in(&r.exit, x, y) {
            Some(HitTarget::ModalCloseExit)
        } else if point_in(&r.cancel, x, y) {
            Some(HitTarget::ModalCloseCancel)
        } else if point_in(&r.remember, x, y) {
            Some(HitTarget::ModalCloseRemember)
        } else {
            None
        };
    }
    if x < e.px(NAV_W) {
        let vis = nav_visible();
        return (0..vis.len())
            .find(|r| point_in(&nav_item_rect(e, *r), x, y))
            .map(|r| HitTarget::Nav(vis[r]));
    }
    match st.active_tab {
        TAB_CONNECT => {
            for (i, (addr, _)) in st.devices.iter().enumerate() {
                if point_in(&device_row_rect(e, i), x, y) {
                    return Some(HitTarget::ConnectDevice(*addr));
                }
            }
            // 身份变化确认（优先于 SAS：此时状态是 REPAIRED，非 SAS_COMPARE）
            if st.identity_change.is_some() {
                let (accept, reject) = identity_buttons(e, st.devices.len());
                if point_in(&accept, x, y) {
                    return Some(HitTarget::AcceptNewIdentity);
                }
                if point_in(&reject, x, y) {
                    return Some(HitTarget::RejectNewIdentity);
                }
            }
            if sas_pending(st) {
                let (confirm, reject) = sas_buttons(e, st.devices.len());
                if point_in(&confirm, x, y) {
                    return Some(HitTarget::SasConfirm);
                }
                if point_in(&reject, x, y) {
                    return Some(HitTarget::SasReject);
                }
            }
            None
        }
        TAB_NOTIFY => {
            // 回复条画在列表上方，先判它
            if st.reply_target.is_some() {
                let (input, send) = notify_reply_rects(e);
                if point_in(&send, x, y) {
                    return Some(HitTarget::SendReply);
                }
                if point_in(&input, x, y) {
                    return Some(HitTarget::FocusReplyInput);
                }
            }
            // 滚动条先判：轨道在行的右外侧，不先判就会被整行的命中吞掉
            if notify_bar_thumb(e, st).is_some() && point_in(&notify_bar_track(e), x, y) {
                return Some(HitTarget::NotifyScroll(notify_scroll_at(e, st, y)));
            }
            let base = notify_base(e, st);
            let rows = notify_visible_rows(e);
            for slot in 0..rows.min(st.notifications.len().saturating_sub(base)) {
                let item = &st.notifications[base + slot];
                // 小按钮优先：整行也是"复制全文"，不先判它们就会被整行的命中吞掉
                let (code, reply, all) =
                    notification_chips(e, slot, notify_code_of(item).is_some(), item.can_reply);
                if let Some(r) = code {
                    if point_in(&r, x, y) {
                        return Some(HitTarget::CopyNotificationCode(base + slot));
                    }
                }
                if let Some(r) = reply {
                    if point_in(&r, x, y) {
                        return Some(HitTarget::ReplyNotification(base + slot));
                    }
                }
                if point_in(&all, x, y) {
                    return Some(HitTarget::CopyNotification(base + slot));
                }
            }
            (0..rows)
                .filter(|s| base + s < st.notifications.len())
                .find(|s| point_in(&notification_row_rect(e, *s), x, y))
                .map(|s| HitTarget::CopyNotification(base + s))
        }
        TAB_CLIP => {
            if point_in(&clip_toggle_rect(e), x, y) {
                return Some(HitTarget::ToggleClipSync);
            }
            if point_in(&clip_send_rect(e), x, y) {
                return Some(HitTarget::SendLocalClip);
            }
            None
        }
        TAB_FILES => {
            // 文件页：发送路径输入框 + 「选择…」+「发送到手机」
            if point_in(&file_path_rect(e), x, y) {
                return Some(HitTarget::FocusSendPath);
            }
            if point_in(&file_browse_rect(e), x, y) {
                return Some(HitTarget::BrowseFile);
            }
            if point_in(&file_send_rect(e), x, y) {
                return Some(HitTarget::SendFile);
            }
            // 传输行的「取消」：只在途行应答（与绘制同一个 file_cancel_hit_rect）
            if let Some(i) = file_cancel_row_at(e, st, x, y) {
                return Some(HitTarget::CancelFile(i));
            }
            if point_in(&inbox_pick_rect(e), x, y) {
                return Some(HitTarget::ChooseInbox);
            }
            None
        }
        // 命中判定必须与 paint_media 的前提一致：没有播放状态时按钮**根本没画**，若此处仍应答，用户在一片空白上点到的就是"音量 ±5"，而 cur 取 0 会把手机静音。
        TAB_MEDIA => {
            let _ = st.media.as_ref()?;
            (0..MEDIA_BTN_COUNT)
                .find(|i| point_in(&media_btn_rect(e, *i), x, y))
                .map(HitTarget::MediaControl)
        }
        TAB_FEATURES => {
            if crate::features::restart_pending(st) && point_in(&feat_restart_rect(e), x, y) {
                return Some(HitTarget::RestartApp);
            }
            for (i, m) in crate::features::ALL.iter().enumerate() {
                if point_in(&feat_switch_rect(e, i), x, y) {
                    return Some(HitTarget::ToggleFeature(*m));
                }
            }
            None
        }
        TAB_SETTINGS => {
            for (i, r) in theme_seg_rects(e).iter().enumerate() {
                if point_in(r, x, y) {
                    return Some(HitTarget::ThemeSet(match i {
                        1 => Theme::Light,
                        2 => Theme::Dark,
                        _ => Theme::System,
                    }));
                }
            }
            if point_in(&set_switch_rect(e, 0), x, y) {
                return Some(HitTarget::ToggleToast);
            }
            if point_in(&set_switch_rect(e, 1), x, y) {
                return Some(HitTarget::ToggleToastContent);
            }
            if point_in(&set_switch_rect(e, 2), x, y) {
                return Some(HitTarget::ToggleClipSync);
            }
            // 开机后自动连接
            if point_in(&set_switch_rect(e, 3), x, y) {
                return Some(HitTarget::ToggleAutoConnect);
            }
            // 开机自启动（注册表里那条启动项的真值）
            if point_in(&set_switch_rect(e, SET_AUTOSTART_ROW), x, y) {
                return Some(HitTarget::ToggleAutostart);
            }
            // Debug 模式开关 + 导出日志
            if point_in(&set_switch_rect(e, SET_DEBUG_ROW), x, y) {
                return Some(HitTarget::ToggleDebug);
            }
            if point_in(&set_behavior_rect(e), x, y) {
                return Some(HitTarget::CycleCloseBehavior);
            }
            if point_in(&debug_export_rect(e), x, y) {
                return Some(HitTarget::ExportDebug);
            }
            // 手动 IP（输入框 + 连接按钮）与已绑定设备（解绑）
            if point_in(&manual_ip_rect(e), x, y) {
                return Some(HitTarget::FocusManualIp);
            }
            if point_in(&manual_ip_btn_rect(e), x, y) {
                return Some(HitTarget::ManualIpConnect);
            }
            let rows = st.bound_devices.len().min(SET_BOUND_MAX);
            if let Some(i) = (0..rows).find(|i| point_in(&bound_row_rect(e, *i), x, y)) {
                return Some(HitTarget::UnbindDevice(i));
            }
            None
        }
        // 关于页只剩三行字，没有任何可点的东西（外链按钮按用户口径已剔除）
        TAB_ABOUT => None,
        TAB_ALBUM => {
            // 工具行优先：格子区从工具行下面开始，两者不重叠，但顺序与绘制同源
            if let Some(i) = (0..ALBUM_BTN_W.len()).find(|i| point_in(&album_btn_rect(e, *i), x, y))
            {
                return Some(HitTarget::AlbumTool(i));
            }
            if album_bar_thumb(e, st).is_some() && point_in(&album_bar_track(e), x, y) {
                return Some(HitTarget::AlbumScroll(album_row_at_bar_y(e, st, y)));
            }
            album_cell_at(e, st, x, y).map(HitTarget::AlbumCell)
        }
        _ => None,
    }
}

// ---------- 字体（角色化 + 跟随系统字体 + 缓存） ----------

/// 文本角色（决定逻辑像素高与字重；行高由字号推算）
#[cfg(windows)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Role {
    Title,
    Body,
    BodyStrong,
    Small,
    /// SAS 大号数字（配对时需在两步外读清）
    Sas,
    /// 品牌大字（关于页的「LinkX」；只有这一处用，别拿它当页标题）
    Display,
}

#[cfg(windows)]
fn role_spec(r: Role) -> (i32, i32) {
    match r {
        Role::Title => (21, FW_SEMIBOLD.0 as i32),
        Role::Body => (15, FW_NORMAL.0 as i32),
        Role::BodyStrong => (15, FW_SEMIBOLD.0 as i32),
        Role::Small => (13, FW_NORMAL.0 as i32),
        Role::Sas => (38, FW_SEMIBOLD.0 as i32),
        Role::Display => (44, FW_BOLD.0 as i32),
    }
}

/// 行高（按字号 1.55 倍取整）
#[cfg(windows)]
fn role_line_h(e: &Env, r: Role) -> i32 {
    let (px, _) = role_spec(r);
    e.px((px as f32 * 1.55).round() as i32)
}

#[cfg(windows)]
thread_local! {
    static FONT_CACHE: RefCell<Vec<((i32, i32), HFONT)>> = const { RefCell::new(Vec::new()) };
}

/// 取角色对应字体（按物理字号缓存；字体名来自系统 UI 字体）
#[cfg(windows)]
fn font_for(e: &Env, role: Role) -> HFONT {
    let (logical, weight) = role_spec(role);
    let px = e.px(logical);
    FONT_CACHE.with(|cell| {
        let mut cache = cell.borrow_mut();
        if let Some((_, f)) = cache.iter().find(|((p, w), _)| *p == px && *w == weight) {
            return *f;
        }
        let hf = unsafe {
            CreateFontW(
                -px,
                0,
                0,
                0,
                weight,
                0,
                0,
                0,
                DEFAULT_CHARSET.0 as u32,
                OUT_TT_PRECIS.0 as u32,
                CLIP_DEFAULT_PRECIS.0 as u32,
                CLEARTYPE_QUALITY.0 as u32,
                (DEFAULT_PITCH.0 as u32) | (FF_DONTCARE.0 as u32),
                PCWSTR(e.face.as_ptr()),
            )
        };
        cache.push(((px, weight), hf));
        hf
    })
}

/// 在某一角色字体下测量文本（返回物理像素 (宽, 高)）
#[cfg(windows)]
fn text_extent(hdc: HDC, e: &Env, role: Role, s: &str) -> (i32, i32) {
    let wide: Vec<u16> = s.encode_utf16().collect();
    if wide.is_empty() {
        return (0, 0);
    }
    let font = font_for(e, role);
    unsafe {
        let old = SelectObject(hdc, HGDIOBJ(font.0));
        let mut sz = SIZE::default();
        let _ = GetTextExtentPoint32W(hdc, &wide, &mut sz);
        let _ = SelectObject(hdc, old);
        (sz.cx, sz.cy)
    }
}

/// 按像素宽度截断（超出补 "…"）——长剪贴板文本/通知正文用
#[cfg(windows)]
fn truncate_px(hdc: HDC, e: &Env, role: Role, s: &str, max_w: i32) -> String {
    if max_w <= 0 {
        // 一点位置都没有：还回全文就是让文字压到旁边的元素上，本函数存在的意义就是不越界
        return String::new();
    }
    if text_extent(hdc, e, role, s).0 <= max_w {
        return s.to_string();
    }
    let ell = "…";
    let ell_w = text_extent(hdc, e, role, ell).0;
    let mut out = String::new();
    let mut w = 0;
    for ch in s.chars() {
        let cw = text_extent(hdc, e, role, &ch.to_string()).0;
        if w + cw + ell_w > max_w {
            break;
        }
        out.push(ch);
        w += cw;
    }
    out.push_str(ell);
    out
}

/// 按像素宽度**保留尾部**（长路径显示末尾更易辨认），前面补 "…"
#[cfg(windows)]
fn tail_px(hdc: HDC, e: &Env, role: Role, s: &str, max_w: i32) -> String {
    if max_w <= 0 {
        return String::new();
    }
    if text_extent(hdc, e, role, s).0 <= max_w {
        return s.to_string();
    }
    let ell = "…";
    let ell_w = text_extent(hdc, e, role, ell).0;
    let mut acc = String::new();
    let mut w = 0;
    for ch in s.chars().rev() {
        let cw = text_extent(hdc, e, role, &ch.to_string()).0;
        if w + cw + ell_w > max_w {
            break;
        }
        acc.insert(0, ch);
        w += cw;
    }
    format!("{ell}{acc}")
}

// ---------- 调色板 / 画刷 ----------

/// 0xRRGGBB → COLORREF(0x00BBGGRR)
#[cfg(windows)]
pub(crate) fn colorref(rgb: u32) -> COLORREF {
    let v = ((rgb & 0x0000FF) << 16) | (rgb & 0x00FF00) | ((rgb & 0xFF0000) >> 16);
    COLORREF(v)
}

#[cfg(windows)]
fn fill(hdc: HDC, rc: &RECT, rgb: u32) {
    unsafe {
        let _ = FillRect(hdc, rc as *const RECT, icons::brush_solid(rgb));
    }
}

/// 圆角填充（同色画笔 + 画刷 → 无边框实心圆角块）
#[cfg(windows)]
fn fill_round(hdc: HDC, rc: &RECT, rgb: u32, radius: i32) {
    let old_pen = unsafe { SelectObject(hdc, HGDIOBJ(icons::pen_solid(1, rgb).0)) };
    let old_brush = unsafe { SelectObject(hdc, HGDIOBJ(icons::brush_solid(rgb).0)) };
    let _ = unsafe { RoundRect(hdc, rc.left, rc.top, rc.right, rc.bottom, radius, radius) };
    unsafe {
        SelectObject(hdc, old_pen);
        SelectObject(hdc, old_brush);
    }
}

#[cfg(windows)]
fn stroke_round(hdc: HDC, rc: &RECT, rgb: u32, radius: i32) {
    let old_pen = unsafe { SelectObject(hdc, HGDIOBJ(icons::pen_solid(1, rgb).0)) };
    let old_brush = unsafe { SelectObject(hdc, icons::null_brush()) };
    let _ = unsafe { RoundRect(hdc, rc.left, rc.top, rc.right, rc.bottom, radius, radius) };
    unsafe {
        SelectObject(hdc, old_pen);
        SelectObject(hdc, old_brush);
    }
}

/// 相册格子的**拖出进度环**（画在格子正中）。拖出载荷是 `CF_HDROP`，shell 只肯复制"磁盘上已经
/// 存在的文件"，所以原图必须先取到本机才谈得上拖——没有这一圈，用户既不知道还要等多久、也不敢
/// 松手（视频更是必然等不到）。按下格子就开始取，这一圈跑完 = 载荷到手 = 可以拖。
/// 画法跟着本壳的图标体系（GDI 实线画笔、无抗锯齿层）：深色底盘 + 灰色整圈轨道 + 强调色弧线 + 白色百分比，保证在任何一张缩略图上都读得清。
#[cfg(windows)]
fn album_drag_ring(hdc: HDC, e: &Env, cell: &RECT, frac: f32, accent: u32) {
    use std::f32::consts::PI;
    let side = (cell.right - cell.left).min(cell.bottom - cell.top);
    let r = ((side as f32 * 0.22) as i32).max(e.px(12));
    let cx = (cell.left + cell.right) / 2;
    let cy = (cell.top + cell.bottom) / 2;
    let ring = rect(cx - r, cy - r, cx + r, cy + r);
    let thick = e.px(3).max(2);
    // 本壳的绘制全程按"背景透明"来画（见 `text_out_center`），这里只保证它是开着的
    let _ = unsafe { SetBkMode(hdc, TRANSPARENT) };
    // 底盘：不透明深色圆，缩略图再花也不影响读数
    let old_pen = unsafe { SelectObject(hdc, HGDIOBJ(icons::pen_solid(1, 0x000000).0)) };
    let old_brush = unsafe { SelectObject(hdc, HGDIOBJ(icons::brush_solid(0x000000).0)) };
    let disc = rect(
        cx - r + thick,
        cy - r + thick,
        cx + r - thick,
        cy + r - thick,
    );
    let _ = unsafe { Ellipse(hdc, disc.left, disc.top, disc.right, disc.bottom) };
    // 之后只描边：不换成空画刷的话，轨道和弧线会把底盘重新填一遍
    let _ = unsafe { SelectObject(hdc, icons::null_brush()) };
    let _ = unsafe { SelectObject(hdc, HGDIOBJ(icons::pen_solid(thick, 0x3A3F47).0)) };
    let _ = unsafe { Ellipse(hdc, ring.left, ring.top, ring.right, ring.bottom) };
    // 进度弧：**GDI 的 `Arc` 从起点"逆时针"扫到终点**（设备坐标 y 向下，逆时针在屏幕上
    // 看着是顺时针）。所以要让 15% 只画 15%，参数必须写成"终点在前、12 点在后"；
    // 反过来传会画出 85% 的长弧——实测截图正是那样，判据要拿图对，不能靠推理。
    let ang = frac.clamp(0.0, 1.0) * 2.0 * PI - PI / 2.0;
    let (ex, ey) = (
        cx + (r as f32 * ang.cos()) as i32,
        cy + (r as f32 * ang.sin()) as i32,
    );
    let _ = unsafe { SelectObject(hdc, HGDIOBJ(icons::pen_solid(thick, accent).0)) };
    // 0% 不画弧：起点就是 12 点，起终点重合时 GDI 描的是整圈，"还没开始"会画成"已经跑满"
    if frac.clamp(0.0, 1.0) > 0.0 {
        let _ = unsafe {
            Arc(
                hdc,
                ring.left,
                ring.top,
                ring.right,
                ring.bottom,
                ex,
                ey,
                cx,
                cy - r,
            )
        };
    }
    unsafe {
        SelectObject(hdc, old_pen);
        SelectObject(hdc, old_brush);
    }
    // 百分比写在环心里：光看环猜"还差多少"是不公平的要求
    let pct = format!("{}%", (frac.clamp(0.0, 1.0) * 100.0).round() as i32);
    text_out_center(hdc, e, Role::Small, &disc, &pct, 0xFFFFFF);
}

#[cfg(windows)]
fn divider(hdc: HDC, x1: i32, x2: i32, y: i32, rgb: u32) {
    fill(hdc, &rect(x1, y, x2, y + 1), rgb);
}

// ---------- 文本绘制 ----------

/// 文本绘制（左对齐左上角），返回文本右缘 x
#[cfg(windows)]
fn text_out(hdc: HDC, e: &Env, role: Role, x: i32, y: i32, s: &str, rgb: u32) -> i32 {
    let wide: Vec<u16> = s.encode_utf16().collect();
    if wide.is_empty() {
        return x;
    }
    let font = font_for(e, role);
    unsafe {
        let old = SelectObject(hdc, HGDIOBJ(font.0));
        let _ = SetTextColor(hdc, colorref(rgb));
        let _ = SetBkMode(hdc, TRANSPARENT);
        let _ = TextOutW(hdc, x, y, &wide);
        let mut sz = SIZE::default();
        let _ = GetTextExtentPoint32W(hdc, &wide, &mut sz);
        let _ = SelectObject(hdc, old);
        x + sz.cx
    }
}

#[cfg(windows)]
fn text_out_right(hdc: HDC, e: &Env, role: Role, x_right: i32, y: i32, s: &str, rgb: u32) {
    let w = text_extent(hdc, e, role, s).0;
    text_out(hdc, e, role, x_right - w, y, s, rgb);
}

#[cfg(windows)]
fn text_out_center(hdc: HDC, e: &Env, role: Role, rc: &RECT, s: &str, rgb: u32) {
    let (tw, th) = text_extent(hdc, e, role, s);
    let x = rc.left + ((rc.right - rc.left) - tw) / 2;
    let y = rc.top + ((rc.bottom - rc.top) - th) / 2;
    text_out(hdc, e, role, x, y, s, rgb);
}

#[cfg(windows)]
fn button(hdc: HDC, e: &Env, rc: &RECT, label: &str, accent: bool, hover: bool) {
    let p = &e.pal;
    let bg = if accent {
        if hover {
            p.accent_dim.max(p.accent)
        } else {
            p.accent
        }
    } else if hover {
        p.row_hover_bg
    } else {
        p.btn_bg
    };
    // 悬停时给强调按钮提亮（简单有效的反馈；不做逐帧混色，避免闪烁）
    let bg = if accent && hover {
        blend(p.accent, 0xFFFFFF, 0.16)
    } else {
        bg
    };
    fill_round(hdc, rc, bg, e.px(8));
    let fg = if accent { 0xFFFFFF } else { p.btn_text };
    text_out_center(hdc, e, Role::Body, rc, label, fg);
}

/// 图标 + 文字按钮：整体在按钮内水平居中，图标在左。媒体页的控制按钮用它——纯文字看不出方向，纯图标又要用户先猜一遍。
#[cfg(windows)]
fn icon_button(hdc: HDC, e: &Env, rc: &RECT, icon: Icon, label: &str, accent: bool, hover: bool) {
    let p = &e.pal;
    let bg = match (accent, hover) {
        (true, true) => blend(p.accent, 0xFFFFFF, 0.16),
        (true, false) => p.accent,
        (false, true) => p.row_hover_bg,
        (false, false) => p.btn_bg,
    };
    fill_round(hdc, rc, bg, e.px(8));
    let fg = if accent { 0xFFFFFF } else { p.btn_text };
    let isz = e.px(NAV_ICON);
    let tw = if label.is_empty() {
        0
    } else {
        text_extent(hdc, e, Role::Small, label).0
    };
    let gap = if tw > 0 { e.px(6) } else { 0 };
    let x0 = rc.left + ((rc.right - rc.left) - (isz + gap + tw)) / 2;
    let y0 = rc.top + ((rc.bottom - rc.top) - isz) / 2;
    icons::draw(hdc, icon, x0, y0, isz, fg);
    if tw > 0 {
        text_out(
            hdc,
            e,
            Role::Small,
            x0 + isz + gap,
            y0 + (isz - role_line_h(e, Role::Small)) / 2,
            label,
            fg,
        );
    }
}

/// 文本输入框（圆角底 + 焦点/悬停描边 + 文本；空值时显示占位提示）。
/// 光标不在此处绘制：`pulse_phase` 驱动的焦点框高亮 + `wants_repaint` 持续重绘已表达"正在输入"，省下每字符一次文本宽度测量。
#[cfg(windows)]
fn input_box(
    hdc: HDC,
    e: &Env,
    rc: &RECT,
    text: &str,
    placeholder: &str,
    focused: bool,
    hovered: bool,
) {
    let p = &e.pal;
    fill_round(hdc, rc, p.card_bg, e.px(8));
    let border = if focused {
        p.accent
    } else if hovered {
        p.sub_text
    } else {
        p.divider
    };
    stroke_round(hdc, rc, border, e.px(8));
    let pad = e.px(10);
    let max_w = (rc.right - rc.left) - pad * 2;
    let (shown, color) = if text.is_empty() {
        (
            truncate_px(hdc, e, Role::Small, placeholder, max_w),
            p.foot_text,
        )
    } else {
        // 长路径显示尾部（更能指示「要发/收到的是哪个文件」）
        (tail_px(hdc, e, Role::Small, text, max_w), p.body_text)
    };
    let (tw, th) = text_extent(hdc, e, Role::Small, &shown);
    let ty = rc.top + ((rc.bottom - rc.top) - th) / 2;
    text_out(hdc, e, Role::Small, rc.left + pad, ty, &shown, color);
    // 焦点：在文本末尾画一根闪烁竖线（半周期亮）
    if focused && pulse_phase() < 0.5 {
        let cx = (rc.left + pad + tw + e.px(1)).min(rc.right - e.px(4));
        fill(
            hdc,
            &rect(cx, rc.top + e.px(6), cx + e.px(2), rc.bottom - e.px(6)),
            p.accent,
        );
    }
}

#[cfg(windows)]
fn progress_bar(hdc: HDC, e: &Env, rc: &RECT, percent: u8, done: bool) {
    let p = &e.pal;
    fill_round(hdc, rc, p.btn_bg, e.px(2));
    let w = rc.right - rc.left;
    let fill_w = (w as i64 * percent.min(100) as i64 / 100) as i32;
    if fill_w <= 0 {
        return;
    }
    let bar = RECT {
        left: rc.left,
        top: rc.top,
        right: rc.left + fill_w,
        bottom: rc.bottom,
    };
    fill_round(hdc, &bar, if done { p.ok_text } else { p.accent }, e.px(2));
}

#[cfg(windows)]
fn blend(a: u32, b: u32, t: f32) -> u32 {
    let ch = |sh: u32| {
        let x = ((a >> sh) & 0xFF) as f32;
        let y = ((b >> sh) & 0xFF) as f32;
        ((x + (y - x) * t).round().clamp(0.0, 255.0) as u32) << sh
    };
    ch(16) | ch(8) | ch(0)
}

/// 开关（胶囊轨道 + 圆形滑块）
#[cfg(windows)]
fn draw_switch(hdc: HDC, e: &Env, rc: &RECT, on: bool, hover: bool) {
    let p = &e.pal;
    let track = if on {
        p.accent
    } else if hover {
        blend(p.btn_bg, p.sub_text, 0.25)
    } else {
        p.btn_bg
    };
    let radius = (rc.bottom - rc.top) / 2;
    fill_round(hdc, rc, track, radius);
    let inset = e.px(3);
    let d = (rc.bottom - rc.top) - inset * 2;
    let kx = if on {
        rc.right - inset - d
    } else {
        rc.left + inset
    };
    let knob = RECT {
        left: kx,
        top: rc.top + inset,
        right: kx + d,
        bottom: rc.bottom - inset,
    };
    fill_round(hdc, &knob, 0xFFFFFF, d / 2);
}

#[cfg(windows)]
fn status_dot(hdc: HDC, cx: i32, cy: i32, r: i32, rgb: u32) {
    let rc = rect(cx - r, cy - r, cx + r, cy + r);
    fill_round(hdc, &rc, rgb, r);
}

// ---------- 过渡 / 脉冲 ----------

/// 过渡进度 t（0→1，ease-out）；无过渡时返回 1.0
#[cfg(windows)]
fn anim_t(st: &UiState) -> f32 {
    let Some(t0) = st.anim_start else {
        return 1.0;
    };
    let ms = t0.elapsed().as_millis();
    if ms >= ANIM_MS {
        return 1.0;
    }
    let x = ms as f32 / ANIM_MS as f32;
    1.0 - (1.0 - x).powi(3) // ease-out cubic
}

/// 连续脉冲相位 0→1（1.2s 一轮）
#[cfg(windows)]
fn pulse_phase() -> f32 {
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    (ms % 1200) as f32 / 1200.0
}

/// 是否还有动画在跑（window.rs 据此决定是否继续刷帧，静止时不空转重绘）
#[cfg(windows)]
pub(crate) fn wants_animation(st: &UiState) -> bool {
    if st
        .anim_start
        .is_some_and(|t| t.elapsed().as_millis() < ANIM_MS)
    {
        return true;
    }
    // 呼吸脉冲只画在连接页（扫描点、SAS 卡片），只有停在这一页才需要持续刷帧：DISCOVER 就是"未连接"的常态，不加这个门槛，界面静止时也按 30fps 整窗重绘。
    st.active_tab == TAB_CONNECT
        && (st.conn_state == state_code::DISCOVER || st.conn_state == state_code::SAS_COMPARE)
}

/// 是否需要重绘（动画进行中、「已复制」瞬时提示有效期内、输入框焦点闪烁光标、或相册还有缩略图在途）
#[cfg(windows)]
pub(crate) fn wants_repaint(st: &UiState) -> bool {
    wants_animation(st)
        || st.input_focus != crate::state::FOCUS_NONE
        || st
            .copied_at
            .is_some_and(|t| t.elapsed() < COPIED_HINT_TTL + std::time::Duration::from_millis(250))
        // 回复提示到点要自己消失：它常常是一句失败原因，赖在右上角比不显示更糟
        || st
            .reply_hint
            .as_ref()
            .is_some_and(|(_, _, t)| t.elapsed() < REPLY_HINT_TTL + std::time::Duration::from_millis(250))
        // 相册：缩略图一张一张到达，而"某张到了"本身不产生任何 Windows 消息——不在这里持续刷帧，用户会看到一半格子永远停在"载入中"。
        || (st.active_tab == TAB_ALBUM && st.album.busy())
}

/// 相册：把"这一帧实际画了多少格"回写进状态（**唯一**让 worker 知道视口大小的通道）。
/// 只回写两个整数，不 bump `ui_rev`：回写不改变画面，bump 了就变成"重绘 → 推进序号 → 再重绘"的自激循环。
#[cfg(windows)]
pub(crate) fn album_note_visible(st: &mut UiState, e: &Env) {
    if st.active_tab != TAB_ALBUM {
        st.album.visible_start = 0;
        st.album.visible_count = 0;
        return;
    }
    // 可见窗口 = 滚动基准行往下的 N 格。回写它，worker 才不会给"看不见的照片"下载缩略图。
    let (cols, _) = album_geom(e);
    st.album.visible_start = album_base_row(e, st) * cols.max(1) as usize;
    st.album.visible_count = album_cell_count(e, st);
}

/// 相册：视口现在画得下几格（工具行/详情页/调试面都问这一个数，别各自复制公式）
#[cfg(windows)]
pub(crate) fn album_viewport_cells(e: &Env) -> usize {
    let (cols, rows) = album_geom(e);
    (cols * rows) as usize
}

/// 悬停目标（导航下标 / 列表行 `(段, 行)`）；供 `WM_MOUSEMOVE` 驱动高亮与手型光标。
/// 段号约定：0=设备列表 1=通知列表 2=剪贴板按钮 4=主题分段 5=设置开关 6=SAS 按钮（3 号段已废弃，不再分配）
/// 7=文件页发送路径输入框 8=文件页发送按钮 9=设置页手动 IP 输入框 10=手动 IP 连接按钮 11=已绑定设备行（解绑）
/// 12=身份变化确认按钮 13=Debug 导出按钮 14=功能页模块开关 15=功能页「重新启动」16/17=重启弹窗主/次按钮
/// 18=媒体页控制按钮 19=「选择…」按钮 20=传输行「取消」21=「改到别处…」（收件目录）22=相册工具行按钮 23=相册格子。
#[cfg(windows)]
pub(crate) fn hover_at(
    st: &UiState,
    e: &Env,
    x: i32,
    y: i32,
) -> (Option<usize>, Option<(u8, usize)>) {
    // 弹窗打开时导航项也不给高亮：那一下点了也不会生效，亮起来是骗人的
    if st.restart_prompt {
        let (later, now) = modal_btn_rects(e);
        return (
            None,
            if point_in(&now, x, y) {
                Some((16u8, 0))
            } else if point_in(&later, x, y) {
                Some((17u8, 0))
            } else {
                None
            },
        );
    }
    if st.close_prompt {
        let r = close_prompt_rects(e);
        return (
            None,
            if point_in(&r.minimize, x, y) {
                Some((26u8, 0))
            } else if point_in(&r.exit, x, y) {
                Some((27u8, 0))
            } else if point_in(&r.cancel, x, y) {
                Some((28u8, 0))
            } else if point_in(&r.remember, x, y) {
                Some((29u8, 0))
            } else {
                None
            },
        );
    }
    if x < e.px(NAV_W) {
        // 返回的是**行号**（可见列表里的位置），绘制那边按同一口径高亮
        let vis = nav_visible();
        return (
            (0..vis.len()).find(|r| point_in(&nav_item_rect(e, *r), x, y)),
            None,
        );
    }
    match st.active_tab {
        TAB_CONNECT => {
            let row = st
                .devices
                .iter()
                .enumerate()
                .find(|(i, _)| point_in(&device_row_rect(e, *i), x, y))
                .map(|(i, _)| (0u8, i));
            if row.is_some() {
                return (None, row);
            }
            if st.identity_change.is_some() {
                let (a, r) = identity_buttons(e, st.devices.len());
                if point_in(&a, x, y) {
                    return (None, Some((12, 0)));
                }
                if point_in(&r, x, y) {
                    return (None, Some((12, 1)));
                }
            }
            if sas_pending(st) {
                let (c, r) = sas_buttons(e, st.devices.len());
                if point_in(&c, x, y) {
                    return (None, Some((6, 0)));
                }
                if point_in(&r, x, y) {
                    return (None, Some((6, 1)));
                }
            }
            (None, None)
        }
        TAB_NOTIFY => {
            let bar = if st.reply_target.is_some() {
                let (input, send) = notify_reply_rects(e);
                if point_in(&send, x, y) {
                    Some((3u8, 0))
                } else if point_in(&input, x, y) {
                    Some((2u8, 0))
                } else {
                    None
                }
            } else {
                None
            };
            (
                None,
                bar.or_else(|| {
                    let base = notify_base(e, st);
                    (0..notify_visible_rows(e))
                        .filter(|s| base + s < st.notifications.len())
                        .find(|s| point_in(&notification_row_rect(e, *s), x, y))
                        .map(|s| (1u8, base + s))
                }),
            )
        }
        TAB_CLIP => (
            None,
            if point_in(&clip_send_rect(e), x, y) {
                Some((2u8, 0))
            } else {
                None
            },
        ),
        TAB_FILES => (
            None,
            if point_in(&file_path_rect(e), x, y) {
                Some((7u8, 0))
            } else if point_in(&file_browse_rect(e), x, y) {
                Some((19u8, 0))
            } else if point_in(&file_send_rect(e), x, y) {
                Some((8u8, 0))
            } else if point_in(&inbox_pick_rect(e), x, y) {
                Some((21u8, 0))
            } else {
                // 20 号段 = 文件页传输行的「取消」按钮（与 hit_test 同一判据、同一几何）
                file_cancel_row_at(e, st, x, y).map(|i| (20u8, i))
            },
        ),
        TAB_MEDIA => (
            None,
            // 与 hit_test 同一前提：按钮没画就不给手型光标
            if st.media.is_some() {
                (0..MEDIA_BTN_COUNT)
                    .find(|i| point_in(&media_btn_rect(e, *i), x, y))
                    .map(|i| (18u8, i))
            } else {
                None
            },
        ),
        TAB_FEATURES => (
            None,
            if crate::features::restart_pending(st) && point_in(&feat_restart_rect(e), x, y) {
                Some((15u8, 0))
            } else {
                let mut hit = None;
                for (i, _m) in crate::features::ALL.iter().enumerate() {
                    if point_in(&feat_switch_rect(e, i), x, y) {
                        hit = Some((14u8, i));
                    }
                }
                hit
            },
        ),
        TAB_SETTINGS => {
            let mut hit = None;
            for (i, r) in theme_seg_rects(e).iter().enumerate() {
                if point_in(r, x, y) {
                    hit = Some((4u8, i));
                }
            }
            for i in 0..SET_TOGGLES {
                if point_in(&set_switch_rect(e, i), x, y) {
                    hit = Some((5u8, i));
                }
            }
            if point_in(&set_behavior_rect(e), x, y) {
                hit = Some((25u8, 0));
            }
            if point_in(&debug_export_rect(e), x, y) {
                hit = Some((13u8, 0));
            }
            if point_in(&manual_ip_rect(e), x, y) {
                hit = Some((9u8, 0));
            }
            if point_in(&manual_ip_btn_rect(e), x, y) {
                hit = Some((10u8, 0));
            }
            let rows = st.bound_devices.len().min(SET_BOUND_MAX);
            for i in 0..rows {
                if point_in(&bound_row_rect(e, i), x, y) {
                    hit = Some((11u8, i));
                }
            }
            (None, hit)
        }
        TAB_ABOUT => (None, None),
        TAB_ALBUM => {
            // 段号 22 = 工具行按钮，23 = 格子（与 hit_test 同一份几何）
            let mut hit = None;
            for i in 0..ALBUM_BTN_W.len() {
                if point_in(&album_btn_rect(e, i), x, y) {
                    hit = Some((22u8, i));
                }
            }
            if hit.is_none() {
                hit = album_cell_at(e, st, x, y).map(|i| (23u8, i));
            }
            // 滑块单独占一个槽位：悬停要亮，否则用户看不出这条窄列是能拖的
            if hit.is_none() && album_bar_thumb(e, st).is_some_and(|t| point_in(&t, x, y)) {
                hit = Some((24u8, 0));
            }
            (None, hit)
        }
        _ => (None, None),
    }
}
// ---------- 连接状态文案 ----------

#[cfg(windows)]
fn conn_state_text(st: &UiState) -> &'static str {
    match st.conn_state {
        state_code::DISCOVER => {
            if st.selected.is_some() {
                "发现对端…"
            } else {
                "扫描中…"
            }
        }
        state_code::HANDSHAKE => "正在握手…",
        state_code::PAIRING => "配对中…",
        state_code::SAS_COMPARE => "待人工比对 SAS",
        state_code::PAIRED => "已配对",
        state_code::REPAIRED => "对端指纹已变化，待复核",
        state_code::RECONNECTING => "重连中…",
        _ => "未连接",
    }
}

/// 毫秒时间戳 → HH:MM:SS（取模一天；免引入时间库）
#[cfg(windows)]
fn fmt_ts(ms: i64) -> String {
    if ms <= 0 {
        return "--".to_string();
    }
    let secs = (ms / 1000) % 86_400;
    format!(
        "{:02}:{:02}:{:02}",
        secs / 3600,
        (secs % 3600) / 60,
        secs % 60
    )
}

// ---------- 页：连接 ----------

#[cfg(windows)]
fn paint_connect(hdc: HDC, e: &Env, st: &UiState, w: i32, h: i32) {
    let p = &e.pal;
    let right = w - e.px(CONTENT_R_PAD);
    text_out(
        hdc,
        e,
        Role::Title,
        e.px(CONTENT_L),
        e.px(TITLE_Y),
        "连接",
        p.body_text,
    );

    let scanning = st.conn_state == state_code::DISCOVER;
    let mut x = e.px(CONTENT_L);
    if scanning {
        let ph = pulse_phase();
        let r = e.px(4) + (ph * e.px(2) as f32).round() as i32;
        status_dot(
            hdc,
            x + e.px(5),
            e.px(SUBTITLE_Y + 9),
            r,
            blend(p.body_bg, p.accent, 0.55),
        );
    }
    x += if scanning { e.px(18) } else { 0 };
    // 状态色只认"此刻真的连着"：没连上 / 没配对就是红色「未配对」，绿色留给引擎真的报 PAIRED；握手/待比对这些中间态用灰色，不冒充成功。
    let negotiating = matches!(
        st.conn_state,
        state_code::HANDSHAKE | state_code::PAIRING | state_code::SAS_COMPARE
    );
    let status_color = if st.link_paired() {
        p.ok_text
    } else if negotiating {
        p.sub_text
    } else {
        p.err_text
    };
    let status = if st.link_paired() {
        format!("已配对 · 已发现 {} 台设备", st.devices.len())
    } else if negotiating {
        format!(
            "{} · 已发现 {} 台设备",
            conn_state_text(st),
            st.devices.len()
        )
    } else {
        // 这里必须说清"为什么不算已配对"：直接抄引擎的状态文字，会出现「未配对 · 已配对」这种自相矛盾，
        // 而用户最想知道的那句（收不到手机消息 / 身份待确认）反而没出现。前缀用"未连接"：绑定是持久的，冷启动后没连上 ≠ 从没配对过。
        format!(
            "未连接 · {} · 已发现 {} 台设备",
            st.link_lie_reason(),
            st.devices.len()
        )
    };
    text_out(
        hdc,
        e,
        Role::Body,
        x,
        e.px(SUBTITLE_Y),
        &status,
        status_color,
    );

    if st.devices.is_empty() {
        text_out(
            hdc,
            e,
            Role::Body,
            e.px(CONTENT_L),
            e.px(DEVICE_Y0 + 6),
            "正在扫描 LinkX 设备…（请在手机上打开 LinkX）",
            p.sub_text,
        );
    } else {
        for (i, (addr, name)) in st.devices.iter().enumerate() {
            let row = device_row_rect(e, i);
            let hovered = st.list_hover == Some((0, i));
            if st.selected == Some(*addr) {
                fill_round(hdc, &row, p.sel_bg, e.px(8));
            } else if hovered {
                fill_round(hdc, &row, p.row_hover_bg, e.px(8));
            }
            let gap = e.px(12);
            let isz = e.px(NAV_ICON);
            // 每行一个手机字形：这一列本来就只放"扫到的手机"，图标比文字更快认出来
            icons::draw(
                hdc,
                Icon::Phone,
                row.left + gap,
                row.top + ((e.px(DEVICE_ROW_H) - isz) / 2).max(0),
                isz,
                p.sub_text,
            );
            let name_x = row.left + gap + isz + gap;
            let name_end = {
                let addr_txt = format!("{addr:012X}");
                let addr_w = text_extent(hdc, e, Role::Small, &addr_txt).0;
                // 先给地址留够位置，再按**剩余宽度**截断设备名：反过来名称一长就把地址往左钳，
                // 地址直接压在名称上（真机截图里长机型名与 12 位地址叠字就是这么来的）。
                let avail = (row.right - gap - addr_w - gap) - name_x;
                let shown = truncate_px(hdc, e, Role::Body, name, avail.max(e.px(48)));
                text_out(
                    hdc,
                    e,
                    Role::Body,
                    name_x,
                    row.top + e.px(9),
                    &shown,
                    p.body_text,
                )
            };
            let addr_txt = format!("{addr:012X}");
            text_out(
                hdc,
                e,
                Role::Small,
                name_end + e.px(12),
                row.top + e.px(11),
                &addr_txt,
                p.foot_text,
            );
        }
    }

    if let Some(change) = &st.identity_change {
        // 对端同名设备换了身份（典型场景：对方重装）→ 必须显式让用户决策；引擎检测到了但 UI 不弹，双端会一起卡死。
        let n = st.devices.len();
        let card = identity_card_rect(e, n);
        text_out(
            hdc,
            e,
            Role::BodyStrong,
            e.px(CONTENT_L),
            e.px(sas_hint_y(n)),
            "对端设备身份已变化，需重新配对确认",
            p.err_text,
        );
        fill_round(hdc, &card, p.card_bg, e.px(12));
        stroke_round(hdc, &card, p.err_text, e.px(12));
        let name = if change.name.is_empty() {
            "对端设备"
        } else {
            change.name.as_str()
        };
        let line = role_line_h(e, Role::Small);
        text_out(
            hdc,
            e,
            Role::Small,
            card.left + e.px(12),
            card.top + e.px(10),
            &format!("设备「{name}」（可能已重装或重置）"),
            p.body_text,
        );
        text_out(
            hdc,
            e,
            Role::Small,
            card.left + e.px(12),
            card.top + e.px(10) + line,
            &format!("旧身份 {}", change.old_fp),
            p.foot_text,
        );
        text_out(
            hdc,
            e,
            Role::Small,
            card.left + e.px(12),
            card.top + e.px(10) + line * 2,
            &format!("新身份 {}", change.new_fp),
            p.accent_dim,
        );
        let (accept, reject) = identity_buttons(e, n);
        button(
            hdc,
            e,
            &accept,
            "信任并重新配对",
            true,
            st.list_hover == Some((12, 0)),
        );
        button(
            hdc,
            e,
            &reject,
            "取消",
            false,
            st.list_hover == Some((12, 1)),
        );
    } else if sas_pending(st) {
        if let Some(sas) = st.sas {
            let n = st.devices.len();
            text_out(
                hdc,
                e,
                Role::Small,
                e.px(CONTENT_L),
                e.px(sas_hint_y(n)),
                "请与手机上显示的数字核对一致",
                p.sub_text,
            );
            let digits = format!("{sas:06}");
            let (dw, dh) = text_extent(hdc, e, Role::Sas, &digits);
            let pad = e.px(18);
            let card = rect(
                e.px(CONTENT_L),
                e.px(sas_card_y(n)),
                e.px(CONTENT_L) + dw + pad * 2,
                e.px(sas_card_y(n)) + dh + e.px(16),
            );
            fill_round(hdc, &card, p.card_bg, e.px(12));
            let ph = pulse_phase();
            // 呼吸相位量化到 1/8 档再混色：连续变化的颜色会以"宽度×颜色"为键
            // 往画笔缓存里不断塞永不销毁的 HPEN（本项目约定缓存只增不减）
            let wave = ((0.35 + 0.45 * (1.0 - (ph * 2.0 - 1.0).abs())) * 8.0).round() / 8.0;
            let border = blend(p.accent, p.card_bg, wave);
            stroke_round(hdc, &card, border, e.px(12));
            text_out(
                hdc,
                e,
                Role::Sas,
                card.left + pad,
                card.top + e.px(8),
                &digits,
                p.accent_dim,
            );

            let (confirm, reject) = sas_buttons(e, n);
            button(
                hdc,
                e,
                &confirm,
                "确认一致",
                true,
                st.list_hover == Some((6, 0)),
            );
            button(
                hdc,
                e,
                &reject,
                "不一致",
                false,
                st.list_hover == Some((6, 1)),
            );
        }
    } else if st.link_paired() {
        let y = e.px(sas_block_y(st.devices.len()));
        let name = if st.peer_name.is_empty() {
            "对端设备"
        } else {
            st.peer_name.as_str()
        };
        let lh = role_line_h(e, Role::Body);
        // 手机电量（DEVICE_STATUS）与「已配对」同一行的右端：先量出徽标宽度，再按剩余宽度截断设备名——顺序反过来名字就会压到徽标上。
        let badge = st.battery.map(|b| {
            let unknown = b.level < 0;
            let txt = if unknown {
                "电量未知".to_string()
            } else if b.charging {
                format!("{}% 充电中", b.level)
            } else {
                format!("{}%", b.level)
            };
            let color = if unknown {
                p.sub_text
            } else if b.charging {
                p.ok_text
            } else if b.level <= 20 {
                p.err_text
            } else {
                p.body_text
            };
            (txt, color)
        });
        let isz = e.px(NAV_ICON);
        let badge_w = badge
            .as_ref()
            .map(|(t, _)| isz + e.px(6) + text_extent(hdc, e, Role::Body, t).0)
            .unwrap_or(0);
        // 这一行是"我记着这台设备"，不是"我现在连着"：连着才配绿色，否则它会在链路已经掉的时候继续绿着说"已配对"。
        let (label, label_color) = if st.link_paired() {
            ("已配对：", p.ok_text)
        } else {
            ("已绑定（未连接）：", p.sub_text)
        };
        let end = text_out(
            hdc,
            e,
            Role::BodyStrong,
            e.px(CONTENT_L),
            y,
            label,
            label_color,
        );
        let name_x = end + e.px(4);
        let avail = (right - e.px(12) - badge_w - e.px(10)) - name_x;
        let shown = truncate_px(hdc, e, Role::Body, name, avail.max(e.px(48)));
        text_out(hdc, e, Role::Body, name_x, y, &shown, p.body_text);
        if let Some((txt, color)) = &badge {
            let tw = text_extent(hdc, e, Role::Body, txt).0;
            let tx = right - e.px(12) - tw;
            icons::draw(
                hdc,
                Icon::Battery,
                tx - isz - e.px(6),
                y + ((lh - isz) / 2).max(0),
                isz,
                *color,
            );
            text_out(hdc, e, Role::Body, tx, y, txt, *color);
        }
        if let Some(fp) = &st.peer_fp {
            text_out(
                hdc,
                e,
                Role::Small,
                e.px(CONTENT_L),
                y + lh,
                &format!("设备指纹 {fp}"),
                p.foot_text,
            );
        }
        // 对端 IP 与 TCP 通道状态（文件传输链路是否就绪一眼可见）
        let ip_line = if st.peer_lan_ip.is_empty() {
            if st.tcp_ready {
                "TCP 通道已就绪".to_string()
            } else {
                "TCP 通道建立中…".to_string()
            }
        } else if st.tcp_ready {
            format!("对端 {} · TCP 通道已就绪", st.peer_lan_ip)
        } else {
            format!("对端 {}（TCP 通道建立中…）", st.peer_lan_ip)
        };
        text_out(
            hdc,
            e,
            Role::Small,
            e.px(CONTENT_L),
            y + lh * 2,
            &ip_line,
            if st.tcp_ready { p.ok_text } else { p.sub_text },
        );
    }

    // 最近错误（最多 3 条；上移避开底部版本条）
    if !st.errors.is_empty() {
        let mut y = h
            - e.px(30)
            - role_line_h(e, Role::Small)
            - e.px(10)
            - st.errors.len() as i32 * e.px(18);
        for err in st.errors.iter().take(3) {
            let txt = truncate_px(hdc, e, Role::Small, &err.text(), right - e.px(CONTENT_L));
            text_out(hdc, e, Role::Small, e.px(CONTENT_L), y, &txt, p.err_text);
            y += e.px(18);
        }
    }
}

// ---------- 页：通知 ----------

#[cfg(windows)]
fn paint_notifications(hdc: HDC, e: &Env, st: &UiState, w: i32) {
    let p = &e.pal;
    let right = w - e.px(CONTENT_R_PAD);
    text_out(
        hdc,
        e,
        Role::Title,
        e.px(CONTENT_L),
        e.px(TITLE_Y),
        "通知",
        p.body_text,
    );

    // 优先级：回复结果 > 刚复制 > 常驻提示。否则点完"发送"看到的还是那句常驻提示，等于没回答。
    let hint = st
        .reply_hint
        .as_ref()
        .filter(|(_, _, at)| at.elapsed() < REPLY_HINT_TTL);
    let copied = st.copied_at.is_some_and(|t| t.elapsed() < COPIED_HINT_TTL);
    if let Some((text, ok, _)) = hint {
        text_out_right(
            hdc,
            e,
            Role::Small,
            right,
            e.px(TITLE_Y + 6),
            text,
            if *ok { p.ok_text } else { p.err_text },
        );
    } else if copied {
        text_out_right(
            hdc,
            e,
            Role::Small,
            right,
            e.px(TITLE_Y + 6),
            "已复制到剪贴板",
            p.ok_text,
        );
    } else if !st.notifications.is_empty() {
        text_out_right(
            hdc,
            e,
            Role::Small,
            right,
            e.px(TITLE_Y + 6),
            "点击任意通知即可复制正文",
            p.foot_text,
        );
    }

    if st.reply_target.is_some() {
        let (input, send) = notify_reply_rects(e);
        input_box(
            hdc,
            e,
            &input,
            &st.reply_input,
            "输入回复内容",
            st.input_focus == crate::state::FOCUS_REPLY,
            st.list_hover == Some((2, 0)),
        );
        button(hdc, e, &send, "发送", true, st.list_hover == Some((3, 0)));
    }

    if st.notifications.is_empty() {
        text_out(
            hdc,
            e,
            Role::Body,
            e.px(CONTENT_L),
            e.px(NOTIFY_Y0 + 6),
            "暂无通知",
            p.sub_text,
        );
        text_out(
            hdc,
            e,
            Role::Small,
            e.px(CONTENT_L),
            e.px(NOTIFY_Y0 + 6) + role_line_h(e, Role::Body),
            "手机推送的通知会显示在这里（需在手机端开启通知读取权限）",
            p.sub_text,
        );
        return;
    }

    // 绘制/命中/悬停共用同一份 base，否则"点第 1 行选中的是第 8 行"
    let base = notify_base(e, st);
    // 这一帧真正画得出的行数：循环边界与"最后一行不画分隔线"用的是同一个数
    let visible = notify_visible_rows(e).min(st.notifications.len().saturating_sub(base));
    for slot in 0..visible {
        let i = base + slot;
        let item = &st.notifications[i];
        let row = notification_row_rect(e, slot);
        let hovered = st.list_hover == Some((1, i));
        // 正在回复的那一行用选中底色标出来：按钮是裸文字，靠描边框子标选中态会在整页里
        // 多出唯一一个"框"，而行底色本来就是这套界面里"就是它"的说法（连接页同做法）。
        // 判据只走 `is_reply_target` 这一份：列表按 key 就地合并、行号一直在变，各处各算一遍迟早算出两个答案
        let selected = st.is_reply_target(item);
        if selected {
            fill_round(hdc, &row, p.sel_bg, e.px(8));
        } else if hovered {
            fill_round(hdc, &row, p.row_hover_bg, e.px(8));
        }
        let y = row.top;
        let text_x = row.left + e.px(12);
        // 行尾三颗文字按钮：有码时「复制验证码」+「复制全文」，应用挂了 RemoteInput 才给「回复」，
        // 没有就是真不支持。按钮必须先算：两行文字的右界要从这里扣掉一块，否则字从按钮底下穿过去。
        let code = notify_code_of(item);
        let (code_rect, reply_rect, all_rect) =
            notification_chips(e, slot, code.is_some(), item.can_reply);
        let text_right = notify_text_right(e, all_rect.left, right);
        // 行 1：标题（强调）+ 右侧时间；标题按像素截断，避免长标题压住时间/复制图标
        let raw_title = if item.title.is_empty() {
            "(无标题)"
        } else {
            item.title.as_str()
        };
        let title = truncate_px(
            hdc,
            e,
            Role::BodyStrong,
            raw_title,
            (text_right - e.px(NOTIFY_TIME_W) - text_x).max(e.px(24)),
        );
        text_out(
            hdc,
            e,
            Role::BodyStrong,
            text_x,
            y + e.px(8),
            &title,
            p.body_text,
        );
        text_out_right(
            hdc,
            e,
            Role::Small,
            right,
            y + e.px(11),
            &fmt_ts(item.ts_ms),
            p.foot_text,
        );
        // 两颗复制并排：有码时「复制验证码」+「复制全文」，没码时只剩「复制全文」。
        // 以前是"悬停才浮一个复制图标"，用户看不出这行能复制什么，只能靠标题下面那行小字提示
        if let Some(chip) = code_rect.as_ref() {
            text_out_center(
                hdc,
                e,
                Role::Small,
                chip,
                "复制验证码",
                if hovered { p.accent } else { p.accent_dim },
            );
        }
        text_out_center(
            hdc,
            e,
            Role::Small,
            &all_rect,
            "复制全文",
            if hovered { p.accent } else { p.accent_dim },
        );
        if let Some(chip) = reply_rect {
            text_out_center(
                hdc,
                e,
                Role::Small,
                &chip,
                "回复",
                if selected || hovered {
                    p.accent
                } else {
                    p.accent_dim
                },
            );
        }
        // 行 2：包名（弱强调）+ 正文，两者都只许用按钮之外的宽度
        let pkg = truncate_px(
            hdc,
            e,
            Role::Small,
            &item.package,
            (text_right - text_x).max(e.px(60)) - e.px(48),
        );
        let end = text_out(
            hdc,
            e,
            Role::Small,
            text_x,
            y + e.px(32),
            &pkg,
            p.accent_dim,
        );
        let body = if item.text.is_empty() {
            "（内容未同步）"
        } else {
            item.text.as_str()
        };
        let x2 = end + e.px(8);
        let txt = truncate_px(hdc, e, Role::Small, body, (text_right - x2).max(e.px(24)));
        text_out(hdc, e, Role::Small, x2, y + e.px(32), &txt, p.sub_text);
        if slot + 1 < visible {
            divider(hdc, row.left, right, row.bottom, p.divider);
        }
    }

    if let Some(thumb) = notify_bar_thumb(e, st) {
        fill_round(hdc, &thumb, p.accent_dim, e.px(4));
    }
}

// ---------- 页：剪贴板 ----------

#[cfg(windows)]
fn paint_clipboard(hdc: HDC, e: &Env, st: &UiState, w: i32) {
    let p = &e.pal;
    let right = w - e.px(CONTENT_R_PAD);
    text_out(
        hdc,
        e,
        Role::Title,
        e.px(CONTENT_L),
        e.px(TITLE_Y),
        "剪贴板",
        p.body_text,
    );

    let tr = clip_toggle_rect(e);
    let on = st.clip_sync;
    fill_round(hdc, &tr, if on { p.accent } else { p.btn_bg }, e.px(8));
    let label = if on {
        "剪贴板同步：开"
    } else {
        "剪贴板同步：关"
    };
    text_out_center(
        hdc,
        e,
        Role::Body,
        &tr,
        label,
        if on { 0xFFFFFF } else { p.btn_text },
    );
    button(
        hdc,
        e,
        &clip_send_rect(e),
        "发送本机剪贴板",
        false,
        st.list_hover == Some((2, 0)),
    );
    // 这颗按钮平时用不上（电脑复制会自动同步），所以就在它下面说清它是"没同步上时手动重发"
    text_out(
        hdc,
        e,
        Role::Small,
        e.px(CONTENT_L),
        e.px(CLIP_SEND_Y + CLIP_BTN_H + 8),
        &truncate_px(
            hdc,
            e,
            Role::Small,
            "平时电脑复制的内容会自动同步；这个按钮用于没同步上时手动重发一次。",
            right - e.px(CONTENT_L + 12),
        ),
        p.sub_text,
    );

    let mut y = e.px(224);
    let body_w = right - e.px(CONTENT_L + 24); // 正文从 CONTENT_L+12 起画，右侧留 12
    for (title, value) in [("最近收到", &st.clip_in), ("最近发送", &st.clip_out)] {
        text_out(hdc, e, Role::Small, e.px(CONTENT_L), y, title, p.foot_text);
        let text = if value.is_empty() {
            "（暂无）".to_string()
        } else {
            truncate_px(hdc, e, Role::Body, value, body_w)
        };
        let color = if value.is_empty() {
            p.foot_text
        } else {
            p.body_text
        };
        text_out(
            hdc,
            e,
            Role::Body,
            e.px(CONTENT_L + 12),
            y + e.px(20),
            &text,
            color,
        );
        y += e.px(84);
    }

    text_out(
        hdc,
        e,
        Role::Small,
        e.px(CONTENT_L),
        y,
        "手机端受 Android 10+ 限制：手机复制的内容要切回前台时才补同步。",
        p.sub_text,
    );
}

// ---------- 页：文件 ----------

/// 传输任务行右侧的状态文案配色
#[cfg(windows)]
fn task_state_color(e: &Env, state: &str) -> u32 {
    let p = &e.pal;
    if state.starts_with("已完成") {
        p.ok_text
    } else if state.starts_with("失败") || state.starts_with("校验失败") {
        p.err_text
    } else {
        p.sub_text
    }
}

#[cfg(windows)]
fn paint_files(hdc: HDC, e: &Env, st: &UiState, w: i32) {
    let p = &e.pal;
    let right = w - e.px(CONTENT_R_PAD);
    text_out(
        hdc,
        e,
        Role::Title,
        e.px(CONTENT_L),
        e.px(TITLE_Y),
        "文件",
        p.body_text,
    );

    // 状态行：TCP 通道（文件走 TCP，未就绪时只排队）+ UDP 发现到的对端 IP
    let (status, color) = if st.tcp_ready {
        if st.peer_lan_ip.is_empty() {
            (
                "TCP 通道已就绪 · UDP 发现中（信标每 3s 广播）· 256KB 分块".to_string(),
                p.ok_text,
            )
        } else {
            (
                format!(
                    "TCP 通道已就绪 · UDP 发现对端 {} · 256KB 分块",
                    st.peer_lan_ip
                ),
                p.ok_text,
            )
        }
    } else if st.link_paired() {
        (
            "TCP 通道未就绪（UDP 发现中，等待手机接入 55676）· 文件将排队等待".to_string(),
            p.sub_text,
        )
    } else {
        (
            "未连接：请先在「连接」页与手机完成配对".to_string(),
            p.sub_text,
        )
    };
    text_out(
        hdc,
        e,
        Role::Body,
        e.px(CONTENT_L),
        e.px(SUBTITLE_Y),
        &status,
        color,
    );

    text_out(
        hdc,
        e,
        Role::Small,
        e.px(CONTENT_L),
        e.px(FILE_SEND_LABEL_Y),
        "发送到手机（点「选择…」挑文件，也可以把文件直接拖进这个窗口）",
        p.foot_text,
    );
    input_box(
        hdc,
        e,
        &file_path_rect(e),
        &st.send_path_input,
        "先点右边的「选择…」",
        st.input_focus == crate::state::FOCUS_SEND_PATH,
        st.list_hover == Some((7, 0)),
    );
    button(
        hdc,
        e,
        &file_browse_rect(e),
        "选择…",
        false,
        st.list_hover == Some((19, 0)),
    );
    icon_button(
        hdc,
        e,
        &file_send_rect(e),
        Icon::Send,
        "发送到手机",
        true,
        st.list_hover == Some((8, 0)),
    );

    text_out(
        hdc,
        e,
        Role::Small,
        e.px(CONTENT_L),
        e.px(FILE_INBOX_LABEL_Y),
        "收件目录（手机发来的文件落盘位置）",
        p.foot_text,
    );
    let chip = inbox_pick_rect(e);
    let inbox = truncate_px(
        hdc,
        e,
        Role::Small,
        &st.inbox_dir,
        chip.left - e.px(CONTENT_L) - e.px(8),
    );
    text_out(
        hdc,
        e,
        Role::Small,
        e.px(CONTENT_L),
        e.px(FILE_INBOX_Y),
        &inbox,
        p.body_text,
    );
    let hov = st.list_hover == Some((21, 0));
    fill_round(
        hdc,
        &chip,
        if hov { p.accent } else { p.row_hover_bg },
        e.px(8),
    );
    text_out_center(
        hdc,
        e,
        Role::Small,
        &chip,
        "改到别处…",
        if hov { 0xFFFFFF } else { p.accent },
    );

    text_out(
        hdc,
        e,
        Role::Small,
        e.px(CONTENT_L),
        e.px(FILE_LIST_LABEL_Y),
        &format!(
            "传输列表（最近 {} 条）",
            st.file_tasks.len().min(FILE_MAX_ROWS)
        ),
        p.foot_text,
    );
    if st.file_tasks.is_empty() {
        text_out(
            hdc,
            e,
            Role::Body,
            e.px(CONTENT_L),
            e.px(FILE_ROW_Y0 + 6),
            "暂无传输任务",
            p.sub_text,
        );
        text_out(
            hdc,
            e,
            Role::Small,
            e.px(CONTENT_L),
            e.px(FILE_ROW_Y0 + 6) + role_line_h(e, Role::Body),
            "选好文件点「发送到手机」即可推送；手机发来的文件会自动落到上面的收件目录",
            p.foot_text,
        );
        return;
    }
    for (i, task) in st.file_tasks.iter().take(FILE_MAX_ROWS).enumerate() {
        let row = file_row_rect(e, i);
        let send = task.direction == crate::state::TASK_DIR_SEND;
        let done = task.state.starts_with("已完成");
        // 「取消」只画在真能取消的行上；命中判定用的是同一个 rect（同一个判据）
        let cancel = file_cancel_hit_rect(e, st, i);
        // 状态文案的右界：有「取消」时让到它左边，两处共用一份几何就不会互相压字
        let state_right = cancel.map(|r| r.left - e.px(6)).unwrap_or(right - e.px(12));
        // 方向箭头（矢量图标：不依赖字体里的箭头字形，随主题换色、随 DPI 无损缩放）
        let x0 = row.left + e.px(12);
        let (arrow, arrow_color) = if send {
            (Icon::Upload, p.accent)
        } else {
            (Icon::Download, p.ok_text)
        };
        let isz = e.px(NAV_ICON);
        icons::draw(hdc, arrow, x0, row.top + e.px(3), isz, arrow_color);
        let name_x = x0 + isz + e.px(8);
        let name_max = state_right - e.px(150) - name_x;
        let name = truncate_px(hdc, e, Role::BodyStrong, &task.name, name_max);
        text_out(
            hdc,
            e,
            Role::BodyStrong,
            name_x,
            row.top + e.px(5),
            &name,
            p.body_text,
        );
        // 右侧：状态 + 百分比 +（已知大小时）实时速度——只有百分比看不出"是不是卡住了、还要等多久"。
        let mut state_txt = format!("{} {}%", task.state, task.percent);
        if task.speed_kbps > 0 {
            state_txt += &format!(" · {:.1} MB/s", task.speed_kbps as f64 / 1024.0);
        }
        text_out_right(
            hdc,
            e,
            Role::Small,
            state_right,
            row.top + e.px(7),
            &state_txt,
            task_state_color(e, &task.state),
        );
        let bar = RECT {
            left: x0,
            top: row.bottom - e.px(11),
            right: state_right,
            bottom: row.bottom - e.px(7),
        };
        progress_bar(hdc, e, &bar, task.percent, done);
        if let Some(rc) = cancel {
            let hovered = st.list_hover == Some((20, i));
            fill_round(
                hdc,
                &rc,
                if hovered { p.accent } else { p.row_hover_bg },
                e.px(8),
            );
            text_out_center(
                hdc,
                e,
                Role::Small,
                &rc,
                "取消",
                if hovered { 0xFFFFFF } else { p.accent },
            );
        }
    }
}

// ---------- 页：媒体（播放状态与反向控制）----------

/// 媒体页：展示手机正在放什么，并提供五个控制按钮。刻意**不搬运音频**：这里只有元数据与控制指令。
/// 未收到任何状态时显示空态，而不是显示一堆假零值。
#[cfg(windows)]
fn paint_media(hdc: HDC, e: &Env, st: &UiState) {
    let p = &e.pal;
    text_out(
        hdc,
        e,
        Role::Title,
        e.px(CONTENT_L),
        e.px(TITLE_Y),
        "媒体",
        p.body_text,
    );
    text_out(
        hdc,
        e,
        Role::Body,
        e.px(CONTENT_L),
        e.px(SUBTITLE_Y),
        "手机上正在播放的内容；LinkX 只同步状态与指令，不搬运音频",
        p.sub_text,
    );

    let Some(m) = st.media.as_ref() else {
        icons::draw(
            hdc,
            Icon::Music,
            e.px(CONTENT_L),
            e.px(MEDIA_CARD_Y + 24),
            e.px(NAV_ICON),
            p.foot_text,
        );
        text_out(
            hdc,
            e,
            Role::Body,
            e.px(CONTENT_L + 40),
            e.px(MEDIA_CARD_Y + 40),
            if crate::features::enabled(crate::features::Module::MediaControl) {
                "手机还没有在播放任何东西"
            } else {
                "「媒体控制」模块已关闭：请到「功能」页开启并重新启动"
            },
            p.foot_text,
        );
        text_out(
            hdc,
            e,
            Role::Small,
            e.px(CONTENT_L),
            e.px(MEDIA_CARD_Y + 66),
            "在手机上打开音乐 / 播客 App，几秒内这里就会出现曲目信息",
            p.foot_text,
        );
        return;
    };

    let card = lrect(
        e,
        CONTENT_L,
        MEDIA_CARD_Y,
        CONTENT_L + 620,
        MEDIA_CARD_Y + MEDIA_CARD_H,
    );
    fill_round(hdc, &card, p.card_bg, e.px(12));
    stroke_round(hdc, &card, p.divider, e.px(12));
    let cx = card.left + e.px(20);
    let title = if m.title.is_empty() {
        "（该应用未提供曲目名）".to_string()
    } else {
        m.title.clone()
    };
    let tw = text_extent(hdc, e, Role::Title, &title).0;
    let maxw = card.right - cx - e.px(96);
    let shown = if tw > maxw {
        truncate_px(hdc, e, Role::Title, &title, maxw)
    } else {
        title.clone()
    };
    text_out(
        hdc,
        e,
        Role::Title,
        cx,
        card.top + e.px(18),
        &shown,
        p.body_text,
    );
    let mut sub = Vec::new();
    if !m.artist.is_empty() {
        sub.push(m.artist.clone());
    }
    if !m.album.is_empty() {
        sub.push(m.album.clone());
    }
    text_out(
        hdc,
        e,
        Role::Body,
        cx,
        card.top + e.px(50),
        &sub.join(" · "),
        p.sub_text,
    );
    // 播放状态 + 来源包名：出问题时"谁在放"是第一个要看的信息
    text_out(
        hdc,
        e,
        Role::Small,
        cx,
        card.top + e.px(78),
        &format!(
            "{}{}{}  ·  来源 {}",
            if m.playing { "播放中" } else { "已暂停" },
            if m.speed_x100 != 100 {
                format!("  {}x", m.speed_x100 as f64 / 100.0)
            } else {
                String::new()
            },
            if m.duration_ms > 0 {
                format!("  {}/{}", fmt_ms(m.position_ms), fmt_ms(m.duration_ms))
            } else {
                String::new()
            },
            if m.package.is_empty() {
                "未知"
            } else {
                &m.package
            }
        ),
        p.foot_text,
    );
    // 进度条：只有拿到时长才画，否则画一条空槽会让人觉得"卡在 0%"
    if m.duration_ms > 0 {
        let bar = RECT {
            left: cx,
            top: card.bottom - e.px(26),
            right: card.right - e.px(20),
            bottom: card.bottom - e.px(18),
        };
        let pct = ((m.position_ms as f64 / m.duration_ms as f64) * 100.0).clamp(0.0, 100.0) as u8;
        progress_bar(hdc, e, &bar, pct, pct >= 100);
    }

    // 控制按钮（数组长度由 MEDIA_BTN_COUNT 锁住：个数不符编译期就报错）
    let center = if m.playing {
        (Icon::Pause, "暂停")
    } else {
        (Icon::Play, "播放")
    };
    let btns: [(Icon, &str); MEDIA_BTN_COUNT] = [
        (Icon::Prev, "上一首"),
        center,
        (Icon::Next, "下一首"),
        (Icon::VolDown, "音量 −"),
        (Icon::VolUp, "音量 +"),
    ];
    for (i, (icon, name)) in btns.iter().enumerate() {
        icon_button(
            hdc,
            e,
            &media_btn_rect(e, i),
            *icon,
            name,
            i == 1,
            st.list_hover == Some((18, i)),
        );
    }
    // 音量以手机上报值为基准（电脑不自己记账，否则用户在手机上调一次就永远对不上）。
    // 但安卓在音乐流未激活时 getStreamVolume 会返回 0 而实际并非静音：那种情况宁可不说，也不要说一个错的数。
    let vol_known = m.volume > 0 || (m.playing && m.volume >= 0);
    let vol_text = if vol_known {
        format!("手机媒体音量 {}%", m.volume.clamp(0, 100))
    } else {
        "手机媒体音量 —（当前无活动播放，读到的 0 不代表静音）".to_string()
    };
    text_out(
        hdc,
        e,
        Role::Small,
        e.px(CONTENT_L),
        e.px(MEDIA_VOL_Y + 6),
        &vol_text,
        p.foot_text,
    );
    if vol_known {
        let vbar = RECT {
            left: e.px(CONTENT_L + 130),
            top: e.px(MEDIA_VOL_Y + 10),
            right: e.px(CONTENT_L + 330),
            bottom: e.px(MEDIA_VOL_Y + 18),
        };
        progress_bar(hdc, e, &vbar, m.volume.clamp(0, 100) as u8, false);
    }
}

/// 毫秒 → `分:秒`（超过一小时带时位）
#[cfg(windows)]
fn fmt_ms(ms: i64) -> String {
    let s = ms.max(0) / 1000;
    let (h, m, sec) = (s / 3600, (s % 3600) / 60, s % 60);
    if h > 0 {
        format!("{h}:{m:02}:{sec:02}")
    } else {
        format!("{m}:{sec:02}")
    }
}

// ---------- 页：功能（运行期功能开关）----------

#[cfg(windows)]
fn paint_features(hdc: HDC, e: &Env, st: &UiState) {
    use crate::features;
    let p = &e.pal;
    text_out(
        hdc,
        e,
        Role::Title,
        e.px(CONTENT_L),
        e.px(TITLE_Y),
        "功能",
        p.body_text,
    );
    text_out(
        hdc,
        e,
        Role::Body,
        e.px(CONTENT_L),
        e.px(SUBTITLE_Y),
        "关掉不用的模块，重启后就不再加载它（不起线程、不开端口）",
        p.sub_text,
    );

    for (i, m) in features::ALL.iter().enumerate() {
        let y = e.px(FEAT_Y0 + i as i32 * FEAT_ROW_H);
        let on = m.wanted(st);
        let right = text_out(
            hdc,
            e,
            Role::BodyStrong,
            e.px(CONTENT_L),
            y,
            m.label(),
            p.body_text,
        );
        if features::enabled(*m) != on {
            text_out(
                hdc,
                e,
                Role::Small,
                right + e.px(10),
                y + e.px(3),
                "待重启生效",
                p.accent_dim,
            );
        }
        text_out(
            hdc,
            e,
            Role::Small,
            e.px(CONTENT_L),
            y + e.px(24),
            if on { m.about() } else { m.consequence() },
            p.sub_text,
        );
        let sr = feat_switch_rect(e, i);
        draw_switch(hdc, e, &sr, on, st.list_hover == Some((14, i)));
    }

    // 摘要两行：加载了什么 + 现在占多少内存。收益必须被看见，而不是只写在"节省内存"这句文案里（用户无从核对）。
    let loaded = features::modules_of(features::active_bits());
    text_out(
        hdc,
        e,
        Role::Small,
        e.px(CONTENT_L),
        e.px(FEAT_SUMMARY_Y),
        &format!(
            "本次启动已加载：{}",
            if loaded.is_empty() {
                "无（仅保持连接）".to_string()
            } else {
                loaded.join("、")
            }
        ),
        p.foot_text,
    );
    text_out(
        hdc,
        e,
        Role::Small,
        e.px(CONTENT_L),
        e.px(FEAT_SUMMARY_Y + 20),
        &format!(
            "当前内存占用 {}（重启后按上面的开关重新加载）",
            match features::working_set_mb() {
                // 取不到就说取不到：显示 0 MB 会被读成"关掉模块真的省了内存"，而这一点已被实测否证。
                Some(mb) => format!("{mb} MB"),
                None => "未知".to_string(),
            }
        ),
        p.foot_text,
    );

    let changes = pending_modules(st);
    if changes.is_empty() {
        return;
    }
    let rc = lrect(
        e,
        CONTENT_L,
        FEAT_BANNER_Y,
        CONTENT_L + FEAT_BANNER_W,
        FEAT_BANNER_Y + FEAT_BANNER_H,
    );
    fill_round(hdc, &rc, blend(p.card_bg, p.accent, 0.10), e.px(10));
    stroke_round(hdc, &rc, blend(p.divider, p.accent, 0.45), e.px(10));
    text_out(
        hdc,
        e,
        Role::BodyStrong,
        rc.left + e.px(16),
        rc.top + e.px(12),
        &format!("有 {} 项改动待重启生效", changes.len()),
        p.body_text,
    );
    for (j, (m, want)) in changes.iter().enumerate() {
        text_out(
            hdc,
            e,
            Role::Small,
            rc.left + e.px(16),
            rc.top + e.px(38) + (j as i32) * e.px(18),
            &format!(
                "{}：重启后{}",
                m.label(),
                if *want {
                    "继续加载"
                } else {
                    "不再加载"
                }
            ),
            p.sub_text,
        );
    }
    button(
        hdc,
        e,
        &feat_restart_rect(e),
        "重新启动",
        true,
        st.list_hover == Some((15, 0)),
    );
}

/// 重启确认弹窗：整窗模态，只有「重新启动」/「稍后启动」两个出口
#[cfg(windows)]
fn paint_restart_modal(hdc: HDC, e: &Env, st: &UiState) {
    let p = &e.pal;
    // 遮罩：把窗口底色压暗，确保不会误看成页面本体
    fill(
        hdc,
        &rect(0, 0, e.win_w, e.win_h),
        blend(p.body_bg, 0x000000, 0.45),
    );
    let card = modal_card_rect(e);
    fill_round(hdc, &card, p.card_bg, e.px(12));
    stroke_round(hdc, &card, p.divider, e.px(12));

    text_out(
        hdc,
        e,
        Role::Title,
        card.left + e.px(24),
        card.top + e.px(20),
        "需要重新启动",
        p.body_text,
    );
    text_out(
        hdc,
        e,
        Role::Body,
        card.left + e.px(24),
        card.top + e.px(50),
        "改动已经保存；重新启动后按上面的开关重新加载模块。",
        p.sub_text,
    );
    // 逐条列出会改变什么。刻意不带 consequence()：那句话就写在开关下面，塞进弹窗只会溢出卡片边界（真机截图上确实溢出过）。
    let mut ly = card.top + e.px(82);
    for (m, want) in pending_modules(st).iter() {
        text_out(
            hdc,
            e,
            Role::Small,
            card.left + e.px(24),
            ly,
            &format!(
                "· {}：重启后{}",
                m.label(),
                if *want {
                    "继续加载"
                } else {
                    "不再加载"
                }
            ),
            p.sub_text,
        );
        ly += e.px(20);
    }
    let (later, now) = modal_btn_rects(e);
    button(
        hdc,
        e,
        &later,
        "稍后启动",
        false,
        st.list_hover == Some((17, 0)),
    );
    button(
        hdc,
        e,
        &now,
        "重新启动",
        true,
        st.list_hover == Some((16, 0)),
    );
}

// ---------- 关闭询问弹窗（自绘模态）----------

/// 勾选框：方框 + 勾 + 文字。整块区域就是命中区（见 `close_prompt_rects`），点方框和点文字一样
#[cfg(windows)]
fn draw_check(hdc: HDC, e: &Env, rc: &RECT, on: bool, hover: bool, label: &str) {
    let p = &e.pal;
    let side = e.px(18);
    let mid = (rc.top + rc.bottom) / 2;
    let box_rc = rect(rc.left, mid - side / 2, rc.left + side, mid + side / 2);
    let bg = if on {
        p.accent
    } else if hover {
        p.row_hover_bg
    } else {
        p.card_bg
    };
    fill_round(hdc, &box_rc, bg, e.px(4));
    stroke_round(
        hdc,
        &box_rc,
        if on { p.accent } else { p.sub_text },
        e.px(4),
    );
    if on {
        let (w, h) = (box_rc.right - box_rc.left, box_rc.bottom - box_rc.top);
        let pts = [
            windows::Win32::Foundation::POINT {
                x: box_rc.left + w / 5,
                y: box_rc.top + h / 2,
            },
            windows::Win32::Foundation::POINT {
                x: box_rc.left + w * 2 / 5,
                y: box_rc.bottom - h / 4,
            },
            windows::Win32::Foundation::POINT {
                x: box_rc.right - w / 5,
                y: box_rc.top + h / 4,
            },
        ];
        let old =
            unsafe { SelectObject(hdc, HGDIOBJ(icons::pen_solid(e.px(2).max(1), 0xFFFFFF).0)) };
        let _ = unsafe { windows::Win32::Graphics::Gdi::Polyline(hdc, &pts) };
        unsafe {
            SelectObject(hdc, old);
        }
    }
    text_out(
        hdc,
        e,
        Role::Small,
        box_rc.right + e.px(8),
        mid - role_line_h(e, Role::Small) / 2,
        label,
        p.body_text,
    );
}

/// 关闭询问弹窗：整窗模态，三个出口加一个勾选，背后一概点不到（分派见 `hit_test`）
#[cfg(windows)]
fn paint_close_modal(hdc: HDC, e: &Env, st: &UiState) {
    let p = &e.pal;
    let r = close_prompt_rects(e);
    fill(
        hdc,
        &rect(0, 0, e.win_w, e.win_h),
        blend(p.body_bg, 0x000000, 0.45),
    );
    fill_round(hdc, &r.card, p.card_bg, e.px(12));
    stroke_round(hdc, &r.card, p.divider, e.px(12));
    text_out(
        hdc,
        e,
        Role::Title,
        r.title.left,
        r.title.top,
        "关闭 LinkX？",
        p.body_text,
    );
    let body = "最小化到托盘只是收起窗口，程序继续在后台运行。";
    let body = truncate_px(hdc, e, Role::Body, body, r.body.right - r.body.left);
    text_out(
        hdc,
        e,
        Role::Body,
        r.body.left,
        r.body.top,
        &body,
        p.sub_text,
    );
    button(
        hdc,
        e,
        &r.cancel,
        "取消",
        false,
        st.list_hover == Some((28, 0)),
    );
    button(
        hdc,
        e,
        &r.exit,
        "退出程序",
        false,
        st.list_hover == Some((27, 0)),
    );
    // 默认动作（回车）排在最右的主行位上：它不销毁任何东西，按错了也不可损失
    button(
        hdc,
        e,
        &r.minimize,
        "最小化到托盘",
        true,
        st.list_hover == Some((26, 0)),
    );
    draw_check(
        hdc,
        e,
        &r.remember,
        st.close_remember,
        st.list_hover == Some((29, 0)),
        "记住我的选择，不再询问",
    );
}

// ---------- 页：关于 ----------

#[cfg(windows)]
/// 关于页：文案与外链都取自 `crate::about`（单一来源），这里只管排版——
/// 改一个字不该动绘制代码，改一个地址更不该。
#[cfg(windows)]
fn paint_about(hdc: HDC, e: &Env, _st: &UiState) {
    let p = &e.pal;
    text_out(
        hdc,
        e,
        Role::Display,
        e.px(CONTENT_L),
        e.px(ABOUT_TITLE_Y),
        crate::about::APP_NAME,
        p.body_text,
    );
    text_out(
        hdc,
        e,
        Role::Body,
        e.px(CONTENT_L),
        e.px(ABOUT_TAGLINE_Y),
        crate::about::TAGLINE,
        p.body_text,
    );
    text_out(
        hdc,
        e,
        Role::Small,
        e.px(CONTENT_L),
        e.px(ABOUT_AUTHOR_Y),
        crate::about::AUTHOR,
        p.sub_text,
    );
    // 这一页只留"是什么 / 谁做的"三行字：外链按钮、许可证行与"可离开本页"提示都撤下——版本与许可证在「功能」页和仓库里都看得到。
}

#[cfg(windows)]
fn paint_settings(hdc: HDC, e: &Env, st: &UiState) {
    let p = &e.pal;
    text_out(
        hdc,
        e,
        Role::Title,
        e.px(CONTENT_L),
        e.px(TITLE_Y),
        "设置",
        p.body_text,
    );

    text_out(
        hdc,
        e,
        Role::Small,
        e.px(CONTENT_L),
        e.px(SET_THEME_Y - 22),
        "外观",
        p.foot_text,
    );
    let segs = theme_seg_rects(e);
    let labels = ["跟随系统", "浅色", "深色"];
    let values = [Theme::System, Theme::Light, Theme::Dark];
    for i in 0..3 {
        let active = st.theme == values[i];
        let hovered = st.list_hover == Some((4, i));
        if active {
            fill_round(hdc, &segs[i], p.accent, e.px(8));
        } else if hovered {
            fill_round(hdc, &segs[i], p.row_hover_bg, e.px(8));
        } else {
            fill_round(hdc, &segs[i], p.btn_bg, e.px(8));
        }
        let fg = if active { 0xFFFFFF } else { p.btn_text };
        text_out_center(hdc, e, Role::Body, &segs[i], labels[i], fg);
    }
    let hint = match st.theme {
        Theme::System => {
            if e.dark {
                "当前跟随系统「应用模式」（系统为深色）"
            } else {
                "当前跟随系统「应用模式」（系统为浅色）"
            }
        }
        _ => "已手动指定外观；选「跟随系统」可恢复自动切换",
    };
    // 提示挪到分段控件右侧：原来占一整行，把下面所有区块往下顶，最矮窗口下会顶到页脚
    let hint_x = CONTENT_L + 3 * (SET_SEG_W + 8) + 12;
    let hint = truncate_px(hdc, e, Role::Small, hint, e.win_w - e.px(hint_x + 24));
    text_out(
        hdc,
        e,
        Role::Small,
        e.px(hint_x),
        e.px(SET_THEME_Y + 10),
        &hint,
        p.sub_text,
    );

    // 开关行（第 5 行 = Debug 模式；「导出日志」与它同行；最后一行是关闭按钮行为）
    let rows: [(&str, &str, bool); SET_TOGGLES] = [
        (
            "消息弹窗",
            "手机通知到达时弹出 Windows 系统通知",
            st.toast_enabled,
        ),
        (
            "弹窗显示正文",
            if st.toast_show_content {
                "弹窗中直接显示通知内容（关闭后仅提示收到通知）"
            } else {
                "当前仅提示「收到一条通知」，不显示内容"
            },
            st.toast_show_content,
        ),
        ("剪贴板自动同步", "复制的纯文本自动推送到对端", st.clip_sync),
        (
            "自动连接已绑定设备",
            if st.auto_connect {
                "启动与掉线后自动连回（换身份仍要人工确认）"
            } else {
                "当前每次都要在「连接」页手动点一次设备"
            },
            st.auto_connect,
        ),
        (
            "开机自启动",
            if !st.autostart {
                "登录 Windows 后不自动启动"
            } else if !st.autostart_is_ours {
                "当前启动项不是本机写入的内容"
            } else {
                "登录 Windows 后自动启动，且不显示主窗口"
            },
            st.autostart,
        ),
        (
            "Debug 模式",
            if st.debug_enabled {
                "全栈运行日志已开启（落盘 LinkX\\Logs，可导出给开发者）"
            } else {
                "开启后持续记录全栈运行状态与错误，便于定位问题"
            },
            st.debug_enabled,
        ),
    ];
    // 行号与命中判定共用：数组顺序改了就在这里响，而不是"点开关拨的是隔壁那一行"
    debug_assert_eq!(rows.get(SET_AUTOSTART_ROW).map(|r| r.0), Some("开机自启动"));
    debug_assert_eq!(rows.get(SET_DEBUG_ROW).map(|r| r.0), Some("Debug 模式"));
    for (i, (label, desc, on)) in rows.iter().enumerate() {
        let y = e.px(SET_TOGGLE_Y0 + i as i32 * SET_ROW_H);
        text_out(
            hdc,
            e,
            Role::BodyStrong,
            e.px(CONTENT_L),
            y,
            label,
            p.body_text,
        );
        // 说明文字截断在开关列之前：文案再长也不可能穿到开关/按钮底下（不靠人眼核对）
        let desc = truncate_px(hdc, e, Role::Small, desc, e.px(SET_SWITCH_L - 24));
        text_out(
            hdc,
            e,
            Role::Small,
            e.px(CONTENT_L),
            y + e.px(SET_DESC_DY),
            &desc,
            p.sub_text,
        );
        let sr = set_switch_rect(e, i);
        draw_switch(hdc, e, &sr, *on, st.list_hover == Some((5, i)));
    }
    // 关闭按钮行为：右侧那一栏换成一个取值按钮，点一下在三个值之间循环
    {
        let y = e.px(SET_TOGGLE_Y0 + SET_BEHAVIOR_ROW as i32 * SET_ROW_H);
        text_out(
            hdc,
            e,
            Role::BodyStrong,
            e.px(CONTENT_L),
            y,
            "关闭按钮行为",
            p.body_text,
        );
        let desc = match st.close_behavior {
            crate::settings::CloseBehavior::Ask => "点关闭按钮时先问一句",
            crate::settings::CloseBehavior::Minimize => "点关闭按钮就收进托盘，程序继续运行",
            crate::settings::CloseBehavior::Exit => "点关闭按钮就退出程序",
        };
        text_out(
            hdc,
            e,
            Role::Small,
            e.px(CONTENT_L),
            y + e.px(SET_DESC_DY),
            &truncate_px(hdc, e, Role::Small, desc, e.px(SET_SWITCH_L - 24)),
            p.sub_text,
        );
        button(
            hdc,
            e,
            &set_behavior_rect(e),
            st.close_behavior.label(),
            false,
            st.list_hover == Some((25, 0)),
        );
    }
    // Debug 日志导出（任意目录；日志含配对与设备信息，仅交给可信方）
    button(
        hdc,
        e,
        &debug_export_rect(e),
        "导出日志",
        false,
        st.list_hover == Some((13, 0)),
    );

    // 设备管理：手动 IP 兜底 + 已绑定设备（解绑）；台数已并入下方列表标题，不另占一行高度。
    text_out(
        hdc,
        e,
        Role::Small,
        e.px(CONTENT_L),
        e.px(SET_MANUAL_HINT_Y),
        "手动 IP（同网搜不到时兜底）",
        p.foot_text,
    );
    input_box(
        hdc,
        e,
        &manual_ip_rect(e),
        &st.manual_ip_input,
        "192.168.1.23",
        st.input_focus == crate::state::FOCUS_MANUAL_IP,
        st.list_hover == Some((9, 0)),
    );
    button(
        hdc,
        e,
        &manual_ip_btn_rect(e),
        "连接",
        false,
        st.list_hover == Some((10, 0)),
    );
    // 发现状态：手动 IP 的"成功标志"就是这里变成对端 IP
    let found = if st.peer_lan_ip.is_empty() {
        "尚未发现对端 IP".to_string()
    } else {
        format!("对端 {}", st.peer_lan_ip)
    };
    text_out(
        hdc,
        e,
        Role::Small,
        e.px(CONTENT_L + SET_MANUAL_W + SET_MANUAL_BTN_W + 24),
        e.px(SET_MANUAL_Y + 9),
        &found,
        if st.peer_lan_ip.is_empty() {
            p.sub_text
        } else {
            p.ok_text
        },
    );
    text_out(
        hdc,
        e,
        Role::Small,
        e.px(CONTENT_L),
        e.px(SET_BOUND_LABEL_Y),
        &format!(
            "已绑定设备（{} 台，解绑后需重新配对）",
            st.bound_devices.len()
        ),
        p.foot_text,
    );
    if st.bound_devices.is_empty() {
        text_out(
            hdc,
            e,
            Role::Small,
            e.px(CONTENT_L),
            e.px(SET_BOUND_Y0 + 6),
            "暂无已绑定设备（配对成功后出现在这里）",
            p.sub_text,
        );
    }
    for (i, (fp, name)) in st.bound_devices.iter().take(SET_BOUND_MAX).enumerate() {
        let row = bound_row_rect(e, i);
        let hovered = st.list_hover == Some((11, i));
        if hovered {
            fill_round(hdc, &row, p.row_hover_bg, e.px(8));
        }
        let shown = if name.is_empty() {
            "（未知设备）".to_string()
        } else {
            name.clone()
        };
        let end = text_out(
            hdc,
            e,
            Role::Body,
            row.left + e.px(12),
            row.top + e.px(6),
            &shown,
            p.body_text,
        );
        let fp_x = (end + e.px(12)).min(row.right - e.px(240));
        text_out(
            hdc,
            e,
            Role::Small,
            fp_x,
            row.top + e.px(8),
            fp,
            p.foot_text,
        );
        let btn = unbind_btn_rect(e, i);
        button(hdc, e, &btn, "解绑", false, hovered);
        // 行分隔细线：右端的「解绑」按钮需要有所属，否则看着像飘在窗口边上
        fill(
            hdc,
            &rect(row.left + e.px(12), row.bottom - 1, row.right, row.bottom),
            p.divider,
        );
    }

    // 信息（两行）：指纹是 TOFU 人工比对的基准，与版本号同行即可，不必再占一行标题
    let y = e.px(SET_INFO_Y);
    let fp = if st.local_fp.is_empty() {
        "--"
    } else {
        st.local_fp.as_str()
    };
    let info = format!(
        "本机身份指纹（TOFU 比对基准） {} · Core {}",
        fp,
        linkx_core::LINKX_FFI_VERSION
    );
    text_out(
        hdc,
        e,
        Role::Body,
        e.px(CONTENT_L),
        y,
        &truncate_px(hdc, e, Role::Body, &info, e.win_w - e.px(CONTENT_L + 24)),
        p.body_text,
    );
    text_out(
        hdc,
        e,
        Role::Small,
        e.px(CONTENT_L),
        y + e.px(20),
        "数据仅在局域网内点对点传输，不经云端、不留存",
        p.sub_text,
    );
}

// ---------- 页：相册（图片互传）----------

/// 字节数 → 人话（格子上要说清"这张原图多大"，拖出去之前用户得知道自己在搬什么）
#[cfg(windows)]
fn fmt_size(b: i64) -> String {
    if b <= 0 {
        return "大小未知".to_string();
    }
    const K: f64 = 1024.0;
    let v = b as f64;
    if v >= K * K * K {
        format!("{:.2} GB", v / (K * K * K))
    } else if v >= K * K {
        format!("{:.1} MB", v / (K * K))
    } else {
        format!("{:.0} KB", v / K)
    }
}

/// epoch 毫秒 → `2026-09-24 18:30`（UTC）。本壳不引时区库（为显示一行日期拉一个 chrono 不值），
/// 详情页按 UTC 标注——换算差最多一天，不会误导"哪张照片"。
#[cfg(windows)]
fn fmt_date(ms: i64) -> String {
    if ms <= 0 {
        return "时间未知".to_string();
    }
    let days = ms.div_euclid(86_400_000);
    let secs = ms.rem_euclid(86_400_000) / 1000;
    // Howard Hinnant 的 civil_from_days（无分支循环，纯算术）
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_095) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}",
        secs / 3600,
        (secs % 3600) / 60
    )
}

/// 解码过的缩略图位图缓存（`id → DIB section`），与 `UiState.album.thumbs`（JPEG 字节）分两层：
/// JPEG 字节是跨线程数据（worker 写、UI 读），而 GDI 句柄只能在 UI 线程建与删；若每帧现解码，
/// 30fps 重绘时一屏十几张 256px JPEG 会把画面拖成肉眼可见的卡。上限按**字节**（见 `DIB_BYTES_MAX`）。
#[cfg(windows)]
struct DibEntry {
    id: u64,
    bmp: HBITMAP,
    w: i32,
    h: i32,
    /// 最近一次被画到的帧号（LRU 淘汰用；按"最旧插入"淘汰会把翻回上一页刚看过的全删掉）
    used: u64,
}

/// 一张 32bpp 位图占的字节数
#[cfg(windows)]
fn dib_bytes(w: i32, h: i32) -> usize {
    (w.max(0) as usize) * (h.max(0) as usize) * 4
}

/// 解码位图缓存的**字节**上限：256px 长边 × 32bpp ≈ 每张 0.2 MB，8 MB ≈ 40 张，够铺满一整页（24 张）还带富余。
/// 为什么按字节不按条数：窗口拉大时可见格子成倍增加，按条数封顶会淘汰掉**正在显示**的图，
/// 下一帧又得重解码——来回抖动比省下的那点内存贵得多。
#[cfg(windows)]
const DIB_BYTES_MAX: usize = 8 * 1024 * 1024;

#[cfg(windows)]
thread_local! {
    static THUMB_DIBS: RefCell<Vec<DibEntry>> = const { RefCell::new(Vec::new()) };
    /// 一个共享内存 DC（与 `icons.rs` 同一思路：贴完即还原选中对象，绝不逐帧新建）
    static THUMB_MEM_DC: Cell<HDC> = const { Cell::new(HDC(null_mut())) };
    static FRAME_NO: Cell<u64> = const { Cell::new(0) };
}

/// 解码位图缓存当前占多少字节。相册状态行必须把它和编码 JPEG 一起报：只报 JPEG 那一份，
/// 会在真正吃内存的地方读数偏小一个量级——一张 256px 的图解码成 32bpp 是 0.25 MB，而 JPEG 只有 5 KB。
/// 这一行是给"50 MB 红线"用的现场仪表，报小了比不报更糟。
#[cfg(windows)]
pub(crate) fn dib_bytes_total() -> usize {
    THUMB_DIBS.with(|c| c.borrow().iter().map(|e| dib_bytes(e.w, e.h)).sum())
}

/// 收进托盘时清空解码位图缓存（相册那一屏最多攒 8 MB 的 32bpp 位图）。
/// 安全：缓存的键是 `id`，编码 JPEG 仍在 `UiState.album` 里，恢复后画到那一格会重新解码 ——
/// 代价是每张几毫秒的一次解码，换掉的是"藏在托盘里还占着全屏两倍面积的位图"。
#[cfg(windows)]
pub(crate) fn purge_thumb_dibs() {
    THUMB_DIBS.with(|c| {
        for e in c.borrow_mut().drain(..) {
            let _ = unsafe { DeleteObject(HGDIOBJ(e.bmp.0)) };
        }
    });
}

#[cfg(windows)]
fn thumb_mem_dc() -> Option<HDC> {
    let cur = THUMB_MEM_DC.get();
    if !cur.is_invalid() {
        return Some(cur);
    }
    let dc = unsafe { CreateCompatibleDC(None) };
    if dc.is_invalid() {
        return None;
    }
    THUMB_MEM_DC.set(dc);
    Some(dc)
}

/// 取（或解码后建）一张缩略图的 DIB。`Err` 带原因，直接画到格子上。
/// `visible` = 这一帧画得下的 id；淘汰先动**看不见**的，全都能见才动最旧的——否则会"刚画完就被自己挤掉、下一帧再解一遍"。
#[cfg(windows)]
fn thumb_dib(id: u64, jpeg: &[u8], visible: &[u64]) -> Result<(HBITMAP, i32, i32), String> {
    let no = FRAME_NO.get();
    if let Some(hit) = THUMB_DIBS.with(|c| {
        c.borrow()
            .iter()
            .find(|e| e.id == id)
            .map(|e| (e.bmp, e.w, e.h))
    }) {
        THUMB_DIBS.with(|c| {
            if let Some(e) = c.borrow_mut().iter_mut().find(|e| e.id == id) {
                e.used = no;
            }
        });
        return Ok(hit);
    }
    let (bmp, w, h) = crate::wic::decode_jpeg(jpeg)
        .and_then(|b| {
            let (w, h) = (b.width as i32, b.height as i32);
            crate::wic::to_dib_section(&b).map(|bmp| (bmp, w, h))
        })
        .map_err(|e| format!("解码失败: {e}"))?;
    THUMB_DIBS.with(|c| {
        let mut cache = c.borrow_mut();
        while cache.iter().map(|e| dib_bytes(e.w, e.h)).sum::<usize>() + dib_bytes(w, h)
            > DIB_BYTES_MAX
        {
            let victim = cache
                .iter()
                .enumerate()
                .filter(|(_, e)| !visible.contains(&e.id))
                .min_by_key(|(_, e)| e.used)
                // 全在眼前也要腾地方：宁可重解一张，也不让缓存越过那条常数线
                .or_else(|| cache.iter().enumerate().min_by_key(|(_, e)| e.used))
                .map(|(i, _)| i);
            match victim {
                Some(i) => {
                    let old = cache.remove(i);
                    unsafe {
                        let _ = DeleteObject(HGDIOBJ(old.bmp.0));
                    }
                }
                None => break,
            }
        }
        cache.push(DibEntry {
            id,
            bmp,
            w,
            h,
            used: no,
        });
    });
    Ok((bmp, w, h))
}

/// 把缩略图按 **cover** 画进格子：等比放大铺满、居中裁掉超出。不拉伸——照片横竖混着，拉伸每张都变形；
/// 不留黑边——格子对不齐时用户会以为"图没加载出来"。
#[cfg(windows)]
fn draw_thumb_cover(hdc: HDC, cell: &RECT, bmp: HBITMAP, w: i32, h: i32) {
    if w <= 0 || h <= 0 {
        return;
    }
    let Some(dc) = thumb_mem_dc() else {
        return;
    };
    let dw = cell.right - cell.left;
    let dh = cell.bottom - cell.top;
    if dw <= 0 || dh <= 0 {
        return;
    }
    let scale = (dw as f64 / w as f64).max(dh as f64 / h as f64);
    let sw = (dw as f64 / scale).ceil() as i32;
    let sh = (dh as f64 / scale).ceil() as i32;
    let (sw, sh) = (sw.min(w), sh.min(h));
    let sx = ((w - sw) / 2).max(0);
    let sy = ((h - sh) / 2).max(0);
    unsafe {
        let old = SelectObject(dc, HGDIOBJ(bmp.0));
        let _ = SetStretchBltMode(hdc, HALFTONE);
        let _ = StretchBlt(
            hdc, cell.left, cell.top, dw, dh, dc, sx, sy, sw, sh, SRCCOPY,
        );
        SelectObject(dc, old);
    }
}

/// 工具行按钮标签（**绘制与命中同一张表**：槽位含义变了不会出现"点到的不是看到的"）
#[cfg(windows)]
fn album_tool_label(st: &UiState, slot: usize) -> String {
    let sel = st.album.selected.len();
    match slot {
        0 => "刷新".to_string(),
        1 => "上一页".to_string(),
        2 => "下一页".to_string(),
        3 => {
            let all = !st.album.items.is_empty() && sel >= st.album.items.len();
            if all {
                "取消全选".to_string()
            } else {
                "全选".to_string()
            }
        }
        _ => format!("导出({sel})…"),
    }
}

#[cfg(windows)]
fn album_tools_enabled() -> bool {
    crate::features::enabled(crate::features::Module::Album)
}

/// 页：相册。缩略图只在内存，翻页/离开即淘汰；本页不写任何缩略图到磁盘。
#[cfg(windows)]
fn paint_album(hdc: HDC, e: &Env, st: &UiState, w: i32) {
    FRAME_NO.set(FRAME_NO.get().wrapping_add(1));
    let p = &e.pal;
    text_out(
        hdc,
        e,
        Role::Title,
        e.px(CONTENT_L),
        e.px(TITLE_Y),
        "相册",
        p.body_text,
    );

    // 副标题：相册三条请求只走局域网 TCP，通道没就绪时**先说清楚**——"点了没反应"是最不能接受的失败形态。
    let (status, color) = if !album_tools_enabled() {
        (
            "相册模块已在「功能」页关闭：重启后本页不再加载".to_string(),
            p.err_text,
        )
    } else if !st.link_paired() {
        (
            "未连接：先在「连接」页点一下手机，把它连上后相册才有数据来源".to_string(),
            p.sub_text,
        )
    } else if !st.tcp_ready {
        (
            "已配对，但局域网 TCP 通道未就绪：相册请求只走 TCP，现在点任何按钮都取不到图"
                .to_string(),
            p.err_text,
        )
    } else {
        (
            "手机相册预览（照片 + 视频）· 缩略图只放内存，不留盘 · 单击格子选中，按住格子可拖到资源管理器/微信"
                .to_string(),
            p.sub_text,
        )
    };
    text_out(
        hdc,
        e,
        Role::Body,
        e.px(CONTENT_L),
        e.px(SUBTITLE_Y),
        &status,
        color,
    );

    for slot in 0..ALBUM_BTN_W.len() {
        let rc = album_btn_rect(e, slot);
        let hover = st.list_hover == Some((22, slot));
        button(hdc, e, &rc, &album_tool_label(st, slot), slot == 4, hover);
    }

    // 状态行：手机侧的 error **逐字**显示在这里——权限没给 / 相册为空 / 手机读失败是三件不同的事，收成一句"加载失败"就把能行动的信息全丢了。
    let mut line = if st.album.loading {
        format!("正在向手机请求第 {} 页…", st.album.page + 1)
    } else if st.album.items.is_empty() {
        "还没有清单：点「刷新」向手机要第一页".to_string()
    } else {
        let pages = (st.album.total as usize).div_ceil(st.album.per_page.max(1) as usize);
        // 字节数直接上屏：产品口径是"缩略图只放内存、关掉就没有"，那"现在占了多少"就该有现场读数，不该只能靠外部工具去量。
        let mb = (st.album.thumb_bytes() + dib_bytes_total()) as f64 / (1024.0 * 1024.0);
        format!(
            "第 {} 页（每页 {} 张）· 相册共 {} 张 · 已选 {} 张 · 内存里 {} 张缩略图 {:.1} MB",
            st.album.page + 1,
            st.album.per_page,
            st.album.total,
            st.album.selected.len(),
            st.album.thumbs.len(),
            mb
        )
        .to_string()
            + &format!("（约 {pages} 页）")
    };
    if !st.album.error.is_empty() {
        line = format!("手机侧回报：{}", st.album.error);
    }
    text_out(
        hdc,
        e,
        Role::Small,
        e.px(CONTENT_L),
        e.px(ALBUM_STATUS_Y),
        &truncate_px(
            hdc,
            e,
            Role::Small,
            &line,
            w - e.px(CONTENT_L) - e.px(CONTENT_R_PAD),
        ),
        if st.album.error.is_empty() {
            p.sub_text
        } else {
            p.err_text
        },
    );

    let (cols, _) = album_geom(e);
    let base = album_base_row(e, st) * cols.max(1) as usize;
    let cells = album_cell_count(e, st);
    // 位图缓存在这一帧"不许挤掉"的集合：见 `thumb_dib` 的淘汰口径
    let visible_ids: Vec<u64> = st
        .album
        .items
        .iter()
        .skip(base)
        .take(cells)
        .map(|i| i.id)
        .collect();
    for (slot, item) in st.album.items.iter().enumerate().skip(base).take(cells) {
        let slot = slot - base;
        let cell = album_cell_rect(e, slot, cols);
        let selected = st.album.selected.contains(&item.id);
        let hover = st.list_hover == Some((23, base + slot));
        fill(hdc, &cell, p.card_bg);
        match st.album.slot(item.id) {
            Some(crate::state::ThumbSlot::Ready(jpeg)) => {
                match thumb_dib(item.id, jpeg, &visible_ids) {
                    Ok((bmp, tw, th)) => draw_thumb_cover(hdc, &cell, bmp, tw, th),
                    Err(why) => {
                        let _ = why;
                        text_out_center(hdc, e, Role::Small, &cell, "解码失败", p.err_text);
                    }
                }
            }
            Some(crate::state::ThumbSlot::Pending) => {
                text_out_center(hdc, e, Role::Small, &cell, "载入中…", p.foot_text);
            }
            Some(crate::state::ThumbSlot::Failed(_)) => {
                // 格子里只放四个字（放不下），**原因留在悬停详情行**：权限收回、文件已删、链路超时是三件不同的事。
                text_out_center(hdc, e, Role::Small, &cell, "取图失败", p.err_text);
            }
            None => {
                text_out_center(hdc, e, Role::Small, &cell, "等排队", p.foot_text);
            }
        }
        let border = if selected {
            p.accent
        } else if hover {
            p.sub_text
        } else {
            p.divider
        };
        stroke_round(hdc, &cell, border, e.px(6));
        if selected {
            // 右上角勾选标记（尺寸由格子推导，不随 DPI 掉成 1px）
            let d = e.px(22);
            let chip = rect(
                cell.right - d - e.px(4),
                cell.top + e.px(4),
                cell.right - e.px(4),
                cell.top + e.px(4) + d,
            );
            fill_round(hdc, &chip, p.accent, d / 4);
            text_out_center(hdc, e, Role::Small, &chip, "✓", 0xFFFFFF);
        }
        // 视频：右下角时长角标（右下是行业惯例，不和右上角的勾选标记抢位置）；缩略图失败也照样画——时长来自清单，跟这张图能不能取到没关系。
        if item.is_video() {
            let label = item.duration_label();
            let (tw, th) = text_extent(hdc, e, Role::Small, &label);
            let pad = e.px(5);
            let chip = rect(
                cell.right - (tw + pad * 2) - e.px(4),
                cell.bottom - (th + pad * 2) - e.px(4),
                cell.right - e.px(4),
                cell.bottom - e.px(4),
            );
            fill_round(hdc, &chip, 0x000000, e.px(4));
            text_out_center(hdc, e, Role::Small, &chip, &label, 0xFFFFFF);
        }
        // 已经取回、留着等"再拖一次"的那一份标在**左上角**：大文件第一次拖必然等不到（按住那一两秒取不完），
        // 不标用户只会以为"视频拖不出去"是坏了。右下已被时长角标占掉——108 逻辑像素的格子扣两处内边距只剩
        // 约 80 px，`H:MM:SS` 会把角标吃掉；右上是选中勾。
        if st.album.drag_ready_path(item.id).is_some() {
            let label = "可拖出";
            let (tw, th) = text_extent(hdc, e, Role::Small, label);
            let pad = e.px(5);
            let chip = rect(
                cell.left + e.px(4),
                cell.top + e.px(4),
                cell.left + e.px(4) + tw + pad * 2,
                cell.top + e.px(4) + th + pad * 2,
            );
            fill_round(hdc, &chip, p.ok_text, e.px(4));
            text_out_center(hdc, e, Role::Small, &chip, label, 0xFFFFFF);
        }
        // 进度环：原图还在取的那一格，正中画一圈"跑到哪了"；与「可拖出」互斥——到手就停环、改标角标。
        if st.album.drag_ready_path(item.id).is_none() {
            if let Some((got, total)) = st.album.drag_progress_of(item.id) {
                let frac = if total > 0 {
                    got as f32 / total as f32
                } else {
                    0.0
                };
                album_drag_ring(hdc, e, &cell, frac, p.accent);
            }
        }
    }
    if st.album.items.is_empty() && !st.album.loading {
        let hint = if st.album.error.is_empty() {
            "还没有清单：点上面的「刷新」向手机要第一页"
        } else {
            "手机侧没能给出清单，原因见上面那行"
        };
        text_out(
            hdc,
            e,
            Role::Body,
            e.px(CONTENT_L),
            e.px(ALBUM_GRID_Y + 8),
            hint,
            p.sub_text,
        );
    }

    // 详情行：悬停格子的原始信息；没悬停就给操作提示 + "本页还有几张画不下"
    let detail = match st.list_hover.filter(|s| s.0 == 23).map(|s| s.1) {
        Some(i) if st.album.items.get(i).is_some() => {
            let it = &st.album.items[i];
            let dim = if it.width > 0 && it.height > 0 {
                format!("{}×{}", it.width, it.height)
            } else {
                "尺寸未知".to_string()
            };
            let mut line = format!(
                "{} · {} · {dim} · {}",
                it.name,
                fmt_size(it.size_bytes),
                fmt_date(it.mtime_ms)
            );
            // 照片不标类型（相册默认就是照片，标了是噪音）；视频格必须说清时长——它的缩略图和照片毫无区别，不标就只有导出时才知道是视频。
            if it.is_video() {
                line = format!("{line} · 视频，时长 {}", it.duration_label());
            }
            if let Some(crate::state::ThumbSlot::Failed(why)) = st.album.slot(it.id) {
                line = format!("{line} · 缩略图取不到：{why}");
            }
            line
        }
        _ => {
            let off = st.album.items.len().saturating_sub(cells);
            if off > 0 {
                format!(
                    "滚轮或拖右侧滑块上下翻 · 这一页共 {} 张，当前第 {}–{} 张",
                    st.album.items.len(),
                    base + 1,
                    base + cells
                )
            } else {
                "单击格子=选中；按住格子拖动=把原件（照片或视频）拖到资源管理器/微信".to_string()
            }
        }
    };
    let dr = album_detail_rect(e);
    text_out(
        hdc,
        e,
        Role::Small,
        dr.left,
        dr.top + (dr.bottom - dr.top - role_line_h(e, Role::Small)) / 2,
        &truncate_px(hdc, e, Role::Small, &detail, dr.right - dr.left),
        p.foot_text,
    );

    // 滚动条：只有"这一页画不完"时才出现（画得完却给一条不能拖的轨道，是噪声）
    if let Some(thumb) = album_bar_thumb(e, st) {
        let track = album_bar_track(e);
        fill(hdc, &track, p.divider);
        let hot = st.list_hover == Some((24, 0));
        fill_round(
            hdc,
            &thumb,
            if hot { p.accent } else { p.sub_text },
            e.px(4),
        );
    }
}

// ---------- 整窗绘制 ----------

#[cfg(windows)]
fn paint_gdi(hdc: HDC, hwnd: HWND, st: &UiState) {
    if hdc.is_invalid() {
        return;
    }
    let e = &theme::detect(hwnd, st.theme);
    let p = &e.pal;
    // 客户区尺寸取自 Env（detect 时已查过一次 GetClientRect）：命中判定与绘制用同一份窗口尺寸，不会一个按新尺寸画、一个按旧尺寸点。
    let (w, h) = (e.win_w, e.win_h);

    // 整窗铺底（抑制 WM_ERASEBKGND 后必须每帧铺满，否则残影）
    fill(hdc, &rect(0, 0, w, h), p.body_bg);

    let nav_w = e.px(NAV_W);
    fill(hdc, &rect(0, 0, nav_w, h), p.nav_bg);
    // 侧栏与内容区分界（浅色下两底色接近，需一条细线兜住边界）
    fill(hdc, &rect(nav_w, 0, nav_w + 1, h), p.divider);
    text_out(
        hdc,
        e,
        Role::Title,
        e.px(NAV_ICON_X),
        e.px(26),
        "LinkX",
        p.nav_text,
    );
    text_out(
        hdc,
        e,
        Role::Small,
        e.px(NAV_ICON_X),
        e.px(54),
        "手机 · 电脑 互联",
        p.nav_brand_sub,
    );

    let t = anim_t(st);
    for (row, &tab) in nav_visible().iter().enumerate() {
        let (label, icon) = nav_entry(tab).unwrap_or(("", Icon::Link));
        let item_rc = nav_item_rect(e, row);
        let selected = tab == st.active_tab;
        let hovered = st.nav_hover == Some(row) && !selected;
        if selected {
            fill_round(
                hdc,
                &RECT {
                    left: e.px(8),
                    top: item_rc.top + e.px(4),
                    right: item_rc.right - e.px(8),
                    bottom: item_rc.bottom - e.px(4),
                },
                p.nav_sel_bg,
                e.px(8),
            );
            let full_h = (item_rc.bottom - item_rc.top) - e.px(12);
            let bar_h = ((full_h as f32) * t).round() as i32;
            let cy = (item_rc.top + item_rc.bottom) / 2;
            fill_round(
                hdc,
                &rect(e.px(0), cy - bar_h / 2, e.px(3), cy + bar_h / 2 + 1),
                p.accent,
                e.px(2),
            );
        } else if hovered {
            fill_round(
                hdc,
                &RECT {
                    left: e.px(8),
                    top: item_rc.top + e.px(4),
                    right: item_rc.right - e.px(8),
                    bottom: item_rc.bottom - e.px(4),
                },
                p.nav_hover_bg,
                e.px(8),
            );
        }
        // 图标 + 文本（一律左对齐，居中会随字数左右跳动，导航列不齐）
        let icon_size = e.px(NAV_ICON);
        let icon_y = item_rc.top + ((item_rc.bottom - item_rc.top) - icon_size) / 2;
        let icon_color = if selected {
            p.accent
        } else if hovered {
            p.nav_text
        } else {
            p.nav_text_dim
        };
        icons::draw(hdc, icon, e.px(NAV_ICON_X), icon_y, icon_size, icon_color);
        let (_, th) = text_extent(hdc, e, Role::Body, label);
        let ty = item_rc.top + ((item_rc.bottom - item_rc.top) - th) / 2;
        let text_color = if selected {
            p.nav_text_sel
        } else if hovered {
            p.nav_text
        } else {
            p.nav_text_dim
        };
        text_out(hdc, e, Role::Body, e.px(NAV_TEXT_X), ty, label, text_color);
    }

    match st.active_tab {
        TAB_CONNECT => paint_connect(hdc, e, st, w, h),
        TAB_NOTIFY => paint_notifications(hdc, e, st, w),
        TAB_CLIP => paint_clipboard(hdc, e, st, w),
        TAB_FILES => paint_files(hdc, e, st, w),
        TAB_MEDIA => paint_media(hdc, e, st),
        TAB_FEATURES => paint_features(hdc, e, st),
        TAB_ALBUM => paint_album(hdc, e, st, w),
        TAB_ABOUT => paint_about(hdc, e, st),
        // 兜底**不画任何页**：把未知页签画成设置页、配合 `hit_test` 的 `_ => None`，就是"看着是设置、点哪都没反应"，比一片空白更难查。
        TAB_SETTINGS => paint_settings(hdc, e, st),
        other => debug_assert!(other > TAB_ABOUT, "页签 {other} 没有绘制分支"),
    }

    let foot = format!("LinkX Core {}", linkx_core::LINKX_FFI_VERSION);
    text_out(
        hdc,
        e,
        Role::Small,
        e.px(CONTENT_L),
        h - e.px(30),
        &foot,
        p.foot_text,
    );

    // 模态弹窗最后画：它要盖住包括导航与版本条在内的整窗
    if st.restart_prompt {
        paint_restart_modal(hdc, e, st);
    }
    if st.close_prompt {
        paint_close_modal(hdc, e, st);
    }
}

/// 往给定 HDC 绘制整窗（HDC 生命周期由 window.rs 的 BeginPaint/EndPaint 管理）
#[cfg(windows)]
pub(crate) fn paint(hdc: HDC, hwnd: HWND, st: &UiState) {
    paint_gdi(hdc, hwnd, st);
}

// ---------- 排版不变量（编译期钉死）----------
// 两类叠字风险（开关说明压住下一节标题；信息区压住页脚）写成 const 断言而不是单测：改行高/加行的那一刻就编译失败，而不是等截图发现。

#[cfg(windows)]
const _: () = assert!(
    SET_TOGGLE_Y0 + (SET_ROWS as i32 - 1) * SET_ROW_H + SET_DESC_DY + 16 <= SET_MANUAL_HINT_Y,
    "开关说明文字压到设备管理标题"
);
#[cfg(windows)]
const _: () = assert!(
    SET_DESC_DY + 16 <= SET_ROW_H,
    "同一行的说明文字压到了下一行的标题"
);
#[cfg(windows)]
const _: () = assert!(
    SET_AUTOSTART_ROW + 1 == SET_DEBUG_ROW && SET_DEBUG_ROW + 1 == SET_BEHAVIOR_ROW,
    "设置页行序变了要同步 paint 的数组顺序与 hit_test 的行号"
);
#[cfg(windows)]
const _: () = assert!(
    SET_SWITCH_L + SET_BEHAVIOR_BTN_W <= MIN_ROW_W,
    "关闭按钮行为那一列在最小窗口下伸出内容区"
);
#[cfg(windows)]
const _: () = assert!(
    SET_TOGGLE_Y0 + SET_DEBUG_ROW as i32 * SET_ROW_H + 2 + SW_H + 4 <= SET_MANUAL_HINT_Y,
    "导出日志按钮压到设备管理标题"
);
#[cfg(windows)]
const _: () = assert!(
    SET_MANUAL_Y + INPUT_H <= SET_BOUND_LABEL_Y,
    "手动 IP 输入框压到列表标题"
);
#[cfg(windows)]
const _: () = assert!(
    SET_BOUND_Y0 + SET_BOUND_MAX as i32 * SET_BOUND_ROW_H <= SET_INFO_Y,
    "绑定设备列表压到信息区"
);
/// 设置页必须在**最小客户区**里画得完，页脚画在 `h - 30`。口径跟着 `LAYOUT_MIN_H` 走：加一行开关就会把这里顶爆——拦下它的应该是编译器，而不是截图。
#[cfg(windows)]
const _: () = assert!(SET_INFO_BOTTOM + 8 <= LAYOUT_MIN_H - 30, "信息区顶到页脚");
/// 导航项数与三处硬编码的 `match st.active_tab` 必须同步
#[cfg(windows)]
const _: () = assert!(
    NAV_ITEMS.len() == 9,
    "改导航项要同步 hit_test / hover_at / paint_gdi"
);
/// 页签号必须**互不重复**且覆盖 0..NAV_ITEMS.len()：重复会让两格导航指向同一页，
/// 漏号则那一页永远进不去（`TAB_*` 与数组第三项是手工对应的，没有编译器兜住就得断言）
#[cfg(windows)]
const _: () = {
    let mut i = 0;
    while i < NAV_ITEMS.len() {
        let this = NAV_ITEMS[i].2;
        let mut j = 0;
        while j < NAV_ITEMS.len() {
            if i != j && NAV_ITEMS[j].2 == this {
                panic!("导航项页签号重复");
            }
            j += 1;
        }
        i += 1;
    }
};
/// 工具行五个按钮必须排得下最小内容行宽（否则窄窗口下第 5 个按钮画到客户区外）
#[cfg(windows)]
const _: () = assert!(
    ALBUM_BTN_W[0]
        + ALBUM_BTN_W[1]
        + ALBUM_BTN_W[2]
        + ALBUM_BTN_W[3]
        + ALBUM_BTN_W[4]
        + ALBUM_BTN_GAP * (ALBUM_BTN_W.len() as i32 - 1)
        <= MIN_ROW_W,
    "相册工具行在最小窗口下放不下五个按钮"
);
/// 网格必须在状态行之下、详情行与页脚之上（最矮客户区 606 的口径，与设置页那套断言同理由）
#[cfg(windows)]
const _: () = assert!(ALBUM_GRID_Y >= ALBUM_STATUS_Y + 16, "相册网格压到状态行");
#[cfg(windows)]
const _: () = assert!(
    ALBUM_GRID_Y + ALBUM_CELL + ALBUM_DETAIL_H + ALBUM_FOOT_H <= 606,
    "相册最矮客户区里连一格都放不下（详情行会顶穿页脚）"
);

#[cfg(all(test, windows))]
mod layout_tests {
    use super::*;

    /// 文件页操作行：输入框→「选择…」→「发送到手机」必须**从左到右不重叠**，且最右按钮不得越出内容区（写死宽度时窗口一窄就画到客户区外）。
    #[test]
    fn file_ops_row_stays_inside_the_client_at_every_width() {
        for (logical_w, scale) in [(LAYOUT_MIN_W, 1.0f32), (960, 1.25), (1400, 1.0)] {
            let e = crate::theme::test_env((logical_w as f32 * scale) as i32, 606, scale);
            let input = file_path_rect(&e);
            let browse = file_browse_rect(&e);
            let send = file_send_rect(&e);
            let limit = e.px(logical_w - CONTENT_R_PAD);
            assert_eq!(input.left, e.px(CONTENT_L), "输入框必须从内容左边界起");
            assert!(
                browse.left >= input.right,
                "「选择…」压到输入框：{browse:?} vs {input:?}"
            );
            assert!(
                send.left > browse.right,
                "两个按钮重叠：{send:?} vs {browse:?}"
            );
            assert!(
                send.right <= limit,
                "「发送到手机」越出内容区：right={} limit={}",
                send.right,
                limit
            );
            for (name, r) in [("输入框", input), ("选择", browse), ("发送", send)] {
                assert!(r.right > r.left, "{name} 宽度算成反向：{r:?}");
                assert!(r.bottom > r.top, "{name} 高度算成反向：{r:?}");
            }
            // 至少要能看出这是个输入框：把已缩放的坐标再喂给 `lrect` 会双重缩放，控件会一路画到客户区外。
            assert!(
                input.right - input.left >= e.px(160),
                "输入框被挤到 {}px（客户区逻辑宽 {logical_w}、缩放 {scale}）",
                (input.right - input.left) / scale as i32,
            );
        }
    }

    /// 通知列表：可见行数按客户区高度算，画不下的靠滚动条到；残留滚动值必须夹住。
    #[test]
    fn notification_list_scrolls_instead_of_clipping() {
        for (w, h) in [(LAYOUT_MIN_W, 606i32), (LAYOUT_MIN_W, 420), (1400, 900)] {
            let e = crate::theme::test_env(w, h, 1.0);
            let rows = notify_visible_rows(&e);
            assert!(rows >= 1, "再矮也得画得下一行");
            assert!(
                NOTIFY_Y0 + rows as i32 * NOTIFY_ITEM_H + NOTIFY_FOOT_H <= h,
                "{h} 高的客户区算出 {rows} 行，最后一行会被版本条裁掉"
            );
            assert!(
                notify_visible_rows(&e)
                    >= notify_visible_rows(&crate::theme::test_env(w, 420, 1.0)),
                "窗口越高行数反而越少"
            );
        }

        let e = crate::theme::test_env(LAYOUT_MIN_W, 606, 1.0);
        let mut st = UiState::default();
        assert_eq!(notify_max_scroll(&e, &st), 0, "空列表没有可滚的量");
        assert!(
            notify_bar_thumb(&e, &st).is_none(),
            "画得下就不该出现滚动条"
        );

        st.notifications = (0..20)
            .map(|i| crate::state::NotificationItem {
                package: format!("p{i}"),
                ..Default::default()
            })
            .collect();
        let rows = notify_visible_rows(&e);
        assert!(
            st.notifications.len() > rows,
            "20 条必须画不下，否则这条用例没意义"
        );
        assert_eq!(notify_max_scroll(&e, &st), 20 - rows);
        assert!(notify_bar_thumb(&e, &st).is_some(), "画不下就必须有滚动条");

        // 滚过头要把 base 夹回最后一屏，而不是指着不存在的条目
        st.notify_scroll = 999;
        assert_eq!(notify_base(&e, &st), 20 - rows);
        st.notifications.clear();
        assert_eq!(notify_base(&e, &st), 0, "列表清空后 base 必须归零");

        // 轨道必须落在通知行之外的留白里：压在行上就会盖住行末的时间文字
        let row = notification_row_rect(&e, 0);
        let track = notify_bar_track(&e);
        assert!(
            track.left >= row.right,
            "滚动条压到通知行：track {track:?} vs row {row:?}"
        );
        // 轨道还要跟着内容右沿走：以前它锚在 700 逻辑像素的封顶上，窗口一拉大就浮在窗口中间
        let wide = crate::theme::test_env(1400, 606, 1.0);
        assert!(
            notify_bar_track(&wide).left > track.left + 400,
            "窗口拉宽后滚动条没跟着走"
        );
        assert!(
            notification_row_rect(&wide, 0).right > row.right + 400,
            "窗口拉宽后通知行没跟着走"
        );
    }

    /// 回复条只有「输入框 + 发送」：复制是"这条通知"的动作，不该挤在"正在写的这句回复"旁边。
    /// 绘制、命中、悬停三处都调同一个 `notify_reply_rects`，所以这里只需断言几何本身。
    #[test]
    fn reply_bar_is_input_and_send_only() {
        for w in [LAYOUT_MIN_W, 1400i32] {
            let e = crate::theme::test_env(w, 606, 1.0);
            let (input, send) = notify_reply_rects(&e);
            assert!(input.right <= send.left, "输入框伸进了「发送」");
            assert!(
                input.right - input.left >= e.px(200),
                "最窄布局下输入框被按钮挤没了（{w} 宽）"
            );
            assert!(
                send.right - send.left >= e.px(40),
                "「发送」挤到点不动（{w} 宽）"
            );
        }
    }

    /// 通知行右侧那三颗按钮（复制验证码 / 复制全文 / 回复）不能压在文字上，也不能伸进行末留给
    /// 时间的那条带子：两行文字的右界要先让给最左那颗。四种组合都要成立 —— 少一颗时剩下的会挪位
    #[test]
    fn notification_text_yields_to_its_buttons() {
        for w in [LAYOUT_MIN_W, 1400i32] {
            let e = crate::theme::test_env(w, 606, 1.0);
            let right = e.px(w - CONTENT_R_PAD);
            let text_x = notification_row_rect(&e, 0).left + e.px(12);
            for (has_code, can_reply) in
                [(true, true), (false, true), (true, false), (false, false)]
            {
                let (code, reply, all) = notification_chips(&e, 0, has_code, can_reply);
                assert_eq!(code.is_some(), has_code, "复制验证码跟着有没有码走");
                assert_eq!(reply.is_some(), can_reply, "回复跟着能不能回走");
                let tr = notify_text_right(&e, all.left, right);
                for chip in [&code, &reply].into_iter().flatten() {
                    assert!(tr <= chip.left, "{w} 宽：文字右界压进按钮");
                    assert!(
                        chip.right + e.px(NOTIFY_TIME_W) <= right,
                        "{w} 宽：按钮伸进了行末留给时间的那条带子"
                    );
                }
                assert!(tr <= all.left, "{w} 宽：正文没让给「复制全文」");
                assert!(
                    all.right + e.px(NOTIFY_TIME_W) <= right,
                    "{w} 宽：「复制全文」伸进时间带子"
                );
                assert!(
                    tr - text_x >= e.px(60),
                    "{w} 宽把文字区挤到截不出字：{tr} vs {text_x}"
                );
            }
        }
    }

    /// 「按钮不许有填充底」只有像素能判：几何断言看不出"整页唯一的药丸"这种不一致。
    /// 本机没有配对设备（`Scripts/ui-shot.py` 要真窗口 + 真通知），所以把通知页画进内存位图
    /// 导出 PPM 自己看，同时留一条取样判据——谁把 `fill_round` 画回按钮槽位，这里就红。
    #[test]
    fn notify_page_pixels() {
        use crate::state::NotificationItem;
        use std::ffi::c_void;
        use std::ptr::null_mut;
        use windows::Win32::Foundation::{COLORREF, RECT};
        use windows::Win32::Graphics::Gdi::{
            CreateCompatibleDC, CreateDIBSection, CreateSolidBrush, DeleteDC, DeleteObject,
            FillRect, GdiFlush, SelectObject, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS,
            HGDIOBJ,
        };

        const W: i32 = 900;
        const H: i32 = 606;

        fn row(pkg: &str, title: &str, text: &str, id: i32, can_reply: bool) -> NotificationItem {
            NotificationItem {
                package: pkg.to_string(),
                title: title.to_string(),
                text: text.to_string(),
                ts_ms: 1_759_377_000_000 + i64::from(id) * 60_000,
                key_hash: id as u32,
                notification_id: id,
                can_reply,
                reply_action_index: i32::from(can_reply),
                reply_result_key: if can_reply {
                    "reply_text".into()
                } else {
                    String::new()
                },
                ..Default::default()
            }
        }

        /// 0xRRGGBB → COLORREF(0x00BBGGRR)
        fn cref(rgb: u32) -> COLORREF {
            COLORREF(((rgb & 0xff) << 16) | (rgb & 0xff00) | (rgb >> 16))
        }

        fn shoot(dark: bool) -> (Vec<u8>, Env) {
            let e = crate::theme::test_env_pixels(W, H, 1.0, dark);
            let mut st = UiState::default();
            st.active_tab = TAB_NOTIFY;
            st.notifications = vec![
                row(
                    "com.android.messaging",
                    "阿里云",
                    "【阿里云】您的验证码是：388020，您正在尝试登录控制台，请勿泄露",
                    1,
                    true,
                ),
                row(
                    "com.tencent.mm",
                    "微信",
                    "今晚八点开会，记得带电脑",
                    2,
                    false,
                ),
                row(
                    "org.telegram.messenger",
                    "Telegram",
                    "Your login code: 471205, do not forward",
                    3,
                    true,
                ),
            ];
            // 第 3 行正在回复中：选中态要落在整行的底色上，不是给「回复」描一个框
            st.reply_target = Some(crate::state::ReplyTarget {
                package: "org.telegram.messenger".into(),
                tag: String::new(),
                notification_id: 3,
                action_index: 0,
                result_key: "reply_text".into(),
            });
            let bmi = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: W,
                    biHeight: -H,
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0,
                    ..Default::default()
                },
                ..Default::default()
            };
            unsafe {
                let hdc = CreateCompatibleDC(None);
                let mut bits: *mut c_void = null_mut();
                let hbmp =
                    CreateDIBSection(hdc, &bmi, DIB_RGB_COLORS, &mut bits, None, 0).expect("DIB");
                let old = SelectObject(hdc, HGDIOBJ(hbmp.0));
                let bg = CreateSolidBrush(cref(e.pal.body_bg));
                FillRect(
                    hdc,
                    &RECT {
                        left: 0,
                        top: 0,
                        right: W,
                        bottom: H,
                    },
                    bg,
                );
                let _ = DeleteObject(HGDIOBJ(bg.0));
                paint_notifications(hdc, &e, &st, e.px(W));
                let _ = GdiFlush();
                let px =
                    std::slice::from_raw_parts(bits as *const u8, (W * H * 4) as usize).to_vec();
                SelectObject(hdc, old);
                let _ = DeleteObject(HGDIOBJ(hbmp.0));
                let _ = DeleteDC(hdc);
                let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../../Temp/ui-review");
                std::fs::create_dir_all(dir).ok();
                let mut out: Vec<u8> = Vec::with_capacity(16 + (W * H * 3) as usize);
                out.extend_from_slice(format!("P6\n{W} {H}\n255\n").as_bytes());
                for chunk in px.chunks_exact(4) {
                    out.extend_from_slice(&[chunk[2], chunk[1], chunk[0]]);
                }
                std::fs::write(
                    format!("{dir}/notify-{}.ppm", if dark { "dark" } else { "light" }),
                    &out,
                )
                .ok();
                (px, e)
            }
        }

        let (px, e) = shoot(false);
        shoot(true);
        let at = |x: i32, y: i32| -> u32 {
            let o = ((y * W + x) * 4) as usize;
            ((px[o + 2] as u32) << 16) | ((px[o + 1] as u32) << 8) | px[o] as u32
        };
        let (code, reply, all) = notification_chips(&e, 0, true, true);
        for chip in [code.unwrap(), reply.unwrap(), all] {
            // 槽位最上一行横跨整个宽度：文字够不到这一行（垂直居中），药丸一定够得到
            let probe_y = chip.top + 1;
            for x in chip.left..chip.right {
                assert_eq!(
                    at(x, probe_y),
                    e.pal.body_bg,
                    "按钮槽位 ({x},{probe_y}) 上有填充底：通知页只许裸文字按钮"
                );
            }
        }
        let sel = notification_row_rect(&e, 2);
        assert_eq!(
            at(sel.left + 6, sel.top + (sel.bottom - sel.top) / 2),
            e.pal.sel_bg,
            "正在回复的那一行没有行底色，选中态没落到行上"
        );
    }

    /// 媒体页五个按钮整排居中，但窄窗口下必须退回内容左边界而不是伸出左侧负坐标。
    #[test]
    fn media_buttons_fit_the_min_width() {
        let e = crate::theme::test_env(LAYOUT_MIN_W, 606, 1.0);
        let first = media_btn_rect(&e, 0);
        let last = media_btn_rect(&e, MEDIA_BTN_COUNT - 1);
        assert!(
            first.left >= e.px(CONTENT_L),
            "媒体按钮排到了内容区左边之外"
        );
        assert!(
            last.right <= e.px(LAYOUT_MIN_W - CONTENT_R_PAD),
            "媒体按钮排超出最小客户区"
        );
    }

    /// 设置页开关区：行与行、行与下一节都不许叠字，右侧那一列也不许伸出内容区。
    /// 编译期断言管的是"最后一行 vs 下一节"，这里管的是"每一行 vs 它的下一行"——行高改小
    /// 时只有这条会红。
    #[test]
    fn settings_rows_never_share_a_line() {
        let e = crate::theme::test_env(LAYOUT_MIN_W, LAYOUT_MIN_H, 1.0);
        let content_right = e.px(CONTENT_L + MIN_ROW_W);
        for i in 0..SET_TOGGLES {
            let sw = set_switch_rect(&e, i);
            assert!(sw.right <= content_right, "第 {i} 行的开关伸出内容区");
            assert!(
                sw.bottom <= e.px(SET_MANUAL_HINT_Y),
                "第 {i} 行的开关压到设备管理标题"
            );
            let next = set_switch_rect(&e, i + 1);
            assert!(
                sw.bottom < next.top,
                "第 {i} 行与第 {} 行的控件叠在一起：{} vs {}",
                i + 1,
                sw.bottom,
                next.top
            );
        }
        let beh = set_behavior_rect(&e);
        assert!(
            beh.right <= content_right,
            "「关闭按钮行为」的按钮在最小窗口下伸出内容区：{}",
            beh.right
        );
        assert!(
            beh.bottom < e.px(SET_MANUAL_HINT_Y),
            "行为行压到设备管理标题"
        );
        // 「导出日志」与行末控件历来吃的是右侧那 32 的页边距（不是内容行宽），判据只能是客户区边界
        assert!(
            debug_export_rect(&e).right <= e.px(LAYOUT_MIN_W),
            "「导出日志」伸出客户区"
        );
    }

    /// 关闭询问弹窗：三颗按钮与勾选必须都在卡片里、互不压住，且在最小客户区也整个看得见
    #[test]
    fn close_prompt_fits_its_card_at_the_min_size() {
        for (w, h) in [(LAYOUT_MIN_W, LAYOUT_MIN_H), (1400, 900)] {
            let e = crate::theme::test_env(w, h, 1.0);
            let r = close_prompt_rects(&e);
            let named = [
                ("取消", &r.cancel),
                ("退出程序", &r.exit),
                ("最小化到托盘", &r.minimize),
                ("记住勾选", &r.remember),
            ];
            for (name, b) in named {
                assert!(
                    b.right > b.left && b.bottom > b.top,
                    "{name} 尺寸算成反向：{b:?}"
                );
                assert!(
                    b.left >= r.card.left && b.right <= r.card.right,
                    "{name} 出卡片左右边界：{b:?} vs {:?}",
                    r.card
                );
                assert!(
                    b.top >= r.body.bottom && b.bottom <= r.card.bottom,
                    "{name} 出卡片上下边界：{b:?} vs {:?}",
                    r.card
                );
                assert!(b.left >= e.px(0) && b.top >= e.px(0), "{name} 跑到客户区外");
            }
            assert!(
                r.cancel.right < r.exit.left && r.exit.right < r.minimize.left,
                "三颗按钮互相压住"
            );
            assert!(r.remember.top >= r.minimize.bottom, "勾选压在按钮上");
            // 卡片居中后仍要在最小客户区内看得见（负坐标=画到屏幕外）
            assert!(
                r.card.top >= 0 && r.card.bottom <= e.px(h),
                "卡片超出客户区"
            );
        }
    }
}
