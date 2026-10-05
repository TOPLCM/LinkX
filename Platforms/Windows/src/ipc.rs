//! 单实例判定 + `linkx.exe <文件路径>` 转交（命名管道）：把 LinkX 挂进「发送到」右键菜单（或把文件
//! 拖到 exe 上）后，**路径落到已经在运行的实例**，而不是再起一个窗口（多实例会重复占用 BLE / TCP / 托盘）。
//! 单实例用 `CreateMutexW("Global\LinkX.SingleInstance")`，`ERROR_ALREADY_EXISTS` 即已有实例；转交是后启动的进程
//! 把 UTF-8 路径写进 `\.\pipe\linkx_ipc` 后立即退出，运行中的实例读出来填进文件页的发送框——
//! **不代发**，点「发送」的必须是用户（见 `deliver`）。
//! **不做** MSI 层面的「发送到」快捷方式创建（安装器脚本更合适）；本模块只保证命令行为正确转交。

use std::ffi::c_void;
use std::io::{Read, Write};
use std::os::windows::io::{AsRawHandle, FromRawHandle};
use std::os::windows::process::CommandExt;
use std::sync::atomic::{AtomicIsize, Ordering};
use std::time::Duration;

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{
    CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, ERROR_PIPE_CONNECTED, HANDLE,
};
use windows::Win32::Storage::FileSystem::PIPE_ACCESS_INBOUND;
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, NAMED_PIPE_MODE, PIPE_READMODE_BYTE,
    PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_WAIT,
};
use windows::Win32::System::Threading::CreateMutexW;

use crate::state::{SharedState, FOCUS_SEND_PATH};
use crate::window::post_state_changed;

/// 单实例互斥体名（`Global\` = 跨会话；本产品是单用户桌面应用，够用且避免多窗口抢端口）
const MUTEX_NAME: PCWSTR = w!("Global\\LinkX.SingleInstance");
/// IPC 命名管道路径（**不放机密**：只传本机文件路径，且只接受本机连接）
const PIPE_PATH: &str = r"\\.\pipe\linkx_ipc";
const PIPE_BUFFER: u32 = 4096;
/// 读间隔超时（毫秒）：见 `pipe_loop` 里 `CreateNamedPipeW` 的那一处注释
const READ_INTERVAL_MS: u32 = 1_000;
/// 本进程持有的单实例互斥体句柄原始值（0 = 未持有）。用 `AtomicIsize` 而不是 `static mut HANDLE`：句柄只写一次、重启路径取一次
static MUTEX_HANDLE: AtomicIsize = AtomicIsize::new(0);
const CONNECT_TRIES: usize = 12;
const CONNECT_RETRY: Duration = Duration::from_millis(120);

/// 取得单实例互斥体；`true` = **本进程是唯一实例**。句柄保存在 [`MUTEX_HANDLE`]：正常退出由进程销毁自动释放，
/// 但**自重启**必须在拉起新实例前先 [`release_single_instance`]，否则新进程看到 `ERROR_ALREADY_EXISTS` 会把自己当第二实例直接退出
pub(crate) fn acquire_single_instance() -> bool {
    unsafe {
        let handle = CreateMutexW(None, false, MUTEX_NAME);
        let err = GetLastError();
        match handle {
            Ok(h) => {
                let existed = err == ERROR_ALREADY_EXISTS;
                if !existed {
                    MUTEX_HANDLE.store(h.0 as isize, Ordering::Release);
                }
                !existed
            }
            Err(e) => {
                crate::say(format!("[LinkX] 单实例互斥体创建失败（继续启动）: {e}"));
                true
            }
        }
    }
}

fn release_single_instance() {
    let raw = MUTEX_HANDLE.swap(0, Ordering::AcqRel);
    if raw == 0 {
        return;
    }
    unsafe {
        let _ = CloseHandle(HANDLE(raw as *mut c_void));
    }
}

/// 原地重启本程序：先放掉单实例互斥体，再拉起一个分离的新实例。调用点必须在 `main` 的收尾之后（托盘已摘、窗口已销毁），
/// 新实例看到的是一个已经完全空出来的位置；返回 `false` 表示拉不起来，此时程序已经要退出，调用方只能把原因写进日志
pub(crate) fn restart_self() -> std::io::Result<()> {
    release_single_instance();
    let exe = std::env::current_exe()?;
    // 不传任何参数：`linkx.exe <文件>` 的语义是"把这个文件交给运行中的实例"，重启不该把上一次的路径再发一遍。
    // DETACHED_PROCESS / CREATE_NEW_PROCESS_GROUP 只是不继承控制台与信号组，**两者都不脱离作业对象**（那需要
    // CREATE_BREAKAWAY_FROM_JOB，作业没开 BREAKAWAY_OK 时 CreateProcess 直接失败）→ 从"退出即杀子进程"的作业里拉起仍会被带走，
    // 真机验证一律用 `powershell Start-Process` 起 LinkX，正是绕开这一点
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    std::process::Command::new(exe)
        .creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP)
        .spawn()?;
    Ok(())
}

