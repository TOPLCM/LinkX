//! LinkX Windows 自绘壳入口：Win32 + GDI 全自绘（无 WPF/.NET/C#，无 Direct2D，无 WebView），
//! 左侧 7 项导航 + 右侧内容区 + 托盘。交叉编译：
//! `cargo build --target x86_64-pc-windows-gnu --manifest-path Platforms/Windows/Cargo.toml`
//! 以 `windows_subsystem = "windows"` 链接 → 双击 exe 不再弹黑色控制台窗口；副作用是 stdout 不复存在，
//! 故所有诊断输出统一走 [`say`]（写 stderr 且忽略失败），绝不用会 panic 的 `println!`。

#![cfg_attr(windows, windows_subsystem = "windows")]

#[cfg(windows)]
mod about;
#[cfg(windows)]
mod app;
#[cfg(windows)]
mod autostart;
#[cfg(windows)]
mod ble_central;
#[cfg(windows)]
mod clipboard;
#[cfg(windows)]
mod debug;
#[cfg(windows)]
mod dialog;
#[cfg(windows)]
mod features;
#[cfg(windows)]
mod icons;
#[rustfmt::skip]
mod icons_svg;
/// 设备身份与信任库持久化。**故意不加 `cfg(windows)`**：信任库解析 / 指纹校验 / 迁移判定等纯逻辑要能被 host 单测覆盖
mod identity;
#[cfg(windows)]
mod ipc;
#[cfg(windows)]
mod network;
#[cfg(windows)]
mod render;
#[cfg(windows)]
mod settings;
#[cfg(windows)]
mod state;
#[cfg(windows)]
mod theme;
#[cfg(windows)]
mod toast;
mod transfer;
#[cfg(windows)]
mod tray;
/// JPEG 编解码（系统 WIC，不引第三方图像库）
#[cfg(windows)]
mod wic;
#[cfg(windows)]
mod window;

#[cfg(windows)]
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, GetMessageW, KillTimer, SetTimer, TranslateMessage, MSG,
};

#[cfg(windows)]
const APP_TITLE: &str = "LinkX";

/// 诊断输出：GUI 子系统下 stderr 句柄可能无效，写入失败被显式忽略，
/// 这样就不会像 `println!` 那样在写入失败时 panic 掉整个程序
#[cfg(windows)]
pub(crate) fn say(msg: impl AsRef<str>) {
    use std::io::Write as _;
    let _ = writeln!(std::io::stderr(), "{}", msg.as_ref());
}

