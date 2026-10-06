//! Win32 主窗口：类注册 / WndProc / 消息泵 / DPI 感知 / 双缓冲

use std::os::raw::c_void;
use std::sync::OnceLock;
use std::time::Instant;

use windows::core::PCWSTR;
use windows::Win32::Foundation::{HINSTANCE, HMODULE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC, DeleteObject,
    EndPaint, InvalidateRect, SelectObject, HBITMAP, HDC, HGDIOBJ, PAINTSTRUCT, SRCCOPY,
};
use windows::Win32::System::Com::IDataObject;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Ole::{IDropSource, OleInitialize, DROPEFFECT_COPY, DROPEFFECT_NONE};
use windows::Win32::UI::HiDpi::{
    AdjustWindowRectExForDpi, GetDpiForSystem, GetDpiForWindow, SetProcessDpiAwarenessContext,
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetCapture, ReleaseCapture, SetCapture, TrackMouseEvent, TME_LEAVE, TRACKMOUSEEVENT, VK_BACK,
    VK_ESCAPE, VK_RETURN,
};
use windows::Win32::UI::Shell::{
    DragAcceptFiles, DragFinish, DragQueryFileW, ILCreateFromPathW, ILFree, SHCreateDataObject,
    SHDoDragDrop, HDROP,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, IsIconic, LoadCursorW, LoadIconW, PostMessageW,
    PostQuitMessage, RegisterClassW, SetCursor, SetForegroundWindow, ShowWindow, CS_HREDRAW,
    CS_VREDRAW, HICON, IDC_ARROW, IDC_HAND, IDI_APPLICATION, MINMAXINFO, SHOW_WINDOW_CMD, SW_HIDE,
    SW_RESTORE, SW_SHOW, WM_APP, WM_CAPTURECHANGED, WM_CHAR, WM_CLIPBOARDUPDATE, WM_CLOSE,
    WM_DESTROY, WM_DPICHANGED, WM_DROPFILES, WM_ERASEBKGND, WM_GETMINMAXINFO, WM_KEYDOWN,
    WM_LBUTTONDBLCLK, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_PAINT,
    WM_SETTINGCHANGE, WM_SIZE, WM_SYSCOLORCHANGE, WM_THEMECHANGED, WM_TIMER, WNDCLASSW,
    WS_OVERLAPPEDWINDOW,
};

use crate::autostart;
use crate::render::{self, HitTarget};
use crate::settings;
use crate::state::{
    decide_close, CloseAction, CloseChoice, ReplyRequest, ReplyTarget, SharedState, UiState,
    FOCUS_MANUAL_IP, FOCUS_NONE, FOCUS_REPLY, FOCUS_SEND_PATH, MAX_INPUT_CHARS,
};
use crate::theme;

/// `WM_MOUSELEAVE` 正确值 = **0x02A3**：写成 0x0216（`WM_ACTIVATEAPP` 附近）就收不到
/// 鼠标移出、hover 态无法复位。windows-rs 把它放在 `Win32::UI::Controls`，为单个常量单开
/// 特性不划算，故取字面量 —— 取字面量的常量必须由下方 `assert!` 与单测把值锁死
const WM_MOUSELEAVE: u32 = 0x02A3;

/// 托盘 v4 协议下"选中图标"的两个事件码（shellapi.h：`NIN_SELECT` = 0x000、
/// `NIN_KEYSELECT` = 0x001）。windows-rs 没把它们导出到可用的位置，取字面量 ⇒ 值由下方
/// 断言与单测锁死。升到 v4 之后鼠标消息**不再**发给本窗口，所以只按 `WM_*` 判的分支等于没修。
const NIN_SELECT: u32 = 0x0000;
const NIN_KEYSELECT: u32 = 0x0001;

/// 编译期锁定关键 Win32 常量值：取字面量的常量必须有值断言
const _: () = {
    assert!(WM_MOUSELEAVE == 0x02A3);
    assert!(WM_MOUSELEAVE != 0x0216, "0x0216 并非 WM_MOUSELEAVE");
    assert!(WM_APP == 0x8000);
    assert!(WM_CLIPBOARDUPDATE == 0x031D);
    assert!(WM_DPICHANGED == 0x02E0);
    assert!(WM_SETTINGCHANGE == 0x001A);
    assert!(WM_THEMECHANGED == 0x031A);
    assert!(WM_ERASEBKGND == 0x0014);
    assert!(WM_CHAR == 0x0102);
    assert!(WM_CHAR != WM_KEYDOWN, "WM_CHAR 与 WM_KEYDOWN 不得混用");
    assert!(WM_KEYDOWN == 0x0100);
    assert!(VK_BACK.0 == 0x08);
    assert!(VK_ESCAPE.0 == 0x1B, "Esc 的虚拟键码写错就关不掉询问弹窗");
    // 相册拖出：按下/抬起/移动靠这两个值判定，写错就是"点一下就拖走"或"拖不动也选不中"，且不会报错
    assert!(WM_LBUTTONUP == 0x0202);
    assert!(WM_LBUTTONDOWN == 0x0201);
    // 托盘回调按这些值分发；写错就是"图标亮着、点它没反应"，且不会有任何报错
    assert!(NIN_SELECT == 0x0000);
    assert!(NIN_KEYSELECT == 0x0001);
    assert!(WM_LBUTTONDBLCLK == 0x0203);
    assert!(WM_CAPTURECHANGED == 0x0215);
    assert!(WM_MOUSEMOVE == 0x0200);
    // WM_MOUSEMOVE 的 wparam 位标志（windows-rs 各版本把它放在不同模块，直接用协议值）
    assert!(MK_LBUTTON == 0x0001);
};

/// `WM_MOUSEMOVE`/按钮消息 wparam 的左键按下位标志（MSW MK_LBUTTON）。
const MK_LBUTTON: usize = 0x0001;

/// worker 线程通过它通知 UI 重绘（与托盘回调 WM_APP+1 错开）
pub(crate) const WM_APP_STATE_CHANGED: u32 = WM_APP + 2;
/// 「别再问了，直接退出」：关闭询问弹窗里选了退出、或功能改动要重启时走这条。
/// 它绕开 `WM_CLOSE` 的行为分派，但仍然交给 `DefWindowProc` 收尾 —— 直接 `process::exit`
/// 会在托盘上留一个死图标
pub(crate) const WM_APP_EXIT: u32 = WM_APP + 3;
/// 「把窗口摆回前台」：第二实例（用户又双击了一次图标）与外部把文件转交进来时用
pub(crate) const WM_APP_SHOW: u32 = WM_APP + 4;

/// exe 内嵌应用图标的资源 id（由 `build.rs` 用 windres 编入 `linkx.rc` 的 id 1）
const IDI_APP_ICON: u32 = 1;

/// 定时器 id（动画 / 事件轮询）
pub(crate) const TIMER_ID: usize = 1;
/// 刷新周期：33ms ≈ 30fps（仅在确有动画/变化时才真正重绘，静止时空转成本极低）
pub(crate) const TIMER_MS: u32 = 33;

/// 载入 exe 内嵌应用图标；缺失时回退系统默认图标（保证窗口仍能创建）
pub(crate) fn load_app_icon() -> HICON {
    unsafe {
        let hinst = get_instance();
        LoadIconW(hinst, PCWSTR(IDI_APP_ICON as usize as *const u16))
            .or_else(|_| LoadIconW(HINSTANCE(std::ptr::null_mut()), IDI_APPLICATION))
            .unwrap_or(HICON(std::ptr::null_mut()))
    }
}

/// 开启 Per-Monitor V2 DPI 感知（**必须在创建任何窗口前调用**）：不声明感知时 Windows
/// 会把整窗位图拉伸，高分屏上字会糊；V2 下按物理像素渲染，跨屏 DPI 变化时收到
/// `WM_DPICHANGED` 再按新 DPI 重排（见 [`Env::px`]）
pub(crate) fn enable_dpi_awareness() {
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
}

static UI_STATE: OnceLock<SharedState> = OnceLock::new();

pub(crate) fn get_instance() -> HINSTANCE {
    unsafe {
        let h = GetModuleHandleW(PCWSTR::null()).unwrap_or(HMODULE(std::ptr::null_mut()));
        HINSTANCE(h.0)
    }
}

pub(crate) fn install_state(state: SharedState) {
    let _ = UI_STATE.set(state);
}

pub(crate) fn shared_state() -> Option<&'static SharedState> {
    UI_STATE.get()
}

/// 供 worker（非 UI 线程）请求重绘：重建 HWND 并 PostMessage
pub(crate) fn post_state_changed(hwnd_raw: isize) {
    let hwnd = HWND(hwnd_raw as *mut c_void);
    if hwnd.0.is_null() {
        return;
    }
    unsafe {
        let _ = PostMessageW(hwnd, WM_APP_STATE_CHANGED, WPARAM(0), LPARAM(0));
    }
}

/// 退出：`WM_APP_EXIT` → `DefWindowProc(WM_CLOSE)` → `WM_DESTROY` → `PostQuitMessage`，
/// 与点标题栏 ✕ 后答"退出程序"走到的是同一个终点，`main` 的收尾（摘托盘、释放双缓冲、
/// 销毁窗口）一样都不会少 —— 直接 `process::exit` 会在托盘上留一个死图标。
/// 走私有消息而不是 `WM_CLOSE`：`WM_CLOSE` 现在归"关闭按钮行为"分派，重启这种"已经答过了"
/// 的关窗如果再被拦一次，用户按了「重新启动」却什么也不会发生。
pub(crate) fn request_exit(hwnd: HWND) {
    if hwnd.0.is_null() {
        return;
    }
    unsafe {
        let _ = PostMessageW(hwnd, WM_APP_EXIT, WPARAM(0), LPARAM(0));
    }
}

/// 供非 UI 线程（调试控制面）请求退出：跨线程只传 `isize`，不在别的线程上持有 `HWND`
#[allow(dead_code)] // 唯一调用方在 agent-debug 控制面，交付构建不带该 feature
pub(crate) fn request_exit_from_raw(hwnd_raw: isize) {
    request_exit(HWND(hwnd_raw as *mut c_void));
}

/// 请求把窗口摆回前台。管道线程（第二实例双击图标、或「发送到 LinkX」）只传 `isize`，
/// 不在别的线程上持有 `HWND`
pub(crate) fn request_show_from_raw(hwnd_raw: isize) {
    let hwnd = HWND(hwnd_raw as *mut c_void);
    if hwnd.0.is_null() {
        return;
    }
    unsafe {
        let _ = PostMessageW(hwnd, WM_APP_SHOW, WPARAM(0), LPARAM(0));
    }
}

