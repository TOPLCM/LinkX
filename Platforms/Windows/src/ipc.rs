//! 单实例判定 + `linkx.exe <文件路径>` 转交（命名管道）：把文件交给**已经在运行的实例**，
//! 而不是再起一个窗口。转交只填发送框、**不代发**——点「发送」的必须是用户（见 `deliver`）。

use std::ffi::c_void;
use std::io::Write;
use std::os::windows::io::{AsRawHandle, FromRawHandle};
use std::os::windows::process::CommandExt;
use std::sync::atomic::{AtomicIsize, Ordering};
use std::time::{Duration, Instant};

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{
    CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, ERROR_PIPE_CONNECTED, HANDLE,
};
use windows::Win32::Storage::FileSystem::PIPE_ACCESS_INBOUND;
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, PeekNamedPipe, NAMED_PIPE_MODE,
    PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_WAIT,
};
use windows::Win32::System::Threading::CreateMutexW;

use crate::state::{SharedState, FOCUS_SEND_PATH};
use crate::window::post_state_changed;

/// 单实例互斥体名（`Global\` = 跨会话；本产品是单用户桌面应用，够用且避免多窗口抢端口）
const MUTEX_NAME: PCWSTR = w!("Global\\LinkX.SingleInstance");
/// IPC 命名管道路径（**不放机密**：只传本机文件路径、把窗口叫回来的哨兵，以及通知卡按钮的
/// 一次性口令；管道默认 DACL 只允许本机同一用户连接）
const PIPE_PATH: &str = r"\\.\pipe\linkx_ipc";
const PIPE_BUFFER: u32 = 4096;
/// 一次连接最多等多久才认定"这个客户端不写了"。**必须待在客户端连接重试的预算之内**（≈1.4 s）
const PIPE_IDLE_WAIT: Duration = Duration::from_millis(1_200);
/// 有数据时的轮询间隔（正常客户端写完即关，用不到等满超时）
const PIPE_POLL: Duration = Duration::from_millis(30);
/// 本进程持有的单实例互斥体句柄原始值（0 = 未持有）。用 `AtomicIsize` 而不是 `static mut HANDLE`：句柄只写一次、重启路径取一次
static MUTEX_HANDLE: AtomicIsize = AtomicIsize::new(0);
const CONNECT_TRIES: usize = 12;
const CONNECT_RETRY: Duration = Duration::from_millis(120);
/// 「把窗口摆回来」的哨兵载荷：带 NUL，任何真实文件路径都不会等于它
const SHOW_TOKEN: &str = "\0show";