#[cfg(windows)]
fn main() {
    // DPI 感知必须最先声明，否则创建窗口后再设无效（高分屏会整体位图拉伸变糊）
    window::enable_dpi_awareness();

    // 命令行里的文件路径（`linkx.exe D:.pdf`、右键"发送到"、拖到 exe 上）。用 `args_os` 而非 `args`：路径可能不是合法 UTF-8，`args()` 会 panic
    let args: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
    let file_arg = args
        .iter()
        .map(std::path::PathBuf::from)
        .find(|p| p.is_file());
    let uri_arg = args
        .iter()
        .map(|a| a.to_string_lossy().into_owned())
        .find(|a| a.starts_with(toast::SCHEME));
    // 开机自启动拉起的那一次只进托盘、不弹主窗口；参数名与写进注册表的那一份同源于 `autostart`
    let start_hidden = autostart::has_minimized_arg(
        &args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect::<Vec<String>>(),
    );

    // 单实例：已有实例在跑 → 把请求经命名管道转交过去，本进程立即退出
    if !ipc::acquire_single_instance() {
        // 转交成没成，在这条路上本来不留任何痕迹：本进程一秒钟就退，GUI 子系统里 stderr 没人看。
        // 按持久化的 Debug 开关把日志挂上，真机报"点了卡片上的按钮没反应"时才有证可查
        // （只记"哪一类请求、成没成"，绝不记 URI 本身——那里面是一次性口令）。
        if settings::load().debug_enabled {
            if let Some(dir) = identity::debug_log_dir() {
                let _ = debuglog::enable(&dir);
            }
        }
        let note = |kind: &str, ok: bool| {
            debuglog::log!(
                if ok {
                    debuglog::Level::Info
                } else {
                    debuglog::Level::Warn
                },
                "ui",
                "ipc.forward",
                &[("kind", kind), ("ok", if ok { "1" } else { "0" })]
            );
        };
        match (uri_arg, file_arg) {
            // 第二实例只做转交：它没有令牌表，自己执行动作等于绕过了口令
            (Some(uri), _) => {
                let ok = ipc::forward_path(&uri);
                note("card", ok);
                if ok {
                    say("[LinkX] 已把通知卡上的这次点击转交给运行中的实例");
                } else {
                    say("[LinkX] 运行中的实例未响应管道，这次点击作废");
                }
            }
            (None, Some(p)) => {
                let path = p.to_string_lossy().to_string();
                let ok = ipc::forward_path(&path);
                note("file", ok);
                if ok {
                    say(format!("[LinkX] 已把 {path} 转交给运行中的实例"));
                } else {
                    say("[LinkX] 运行中的实例未响应管道，路径未转交");
                }
            }
            // 又双击了一次图标：请运行中的实例把窗口摆回来，它可能正藏在托盘里；`--minimized` 那次不要求显示
            (None, None) => {
                let ok = start_hidden || ipc::request_show();
                note("show", ok);
                if ok {
                    say("[LinkX] 已有实例在运行，本进程退出");
                } else {
                    say("[LinkX] 已有实例在运行，但未能通知它，本进程退出");
                }
            }
        }
        return;
    }

    // 上一轮遗留的拖拽载荷（没拖完就退出、或崩溃留下的全尺寸原图）开起来就清掉：产品口径是"关掉就没有缓存"
    transfer::sweep_album_drag_dir();
    toast::register_identity();

    let hinst = window::get_instance();
    // 视觉走查预览态（LINKX_UI_PREVIEW）：仅渲染真实版式，不接 BLE worker。
    // 状态必须**先于**窗口建好：标题栏要在窗口显示之前按用户主题染好色，晚一步就是先闪一条系统默认的白条
    let preview = state::preview_shared();
    let shared = preview.clone().unwrap_or_else(state::new_shared);
    let theme_pref = shared.lock().unwrap().theme;
    let hwnd =
        match window::create_main_window(hinst, APP_TITLE, 960, 660, theme_pref, !start_hidden) {
            Some(h) => h,
            None => {
                say("[LinkX] 创建主窗口失败");
                return;
            }
        };
    if start_hidden {
        say("[LinkX] 按开机自启动方式拉起：主窗口不显示，托盘图标照常可用");
    }

    {
        let mut st = shared.lock().unwrap();
        st.hwnd_raw = hwnd.0 as isize;
    }
    window::install_state(shared.clone());

    // 首实例自带路径（例如双击"发送到 → LinkX"）：填入输入框并请求发送，worker 会在配对 + TCP 就绪时启动传输
    if let Some(p) = file_arg {
        let path = p.to_string_lossy().to_string();
        let mut st = shared.lock().unwrap();
        st.send_path_input = path.clone();
        st.send_file_req = true;
        say(format!("[LinkX] 启动参数文件已排队: {path}"));
    }
    // 点按钮时 LinkX 已经退出过：这一份全新状态里没有那张卡的令牌，如实按"过期"报给用户
    if let Some(uri) = uri_arg {
        toast::handle_activation(&shared, &uri);
    }

    tray::add_tray_icon(hwnd);
    // 33ms 定时器：驱动动效（悬停/过渡/呼吸脉冲）与弹窗派发；静止时不产生重绘
    unsafe {
        let _ = SetTimer(hwnd, window::TIMER_ID, window::TIMER_MS, None);
    }
    // 关闭"剪贴板同步"时**不注册** Win32 剪贴板监听，系统每次剪贴板变化就不会再把消息打进我们的窗口过程
    if features::enabled(features::Module::Clipboard) {
        clipboard::register(hwnd);
    } else {
        say("[LinkX] 剪贴板同步已关闭：未注册剪贴板监听");
    }
    if preview.is_none() {
        app::spawn_worker(hwnd, shared.clone());
        // 命名管道是**壳层**的单实例通道：第二次双击图标要靠它把窗口叫回来，这份职责不属于
        // 文件互传模块，所以不随模块关停下。模块关掉时挡的是文件转交本身（见 ipc::deliver）。
        ipc::spawn_pipe_server(shared.clone());
    } else {
        say("[LinkX] UI 预览模式（LINKX_UI_PREVIEW）：未启动 BLE worker");
    }

    say(format!(
        "[LinkX] shell up. core version: {}",
        linkx_core::LINKX_FFI_VERSION
    ));

    let mut msg: MSG = unsafe { std::mem::zeroed() };
    loop {
        let ret = unsafe { GetMessageW(&mut msg, None, 0, 0) }; // BOOL: 0=WM_QUIT <0=错误 >0=正常
        if ret.0 > 0 {
            unsafe {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        } else {
            break;
        }
    }
    unsafe {
        let _ = KillTimer(hwnd, window::TIMER_ID);
        tray::remove_tray_icon();
        window::release_back_buffer();
        window::destroy_main_window(hwnd);
    }
    say("[LinkX] shell teardown");
    // 退出也要扫一次：只靠启动清扫的话，最后一次会话留下的那一两张全尺寸原图（视频可达 221 MB/张）会一直躺在 %TEMP%\LinkX 里
    transfer::sweep_album_drag_dir();

    // 功能开关改动后的自重启放在收尾**之后**：让新实例看到一个已经空出来的位置（托盘已摘、单实例互斥体在 `restart_self` 里释放）
    let want_restart = shared.lock().map(|s| s.restart_req).unwrap_or(false);
    if want_restart {
        say("[LinkX] 按功能开关改动重新启动");
        if let Err(e) = ipc::restart_self() {
            // 起不来就如实说：改动已经落盘，用户手动再开一次即可生效
            say(format!("[LinkX] 重新启动失败: {e}（请手动重新打开 LinkX）"));
        }
    }
}

#[cfg(not(windows))]
fn main() {
    eprintln!("linkx-windows 仅支持 Windows 目标，请用 --target x86_64-pc-windows-gnu 构建");
}
