//! BLE worker 线程 + 会话编排（文件传输 · TCP 通道 · 局域网发现）
//!
//! 线程模型（全部经 `Arc<Mutex<..>>` 转交，无跨线程回调直连 UI）：
//! - **UI 线程**：WndProc 读 `UiState` 渲染，写命令字段；
//! - **WinRT 回调线程**：`start_scan` / `subscribe_notify` 只把结果 push 进锁保护的容器；
//! - **本 worker 线程**：唯一的 BLE/引擎驱动者，200ms 一轮，把上行事件落到 `UiState`
//!   并用 `PostMessageW(hwnd, WM_APP_STATE_CHANGED)` 通知 UI 重绘；
//! - **TCP accept + 读线程**：Windows 为 TCP 服务端（`0.0.0.0:55676`），accept 一条连接后在
//!   **同一线程内**阻塞读帧 → 队列 → 本 worker 喂给引擎（`feed_tcp`）。
//!
//! 引擎只管协议与校验，分块 / CRC32 / SHA-256 是纯逻辑，**文件 IO 与 socket IO 全部在本壳完成**。
//! `panic = "abort"`，故网络输入一律走 `Result` + 可读错误上报，绝不 `unwrap` 网络数据、绝不 `panic`。

use std::collections::VecDeque;
use std::fs;
use std::io::{Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use windows::Win32::Foundation::HWND;

use linkx_lan::{parse_manual_addr, DiscoveryBeacon, UdpDiscovery, DISCOVERY_UDP_PORT};
use linkx_protocol::pb::{FileMeta, NotificationReply};
use linkx_session::binding::BindRole;
use linkx_session::engine::{
    state_code, ConfigEntryItem, EngineConfig, EngineEvent, EngineRole, SessionEngine, TrustedPeer,
};
use linkx_transfer::filename::sanitize_file_name;
use linkx_transfer::{chunks_total, crc32, FileHasher, SendTask, CHUNK_SIZE};

use crate::ble_central::BleCentral;
use crate::clipboard;
use crate::identity;
use crate::network::TcpService;
use crate::settings;
use crate::state::{
    IdentityChangeView, NotificationItem, SharedState, UiState, TASK_DIR_RECV, TASK_DIR_SEND,
};
use crate::transfer::{
    file_base_name, new_file_id, open_chunker, percent_of, unique_path, RecvSession, SendSession,
};
use crate::window::post_state_changed;

/// 本端展示名（HELLO / UDP 信标）
const LOCAL_NAME: &str = "Windows-PC";
const POLL_INTERVAL: Duration = Duration::from_millis(200);
/// 引擎 tick 周期（心跳/超时）
const TICK_INTERVAL: Duration = Duration::from_secs(1);
/// 每轮最多发送的分块数——**吞吐上限的来源**。
/// 按 8 给只有 ~10 MB/s（千兆局域网下明显慢于链路能力）；按 32 给 ≈40 MB/s，仍然有界：
/// 一轮写不完就下一轮，不会把整条链路堵死（socket 写是同步的，背压由内核缓冲自然形成）。
const SEND_CHUNKS_PER_TICK: usize = 32;
/// 每轮最多写出的 BLE 分片数（次数上限，与链路 MTU 无关：即便对端只支持 23，每 200 ms
/// 也能移出 112B 净流，够身份/心跳/剪贴板用；文件类大载荷走 TCP）。次数挡不住阻塞，
/// 还要配下面的时长上限——真机曾测到单轮 `flush_ble_out` 跑到 30.7 s，把收包与心跳全饿死。
const BLE_WRITE_BUDGET_PER_ROUND: usize = 8;
/// 单轮 `flush_ble_out` 的**时长**上限。写是异步发起、下一轮轮询的，但链路与 OS 调用
/// 仍可能慢；150 ms 小于 POLL_INTERVAL 量级，保证死链路下 worker 也按节奏收包、发心跳。
const BLE_WRITE_TIME_BUDGET: Duration = Duration::from_millis(150);
/// 单个接收任务的最大续传请求次数（防对端坏块死循环）：数的是整任务的轮次，与 transfer 侧数块的 `MAX_CRC_RETRIES` 是两回事
const MAX_RESUME_TRIES: u32 = 8;
/// 引擎侧入站分块积压的字节上限。超过就**本轮不再从读队列取帧**（见 `pump_tcp_rx`），让背压顺着
/// 读队列 → 读线程 → socket 缓冲区回到对端。必须有它：TCP 是可靠字节流，从可靠流里丢一帧不会触发
/// 重传，只会在文件里留下一个永久的洞——两端都以为成功。8 MB ≈ 32 块，与发送侧在途预算对称。
const INBOUND_CHUNK_BYTES_MAX: usize = 8 * 1024 * 1024;
/// 单个接收任务允许 stash 多少字节的「早到」分块（见 `RecvSession::ahead`）。
/// 4 MB ≈ 16 块：够跨过一次真实的乱序/重传窗口，又不至于让一个永远补不上的洞把内存
/// 吃掉一整段。超出上限就退回"发续传请求"的老处置，并如实告诉用户为什么。
const AHEAD_BYTES_MAX: usize = 4 * 1024 * 1024;
/// 所有在途接收任务加起来能 stash 的字节上限。一次相册导出可以有 64 条接收会话，
/// 只封单任务的话总量是 64 × 4 MB——内存红线就是这么被"每个都不超"的分配顶破的。
const AHEAD_TOTAL_BYTES_MAX: usize = 8 * 1024 * 1024;
/// 发送侧等待对端 FILE_DONE 回执的上限（发完 ≠ 送达）。
const ACK_TIMEOUT_MS: i64 = 30_000;

/// 收到"发端说发完了、可本端有洞"之后，暂缓收尾等补发的窗口。
/// 计时口径是**"多久没有补发进展"**而不是"一共等了多久"：每落一块就重新计时（见 `land_chunk`），
/// 补发正在进行时这条窗口永远不会把自己掐死。
/// 起始 20 秒：拖到收尾才发现的洞必在最后 `AHEAD_BYTES_MAX`（4 MB）之内；对端不肯补时少白等 40 秒。
const RESUME_HOLD_MS: i64 = 20_000;

/// 单个接收文件的上限：`size` 是**对端声明的**，不能由它决定本机开多大的文件。
/// 4 GiB 对照片/视频/常见文档都有余量，同时挡住"声明一个天文数字然后慢慢写满你盘"。
/// 安卓侧同一口径（`LinkxRuntime.kt` 的 `MAX_RECV_FILE_BYTES`）——两台机器契约必须一致。
const MAX_RECV_FILE_BYTES: u64 = 4 * 1024 * 1024 * 1024;
/// 自愈重连最多等多久让对端带着新地址重新广播。手机切回前台通常一两秒内就会重新广播，
/// 30 秒覆盖"从口袋里掏出来解锁打开 App"这一档操作。
const BLE_RECONNECT_WAIT: Duration = Duration::from_secs(30);

/// 从扫描结果里挑出「这个名字**最新一次**广播用的地址」。
/// 手机换地址后表里会同时留着新旧两条同名记录（旧的靠 TTL 自己退场），而 `push_device`
/// 保证"越靠后越新可见"——必须从后往前找，取前面那条就会连到已经失效的旧地址。
fn freshest_addr_for(devices: &[(u64, String)], names: &[String]) -> Option<u64> {
    devices
        .iter()
        .rev()
        .find(|(_, n)| names.iter().any(|want| want == n))
        .map(|(a, _)| *a)
}

/// 自愈重连可认领的广播名候选，按优先级去重。
///
/// 少了"上次落库的广播名"会出死局：`selected` 常已从扫描列表过期，只剩握手自报名，
/// 就去等一个永远不会出现的名字（实测广播名「Redmi Note 11T Pro」vs 自报名「22041216C」）。
fn reconnect_names(scan: Option<&str>, persisted: Option<&str>, hello: &str) -> Vec<String> {
    let mut v: Vec<String> = Vec::new();
    for cand in [scan, persisted, Some(hello)]
        .into_iter()
        .flatten()
        .map(str::trim)
        .filter(|n| !n.is_empty())
    {
        if !v.iter().any(|seen| seen == cand) {
            v.push(cand.to_string());
        }
    }
    v
}

/// 发现 / TCP 监听启动失败后的重试间隔（端口被占用时不至于每 200ms 刷屏）
const CHANNEL_RETRY: Duration = Duration::from_secs(5);

/// 两次自动拨号之间的最小间隔。广播地址过期要 12 s，握手本身也要时间，更密的重试
/// 只会在后台空转（连续三次不成本轮就交给用户，见 `resolve_auto_dial`）。
const AUTO_DIAL_RETRY: Duration = Duration::from_secs(30);

/// 自动拨号该认哪些广播名。两个名字源都必须认：信任库里的名字是**握手 TLV 的机型名**（`22041216C`），
/// 而扫描列表里那条是**系统蓝牙的广播友好名**（`Redmi Note 11T Pro`）——只认前者会把按名重连变成空转。
/// 硬约束：**名字表里指纹不在信任库中的条目一律丢掉**——"没配对过/已解绑"的设备不会被自动拨号；
/// 信任判定仍只在引擎侧按 RSA 指纹做（锚是指纹，不是 MAC、不是设备名），这里只挑"该往哪个广播伸手"。
fn auto_dial_names(trusted: &[TrustedPeer], hints: &[(String, String)]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut add = |n: &str| {
        let n = n.trim();
        if !n.is_empty() && !out.iter().any(|x| x == n) {
            out.push(n.to_string());
        }
    };
    for (fp, name) in hints {
        if trusted.iter().any(|p| &p.fingerprint == fp) {
            add(name);
        }
    }
    for p in trusted {
        add(&p.name);
    }
    out
}

/// 启动 BLE worker 线程（在 UI 线程调用）
pub(crate) fn spawn_worker(hwnd: HWND, state: SharedState) {
    // HWND 非 Send：仅取原始句柄值跨线程传递，worker 内按需重建
    let hwnd_raw = hwnd.0 as isize;
    std::thread::spawn(move || worker_loop(hwnd_raw, state));
}

/// 一次性取出的 UI 下行命令（取走即清空，避免持锁期间调用引擎/系统 API）
#[derive(Debug, Default)]
struct UiRequests {
    connect: Option<u64>,
    confirm_sas: bool,
    reject_sas: bool,
    send_clip: Option<String>,
    send_file: bool,
    manual_ip: bool,
    unbind: bool,
    accept_identity: bool,
    reject_identity: bool,
    /// 取消一条在途传输，行键 `(方向, 文件名)`
    cancel_file: Option<(u8, String)>,
    /// 待下发的通知回复（UI 已分配 reply_id 并登记在 `reply_pending`）
    notify_reply: Option<crate::state::ReplyRequest>,
    /// 待下发的播放指令 `(action, volume, delta_ms)`
    media_cmd: Option<(i32, i32, i64)>,
    // ---- 相册：三条只走局域网 TCP 的请求（UI 写、worker 取走）----
    album_list: Option<(u32, u32)>,
    album_full: Option<Vec<u64>>,
    album_drag: Vec<u64>,
    /// 调试面单独点名要一张缩略图
    album_thumb_one: Option<u64>,
}

/// 从共享状态取出并清空本轮命令
fn take_ui_requests(st: &mut UiState) -> UiRequests {
    let confirm_sas = st.confirm_sas_req;
    st.confirm_sas_req = false;
    let reject_sas = st.reject_sas_req;
    st.reject_sas_req = false;
    let send_file = st.send_file_req;
    st.send_file_req = false;
    let manual_ip = st.manual_ip_req;
    st.manual_ip_req = false;
    let unbind = st.unbind_requested;
    st.unbind_requested = false;
    let accept_identity = st.accept_identity_req;
    st.accept_identity_req = false;
    let reject_identity = st.reject_identity_req;
    st.reject_identity_req = false;
    UiRequests {
        media_cmd: st.media_cmd_req.take(),
        connect: st.connect_req.take(),
        confirm_sas,
        reject_sas,
        send_clip: st.send_clip_req.take(),
        send_file,
        manual_ip,
        unbind,
        accept_identity,
        reject_identity,
        cancel_file: st.cancel_file_req.take(),
        notify_reply: st.reply_req.take(),
        album_list: st.album.req_list.take(),
        album_full: st.album.req_full.take(),
        album_drag: std::mem::take(&mut st.album.drag_req),
        album_thumb_one: st.album.thumb_one_req.take(),
    }
}

fn worker_loop(hwnd_raw: isize, state: SharedState) {
    // 身份三件套（全部持久化，跨重启稳定）：① X25519 Noise static（握手用）② RSA-2048
    // 设备身份（信任锚）③ 信任库。身份加载失败必须显式上报，**不能**静默用临时身份继续
    // （那会让对端看到「新设备」）。
    let local_sk = match identity::load_or_create_static_key() {
        Ok(sk) => sk,
        Err(e) => {
            let mut st = state.lock().unwrap();
            st.hwnd_raw = hwnd_raw;
            st.push_error(format!("身份初始化失败: {e}"));
            st.conn_state = state_code::CLOSED;
            drop(st);
            post_state_changed(hwnd_raw);
            return;
        }
    };
    let device_der = match identity::load_or_create_device_identity() {
        Ok(der) => der,
        Err(e) => {
            let mut st = state.lock().unwrap();
            st.hwnd_raw = hwnd_raw;
            st.push_error(format!("设备身份初始化失败: {e}"));
            st.conn_state = state_code::CLOSED;
            drop(st);
            post_state_changed(hwnd_raw);
            return;
        }
    };
    let trusted = identity::load_trusted_peers();
    {
        let mut st = state.lock().unwrap();
        st.hwnd_raw = hwnd_raw;
        // 本机指纹 = RSA 身份指纹（16 位 hex；与对端看到的、日志里的完全同源）
        st.local_fp = identity::identity_fingerprint(&device_der).unwrap_or_default();
        // 信任库 = 设置页「已绑定设备」数据源（跨重启可见，解绑可清）
        st.bound_devices = trusted
            .iter()
            .map(|p| (p.fingerprint.clone(), p.name.clone()))
            .collect();
        // Debug 模式持久化开启 → 立刻挂 sink（日志落 `%APPDATA%\LinkX\Logs`）
        if st.debug_enabled {
            if let Some(dir) = identity::debug_log_dir() {
                if let Err(e) = debuglog::enable(&dir) {
                    st.push_error(format!("Debug 日志初始化失败: {e}"));
                }
            }
        }
        // 收件目录：默认 %USERPROFILE%\Downloads\LinkX，缺失即创建（接收落盘依赖它）
        let inbox = st.inbox_dir.clone();
        if let Err(e) = fs::create_dir_all(&inbox) {
            st.push_error(format!("收件目录创建失败（{inbox}）: {e}"));
        }
    }
    post_state_changed(hwnd_raw);

    let ble = match BleCentral::new() {
        Ok(b) => b,
        Err(e) => {
            state
                .lock()
                .unwrap()
                .push_error(format!("BLE 初始化失败: {e}"));
            post_state_changed(hwnd_raw);
            return;
        }
    };

    // WinRT 通知线程 → 本线程的字节队列（通知回调在 WinRT 线程执行）
    let rx_ble: Arc<Mutex<VecDeque<Vec<u8>>>> = Arc::new(Mutex::new(VecDeque::new()));

    // 扫描：只上报命中 LinkX 特征的设备（WinRT 线程回调）
    {
        let st_scan = state.clone();
        let hwnd_scan = hwnd_raw;
        let res = ble.start_scan(move |adv| {
            if !adv.linkx {
                return;
            }
            let changed = st_scan
                .lock()
                .unwrap()
                .push_device(adv.address, adv.name.clone());
            if changed {
                post_state_changed(hwnd_scan);
            }
        });
        if let Err(e) = res {
            state
                .lock()
                .unwrap()
                .push_error(format!("BLE 扫描启动失败: {e}"));
            post_state_changed(hwnd_raw);
        }
    }

    let mut worker = Worker {
        hwnd_raw,
        state,
        ble,
        rx_ble,
        engine: None,
        local_sk,
        device_der,
        trusted,
        tcp: None,
        tcp_bind_started: false,
        tcp_live_conn: 0,
        tcp_start_failed_at: None,
        config_sent: false,
        discovery: None,
        discovery_failed_at: None,
        pending_send: None,
        send: None,
        recv: Vec::new(),
        drop_chunk_at: None,
        drop_recv_chunk_at: None,
        last_send_err: None,
        last_tick: Instant::now(),
        ble_fail_streak: 0,
        last_rx_at: Instant::now(),
        last_ble_retry: Instant::now(),
        ble_heal_attempts: 0,
        reconnect: None,
        reconnect_deadline: None,
        auto_dial: None,
        auto_dial_not_before: None,
        auto_dial_fails: 0,
        album_inflight: Vec::new(),
    };
    // 冷启动自动重连的武装点：开关关着、或这台电脑从来没配对成功过（信任库为空）→
    // 名单是 None，必须人在「连接」页点一次设备。
    worker.arm_auto_dial("startup");
    worker.run();
}

/// 连接指定设备并创建会话引擎（Initiator = Central）。身份口径：Noise static 只建通道；
/// **设备识别与信任判定全部以 RSA 身份指纹（`device_der` / `trusted`）为准**——已信任设备
/// 命中即跳过 SAS 弹窗自动放行，同名新身份则走 -213 + `IdentityChanged` 复核。
fn connect_and_engine(
    ble: &BleCentral,
    rx: &Arc<Mutex<VecDeque<Vec<u8>>>>,
    addr: u64,
    local_sk: &[u8; 32],
    device_der: &[u8],
    trusted: &[TrustedPeer],
) -> Result<SessionEngine, String> {
    ble.connect(addr)
        .map_err(|e| format!("BLE 连接失败: {e}"))?;

    let rx_sink = rx.clone();
    ble.subscribe_notify(move |bytes: &[u8]| {
        // WinRT 线程：仅搬运字节，不碰 UI
        #[cfg(feature = "agent-debug")]
        linkx_debugd::bump("ble_rx_from_stack", 1);
        rx_sink.lock().unwrap().push_back(bytes.to_vec());
    })
    .map_err(|e| format!("BLE 订阅失败: {e}"))?;

    let cfg = EngineConfig::new(
        EngineRole::Initiator,
        LOCAL_NAME,
        linkx_protocol::OS_WINDOWS,
        linkx_core::LINKX_FFI_VERSION,
        *local_sk,
    )
    .with_identity_der(device_der.to_vec())
    .with_trusted_peers(trusted.to_vec());
    let mut eng = SessionEngine::new(cfg);
    // 分片长度不再由平台层查询注入：引擎按**实际收到的对端分片长度**自适应抬升
    // （见 `SessionEngine::feed`）。首帧 HELLO 仍按保守的 23 切，收到对端片后即升到链路真实能力。
    eng.start(Instant::now()); // 发本端 HELLO，开始握手
    Ok(eng)
}

/// worker 上下文（收拢散装局部变量，便于文件/TCP/发现三块状态共享）
struct Worker {
    hwnd_raw: isize,
    state: SharedState,
    ble: BleCentral,
    rx_ble: Arc<Mutex<VecDeque<Vec<u8>>>>,
    engine: Option<SessionEngine>,
    local_sk: [u8; 32],
    /// RSA-2048 设备身份（PKCS#8 DER；跨重启稳定，注入每次新建的引擎）
    device_der: Vec<u8>,
    /// 已信任对端（RSA 指纹 + 名称；配对成功后 upsert 并落盘）
    trusted: Vec<TrustedPeer>,

    tcp: Option<TcpService>,
    /// 是否已调用 `begin_tcp_binding`（未调用前不喂 TCP 帧）
    tcp_bind_started: bool,
    /// 当前被视为"活着"的 TCP 连接号（0 = 无）。用于丢弃旧连接遗留的断开事件，
    /// 否则对端换目标重连时，上一条连接的关闭事件会把刚建好的新链路拆掉。
    tcp_live_conn: u64,
    /// 上次 TCP 启动失败时刻（按 `CHANNEL_RETRY` 退避重试）
    tcp_start_failed_at: Option<Instant>,
    /// 跨端配置是否已在本次会话推送过
    config_sent: bool,

    discovery: Option<UdpDiscovery>,
    /// 上次 UDP 发现启动失败时刻
    discovery_failed_at: Option<Instant>,

    /// 待发送文件路径（等配对 + TCP 就绪）
    pending_send: Option<String>,
    send: Option<SendSession>,
    recv: Vec<RecvSession>,
    /// 故障注入的落地副本（`/action/drop-chunk` 写 `state.debug_drop_chunk_at`，见 `pump_send`）
    drop_chunk_at: Option<u32>,
    /// 收端故障注入（`/action/drop-recv-chunk`）：这一号的入站分块当作"链路没送到"丢掉
    drop_recv_chunk_at: Option<u32>,

    last_send_err: Option<String>,
    last_tick: Instant,

    /// 连续 BLE 写失败次数（成功即归零）；见 [`Worker::heal_stalled_ble`]
    ble_fail_streak: u32,
    /// 最后一次收到任何 BLE 字节的时刻
    last_rx_at: Instant,
    /// 上一次自愈重连的时刻（限流用，避免每轮重连）
    last_ble_retry: Instant,
    /// 本轮已尝试过几次自愈（成功建链即归零；到上限就停下来交给用户）
    ble_heal_attempts: u32,
    /// 等待"按名字重新出现"的对端：`(用于展示的名字, 全部可接受的名字)`。
    /// 安卓侧广播用的是随机可解析地址，手机每次唤醒/重启 App 都会换新地址，**缓存下来的旧地址是
    /// 连不上的**——自愈不能拿旧地址硬试，只能等它带着新地址重新被扫描到；名字（HELLO 的
    /// advert_name / 广播名）才是稳定的身份。两个名字源必须同时接受：设备列表里那条是 **BLE 扫描名**
    /// （系统蓝牙友好名），而 `peer_name` 是握手 TLV 里的机型名，只认后者永远找不到。
    reconnect: Option<(String, Vec<String>)>,
    /// 按名字重连的等待截止时刻；超时即出声交给用户，不无限等
    reconnect_deadline: Option<Instant>,
    /// 自动拨号待认的广播名（None = 未武装：开关关了、或没绑定过设备）
    auto_dial: Option<Vec<String>>,
    /// 下一次允许自动拨号的最早时刻（两次尝试之间留 [`AUTO_DIAL_RETRY`]）
    auto_dial_not_before: Option<Instant>,
    /// 连续自动拨号失败次数：攒够就把话说明白交给用户，不在后台无限空转
    auto_dial_fails: u32,

    /// 在途缩略图 `(页码代际, 照片 id, 发出时刻)`。代际必须由 worker 自己记住：应答里的
    /// `id` 只能说明"是哪张照片"，说明不了"它是哪一页要的"——不带代际就会把上一页的
    /// 应答画到这一页上。
    album_inflight: Vec<(u64, u64, Instant)>,
}

impl Worker {
    /// 主循环：一轮 11 步，全部在**本线程**内完成（UI 线程只读状态与写命令）。
    /// 单线程串行意味着：**任何一步阻塞，整轮都停**——`pump_ble_rx` 不再取包、`tick` 不再
    /// 发心跳，这正是长阻塞卡死心跳的机理；故 `agent-debug` 下给两个阻塞式 IO 步骤各装
    /// 水位计时器（光测整轮耗时不足以定位卡在哪一步）。
    fn run(&mut self) {
        #[cfg(feature = "agent-debug")]
        {
            // 注册 /debug 端点的回调：复用既有 apply_debug_toggle（它才掌握平台相关的日志目录
            // 决策 `%APPDATA%\LinkX\Logs`），控制面自身不决定落盘点；不注册则 /debug 返回 applied:false。
            linkx_debugd::on_debug_toggle(|on| {
                crate::debug::apply_debug_toggle(on);
                debuglog::is_enabled() == on
            });
            // 动作处理器：**只设 UiState 上那组既有的 *_req 命令字段**，由 worker 的
            // pump_ui_requests 消费——与鼠标点击、SendTo 启动参数走同一条生产路径。绝不另写
            // "调试专用"连接/配对逻辑，否则会造出「调试面跑得通、生产面跑不通」的假验证。
            linkx_debugd::on_action(|name, query| {
                let arc = crate::window::shared_state().ok_or("UI 状态未就绪")?;
                let mut st = arc.lock().map_err(|_| "状态锁不可用")?;
                match name {
                    "connect" => {
                        let raw =
                            linkx_debugd::query_param(query, "addr").ok_or("缺 ?addr=<16进制>")?;
                        let addr =
                            u64::from_str_radix(&raw, 16).map_err(|_| "addr 需为 16 进制")?;
                        st.connect_req = Some(addr);
                        Ok(format!("已下发连接请求 addr={raw}"))
                    }
                    "confirm-sas" => {
                        st.confirm_sas_req = true;
                        Ok("已下发 SAS 确认一致".to_string())
                    }
                    "reject-sas" => {
                        st.reject_sas_req = true;
                        Ok("已下发 SAS 不一致".to_string())
                    }
                    "send-clip" => {
                        let text = linkx_debugd::query_param(query, "text").ok_or("缺 ?text=")?;
                        st.send_clip_req = Some(text);
                        Ok("已下发剪贴板发送".to_string())
                    }
                    // 程序化回复一条通知：与鼠标点「回复」→ 输入 → 「发送」写同一组字段、
                    // 调同一个 submit_reply，绝不另开一条调试专用发送路径。
                    "reply-notify" => {
                        let pkg = linkx_debugd::query_param(query, "pkg").ok_or("缺 ?pkg=")?;
                        let text = linkx_debugd::query_param(query, "text").ok_or("缺 ?text=")?;
                        let id: i32 = linkx_debugd::query_param(query, "id")
                            .and_then(|v| v.parse().ok())
                            .unwrap_or_default();
                        let tag = linkx_debugd::query_param(query, "tag").unwrap_or_default();
                        let item = st
                            .notifications
                            .iter()
                            .find(|n| n.package == pkg && n.notification_id == id && n.tag == tag)
                            .ok_or(
                                "通知列表里没有这一条（pkg/id/tag 照 /state.notifications 给）",
                            )?;
                        if !item.can_reply {
                            return Err("这条通知没有回复入口：应用没挂 RemoteInput".to_string());
                        }
                        st.reply_target = Some(crate::state::ReplyTarget {
                            package: item.package.clone(),
                            tag: item.tag.clone(),
                            notification_id: item.notification_id,
                            action_index: item.reply_action_index,
                            result_key: item.reply_result_key.clone(),
                        });
                        st.reply_input = text;
                        crate::window::submit_reply(&mut st);
                        Ok("已下发通知回复（结果看 /state 的 reply_hint）".to_string())
                    }
                    "send-file" => {
                        let path = linkx_debugd::query_param(query, "path").ok_or("缺 ?path=")?;
                        st.send_path_input = path.clone();
                        st.send_file_req = true;
                        // 这里只"下发命令"，不宣称已排队：是否受理由 worker 判（模块关、路径空、已有任务在传都会拒），结论看 /state.errors。
                        Ok(format!(
                            "已下发发送请求: {path}（受理结果见 /state 的 errors）"
                        ))
                    }
                    // 程序化取消一条在途传输——与文件页行上「取消」写同一个命令字段，不另造调试路径。
                    "cancel-file" => {
                        let name = linkx_debugd::query_param(query, "name").ok_or("缺 ?name=")?;
                        let dir = match linkx_debugd::query_param(query, "dir").as_deref() {
                            Some("recv") | Some("1") => crate::state::TASK_DIR_RECV,
                            _ => crate::state::TASK_DIR_SEND,
                        };
                        st.cancel_file_req = Some((dir, name.clone()));
                        Ok(format!(
                            "已下发取消请求: {name}（dir={dir}，结果看 /state 的 file_tasks 与 errors）"
                        ))
                    }
                    // 故障注入（只用于验证补发路径）：把某一号 `FILE_CHUNK` 在"交给引擎"之前丢掉，
                    // 其余一切照常——与链路真丢一帧的后果一致。
                    "drop-chunk" => {
                        let raw = linkx_debugd::query_param(query, "at")
                            .ok_or("缺 ?at=<分块序号，一次性>")?;
                        let at = raw.parse::<u32>().map_err(|_| "at 需为十进制数字")?;
                        st.debug_drop_chunk_at = Some(at);
                        Ok(format!(
                            "已登记：第 {at} 块 FILE_CHUNK 不会交给引擎（一次性）"
                        ))
                    }
                    "drop-recv-chunk" => {
                        let raw = linkx_debugd::query_param(query, "at")
                            .ok_or("缺 ?at=<分块序号，一次性>")?;
                        let at = raw.parse::<u32>().map_err(|_| "at 需为十进制数字")?;
                        st.debug_drop_recv_chunk_at = Some(at);
                        Ok(format!("已登记：入站第 {at} 块将当作没收到（一次性）"))
                    }
                    "manual-ip" => {
                        st.manual_ip_req = true;
                        Ok("已下发手动 IP 连接".to_string())
                    }
                    "unbind" => {
                        st.unbind_requested = true;
                        Ok("已下发解绑".to_string())
                    }
                    "accept-identity" => {
                        st.accept_identity_req = true;
                        Ok("已接受身份变更".to_string())
                    }
                    "reject-identity" => {
                        st.reject_identity_req = true;
                        Ok("已拒绝身份变更".to_string())
                    }
                    // 功能开关：改完立刻落盘（与鼠标点击同一条保存出口），但**不**自动重启——
                    // 重启必须由验收脚本明确发起，否则测试里进程会在两行断言之间换掉，读到的状态
                    // 对不上是哪个实例。
                    "feature" => {
                        let raw = linkx_debugd::query_param(query, "module")
                            .ok_or("缺 ?module=notifications|clipboard|file_transfer")?;
                        let m = crate::features::Module::from_key(&raw)
                            .ok_or("module 取值无效（须为三者之一）")?;
                        let flag = linkx_debugd::query_param(query, "on")
                            .ok_or("缺 ?on=0|1（不给默认值：写错参数不该悄悄改状态）")?;
                        let on = match flag.as_str() {
                            "1" => true,
                            "0" => false,
                            _ => return Err("on 只接受 0 或 1".to_string()),
                        };
                        m.set_wanted(&mut st, on);
                        // 保存必须**在同一把锁内**完成，与 UI 点击路径（window.rs 同样持锁 save）一个约定。若改成
                        // "取快照 → 放锁 → 写盘"，两条路径交错时会拿旧快照盖掉用户刚改的开关：内存说关、磁盘说开。
                        settings::Settings::from_state(&st).save();
                        Ok(format!(
                            "已设置功能开关 {}={}；本次启动仍加载={}，重启后生效",
                            m.key(),
                            on,
                            crate::features::enabled(m)
                        ))
                    }
                    // 下发播放指令（与媒体页按钮写同一个命令字段）
                    "media" => {
                        let a = linkx_debugd::query_param(query, "action")
                            .and_then(|v| v.parse::<i32>().ok())
                            .ok_or("缺 ?action=0..8")?;
                        let volume = linkx_debugd::query_param(query, "volume")
                            .and_then(|v| v.parse::<i32>().ok())
                            .unwrap_or(0);
                        let delta = linkx_debugd::query_param(query, "delta_ms")
                            .and_then(|v| v.parse::<i64>().ok())
                            .unwrap_or(0);
                        st.media_cmd_req = Some((a, volume, delta));
                        Ok(format!("已下发播放指令 action={a} volume={volume}"))
                    }
                    // 相册的三个动作都**只写生产用的那批命令字段**（`album.req_*` / `plan_fetch`），由 worker
                    // 同一处出口消费——"调试面跑得通而生产面跑不通"的假验证，就是从"另写一套"开始的。
                    "album-list" => {
                        let page = linkx_debugd::query_param(query, "page")
                            .and_then(|v| v.parse::<u32>().ok())
                            .unwrap_or(0);
                        let per = linkx_debugd::query_param(query, "per")
                            .and_then(|v| v.parse::<u32>().ok())
                            .unwrap_or(crate::state::ALBUM_PER_PAGE);
                        st.album.req_list = Some((page, per));
                        Ok(format!("已下发相册清单请求 page={page} per={per}（结果看 /state.album 与 errors）"))
                    }
                    "album-thumb" => {
                        let raw =
                            linkx_debugd::query_param(query, "id").ok_or("缺 ?id=<照片 id>")?;
                        let id = raw.parse::<u64>().map_err(|_| "id 需为十进制数字")?;
                        st.album.thumb_one_req = Some(id);
                        Ok(format!("已下发缩略图请求 id={id}"))
                    }
                    "album-export" => {
                        let raw = linkx_debugd::query_param(query, "id")
                            .ok_or("缺 ?id=<照片 id，可逗号分隔>")?;
                        let ids: Vec<u64> = raw
                            .split(',')
                            .filter_map(|s| s.trim().parse::<u64>().ok())
                            .collect();
                        if ids.is_empty() {
                            return Err("id 列表里没有一个能解析成数字".to_string());
                        }
                        let dir = linkx_debugd::query_param(query, "dir")
                            .ok_or("缺 ?dir=<导出目录绝对路径>")?;
                        st.album
                            .plan_fetch(&ids, dir.clone(), crate::state::AlbumPurpose::Export);
                        Ok(format!(
                            "已下发原图请求 {} 张 → {dir}（进度看 /state 的 file_tasks）",
                            ids.len()
                        ))
                    }
                    "restart" => {
                        // 与「重新启动」按钮同一条路径：置命令位 → 退出收尾 → `main` 拉起新实例
                        st.restart_req = true;
                        let hwnd_raw = st.hwnd_raw;
                        drop(st);
                        crate::window::request_exit_from_raw(hwnd_raw);
                        Ok("已按重启流程退出并拉起新实例".to_string())
                    }
                    other => Err(format!("未知动作 {other}")),
                }
            });
        }
        #[cfg(feature = "agent-debug")]
        if let Err(e) = linkx_debugd::start(linkx_debugd::DEFAULT_PORT) {
            // 控制面起不来不得影响主功能（端口被占等）——只报一次。
            eprintln!("[LinkX] debugd 未启动（不影响功能）: {e}");
        }
        // 只声明不赋初值：计时必须在每轮工作开始时取点，循环外取的值永远读不到（clippy -D warnings 拦下）。
        #[cfg(feature = "agent-debug")]
        let mut round_t0;
        // 上一轮已通知 UI 的界面变化序号（见 `UiState::ui_rev`）
        let mut last_ui_rev = 0u64;

        loop {
            // 计时起点必须在本轮工作**开始时**取；若在末尾取，200ms 的 POLL_INTERVAL 会被计入
            // loop_round_us，基线等于 sleep 时长，再卡也看不出差别——这是判长阻塞的唯一判据。
            #[cfg(feature = "agent-debug")]
            {
                round_t0 = std::time::Instant::now();
            }

            self.pump_ui_requests();
            self.ensure_channels();
            // TCP 必须**先于** BLE 处理：对端一建立 TCP 就会立刻经 BLE 发来绑定 proof，而 `begin_tcp_binding`
            // 是在 `pump_tcp_rx` 里发起的；顺序反过来时 proof 到达时引擎的 `binding` 还是 None、被直接丢弃——
            // Peripheral 侧已 bound、Central 侧永远绑不上。
            self.pump_tcp_rx();
            self.pump_ble_rx();

            #[cfg(feature = "agent-debug")]
            let ble_t0 = std::time::Instant::now();
            self.flush_ble_out();
            #[cfg(feature = "agent-debug")]
            linkx_debugd::bump_max("step_flush_ble_out_us", ble_t0.elapsed().as_micros() as u64);
            // 写失败攒够且长时间收不到字节 → 作废句柄，改为等对端带新地址重新广播
            self.heal_stalled_ble();
            // 每轮都看一眼扫描结果：等到的话这里就真重连（地址漂移）
            self.resolve_reconnect();
            // 没有会话时自动拨已绑定设备（用户刚点过 / 自愈在等都不插手）
            self.resolve_auto_dial();

            #[cfg(feature = "agent-debug")]
            let tcp_t0 = std::time::Instant::now();
            self.flush_tcp_out();
            #[cfg(feature = "agent-debug")]
            linkx_debugd::bump_max("step_flush_tcp_out_us", tcp_t0.elapsed().as_micros() as u64);

            self.drain_events();
            self.drain_chunks();
            self.pump_send();
            self.album_pump();
            self.tick();
            // 暂缓收尾的补发窗口每轮看一眼，到点必须有人把它收掉
            self.tick_resume_holds();
            self.pump_discovery();
            // 设备列表按 TTL 过期：安卓广播用的是会轮换的随机地址，不过期就会堆满同一台手机的历史地址。
            if self.state.lock().unwrap().sweep_devices() > 0 {
                post_state_changed(self.hwnd_raw);
            }
            // 等不到回执的回复每轮看一眼：对端版本不认识这条请求时，电脑不能永远停在"什么都没发生"
            if self
                .state
                .lock()
                .unwrap()
                .expire_reply_timeouts(Instant::now())
                > 0
            {
                post_state_changed(self.hwnd_raw);
            }
            // 链路静默时长每轮都要写回，并在"活着↔收不到"翻转的那一刻拉一次重绘。不这么做的话，
            // 绿色「已配对」要一直挂到引擎自己的几十秒超时才会变色——用户那边早就什么都干不了。
            {
                let silent = self
                    .last_rx_at
                    .elapsed()
                    .min(Duration::from_secs(3600))
                    .as_millis() as u64;
                let mut st = self.state.lock().unwrap();
                let was_dead = st.rx_silent_ms >= crate::state::LINK_SILENCE_MS;
                st.rx_silent_ms = silent;
                if was_dead != (silent >= crate::state::LINK_SILENCE_MS) {
                    st.ui_rev += 1;
                }
            }
            // 进度条/状态文字这类"worker 写完没有任何消息会到达 UI"的改动，靠 ui_rev 自己拉一次重绘。
            let rev = self.state.lock().unwrap().ui_rev;
            if rev != last_ui_rev {
                last_ui_rev = rev;
                post_state_changed(self.hwnd_raw);
            }

            #[cfg(feature = "agent-debug")]
            {
                linkx_debugd::bump_max("loop_round_us", round_t0.elapsed().as_micros() as u64);
                linkx_debugd::bump("loop_rounds", 1);
                self.publish_debug_state();
            }

            std::thread::sleep(POLL_INTERVAL);
        }
    }

    /// `/state.host.clip` 的观测值：最多 1 Hz 真读一次本机剪贴板，其余时候回读缓存。
    /// 读不到如实写 `<unreadable>`（剪贴板被别的进程锁住是常态，留空会被误读成"剪贴板是空的"）。
    #[cfg(feature = "agent-debug")]
    fn observed_clipboard() -> String {
        // 采样间隔：worker 每 200 ms 跑一轮，这里取略小于 1 s，读数才不会长期停在旧值上
        const READ_EVERY: std::time::Duration = std::time::Duration::from_millis(950);
        thread_local! {
            static CLIP_OBS: std::cell::RefCell<Option<(std::time::Instant, String)>> =
                const { std::cell::RefCell::new(None) };
        }
        CLIP_OBS.with(|c| {
            let mut g = c.borrow_mut();
            if g.as_ref().is_none_or(|(at, _)| at.elapsed() >= READ_EVERY) {
                let v = crate::clipboard::read_text().unwrap_or_else(|| "<unreadable>".to_string());
                *g = Some((std::time::Instant::now(), v));
            }
            g.as_ref().map(|(_, v)| v.clone()).unwrap_or_default()
        })
    }

    /// 把本轮排查心跳所需的状态面一次性发布到 `/state`。刻意只读、不改任何行为；
    /// 缺的字段（如引擎内部 idle）由计数器与 `loop_round_us` 反推，避免为调试去动核心引擎的私有状态。
    #[cfg(feature = "agent-debug")]
    fn publish_debug_state(&mut self) {
        let phase = self
            .engine
            .as_ref()
            .map(|e| format!("{:?}", e.state()))
            .unwrap_or_else(|| "no-engine".to_string());
        let paired = self.engine.as_ref().map(|e| e.is_paired()).unwrap_or(false);
        // 引擎级 tcp_bound 与 app 级 tcp_bind_started 是**两回事**：后者只说明"我发起过绑定"，
        // 前者才是"绑定四步走完、可以传业务数据"；排查「等待 TCP 通道」时只看后者会误判成已就绪。
        let engine_tcp_bound = self
            .engine
            .as_ref()
            .map(|e| e.is_tcp_bound())
            .unwrap_or(false);
        let own_fp = self
            .engine
            .as_ref()
            .and_then(|e| e.own_identity_fingerprint());
        // Option 而非 usize::MAX 哨兵：JSON 里要如实表达「没有发现器」= null，占位值会误导读日志的人。
        let peers = self.discovery.as_ref().map(|d| d.peers().len());
        // 分片积压与生效片长一起看才有意义：积压持续涨=每轮片数封顶不够用；片长停在 23=还在保守档。
        let ble_mtu = self.engine.as_ref().map(|e| e.ble_mtu());
        let out_pending = self.engine.as_ref().map(|e| e.outbound_pending());
        let st = self.state.lock().unwrap();
        // 设备地址一并导出：`/action/connect?addr=` 需要它；没有它，免 GUI 驱动配对还得先靠人把地址抄过来。
        let devices: Vec<serde_json::Value> = st
            .devices
            .iter()
            .map(|(a, n)| {
                serde_json::json!({
                    "addr": format!("{a:012X}"),
                    "name": n,
                })
            })
            .collect();
        // 相册滚动的真值（含滚动条轨道的物理矩形）：绘制对不对、点轨道响不响应，导出几何一次看清，不必拿像素反推。
        let album_scroll = {
            let h = windows::Win32::Foundation::HWND(self.hwnd_raw as *mut core::ffi::c_void);
            serde_json::json!({
                "row": st.album.scroll_row,
                "max_row": crate::render::album_scroll_row_max(h, &st),
                "track": crate::render::album_track_dbg(h, &st),
                "cells": crate::render::album_visible_dbg(h, &st),
                "rects": crate::render::album_cells_dbg(h, &st),
                // 不导出页签，自动化会拿"屏幕上没有的格子"的坐标去点（几何函数不看页签）
                "tab": st.active_tab,
            })
        };
        let v = serde_json::json!({
            "platform": "windows",
            "phase": phase,
            "paired": paired,
            "discovery_peers": peers,
            "tcp_bind_started": self.tcp_bind_started,
            "engine_tcp_bound": engine_tcp_bound,
            // 当前被视为活着的连接号。Windows 一次只维持一条 TCP 连接，而真机上对端重连会产生多条；
            // 不导出这个数，就分不清"绑定没完成"是没收到帧、还是帧来自另一条已被丢弃的连接。
            "tcp_live_conn": self.tcp_live_conn,
            "ble_connected": self.ble.is_connected(),
            "has_send": self.send.is_some(),
            "recv_sessions": self.recv.len(),
            "pending_send": self.pending_send,
            "last_send_err": self.last_send_err.clone(),
            "ui_conn_state": format!("{:?}", st.conn_state),
            "ui_devices": st.devices.len(),
            // 徽章判据里"多久没收到一帧"必须能读出来，否则变色过程只能盯屏幕，改阈值也没法验证。
            "rx_silent_ms": st.rx_silent_ms,
            // 相册滚动的真值：见上面 `album_scroll` 的注释
            "ui_album_scroll": album_scroll,
            // 相册条目的 id 与"能不能拖"必须导出：`/action/album-export?id=`、`/action/album-thumb?id=`
            // 都要真实 id，不导出自动化只能拿截图像素反推（理由同上面 `devices` 里的地址）。
            "ui_album_items": st
                .album
                .items
                .iter()
                .take(24)
                .map(|it| serde_json::json!({
                    "id": it.id,
                    "name": it.name,
                    "size": it.size_bytes,
                    "kind": if it.is_video() { "video" } else { "photo" },
                    // landed = 载荷已在本地（可以拖）；got/total = 还在取的话到多少了
                    "landed": st.album.drag_ready_path(it.id).is_some(),
                    "got": st.album.drag_progress_of(it.id).map(|(g, _)| g),
                    "total": st.album.drag_progress_of(it.id).map(|(_, t)| t),
                }))
                .collect::<Vec<_>>(),
            // 传输任务的界面真值："状态在推进但画面不动"这类缺陷，只有把百分比导出才能和屏幕像素对表。
            "ui_file_tasks": st
                .file_tasks
                .iter()
                .take(4)
                .map(|t| serde_json::json!({
                    "name": t.name,
                    "dir": if t.direction == crate::state::TASK_DIR_SEND { "send" } else { "recv" },
                    "percent": t.percent,
                    "state": t.state,
                    "size": t.size,
                    "speed_kbps": t.speed_kbps,
                }))
                .collect::<Vec<_>>(),
            "ui_battery": st.battery.map(|b| format!("{}%{}", b.level, if b.charging { "+充电" } else { "" })),
            "devices": devices,
            "sas": st.sas,
            // 「对端换了身份、停在人工复核」要单独导出：只导 phase 的话，自动化分不清"在等用户裁决"和"就是没连上"。
            "identity_change": st.identity_change.as_ref().map(|c| {
                serde_json::json!({
                    "name": c.name,
                    "old_fp": c.old_fp,
                    "new_fp": c.new_fp,
                })
            }),
            "peer_name": st.peer_name,
            "own_fp": own_fp,
            "ble_mtu": ble_mtu,
            "ble_out_pending": out_pending,
            // UI 错误流必须导出：`push_error` 是 Windows 侧唯一的失败出口（发送被拒、TCP 断开、身份初始化失败都写这里），不导出等于把"为什么没成"留给用户去屏幕上找。
            "errors": st.errors.iter().take(5).map(|e| e.text()).collect::<Vec<_>>(),
            // 剪贴板真值：与安卓侧 `/state.host.clip` 同口径。读失败（被别的进程占着）如实写成
            // <unreadable>——静默留空会被误读成"剪贴板是空的"。
            // 采样而不是每轮都读：本函数跟着 worker 每 200 ms 跑一次，而 `OpenClipboard` 是跨进程
            // 调用（还常被远程桌面/同步工具短时锁住）。"观测挂在数据面每一轮上"正是安卓侧那条
            // 把吞吐压掉一半的同款形状，这里提前按 1 Hz 收口。
            "clip": Self::observed_clipboard(),
            // 手机当前播放。没有它，"电脑到底收没收到播放状态"只能靠截图。
            "media": st.media.as_ref().map(|m| {
                serde_json::json!({
                    "pkg": m.package,
                    "title": m.title,
                    "artist": m.artist,
                    "album": m.album,
                    "playing": m.playing,
                    "position_ms": m.position_ms,
                    "duration_ms": m.duration_ms,
                    "speed_x100": m.speed_x100,
                    "volume": m.volume,
                    // 封面只报有没有对上以及多大，不报内容：验收要的是"这条链路通了"，不是把图搬回控制面
                    "cover_bytes": st.cover_of_current().map(|c| c.jpeg.len()).unwrap_or(0),
                })
            }),
            // 通知列表必须能从控制面读出来（"手机发通知 → 电脑展示"不能只靠截图验收）；与 /state 的 clip 同口径，不另发明端点，所有验收脚本读同一份快照。
            "notifications": st
                .notifications
                .iter()
                .take(5)
                .map(|n| {
                    serde_json::json!({
                        "pkg": n.package,
                        "title": n.title,
                        "text": n.text,
                        "ts_ms": n.ts_ms,
                        "key": n.key_hash,
                        // 回复验收要看的就是这几个：有没有入口、点对了哪条通知
                        "id": n.notification_id,
                        "tag": n.tag,
                        "can_reply": n.can_reply,
                    })
                })
                .collect::<Vec<_>>(),
            // 运行期功能开关：**已加载**与**想要**必须分开导出，否则看到 working_set 变小却不知道少了哪个模块。
            "features_loaded": crate::features::modules_of(crate::features::active_bits()),
            "features_wanted": crate::features::modules_of(crate::features::capture(&st)),
            "restart_pending": crate::features::restart_pending(&st),
            "mem_working_set_mb": crate::features::working_set_mb(),
            // 自动拨号的"开关 + 此刻还在等什么名字"必须能读出来，否则冷启动验收只能猜这次是不是自动连上的。
            "auto_connect": st.auto_connect,
            "auto_dial": self.auto_dial.clone(),
            // 回复的最后一句结果 + 还在等回执的条数：真机矩阵靠这两个字段判"手机真的回了"，
            // 而不是靠人眼看截图。
            "reply_hint": st.reply_hint.as_ref()
                .filter(|(_, _, at)| at.elapsed() < crate::state::REPLY_HINT_TTL)
                .map(|(text, ok, _)| serde_json::json!({ "text": text, "ok": ok })),
            "reply_waiting": st.reply_pending.len(),
        });
        drop(st);
        linkx_debugd::publish(v);
    }

    /// 往界面错误流写一句（worker 侧唯一的失败出口）：`self.state` 取一次锁、写完就放。
    /// `UiState::push_error` 不可重入，所以调用方**不得持着这把锁**再报（整壳冻结）；
    /// 引擎借用还活着的那几处只能继续走字段访问，用不了这个包装
    fn push_error(&self, msg: String) {
        self.state.lock().unwrap().push_error(msg);
    }

    // ---------- 1) 下行命令 ----------

    /// 建立/重建到指定 BLE 地址的链路。**用户点击与卡死自愈共用这一条出口**，避免"自愈"另养
    /// 一套 teardown（那会把瞬时失败变成永久断链）。
    fn reconnect_ble(&mut self, addr: u64) {
        if let Some(mut old) = self.engine.take() {
            old.user_disconnect();
        }
        self.abandon_inflight_tasks();
        self.teardown_channels();
        // 上一轮残留的分片必须丢掉：它们属于**已作废的会话密钥**，喂进新引擎
        // 只会变成 AEAD 失败或"心跳载荷畸形"这类看不出来源的错误。
        self.rx_ble.lock().unwrap().clear();
        // 在途写同理：旧链路的异步句柄不能带进新连接，否则新引擎的第一片
        // 会被"上一轮还在途"挡住，直到旧句柄自己超时。
        self.ble.abort_inflight();
        self.ble.invalidate_link();
        self.ble_fail_streak = 0;
        self.last_ble_retry = Instant::now();
        self.state.lock().unwrap().selected = Some(addr);
        match connect_and_engine(
            &self.ble,
            &self.rx_ble,
            addr,
            &self.local_sk,
            &self.device_der,
            &self.trusted,
        ) {
            Ok(eng) => {
                self.engine = Some(eng);
                self.last_rx_at = Instant::now();
                self.ble_heal_attempts = 0;
                self.reconnect = None;
                self.reconnect_deadline = None;
            }
            Err(msg) => {
                let mut st = self.state.lock().unwrap();
                st.push_error(msg);
                st.conn_state = state_code::CLOSED;
            }
        }
    }

    /// 链路重建前把在途任务落到"失败"并说清原因。不这么做的话，任务行会永远停在
    /// "发送中 60%"、半个文件留在盘上，而用户没有任何提示。
    fn abandon_inflight_tasks(&mut self) {
        let mut st = self.state.lock().unwrap();
        if let Some(s) = self.send.take() {
            st.update_file_task(&s.name, TASK_DIR_SEND, 0, "失败");
            st.push_error(format!("链路重建，{} 的发送已中断，请重新发送", s.name));
        }
        if let Some(p) = self.pending_send.take() {
            st.push_error(format!("{p} 还在排队就被取消：链路已重建"));
        }
        let dropped: Vec<(String, u64)> = self
            .recv
            .drain(..)
            .map(|r| (r.name.clone(), r.album_id))
            .collect();
        for (name, _album_id) in &dropped {
            st.update_file_task(name, TASK_DIR_RECV, 0, "失败");
            st.push_error(format!("链路重建，{name} 的接收未完成，请让对方重发"));
        }
        drop(st);
        // 在等的拖拽必须被告知这一声，否则它会一直等到超时才说得出原因
        for (name, album_id) in dropped {
            self.album_settled(album_id, false, format!("{name}：链路重建，接收中断"));
        }
    }

    /// 卡死的 GATT 句柄自愈：手机 LinkX 被杀或覆盖安装后，它的 GATT Server 消失，而 Windows 这边的
    /// `BluetoothLEDevice` 句柄还"连着"——每次写都返回 `Unreachable`、握手永远停在 Handshake。
    /// 只作废句柄不重连会把瞬时失败变成永久断链，所以作废之后必须重连。
    /// 三道闸门，任一不满足都不动手：① 连续写失败 ≥ 6 次；② ≥ 5 s 没收到任何 BLE 字节；③ 距上次
    /// 重试 ≥ 8 s。两条护栏是这条路径能被安全自动化的前提：**局域网还活着就不拆**（重建链路会连带
    /// 停掉 TCP 服务，为修蓝牙打断正在跑的大文件传输是净负收益）、**最多三次**（修不好就停下来交给用户）。
    fn heal_stalled_ble(&mut self) {
        if self.engine.is_none() || self.reconnect.is_some() {
            return; // 本来就没在连，或已经在等按名字重连
        }
        if self.ble_fail_streak < 6 {
            return;
        }
        if self.last_rx_at.elapsed() < Duration::from_secs(5) {
            return;
        }
        if self.last_ble_retry.elapsed() < Duration::from_secs(8) {
            return;
        }
        if self.tcp.as_ref().is_some_and(|t| t.has_link()) {
            self.ble_fail_streak = 0; // 局域网还在，蓝牙不值得为它拆链路
            return;
        }
        if self.ble_heal_attempts >= 3 {
            return; // 已经报过终态，等用户操作
        }
        // 认"名字"不认"地址"：手机每次唤醒/重启 App 都会换一个新的可解析随机地址，拿缓存地址重连必然连不上。
        // 展示用扫描名（列表里真有的那个），匹配用两个名字都认（见 `reconnect` 的注释）
        // 落库名要读盘，所以放在锁外：worker 按着 `UiState` 锁做 IO 会一起拖住界面线程
        let (scan, fp, hello) = {
            let st = self.state.lock().unwrap();
            let scan = st.selected.and_then(|a| {
                st.devices
                    .iter()
                    .rev()
                    .find(|(addr, _)| *addr == a)
                    .map(|(_, n)| n.clone())
            });
            (scan, st.peer_fp.clone(), st.peer_name.clone())
        };
        let persisted = fp.as_deref().and_then(|want| {
            identity::load_peer_names()
                .into_iter()
                .find(|(f, _)| f == want)
                .map(|(_, n)| n)
        });
        let v = reconnect_names(scan.as_deref(), persisted.as_deref(), &hello);
        let names = v.clone();
        let name = scan
            .filter(|n| !n.trim().is_empty())
            .or_else(|| v.first().cloned());
        let Some(name) = name else {
            // 连名字都没有就没法按名字等广播，只能报清楚，别默默重试
            self.ble_heal_attempts = 3;
            self.last_ble_retry = Instant::now();
            self.ble_fail_streak = 0;
            self.push_error(
                "蓝牙链路无响应，且不知道对端叫什么（无法自动重连）：请在「连接」页点一次设备"
                    .to_string(),
            );
            return;
        };
        self.ble_heal_attempts += 1;
        self.last_ble_retry = Instant::now();
        self.ble_fail_streak = 0;
        // 先把死句柄丢掉，停止往它写；完整的拆建交给 resolve_reconnect 里那次真重连
        self.ble.abort_inflight();
        self.ble.invalidate_link();
        if self.ble_heal_attempts == 3 {
            self.push_error(format!(
                "蓝牙链路多次无响应，正在最后一次尝试等「{name}」重新广播；\
                 还不行请确认手机开着 LinkX 并靠近电脑"
            ));
        } else {
            self.push_error(format!(
                "蓝牙链路无响应，正在等「{name}」重新广播后重连（第 {} 次，期间文件传输会中断）",
                self.ble_heal_attempts
            ));
        }
        self.reconnect = Some((name, names));
        self.reconnect_deadline = Some(Instant::now() + BLE_RECONNECT_WAIT);
    }

    /// 等对端带着**新地址**重新出现在扫描结果里，然后按那个地址重连。
    /// 扫描回调在 WinRT 线程上，只能往设备表里塞数据；真正的连接必须在本线程做。
    fn resolve_reconnect(&mut self) {
        let Some((name, names)) = self.reconnect.clone() else {
            return;
        };
        if self.reconnect_deadline.is_some_and(|d| Instant::now() >= d) {
            self.reconnect = None;
            self.reconnect_deadline = None;
            // 自愈窗口用完了，可手机可能一小时后才回来：把自动拨号重新武装，一出现在扫描列表里就拨。
            // 文案跟着真实行为走，不许继续说"请你点一次"。
            self.arm_auto_dial("heal-gave-up");
            let still_waiting = self.auto_dial.is_some();
            self.push_error(format!(
                    "没等到「{name}」重新广播：{}",
                    if still_waiting {
                        "仍在后台等它出现，一出现就自动连（不想自动连可到设置页关掉「自动连接已绑定设备」）"
                    } else {
                        "请把手机上的 LinkX 切到前台，再在「连接」页点一次设备"
                    }
                ),
            );
            return;
        }
        let addr = freshest_addr_for(&self.state.lock().unwrap().devices, &names);
        let Some(addr) = addr else { return };
        self.reconnect = None;
        self.reconnect_deadline = None;
        self.reconnect_ble(addr);
    }

    /// 武装/刷新自动拨号名单：冷启动、配对成功、自愈放弃三处调用。名单每次从磁盘重读
    /// （这三处都是低频点），所以"刚配好的第二台手机"下一次冷启动就在名单里。
    fn arm_auto_dial(&mut self, why: &str) {
        let on = self.state.lock().unwrap().auto_connect;
        let names = if on && !self.trusted.is_empty() {
            auto_dial_names(&self.trusted, &identity::load_peer_names())
        } else {
            Vec::new()
        };
        // 名单没变就别刷日志：每次成功重连都会回到这里重武装一遍，全记下来只剩噪声
        let changed = self.auto_dial.as_deref() != Some(names.as_slice());
        self.auto_dial = (!names.is_empty()).then_some(names.clone());
        self.auto_dial_fails = 0;
        self.auto_dial_not_before = None;
        if changed && !names.is_empty() {
            let joined = names.join(",");
            debuglog::log(
                debuglog::Level::Info,
                "link",
                "autodial.arm",
                &[("names", &joined), ("why", why)],
            );
        }
    }

    /// 没有会话时，等已绑定设备出现在扫描结果里就自动拨它。覆盖两档，同一句用户口径——"不要让我
    /// 每次都手动点重连"：**冷启动**（手机还在解锁/开机）与**运行中掉线、自愈放弃之后**。
    /// 出口只有 `reconnect_ble(addr)` 一个，与用户点设备行、与卡死自愈**同一条**生产路径（不许为
    /// 自动化另造一套连接逻辑）；这里只决定"什么时候自动走那条路"，不新增任何自动放行：
    /// 身份仍然由引擎按 RSA 指纹判。
    /// 四条收手条件：
    /// - 自愈正在等广播，或引擎还活着且链路没静默满 30 s → 不插手（名单保留）。掉线超过 30 s 而
    ///   引擎仍挂在 Reconnecting 里空发 HELLO 的是**僵尸引擎**，要让它下台；
    /// - 正在等用户核对 SAS 或裁决新身份 → 停手，否则被拒一次又会立刻弹一次关不掉的窗；
    /// - 两次尝试之间隔 [`AUTO_DIAL_RETRY`]，连续三次拨不上就把话说明白交给用户，不后台空转；
    /// - 开关关掉 → 当场 disarm（这一轮就生效，不等下次启动）。
    fn resolve_auto_dial(&mut self) {
        // 自愈正在等广播 → 不插手（它比这条更懂"刚掉线"这件事）
        if self.reconnect.is_some() {
            return;
        }
        // 引擎还在且**最近 30 s 内收到过字节** → 不插手。判据用"收没收到东西"而不是"写特征
        // 句柄在不在"（句柄在 ≠ 链路活着）；僵尸引擎的下台见函数头注释。`reconnect_ble` 本来
        // 就会先 `user_disconnect()` 再重建，这里不是新语义。
        if self.engine.is_some() && self.last_rx_at.elapsed() < BLE_RECONNECT_WAIT {
            return;
        }
        let Some(names) = self.auto_dial.clone() else {
            return;
        };
        if self
            .auto_dial_not_before
            .is_some_and(|t| Instant::now() < t)
        {
            return;
        }
        let (addr, still_on, waiting_human) = {
            let st = self.state.lock().unwrap();
            (
                freshest_addr_for(&st.devices, &names),
                st.auto_connect,
                st.sas.is_some() || st.identity_change.is_some(),
            )
        };
        if !still_on || waiting_human {
            self.auto_dial = None;
            return;
        }
        let Some(addr) = addr else { return };
        self.auto_dial_not_before = Some(Instant::now() + AUTO_DIAL_RETRY);
        let addr_hex = format!("{addr:012x}");
        debuglog::log(
            debuglog::Level::Info,
            "link",
            "autodial.dial",
            &[("addr", &addr_hex)],
        );
        self.reconnect_ble(addr);
        if self.engine.is_none() {
            // 广播是刚过期的旧地址：连不上是常态（手机刚好又睡了），但要有限地试
            self.auto_dial_fails += 1;
            if self.auto_dial_fails >= 3 {
                self.auto_dial = None;
                self.push_error(
                    "自动连接试了三次都没成功：请在「连接」页点一次设备，\
                     或到设置页关掉「自动连接已绑定设备」"
                        .to_string(),
                );
            }
        }
    }

    fn pump_ui_requests(&mut self) {
        let req = {
            let mut st = self.state.lock().unwrap();
            take_ui_requests(&mut st)
        };

        if let Some(addr) = req.connect {
            // 用户手动点的优先级最高：清掉自愈的等待，否则稍后解析到新广播会再拆一次链路
            self.reconnect = None;
            self.reconnect_deadline = None;
            self.reconnect_ble(addr);
        }

        // 引擎不在（还没配对完 / 正在重连）时下面整块都不执行，而这些请求已经被 take 走了：
        // 不出声就等于用户看到的"点了没反应"。兜底保证每次操作都有回应。
        if self.engine.is_none()
            && (req.media_cmd.is_some()
                || req.send_clip.is_some()
                || req.send_file
                || req.cancel_file.is_some()
                || req.notify_reply.is_some()
                || req.album_list.is_some()
                || req.album_full.is_some()
                || !req.album_drag.is_empty()
                || req.album_thumb_one.is_some())
        {
            self.push_error("操作没有执行：本机现在没有已配对的连接".to_string());
        }

        if let Some(eng) = self.engine.as_mut() {
            if req.confirm_sas {
                eng.confirm_sas();
            }
            if req.reject_sas {
                eng.reject_sas();
            }
            // 用户对「对端新身份」的裁决必须回灌引擎：检测到指纹变化却没有决策入口时，
            // 引擎停在 Repaired 等不到 ReAccepted/SasRejected → 双端都不推进。
            if req.accept_identity {
                eng.accept_new_fingerprint();
            }
            if req.reject_identity {
                // 拒绝 = 不更新信任库并断开（引擎走 SasRejected → CLOSED）
                eng.reject_sas();
            }
            if let Some(text) = req.send_clip {
                let sent = if crate::features::enabled(crate::features::Module::Clipboard) {
                    eng.send_clipboard_text(&text, now_ms())
                } else {
                    self.state
                        .lock()
                        .unwrap()
                        .push_error("剪贴板同步已在设置中关闭（重启后生效）".to_string());
                    false
                };
                if sent {
                    self.state.lock().unwrap().clip_out = text;
                } else if crate::features::enabled(crate::features::Module::Clipboard) {
                    // 与播放指令、通知回复同一条纪律：没发出去必须当场说一句，否则用户点了
                    // 「立即同步」什么也不会发生，唯一线索是屏幕上没有线索。
                    self.state
                        .lock()
                        .unwrap()
                        .push_error("剪贴板未发出：尚未配对，或链路未就绪".to_string());
                }
            }
            // 播放指令与剪贴板同一套处理：开关关了就报可读错误，否则"点了没反应"时唯一线索是什么都没发生。
            if let Some((action, volume, delta)) = req.media_cmd {
                let sent = if crate::features::enabled(crate::features::Module::MediaControl) {
                    eng.send_media_command(action, volume, delta, now_ms())
                } else {
                    self.state
                        .lock()
                        .unwrap()
                        .push_error("媒体控制已在设置中关闭（重启后生效）".to_string());
                    false
                };
                if !sent && crate::features::enabled(crate::features::Module::MediaControl) {
                    self.state
                        .lock()
                        .unwrap()
                        .push_error("播放指令未发出：尚未配对，或链路未就绪".to_string());
                }
            }
            // 通知回复：与播放指令同一条纪律 —— 这条会真的把文字送进对端某个应用，
            // 发不出去时必须当场落到那条回复的提示上，而不是只留一行日志。
            if let Some(r) = req.notify_reply {
                let sent = if crate::features::enabled(crate::features::Module::Notifications) {
                    eng.send_notify_reply(
                        &NotificationReply {
                            reply_id: r.reply_id,
                            package: r.target.package,
                            tag: r.target.tag,
                            notification_id: r.target.notification_id,
                            action_index: r.target.action_index,
                            result_key: r.target.result_key,
                            text: r.text,
                        },
                        now_ms(),
                    )
                } else {
                    false
                };
                if !sent {
                    self.state.lock().unwrap().apply_reply_ack(
                        r.reply_id,
                        false,
                        "回复未发出：通知同步已关闭，或未配对/链路未就绪",
                    );
                }
            }
        }
        if req.accept_identity || req.reject_identity {
            // 提示是一次性决策：用户已表态 → 立即收起（SAS 复核页或断开随后呈现）
            self.state.lock().unwrap().identity_change = None;
            post_state_changed(self.hwnd_raw);
        }

        if req.unbind {
            self.unbind();
        }
        if req.manual_ip {
            self.add_manual_peer();
        }
        if req.send_file {
            // 运行期功能开关：文件传输未加载时，在**唯一的排队入口**上拒掉——四个入口（文件页按钮、启动参数、
            // `linkx.exe <路径>` 转交、控制面 `/action/send-file`）都汇到这里，在这里拦一次就不会出现
            // "已排进队列、但 TCP 通道永不绑定 → 任务无限停在『等待 TCP 通道』"。
            if !crate::features::enabled(crate::features::Module::FileTransfer) {
                self.push_error("文件传输已关闭：请到「功能」页重新开启并重新启动".to_string());
            } else {
                let path = self
                    .state
                    .lock()
                    .unwrap()
                    .send_path_input
                    .trim()
                    .to_string();
                if path.is_empty() {
                    self.push_error("请先填写要发送的文件路径".to_string());
                } else if let Some(busy) = &self.pending_send {
                    // 出站一次只有一条任务（引擎侧也是单 `SendTask`）：静默顶掉前一条，前一条的
                    // 界面行会永远停在"等待 TCP 通道"，两端都以为还在传。
                    self.push_error(format!("已经在等「{busy}」发出去了：一条传完才能排下一条"));
                } else {
                    self.pending_send = Some(path);
                }
            }
        }
        if let Some((dir, name)) = req.cancel_file {
            self.cancel_task(dir, &name);
        }
        // ---- 相册：三条请求只走局域网 TCP，引擎在未绑定时会自己报错并回 false。
        // `false` 必须落到界面上（"点了没反应"是这里最坏的失败形态）。----
        if let Some((page, per)) = req.album_list {
            self.request_album_list(page, per);
        }
        if let Some(ids) = req.album_full {
            self.request_album_full(&ids);
        }
        if !req.album_drag.is_empty() {
            self.request_album_drag(&req.album_drag);
        }
        if let Some(id) = req.album_thumb_one {
            // 与视口排队共用同一条队列与同一个并发上限，只是绕开"可见才请求"
            let mut st = self.state.lock().unwrap();
            if st.album.needs_thumb(id) && !st.album.req_thumbs.contains(&id) {
                st.album.req_thumbs.push(id);
            }
        }
    }

    // ---------- 相册：请求发出与应答落状态 ----------

    /// 相册请求的共同前提。返回 `Err(文本)` = 这条请求注定发不出去，调用方**必须**把文本交给用户：
    /// 模块关了 / 没配对 / TCP 没绑定，三种情况的下一步动作完全不同，合并成一句"失败了"就等于没说话。
    fn album_gate(&self) -> Result<(), String> {
        if !crate::features::enabled(crate::features::Module::Album) {
            return Err("相册已在「功能」页关闭：重新开启并重启后才能取照片".to_string());
        }
        let paired = self.engine.as_ref().map(|e| e.is_paired()).unwrap_or(false);
        if !paired {
            return Err("相册请求没有发出：还没连上手机（在「连接」页点一下设备）".to_string());
        }
        let bound = self
            .engine
            .as_ref()
            .map(|e| e.is_tcp_bound())
            .unwrap_or(false);
        if !bound {
            return Err(
                "相册请求没有发出：局域网 TCP 通道未就绪（相册只走 TCP，不降级蓝牙）".to_string(),
            );
        }
        Ok(())
    }

    fn request_album_list(&mut self, page: u32, per_page: u32) {
        if let Err(why) = self.album_gate() {
            let mut st = self.state.lock().unwrap();
            st.album.loading = false;
            st.push_error(why);
            st.ui_rev += 1;
            drop(st);
            post_state_changed(self.hwnd_raw);
            return;
        }
        let sent = self
            .engine
            .as_mut()
            .map(|e| e.send_album_list_request(page, per_page, now_ms()))
            .unwrap_or(false);
        let mut st = self.state.lock().unwrap();
        if sent {
            st.album.loading = true;
            st.album.error.clear();
            st.album.page = page;
            st.album.per_page = per_page;
            // 换页 = 换一叠照片：滚动位置留在上一页的中段，会让人觉得"新页少了开头几张"
            st.album.scroll_row = 0;
            // 回到第 0 页 = 用户主动要一次新数据：把失败格清掉让它们重新排队。平时不自动重试
            // （坏图会每帧再要一次），但链路抖动造成的超时也不该被记成永久失败、让界面一直红着。
            if page == 0 {
                st.album.drop_failed_thumbs();
            }
            // 新页 = 新代际：上一页仍在途的应答不能再闪进这一页
            st.album.invalidate();
        } else {
            st.album.loading = false;
            st.push_error("相册清单请求未发出：通道在按下的一瞬间断了，请重试".to_string());
        }
        st.ui_rev += 1;
        drop(st);
        post_state_changed(self.hwnd_raw);
    }

    /// 导出：向手机要这一批原图。落盘目录与 id 集合由 UI 在按下按钮时登记进
    /// `album.routes` / `album.export_dir`（worker 只负责发，两处分派必然漂）。
    fn request_album_full(&mut self, ids: &[u64]) {
        if ids.is_empty() {
            self.push_error("还没有选中照片：先在格子上点一下再导出".to_string());
            return;
        }
        if let Err(why) = self.album_gate() {
            let mut st = self.state.lock().unwrap();
            for id in ids {
                st.album.routes.remove(id);
            }
            st.push_error(why);
            st.ui_rev += 1;
            drop(st);
            post_state_changed(self.hwnd_raw);
            return;
        }
        let sent = self
            .engine
            .as_mut()
            .map(|e| e.send_album_full_request(ids, now_ms()))
            .unwrap_or(false);
        let mut st = self.state.lock().unwrap();
        if sent {
            let body = format!("已请求 {} 张，落盘目录：{}", ids.len(), st.album.export_dir);
            st.push_toast("正在导出相册原图".to_string(), body);
        } else {
            // 没发出去就把路由撤掉：留着会让稍后到手的同名请求被误认成本次导出
            for id in ids {
                st.album.routes.remove(id);
            }
            st.push_error("原图请求未发出：局域网通道在按下的一瞬间断了，请重试".to_string());
        }
        st.ui_rev += 1;
        drop(st);
        post_state_changed(self.hwnd_raw);
    }

    /// 拖出：取一张原图到临时目录。等待方是 **UI 线程**（OLE 拖放必须在按下之后的那条消息里同步发起），任何失败路径都要把 `drag_result` 填上，否则界面干等到超时。
    fn request_album_drag(&mut self, ids: &[u64]) {
        // 多选拖出时每一张都要各自作废：只报第一条的话，剩下的格子会一直转圈等到用户以为还在取。
        let first = ids.first().copied();
        if let Err(why) = self.album_gate() {
            let mut st = self.state.lock().unwrap();
            for id in ids {
                st.album.routes.remove(id);
                st.album.clear_drag_progress(*id);
            }
            if let Some(id) = first {
                st.album.drag_result = Some((id, Err(why.clone())));
            }
            st.push_error(why);
            st.ui_rev += 1;
            drop(st);
            post_state_changed(self.hwnd_raw);
            return;
        }
        let sent = self
            .engine
            .as_mut()
            .map(|e| e.send_album_full_request(ids, now_ms()))
            .unwrap_or(false);
        if !sent {
            let mut st = self.state.lock().unwrap();
            for id in ids {
                st.album.routes.remove(id);
                st.album.clear_drag_progress(*id);
            }
            if let Some(id) = first {
                st.album.drag_result = Some((
                    id,
                    Err("原图请求未发出：局域网通道在按下的一瞬间断了".to_string()),
                ));
            }
            st.ui_rev += 1;
            drop(st);
            post_state_changed(self.hwnd_raw);
        }
    }

    /// 每轮：给可见格子补要缩略图（并发上限 + FIFO），并把超时/换代/已应答的在途清掉。
    fn album_pump(&mut self) {
        if !crate::features::enabled(crate::features::Module::Album) {
            if !self.album_inflight.is_empty() {
                self.album_inflight.clear();
                self.state.lock().unwrap().album.inflight = 0;
            }
            return;
        }
        let mut st = self.state.lock().unwrap();
        let changed = st.active_tab != crate::render::TAB_ALBUM;
        if changed {
            // 离开相册页：在途全部作废（"关掉就没有"也包括不再占带宽）
            self.album_inflight.clear();
            st.album.inflight = 0;
            st.album.visible_count = 0;
        } else {
            let now = Instant::now();
            let mut expired: Vec<u64> = Vec::new();
            let gen = st.album.generation;
            self.album_inflight.retain(|(g, id, at)| {
                if *g != gen {
                    return false;
                }
                if now.duration_since(*at) > crate::state::ALBUM_THUMB_TIMEOUT {
                    expired.push(*id);
                    return false;
                }
                true
            });
            for id in expired {
                st.album.put_thumb(
                    id,
                    crate::state::ThumbSlot::Failed("手机没有回这张缩略图（超时）".to_string()),
                );
            }
            st.album.inflight = self.album_inflight.len();
        }
        let inflight_before = self.album_inflight.len();
        let mut sent_any = false;
        if !changed {
            let gen = st.album.generation;
            // 排队：只给"画得出来的格子"下载（看不见的照片不该占带宽与内存），FIFO
            let queued: Vec<u64> = st
                .album
                .visible_ids()
                .iter()
                .map(|i| i.id)
                .filter(|id| st.album.needs_thumb(*id) && !st.album.req_thumbs.contains(id))
                .collect();
            for id in queued {
                if self.album_inflight.len() >= crate::state::ALBUM_THUMB_INFLIGHT {
                    break;
                }
                if st.album.thumbs.contains_key(&id) {
                    continue;
                }
                let ok = self
                    .engine
                    .as_mut()
                    .map(|e| {
                        e.send_album_thumb_request(id, crate::state::ALBUM_THUMB_EDGE, now_ms())
                    })
                    .unwrap_or(false);
                if ok {
                    self.album_inflight.push((gen, id, Instant::now()));
                    st.album.mark_thumb_pending(id);
                    sent_any = true;
                } else {
                    st.album.put_thumb(
                        id,
                        crate::state::ThumbSlot::Failed("通道未就绪，缩略图没有发出".to_string()),
                    );
                    sent_any = true;
                }
            }
            st.album.inflight = self.album_inflight.len();
        }
        if sent_any || st.album.inflight != inflight_before || changed {
            st.ui_rev += 1;
        }
        let need_post = sent_any || st.album.inflight != inflight_before;
        drop(st);
        if need_post {
            post_state_changed(self.hwnd_raw);
        }
    }

    /// `AlbumPage`：一页清单到手。**只认我正在等的那一页**——迟到的上一页清单若照收，界面就会"翻页翻乱"。
    fn on_album_page(
        &mut self,
        items: Vec<linkx_protocol::pb::AlbumItem>,
        page: u32,
        total: u32,
        error: String,
    ) {
        let mut st = self.state.lock().unwrap();
        if page != st.album.page {
            debuglog::log!(
                debuglog::Level::Warn,
                "album",
                "stale_page_dropped",
                &[
                    ("page", &page.to_string()),
                    ("want", &st.album.page.to_string())
                ]
            );
            return;
        }
        st.album.loading = false;
        st.album.items = items
            .into_iter()
            .map(|i| crate::state::AlbumItemView {
                id: i.id,
                name: i.name,
                size_bytes: i.size_bytes,
                mtime_ms: i.mtime_ms,
                width: i.width,
                height: i.height,
                kind: i.kind,
                duration_ms: i.duration_ms,
            })
            .collect();
        st.album.total = total;
        // 手机侧的 error 逐字保留：权限没给 / 相册为空 / 手机读失败是三件不同的事
        st.album.error = error;
        // 换了页 → 选中态清空：列表下标与照片都变了，留着选中会让"导出(3)"导出的是另一页的照片。
        st.album.selected.clear();
        st.ui_rev += 1;
        drop(st);
        post_state_changed(self.hwnd_raw);
    }

    /// `AlbumThumb`：一张缩略图到手（只进内存）。
    fn on_album_thumb(&mut self, id: u64, jpeg: Vec<u8>, error: String) {
        let mut st = self.state.lock().unwrap();
        let pos = self.album_inflight.iter().position(|(_, i, _)| *i == id);
        let Some(pos) = pos else {
            // 不在途的应答（手机在回一页以前被删掉的请求）：丢弃，但必须留痕——"手机发了、电脑没显示"查下去全靠这条日志。
            debuglog::log!(
                debuglog::Level::Warn,
                "album",
                "thumb_not_inflight",
                &[("id", &id.to_string()), ("bytes", &jpeg.len().to_string())]
            );
            return;
        };
        let (gen, _, _) = self.album_inflight.remove(pos);
        st.album.inflight = self.album_inflight.len();
        if gen != st.album.generation {
            // 上一页的应答迟到了：丢掉，但别把它画到这一页
            st.ui_rev += 1;
            drop(st);
            post_state_changed(self.hwnd_raw);
            return;
        }
        if !error.is_empty() {
            st.album
                .put_thumb(id, crate::state::ThumbSlot::Failed(error));
        } else if jpeg.is_empty() {
            st.album.put_thumb(
                id,
                crate::state::ThumbSlot::Failed("手机回了一张空的缩略图".to_string()),
            );
        } else {
            st.album
                .put_thumb(id, crate::state::ThumbSlot::Ready(Arc::new(jpeg)));
        }
        st.ui_rev += 1;
        drop(st);
        post_state_changed(self.hwnd_raw);
    }

    /// 一张相册原图有了结局（成功落盘 / 失败 / 被取消）：注销路由、把结果交给等待方。
    /// `album_id == 0` 是普通收文件，与本函数无关。拖出的等待方是 **UI 线程**，所以
    /// **失败路径也必须调用**，否则界面会一直干等到超时才说得出原因。
    fn album_settled(&mut self, album_id: u64, ok: bool, detail: String) {
        if album_id == 0 {
            return;
        }
        let mut st = self.state.lock().unwrap();
        let Some(route) = st.album.routes.remove(&album_id) else {
            st.album.clear_drag_progress(album_id);
            return;
        };
        // 有结局就摘掉进度：留着那一格会一直转，而"转完=可以拖"是这环唯一的含义
        st.album.clear_drag_progress(album_id);
        match route {
            crate::state::AlbumPurpose::Drag => {
                // 成功的那一份先留着：等不到它的拖出（大视频）要靠它做到"再拖一次立刻出去"
                if ok {
                    st.album.keep_drag(album_id, detail.clone());
                }
                st.album.drag_result = Some((
                    album_id,
                    if ok {
                        Ok(detail.clone())
                    } else {
                        Err(detail.clone())
                    },
                ));
            }
            crate::state::AlbumPurpose::Export => {
                if ok {
                    st.push_toast("相册原图已导出".to_string(), detail.clone());
                } else {
                    st.push_error(format!("相册原图导出失败: {detail}"));
                }
            }
        }
        st.ui_rev += 1;
        drop(st);
        post_state_changed(self.hwnd_raw);
    }

    // ---------- 2) 配对后启动 TCP / 发现；断开时由事件侧拆除 ----------

    fn ensure_channels(&mut self) {
        let paired = self.engine.as_ref().map(|e| e.is_paired()).unwrap_or(false);
        if !paired {
            return; // 未配对一律不开口（绑定须在已认证通道上完成）
        }

        // 跨端配置同步：配对即推一次；必须排在文件传输门控**之前**——配置走 BLE 就能到，挂在 UDP/TCP 之后会让"关掉文件传输"连带把通知/剪贴板设置也同步不过去。
        if !self.config_sent {
            self.send_config_once();
        }

        // 运行期功能开关：关掉"文件传输"就不建 UDP 发现与 TCP 服务端——这两套是文件传输的承载，
        // 也是本进程里最主要的线程与缓冲开销来源；通知与剪贴板只走 BLE，不受影响。
        if !crate::features::enabled(crate::features::Module::FileTransfer) {
            return;
        }

        // UDP 发现：Windows 主动广播，Android 据此学到本机 IP（双向 pump 收对端信标）
        if self.discovery.is_none() && !self.retry_pending(self.discovery_failed_at) {
            match UdpDiscovery::bind(DISCOVERY_UDP_PORT) {
                Ok(d) => {
                    self.discovery = Some(d);
                    crate::say(format!("[LinkX] UDP 发现已启动 :{DISCOVERY_UDP_PORT}"));
                }
                Err(e) => {
                    self.discovery_failed_at = Some(Instant::now());
                    self.push_error(format!("UDP 发现启动失败: {e}"));
                }
            }
        }

        // TCP 服务端：accept 一条连接后发起绑定
        if self.tcp.is_none() && !self.retry_pending(self.tcp_start_failed_at) {
            match TcpService::start() {
                Ok(s) => self.tcp = Some(s),
                Err(e) => {
                    self.tcp_start_failed_at = Some(Instant::now());
                    self.push_error(e);
                }
            }
        }
    }

    /// 失败退避判定：`None` = 从未失败过，可以尝试
    fn retry_pending(&self, failed_at: Option<Instant>) -> bool {
        failed_at.is_some_and(|t| t.elapsed() < CHANNEL_RETRY)
    }

    fn teardown_channels(&mut self) {
        self.discovery = None;
        self.discovery_failed_at = None;
        if let Some(t) = self.tcp.take() {
            t.stop();
        }
        self.tcp_bind_started = false;
        self.tcp_live_conn = 0;
        self.tcp_start_failed_at = None;
        self.config_sent = false;
        self.state.lock().unwrap().tcp_ready = false;
    }

    // ---------- 3/4) 入站：BLE 分片包 + TCP 完整帧 ----------

    fn pump_ble_rx(&mut self) {
        let Some(eng) = self.engine.as_mut() else {
            return;
        };
        loop {
            let pkt = self.rx_ble.lock().unwrap().pop_front();
            match pkt {
                Some(p) => {
                    #[cfg(feature = "agent-debug")]
                    linkx_debugd::bump("ble_rx_fed", 1);
                    self.last_rx_at = Instant::now();
                    eng.feed(&p, Instant::now())
                }
                None => break,
            }
        }
    }

    fn pump_tcp_rx(&mut self) {
        // 1) 新连接 → 发起绑定（必须早于喂帧，否则 CHANNEL_BIND 因 binding=None 被丢弃）
        if let Some((id, addr)) = self
            .tcp
            .as_ref()
            .and_then(|t| t.pending_accept.lock().unwrap().take())
        {
            // 换连接即视为一次全新的绑定：先丢掉上一条连接遗留的关闭事件，否则它会把刚建好的新链路 `close_link()` 掉。
            if id != self.tcp_live_conn {
                self.tcp_live_conn = id;
                if let Some(t) = self.tcp.as_ref() {
                    let mut g = t.closed.lock().unwrap();
                    if g.as_ref().is_some_and(|(cid, _)| *cid != id) {
                        g.take();
                    }
                }
            }
            let started = self
                .engine
                .as_mut()
                .map(|e| e.begin_tcp_binding(BindRole::TcpServer))
                .unwrap_or(false);
            if started {
                self.tcp_bind_started = true;
                self.state.lock().unwrap().peer_lan_ip = addr.ip().to_string();
                crate::say(format!("[LinkX] TCP 通道绑定已发起（对端 {addr} #{id}）"));
            } else {
                // 对端已经连进端口了，本端却因为"没有引擎 / 还没配对"没法发起绑定：这一句不回、
                // 这条线不收，对端就抱着一永远等不到 CHANNEL_BIND 应答的 socket 干等，而界面上
                // 一切正常。放掉句柄并说一句为什么 —— 读线程会在下一个读超时里把 socket 关掉。
                self.push_error(format!(
                    "{addr} 连上了局域网端口，但本机还不能建立通道（未配对或引擎未就绪）"
                ));
                if let Some(t) = self.tcp.as_ref() {
                    t.close_link();
                }
            }
        }

        // 2) 断开上报（读线程/写失败只报一次真实原因）
        if let Some((id, reason)) = self
            .tcp
            .as_ref()
            .and_then(|t| t.closed.lock().unwrap().take())
        {
            if id != self.tcp_live_conn {
                // 陈旧事件：这条连接早已被更新的连接取代，处理它只会误拆新链路。
                crate::say(format!(
                    "[LinkX] 忽略旧 TCP 连接 #{id} 的断开事件: {reason}"
                ));
            } else {
                if let Some(eng) = self.engine.as_mut() {
                    eng.on_tcp_closed(reason.clone());
                }
                self.tcp_bind_started = false;
                self.tcp_live_conn = 0;
                let mut st = self.state.lock().unwrap();
                st.tcp_ready = false;
                st.push_error(format!("TCP 通道断开: {reason}"));
            }
        }

        // 3) 入站帧 → 引擎（绑定发起前不喂，避免会话中段丢 nonce）
        if !self.tcp_bind_started {
            return;
        }
        loop {
            // 引擎的分块积压到预算就**先停手**，把帧留在读队列里。不停手的话，对端能在本机一次
            // 都还没落盘之前把整个文件搬进本进程内存——既是内存红线问题，也是"读队列满了就丢最旧"
            // 丢帧的诱因。停手后压力顺着读队列 → 读线程 → socket 缓冲区回到对端，速度自然落到
            // 本机真正吃得下的那一点上，一个字节都不少。
            let backlog = self
                .engine
                .as_ref()
                .map(|e| e.in_chunks_backlog())
                .unwrap_or(0);
            if backlog >= INBOUND_CHUNK_BYTES_MAX {
                // 不再单独埋点：读线程那条 tcp.rx.backpressure 已证明整条链路在受压，两处都记就变成每秒几十行日志。
                break;
            }
            let entry = self
                .tcp
                .as_ref()
                .and_then(|t| t.rx.lock().unwrap().pop_front());
            let Some((id, frame)) = entry else { break };
            // 旧连接残留的帧必须丢掉：把它们喂进本次绑定，等于让上一条连接的 CHANNEL_BIND 冒充
            // 本次应答（真机上 Peripheral 侧已 bound、Central 侧却永远绑不上，就是混队列造成的）。
            if id != self.tcp_live_conn {
                // 陈旧事件：这条连接早已被更新的连接取代，处理它只会误拆新链路。
                debuglog::log!(
                    debuglog::Level::Warn,
                    "lan",
                    "tcp.rx.drop_stale",
                    &[
                        ("conn", &id.to_string()),
                        ("live", &self.tcp_live_conn.to_string())
                    ]
                );
                continue;
            }
            if let Some(eng) = self.engine.as_mut() {
                // 心跳每 5 秒一帧，逐帧埋点会把环形缓冲里真正有信息量的事件挤掉（验收取证时
                // 表现为"采不到分块/路由事件"），所以只给业务帧记一行。
                if frame.get(3).copied() != Some(linkx_protocol::msg_type::HEARTBEAT) {
                    debuglog::log!(
                        debuglog::Level::Info,
                        "lan",
                        "tcp.rx.frame",
                        &[
                            ("conn", &id.to_string()),
                            ("len", &frame.len().to_string()),
                            (
                                "type",
                                &frame.get(3).map(|t| t.to_string()).unwrap_or_default()
                            ),
                        ]
                    );
                }
                eng.feed_tcp(&frame, Instant::now());
                // 局域网帧同样是"对端还活着"的证据：只认 BLE 的话，手机 BLE 掉线但 TCP 还在发心跳时，界面会一直红着。
                self.last_rx_at = Instant::now();
            }
        }
        // 取走了位置就要唤醒读线程（它可能正卡在"等队列腾位置"上）。
        // 不通知也不会死锁——那边是带 100 ms 超时的等——但每帧白等一个周期。
        if let Some(t) = self.tcp.as_ref() {
            t.rx_room.notify_all();
        }
    }

    // ---------- 5/6) 出站：BLE 分片包 + TCP 完整帧 ----------

    fn flush_ble_out(&mut self) {
        // 非阻塞、带时间预算的写出循环：发起写即返回，下一轮再轮询状态。封顶次数挡不住阻塞——真机单次写
        // 能挂 30 秒，而 worker 是唯一线程，一停就把收包和心跳全饿死；超 `BLE_WRITE_TIME_BUDGET` 就把剩余片留在队列里让出本轮。
        let t0 = Instant::now();
        let mut issued = 0usize;
        loop {
            match self.ble.poll_inflight() {
                crate::ble_central::WritePoll::Pending => {
                    if t0.elapsed() >= BLE_WRITE_TIME_BUDGET {
                        break; // 链路慢/已死：不等，交还给主循环
                    }
                    std::thread::sleep(Duration::from_millis(2));
                    continue;
                }
                crate::ble_central::WritePoll::Done(Ok(()), _) => {
                    self.ble_fail_streak = 0;
                    #[cfg(feature = "agent-debug")]
                    linkx_debugd::bump("ble_write_ok", 1);
                }
                crate::ble_central::WritePoll::Done(Err(e), pkt) => {
                    self.ble_fail_streak += 1;
                    #[cfg(feature = "agent-debug")]
                    linkx_debugd::bump("ble_write_status_fail", 1);
                    // 失败片回塞队首：分片缺一片接收侧永远凑不齐，记了错误不等于没丢数据。
                    if let Some(eng) = self.engine.as_mut() {
                        eng.requeue_outbound_front(vec![pkt]);
                    }
                    let msg = format!("BLE 发送失败: {e}");
                    if self.last_send_err.as_deref() != Some(msg.as_str()) {
                        self.last_send_err = Some(msg.clone());
                        self.push_error(msg);
                    }
                    // 这里**不**当场作废句柄：一次写失败多半是瞬时抖动，"3 次即作废、之后没人再解析特征"
                    // 的写法会把瞬时失败变成永久断链。真正卡死的判据（连续失败 + 长时间收不到任何字节）
                    // 交给 [`Worker::heal_stalled_ble`]，它作废之后会立刻走同一条重连出口。
                    break;
                }
                crate::ble_central::WritePoll::Idle => {}
            }
            if issued >= BLE_WRITE_BUDGET_PER_ROUND || t0.elapsed() >= BLE_WRITE_TIME_BUDGET {
                break;
            }
            let Some(eng) = self.engine.as_mut() else {
                break;
            };
            let Some(pkt) = eng.take_outbound_n(1).pop() else {
                break;
            };
            if let Err(e) = self.ble.begin_write(pkt) {
                self.ble_fail_streak += 1;
                let msg = format!("BLE 发送失败: {e}");
                if self.last_send_err.as_deref() != Some(msg.as_str()) {
                    self.last_send_err = Some(msg.clone());
                    self.push_error(msg);
                }
                break;
            }
            issued += 1;
        }
    }

    fn flush_tcp_out(&mut self) {
        if !self.tcp_bind_started {
            return;
        }
        let Some(tcp) = self.tcp.as_ref() else {
            return;
        };
        // 连接尚未建立 / 已断开：不从引擎取帧（取了只能丢弃），留给下一轮
        if !tcp.has_link() {
            return;
        }
        let frames = match self.engine.as_mut() {
            Some(e) => e.take_tcp_outbound(),
            None => return,
        };
        if frames.is_empty() {
            return;
        }
        if let Err(e) = tcp.write_frames(&frames) {
            let reason = format!("TCP 写出失败: {e}");
            if let Some(eng) = self.engine.as_mut() {
                eng.on_tcp_closed(reason.clone());
            }
            self.tcp_bind_started = false;
            if let Some(t) = self.tcp.as_ref() {
                t.close_link();
            }
            let mut st = self.state.lock().unwrap();
            st.tcp_ready = false;
            st.push_error(reason);
        }
    }

    // ---------- 7) 引擎事件 → UiState ----------

    fn drain_events(&mut self) {
        let evs = match self.engine.as_mut() {
            Some(e) => e.take_events(),
            None => return,
        };
        if evs.is_empty() {
            return;
        }
        for ev in evs {
            self.handle_event(ev);
        }
        post_state_changed(self.hwnd_raw);
    }

    fn handle_event(&mut self, ev: EngineEvent) {
        match ev {
            EngineEvent::FileMetaReceived {
                file_id,
                name,
                size,
                chunk_size,
                sha256,
                crc32,
                album_id,
            } => self.begin_recv(file_id, name, size, chunk_size, sha256, crc32, album_id),
            EngineEvent::FileDoneReceived {
                file_id,
                ok,
                error,
                sha256,
            } => {
                // 引擎把「事件」和「文件分块」放在两条队列里（`events` vs `in_chunks`），而 worker 一轮里先
                // `drain_events` 再 `drain_chunks`。局域网快的时候 META + 全部分块 + DONE 在同一轮到达，DONE 会比
                // 它前面的分块先被处理——收尾时收件会话还在，分块随后全被判成 orphan_chunk 丢弃，文件留成 0 字节。
                // 所以处理 DONE 之前必须先把已到达的分块落进会话，而且**只能取这个 file_id 的分块**：一次收多张
                // 相册原图时，第一张的 DONE 若整队取走分块，第二张的分块会在自己的 META 之前被喂进来、全成孤儿。
                self.drain_chunks_for(file_id);
                // 若本端正有一条**发送**任务在等回执，这条就是那声"收到了"——发送侧永远等不到确认就会出现"假成功"。
                // 先认"本端正在收的那条"再认"正在发的那条"：file_id 两端各自生成、同一毫秒起算理论上会撞号，按在途会话归属判更稳。
                let has_recv = self.recv.iter().any(|r| r.file_id == file_id);
                let is_ack = !has_recv
                    && self
                        .send
                        .as_ref()
                        .map(|s| s.file_id == file_id)
                        .unwrap_or(false);
                if is_ack {
                    let name = self
                        .send
                        .as_ref()
                        .map(|s| s.name.clone())
                        .unwrap_or_default();
                    self.send = None;
                    let mut st = self.state.lock().unwrap();
                    if ok {
                        st.update_file_task(&name, TASK_DIR_SEND, 100, "已完成");
                        st.push_toast("文件已送达".to_string(), format!("{name}：对端校验通过"));
                    } else {
                        let reason = error.unwrap_or_else(|| "对端未说明原因".to_string());
                        st.update_file_task(&name, TASK_DIR_SEND, 0, "失败");
                        st.push_error(format!("对端拒收（{name}）: {reason}"));
                    }
                } else {
                    self.finish_recv(file_id, ok, error, sha256)
                }
            }
            EngineEvent::FileResumeRequested {
                file_id,
                from_index,
            } => self.on_resume_requested(file_id, from_index),
            EngineEvent::TcpBound => {
                {
                    let mut st = self.state.lock().unwrap();
                    st.tcp_ready = true;
                }
                // 绑定完成后重推一次配置，确保走 TCP（大消息路径）
                self.config_sent = false;
                self.send_config_once();
            }
            EngineEvent::TcpUnbound { reason } => {
                self.tcp_bind_started = false;
                // 关掉这条 TCP 连接：accept 线程回到监听，等对端重连后重走绑定
                if let Some(t) = self.tcp.as_ref() {
                    t.close_link();
                }
                let mut st = self.state.lock().unwrap();
                st.tcp_ready = false;
                st.push_error(format!("TCP 通道解绑: {reason}"));
            }
            EngineEvent::ConfigReceived { entries } => self.apply_config(&entries),
            // 相册只有这两条应答会到达电脑（电脑是发起方）。必须在 worker 侧消费而不是
            // `apply_event`：应答要与 worker 私有的在途队列 `(代际, id)` 对齐，纯 UI 函数拿不到那个队列。
            EngineEvent::AlbumPage {
                items,
                page,
                total,
                error,
            } => self.on_album_page(items, page, total, error),
            EngineEvent::AlbumThumb {
                id, jpeg, error, ..
            } => self.on_album_thumb(id, jpeg, error),
            // 文件任务被引擎硬失败（未绑定就发送 / 传输中 TCP 断开）：必须落到任务状态与错误流，
            // 这类失败只存在于日志里就等于没报。
            EngineEvent::FileTaskFailed { file_id, reason } => {
                let mine = self
                    .send
                    .as_ref()
                    .map(|s| s.file_id == file_id)
                    .unwrap_or(false);
                if mine {
                    let name = self
                        .send
                        .as_ref()
                        .map(|s| s.name.clone())
                        .unwrap_or_default();
                    self.send = None;
                    let mut st = self.state.lock().unwrap();
                    st.update_file_task(&name, TASK_DIR_SEND, 0, "失败");
                    st.push_error(format!("文件发送失败（{name}）: {reason}"));
                } else if let Some(idx) = self.recv.iter().position(|r| r.file_id == file_id) {
                    // 引擎在 TCP 断开时会把本机在途**接收**一并判失败。发送方有通道锁会自己收到失败通知，
                    // 接收方必须也落到结束态——否则那一行永远挂「接收中」，一台早就断开的电脑看起来还在等数据。
                    let r = self.recv.remove(idx);
                    self.fail_recv(r, &reason);
                }
            }
            // 用户取消（本端点的、还是对端发来的，都走这一条）。主动取消不是故障，显示成"失败"会让人
            // 以为链路坏了；而半截文件必须删掉，留着就像传成功了。
            EngineEvent::FileTaskCancelled { file_id, reason } => {
                if self.send.as_ref().is_some_and(|s| s.file_id == file_id) {
                    let name = self
                        .send
                        .as_ref()
                        .map(|s| s.name.clone())
                        .unwrap_or_default();
                    self.send = None;
                    let mut st = self.state.lock().unwrap();
                    st.update_file_task(&name, TASK_DIR_SEND, 0, "已取消");
                    st.push_toast("发送已取消".to_string(), format!("{name}：{reason}"));
                    return;
                }
                if let Some(idx) = self.recv.iter().position(|r| r.file_id == file_id) {
                    let r = self.recv.remove(idx);
                    let name = r.name.clone();
                    let path = r.path.clone();
                    let album_id = r.album_id;
                    // 先释放句柄再删：Windows 上"文件正被占用"删不掉，留半截假文件给用户更糟。
                    drop(r);
                    {
                        let mut st = self.state.lock().unwrap();
                        st.update_file_task(&name, TASK_DIR_RECV, 0, "已取消");
                        if fs::remove_file(&path).is_ok() {
                            st.push_toast("接收已取消".to_string(), format!("{name}：{reason}"));
                        } else {
                            st.push_error(format!(
                                "接收已取消（{name}），但残留文件没删掉，请手动清理：{}",
                                path.display()
                            ));
                        }
                    }
                    self.album_settled(album_id, false, format!("{name}：{reason}"));
                    return;
                }
                // 本机已没有这条会话：不静默吞，否则一端显示"已取消"、另一端显示"已完成"，两边都以为自己没错。
                self.push_error(format!(
                    "收到一条取消收尾（文件 {file_id:#x}），但本机已无对应的在途任务：{reason}"
                ));
            }
            EngineEvent::PeerPaired { fingerprint } => {
                let fp = fingerprint.clone();
                let mut st = self.state.lock().unwrap();
                // 无论首次配对还是「新身份复核通过」，都在此入库：引擎侧此时才把指纹加入信任库，
                // 平台侧同步落盘 → 下次免弹窗。
                let name = st.peer_name.clone();
                // 顺手记下"这个指纹当初是靠哪个广播名被扫到的"：信任库里存的是握手 TLV 的机型名，
                // 与扫描列表里的广播友好名不同源，冷启动自动拨号只能靠这条线索在列表里认人。
                let scan_name = st.selected.and_then(|a| {
                    st.devices
                        .iter()
                        .rev()
                        .find(|(addr, _)| *addr == a)
                        .map(|(_, n)| n.clone())
                });
                st.identity_change = None;
                apply_event(&mut st, EngineEvent::PeerPaired { fingerprint });
                st.remember_bound_device(&fp, &name);
                drop(st);
                identity::upsert_trusted_peer(&mut self.trusted, &fp, &name);
                identity::save_trusted_peers(&self.trusted);
                // 把"指纹 = 哪个广播名"落到线索文件（放锁之外做 IO）；只在配对成功时记——没配对成功的名字不值得下次开机去拨。
                if let Some(scan) = scan_name.as_deref() {
                    identity::save_peer_name(&fp, scan);
                }
                // 名单里可能刚多出这台设备（第一次配对）：立刻重读一次，别等下次启动
                self.arm_auto_dial("paired");
                {
                    let mut st = self.state.lock().unwrap();
                    st.bound_devices = self
                        .trusted
                        .iter()
                        .map(|p| (p.fingerprint.clone(), p.name.clone()))
                        .collect();
                }
            }
            // 同名设备呈递新身份 → 必须显式派发到 UI 决策
            EngineEvent::IdentityChanged {
                name,
                old_fingerprint,
                new_fingerprint,
            } => {
                let mut st = self.state.lock().unwrap();
                st.identity_change = Some(IdentityChangeView {
                    name: name.clone(),
                    old_fp: old_fingerprint.clone(),
                    new_fp: new_fingerprint.clone(),
                });
                st.push_error(format!(
                    "对端「{name}」身份已变化（{old_fingerprint} → {new_fingerprint}），等待确认"
                ));
            }
            EngineEvent::StateChanged { state } => {
                if state == state_code::CLOSED {
                    self.teardown_channels();
                    // 断开即收起身份变化提示（连接已不在，弹窗无意义）
                    self.state.lock().unwrap().identity_change = None;
                }
                self.state.lock().unwrap().conn_state = state;
            }
            other => {
                let mut st = self.state.lock().unwrap();
                apply_event(&mut st, other);
            }
        }
    }

    /// 跨端配置推送（归属口径：cross = 全局共享，per_peer = 每设备）
    fn send_config_once(&mut self) {
        let entries = {
            let st = self.state.lock().unwrap();
            vec![
                ConfigEntryItem {
                    key: "notify.toast.enabled".to_string(),
                    value: (st.toast_enabled as u8).to_string(),
                    scope: "cross".to_string(),
                },
                ConfigEntryItem {
                    key: "notify.toast.show_content".to_string(),
                    value: (st.toast_show_content as u8).to_string(),
                    scope: "cross".to_string(),
                },
                ConfigEntryItem {
                    key: "clip.sync".to_string(),
                    value: (st.clip_sync as u8).to_string(),
                    scope: "per_peer".to_string(),
                },
            ]
        };
        let ok = self
            .engine
            .as_mut()
            .map(|e| e.send_config(&entries, now_ms()))
            .unwrap_or(false);
        if ok {
            self.config_sent = true;
        }
    }

    /// 应用对端同步过来的配置（仅 `scope = cross | per_peer`）并持久化
    fn apply_config(&mut self, entries: &[ConfigEntryItem]) {
        let mut st = self.state.lock().unwrap();
        let mut changed = false;
        for e in entries {
            if e.scope != "cross" && e.scope != "per_peer" {
                continue; // 本机专属项不跨端（归属矩阵：cross/per_peer）
            }
            let on = e.value == "1"; // 与 settings.rs 同口径：垃圾值不算"开"
            match e.key.as_str() {
                "notify.toast.enabled" if st.toast_enabled != on => {
                    st.toast_enabled = on;
                    changed = true;
                }
                "notify.toast.show_content" if st.toast_show_content != on => {
                    st.toast_show_content = on;
                    changed = true;
                }
                "clip.sync" if st.clip_sync != on => {
                    st.clip_sync = on;
                    changed = true;
                }
                _ => {}
            }
        }
        // 出锁再写盘：`save()` 是 `fs::write`，慢盘/杀毒软件挂着能把 worker 每 200 ms 的锁请求全卡住（与系统对话框、导出日志同一口径）。
        if changed {
            let snap = settings::Settings::from_state(&st);
            drop(st);
            snap.save();
        }
    }

    // ---------- 8) 入站文件分块 → 落盘 ----------

    fn drain_chunks(&mut self) {
        let chunks = match self.engine.as_mut() {
            Some(e) => e.take_chunks(),
            None => return,
        };
        self.apply_chunks(chunks);
    }

    /// 只落 `file_id` 自己的分块（`FileDone` 收尾用），其余留在引擎队列里等各自的 META。
    fn drain_chunks_for(&mut self, file_id: u64) {
        let chunks = match self.engine.as_mut() {
            Some(e) => e.take_chunks_for(file_id),
            None => return,
        };
        self.apply_chunks(chunks);
    }

    fn apply_chunks(&mut self, chunks: Vec<linkx_session::engine::IncomingChunk>) {
        if self.drop_recv_chunk_at.is_none() {
            self.drop_recv_chunk_at = self.state.lock().unwrap().debug_drop_recv_chunk_at.take();
        }
        for c in chunks {
            let Some(idx) = self.recv.iter().position(|r| r.file_id == c.file_id) else {
                // 未收到 FILE_META（或会话已被 FileDone 收尾）的分块：只能丢，但**必须留痕**——静默
                // continue 会造成"手机显示已完成、电脑这边文件建出来却是 0 字节"，两边都看不出哪里断了。
                debuglog::log!(
                    debuglog::Level::Warn,
                    "file",
                    "orphan_chunk",
                    &[
                        ("file_id", &format!("{:#x}", c.file_id)),
                        ("index", &c.index.to_string()),
                        ("len", &c.data.len().to_string()),
                        ("recv_sessions", &self.recv.len().to_string()),
                    ]
                );
                continue;
            };
            let expect = self.recv[idx].next_index;

            // 收端故障注入（`/action/drop-recv-chunk`）：当作这一帧根本没到。
            // 用它才能在没有调试对端的情况下造出真洞，验证「暂缓收尾 → 补发 → 超时判死」整条路。
            if self.drop_recv_chunk_at == Some(c.index) {
                self.drop_recv_chunk_at = None;
                debuglog::log!(
                    debuglog::Level::Warn,
                    "file",
                    "fault.drop_recv_chunk",
                    &[
                        ("index", &c.index.to_string()),
                        ("file_id", &format!("{:#x}", c.file_id))
                    ]
                );
                continue;
            }

            // 1) 每块 CRC32：不符 → 请求从期望位置重传
            if crc32(&c.data) != c.crc32 {
                self.request_resume(c.file_id, expect, "分块 CRC32 校验失败");
                continue;
            }
            // 2) 已经落过的重复块：忽略（续传之后对端会重发已收到的那一段）
            if c.index < expect {
                continue;
            }
            // 3) 洞之后的「早到」块：先收着，等洞补上再按序喂摘要器。直接丢掉并发续传的话，发端一旦发出
            //    FILE_DONE 就没有人再续了——丢一块等于判这次传输死刑。落盘本来就是随机写（偏移 = index ×
            //    chunk_size），顺序只对增量摘要有意义。
            if c.index > expect {
                let r = &self.recv[idx];
                let mine = r.ahead_bytes;
                let total: usize = self.recv.iter().map(|s| s.ahead_bytes).sum();
                // 单任务与全局各封一道：一次导出 64 张时 `recv` 可以有 64 条会话，只封单任务的话总量是 64 × 4 MB，红线就是这么被顶破的。
                let over = mine + c.data.len() > AHEAD_BYTES_MAX
                    || total + c.data.len() > AHEAD_TOTAL_BYTES_MAX;
                // 落在声明长度之外的 index 不是"早到"，是对端算错了：照旧走续传请求
                let bogus = c.index as u64 * r.chunk_size as u64 >= r.size;
                if over || bogus {
                    self.request_resume(c.file_id, expect, "分块序号不连续");
                    continue;
                }
                let len = c.data.len();
                if let std::collections::btree_map::Entry::Vacant(v) =
                    self.recv[idx].ahead.entry(c.index)
                {
                    v.insert(c.data);
                    self.recv[idx].ahead_bytes += len;
                }
                self.drain_ahead(c.file_id);
                continue;
            }
            // 4) 正好是期望块：落盘，再把跟在后面的连续段一并接上
            if self.land_chunk(c.file_id, c.index, c.data) {
                self.drain_ahead(c.file_id);
            }
        }
    }

    /// 落一块：seek + write + 增量摘要 + 计数 + 界面进度。返回 `false` = 写盘失败，会话已当场
    /// 收尾（删半截文件 + 回一条失败 FILE_DONE），调用方不许再拿这个 `file_id` 继续走后面的块。
    fn land_chunk(&mut self, file_id: u64, index: u32, data: Vec<u8>) -> bool {
        let Some(idx) = self.recv.iter().position(|r| r.file_id == file_id) else {
            return false;
        };
        // 这条不变量是整个接收路径的地基：`next_index` 只按"正好轮到的那一块"前进，
        // 早到的走 `ahead` stash、晚到的走续传。谁要是绕过它直接调这里，
        // 增量摘要就会漏一段或重一段——那是最难查的一类错（摘要不符但文件看着是好的）。
        debug_assert_eq!(
            index, self.recv[idx].next_index,
            "land_chunk 只能落「正好轮到的那一块」，早到的要走 drain_ahead"
        );
        // 偏移 = index × chunk_size（随机写语义）
        let offset = index as u64 * self.recv[idx].chunk_size as u64;
        let outcome: Result<(String, u8, u64, u64, u64), String> = {
            let r = &mut self.recv[idx];
            if offset + data.len() as u64 > r.size {
                // 对端算错或在撒谎时，随机写会把文件撑到声明之外——收件目录不该
                // 被一次传输写得比它自己说的还大。这一条必须回话，不能只丢一块。
                Err(format!(
                    "分块越出声明长度：{} 第 {index} 块 @偏移 {offset} +{} B > {} B",
                    r.name,
                    data.len(),
                    r.size
                ))
            } else {
                match r
                    .file
                    .seek(SeekFrom::Start(offset))
                    .and_then(|_| r.file.write_all(&data))
                {
                    Ok(()) => {
                        r.hasher.update(&data);
                        r.next_index += 1;
                        r.received += data.len() as u64;
                        // 按期望位置落了一块 = 续传真的起效了，下一次再缺块算新的一轮
                        r.pending_resume = None;
                        // 补发正在进展 → 暂缓窗口重新计时：窗口量的是"多久没动静"，否则一次整段重发会被自己掐死。
                        if r.resume_hold_ms != 0 {
                            r.resume_hold_ms = now_ms() + RESUME_HOLD_MS;
                        }
                        Ok((
                            r.name.clone(),
                            percent_of(r.received, r.size),
                            r.album_id,
                            r.received,
                            r.size,
                        ))
                    }
                    Err(e) => Err(format!("文件落盘失败：{}（偏移 {offset}）: {e}", r.name)),
                }
            }
        };
        match outcome {
            Ok((name, percent, album_id, got, total)) => {
                let mut st = self.state.lock().unwrap();
                st.update_file_task(&name, TASK_DIR_RECV, percent, "接收中");
                // 拖出进度环的数据源：这条接收如果是"取原图"，把已收/总量记进那一格。
                // 只有整数百分比变了才 bump `ui_rev`——几千个分块逐个刷屏既没意义，也违反"静止时不空转重绘"。
                if album_id != 0 && st.album.set_drag_progress(album_id, got, total) {
                    st.ui_rev += 1;
                }
                true
            }
            Err(msg) => {
                let r = self.recv.remove(idx);
                let album_id = r.album_id;
                let name = r.name.clone();
                let _ = fs::remove_file(&r.path); // 不完整文件不留在收件目录
                self.state
                    .lock()
                    .unwrap()
                    .update_file_task(&name, TASK_DIR_RECV, 0, "失败");
                // 回执是必须的：发送侧在 EOF 时自己判"已完成"，收不到 FileDone 就会把一次失败的传输报成成功。
                self.refuse_recv(r.file_id, album_id, "接收写入失败", msg.clone());
                self.album_settled(album_id, false, format!("{name}：{msg}"));
                false
            }
        }
    }

    /// 把「早到」的块按序接回去：洞一补上，stash 里连着的那一段就一并落盘。
    fn drain_ahead(&mut self, file_id: u64) {
        loop {
            let Some(idx) = self.recv.iter().position(|r| r.file_id == file_id) else {
                return;
            };
            let want = self.recv[idx].next_index;
            let Some(data) = self.recv[idx].ahead.remove(&want) else {
                return;
            };
            self.recv[idx].ahead_bytes = self.recv[idx].ahead_bytes.saturating_sub(data.len());
            if !self.land_chunk(file_id, want, data) {
                return;
            }
        }
    }

    /// 拒绝这次接收，并**把原因回给发送侧**：只往 UI 推一条错误就 `return` 的话，发送侧等不到
    /// `FileDone`，进度条永远停在"传输中"——用户看到的是"手机发了、电脑什么都没有"。拒绝可以，但必须让对方知道。
    /// `album_id` 非 0 时必须一起作废：那一格的进度环与落盘路由只有在这里被摘掉才会停，
    /// 否则环永远转、这格也再不会发起预取（`window.rs` 以"有路由"判在途）。
    ///
    /// `code` 与 `detail` 分开是有意的：`detail` 带本机绝对路径与卷剩余空间，只进界面；
    /// `code` 才是回给对端的那句——FILE_DONE.error 会显示在对方手机上。
    fn refuse_recv(&mut self, file_id: u64, album_id: u64, code: &str, detail: String) {
        self.push_error(format!("已拒绝接收：{detail}"));
        self.ack_refusal(file_id, code);
        self.album_settled(album_id, false, detail);
    }

    /// 只发拒绝回执，不往界面推错误（供"预期内的拒收"用，见 `begin_recv` 的撤销分支）
    fn ack_refusal(&mut self, file_id: u64, reason: &str) {
        if let Some(e) = self.engine.as_mut() {
            // send_file_done 返回 false = 回执没发出去，发送侧会停在"传输中"：必须留痕，否则又变成查不到根因的静默失败。
            if !e.send_file_done(file_id, false, Some(reason), now_ms()) {
                debuglog::log(
                    debuglog::Level::Warn,
                    "file",
                    "拒绝回执未能发出（引擎拒绝），发送侧将看不到失败原因",
                    &[],
                );
            }
        } else {
            debuglog::log(
                debuglog::Level::Warn,
                "file",
                "无引擎句柄，拒绝原因无法回给发送侧",
                &[],
            );
        }
    }

    /// 启动接收任务（`FileMetaReceived`）：净化文件名 → 在收件目录建文件
    #[allow(clippy::too_many_arguments)] // 参数就是 FILE_META 的字段，包一层结构体只会多一次搬运
    fn begin_recv(
        &mut self,
        file_id: u64,
        raw_name: String,
        size: u64,
        chunk_size: u32,
        sha256: Option<[u8; 32]>,
        _crc32: u32,
        album_id: u64,
    ) {
        if chunk_size == 0 || chunk_size as usize > CHUNK_SIZE {
            self.refuse_recv(
                file_id,
                album_id,
                "分块大小非法",
                format!("文件分块大小非法（{chunk_size}）"),
            );
            return;
        }
        // `size` 与 `chunk_size` 都是**对端声明的**，不能由它决定本机开多大的文件：上限挡
        // "声明一个天文数字然后慢慢写"；剩余空间在目标 dir 定下来之后查（见下），导出目录可能
        // 在另一个盘，收件目录的余量代表不了它。
        if size > MAX_RECV_FILE_BYTES {
            self.refuse_recv(
                file_id,
                album_id,
                "文件超过本机接收上限",
                format!("文件超过本机接收上限（{size} B > {MAX_RECV_FILE_BYTES} B）"),
            );
            return;
        }
        // 文件名改成安全的单段名（**永不拒绝**），落盘只 join 收件目录；改名结果就是传输列表里显示的实际文件名。
        let name = sanitize_file_name(&raw_name);
        if name != raw_name {
            debuglog::log(
                debuglog::Level::Info,
                "file",
                &format!("文件名已按 NTFS 规则改写：{raw_name:?} → {name:?}"),
                &[],
            );
        }
        // 同 file_id 重复 META：丢弃旧任务重开（对端重发场景）
        if let Some(i) = self.recv.iter().position(|r| r.file_id == file_id) {
            let old = self.recv.remove(i);
            let _ = fs::remove_file(&old.path);
        }
        // 本机自己撤销过的拖出（"点一下"不是拖出）：这一份到货属于**预期内**，安静丢掉就行。
        // 但回绝必须发——发送侧等不到 FileDone 会把失败报成成功；报错拦住又会让用户以为出了故障。
        if album_id != 0 && self.state.lock().unwrap().album.take_cancelled(album_id) {
            let reason = format!("照片 {album_id} 的拖出已取消");
            debuglog::log(
                debuglog::Level::Info,
                "album",
                &format!("撤销在途的拖出到货，已静默拒收: {reason}"),
                &[],
            );
            self.ack_refusal(file_id, &reason);
            return;
        }
        // 带 album_id 的 META 是「取原图」的应答，必须落到**登记过的目标目录**（导出目录或拖出
        // 临时目录），而不是混进收件目录——那会让"导出"变成"没导出"。没登记过就如实拒收：
        // 落盘位置由本机决定，手机不该也不能替用户挑目录。
        let dir = if album_id == 0 {
            self.state.lock().unwrap().inbox_dir.clone()
        } else {
            let decision = {
                let st = self.state.lock().unwrap();
                match st.album.routes.get(&album_id).copied() {
                    Some(crate::state::AlbumPurpose::Export) => {
                        let d = st.album.export_dir.trim().to_string();
                        if d.is_empty() {
                            Err(format!(
                                "照片 {album_id} 属于本次导出，但导出目录已不可用（请重新选一次目录）"
                            ))
                        } else {
                            Ok(d)
                        }
                    }
                    Some(crate::state::AlbumPurpose::Drag) => {
                        Ok(crate::transfer::album_drag_dir().display().to_string())
                    }
                    None => Err(format!(
                        "收到相册原图（照片 {album_id}），但它不属于本机任何一次导出或拖出请求：没有登记落盘目录，已拒绝"
                    )),
                }
            };
            match decision {
                Ok(d) => d,
                Err(why) => {
                    self.refuse_recv(file_id, album_id, "原图不属于本机任何一次请求", why);
                    return;
                }
            }
        };
        if let Err(e) = fs::create_dir_all(&dir) {
            let why = format!("创建落盘目录失败（{dir}）: {e}");
            self.refuse_recv(file_id, album_id, "创建落盘目录失败", why);
            return;
        }
        // 剩余空间在**目标目录**定下来之后查（收件目录与导出目录可能不在同一个卷）。
        // 读不到余量不当成"没空间"——那是误拒；真写不下时 `land_chunk` 的写盘失败路径照样大声收尾。
        match crate::transfer::volume_free_bytes(Path::new(&dir)) {
            None => debuglog::log(
                debuglog::Level::Warn,
                "transfer",
                "recv.freespace_unknown",
                &[("file_id", &format!("{file_id:#x}")), ("dir", &dir.clone())],
            ),
            Some(free) if free < size => {
                let why = format!("本机空间不足：需要 {size} B，{dir} 所在卷可用 {free} B");
                self.refuse_recv(file_id, album_id, "本机空间不足", why);
                return;
            }
            Some(_) => {}
        }
        let path = match unique_path(Path::new(&dir), &name) {
            Some(p) => p,
            None => {
                let why = format!("{dir} 里同名文件已多于 999 个，为不覆盖任何已有文件而拒收");
                self.refuse_recv(file_id, album_id, "同名文件过多，未覆盖已有文件", why);
                return;
            }
        };
        let file = match fs::File::create(&path) {
            Ok(f) => f,
            Err(e) => {
                // unique_path 是按 create_new 独占建出来的，这次失败就是留了个 0 字节空文件：
                // 不删，收件目录里会攒一堆没名字对不上号的空壳（取消路径同口径）
                let _ = fs::remove_file(&path);
                let why = format!("创建接收文件 {} 失败: {e}", path.display());
                self.refuse_recv(file_id, album_id, "创建接收文件失败", why);
                return;
            }
        };
        self.recv.push(RecvSession {
            file_id,
            name: name.clone(),
            path,
            file,
            size,
            chunk_size,
            next_index: 0,
            received: 0,
            hasher: FileHasher::new(),
            expect_sha256: sha256,
            resume_tries: 0,
            pending_resume: None,
            resume_hold_ms: 0,
            album_id,
            ahead: std::collections::BTreeMap::new(),
            ahead_bytes: 0,
        });
        {
            let mut st = self.state.lock().unwrap();
            st.update_file_task(&name, TASK_DIR_RECV, 0, "接收中");
            // 登记大小：速度是按"百分比增量 ÷ 用时"算的，没有总大小就只能显示百分比
            st.set_file_task_size(&name, TASK_DIR_RECV, size);
        }
    }

    /// 续传请求（`RecvAction::RequestResume` 的平台侧等价）：超限则放弃该任务。
    /// **同一个起点只问一次**：一个空洞之后的每一块都会走到这里，按到达次数算预算的话，292 块的
    /// 视频会在几毫秒内烧光 `MAX_RESUME_TRIES` 并放弃接收——而对端还没来得及重传任何一块。重复请求也没有信息量。
    fn request_resume(&mut self, file_id: u64, from_index: u32, why: &str) {
        let Some(idx) = self.recv.iter().position(|r| r.file_id == file_id) else {
            return;
        };
        if self.recv[idx].pending_resume == Some(from_index) {
            return; // 这一处已经问过，正在等它
        }
        self.recv[idx].pending_resume = Some(from_index);
        self.recv[idx].resume_tries += 1;
        let tries = self.recv[idx].resume_tries;
        let name = self.recv[idx].name.clone();
        let percent = percent_of(self.recv[idx].received, self.recv[idx].size);
        if tries > MAX_RESUME_TRIES {
            let r = self.recv.remove(idx);
            let _ = fs::remove_file(&r.path);
            self.state
                .lock()
                .unwrap()
                .update_file_task(&r.name, TASK_DIR_RECV, 0, "失败");
            self.refuse_recv(
                r.file_id,
                r.album_id,
                "续传多次仍未成功",
                format!("{why}；续传 {tries} 次仍未成功，已放弃接收"),
            );
            return;
        }
        if let Some(eng) = self.engine.as_mut() {
            let _ = eng.send_file_resume(file_id, from_index, now_ms());
        }
        let mut st = self.state.lock().unwrap();
        st.update_file_task(&name, TASK_DIR_RECV, percent, "重传中");
        st.push_error(format!("{why}；已请求对端从第 {from_index} 块重传"));
    }

    /// 接收收尾（`FileDoneReceived`）：核对字节数并汇总整文件校验结果
    ///
    /// `done_sha`：发端在 FILE_DONE 里交付的整文件摘要（发分块时增量算出来的那份）。
    fn finish_recv(
        &mut self,
        file_id: u64,
        ok: bool,
        error: Option<String>,
        done_sha: Option<[u8; 32]>,
    ) {
        let Some(idx) = self.recv.iter().position(|r| r.file_id == file_id) else {
            // 没有本端在途接收却收到结束帧：多半是重复/迟到的 DONE，或发端把会话收尾后
            // 才收到我们那条"补发超时"的回执。**必须留痕**——静默 return 的话，
            // "发端还在补发、收端早就收尾"这类分歧在日志里完全看不见。
            debuglog::log!(
                debuglog::Level::Warn,
                "transfer",
                "recv.done_no_session",
                &[
                    ("file_id", &format!("{file_id:#x}")),
                    ("ok", &ok.to_string()),
                    ("recv_sessions", &self.recv.len().to_string()),
                ]
            );
            return;
        };
        let r = self.recv.remove(idx);
        if !ok {
            let msg = error.unwrap_or_else(|| "对端报告失败".to_string());
            self.fail_recv(r, &msg);
            return;
        }
        // 分块已逐块校验，此处核对「块全部到齐 + 字节数与声明一致」。
        let total_chunks = chunks_total(r.size, r.chunk_size as usize);
        let complete = r.next_index >= total_chunks && r.received == r.size;
        // 发端的 FILE_DONE 说"发完了"，而我们手里有洞——此刻**不该收尾**：先摘会话再回执"不完整"的话，发端
        // 永远没有机会补发（它发出第一条 FILE_DONE 后就再没人读续传请求，重发的每一块只会变成孤儿）。
        // 这里给会话一个有界的暂缓窗口再要一次补发，窗口内没有任何补发进展才按失败收尾；判据里**不看**
        // `resume_hold_ms` 是不是已经置过——第二个洞同样该再救一次，真正的上界是 `resume_tries`。
        if !complete && r.resume_tries < MAX_RESUME_TRIES {
            self.hold_for_resume(r);
            return;
        }
        self.settle_recv(r, done_sha);
    }

    /// 收端报告失败（发端那条 FILE_DONE 说 ok=false）：只把原因说出来，不再回执。
    fn fail_recv(&mut self, r: RecvSession, msg: &str) {
        let album_id = r.album_id;
        {
            let mut st = self.state.lock().unwrap();
            st.update_file_task(
                &r.name,
                TASK_DIR_RECV,
                percent_of(r.received, r.size),
                "失败",
            );
            st.push_error(format!("文件接收失败（{}）: {msg}", r.name));
        }
        self.album_settled(album_id, false, format!("{}：{msg}", r.name));
    }

    /// 暂缓收尾：把会话放回在途表，向发端再要一次从 `next_index` 起的补发。
    fn hold_for_resume(&mut self, mut r: RecvSession) {
        r.resume_hold_ms = now_ms() + RESUME_HOLD_MS;
        r.resume_tries += 1;
        r.pending_resume = Some(r.next_index);
        let (file_id, from, tries) = (r.file_id, r.next_index, r.resume_tries);
        let name = r.name.clone();
        let percent = percent_of(r.received, r.size);
        if let Some(eng) = self.engine.as_mut() {
            let _ = eng.send_file_resume(file_id, from, now_ms());
        }
        debuglog::log!(
            debuglog::Level::Warn,
            "transfer",
            "recv.hold_for_resume",
            &[
                ("file_id", &format!("{file_id:#x}")),
                ("from", &from.to_string()),
                ("tries", &tries.to_string()),
            ]
        );
        {
            let mut st = self.state.lock().unwrap();
            st.update_file_task(&name, TASK_DIR_RECV, percent, "重传中");
            st.push_error(format!(
                "{name}：对端说发完了但本机缺第 {from} 块，正在等它补发"
            ));
        }
        self.recv.push(r);
    }

    /// 暂缓窗口到点：补发没来（或没来全）——必须大声收尾，不许永远挂着「重传中」。
    fn tick_resume_holds(&mut self) {
        let Some(idx) = self
            .recv
            .iter()
            .position(|r| r.resume_hold_ms != 0 && now_ms() > r.resume_hold_ms)
        else {
            return;
        };
        let mut r = self.recv.remove(idx);
        let total = chunks_total(r.size, r.chunk_size as usize);
        let complete = r.next_index >= total && r.received == r.size;
        r.resume_hold_ms = 0;
        let (file_id, from) = (r.file_id, r.next_index);
        debuglog::log!(
            debuglog::Level::Warn,
            "transfer",
            "recv.hold_expired",
            &[
                ("file_id", &format!("{file_id:#x}")),
                ("from", &from.to_string()),
            ]
        );
        if complete {
            // 补发都到了，只是发端第二条 FILE_DONE 没落进来：按老路校验收尾
            // （摘要此时只能回落 FILE_META 声明的那份，缺位就报「未校验」——都是真话）。
            self.settle_recv(r, None);
            return;
        }
        let why = format!("等对端从第 {from} 块补发超时，文件不完整");
        if let Some(eng) = self.engine.as_mut() {
            // 让发端停止补发：它此刻可能还在重发同一批分块
            let _ = eng.send_file_done(file_id, false, Some(why.as_str()), now_ms());
        }
        self.fail_recv(r, &why);
    }

    /// 校验收尾：核对字节数与整文件摘要，回执一条 FILE_DONE，再把结果落到界面。
    fn settle_recv(&mut self, mut r: RecvSession, done_sha: Option<[u8; 32]>) {
        let (album_id, file_id) = (r.album_id, r.file_id);
        // 整文件校验：分块已逐块校验，此处核「块全部到齐 + 字节数与声明一致」，再比对本端增量算出的 SHA-256。
        let total_chunks = chunks_total(r.size, r.chunk_size as usize);
        let complete = r.next_index >= total_chunks && r.received == r.size;
        let actual_size = fs::metadata(&r.path).map(|m| m.len()).unwrap_or(0);
        // `File::flush` 是空操作，真正要的是把内核缓存刷到介质：不刷就回 FILE_DONE{ok=true}，
        // 掉电或拔盘时用户看到的"已完成"是假的。刷不上就按失败回执，别报成功。
        let flushed = r.file.flush().and_then(|_| r.file.sync_all()).is_ok();
        let (sha, _crc) = r.hasher.finish();
        // 校验源到收尾时才定：FILE_DONE 的摘要优先，缺位才回落 FILE_META 的声明值；两处都没有 = 绝不落「已完成」。
        let expected = done_sha.or(r.expect_sha256);
        let digest_ok = expected == Some(sha);
        let sha_prefix: String = sha.iter().take(6).map(|b| format!("{b:02x}")).collect();
        let expect_prefix = expected
            .map(|e| {
                e.iter()
                    .take(6)
                    .map(|b| format!("{b:02x}"))
                    .collect::<String>()
            })
            .unwrap_or_else(|| "—".to_string());
        let name = r.name.clone();
        let shown = r.path.display().to_string();
        // 收端**无论成败都回一条 FILE_DONE**：只在拒收时回话、成功接收从不作声的话，发送侧就没有
        // 任何"确认"可等，只能在 EOF 时自己判成功。
        let ack_ok = complete && actual_size == r.size && digest_ok && flushed;
        let ack_reason = if ack_ok {
            None
        } else if !flushed {
            Some("接收端未能把数据刷到磁盘（磁盘已满、被占用或权限不足）")
        } else if expected.is_none() {
            Some("接收端没有可比对的整文件摘要")
        } else if complete && actual_size == r.size {
            Some("接收端摘要不符")
        } else {
            Some("接收端不完整")
        };
        if let Some(eng) = self.engine.as_mut() {
            let _ = eng.send_file_done(file_id, ack_ok, ack_reason, now_ms());
        }
        let mut st = self.state.lock().unwrap();
        if ack_ok {
            st.update_file_task(&name, TASK_DIR_RECV, 100, "已完成");
            st.push_toast(
                "文件接收完成".to_string(),
                format!("{name} → {shown}（SHA-256 {sha_prefix}…）"),
            );
            drop(st);
            // 相册原图到这儿才算真的到手：把落盘路径回给等待方（拖出）或报一声（导出）
            self.album_settled(album_id, true, shown.clone());
        } else if expected.is_none() {
            st.update_file_task(
                &name,
                TASK_DIR_RECV,
                percent_of(r.received, r.size),
                "未校验",
            );
            st.push_error(format!(
                "文件无法校验（{name}）：对端的 FILE_META 与 FILE_DONE 都没给整文件摘要；文件已隔离在收件目录"
            ));
            drop(st);
            self.album_settled(
                album_id,
                false,
                format!("{name}：对端没给整文件摘要，无法确认完整"),
            );
        } else if complete && actual_size == r.size {
            st.update_file_task(&name, TASK_DIR_RECV, 100, "摘要不符");
            st.push_error(format!(
                "文件摘要不符（{name}）：本端 {sha_prefix}… ≠ 对端 {expect_prefix}…；文件已隔离在收件目录"
            ));
            drop(st);
            self.album_settled(album_id, false, format!("{name}：摘要不符"));
        } else {
            st.update_file_task(
                &name,
                TASK_DIR_RECV,
                percent_of(r.received, r.size),
                "校验失败",
            );
            st.push_error(format!(
                "文件校验失败（{name}）：声明 {} 字节 / {total_chunks} 块，实际 {} 字节 / {} 块",
                r.size, actual_size, r.next_index
            ));
            drop(st);
            self.album_settled(album_id, false, format!("{name}：不完整（校验失败）"));
        }
    }

    /// 「取消」的唯一入口：把 UI 行键 `(方向, 文件名)` 换成本机在途会话，再交给引擎掐断。命令只下发、
    /// 不宣称已取消——真正结果看随后的 `FileTaskCancelled` 事件。取消**未起播**的排队任务是本地直接撤行。
    fn cancel_task(&mut self, dir: u8, name: &str) {
        let send_id = if dir == TASK_DIR_SEND {
            self.send
                .as_ref()
                .filter(|s| s.name == name)
                .map(|s| s.file_id)
        } else {
            None
        };
        let recv_id = if dir == TASK_DIR_RECV {
            self.recv.iter().find(|r| r.name == name).map(|r| r.file_id)
        } else {
            None
        };
        if let Some(file_id) = send_id.or(recv_id) {
            let Some(eng) = self.engine.as_mut() else {
                self.push_error(format!("取消「{name}」失败：本机会话已不存在"));
                return;
            };
            // 收端取消必须回话（FILE_CANCEL）才能真的让发端停下；发端取消直接落结束帧。
            let sent = if dir == TASK_DIR_SEND {
                eng.cancel_file_send(file_id, "电脑端已取消", now_ms())
            } else {
                eng.cancel_file_recv(file_id, "电脑端已取消", now_ms())
            };
            // 引擎两种失败都仍会发事件（原因里带"结束帧未能发出"），这里只补一条埋点
            debuglog::log!(
                debuglog::Level::Warn,
                "ui",
                "file.cancel.click",
                &[
                    ("dir", if dir == TASK_DIR_SEND { "send" } else { "recv" }),
                    ("name", name),
                    ("frame_sent", if sent { "true" } else { "false" }),
                ]
            );
            return;
        }
        // 还在队列里等配对/等 TCP：一帧都没发过，撤掉排队就是取消。
        if dir == TASK_DIR_SEND
            && self
                .pending_send
                .as_deref()
                .is_some_and(|p| file_base_name(p) == name)
        {
            self.pending_send = None;
            self.state
                .lock()
                .unwrap()
                .update_file_task(name, TASK_DIR_SEND, 0, "已取消");
            return;
        }
        // 找不到在途会话不是"没什么可做的"，而是"用户点了一下却什么都没发生"——必须说。
        self.push_error(format!(
            "「{name}」已经不在途了，这一下没有可取消的传输（终态行不会画「取消」，请看该行当前状态）"
        ));
    }

    // ---------- 9) 文件发送（由 200ms 循环驱动 + 分块节流） ----------
    fn pump_send(&mut self) {
        // 0) 已发完、正在等对端 FILE_DONE 回执：不再读盘、不再发帧，只判超时。但这个窗口里仍可能收到
        // RESUME——那是对端在说"你说发完了，可我手里有洞"。这里若无条件 `return`，续传请求被记进
        // `resume_from` 之后就没人再读它，"迟到的续传"结构上不可能成功；正确做法是收起回执等待、按起点补发、再发第二条 FILE_DONE。
        let awaiting = self
            .send
            .as_ref()
            .map(|s| s.ack_deadline_ms != 0)
            .unwrap_or(false);
        if awaiting {
            let resumed = self
                .send
                .as_ref()
                .map(|s| s.resume_from.is_some())
                .unwrap_or(false);
            if !resumed {
                let expired = self
                    .send
                    .as_ref()
                    .map(|s| now_ms() > s.ack_deadline_ms)
                    .unwrap_or(false);
                if expired {
                    let name = self
                        .send
                        .as_ref()
                        .map(|s| s.name.clone())
                        .unwrap_or_default();
                    self.send = None;
                    let mut st = self.state.lock().unwrap();
                    st.update_file_task(&name, TASK_DIR_SEND, 100, "已发送（未确认）");
                    st.push_error(format!("文件 {name} 没有收到对端回执，不能判定为已完成"));
                }
                return;
            }
            if let Some(s) = self.send.as_mut() {
                s.ack_deadline_ms = 0; // 又开始真正发东西了，回执从下一次 EOF 重新计
                debuglog::log!(
                    debuglog::Level::Warn,
                    "transfer",
                    "send.resume_in_ack_window",
                    &[("file_id", &format!("{:#x}", s.file_id))]
                );
            }
        }

        // 1) 待发路径 → 启动（需已配对；分块流式发送需 TCP 已绑定）
        if let Some(path) = self.pending_send.clone() {
            if self.send.is_some() {
                // 已有任务在传：不并发（队列语义留待后续里程碑），明确提示而不是静默丢弃
                self.pending_send = None;
                self.push_error("已有文件正在传输，请等当前任务结束后再发送".to_string());
            } else {
                let (paired, bound) = self
                    .engine
                    .as_ref()
                    .map(|e| (e.is_paired(), e.is_tcp_bound()))
                    .unwrap_or((false, false));
                if !paired {
                    self.pending_send = None;
                    self.push_error("未配对，文件未发送（请先在连接页与手机配对）".to_string());
                } else if bound {
                    self.pending_send = None;
                    if let Err(e) = self.start_send(&path) {
                        self.push_error(e);
                    }
                } else {
                    // 已配对但 TCP 尚未就绪：保持等待，UI 显示「等待 TCP 通道」
                    let name = file_base_name(&path);
                    self.state.lock().unwrap().update_file_task(
                        &name,
                        TASK_DIR_SEND,
                        0,
                        "等待 TCP 通道",
                    );
                }
            }
        }

        if self.send.is_none() {
            return;
        }

        // 2) 对端 RESUME → 重建读取器并跳过已发送前缀（跳过的前缀补进摘要）
        let resume_err = {
            let Some(sess) = self.send.as_mut() else {
                return;
            };
            match sess.resume_from.take() {
                Some(from) => match open_chunker(&sess.path, from, &mut sess.task) {
                    Ok(c) => {
                        sess.chunker = c;
                        sess.sent_chunks = from;
                        None
                    }
                    Err(e) => Some((sess.name.clone(), format!("续传失败: {e}"))),
                },
                None => None,
            }
        };
        if let Some((name, msg)) = resume_err {
            self.fail_send(&name, &msg);
            return;
        }

        // 3) 节流发送：TCP 未绑定则暂停等待（256KB × N 走 BLE 会拖垮链路，故不降级发送）
        let bound = self
            .engine
            .as_ref()
            .map(|e| e.is_tcp_bound())
            .unwrap_or(false);
        if !bound {
            if let Some(s) = self.send.as_ref() {
                let (name, percent) = (s.name.clone(), s.percent());
                self.state.lock().unwrap().update_file_task(
                    &name,
                    TASK_DIR_SEND,
                    percent,
                    "等待 TCP 通道",
                );
            }
            return;
        }

        // 在途预算 = 窗口 - 当前队列深度：只提每轮发送上限、不管队列深度的话，发送速度长期高于
        // 链路速度，队列一路涨到"剩余文件大小"，100 MB 级积压直接把进程撑死——提速必须和背压一起做。
        let (depth, window) = self
            .engine
            .as_ref()
            .map(|e| (e.tcp_out_depth(), e.tcp_out_window()))
            .unwrap_or((0, SEND_CHUNKS_PER_TICK));
        let budget = window
            .saturating_sub(depth)
            .min(SEND_CHUNKS_PER_TICK.max(window / 2));
        if budget == 0 {
            if let Some(s) = self.send.as_ref() {
                let (name, percent) = (s.name.clone(), s.percent());
                self.state.lock().unwrap().update_file_task(
                    &name,
                    TASK_DIR_SEND,
                    percent,
                    "等待链路排空",
                );
            }
            return;
        }

        let mut sent = 0usize;
        let mut eof = false;
        let mut read_err: Option<String> = None;
        // 故障注入待取：发布包里没有任何入口能写 `debug_drop_chunk_at`，它恒为 None
        if self.drop_chunk_at.is_none() {
            self.drop_chunk_at = self.state.lock().unwrap().debug_drop_chunk_at.take();
        }
        {
            let Some(sess) = self.send.as_mut() else {
                return;
            };
            while sent < budget {
                let chunk = match sess.chunker.next_chunk() {
                    Ok(Some(c)) => c,
                    Ok(None) => {
                        eof = true;
                        break;
                    }
                    Err(e) => {
                        read_err = Some(format!("读取文件失败: {e}"));
                        break;
                    }
                };
                if self.drop_chunk_at == Some(chunk.index) {
                    self.drop_chunk_at = None;
                    debuglog::log!(
                        debuglog::Level::Warn,
                        "file",
                        "fault.drop_chunk",
                        &[("index", &chunk.index.to_string())]
                    );
                    // 只跳过「交给引擎」这一步：chunker 已前进、摘要照常累计、sent_chunks 照常推进——与链路真吞掉这一帧完全一致。
                    sess.sent_chunks = chunk.index + 1;
                    if let Err(e) = sess.task.note_sent(&chunk.data) {
                        read_err = Some(format!("整文件摘要累计失败: {e}"));
                        break;
                    }
                    sent += 1;
                    continue;
                }
                let ok = self
                    .engine
                    .as_mut()
                    .map(|e| {
                        e.send_file_chunk(
                            sess.file_id,
                            chunk.index,
                            chunk.crc32,
                            &chunk.data,
                            now_ms(),
                        )
                    })
                    .unwrap_or(false);
                if !ok {
                    break; // 会话暂不可用：保留进度，下一轮重试
                }
                sess.sent_chunks = chunk.index + 1;
                // 摘要与「真的交出去的字节」严格同步：少算一段就是给对端一个必定不符的摘要
                if let Err(e) = sess.task.note_sent(&chunk.data) {
                    read_err = Some(format!("整文件摘要累计失败: {e}"));
                    break;
                }
                sent += 1;
            }
        }

        if let Some(msg) = read_err {
            let name = self
                .send
                .as_ref()
                .map(|s| s.name.clone())
                .unwrap_or_default();
            self.fail_send(&name, &msg);
            return;
        }

        // 4) 全部发完 → FILE_DONE（带增量算出的整文件摘要），然后**等对端回执**（发完 ≠ 送达）
        if eof {
            let (file_id, name, digest) = match self.send.as_mut() {
                Some(s) => (
                    s.file_id,
                    s.name.clone(),
                    s.task.whole_file_digest().map(|d| d.0),
                ),
                None => return,
            };
            let Some(sha) = digest else {
                // 分块与摘要不同步（不该发生）：宁可大声失败，也不发一个没有摘要的"成功"
                self.fail_send(&name, "整文件摘要没算全（分块与摘要不同步）");
                return;
            };
            let ok = self
                .engine
                .as_mut()
                .map(|e| e.send_file_done_digest(file_id, true, Some(sha), None, now_ms()))
                .unwrap_or(false);
            if let Some(s) = self.send.as_mut() {
                let _ = s.task.finish(ok, None);
            }
            if !ok {
                self.send = None;
                let mut st = self.state.lock().unwrap();
                st.update_file_task(&name, TASK_DIR_SEND, 100, "已发完（结束帧未发出）");
                st.push_error(format!(
                    "文件 {name} 的结束帧没能发出，对端可能一直显示传输中"
                ));
                return;
            }
            // 会话保留：收到对端 FileDoneReceived 才落「已完成」，超时落「已发送（未确认）」——在 EOF 就当成功是在骗用户。
            if let Some(s) = self.send.as_mut() {
                s.ack_deadline_ms = now_ms() + ACK_TIMEOUT_MS;
            }
            self.state
                .lock()
                .unwrap()
                .update_file_task(&name, TASK_DIR_SEND, 100, "等待对端确认");
            return;
        }

        // 5) 刷新进度
        if let Some(s) = self.send.as_ref() {
            let (name, percent) = (s.name.clone(), s.percent());
            self.state
                .lock()
                .unwrap()
                .update_file_task(&name, TASK_DIR_SEND, percent, "发送中");
        }
    }

    /// 启动一次文件发送：立刻发 FILE_META → 流式分块，整文件摘要边发边算、由 FILE_DONE 交付
    fn start_send(&mut self, path: &str) -> Result<(), String> {
        let p = Path::new(path.trim());
        let meta = fs::metadata(p).map_err(|e| format!("打开文件失败（{path}）: {e}"))?;
        if !meta.is_file() {
            return Err(format!("路径不是文件: {path}"));
        }
        let size = meta.len();
        let name = p
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| format!("无法从路径解析文件名: {path}"))?;
        // 不再预扫整文件算摘要：8.6 GB 的预扫会让 UI 在第一个分块发出前静默十几秒，
        // 而且「先读一遍算摘要、再读一遍发块」是双份 IO。摘要由 SendTask 随分块增量算出。
        let file_id = new_file_id();
        let mut task = SendTask::streaming(file_id, name.clone(), size, CHUNK_SIZE)
            .map_err(|e| format!("创建发送任务失败: {e}"))?;
        task.start();
        let total_chunks = task.chunks_total();
        let chunker = open_chunker(p, 0, &mut task)?;

        let fm = FileMeta {
            name: name.clone(),
            size,
            file_id,
            chunk_size: CHUNK_SIZE as u32,
            // 摘要延后 → 长度 0。整文件 CRC32 同批延后（逐块 CRC32 仍然每块随帧携带）
            sha256: Default::default(),
            crc32: 0,
            album_id: 0, // 0 = 普通文件传输（非相册取原图）
        };
        let ok = self
            .engine
            .as_mut()
            .map(|e| e.send_file_meta(&fm, now_ms()))
            .unwrap_or(false);
        if !ok {
            return Err("FILE_META 发送失败（会话未就绪）".to_string());
        }

        self.send = Some(SendSession {
            file_id,
            task,
            chunker,
            name: name.clone(),
            total_chunks,
            sent_chunks: 0,
            resume_from: None,
            path: p.to_path_buf(),
            resume_rounds: 0,
            ack_deadline_ms: 0,
        });
        {
            let mut st = self.state.lock().unwrap();
            st.update_file_task(&name, TASK_DIR_SEND, 0, "发送中");
            st.set_file_task_size(&name, TASK_DIR_SEND, size);
        }
        Ok(())
    }

    /// 对端请求续传
    fn on_resume_requested(&mut self, file_id: u64, from_index: u32) {
        let Some(sess) = self.send.as_mut() else {
            // 本端根本没有在途发送：这条请求没人能兑现。留痕，别静默吞掉。
            debuglog::log!(
                debuglog::Level::Warn,
                "transfer",
                "send.resume_unhandled",
                &[
                    ("file_id", &format!("{file_id:#x}")),
                    ("from", &from_index.to_string()),
                    ("why", "本机没有在途发送任务"),
                ]
            );
            return;
        };
        if sess.file_id != file_id {
            return; // 多路复用下对端问的是别的文件，本端逐条各自处理
        }
        // 补发轮次有界，与收端 `MAX_RESUME_TRIES` 同口径：一个反复重放的迟到请求
        // 不该把发送循环拖着无限补发。超限后本端不再动作，由收端把自己的预算判死。
        if sess.resume_rounds >= MAX_RESUME_TRIES {
            debuglog::log!(
                debuglog::Level::Warn,
                "transfer",
                "send.resume_capped",
                &[
                    ("file_id", &format!("{file_id:#x}")),
                    ("rounds", &sess.resume_rounds.to_string()),
                ]
            );
            return;
        }
        // 合法起点：不得回退到已确认之前，也不得越过总块数
        if from_index > sess.sent_chunks || from_index > sess.total_chunks {
            return;
        }
        if sess.task.resume_from(from_index).is_err() {
            return;
        }
        sess.resume_rounds += 1;
        sess.sent_chunks = from_index;
        sess.resume_from = Some(from_index);
        let (name, percent) = (sess.name.clone(), sess.percent());
        self.state
            .lock()
            .unwrap()
            .update_file_task(&name, TASK_DIR_SEND, percent, "续传中");
    }

    /// 发送任务不可恢复失败 → 通知对端并清理
    fn fail_send(&mut self, name: &str, msg: &str) {
        if let Some(s) = self.send.take() {
            if let Some(eng) = self.engine.as_mut() {
                let _ = eng.send_file_done(s.file_id, false, Some(msg), now_ms());
            }
        }
        let mut st = self.state.lock().unwrap();
        st.update_file_task(name, TASK_DIR_SEND, 0, "失败");
        st.push_error(format!("文件发送失败（{name}）: {msg}"));
    }

    // ---------- 10) 心跳 / 超时 ----------

    fn tick(&mut self) {
        if self.last_tick.elapsed() >= TICK_INTERVAL {
            self.last_tick = Instant::now();
            if let Some(eng) = self.engine.as_mut() {
                eng.tick(Instant::now());
            }
        }
    }

    // ---------- 11) UDP 发现 ----------

    fn pump_discovery(&mut self) {
        let Some(d) = self.discovery.as_mut() else {
            return;
        };
        let now = Instant::now();
        let beacon = DiscoveryBeacon {
            advert_name: LOCAL_NAME.to_string(),
            os: linkx_protocol::OS_WINDOWS,
            version: linkx_core::LINKX_FFI_VERSION.to_string(),
        };
        // 广播失败不致命（无广播路由的沙箱/关网环境仅 lost 一包）
        if let Err(e) = d.broadcast_if_due(&beacon, now) {
            crate::say(format!("[LinkX] UDP 信标广播失败: {e}"));
        }
        let tick = d.pump(now);
        if tick.updated.is_empty() {
            return;
        }
        // 取一个 Android 对端的 IP 供展示（Windows 是 TCP 服务端，此 IP 用于诊断/manual IP）
        let ip = d
            .peers()
            .live(now)
            .into_iter()
            .find(|p| {
                p.beacon
                    .as_ref()
                    .map(|b| b.os == linkx_protocol::OS_ANDROID)
                    .unwrap_or(false)
            })
            .map(|p| p.addr.ip().to_string());
        if let Some(ip) = ip {
            let mut st = self.state.lock().unwrap();
            if st.peer_lan_ip != ip {
                st.peer_lan_ip = ip;
            }
        }
    }

    /// 手动 IP 兜底：入发现表 + 作为单播目标
    fn add_manual_peer(&mut self) {
        let input = self
            .state
            .lock()
            .unwrap()
            .manual_ip_input
            .trim()
            .to_string();
        if input.is_empty() {
            self.push_error("请填写对端 IP（例如 192.168.1.23）".to_string());
            return;
        }
        let addr = match parse_manual_addr(&input) {
            Ok(a) => a,
            Err(e) => {
                self.push_error(format!("IP 无法解析（{input}）: {e}"));
                return;
            }
        };
        let Some(d) = self.discovery.as_mut() else {
            self.push_error("发现服务未启动（需先与手机完成配对）".to_string());
            return;
        };
        d.add_manual_peer(addr, Instant::now());
        let mut st = self.state.lock().unwrap();
        st.peer_lan_ip = addr.ip().to_string();
        st.push_toast(
            "手动 IP 已加入".to_string(),
            format!("将持续向 {addr} 单向探测信标"),
        );
    }

    /// 解绑设备：清引擎信任库 + 本地持久化 + 全部对端态
    fn unbind(&mut self) {
        if let Some(eng) = self.engine.as_mut() {
            eng.on_tcp_closed("设备解绑");
            eng.unbind_peer();
        }
        self.teardown_channels();
        // 在途的收发不能一清了之：半截文件要删、任务要落到终态、等拖拽的那一方要被告知结案。
        // 否则"解绑成功"的同一秒，磁盘上留着一份没人认领的残件、文件页上挂着一条永远
        // 不动的进度，而拖出等待方要挂到超时才说得出原因。口径对齐取消路径（先放句柄
        // 再删）与 abandon_inflight_tasks（逐条 album_settled）。
        if let Some(s) = self.send.take() {
            let mut st = self.state.lock().unwrap();
            st.update_file_task(&s.name, TASK_DIR_SEND, 0, "已取消");
            st.push_toast("发送已取消".to_string(), format!("{}：设备已解绑", s.name));
        }
        let queued = self.pending_send.take();
        let sessions = std::mem::take(&mut self.recv);
        let mut dropped: Vec<(String, u64, std::path::PathBuf)> =
            Vec::with_capacity(sessions.len());
        for r in sessions {
            let row = (r.name.clone(), r.album_id, r.path.clone());
            drop(r); // 先释放写句柄再删：Windows 上"正被占用"的文件删不掉
            dropped.push(row);
        }
        {
            let mut st = self.state.lock().unwrap();
            if let Some(p) = queued {
                st.push_error(format!("{p} 还在排队就被取消：设备已解绑"));
            }
            for (name, _, path) in &dropped {
                st.update_file_task(name, TASK_DIR_RECV, 0, "已取消");
                if !path.exists() {
                    st.push_toast("接收已取消".to_string(), format!("{name}：设备已解绑"));
                } else if fs::remove_file(path).is_ok() {
                    st.push_toast(
                        "接收已取消".to_string(),
                        format!("{name}：设备已解绑，没收完的半截文件已删除"),
                    );
                } else {
                    st.push_error(format!(
                        "接收已取消（{name}），但半截文件没删掉，请手动清理：{}",
                        path.display()
                    ));
                }
            }
        }
        for (name, album_id, _) in &dropped {
            self.album_settled(*album_id, false, format!("{name}：设备已解绑，接收中断"));
        }
        // 清信任库（内存 + 落盘）：否则下次连接会命中旧信任自动放行
        self.trusted.clear();
        identity::clear_trust_store();
        // 本轮启动的自动拨号要一起收手：线索文件已删，但内存里那份名字还在，而刚解绑的设备
        // 通常还在扫描列表里没过期——不收手就等于把「解绑」变成一次新的配对弹窗。
        self.auto_dial = None;
        let mut st = self.state.lock().unwrap();
        st.paired = false;
        st.peer_fp = None;
        st.bound_devices.clear();
        st.sas = None;
        st.identity_change = None;
        st.tcp_ready = false;
        st.peer_lan_ip.clear();
        st.file_tasks.clear();
        st.selected = None;
        st.conn_state = state_code::DISCOVER;
        drop(st);
        post_state_changed(self.hwnd_raw);
    }
}