/// 收进托盘：只藏窗口。进程、托盘图标、定时器与 worker 线程都原地不动，之后由托盘图标
/// 那一下、或再双击一次程序图标（走 `ipc` 的唤醒请求）把窗口摆回来。
/// 顺手清显示资源：托盘才是这个程序大部分时间待着的地方，而 `SW_HIDE` 不发 `WM_SIZE`，
/// 只挂在最小化那条路上等于没做。
pub(crate) fn hide_to_tray(hwnd: HWND) {
    if hwnd.0.is_null() {
        return;
    }
    unsafe {
        let _ = ShowWindow(hwnd, SW_HIDE);
    }
    shed_display_memory();
}

/// 窗口不再显示时释放"只为显示而存在"的资源并把内存优先级降到 LOW；恢复可见时再要回来。
/// 双缓冲走 `CreateCompatibleBitmap`，位图记在会话的 GDI 堆上，所以本进程常驻不会因此变小 ——
/// 这条买的是"不占别人的额度" + 相册那份记在自己头上的解码缓存。位图由下一次 `WM_PAINT` 重建。
fn shed_display_memory() {
    unsafe { release_back_buffer() };
    crate::render::purge_thumb_dibs();
    set_memory_priority(true);
}

/// 创建主窗口。入参是**逻辑尺寸**（96dpi 基准），按系统 DPI 换算成物理像素。
/// 标题栏必须在 `ShowWindow` **之前**染好色，否则会闪一帧系统默认的白条。
/// `show = false` 用于 `--minimized` 拉起的那一次：窗口不显示，托盘图标与消息泵照旧
pub(crate) fn create_main_window(
    hinst: HINSTANCE,
    title: &str,
    w: i32,
    h: i32,
    pref: settings::Theme,
    show: bool,
) -> Option<HWND> {
    let class = windows::core::w!("LinkXShellWnd");
    let wc = WNDCLASSW {
        style: CS_HREDRAW | CS_VREDRAW,
        lpfnWndProc: Some(wnd_proc),
        hInstance: hinst,
        hIcon: load_app_icon(), // 任务栏 / Alt+Tab / 标题栏
        hCursor: unsafe { LoadCursorW(None, IDC_ARROW).unwrap_or_default() },
        lpszClassName: class,
        ..Default::default()
    };
    unsafe {
        if RegisterClassW(&wc) == 0 {
            return None;
        }
        let dpi = {
            let d = GetDpiForSystem();
            if d == 0 {
                96
            } else {
                d
            }
        };
        let scale = dpi as f32 / 96.0;
        let cx = (w as f32 * scale).round() as i32;
        let cy = (h as f32 * scale).round() as i32;
        // 按 DPI 修正外框：客户区正好是请求的逻辑尺寸
        let mut rc = RECT {
            left: 0,
            top: 0,
            right: cx,
            bottom: cy,
        };
        let _ =
            AdjustWindowRectExForDpi(&mut rc, WS_OVERLAPPEDWINDOW, false, Default::default(), dpi);

        let title_wide: Vec<u16> = title.encode_utf16().chain(std::iter::once(0)).collect();
        let hwnd = CreateWindowExW(
            Default::default(),
            class,
            PCWSTR(title_wide.as_ptr()),
            WS_OVERLAPPEDWINDOW,
            Default::default(),
            Default::default(),
            rc.right - rc.left,
            rc.bottom - rc.top,
            None,
            None,
            hinst,
            None,
        )
        .ok()?;
        theme::apply_caption(hwnd, pref);
        // 拖进窗口的文件 = 待发送路径（与「选择…」同一出口）。这个绑定没有返回值，注册失败无从得知
        // —— 拖放这条入口验收时要真拖一次，不能只看编译过
        DragAcceptFiles(hwnd, true);
        if show {
            let _ = ShowWindow(hwnd, SHOW_WINDOW_CMD(5)); // SW_SHOW
        } else {
            // 这条路径永远等不到 `WM_SIZE`，优先级只能在这里降
            set_memory_priority(true);
        }
        Some(hwnd)
    }
}

pub(crate) unsafe fn destroy_main_window(hwnd: HWND) {
    if !hwnd.0.is_null() {
        let _ = DestroyWindow(hwnd);
    }
}

// ---------- 双缓冲（消除动效期间的闪烁）----------

struct BackBuffer {
    hdc: HDC,
    bmp: HBITMAP,
    /// 创建时被选出的默认 1×1 位图：删除 DC 前必须选回，否则位图泄漏
    old_obj: HGDIOBJ,
    w: i32,
    h: i32,
}

static mut BACK: Option<BackBuffer> = None;

/// 释放一个双缓冲：先选回原对象，再删位图与 DC
#[allow(static_mut_refs)]
unsafe fn destroy_back_buffer(b: BackBuffer) {
    unsafe {
        // 位图仍在 DC 中时 DeleteObject 会失败 → 必须先还原旧对象
        if !b.old_obj.is_invalid() {
            let _ = SelectObject(b.hdc, b.old_obj);
        }
        let _ = DeleteObject(HGDIOBJ(b.bmp.0));
        let _ = DeleteDC(b.hdc);
    }
}

/// 取得与窗口同尺寸的内存 DC（尺寸变化时重建）。返回 `None` 表示创建失败（退化为直接绘制）。
#[allow(static_mut_refs)]
unsafe fn back_buffer(window_dc: HDC, w: i32, h: i32) -> Option<HDC> {
    if w <= 0 || h <= 0 {
        return None;
    }
    if let Some(b) = BACK.as_ref() {
        if b.w == w && b.h == h {
            return Some(b.hdc);
        }
    }
    if let Some(old) = BACK.take() {
        unsafe { destroy_back_buffer(old) };
    }
    let hdc = unsafe { CreateCompatibleDC(window_dc) };
    if hdc.is_invalid() {
        return None;
    }
    let bmp = unsafe { CreateCompatibleBitmap(window_dc, w, h) };
    if bmp.is_invalid() {
        let _ = unsafe { DeleteDC(hdc) };
        return None;
    }
    // 保存 SelectObject 返回的旧对象，释放时还原，避免 GDI 句柄泄漏
    let old_obj = unsafe { SelectObject(hdc, HGDIOBJ(bmp.0)) };
    BACK = Some(BackBuffer {
        hdc,
        bmp,
        old_obj,
        w,
        h,
    });
    Some(hdc)
}

// 相册格子按下点 `(逻辑坐标, 格子下标)`："选中"必须推迟到 `WM_LBUTTONUP` —— 同一句按住既是
// "点一下选中"也是"拖出去"，按下就改选中态的话，拖完会多出一格谁也没点的选中
//
// 正在拖滚动条：轨道很窄，按下之后必须抓住鼠标，否则拖出轨道就断。相册与通知两条轨道共用这套
// 拖拽逻辑，判据只有"拖没拖"（拖的是哪一页由各页自己的滚动字段决定），所以这里是个布尔而不是
// 页号 —— 用 0 当"没拖"的哨兵时，0 号页（连接页）一旦加滚动条就会拖不动且鼠标释放不掉。
thread_local! {
    static BAR_DRAGGING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}
