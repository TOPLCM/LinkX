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

/// 导出 Debug 日志到用户选定目录（系统「选择文件夹」；取消则**静默返回**）。日志**不做脱敏**
/// （用户知情接受）——导出前有风险提示，结果以气泡通知给出目标路径，绝不静默写到用户不知道的位置。
///
/// 通道分工：**成功只走气泡通知，不进 `errors`**。`errors` 是失败通道（且只在连接页渲染、
/// 还会被控制面导成 `/state` 的 `errors` 数组），把成功塞进去会让用户在自己没发起操作的
/// 页面上看到一行红色失败，自动化脚本也会把成功读成失败。
pub(crate) fn run_debug_export(hwnd: HWND) {
    let Some(arc) = shared_state() else {
        return;
    };
    let target = match crate::dialog::pick_folder(hwnd, "选择 LinkX Debug 日志导出目录") {
        // 选文件夹只会给回一个；多出来的那条（正常不会发生）取第一个即可
        crate::dialog::Pick::Picked(mut dirs) => match dirs.drain(..).next() {
            Some(dir) => dir,
            None => {
                arc.lock()
                    .unwrap()
                    .push_error("系统文件夹对话框没有返回目录".to_string());
                return;
            }
        },
        // 取消是**用户自己放弃**，不是故障：什么都不报（既不进 errors 也不弹气泡）
        crate::dialog::Pick::Cancelled => return,
        // 对话框没弹起来是真故障，说成"已取消"会把排查一路带偏
        crate::dialog::Pick::Failed(why) => {
            arc.lock()
                .unwrap()
                .push_error(format!("系统文件夹对话框不可用：{why}"));
            return;
        }
    };
    // 导出是磁盘 IO，**必须在锁外**做：持着状态锁做慢 IO 会把 worker 每 200ms 的锁请求
    // 一起卡住（心跳停摆会被看门狗当成"程序卡死"）
    let exported = debuglog::export_to(&target);
    let mut st = arc.lock().unwrap();
    match exported {
        Ok(path) => {
            // 成功 = 气泡通知，不进错误通道
            st.push_toast(
                "LinkX · Debug".to_string(),
                format!("Debug 日志已导出到 {}", path.display()),
            );
        }
        Err(e) => {
            // 失败才进错误通道（连接页会显示这行红色文字）
            st.push_error(format!("Debug 日志导出失败: {e}"));
        }
    }
}