/// 把引擎上行事件落到共享状态（简单事件；文件/TCP/配置由 worker 单独处理）
fn apply_event(st: &mut UiState, ev: EngineEvent) {
    match ev {
        EngineEvent::StateChanged { state } => {
            st.conn_state = state;
            // 播放状态与电量都是"对端此刻"的实时值：会话一离开主态就作废，否则把上一台的曲目/电量显示给现在这台。
            if !matches!(
                state,
                state_code::PAIRED | state_code::REPAIRED | state_code::RECONNECTING
            ) {
                st.media = None;
                st.media_cover = None;
                st.battery = None;
            }
        }
        EngineEvent::PeerHello { name, os, .. } => {
            st.peer_name = name;
            st.peer_os = os;
        }
        EngineEvent::SasReady { sas } => st.sas = Some(sas),
        EngineEvent::PeerPaired { fingerprint } => {
            st.paired = true;
            st.peer_fp = Some(fingerprint);
            // 信任库落盘由 `Worker::handle_event` 统一负责（需要 Worker 持的列表；纯 UI 侧函数拿不到，避免两处写入不一致）。
        }
        EngineEvent::Notification {
            package,
            title,
            text,
            post_ts_ms,
            key_hash,
            tag,
            notification_id,
            can_reply,
            reply_action_index,
            reply_result_key,
        } => {
            // 关掉"通知同步"后不再落任何通知状态、也不弹窗：引擎仍会收到帧（协议层不该懂功能开关），在这里丢掉。
            if !crate::features::enabled(crate::features::Module::Notifications) {
                return;
            }
            // 收到必须留痕：排"通知不同步"时手机侧证明确实发了、电脑侧却毫无痕迹。标题/正文不进日志。
            debuglog::log!(
                debuglog::Level::Info,
                "app",
                "notif.rx",
                &[
                    ("pkg", package.as_str()),
                    ("title_len", &title.chars().count().to_string()),
                    ("text_len", &text.chars().count().to_string()),
                    // 回复入口的判据也要留痕：用户说"怎么没有回复框"时，这一行能分清
                    // 是应用没挂 RemoteInput，还是我们没收到
                    ("id", &notification_id.to_string()),
                    ("tag", tag.as_str()),
                    ("reply", if can_reply { "1" } else { "0" }),
                ]
            );
            // 联动系统消息通知：手机来的通知在电脑上弹一条弹窗（由 UI 线程执行）；两个持久化设置项——弹窗总开关、是否显示正文。
            if st.toast_enabled {
                let title_for_toast = if title.is_empty() {
                    package.clone()
                } else {
                    title.clone()
                };
                let body = if !st.toast_show_content {
                    "收到一条通知（正文已隐藏）".to_string()
                } else if text.is_empty() {
                    "（内容未同步）".to_string()
                } else {
                    text.clone()
                };
                st.push_toast(title_for_toast, body);
            }
            st.push_notification(NotificationItem {
                package,
                title,
                text,
                ts_ms: post_ts_ms,
                key_hash,
                tag,
                notification_id,
                can_reply,
                reply_action_index,
                reply_result_key,
            });
        }
        EngineEvent::NotifyReplyAck {
            reply_id,
            package,
            ok,
            error,
        } => {
            // 回执对上号才落提示；对不上号必须出声，否则"手机回了但电脑没显示"是最难查的形态
            let landed = st.apply_reply_ack(reply_id, ok, &error);
            debuglog::log!(
                debuglog::Level::Info,
                "app",
                "notif.reply.ack",
                &[
                    ("reply_id", &reply_id.to_string()),
                    ("pkg", package.as_str()),
                    ("ok", if ok { "1" } else { "0" }),
                    ("err_len", &error.chars().count().to_string()),
                    ("matched", if landed { "1" } else { "0" }),
                ]
            );
            if !landed {
                st.push_error(format!("收到一条不认识的回执（第 {reply_id} 次回复）"));
            }
        }
        // 回复请求是手机侧的执行入口，电脑自己永远不会收到它（同 MediaState / DeviceStatus 的口径）
        EngineEvent::NotifyReplyRequested { .. } => {}
        EngineEvent::NotifyDismissed {
            package,
            tag,
            notification_id,
            ..
        } => {
            // 通知在手机上消失了：撤掉回复入口，那一行留着（正文还能复制）
            let dropped = st.mark_notification_gone(&package, &tag, notification_id);
            debuglog::log!(
                debuglog::Level::Info,
                "app",
                "notif.dismiss",
                &[
                    ("pkg", package.as_str()),
                    ("id", &notification_id.to_string()),
                    ("tag", tag.as_str()),
                    ("entry", if dropped { "removed" } else { "none" }),
                ]
            );
        }
        EngineEvent::Clipboard { text } => {
            if !crate::features::enabled(crate::features::Module::Clipboard) {
                // 开关已关：不写剪贴板、不更新 UI。对端仍可能推过来，静默丢弃即可。
                return;
            }
            st.clip_in = text.clone();
            st.last_applied_clip = text.clone(); // 防回声：WM_CLIPBOARDUPDATE 读到相同内容则忽略
            clipboard::set_text(&text);
        }
        EngineEvent::Error { code, context } => st.push_error(format!("[{code}] {context}")),
        // 手机推来的播放状态：关掉"媒体控制"模块时同样在此丢弃——"手机上正在听什么"是隐私信息，不该越过开关落进状态。
        EngineEvent::MediaState {
            package,
            title,
            artist,
            album,
            playing,
            position_ms,
            duration_ms,
            speed_x100,
            volume,
        } => {
            if !crate::features::enabled(crate::features::Module::MediaControl) {
                return;
            }
            let key =
                crate::state::media_track_key(&package, &title, &artist);
            // 换歌即撤封面：新封面可能还在路上（甚至这首根本没有封面），留着旧图就是错配
            if st.media_cover.as_ref().map(|c| c.track_key.as_str()) != Some(key.as_str()) {
                st.media_cover = None;
            }
            st.media = Some(crate::state::MediaView {
                package,
                title,
                artist,
                album,
                playing,
                position_ms,
                duration_ms,
                speed_x100,
                volume,
            });
        }
        // 封面与"媒体控制"开关同口径：开关关了，连"手机上正在放什么"都不落进状态，图更不能例外。
        EngineEvent::MediaCover { track_key, jpeg } => {
            if !crate::features::enabled(crate::features::Module::MediaControl) {
                return;
            }
            st.media_cover = Some(crate::state::MediaCoverView { track_key, jpeg });
        }
        // 播放指令是"电脑 → 手机"方向的，Windows 收到它说明对端把方向搞反了：不执行、也不静默，留一条错误。
        EngineEvent::MediaCommand { .. } => {
            st.push_error("收到对端发来的播放指令（本端是指令发起方，已忽略）".to_string())
        }
        EngineEvent::DeviceStatus {
            battery,
            charging,
            ts_ms,
        } => {
            // 对端读数一律当"外部输入"处理：越界值收敛到 -1（读不到），否则进度条/图标会按一个不存在的百分比去画。
            let level = if (0..=100).contains(&battery) {
                battery
            } else {
                debuglog::log!(
                    debuglog::Level::Warn,
                    "ui",
                    "battery.out_of_range",
                    &[("value", &battery.to_string())]
                );
                -1
            };
            // 旧读数不得覆盖新读数：两条状态帧可能走了不同通道（见 `BatteryView::at_ms`）
            if st.battery.is_some_and(|b| b.at_ms > ts_ms) {
                return;
            }
            st.battery = Some(crate::state::BatteryView {
                level,
                charging,
                at_ms: ts_ms,
            });
        }
        // 文件 / TCP / 配置 / 身份变化类事件由 `Worker::handle_event` 处理，不会走到这里
        EngineEvent::FileMetaReceived { .. }
        | EngineEvent::FileDoneReceived { .. }
        | EngineEvent::FileResumeRequested { .. }
        | EngineEvent::FileTaskFailed { .. }
        | EngineEvent::FileTaskCancelled { .. }
        | EngineEvent::TcpBound
        | EngineEvent::TcpUnbound { .. }
        | EngineEvent::ConfigReceived { .. }
        | EngineEvent::IdentityChanged { .. }
        // 相册的两条**应答**由 `Worker::handle_event` 消费（它才握有在途队列），走到这里说明接线错了，值得留一条错误而不是静默。
        | EngineEvent::AlbumPage { .. }
        | EngineEvent::AlbumThumb { .. } => {
            st.push_error("内部错误：相册应答走错了分发路径，已忽略".to_string());
        }
        // 相册的三条**请求**：电脑是发起方，收到它们只可能对端（手机）把方向搞反了。与 `MediaCommand`
        // 同一口径：不执行、也不静默——协议错位只表现为"对面没反应"时，永远查不下去。
        EngineEvent::AlbumListRequested { .. }
        | EngineEvent::AlbumThumbRequested { .. }
        | EngineEvent::AlbumFullRequested { .. } => st.push_error(
            "收到对端发来的相册请求（本端是发起方，已忽略）".to_string(),
        ),
    }
}