/// 把文件路径转交给已在运行的实例（成功返回 `true`）
pub(crate) fn forward_path(path: &str) -> bool {
    for _ in 0..CONNECT_TRIES {
        match std::fs::OpenOptions::new().write(true).open(PIPE_PATH) {
            Ok(mut f) => return f.write_all(path.as_bytes()).is_ok(),
            Err(_) => std::thread::sleep(CONNECT_RETRY),
        }
    }
    false
}

pub(crate) fn spawn_pipe_server(state: SharedState) {
    std::thread::spawn(move || pipe_loop(state));
}

/// 创建管道并循环接收路径；每次连接处理完即 `DisconnectNamedPipe` 等待下一次
fn pipe_loop(state: SharedState) {
    let name: Vec<u16> = PIPE_PATH.encode_utf16().chain(std::iter::once(0)).collect();
    let mode = NAMED_PIPE_MODE(
        PIPE_TYPE_BYTE.0 | PIPE_READMODE_BYTE.0 | PIPE_WAIT.0 | PIPE_REJECT_REMOTE_CLIENTS.0,
    );
    let handle = unsafe {
        CreateNamedPipeW(
            PCWSTR(name.as_ptr()),
            PIPE_ACCESS_INBOUND,
            mode,
            1,
            PIPE_BUFFER,
            PIPE_BUFFER,
            // 读间隔超时：默认 0 = 一直等。整条管道只有这一条线程，客户端连上却不写也不关
            // （或写完不关句柄）就会把它永久卡住，之后所有"右键发送到 LinkX"静默失效。
            // `take(4096)` 只挡内存，挡不住阻塞。
            READ_INTERVAL_MS,
            None,
        )
    };
    if handle.is_invalid() {
        crate::say("[LinkX] 命名管道创建失败：linkx.exe <路径> 的转交不可用");
        return;
    }
    let mut file = unsafe { std::fs::File::from_raw_handle(handle.0) };
    loop {
        let h = HANDLE(file.as_raw_handle());
        // 客户端可能在本进程 ConnectNamedPipe 之前就连上了 → ERROR_PIPE_CONNECTED 也算成功
        let connected = unsafe { ConnectNamedPipe(h, None) }.is_ok()
            || unsafe { GetLastError() } == ERROR_PIPE_CONNECTED;
        if !connected {
            std::thread::sleep(Duration::from_millis(200));
            continue;
        }
        // 读满 4 KiB 就收：`read_to_end` 在对端不关闭时会一直涨，而这个循环只有一条线程，卡住之后所有"右键发送到 LinkX"都会静默失效
        const MAX_PATH_BYTES: u64 = 4096;
        let mut buf = Vec::new();
        let read = std::io::Read::by_ref(&mut file)
            .take(MAX_PATH_BYTES)
            .read_to_end(&mut buf);
        let _ = unsafe { DisconnectNamedPipe(h) };
        // 读间隔超时也算 Err，但已经收到的字节仍是有效载荷：整条丢掉会把"客户端写完没关句柄"
        // 这种常见形态变成静默失效。只有一个字都没收到才作废。
        if let Err(e) = read {
            if buf.is_empty() {
                crate::say(format!("[LinkX] 管道读取失败，本次转交作废: {e}"));
                continue;
            }
            crate::say(format!(
                "[LinkX] 管道读取提前结束（{e}），按已收到的内容继续"
            ));
        }
        let path = String::from_utf8_lossy(&buf).trim().to_string();
        if path.is_empty() {
            continue;
        }
        deliver(&state, &path);
    }
}

/// 把转交来的路径填进文件页的发送框（**不代发**，与拖进窗口、「选择…」同一条口径）
fn deliver(state: &SharedState, path: &str) {
    // 这是"本机任意进程 → 让 LinkX 把一个文件发给对端"的入口，所以三条底线：路径必须真的是个
    // 文件、只填输入框不代发（点「发送」的必须是用户）、填完要跳到文件页让人看得见。
    // 少了第二条，任何本机进程都能借已配对的 LinkX 静默外发它能读到的任何文件。
    if !std::fs::metadata(path).is_ok_and(|m| m.is_file()) {
        let mut st = state.lock().unwrap();
        st.push_error(format!(
            "收到一个外部发送请求，但 {path} 不是本机可读的普通文件，已忽略"
        ));
        st.ui_rev += 1;
        crate::say(format!("[LinkX] 转交路径不可用，已拒绝: {path}"));
        return;
    }
    let hwnd_raw = {
        let mut st = state.lock().unwrap();
        st.send_path_input = path.to_string();
        st.active_tab = crate::render::TAB_FILES;
        st.input_focus = FOCUS_SEND_PATH;
        st.ui_rev += 1;
        st.hwnd_raw
    };
    crate::say(format!("[LinkX] 已把 {path} 填进发送框，按「发送」确认"));
    post_state_changed(hwnd_raw);
}
