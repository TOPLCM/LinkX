//! 系统文件/文件夹对话框（Shell `IFileOpenDialog`）。
//!
//! 主窗口是自绘的，但"挑一个本机文件/目录"这件事一律交给系统对话框：
//! 用户认得它，地址栏、搜索、键盘操作都是现成的，自己画只会更差。

use windows::Win32::Foundation::{ERROR_CANCELLED, HWND};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CLSCTX_ALL, COINIT_APARTMENTTHREADED,
};
use windows::Win32::UI::Shell::{
    FileOpenDialog, IFileOpenDialog, FOS_ALLOWMULTISELECT, FOS_FORCEFILESYSTEM, FOS_PATHMUSTEXIST,
    FOS_PICKFOLDERS, SIGDN_FILESYSPATH,
};

/// 对话框的三种结局。**"用户取消"和"没弹起来"必须分开**：
/// 合成一个 `Option` 时，COM/Shell 失败会被调用方报成"已取消"，
/// 于是真故障穿上了一件"是你自己不干的"外衣，谁也查不下去。
///
/// 选中的可以是**一个或多个**（`Picked` 是列表）：Windows 端一次要能挑一叠文件发到手机，
/// 单选的系统对话框做不到这件事，用户只能一个一个挑、一���一个发。
pub(crate) enum Pick {
    Picked(Vec<std::path::PathBuf>),
    Cancelled,
    Failed(String),
}

/// 「选择一个或多个文件」（系统对话框带多选，Ctrl / Shift 多选、框选都支持）
pub(crate) fn pick_files(hwnd: HWND, title: &str) -> Pick {
    run_dialog(hwnd, title, false, true)
}

/// 「选择一个文件夹」
pub(crate) fn pick_folder(hwnd: HWND, title: &str) -> Pick {
    run_dialog(hwnd, title, true, false)
}

// 本线程是否已初始化过 COM。`CoInitializeEx` 在**已初始化**的线程上返回 `S_FALSE`，
// 那也算 `is_ok()`，照"is_ok 就配对 Uninitialize"写会把同线程别的模块（WIC、OLE）那层一起减掉
// —— 症状是"弹过一次文件夹对话框，相册缩略图全空"。与 `wic.rs` 同一口径：只初始化、不卸载，
// 因为调用方是活到进程结束的 UI 线程。
thread_local! {
    static COM_OWNED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

fn run_dialog(hwnd: HWND, title: &str, folders: bool, multi: bool) -> Pick {
    let title = windows::core::HSTRING::from(title);
    unsafe {
        if !COM_OWNED.with(|c| c.get()) && CoInitializeEx(None, COINIT_APARTMENTTHREADED).is_ok() {
            COM_OWNED.with(|c| c.set(true));
        }
        try_dialog(hwnd, &title, folders, multi)
    }
}

/// 调用方必须处于已初始化的 COM 套间里（`run_dialog` 负责），故本身标 unsafe。
unsafe fn try_dialog(
    hwnd: HWND,
    title: &windows::core::HSTRING,
    folders: bool,
    multi: bool,
) -> Pick {
    let failed = |what: &str, e: windows::core::Error| match e.code() {
        c if c == ERROR_CANCELLED.to_hresult() => Pick::Cancelled,
        c => Pick::Failed(format!("{what}失败: {e} (0x{:08X})", c.0 as u32)),
    };
    let dialog: IFileOpenDialog = match CoCreateInstance(&FileOpenDialog, None, CLSCTX_ALL) {
        Ok(d) => d,
        Err(e) => return failed("创建系统文件对话框", e),
    };
    let mut opts = match dialog.GetOptions() {
        Ok(o) => o,
        Err(e) => return failed("读取对话框选项", e),
    };
    if folders {
        opts |= FOS_PICKFOLDERS;
    }
    // 多选开关：只对文件对话框有意义（挑文件夹时一次只能一个目录）
    if multi && !folders {
        opts |= FOS_ALLOWMULTISELECT;
    }
    opts |= FOS_FORCEFILESYSTEM | FOS_PATHMUSTEXIST;
    if let Err(e) = dialog.SetOptions(opts) {
        return failed("设置对话框选项", e);
    }
    let _ = dialog.SetTitle(title);
    if let Err(e) = dialog.Show(hwnd) {
        return failed("打开系统文件对话框", e);
    }
    // 走到这里用户已经点了「打开」——此后任何失败都是**丢掉了用户选的文件**，
    // 绝不能退化成"取消"。
    let mut picked: Vec<std::path::PathBuf> = Vec::new();
    if multi && !folders {
        // 多选走 GetResults（IShellItemArray）：一次拿到全部选中项的完整路径
        let arr = match dialog.GetResults() {
            Ok(a) => a,
            Err(e) => return failed("取回所选文件列表", e),
        };
        let count = match arr.GetCount() {
            Ok(c) => c,
            Err(e) => return failed("读取所选文件数量", e),
        };
        for i in 0..count {
            let item = match arr.GetItemAt(i) {
                Ok(it) => it,
                Err(e) => return failed("取回所选文件", e),
            };
            let pw = match item.GetDisplayName(SIGDN_FILESYSPATH) {
                Ok(p) => p,
                Err(e) => return failed("读取所选文件的完整路径", e),
            };
            let text = pw.to_string();
            // `GetDisplayName` 返回的缓冲是 Shell 用 `CoTaskMemAlloc` 给的，`PWSTR` 只是裸指针、
            // 不会自己释放：不补这一句，每次成功挑选都漏下一整条路径字符串
            unsafe { CoTaskMemFree(Some(pw.0 as *const std::ffi::c_void)) };
            match text {
                Ok(s) if !s.trim().is_empty() => picked.push(std::path::PathBuf::from(s)),
                Ok(_) => continue,
                Err(e) => return Pick::Failed(format!("所选路径不是合法文本: {e}")),
            }
        }
        if picked.is_empty() {
            return Pick::Failed("没有取到任何所选文件".to_string());
        }
        return Pick::Picked(picked);
    }
    let item = match dialog.GetResult() {
        Ok(i) => i,
        Err(e) => return failed("取回所选文件", e),
    };
    let pw = match item.GetDisplayName(SIGDN_FILESYSPATH) {
        Ok(p) => p,
        Err(e) => return failed("读取所选文件的完整路径", e),
    };
    let s = pw.to_string();
    unsafe { CoTaskMemFree(Some(pw.0 as *const std::ffi::c_void)) };
    match s {
        Ok(s) if !s.trim().is_empty() => Pick::Picked(vec![std::path::PathBuf::from(s)]),
        Ok(_) => Pick::Failed("所选路径是空的".to_string()),
        Err(e) => Pick::Failed(format!("所选路径不是合法文本: {e}")),
    }
}