// ---------- 身份 / 信任库持久化 ----------
// 身份与信任库全部在 `crate::identity`（`data_dir` / Noise static / RSA-2048 设备身份 /
// 信任库 TSV / DPAPI），本模块只保留「何时读、何时写、何时清」的编排。

/// 当前 Unix 毫秒时间戳（失败退 0）
pub(crate) fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    /// 广播名与握手自报名是两回事：只拿自报名去等广播，就永远等不到（真机踩过）
    #[test]
    fn reconnect_names_keeps_the_persisted_scan_name_when_the_list_entry_expired() {
        let got = reconnect_names(None, Some("Redmi Note 11T Pro"), "22041216C");
        assert_eq!(
            got,
            vec!["Redmi Note 11T Pro".to_string(), "22041216C".to_string()]
        );
        // 名单里能匹配上的就是它，而不是那个永远不会出现的机型名
        let devices = [(0x1234_u64, "Redmi Note 11T Pro".to_string())];
        assert_eq!(freshest_addr_for(&devices, &got), Some(0x1234));
    }

    /// 三个来源可能重名或为空：去重、保序、空串不进名单
    #[test]
    fn reconnect_names_dedupes_and_drops_blanks() {
        let got = reconnect_names(Some(" 小米 "), Some("小米"), "   ");
        assert_eq!(got, vec!["小米".to_string()]);
    }

    /// 手机换地址后表里同时有新旧两条同名记录：必须取最新那条
    #[test]
    fn reconnect_picks_the_rotated_address_not_the_stale_one() {
        let devices = [
            (0xAAAA_u64, "Redmi Note 11T Pro".to_string()), // 轮换前的旧地址
            (0xBBBB_u64, "别的手机".to_string()),
            (0xCCCC_u64, "Redmi Note 11T Pro".to_string()), // 最新可见
        ];
        let want = ["Redmi Note 11T Pro".to_string()];
        assert_eq!(
            freshest_addr_for(&devices, &want),
            Some(0xCCCC),
            "取前面那条就会拿失效地址硬试"
        );
        // 列表里是 BLE 扫描名，而 `peer_name` 是握手里的机型名，两个都得认
        let both = ["22041216C".to_string(), "Redmi Note 11T Pro".to_string()];
        assert_eq!(
            freshest_addr_for(&devices, &both),
            Some(0xCCCC),
            "只认其中一个名字会让自愈永远等不到广播"
        );
    }

    /// 还没重新广播时必须返回 None，让自愈继续等而不是拿旧地址瞎连
    #[test]
    fn reconnect_waits_when_the_name_has_not_reappeared() {
        let devices = [(0xAAAA_u64, "Redmi Note 11T Pro".to_string())];
        let want = ["Pixel 7".to_string()];
        assert_eq!(freshest_addr_for(&devices, &want), None);
        assert_eq!(
            freshest_addr_for(&[], &["Redmi Note 11T Pro".to_string()]),
            None
        );
    }

    /// 自动拨号认的名字必须**同时**覆盖两个名字源；名字表里指纹不在信任库中的条目一律不算（解绑过/没配对过的设备不该被自动伸手）。
    #[test]
    fn auto_dial_names_bridges_name_sources_only_for_trusted_peers() {
        let trusted = vec![TrustedPeer {
            fingerprint: "3fa76f6244743c27".into(),
            name: "22041216C".into(), // 握手 TLV 里的机型名
        }];
        let hints = vec![
            (
                "3fa76f6244743c27".to_string(),
                "Redmi Note 11T Pro".to_string(),
            ),
            // 已解绑设备留下的线索：必须被丢掉，否则「解绑」下次开机又被拨回来
            ("b091d4e77a2c10ff".to_string(), "已解绑的手机".to_string()),
            ("3fa76f6244743c27".to_string(), "   ".to_string()),
        ];
        let names = auto_dial_names(&trusted, &hints);
        assert_eq!(
            names,
            vec!["Redmi Note 11T Pro".to_string(), "22041216C".to_string()],
            "扫描名在前（列表里就是它），机型名兜底；空白名与未信任指纹都不算"
        );
        // 这份名字拿去扫描列表里认人（两个名字源都要认）
        let devices = [(0xCCCC_u64, "Redmi Note 11T Pro".to_string())];
        assert_eq!(freshest_addr_for(&devices, &names), Some(0xCCCC));
        // 只认机型名的那台手机（ROM 让广播名 = 机型名）也要认得到
        let same = [(0xDDDD_u64, "22041216C".to_string())];
        assert_eq!(freshest_addr_for(&same, &names), Some(0xDDDD));
    }

    /// 开关默认必须是**开**：不要让用户每次都手动点重连
    #[test]
    fn auto_connect_defaults_on() {
        assert!(UiState::default().auto_connect);
        assert!(crate::settings::Settings::default().auto_connect);
    }

    fn status(battery: i32, charging: bool, ts_ms: i64) -> EngineEvent {
        EngineEvent::DeviceStatus {
            battery,
            charging,
            ts_ms,
        }
    }

    /// 两条状态帧可能走不同通道（BLE 与 TCP），后发先至是常态：旧读数必须被丢掉，而不是把百分比改回上一个值。
    #[test]
    fn stale_battery_reading_is_dropped() {
        let mut st = UiState::default();
        apply_event(&mut st, status(72, false, 200));
        assert_eq!(st.battery.map(|b| b.level), Some(72));
        apply_event(&mut st, status(60, false, 100));
        assert_eq!(
            st.battery.map(|b| b.level),
            Some(72),
            "迟到的旧读数不得覆盖新读数"
        );
        apply_event(&mut st, status(55, true, 300));
        assert_eq!(st.battery.map(|b| (b.level, b.charging)), Some((55, true)));
    }

    /// 越界读数收敛成"电量未知"，而不是画一个不存在的百分比。
    #[test]
    fn out_of_range_battery_becomes_unknown() {
        let mut st = UiState::default();
        for bad in [101, -5, i32::MAX] {
            apply_event(&mut st, status(bad, false, 1_000 + bad as i64));
            assert_eq!(st.battery.map(|b| b.level), Some(-1), "{bad} 应判为读不到");
        }
    }

    /// 会话一离开主态，电量与播放状态一起作废：留着会把上一台手机的读数显示给现在这台。
    #[test]
    fn leaving_paired_state_clears_battery() {
        let mut st = UiState::default();
        apply_event(&mut st, status(88, false, 1));
        assert!(st.battery.is_some());
        apply_event(
            &mut st,
            EngineEvent::StateChanged {
                state: state_code::HANDSHAKE,
            },
        );
        assert!(st.battery.is_none(), "未配对时不该继续显示电量");
    }
}
