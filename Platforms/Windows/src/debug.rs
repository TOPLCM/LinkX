//! Debug 模式：UI 交互与系统对话框归平台壳，核心层日志埋点由 `debuglog` 落盘。
//!
//! 开关是**进程级全局**效果（全栈全模块，不做模块白名单）；日志导出走系统「选择文件夹」
//! 对话框（日志不做脱敏，但导出前后有风险提示与结果通知）。

use windows::Win32::Foundation::HWND;

use crate::window::shared_state;

/// 按设置项开关全局 Debug sink。开关是**进程级全局**效果：开启后核心层所有埋点立即落盘；
/// 关闭后埋点先判原子标志直接返回。
pub(crate) fn apply_debug_toggle(on: bool) {
    if on {
        if let Some(dir) = crate::identity::debug_log_dir() {
            let _ = std::fs::create_dir_all(&dir);
            let _ = debuglog::enable(&dir);
        }
    } else {
        debuglog::disable();
    }
}

/// 导出 Debug 日志到用户选定目录（系统「选择文件夹」；取消则静默返回）。日志**不做脱敏**
/// （用户知情接受）——导出前有风险提示，结果以气泡通知给出目标路径，绝不静默写到用户不知道的位置。
pub(crate) fn run_debug_export(hwnd: HWND) {
    let Some(arc) = shared_state() else {
        return;
    };
    let target = match crate::dialog::pick_folder(hwnd, "选择 LinkX Debug 日志导出目录") {
        crate::dialog::Pick::Picked(dir) => dir,
        crate::dialog::Pick::Cancelled => {
            arc.lock()
                .unwrap()
                .push_error("已取消 Debug 日志导出（未选择目录）".to_string());
            return;
        }
        // 对话框没弹起来是真故障，说成"已取消"会把排查一路带偏
        crate::dialog::Pick::Failed(why) => {
            arc.lock()
                .unwrap()
                .push_error(format!("系统文件夹对话框不可用：{why}"));
            return;
        }
    };
    let msg = match debuglog::export_to(&target) {
        Ok(path) => format!("Debug 日志已导出到 {}", path.display()),
        Err(e) => format!("Debug 日志导出失败: {e}"),
    };
    let mut st = arc.lock().unwrap();
    st.push_error(msg.clone());
    st.push_toast("LinkX · Debug".to_string(), msg);
}