thread_local! {
    static ALBUM_PRESS: std::cell::Cell<Option<(i32, i32, usize)>> = const {
        std::cell::Cell::new(None)
    };
}
thread_local! {
    /// 本线程是否已初始化过 OLE（见 `album_drag_out`：只加一次、不卸）
    static OLE_OWNED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// 拖出启动阈值（逻辑像素）：用户"点歪几个像素"不该被当成拖拽
const ALBUM_DRAG_PX: i32 = 6;

/// 把一段文本写进本机剪贴板，并登记为「本端已应用内容」。
/// 防回声不能省：不登记的话 `WM_CLIPBOARDUPDATE` 会把它当成本机新复制的内容再推回对端
/// （手机弹一条自己刚发来的通知）。整行复制与"只复制验证码"共用这一条出口
fn copy_locally(st: &mut UiState, payload: Option<String>) -> Option<String> {
    let payload = payload.filter(|p| !p.is_empty())?;
    st.last_applied_clip = payload.clone();
    st.copied_at = Some(Instant::now());
    Some(payload)
}

/// 把回复框里的文字发出去：分配序号、登记待回执、清空输入。
/// 空文本不发 —— 发一条空回复只会让手机回一句"正文为空"，白跑一趟。
/// 与「发送」按钮、回车和调试面共用这一份，否则三条入口的判空与登记口径迟早会漂。
pub(crate) fn submit_reply(st: &mut UiState) {
    let Some(target) = st.reply_target.clone() else {
        return;
    };
    let text = st.reply_input.trim().to_string();
    if text.is_empty() {
        return;
    }
    // 单槽命令字段：worker 每 200 ms 才取走一次，这期间连点两次"发送"会把第一条整包顶掉，
    // 而 `reply_pending` 已经替第一条登记了回执号 —— 十秒后冒出来的"手机没有回应"是替一条
    // 从未发出的请求说的话。宁可让用户等一下再发，也不许静默丢。
    // 这一支**不占号**：序号只在真的排进 `reply_req` 时才前进，不然白烧一个号。
    if st.reply_req.is_some() {
        st.push_error("上一条回复还没发出去，稍等一下再发".to_string());
        return;
    }
    let reply_id = st.reply_next_id;
    st.reply_next_id = st.reply_next_id.wrapping_add(1);
    st.reply_pending.push((reply_id, Instant::now()));
    st.reply_req = Some(ReplyRequest {
        reply_id,
        target,
        text,
    });
    st.reply_input.clear();
    st.ui_rev += 1;
}

fn on_left_click(x: i32, y: i32) {
    let Some(arc) = shared_state() else {
        return;
    };
    let mut st = arc.lock().unwrap();
    let hwnd = hwnd_of(&st);
    let env = theme::detect(hwnd, st.theme);
    let Some(hit) = render::hit_test(&st, &env, x, y) else {
        // 点在空白处：取消文本焦点（否则键盘输入会"悄悄"写进看不见的输入框）
        st.input_focus = FOCUS_NONE;
        return;
    };
    let mut persist = false;
    // 需要「放锁后」执行的副作用（模态框、注册表、跨进程剪贴板）：持锁时 worker 每 200ms 的锁请求全卡住
    let mut sync_debug: Option<bool> = None;
    let mut export_now = false;
    let mut browse_now = false;
    let mut inbox_now = false;
    let mut album_export_now = false;
    let mut restart_now: Option<HWND> = None;
    // 注册表读写同样是出锁才做的：慢在注册表上一卡，卡的整壳都跟着停
    let mut autostart_want: Option<bool> = None;
    let mut autostart_resync = false;
    let mut close_answer: Option<CloseChoice> = None;
    match hit {
        HitTarget::Nav(i) => {
            // 进设置页就回读一次注册表：开关显示的是真值，不是 ini 里那份记忆。
            // 已经在这页时再点一次导航 = 用户要"再看一眼真值"，同样回读
            if i == render::TAB_SETTINGS {
                autostart_resync = true;
            }
            if st.active_tab != i {
                // 离开相册页：代际 +1，在途缩略图应答随即作废（否则"上一页的图闪进这一页"）
                if st.active_tab == render::TAB_ALBUM {
                    st.album.invalidate();
                }
                st.active_tab = i;
                st.anim_start = Some(Instant::now());
                st.list_hover = None;
                st.input_focus = FOCUS_NONE;
                // 进相册页而清单为空：自动要第一页，每页张数按视口算 —— 画不出来的就不该下载
                if i == render::TAB_ALBUM
                    && crate::features::enabled(crate::features::Module::Album)
                    && st.album.items.is_empty()
                    && !st.album.loading
                {
                    let per = render::album_viewport_cells(&env).max(1) as u32;
                    st.album.req_list = Some((0, per));
                }
            }
        }
        HitTarget::ConnectDevice(addr) => {
            st.selected = Some(addr);
            st.connect_req = Some(addr);
        }
        HitTarget::SasConfirm => st.confirm_sas_req = true,
        HitTarget::SasReject => st.reject_sas_req = true,
        HitTarget::ToggleClipSync => {
            st.clip_sync = !st.clip_sync;
            persist = true;
        }
        HitTarget::ToggleToast => {
            st.toast_enabled = !st.toast_enabled;
            persist = true;
        }
        HitTarget::ToggleToastContent => {
            st.toast_show_content = !st.toast_show_content;
            persist = true;
        }
        // 自动连接已绑定设备：只改"下次启动怎么办"，本轮已建立的链路不动
        HitTarget::ToggleAutoConnect => {
            st.auto_connect = !st.auto_connect;
            persist = true;
        }
        // 开机自启动：这里只记下"想要哪个状态"，注册表在出锁后写，写完再按真值回读
        HitTarget::ToggleAutostart => {
            autostart_want = Some(!st.autostart);
        }
        // 关闭按钮行为：点一下换下一个取值并立刻落盘
        HitTarget::CycleCloseBehavior => {
            st.close_behavior = st.close_behavior.next();
            st.ui_rev += 1;
            persist = true;
        }
        HitTarget::ThemeSet(t) => {
            st.theme = t;
            st.anim_start = Some(Instant::now());
            persist = true;
            // 标题栏不归客户区重绘管：切了主题要当场把 DWM 的染色属性重设一遍
            theme::apply_caption(hwnd, t);
        }
        // 媒体控制按钮 → 播放指令。动作码取 proto 生成的枚举，不手抄数字：两端各自硬编码时
        // 改枚举顺序不会有任何地方报错
        HitTarget::MediaControl(i) => {
            use linkx_protocol::pb::media_command::Action;
            let cur = st.media.as_ref().map(|m| m.volume).unwrap_or(0);
            // 逐下标显式映射、越界不动作：写成 `_ => 音量+` 时"按钮个数变了"就成了意外的加音量
            let cmd = match i {
                0 => Some((Action::Prev as i32, 0, 0)),
                1 => Some((Action::PlayPause as i32, 0, 0)),
                2 => Some((Action::Next as i32, 0, 0)),
                // 手机说"音量读不到"（-1）时不能拿它当基准 ±5：`-1 - 5` 夹成 0 = 一下把手机静音
                3 | 4 if cur < 0 => None,
                3 => Some((Action::SetVolume as i32, (cur - 5).clamp(0, 100), 0)),
                4 => Some((Action::SetVolume as i32, (cur + 5).clamp(0, 100), 0)),
                _ => None,
            };
            if let Some((action, volume, delta)) = cmd {
                st.media_cmd_req = Some((action, volume, delta));
            }
        }
        HitTarget::SendLocalClip => {
            drop(st);
            let text = crate::clipboard::read_text().filter(|t| !t.is_empty());
            let Some(arc) = shared_state() else { return };
            let mut st = arc.lock().unwrap();
            if let Some(text) = text {
                st.clip_out = text.clone();
                st.send_clip_req = Some(text);
            }
            return;
        }
        HitTarget::CopyNotification(idx) => {
            // 通知仅作查看不够用（验证码/长文需要粘贴）→ 点条目即复制正文到本机剪贴板
            let payload = st.notifications.get(idx).map(|item| {
                if item.text.is_empty() {
                    item.title.clone()
                } else {
                    item.text.clone()
                }
            });
            if let Some(p) = copy_locally(&mut st, payload) {
                drop(st); // 锁外做跨进程剪贴板写：占着锁写系统服务会拖住整条 UI 线程
                crate::clipboard::set_text(&p);
                return;
            }
        }
        HitTarget::CopyNotificationCode(idx) => {
            // 只复制抽出来的数字码：短信通知整段粘过去往往还得手动删掉"请勿泄露"
            let payload = st
                .notifications
                .get(idx)
                .and_then(|item| linkx_session::code_extract::extract_code(&item.title, &item.text))
                .map(|c| c.digits);
            if let Some(p) = copy_locally(&mut st, payload) {
                drop(st); // 锁外做跨进程剪贴板写：占着锁写系统服务会拖住整条 UI 线程
                crate::clipboard::set_text(&p);
                return;
            }
        }
        // ---- 通知回复 ----
        HitTarget::ReplyNotification(idx) => {
            let Some(item) = st.notifications.get(idx) else {
                return;
            };
            if !item.can_reply {
                // 没有入口就不该被命中；真走到这里说明绘制与命中的几何漂了，宁可什么都不做
                return;
            }
            let next = ReplyTarget {
                package: item.package.clone(),
                tag: item.tag.clone(),
                notification_id: item.notification_id,
                action_index: item.reply_action_index,
                result_key: item.reply_result_key.clone(),
            };
            // 再点一次同一条 = 收起回复条（用户改主意了，不该留一个空框在界面上）
            if st.reply_target.as_ref() == Some(&next) {
                st.close_reply_bar();
            } else {
                st.reply_target = Some(next);
                st.reply_input.clear();
                st.input_focus = FOCUS_REPLY;
            }
        }
        HitTarget::FocusReplyInput => st.input_focus = FOCUS_REPLY,
        HitTarget::SendReply => submit_reply(&mut st),
        // ---- 文件页 / 设置页 ----
        HitTarget::FocusSendPath => st.input_focus = FOCUS_SEND_PATH,
        HitTarget::FocusManualIp => st.input_focus = FOCUS_MANUAL_IP,
        HitTarget::SendFile => {
            st.send_file_req = true;
            // 空路径不在此处报错：worker 会给出可读错误（避免 UI 直接判空造成重复逻辑）
        }
        HitTarget::BrowseFile => browse_now = true,
        HitTarget::ChooseInbox => inbox_now = true,
        HitTarget::CancelFile(i) => {
            // 行下标 → 行键 `(方向, 文件名)`：绘制与命中同用 `is_cancellable`，掐断由 worker 做
            if let Some(t) = st.file_tasks.get(i) {
                st.cancel_file_req = Some((t.direction, t.name.clone()));
            }
        }
        HitTarget::ManualIpConnect => st.manual_ip_req = true,
        HitTarget::UnbindDevice(_) => {
            // 引擎的信任库是「单对端」语义：解绑即清空该设备的 TOFU 信任
            st.unbind_requested = true;
        }
        // ---- 对端新身份裁决（检测→派发→用户决策→引擎推进，闭环）----
        HitTarget::AcceptNewIdentity => st.accept_identity_req = true,
        HitTarget::RejectNewIdentity => st.reject_identity_req = true,
        // ---- Debug 模式开关 + 日志导出 ----
        HitTarget::ToggleDebug => {
            st.debug_enabled = !st.debug_enabled;
            sync_debug = Some(st.debug_enabled);
            persist = true;
        }
        HitTarget::ExportDebug => export_now = true,
        // ---- 运行期功能开关 + 重启确认弹窗 ----
        HitTarget::ToggleFeature(m) => {
            let on = !m.wanted(&st);
            m.set_wanted(&mut st, on);
            // 走既有的保存出口：改动立刻落盘，重启后由 `features::init` 固化
            persist = true;
            // 弹窗是模态的：先把输入框焦点摘掉。否则"先点了手动 IP 输入框、再拨开关"
            // 时焦点还在，弹窗期间敲键盘会写进一个已经看不见的输入框。
            st.input_focus = FOCUS_NONE;
            // 只有"已加载"与"想要"真的不一致时才弹窗：来回拨回原值不该被追问
            if crate::features::restart_pending(&st) {
                st.restart_prompt = true;
            }
        }
        HitTarget::RestartApp | HitTarget::ModalRestartNow => {
            st.restart_prompt = false;
            st.restart_req = true;
            restart_now = Some(hwnd);
        }
        // 「稍后启动」不丢改动：状态已落盘，功能页会继续显示"待重启生效"
        HitTarget::ModalRestartLater => st.restart_prompt = false,
        // ---- 关闭询问弹窗：答案只登记，动作（藏窗口 / 退出）在出锁后做 ----
        h @ (HitTarget::ModalCloseMinimize
        | HitTarget::ModalCloseExit
        | HitTarget::ModalCloseCancel) => {
            close_answer = Some(match h {
                HitTarget::ModalCloseMinimize => CloseChoice::Minimize,
                HitTarget::ModalCloseExit => CloseChoice::Exit,
                _ => CloseChoice::Cancel,
            });
        }
        HitTarget::ModalCloseRemember => {
            st.close_remember = !st.close_remember;
            st.ui_rev += 1;
        }
        // ---- 相册 ----
        HitTarget::AlbumTool(slot) => {
            let per = st.album.per_page.max(1);
            match slot {
                // 刷新 = 按当前视口重要第一页（视口变了每页张数也跟着变）
                0 => {
                    let cells = render::album_viewport_cells(&env).max(1) as u32;
                    st.album.req_list = Some((0, cells));
                }
                1 => {
                    if st.album.page == 0 {
                        st.push_error("已经是第一页了".to_string());
                    } else {
                        st.album.req_list = Some((st.album.page - 1, per));
                    }
                }
                2 => {
                    let next = st.album.page + 1;
                    let covered = next as usize * per as usize;
                    if st.album.total > 0 && covered >= st.album.total as usize {
                        let why = format!(
                            "下一页没有内容：相册共 {} 张，第 {} 页已经到最后",
                            st.album.total,
                            st.album.page + 1
                        );
                        st.push_error(why);
                    } else {
                        st.album.req_list = Some((next, per));
                    }
                }
                3 => {
                    let all = !st.album.items.is_empty()
                        && st.album.selected.len() >= st.album.items.len();
                    let ids: Vec<u64> = st.album.items.iter().map(|i| i.id).collect();
                    st.album.selected.clear();
                    if !all {
                        st.album.selected.extend(ids);
                    }
                    st.ui_rev += 1;
                }
                _ => {
                    if st.album.selected.is_empty() {
                        st.push_error(
                            "还没有选中照片：先在格子上点一下（或按「全选」）再导出".to_string(),
                        );
                    } else {
                        album_export_now = true;
                    }
                }
            }
        }
        // 格子：按下只记账，选中态在 WM_LBUTTONUP 里改（同一次按住还可能是拖出）
        HitTarget::AlbumCell(i) => {
            ALBUM_PRESS.set(Some((x, y, i)));
            // 顺手取回这张的原图：拖出必须"按住就走"就有载荷，等挪出阈值再取用户早就松手了
            // 已经在途就不重复发：两次 ALBUM_FULL_REQ 会回两份原图，第二份到时无路由只能报"已拒绝"
            if let Some(id) = st.album.items.get(i).map(|it| it.id) {
                let in_flight = st.album.routes.contains_key(&id);
                let landed = matches!(&st.album.drag_result, Some((gid, Ok(_))) if *gid == id)
                    // 上次拖没等到、已取回留在本地的那一份也算"到手"，再发请求就是白烧带宽
                    || st.album.drag_ready_path(id).is_some();
                if !in_flight && !landed {
                    st.album.plan_fetch(
                        core::slice::from_ref(&id),
                        crate::transfer::album_drag_dir().display().to_string(),
                        crate::state::AlbumPurpose::Drag,
                    );
                    st.ui_rev += 1;
                }
            }
        }
        // 滚动条：按下即定位，并抓住鼠标继续拖（轨道很窄，一次点击很难精确落点）
        h @ (HitTarget::AlbumScroll(_) | HitTarget::NotifyScroll(_)) => {
            match h {
                HitTarget::NotifyScroll(row) => st.notify_scroll = row,
                HitTarget::AlbumScroll(row) => st.album.scroll_row = row,
                _ => unreachable!(),
            }
            st.ui_rev += 1;
            BAR_DRAGGING.set(true);
            let _ = unsafe { SetCapture(hwnd) };
        }
    }
    if persist {
        settings::Settings::from_state(&st).save();
    }
    drop(st);
    if let Some(want) = autostart_want {
        // 写/删注册表都在锁外；失败就出声，绝不能把"没写成"显示成"已开启"
        if let Err(why) = autostart::apply(want) {
            push_error(format!(
                "开机自启动没能{}：{why}",
                if want { "开启" } else { "关闭" }
            ));
        }
        autostart_resync = true;
    }
    if autostart_resync {
        sync_autostart_truth();
    }
    if let Some(choice) = close_answer {
        answer_close_prompt(hwnd, choice);
    }
    if let Some(h) = restart_now {
        request_exit(h);
    }
    if let Some(on) = sync_debug {
        crate::debug::apply_debug_toggle(on);
    }
    if export_now {
        crate::debug::run_debug_export(hwnd);
    }
    if browse_now {
        // 系统对话框必须放锁后弹：一路模态到用户点完，握着锁会让 worker 的锁请求全部卡死
        use crate::dialog::Pick;
        match crate::dialog::pick_file(hwnd, "选择要发送到手机的文件") {
            // 只填路径、不代发：发送仍由用户点「发送到手机」确认，
            // 免得手滑选错文件就直接把大文件推出去。
            Pick::Picked(p) => set_send_path(p.to_string_lossy().into_owned()),
            Pick::Cancelled => {}
            Pick::Failed(why) => push_error(format!("系统文件对话框不可用：{why}")),
        }
    }
    if inbox_now {
        // 换收件目录：同样放锁后弹；建不出来就不改 —— 留个用不了的目录比不改更糟
        use crate::dialog::Pick;
        match crate::dialog::pick_folder(hwnd, "选择收件目录（手机发来的文件落这里）")
        {
            Pick::Picked(dir) => match std::fs::create_dir_all(&dir) {
                Ok(()) => {
                    let Some(arc) = shared_state() else { return };
                    let mut st = arc.lock().unwrap();
                    st.inbox_dir = dir.display().to_string();
                    let saved = settings::Settings::from_state(&st);
                    drop(st);
                    saved.save();
                    let mut st = arc.lock().unwrap();
                    st.push_toast(
                        "收件目录已更改".to_string(),
                        format!("手机发来的文件将落到：{}", dir.display()),
                    );
                }
                Err(e) => push_error(format!("收件目录用不了（{}）: {e}", dir.display())),
            },
            Pick::Cancelled => {}
            Pick::Failed(why) => push_error(format!("系统文件夹对话框不可用：{why}")),
        }
    }
    if album_export_now {
        // 相册导出：「选择文件夹」也须**放锁后**弹，选完才把目标目录 + 待收 id 一起登记并发请求
        use crate::dialog::Pick;
        match crate::dialog::pick_folder(hwnd, "选择导出目录（手机原图会落到这里）")
        {
            Pick::Picked(dir) => match std::fs::create_dir_all(&dir) {
                Ok(()) => {
                    let Some(arc) = shared_state() else { return };
                    let mut st = arc.lock().unwrap();
                    let ids: Vec<u64> = st.album.selected.clone();
                    if ids.is_empty() {
                        st.push_error("选中状态在这一步被清掉了，没有请求任何原图".to_string());
                    } else {
                        st.album.plan_fetch(
                            &ids,
                            dir.display().to_string(),
                            crate::state::AlbumPurpose::Export,
                        );
                        st.ui_rev += 1;
                    }
                }
                Err(e) => push_error(format!("导出目录用不了（{}）: {e}", dir.display())),
            },
            // 取消不是失败，但也得留一句：否则用户以为"点了没反应"
            Pick::Cancelled => {
                push_error("已取消相册导出（没有选择目录，原图请求没有发出）".to_string())
            }
            Pick::Failed(why) => push_error(format!("系统文件夹对话框不可用：{why}")),
        }
    }
}

/// 相册：格子抬起 = 切换选中（按下时只记了坐标，见 `ALBUM_PRESS`）。
/// **抬起不撤单**：换成"进度环"之后点这一下就是要它，撤单会把正在取的原图掐掉，于是圆环永远跑不
/// 满、手机侧报"拖出已取消"，用户只能反复按；**取消选中才撤单**（见下面的分支）。
/// 垃圾仍然有界：`ALBUM_DRAG_KEEP` 只留最近两张（超出删最旧），启动时 `%TEMP%\LinkX` 还会整体清扫。
fn on_album_cell_up(cell: usize) {
    let Some(arc) = shared_state() else {
        return;
    };
    let mut st = arc.lock().unwrap();
    match st.album.items.get(cell) {
        Some(item) => {
            let id = item.id;
            st.album.toggle_selected(id);
            // 取消选中 = 这格我不打算拖了：这时才撤单，把在途的取图停掉
            if !st.album.selected.contains(&id) {
                st.album.cancel_drag(id);
            }
            st.ui_rev += 1;
        }
        // 抬起前翻页/刷新把那一格换掉了：不静默，说清为什么这一下没选上
        None => st.push_error(format!(
            "第 {} 格已经没有照片了（清单刚变过），这次点击没有选中任何东西",
            cell + 1
        )),
    }
}

/// 相册：把一张原图拖出到资源管理器/微信。
/// **原图在按下格子那一刻就开始取了**，照片到这里通常已经在本地。这一步只等很短一下
/// （`ALBUM_DRAG_WAIT`）：`SHDoDragDrop` 必须在鼠标还按着的时候发起，而它一进门就是自己的模态
/// 循环 —— 等得越久用户越可能已经松手，松手之后再起来的拖放只会得到一个灰色禁止圈。
/// 等不到时**不撤单**：取回继续跑完并留在本地，下一次拖直接命中缓存 —— 视频靠的就是这一条。
/// 载荷写进 `%TEMP%\LinkX\`（拖拽数据，不是缩略图缓存），拖完即删、最多留两张。
fn album_drag_out(cell: usize) {
    let Some(arc) = shared_state() else {
        return;
    };
    // 取名句要在锁外报错：`match arc.lock().unwrap()...` 的临时守卫活到整个 match 结束，
    // 而在 match 里调 push_error 就是再去拿同一把不可重入的锁 —— UI 线程当场永久卡死。
    let id = {
        let st = arc.lock().unwrap();
        st.album.items.get(cell).map(|item| item.id)
    };
    let Some(id) = id else {
        push_error(format!("第 {} 格已经没有照片了，这次拖拽取消", cell + 1));
        return;
    };
    let dir = crate::transfer::album_drag_dir();
    if let Err(e) = std::fs::create_dir_all(&dir) {
        push_error(format!("拖拽临时目录建不出来（{}）: {e}", dir.display()));
        return;
    }
    // **拖哪几张**：按下这格若在多选集合里就把选中的全带上（清单顺序 = 落点里文件的先后）
    let ids: Vec<u64> = {
        let st = arc.lock().unwrap();
        if st.album.selected.len() > 1 && st.album.selected.contains(&id) {
            st.album
                .items
                .iter()
                .map(|it| it.id)
                .filter(|i| st.album.selected.contains(i))
                .collect()
        } else {
            vec![id]
        }
    };
    // 载荷必须**已经在本地**（`CF_HDROP` 只肯复制磁盘上存在的文件）：缺的当场补发请求，但绝不在这里干等
    let (mut paths, mut missing) = (Vec::new(), Vec::new());
    {
        let st = arc.lock().unwrap();
        for i in &ids {
            match st.album.drag_ready_path(*i) {
                Some(p) => paths.push(p),
                None => missing.push(*i),
            }
        }
    }
    if !missing.is_empty() {
        let n = missing.len();
        let mut st = arc.lock().unwrap();
        st.album.plan_fetch(
            &missing,
            dir.display().to_string(),
            crate::state::AlbumPurpose::Drag,
        );
        st.ui_rev += 1;
        drop(st);
        push_error(format!(
            "这次没有拖出去：{n} 张原图还在手机上没取回来（已发起补齐）。\
             等这几格的圆环跑完再拖，就是一次全部出去"
        ));
        return;
    }
    let hwnd = HWND({
        let st = arc.lock().unwrap();
        st.hwnd_raw as *mut c_void
    });
    let drag = unsafe {
        // SHDoDragDrop 要求调用线程已按 STA 初始化 OLE。`OleInitialize` 在已初始化的线程上
        // 返回 S_FALSE，那也算 is_ok()，照它配对 OleUninitialize 就会把同线程的 COM 套间抽干
        // （`dialog.rs` / `wic.rs` 同一课）。这一线程活到进程结束，所以只加一次、不卸。
        if !OLE_OWNED.with(|c| c.get()) && OleInitialize(None).is_ok() {
            OLE_OWNED.with(|c| c.set(true));
        }
        run_shell_drag(hwnd, &paths)
    };
    if let Err(why) = drag {
        push_error(format!("拖拽失败：{why}"));
    } else {
        // 拖成功 = 落点方已经拿到完整文件，本机这一份就是垃圾，删掉。
        // 取消 / 落点不收时**全留着**：用户多半立刻再拖一次，几十 MB 的视频删了就是重取。
        // 删除必须在锁外做，失败也只能攒着出去报：`push_error` 自己会取同一把 `UiState` 锁，
        // 而 std Mutex 不可重入 —— 持锁期间删一个被资源管理器/杀软占住的文件，一次提示
        // 就变成整壳冻结（界面再也不响应，且没有任何日志指向这里）。
        let leftover: Vec<String> = paths
            .iter()
            .filter_map(|p| std::fs::remove_file(p).err().map(|e| format!("{p}（{e}）")))
            .collect();
        {
            let mut st = arc.lock().unwrap();
            for i in &ids {
                st.album.forget_drag(*i);
                // 这一批已经搬走了：选中态一律清掉，留着会让下一次「导出」重复搬同一批
                if st.album.selected.contains(i) {
                    st.album.toggle_selected(*i);
                }
            }
            st.album.drag_result = None;
            st.ui_rev += 1;
        }
        for p in leftover {
            push_error(format!("拖拽用的临时文件没删掉，请手动清理：{p}"));
        }
    }
}

/// 数据对象 advertise 的剪贴板格式号表。生产路径拿它做"空格式表"自检：
/// `SHCreateDataObject` 参数用错时不会报错，只会给一个什么格式都没有的对象，
/// 用户端表现为到处是灰色禁止圈
#[cfg(windows)]
pub(crate) unsafe fn enumerate_cf(obj: &IDataObject) -> Vec<u16> {
    use windows::Win32::System::Com::FORMATETC;
    let mut out = Vec::new();
    // DATADIR_GET = 1
    let Ok(en) = obj.EnumFormatEtc(1) else {
        return out;
    };
    loop {
        let mut one = [FORMATETC::default()];
        let mut got = 0u32;
        if en.Next(&mut one, Some(&mut got)).is_err() || got == 0 {
            break;
        }
        out.push(one[0].cfFormat);
    }
    out
}

/// 一次 shell 拖放：`SHCreateDataObject` 建 `IDataObject`（**由 shell 实现，本壳不写
/// IDropSource/IDataObject 的 vtable**）→ `SHDoDragDrop`（自带模态消息循环）。
/// **参数用法是这条路上唯一的坑**：必须给「目录绝对 pidl + 子项**相对** pidl」。图省事传
/// `pidlFolder=None` + 一个绝对 pidl，它会照样返回 `Ok`，但 advertise 的格式表是**空的** ——
/// 拖起来是白色方块、落到任何文件夹都是灰色禁止圈，而且一句话都不说
unsafe fn run_shell_drag(hwnd: HWND, paths: &[String]) -> Result<(), String> {
    use windows::Win32::UI::Shell::Common::ITEMIDLIST;
    use windows::Win32::UI::Shell::SHSimpleIDListFromPath;

    if paths.is_empty() {
        return Err("这次没有可拖出的文件".to_string());
    }
    // 所有载荷都在同一个临时目录里（`album_drag_dir`），所以 folder 只有一个；
    // 子项 pidl 必须是**相对**的：绝对 pidl + None folder 会得到格式表全空的对象，任何落点只给灰色禁止圈
    let first = std::path::Path::new(&paths[0]);
    let dir = first
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .ok_or_else(|| "拖拽载荷路径看不出所在目录".to_string())?;
    // 子项是**相对**名，只能挂在一个父目录上：跨目录的混合载荷会把除第一份以外的每份都挂到错的父项，宁可不拖
    for p in &paths[1..] {
        let parent = std::path::Path::new(p).parent();
        if parent != Some(dir) {
            return Err(format!(
                "这一批拖出的载荷不在同一个目录（{} 与 {}），已中止",
                dir.display(),
                parent.map(|x| x.display().to_string()).unwrap_or_default()
            ));
        }
    }
    let folder: *mut ITEMIDLIST = ILCreateFromPathW(&windows::core::HSTRING::from(dir));
    if folder.is_null() {
        return Err(format!("系统把拖拽目录转不成 shell 项：{}", dir.display()));
    }
    let mut children: Vec<*mut ITEMIDLIST> = Vec::with_capacity(paths.len());
    for p in paths {
        let name = match std::path::Path::new(p).file_name() {
            Some(n) => n.to_os_string(),
            None => {
                ILFree(Some(folder));
                for c in children {
                    ILFree(Some(c));
                }
                return Err(format!("拖拽载荷看不出文件名：{p}"));
            }
        };
        let child: *mut ITEMIDLIST = SHSimpleIDListFromPath(&windows::core::HSTRING::from(&name));
        if child.is_null() {
            ILFree(Some(folder));
            for c in children {
                ILFree(Some(c));
            }
            return Err(format!("系统把文件名转不成相对 shell 项：{name:?}"));
        }
        children.push(child);
    }
    let refs: Vec<*const ITEMIDLIST> = children.iter().copied().map(|c| c as *const _).collect();
    let made: windows::core::Result<IDataObject> =
        SHCreateDataObject(Some(folder), Some(&refs), None::<&IDataObject>);
    ILFree(Some(folder));
    for c in children {
        ILFree(Some(c));
    }
    let data = match made {
        Ok(d) => d,
        Err(e) => return Err(format!("创建拖放数据对象失败: {e}")),
    };
    // 空格式表 = 所有目标都拒绝。宁可在这里就报错，也不要让用户去猜那个禁止圈
    if enumerate_cf(&data).is_empty() {
        return Err(
            "系统给的拖放数据对象没有任何格式（这次一定放不进任何文件夹）：请重试一次".to_string(),
        );
    }
    // drop source 传 `None` = 用 shell 自带实现（拖到可放置目标即复制、Esc 取消）
    let effect = SHDoDragDrop(hwnd, &data, None::<&IDropSource>, DROPEFFECT_COPY);
    match effect {
        // 取消时 shell 把效果码清成 0：不判这一条，"拖了个空"就什么也不会说 ——
        // 用户看到的正是：格子按下去了、文件没出现、界面一句话都没有
        Ok(e) if e == DROPEFFECT_NONE => Err(
            "拖拽没有落点（取消，或目标不接受文件）：请拖到资源管理器窗口或微信聊天框里"
                .to_string(),
        ),
        Ok(e) if e.0 & DROPEFFECT_COPY.0 != 0 => Ok(()),
        Ok(e) => Err(format!(
            "放下了，但目标给的效果是 {:#x}（不是复制）：换一个文件夹窗口再拖",
            e.0
        )),
        Err(e) => Err(format!("SHDoDragDrop 失败: {e}")),
    }
}

/// 填待发送路径（对话框与拖放两个入口共用）。
fn set_send_path(path: String) {
    let Some(arc) = shared_state() else {
        push_error("界面状态尚未就绪，路径没有填入".to_string());
        return;
    };
    let mut st = arc.lock().unwrap();
    st.send_path_input = path;
}

fn push_error(msg: String) {
    if let Some(arc) = shared_state() {
        arc.lock().unwrap().push_error(msg);
    }
}

/// 用注册表真值刷新「开机自启动」：ini 与注册表不一致时**以注册表为准**并把 ini 同步过去
/// （用户可能自己在注册表里改过，也可能被安全软件清掉了）。
/// 读写都在锁外做；读不到时保持界面原样并出声 —— "读不到"不等于"没开"。
fn sync_autostart_truth() {
    let observed = autostart::observe();
    let Some(arc) = shared_state() else {
        return;
    };
    let mut st = arc.lock().unwrap();
    let entry = match observed {
        Ok(e) => e,
        Err(why) => {
            st.push_error(format!("读不到开机自启动的状态：{why}"));
            return;
        }
    };
    let (on, ours) = (entry.enabled(), entry.is_ours());
    let changed = st.autostart != on || st.autostart_is_ours != ours;
    st.autostart = on;
    st.autostart_is_ours = ours;
    if !changed {
        return;
    }
    st.ui_rev += 1;
    let snap = settings::Settings::from_state(&st);
    drop(st);
    snap.save();
}

/// 回答一次关闭询问：按钮、回车、Esc 三条入口都走这里，决策只在 `decide_close` 一处
fn answer_close_prompt(hwnd: HWND, choice: CloseChoice) {
    let Some(arc) = shared_state() else {
        return;
    };
    let (action, snap) = {
        let mut st = arc.lock().unwrap();
        let d = decide_close(st.close_behavior, st.close_remember, Some(choice));
        st.close_prompt = false;
        st.close_remember = false;
        st.ui_rev += 1;
        let snap = match d.persist {
            Some(b) => {
                st.close_behavior = b;
                Some(settings::Settings::from_state(&st))
            }
            None => None,
        };
        (d.action, snap)
    };
    if let Some(s) = snap {
        s.save();
    }
    match action {
        CloseAction::Minimize => hide_to_tray(hwnd),
        CloseAction::Exit => request_exit(hwnd),
        _ => {}
    }
    let _ = unsafe { InvalidateRect(hwnd, None, true) };
}

/// 把文件拖进窗口 → 填成待发送路径（不代发，与「选择…」一致）。
/// 一次只取第一个：引擎侧同一时刻只有一个 `SendTask`；哪些没被采纳必须说清楚，否则像丢了文件
fn on_drop_files(hwnd: HWND, hdrop_raw: *mut c_void) {
    if hdrop_raw.is_null() {
        return;
    }
    let hdrop = HDROP(hdrop_raw);
    let mut first: Option<String> = None;
    let mut skipped = 0usize;
    let mut dirs = 0usize;
    let total;
    unsafe {
        // 两次调用：传 None 问长度，再按长度取串
        total = DragQueryFileW(hdrop, u32::MAX, None) as usize;
        for i in 0..total {
            let len = DragQueryFileW(hdrop, i as u32, None) as usize;
            let mut buf = vec![0u16; len + 1];
            let got = DragQueryFileW(hdrop, i as u32, Some(&mut buf)) as usize;
            buf.truncate(got);
            let path = match String::from_utf16(&buf) {
                // 路径不是合法 UTF-16 时宁可拒绝，也不用 lossy 造一个带 U+FFFD 的假路径 —— 那会在几十秒后变成"文件不存在"
                Ok(s) => s,
                Err(_) => {
                    skipped += 1;
                    continue;
                }
            };
            if path.trim().is_empty() {
                skipped += 1;
                continue;
            }
            if std::path::Path::new(&path).is_dir() {
                dirs += 1;
                continue;
            }
            if first.is_none() {
                first = Some(path);
            }
        }
        // 系统分配的 drop 结构必须显式释放，否则每次拖拽漏一块进程堆
        DragFinish(hdrop);
    }
    let usable = total - skipped - dirs;
    let Some(arc) = shared_state() else { return };
    let mut st = arc.lock().unwrap();
    match first {
        None => st.push_error(if total == 0 {
            "系统没给出拖入的文件，请改用「选择…」".to_string()
        } else {
            format!("拖进来的 {total} 项里没有可发送的文件（文件夹不算），请改用「选择…」")
        }),
        Some(path) => {
            st.send_path_input = path;
            st.active_tab = render::TAB_FILES;
            st.list_hover = None;
            st.input_focus = FOCUS_NONE;
            // 备注合成一条：错误列表只留 `MAX_ERRORS` 条，拆成三行会把真正的错误挤掉
            let mut notes: Vec<String> = Vec::new();
            if usable > 1 {
                notes.push(format!(
                    "一次只能发送一个文件，已填入第 1 个（共拖入 {usable} 个）"
                ));
            }
            if dirs > 0 {
                notes.push(format!("{dirs} 个文件夹不能直接发送"));
            }
            if skipped > 0 {
                notes.push(format!("{skipped} 项读不出文件名，已跳过"));
            }
            if !notes.is_empty() {
                st.push_error(notes.join("；"));
            }
        }
    }
    drop(st);
    let _ = unsafe { InvalidateRect(hwnd, None, true) };
}

/// 鼠标移动：更新悬停并切换手型光标，状态确有变化时请求重绘。
/// `left_down` 取自 wparam 的 `MK_LBUTTON`：相册"按住拖动"只能靠它判定，自己记按下状态会在鼠标冲出窗口时留一个幽灵按压
fn on_mouse_move(hwnd: HWND, x: i32, y: i32, left_down: bool) {
    // 相册：按住格子挪出阈值 = 拖出（此时不再算点击）
    if let Some((px, py, cell)) = ALBUM_PRESS.get() {
        let pref = shared_state().map(|a| a.lock().unwrap().theme);
        let Some(t) = pref else {
            ALBUM_PRESS.set(None);
            return;
        };
        if !left_down {
            // 按钮已经不在我们窗口里抬起来的（被别的窗口抢走/移出）：按压记账作废
            ALBUM_PRESS.set(None);
        } else {
            let env = theme::detect(hwnd, t);
            if (x - px).abs() + (y - py).abs() > env.px(ALBUM_DRAG_PX) {
                ALBUM_PRESS.set(None);
                album_drag_out(cell);
                let _ = unsafe { InvalidateRect(hwnd, None, true) };
                return;
            }
        }
    }
    let Some(arc) = shared_state() else {
        return;
    };
    let (nav, list, clickable) = {
        let st = arc.lock().unwrap();
        let env = theme::detect(hwnd, st.theme);
        let (n, l) = render::hover_at(&st, &env, x, y);
        let c = render::hit_test(&st, &env, x, y).is_some();
        (n, l, c)
    };
    unsafe {
        let c = if clickable {
            LoadCursorW(None, IDC_HAND)
        } else {
            LoadCursorW(None, IDC_ARROW)
        };
        if let Ok(c) = c {
            let _ = SetCursor(c);
        }
    }
    let changed = {
        let mut st = arc.lock().unwrap();
        if st.nav_hover == nav && st.list_hover == list {
            false
        } else {
            st.nav_hover = nav;
            st.list_hover = list;
            true
        }
    };
    if changed {
        let _ = unsafe { InvalidateRect(hwnd, None, true) };
    }
}

/// 鼠标离开窗口：清空悬停高亮
fn clear_hover(hwnd: HWND) {
    let Some(arc) = shared_state() else {
        return;
    };
    let changed = {
        let mut st = arc.lock().unwrap();
        let c = st.nav_hover.is_some() || st.list_hover.is_some();
        st.nav_hover = None;
        st.list_hover = None;
        c
    };
    if changed {
        let _ = unsafe { InvalidateRect(hwnd, None, true) };
    }
}

/// 弹出 worker 排入的系统消息通知（UI 线程执行；一次消息最多弹 `MAX_PENDING_TOASTS` 条）
fn drain_pending_toasts() -> bool {
    let Some(arc) = shared_state() else {
        return false;
    };
    // 先取出再调用 Shell API，避免持锁期间做系统调用
    let queued: Vec<(String, String)> = std::mem::take(&mut arc.lock().unwrap().pending_toasts);
    if queued.is_empty() {
        return false;
    }
    for (title, text) in queued {
        if !unsafe { crate::tray::show_balloon(&title, &text) } {
            // 弹窗被系统拒绝：记一次可读原因，便于真机排障（相同内容不重复刷屏）
            let msg =
                "[系统弹窗失败] 托盘图标未建立或被系统禁止（检查「通知和操作」设置）".to_string();
            arc.lock().unwrap().push_error(msg);
        }
    }
    true
}

/// 剪贴板变更：本机复制 → （开关开且已配对）请求发送；同时更新最近发送
fn on_clipboard_update() {
    let Some(arc) = shared_state() else {
        return;
    };
    let Some(text) = crate::clipboard::read_text() else {
        return;
    };
    if text.is_empty() {
        return;
    }
    let mut st = arc.lock().unwrap();
    st.clip_out = text.clone();
    // 防回声：与刚应用的对端内容相同则忽略一次
    if st.clip_sync && st.link_paired() && text != st.last_applied_clip {
        st.send_clip_req = Some(text);
    }
}

/// 从状态里取窗口句柄（供 hit_test 需要的 Env 使用）
fn hwnd_of(st: &UiState) -> HWND {
    HWND(st.hwnd_raw as *mut c_void)
}

// ---------- 文本输入（WM_CHAR / WM_KEYDOWN）----------
// 自绘界面没有原生 EDIT 控件，键盘输入由 WndProc 直接落到 `UiState` 的焦点字段：
// `WM_CHAR` 追加可打印字符（控制字符忽略）；`VK_BACK` 按 char 边界删，中文也能正确删

/// `WM_CHAR`：把可打印字符追加到当前焦点输入框
fn on_char(code: u32) {
    let Some(arc) = shared_state() else {
        return;
    };
    let Some(ch) = char::from_u32(code) else {
        return; // 非法码点（代理项/超范围）直接忽略
    };
    if ch.is_control() {
        return;
    }
    let mut st = arc.lock().unwrap();
    let Some(field) = st.focused_input_mut() else {
        return;
    };
    if field.chars().count() >= MAX_INPUT_CHARS {
        return;
    }
    field.push(ch);
}

/// `WM_KEYDOWN(VK_BACK)`：删除焦点输入框的最后一个字符
fn on_backspace() {
    let Some(arc) = shared_state() else {
        return;
    };
    let mut st = arc.lock().unwrap();
    if let Some(field) = st.focused_input_mut() {
        let _ = field.pop();
    }
}

/// `WM_KEYDOWN(VK_RETURN)`：弹窗开着时回车 = 按默认那颗按钮；否则回复框里回车即发送。
/// 此前自绘界面没有任何一处应答回车，而"打完字按回车"是输入的人最自然的下一步——不给就是界面没做完。
fn on_enter(hwnd: HWND) {
    let Some(arc) = shared_state() else {
        return;
    };
    // 同一条纪律：条件的临时守卫活到 if 块结束，块里再锁就自死锁
    let close_prompt = arc.lock().unwrap().close_prompt;
    if close_prompt {
        answer_close_prompt(hwnd, CloseChoice::Minimize);
        return;
    }
    let mut st = arc.lock().unwrap();
    if st.input_focus != FOCUS_REPLY {
        return;
    }
    submit_reply(&mut st);
}

/// `WM_KEYDOWN(VK_ESCAPE)`：关闭询问弹窗上 Esc = 取消（窗口不动）。
/// 只管这一个弹窗：输入框里的内容不靠 Esc 撤，免得一次误按把用户打的字清了
fn on_escape(hwnd: HWND) {
    let Some(arc) = shared_state() else {
        return;
    };
    if !arc.lock().unwrap().close_prompt {
        return;
    }
    answer_close_prompt(hwnd, CloseChoice::Cancel);
}

/// 请求系统在鼠标离开时发 `WM_MOUSELEAVE`（一次性，每次移动都要重新登记）
fn request_leave_tracking(hwnd: HWND) {
    unsafe {
        let mut tme = TRACKMOUSEEVENT {
            cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
            dwFlags: TME_LEAVE,
            hwndTrack: hwnd,
            dwHoverTime: 0,
        };
        let _ = TrackMouseEvent(&mut tme);
    }
}

/// 托盘点一下要把窗口摆回来时用哪条 `ShowWindow` 命令。
/// 「最小化到托盘」走的是 `SW_HIDE`（窗口是藏起来的，不是最小化的），对这种窗口 `SW_RESTORE`
/// 不顶事；真的最小化过（任务栏那条）才需要 `SW_RESTORE`。判错就是"图标亮着、点它没反应"
fn restore_cmd(iconic: bool) -> SHOW_WINDOW_CMD {
    if iconic {
        SW_RESTORE
    } else {
        SW_SHOW
    }
}

/// 把窗口摆回前台：托盘那一下与"第二实例请它现身"用的是同一套动作
fn bring_to_front(hwnd: HWND) {
    unsafe {
        let iconic = IsIconic(hwnd).as_bool();
        let _ = ShowWindow(hwnd, restore_cmd(iconic));
        let _ = SetForegroundWindow(hwnd);
        let _ = InvalidateRect(hwnd, None, true);
    }
    // 藏在托盘里的那一次是 `SW_HIDE`，不送 `WM_SIZE`，优先级只能在这里要回来
    set_memory_priority(false);
}

extern "system" fn wnd_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_PAINT => {
            let mut ps = unsafe { std::mem::zeroed::<PAINTSTRUCT>() };
            let hdc = unsafe { BeginPaint(hwnd, &mut ps) };
            // 最小化中不重绘：BeginPaint/EndPaint 仍要调用，否则无效区不消失、消息风暴
            if unsafe { IsIconic(hwnd) }.as_bool() {
                let _ = unsafe { EndPaint(hwnd, &ps) };
                return LRESULT(0);
            }
            let rc = unsafe {
                let mut rc: RECT = std::mem::zeroed();
                let _ = windows::Win32::UI::WindowsAndMessaging::GetClientRect(hwnd, &mut rc);
                rc
            };
            // 双缓冲：先画到内存 DC 再一次性 BitBlt，避免动效期间闪烁
            let back = unsafe { back_buffer(hdc, rc.right - rc.left, rc.bottom - rc.top) };
            let target = back.unwrap_or(hdc);
            match shared_state() {
                Some(arc) => {
                    let mut st = arc.lock().unwrap();
                    render::paint(target, hwnd, &st);
                    // 相册：把"这一帧真正画得下几格"回写进状态 —— worker 只给画得出来的格子下载缩略图。
                    // 只写两个整数、不 bump ui_rev：回写本身不改画面，bump 了就成"重绘→推进序号→再重绘"的自激循环
                    if st.active_tab == render::TAB_ALBUM {
                        let env = theme::detect(hwnd, st.theme);
                        render::album_note_visible(&mut st, &env);
                    }
                }
                None => render::paint(target, hwnd, &UiState::default()),
            }
            if back.is_some() {
                let _ = unsafe { BitBlt(hdc, 0, 0, rc.right, rc.bottom, target, 0, 0, SRCCOPY) };
            }
            let _ = unsafe { EndPaint(hwnd, &ps) };
            LRESULT(0)
        }
        WM_ERASEBKGND => {
            // 整窗自绘且双缓冲铺满 → 抑制默认擦除以消除闪烁
            LRESULT(1)
        }
        WM_SIZE => {
            // 判状态用 `IsIconic` 而不是 wparam 的 `SIZE_MINIMIZED`（0 还原 / 1 最小化 / 2 最大化
            // 极易记错，记错了就变成"最大化时拆缓冲、最小化时什么都不做"），并先让 DefWindowProc
            // 更新状态再问。**定时器不能停**：`WM_TIMER` 还负责派发通知弹窗。
            let _ = unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) };
            if unsafe { IsIconic(hwnd) }.as_bool() {
                shed_display_memory();
            } else {
                set_memory_priority(false);
            }
            let _ = unsafe { InvalidateRect(hwnd, None, true) };
            LRESULT(0)
        }
        WM_GETMINMAXINFO => {
            // 自绘布局有自己的最小可画尺寸：不挡住的话用户能把窗口压到侧栏最后一项消失、
            // 内容画到客户区外。换算与 `create_main_window` 同一套：逻辑 → 本窗口 DPI 的物理像素，
            // 再由 AdjustWindowRectExForDpi 加回边框，得到**外框**最小跟踪尺寸
            let mmi = lparam.0 as *mut MINMAXINFO;
            if !mmi.is_null() {
                unsafe {
                    let dpi = GetDpiForWindow(hwnd).max(96) as u32;
                    let scale = dpi as f32 / 96.0;
                    let mut rc = RECT {
                        left: 0,
                        top: 0,
                        right: (render::LAYOUT_MIN_W as f32 * scale).round() as i32,
                        bottom: (render::LAYOUT_MIN_H as f32 * scale).round() as i32,
                    };
                    let _ = AdjustWindowRectExForDpi(
                        &mut rc,
                        WS_OVERLAPPEDWINDOW,
                        false,
                        Default::default(),
                        dpi,
                    );
                    (*mmi).ptMinTrackSize = POINT {
                        x: rc.right - rc.left,
                        y: rc.bottom - rc.top,
                    };
                }
            }
            LRESULT(0)
        }
        WM_DPICHANGED => {
            // 换到不同 DPI 的显示器：按系统建议的物理尺寸重排外框，随后整窗重绘
            let suggested = lparam.0 as *const RECT;
            if !suggested.is_null() {
                let r = unsafe { *suggested };
                unsafe {
                    let _ = windows::Win32::UI::WindowsAndMessaging::SetWindowPos(
                        hwnd,
                        None,
                        r.left,
                        r.top,
                        r.right - r.left,
                        r.bottom - r.top,
                        Default::default(),
                    );
                }
            }
            let _ = unsafe { InvalidateRect(hwnd, None, true) };
            LRESULT(0)
        }
        WM_SETTINGCHANGE | WM_THEMECHANGED | WM_SYSCOLORCHANGE => {
            // 系统切明暗/改字体/改方案色：失效缓存整窗重绘；标题栏是 DWM 画的，还得按新明暗重设染色
            theme::invalidate_theme_cache();
            if let Some(arc) = shared_state() {
                let pref = arc.lock().unwrap().theme;
                theme::apply_caption(hwnd, pref);
            }
            let _ = unsafe { InvalidateRect(hwnd, None, true) };
            LRESULT(0)
        }
        WM_TIMER => {
            let toasts = drain_pending_toasts();
            // 系统媒体卡：本线程就是消息循环所在的那条，卡片状态与按钮派发都在这里
            crate::smtc::tick();
            // 仅在确有动画或内容变化时重绘，静止时不空转
            let need = toasts
                || shared_state()
                    .map(|arc| render::wants_repaint(&arc.lock().unwrap()))
                    .unwrap_or(false);
            if need {
                let _ = unsafe { InvalidateRect(hwnd, None, true) };
            }
            LRESULT(0)
        }
        WM_APP_STATE_CHANGED => {
            let _ = unsafe { InvalidateRect(hwnd, None, true) };
            LRESULT(0)
        }
        WM_CLIPBOARDUPDATE => {
            on_clipboard_update();
            let _ = unsafe { InvalidateRect(hwnd, None, true) };
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            request_leave_tracking(hwnd);
            let lp = lparam.0 as u32;
            let x = (lp & 0xFFFF) as u16 as i16 as i32;
            let y = ((lp >> 16) & 0xFFFF) as u16 as i16 as i32;
            // 正在拖滚动条时这一帧只做一件事：走正常悬停/拖出逻辑会在窄轨道上误判成一次拖拽
            if BAR_DRAGGING.get() {
                if (wparam.0 & MK_LBUTTON) == 0 || unsafe { GetCapture() } != hwnd {
                    BAR_DRAGGING.set(false);
                } else if let Some(arc) = shared_state() {
                    let mut st = arc.lock().unwrap();
                    if render::drag_scroll_bar(hwnd, &mut st, y) {
                        st.ui_rev += 1;
                        drop(st);
                        let _ = unsafe { InvalidateRect(hwnd, None, true) };
                    }
                }
                return LRESULT(0);
            }
            let left_down = (wparam.0 & MK_LBUTTON) != 0;
            on_mouse_move(hwnd, x, y, left_down);
            LRESULT(0)
        }
        WM_MOUSEWHEEL => {
            // 滚轮一格一行；只有带滚动条的页应答，其余页交给默认处理，免得滚轮被这里整个吃掉
            let delta = ((wparam.0 >> 16) & 0xFFFF) as u16 as i16 as i32;
            let mut scrolled = false;
            if delta != 0 {
                if let Some(arc) = shared_state() {
                    let mut st = arc.lock().unwrap();
                    scrolled = render::wheel_scroll_bar(hwnd, &mut st, delta > 0);
                    if scrolled {
                        st.ui_rev += 1;
                    }
                }
            }
            if !scrolled {
                // 说了"交给默认处理"就得真的交回去：返 0 等于把滚轮整个吞掉
                return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) };
            }
            let _ = unsafe { InvalidateRect(hwnd, None, true) };
            LRESULT(0)
        }
        WM_MOUSELEAVE => {
            clear_hover(hwnd);
            LRESULT(0)
        }
        WM_DROPFILES => {
            on_drop_files(hwnd, wparam.0 as *mut c_void);
            LRESULT(0)
        }
        WM_LBUTTONDOWN => {
            // 低 16 位 x、高 16 位 y（i16 符号扩展，兼容多显示器负坐标）
            let lp = lparam.0 as u32;
            let x = (lp & 0xFFFF) as u16 as i16 as i32;
            let y = ((lp >> 16) & 0xFFFF) as u16 as i16 as i32;
            on_left_click(x, y);
            let _ = unsafe { InvalidateRect(hwnd, None, true) };
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            if BAR_DRAGGING.take() && unsafe { GetCapture() } == hwnd {
                let _ = unsafe { ReleaseCapture() };
            }
            // 相册格子的「选中」在这里落地：抬起时按压记录还在 = 没挪出拖拽阈值 = 这是一次点击
            if let Some((_, _, cell)) = ALBUM_PRESS.take() {
                on_album_cell_up(cell);
                let _ = unsafe { InvalidateRect(hwnd, None, true) };
            }
            let _ = unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) };
            LRESULT(0)
        }
        // 托盘图标回调。以前只注册不接：图标亮着，点它没任何反应，最小化之后只能去任务栏找。
        // 事件码看协议版本：`tray.rs` 下发的是 v4，选中上报 `NIN_SELECT`/`NIN_KEYSELECT`；
        // 只有 SETVERSION 没成功的旧 shell 才回落到鼠标消息。两套都接，漏一套就是"在某些机器
        // 上点了没反应"这种查起来最费时间的形态。
        crate::tray::CALLBACK => {
            if matches!(
                (lparam.0 as u32) & 0xFFFF,
                NIN_SELECT | NIN_KEYSELECT | WM_LBUTTONUP | WM_LBUTTONDBLCLK
            ) {
                // 藏在托盘里的窗口是被 `SW_HIDE` 藏掉的（`--minimized` 拉起的那一次、选了
                // 「最小化到托盘」的那一次），对它 `SW_RESTORE` 不顶事；真最小化过才需要还原。
                // 判错就是"图标亮着、点它没反应"。最小化时释放过的后备缓冲由 `WM_PAINT` 重建
                bring_to_front(hwnd);
            }
            LRESULT(0)
        }
        WM_CAPTURECHANGED => {
            // 系统收走捕获（切窗口、弹模态）时不会再送 `WM_LBUTTONUP`：不在这儿收口，拖拽态会一直
            // 挂着，下一次鼠标进窗先被"还在拖"那一支吞掉一次移动
            if BAR_DRAGGING.take() {
                let _ = unsafe { InvalidateRect(hwnd, None, true) };
            }
            let _ = unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) };
            LRESULT(0)
        }
        WM_CHAR => {
            on_char(wparam.0 as u32);
            let _ = unsafe { InvalidateRect(hwnd, None, true) };
            LRESULT(0)
        }
        WM_KEYDOWN => {
            if wparam.0 == VK_BACK.0 as usize {
                on_backspace();
                let _ = unsafe { InvalidateRect(hwnd, None, true) };
            }
            if wparam.0 == VK_RETURN.0 as usize {
                on_enter(hwnd);
                let _ = unsafe { InvalidateRect(hwnd, None, true) };
            }
            if wparam.0 == VK_ESCAPE.0 as usize {
                on_escape(hwnd);
                let _ = unsafe { InvalidateRect(hwnd, None, true) };
            }
            LRESULT(0)
        }
        WM_CLOSE => {
            // 点关闭按钮（含 Alt+F4）按用户选的「关闭按钮行为」走。重启确认弹窗开着时这里
            // 直接吞掉：两个模态叠在一起，谁都不认得这颗 ✕
            let Some(arc) = shared_state() else {
                return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) };
            };
            let action = {
                let mut st = arc.lock().unwrap();
                if st.restart_prompt {
                    return LRESULT(0);
                }
                let d = decide_close(st.close_behavior, false, None);
                if d.action == CloseAction::Ask {
                    st.close_prompt = true;
                    st.close_remember = false;
                    // 弹窗期间不留输入焦点：否则敲下去的字进的是一个已经被盖住的输入框
                    st.input_focus = FOCUS_NONE;
                    st.ui_rev += 1;
                }
                d.action
            };
            match action {
                CloseAction::Ask => {
                    let _ = unsafe { InvalidateRect(hwnd, None, true) };
                    LRESULT(0)
                }
                CloseAction::Minimize => {
                    hide_to_tray(hwnd);
                    LRESULT(0)
                }
                // 取消：窗口原地不动
                CloseAction::Stay => LRESULT(0),
                // 退出交给默认处理：WM_DESTROY → PostQuitMessage，`main` 的收尾一样不少
                CloseAction::Exit => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
            }
        }
        // 已经答过一次（弹窗里选了退出）或功能改动要重启：不再询问，直接走默认收尾
        WM_APP_EXIT => {
            let _ = unsafe { DefWindowProcW(hwnd, WM_CLOSE, WPARAM(0), LPARAM(0)) };
            LRESULT(0)
        }
        // 第二实例（再双击一次图标）或外部转交文件：把窗口摆回前台
        WM_APP_SHOW => {
            bring_to_front(hwnd);
            LRESULT(0)
        }
        WM_DESTROY => {
            unsafe { PostQuitMessage(0) };
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

/// 供 main.rs 释放双缓冲资源（进程退出前）
#[allow(static_mut_refs)]
pub(crate) unsafe fn release_back_buffer() {
    if let Some(b) = BACK.take() {
        unsafe { destroy_back_buffer(b) };
    }
}

/// 窗口不再显示时把本进程的内存优先级降到 LOW，恢复可见时改回 NORMAL：内存吃紧时系统先换出
/// "没在显示"的进程。它不会让任务管理器的数字变小（实测常驻 29.5 → 29.6 MB），改的是被牺牲的顺序。
/// 结果必须落埋点 —— 这一调用失败时窗口一切如常，是"优化没生效但没人知道"的形态。
fn set_memory_priority(low: bool) {
    use windows::Win32::System::Threading::{
        GetCurrentProcess, ProcessMemoryPriority, SetProcessInformation,
        MEMORY_PRIORITY_INFORMATION, MEMORY_PRIORITY_LOW, MEMORY_PRIORITY_NORMAL,
    };
    let info = MEMORY_PRIORITY_INFORMATION {
        MemoryPriority: if low {
            MEMORY_PRIORITY_LOW
        } else {
            MEMORY_PRIORITY_NORMAL
        },
    };
    let r = unsafe {
        SetProcessInformation(
            GetCurrentProcess(),
            ProcessMemoryPriority,
            &info as *const MEMORY_PRIORITY_INFORMATION as *const core::ffi::c_void,
            std::mem::size_of::<MEMORY_PRIORITY_INFORMATION>() as u32,
        )
    };
    let want = if low { "low" } else { "normal" };
    let note = match &r {
        Ok(()) => want.to_string(),
        Err(e) => format!("{want} 没改成: {e}"),
    };
    debuglog::log!(
        if r.is_ok() {
            debuglog::Level::Info
        } else {
            debuglog::Level::Warn
        },
        "ui",
        "mem.priority",
        &[("note", &note)]
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 字面量 Win32 常量的值断言：取字面量就必须锁住值，防止再次写错
    #[test]
    fn win32_constants_have_correct_values() {
        assert_eq!(WM_MOUSELEAVE, 0x02A3, "WM_MOUSELEAVE 必须为 0x02A3");
        assert_ne!(WM_MOUSELEAVE, 0x0216, "0x0216 不是 WM_MOUSELEAVE");
        assert_eq!(WM_CLIPBOARDUPDATE, 0x031D);
        assert_eq!(WM_DPICHANGED, 0x02E0);
        assert_eq!(WM_SETTINGCHANGE, 0x001A);
        assert_eq!(WM_THEMECHANGED, 0x031A);
        // 最小化状态一律用 IsIconic 问窗口本身，不比对 WM_SIZE 的 wparam
        //（SIZE_RESTORED=0 / SIZE_MINIMIZED=1 / SIZE_MAXIMIZED=2，记错一位就变成"最大化拆缓冲、最小化不做事"）
        assert_eq!(WM_ERASEBKGND, 0x0014);
        assert_eq!(WM_APP, 0x8000);
        assert_eq!(WM_CHAR, 0x0102);
        assert_eq!(WM_KEYDOWN, 0x0100);
        assert_ne!(WM_CHAR, WM_KEYDOWN);
        assert_eq!(VK_BACK.0, 0x08);
        assert_eq!(VK_ESCAPE.0, 0x1B, "Esc 写错就关不掉询问弹窗");
        // 四条自建消息不得撞车（撞了就是"重绘请求被当成退出"这种最难查的形态）
        assert_eq!(WM_APP_STATE_CHANGED, WM_APP + 2);
        assert_eq!(WM_APP_EXIT, WM_APP + 3);
        assert_eq!(WM_APP_SHOW, WM_APP + 4);
        assert_ne!(WM_APP_STATE_CHANGED, WM_APP + 1);
        assert_ne!(WM_APP_EXIT, WM_APP_STATE_CHANGED);
        assert_ne!(WM_APP_EXIT, WM_APP + 1);
        assert_ne!(WM_APP_SHOW, WM_APP_EXIT);
        assert_ne!(WM_APP_SHOW, WM_APP_STATE_CHANGED);
        assert_ne!(WM_APP_SHOW, crate::tray::CALLBACK);
        // 托盘：注册的那条消息必须就是 WndProc 里分支用的同一条（各写一遍数字就会"注册了没人接"）
        assert_eq!(crate::tray::CALLBACK, WM_APP + 1);
        assert_ne!(crate::tray::CALLBACK, WM_APP_STATE_CHANGED);
    }

    /// 藏在托盘里的窗口是被 `SW_HIDE` 藏掉的，不是最小化：判错就是"图标亮着、点它没反应"，
    /// 「最小化到托盘」这条路径整个废掉
    #[test]
    fn tray_restore_shows_hidden_windows_and_restores_minimized_ones() {
        assert_eq!(
            restore_cmd(false).0,
            SW_SHOW.0,
            "被藏起来的窗口用 SW_SHOW 摆回来"
        );
        assert_eq!(
            restore_cmd(true).0,
            SW_RESTORE.0,
            "真最小化过的才用 SW_RESTORE"
        );
        assert_ne!(SW_SHOW.0, SW_HIDE.0);
        assert_ne!(SW_SHOW.0, SW_RESTORE.0);
    }

    /// 拖出载荷必须 advertise `CF_HDROP(15)`，否则落到任何文件夹都是灰色禁止圈。
    /// 根因在 `SHCreateDataObject` 的参数：传 `pidlFolder=None` + 绝对 pidl 时它**照样返回 Ok**，
    /// 却给了一个格式表全空的对象、一点都不报错。这条测试钉住生产用法，谁改回"省事"写法就红
    #[test]
    fn drag_payload_advertises_hdrop() {
        use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};
        use windows::Win32::UI::Shell::Common::ITEMIDLIST;
        use windows::Win32::UI::Shell::SHSimpleIDListFromPath;

        let dir = std::env::temp_dir().join("linkx-drag-probe");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("probe.txt");
        std::fs::write(&f, b"probe").unwrap();

        let formats = unsafe {
            // 测试线程可能已被同进程的其他用例初始化过 COM：失败与否都不影响这一步
            let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
            let folder = ILCreateFromPathW(&windows::core::HSTRING::from(dir.as_path()));
            // 子项必须是**相对** pidl：只传文件名时 SHSimpleIDListFromPath 给单元素 SIMPLEIDLIST
            let child: *mut ITEMIDLIST =
                SHSimpleIDListFromPath(&windows::core::HSTRING::from("probe.txt"));
            assert!(
                !folder.is_null() && !child.is_null(),
                "shell 连路径项都造不出来，无法判定"
            );
            let made: windows::core::Result<IDataObject> = SHCreateDataObject(
                Some(folder),
                Some(&[child as *const _]),
                None::<&IDataObject>,
            );
            ILFree(Some(folder));
            ILFree(Some(child));
            let obj = made.expect("SHCreateDataObject 失败");
            enumerate_cf(&obj)
        };
        let _ = std::fs::remove_dir_all(&dir);
        assert!(
            formats.contains(&15),
            "拖拽载荷里没有 CF_HDROP(15)，拖出去到处是灰色禁止圈；实际 advertise: {formats:?}"
        );
    }
}