/// 取得单实例互斥体；`true` = **本进程是唯一实例**。句柄保存在 [`MUTEX_HANDLE`]：正常退出由进程销毁自动释放，
/// 但**自重启**必须在拉起新实例前先 [`release_single_instance`]，否则新进程看到 `ERROR_ALREADY_EXISTS` 会把自己当第二实例直接退出
pub(crate) fn acquire_single_instance() -> bool {
    unsafe {
        let handle = CreateMutexW(None, false, MUTEX_NAME);
        let err = GetLastError();
        match handle {
            Ok(h) => {
                let existed = err == ERROR_ALREADY_EXISTS;
                if existed {
                    // 第二实例只是"看一眼有没有人占着"，手上这个句柄不当值留着
                    let _ = CloseHandle(h);
                } else {
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
    // 这里只脱离控制台与信号组，**不脱离作业对象**（那要 CREATE_BREAKAWAY_FROM_JOB）→ 从"退出即杀子进程"的
    // 作业里拉起仍会被带走，所以真机验证一律用 `powershell Start-Process` 起 LinkX
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    std::process::Command::new(exe)
        .creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP)
        .spawn()?;
    Ok(())
}

pub(crate) fn forward_path(path: &str) -> bool {
    for _ in 0..CONNECT_TRIES {
        match std::fs::OpenOptions::new().write(true).open(PIPE_PATH) {
            Ok(mut f) => return f.write_all(path.as_bytes()).is_ok(),
            Err(_) => std::thread::sleep(CONNECT_RETRY),
        }
    }
    false
}

/// 第二实例（用户又双击了一次图标）请运行中的实例显示主窗口
pub(crate) fn request_show() -> bool {
    forward_path(SHOW_TOKEN)
}

pub(crate) fn spawn_pipe_server(state: SharedState) {
    std::thread::spawn(move || pipe_loop(state));
}

/// 读一次管道连接，带空闲超时。`CreateNamedPipeW` 的第 7 参只是给客户端 `WaitNamedPipe`
/// 用的默认等待值，对服务端的读没有任何超时作用 —— 一个"连上、不写、也不关"的客户端能把
/// 这条唯一的管道线程永久卡死，之后所有「右键发送到 LinkX」都静默失效。
/// 做法：只用 `PeekNamedPipe` 问"缓冲区里有几字节"，有才读（这时读不阻塞）；对端关闭时
/// Peek 直接失败，所以正常路径当场就结束，用不到等满空闲超时。
fn read_pipe_bounded(file: &mut std::fs::File, h: HANDLE) -> Vec<u8> {
    const MAX_PATH_BYTES: usize = 4096;
    let mut buf: Vec<u8> = Vec::new();
    let mut last_byte_at = Instant::now();
    loop {
        let mut avail = 0u32;
        if unsafe { PeekNamedPipe(h, None, 0, None, Some(&mut avail), None) }.is_err() {
            break; // 对端断开或管道已关：手上这些就是全部
        }
        if avail == 0 {
            if buf.is_empty() {
                // 一个字都没来：等到空闲超时，超时后外层直接作废这次连接
                if last_byte_at.elapsed() >= PIPE_IDLE_WAIT {
                    break;
                }
                std::thread::sleep(PIPE_POLL);
                continue;
            }
            break; // 排空了：路径是一次性写进来的，当作写完
        }
        let want = (avail as usize).min(MAX_PATH_BYTES - buf.len());
        if want == 0 {
            break; // 到上限：剩下的不无限涨
        }
        let mut chunk = vec![0u8; want];
        match std::io::Read::read(file, &mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                last_byte_at = Instant::now();
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
    buf
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
            // 读超时管不到这里，见 read_pipe_bounded
            0,
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
        let buf = read_pipe_bounded(&mut file, h);
        let _ = unsafe { DisconnectNamedPipe(h) };
        if buf.is_empty() {
            crate::say("[LinkX] 命名管道连接没有送到内容，本次转交作废");
            continue;
        }
        let path = String::from_utf8_lossy(&buf).trim().to_string();
        if path.is_empty() {
            continue;
        }
        if path == SHOW_TOKEN {
            let hwnd_raw = {
                let st = state.lock().unwrap();
                st.hwnd_raw
            };
            crate::window::request_show_from_raw(hwnd_raw);
            continue;
        }
        // 通知卡上的按钮：口令校验与执行全在 `toast`，这里只认前缀
        if path.starts_with(crate::toast::SCHEME) {
            crate::toast::handle_activation(&state, &path);
            continue;
        }
        deliver(&state, &path);
    }
}

/// 把转交来的路径填进文件页的发送框（**不代发**，与拖进窗口、「选择…」同一条口径）
fn deliver(state: &SharedState, path: &str) {
    // 文件互传关了就不接转交：管道本身留着是给"再双击一次图标把窗口叫回来"用的（壳层职责）
    if !crate::features::enabled(crate::features::Module::FileTransfer) {
        {
            let mut st = state.lock().unwrap();
            st.push_error(format!("文件互传已关闭，{path} 的转交没有接收"));
            st.ui_rev += 1;
        }
        // `say` 是 stderr 阻塞写，不能占着界面锁做： paint 线程要拿同一把锁
        crate::say("[LinkX] 文件互传已关闭，本次转交未接收");
        return;
    }
    // 这是"本机任意进程 → 让 LinkX 把一个文件发给对端"的入口，两条底线：路径必须真的是个文件、
    // 只填输入框不代发。少了第二条，任何本机进程都能借已配对的 LinkX 静默外发它读到的任何文件。
    if !std::fs::metadata(path).is_ok_and(|m| m.is_file()) {
        {
            let mut st = state.lock().unwrap();
            st.push_error(format!(
                "收到一个外部发送请求，但 {path} 不是本机可读的普通文件，已忽略"
            ));
            st.ui_rev += 1;
        }
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
    // 实例可能藏在托盘里：只填输入框的话，用户看到的是"右键发送到 LinkX 之后什么都没发生"
    crate::window::request_show_from_raw(hwnd_raw);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU32;

    /// 真建一对管道跑一遍 `read_pipe_bounded`：这条线程被"连上不写"的客户端卡死过一次，
    /// 只能靠真实句柄验，假输入模拟不出 Win32 的阻塞语义。
    fn round_trip(client: impl FnOnce(std::fs::File) + Send + 'static) -> (Vec<u8>, Duration) {
        static SEQ: AtomicU32 = AtomicU32::new(0);
        let path = format!(
            "\\\\.\\pipe\\linkx_ipc_test_{}_{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        );
        let wide: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
        let mode = NAMED_PIPE_MODE(
            PIPE_TYPE_BYTE.0 | PIPE_READMODE_BYTE.0 | PIPE_WAIT.0 | PIPE_REJECT_REMOTE_CLIENTS.0,
        );
        let handle = unsafe {
            CreateNamedPipeW(
                PCWSTR(wide.as_ptr()),
                PIPE_ACCESS_INBOUND,
                mode,
                1,
                PIPE_BUFFER,
                PIPE_BUFFER,
                0,
                None,
            )
        };
        assert!(!handle.is_invalid(), "建测试管道失败");
        let mut server = unsafe { std::fs::File::from_raw_handle(handle.0) };
        let writer = std::thread::spawn(move || {
            let c = match std::fs::OpenOptions::new().write(true).open(&path) {
                Ok(c) => c,
                Err(e) => panic!("客户端连不上自己的测试管道: {e}"),
            };
            client(c);
        });
        let h = HANDLE(server.as_raw_handle());
        let connected = unsafe { ConnectNamedPipe(h, None) }.is_ok()
            || unsafe { GetLastError() } == ERROR_PIPE_CONNECTED;
        assert!(connected, "测试管道握手失败");
        let t0 = Instant::now();
        let buf = read_pipe_bounded(&mut server, h);
        let elapsed = t0.elapsed();
        let _ = unsafe { DisconnectNamedPipe(h) };
        let _ = writer.join();
        (buf, elapsed)
    }

    const PAYLOAD: &[u8] = b"E:\\Dir\\report.pdf";

    #[test]
    fn pipe_read_returns_the_payload_without_waiting_for_the_timeout() {
        let (buf, elapsed) = round_trip(|mut c| {
            c.write_all(PAYLOAD).expect("写测试载荷");
            drop(c); // 写完即关：正常「右键发送到 LinkX」就是这个形状
        });
        assert_eq!(buf, PAYLOAD, "写完即关的客户端要当场读到");
        assert!(
            elapsed < PIPE_IDLE_WAIT / 2,
            "正常转交不该等满空闲超时：{elapsed:?}"
        );
    }

    #[test]
    fn client_that_never_writes_cannot_wedge_the_pipe_thread() {
        let (buf, elapsed) = round_trip(|c| {
            std::thread::sleep(PIPE_IDLE_WAIT * 3); // 连上、不写、也不关：drop 只发生在超时之后
            drop(c);
        });
        assert!(buf.is_empty(), "没收到内容就不该编造载荷");
        assert!(
            elapsed >= PIPE_IDLE_WAIT && elapsed < PIPE_IDLE_WAIT * 2,
            "空闲超时没生效（旧写法在这里会永久卡住）：{elapsed:?}"
        );
    }

    #[test]
    fn pipe_payload_is_capped() {
        let (buf, _) = round_trip(|mut c| {
            let _ = c.write_all(&[b'A'; 5000]); // 超上限：多出来的部分不能无限涨
            drop(c);
        });
        assert_eq!(buf.len(), 4096, "载荷上限 4 KiB 要硬生效");
    }

    /// 客户端一次写超上限时，余下的字节**不许**出现在下一条连接里。
    /// `pipe_loop` 复用同一个管道实例连续服务，靠的就是 `DisconnectNamedPipe` 把没读走的
    /// 部分丢掉；哪天换成多实例或 `TransactNamedPipe`，这条会先红。
    /// 本机任意进程一次写 5000 字节就能污染此后每一条转交，所以值得钉住。
    #[test]
    fn overflow_from_one_client_does_not_leak_into_the_next_connection() {
        static SEQ: AtomicU32 = AtomicU32::new(0);
        let path = format!(
            "\\\\.\\pipe\\linkx_ipc_overflow_{}_{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        );
        let wide: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
        let mode = NAMED_PIPE_MODE(
            PIPE_TYPE_BYTE.0 | PIPE_READMODE_BYTE.0 | PIPE_WAIT.0 | PIPE_REJECT_REMOTE_CLIENTS.0,
        );
        let handle = unsafe {
            CreateNamedPipeW(
                PCWSTR(wide.as_ptr()),
                PIPE_ACCESS_INBOUND,
                mode,
                1,
                PIPE_BUFFER,
                PIPE_BUFFER,
                0,
                None,
            )
        };
        assert!(!handle.is_invalid(), "建测试管道失败");
        let mut server = unsafe { std::fs::File::from_raw_handle(handle.0) };
        let h = HANDLE(server.as_raw_handle());

        // 第一条连接：一次写 5000 字节，服务端按上限只取 4096
        let first_client = {
            let p = path.clone();
            std::thread::spawn(move || {
                let mut c = open_client(&p);
                let _ = c.write_all(&[b'A'; 5000]);
                drop(c);
            })
        };
        assert!(connect(&mut server, h), "第一条连接握手失败");
        let first = read_pipe_bounded(&mut server, h);
        let _ = unsafe { DisconnectNamedPipe(h) };
        let _ = first_client.join();
        assert_eq!(first.len(), 4096, "第一条只该取到上限");

        // 第二条连接：一条正常的转交，必须只看见自己那 17 字节
        let second_client = {
            let p = path.clone();
            std::thread::spawn(move || {
                let mut c = open_client(&p);
                let _ = c.write_all(REAL_PATH);
                drop(c);
            })
        };
        assert!(connect(&mut server, h), "第二条连接握手失败");
        let second = read_pipe_bounded(&mut server, h);
        let _ = unsafe { DisconnectNamedPipe(h) };
        let _ = second_client.join();
        assert_eq!(second, REAL_PATH, "上一条连接剩下的 904 字节串到了这一条");
    }

    const REAL_PATH: &[u8] = b"E:\\real\\path.pdf";

    fn open_client(path: &str) -> std::fs::File {
        std::fs::OpenOptions::new()
            .write(true)
            .open(path)
            .expect("客户端连不上自己的测试管道")
    }

    /// 与 `pipe_loop` 同一条握手口径：客户端可能先连上，那时 `ConnectNamedPipe` 报
    /// `ERROR_PIPE_CONNECTED` 也算成功
    fn connect(_file: &mut std::fs::File, h: HANDLE) -> bool {
        unsafe { ConnectNamedPipe(h, None) }.is_ok()
            || unsafe { GetLastError() } == ERROR_PIPE_CONNECTED
    }
}
