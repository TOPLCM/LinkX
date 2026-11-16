//! Win32 剪贴板：Windows ↔ Android 纯文本同步
//!
//! 只做纯文本读写；所有 Win32 调用均在 `unsafe` 块内并容错（失败一律返回 `None` / 静默），
//! 不 panic。所有权提醒：`SetClipboardData` 成功后 `hmem` 归系统所有，**不可**再 GlobalFree。

use windows::Win32::Foundation::{HANDLE, HGLOBAL, HWND};
use windows::Win32::System::DataExchange::{
    AddClipboardFormatListener, CloseClipboard, EmptyClipboard, GetClipboardData, OpenClipboard,
    SetClipboardData,
};
use windows::Win32::System::Memory::{
    GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock, GMEM_MOVEABLE,
};

/// `CF_UNICODETEXT`：剪贴板 UTF-16 纯文本格式（Win32 常量值 13）。
/// 直接取字面量，避免为单个常量额外开启 `Win32_System_Ole` 特性模块。
const CF_UNICODETEXT: u32 = 13;
/// 扫描 NUL 的最大跨度（**兜底**上限；真实边界以 `GlobalSize` 为准）
const MAX_SCAN_UNITS: usize = 1 << 20;

/// 编译期锁定字面量常量值（SPEC-03）
const _: () = {
    assert!(CF_UNICODETEXT == 13);
};

/// 注册剪贴板变更监听（`WM_CLIPBOARDUPDATE`）；失败静默
pub(crate) fn register(hwnd: HWND) {
    unsafe {
        let _ = AddClipboardFormatListener(hwnd);
    }
}

/// 读取剪贴板纯文本；任一步失败返回 `None`
pub(crate) fn read_text() -> Option<String> {
    unsafe {
        if OpenClipboard(None).is_err() {
            return None;
        }
        let text = read_locked();
        let _ = CloseClipboard();
        text
    }
}

/// 在已打开的剪贴板上读取（调用方负责 CloseClipboard）
unsafe fn read_locked() -> Option<String> {
    let handle = unsafe { GetClipboardData(CF_UNICODETEXT).ok()? };
    if handle.0.is_null() {
        return None;
    }
    let hmem = HGLOBAL(handle.0);
    let ptr = unsafe { GlobalLock(hmem) } as *const u16;
    if ptr.is_null() {
        return None;
    }
    // 必须以 **GlobalSize 声明的实际分配大小** 限定扫描范围：只按「扫描步数上限」挡，
    // 遇到非 NUL 结尾且小于该上限的块就会越界读。现取真实块大小与兜底上限的较小值。
    let size_bytes = unsafe { GlobalSize(hmem) };
    let avail_units = size_bytes / std::mem::size_of::<u16>();
    let cap = avail_units.min(MAX_SCAN_UNITS);

    let mut len = 0usize;
    while len < cap && unsafe { *ptr.add(len) } != 0 {
        len += 1;
    }
    let out = unsafe { std::slice::from_raw_parts(ptr, len) };
    let text = String::from_utf16_lossy(out);
    let _ = unsafe { GlobalUnlock(hmem) };
    Some(text)
}

/// 写入剪贴板纯文本（失败静默）
pub(crate) fn set_text(s: &str) {
    let mut wide: Vec<u16> = s.encode_utf16().collect();
    wide.push(0); // NUL 结尾
    let bytes = wide.len() * std::mem::size_of::<u16>();

    unsafe {
        if OpenClipboard(None).is_err() {
            return;
        }
        let _ = EmptyClipboard();
        if let Ok(hmem) = GlobalAlloc(GMEM_MOVEABLE, bytes) {
            let ptr = GlobalLock(hmem) as *mut u16;
            if !ptr.is_null() {
                std::ptr::copy_nonoverlapping(wide.as_ptr(), ptr, wide.len());
                let _ = GlobalUnlock(hmem);
                // 成功后所有权移交系统；失败则释放，避免泄漏
                if SetClipboardData(CF_UNICODETEXT, HANDLE(hmem.0)).is_err() {
                    let _ = windows::Win32::Foundation::GlobalFree(hmem);
                }
            } else {
                let _ = windows::Win32::Foundation::GlobalFree(hmem);
            }
        }
        let _ = CloseClipboard();
    }
}
