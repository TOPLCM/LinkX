//! 托盘图标 + 系统消息通知（图标取自 exe 内嵌资源）

use windows::Win32::Foundation::{BOOL, HWND};
use windows::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_SHOWTIP, NIF_TIP, NIIF_INFO, NIM_ADD,
    NIM_DELETE, NIM_MODIFY, NIM_SETVERSION, NOTIFYICONDATAW,
};
use windows::Win32::UI::WindowsAndMessaging::WM_APP;

const TRAY_ID: u32 = 1;
/// 托盘回调消息。`window.rs` 的 WndProc 必须有一支接住它 —— 只注册不接，图标亮着却点不动
pub(crate) const CALLBACK: u32 = WM_APP + 1;
/// 托盘协议版本 4（Vista+）：让气泡走现代通知路径，并在 Win10/11 归入"通知中心"。
/// ⚠ 它按头文件的说法会**改掉事件语义**（选中图标上报 `NIN_SELECT`/`NIN_KEYSELECT` 而不是
/// `WM_LBUTTONUP` 那一套），但本机实测到的仍是 `WM_*` 码 —— 两种说法都有依据，所以 `window.rs`
/// 的回调分支两套都判，并在认不出时把码值记进日志。
const NOTIFYICON_VERSION_4: u32 = 4;

static mut NID: Option<NOTIFYICONDATAW> = None;

pub(crate) fn add_tray_icon(hwnd: HWND) {
    unsafe {
        let mut nid: NOTIFYICONDATAW = std::mem::zeroed();
        nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
        nid.hWnd = hwnd;
        nid.uID = TRAY_ID;
        // NIF_SHOWTIP：协议升到 v4 之后系统默认不显示标准 tooltip，只带 NIF_TIP 的话
        // "LinkX — 待机" 这行悬停文字根本不会出现
        nid.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP | NIF_SHOWTIP;
        nid.uCallbackMessage = CALLBACK;
        // 与窗口/任务栏同一份内嵌图标（build.rs 编入的 linkx.rc 资源 id 1）
        nid.hIcon = crate::window::load_app_icon();

        let tip: Vec<u16> = "LinkX — 待机".encode_utf16().collect();
        let n = tip.len().min(127);
        nid.szTip[..n].copy_from_slice(&tip[..n]);
        nid.szTip[n] = 0;

        let ok: BOOL = Shell_NotifyIconW(NIM_ADD, &nid);
        if !ok.as_bool() {
            crate::say("[LinkX] 托盘图标创建失败，系统弹窗将不可用");
            return;
        }
        // 升级到 v4 协议：不升的话部分 Win10/11 版本不会把气泡升级为系统通知
        nid.Anonymous.uVersion = NOTIFYICON_VERSION_4;
        let _ = Shell_NotifyIconW(NIM_SETVERSION, &nid);
        NID = Some(nid);
    }
}

/// 弹出系统消息通知（气泡；Win10/11 会升级为通知中心通知）。
///
/// 走 `Shell_NotifyIconW(NIM_MODIFY) + NIF_INFO`——不需要 WinRT Toast 与 AUMID 快捷方式
/// 注册，零额外依赖。托盘图标尚未创建（或已被移除）时返回 `false`，由调用方决定如何提示。
///
/// 注意：`szInfo` 上限 256 个 UTF-16 码元、`szInfoTitle` 上限 64，超长会被系统截断。
#[allow(static_mut_refs)]
pub(crate) unsafe fn show_balloon(title: &str, text: &str) -> bool {
    let Some(nid) = (unsafe { NID.as_mut() }) else {
        return false;
    };
    // 同时带上 NIF_ICON/NIF_TIP：既让通知带上应用图标，也避免某些系统版本
    // 因"仅有 NIF_INFO"而判定托盘项信息不完整、丢弃本次气泡。
    nid.uFlags = NIF_INFO | NIF_ICON | NIF_TIP;
    nid.dwInfoFlags = NIIF_INFO;
    fill_wide(&mut nid.szInfoTitle, title);
    fill_wide(&mut nid.szInfo, text);
    unsafe { Shell_NotifyIconW(NIM_MODIFY, nid) }.as_bool()
}

/// 把字符串写入 Win32 定长 UTF-16 缓冲（NUL 结尾，超长截断，余位清零）
fn fill_wide(dst: &mut [u16], s: &str) {
    let cap = dst.len().saturating_sub(1);
    let mut w: Vec<u16> = s.encode_utf16().collect();
    w.truncate(cap);
    let len = w.len();
    dst[..len].copy_from_slice(&w);
    dst[len..].fill(0);
}

// 托盘在 UI 主线程创建/销毁（无并发），static mut 可证安全，故豁免
// static_mut_refs 警告（不做无谓的 Cell/指针体操，避免遮挡可读性）
#[allow(static_mut_refs)]
pub(crate) unsafe fn remove_tray_icon() {
    let mut nid = NID.take();
    if let Some(guard) = nid.as_mut() {
        let _ = Shell_NotifyIconW(NIM_DELETE, guard);
    }
}
