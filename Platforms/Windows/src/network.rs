//! TCP 服务与链路（分层：平台壳负责 socket IO，引擎只管协议与校验）。Windows 为 TCP 服务端：常驻 accept 线程（非阻塞轮询便于优雅停止）+ 每连接的阻塞读循环。

use std::collections::VecDeque;
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use linkx_lan::{TcpStreamLink, TransportError, TRANSPORT_TCP_PORT};

const MAX_TCP_RX_FRAMES: usize = 64;
/// 入站帧队列的**字节**上限。按字节封顶才和"对端发多大的文件"无关：旧的 256 条 × 256 KB 等于允许 64 MB 只压在接收队列里，既顶破内存红线，又会在超出时"丢最旧" —— 60 MB 刚好不越线、76 MB 必翻车
const MAX_TCP_RX_BYTES: usize = 8 * 1024 * 1024;

fn queued_bytes(q: &VecDeque<(u64, Vec<u8>)>) -> usize {
    q.iter().map(|(_, f)| f.len()).sum()
}

/// 入站帧队列 `(连接号, 帧)`。连接号必须随帧一起走：worker 按轮取帧，对端换目标重连时旧连接的残留帧会和新连接的混在同一个队列里 —— 把**上一条**连接的 CHANNEL_BIND 当成本次绑定的应答，就永远绑不上
type TcpRx = Arc<Mutex<VecDeque<(u64, Vec<u8>)>>>;

type TcpRxRoom = Arc<Condvar>;

/// 读线程最多能"等 worker 腾位置"等多久：超过就说明没人取了（绑定没起来、worker 卡死、链路被判死）。这时**大声断开** —— 一次只维持一条连接，把 accept 线程钉在这里就等于手机再也连不上
const MAX_PARK_TIME: Duration = Duration::from_secs(15);

/// 这一帧入队会不会越过预算（越过就让读线程等一等，而不是把旧帧挤掉）
fn rx_full(q: &VecDeque<(u64, Vec<u8>)>, incoming: usize) -> bool {
    // **队列为空时必须放行**：读线程是 accept 线程同步调用的，在这里等 = 既不 accept 也不上报
    // 断开。单帧上限已收到 1 MiB（`MAX_FRAME_PAYLOAD`，一帧自己绝超不过 8 MiB 预算），
    // 但这条判据不跟着收：一旦哪天预算与上限再被反向调过，代价是"一个合法大帧把链路钉死"。
    !q.is_empty() && (q.len() >= MAX_TCP_RX_FRAMES || queued_bytes(q) + incoming > MAX_TCP_RX_BYTES)
}

pub(crate) struct TcpService {
    /// 写侧链路（worker 持有；`None` = 当前无连接 / 本端已判死）
    link: Arc<Mutex<Option<TcpStreamLink>>>,
    pub(crate) rx: TcpRx,
    pub(crate) rx_room: TcpRxRoom,
    pub(crate) pending_accept: Arc<Mutex<Option<(u64, SocketAddr)>>>,
    pub(crate) closed: Arc<Mutex<Option<(u64, String)>>>,
    running: Arc<AtomicBool>,
}

impl TcpService {
    pub(crate) fn start() -> Result<Self, String> {
        let listener = TcpStreamLink::bind(&format!("0.0.0.0:{TRANSPORT_TCP_PORT}"))
            .map_err(|e| format!("TCP 监听 {TRANSPORT_TCP_PORT} 失败: {e}"))?;
        // 阻塞 accept 会拖住「停止」，故非阻塞轮询以便及时释放端口
        listener
            .set_nonblocking(true)
            .map_err(|e| format!("设置非阻塞失败: {e}"))?;

        let link: Arc<Mutex<Option<TcpStreamLink>>> = Arc::new(Mutex::new(None));
        let rx: TcpRx = Arc::new(Mutex::new(VecDeque::new()));
        let rx_room: TcpRxRoom = Arc::new(Condvar::new());
        let pending_accept: Arc<Mutex<Option<(u64, SocketAddr)>>> = Arc::new(Mutex::new(None));
        let closed: Arc<Mutex<Option<(u64, String)>>> = Arc::new(Mutex::new(None));
        let conn_id = Arc::new(AtomicU64::new(0));
        let running = Arc::new(AtomicBool::new(true));

        let shared = ConnShared {
            link: link.clone(),
            rx: rx.clone(),
            rx_room: rx_room.clone(),
            pending_accept: pending_accept.clone(),
            closed: closed.clone(),
        };
        let (cid, ru) = (conn_id.clone(), running.clone());
        std::thread::spawn(move || accept_loop(listener, ru, cid, shared));
        crate::say(format!(
            "[LinkX] TCP 服务端已监听 0.0.0.0:{TRANSPORT_TCP_PORT}"
        ));
        Ok(Self {
            link,
            rx,
            rx_room,
            pending_accept,
            closed,
            running,
        })
    }

    pub(crate) fn write_frames(&self, frames: &[Vec<u8>]) -> Result<(), String> {
        let mut guard = self.link.lock().unwrap();
        let Some(link) = guard.as_mut() else {
            return Err("连接已关闭".to_string());
        };
        for f in frames {
            link.write_raw(f).map_err(|e| e.to_string())?;
        }
        Ok(())
    }

    /// 丢弃**本端**的写侧句柄。一条连接有两个句柄（写侧 clone + 读侧），内核要等最后一个也放掉才真正关，所以这里只是把"本端已判死"记在槽位上，由 [read_loop] 在下一次空闲超时时看到它并收尾
    pub(crate) fn close_link(&self) {
        *self.link.lock().unwrap() = None;
    }

    pub(crate) fn has_link(&self) -> bool {
        self.link.lock().unwrap().is_some()
    }

    /// 停止服务（accept 线程最多在一个读超时周期内退出并释放端口）
    pub(crate) fn stop(&self) {
        self.running.store(false, Ordering::SeqCst);
        self.close_link();
        self.rx_room.notify_all();
    }
}

/// accept / read 两个线程共用的通道状态：一次 clone 全带上，而不是把五条 `Arc` 一个个当参数往下传
#[derive(Clone)]
struct ConnShared {
    link: Arc<Mutex<Option<TcpStreamLink>>>,
    rx: TcpRx,
    rx_room: TcpRxRoom,
    pending_accept: Arc<Mutex<Option<(u64, SocketAddr)>>>,
    closed: Arc<Mutex<Option<(u64, String)>>>,
}

/// accept 循环：一次只维持一条连接（对端断开后回到监听，支持重连）。每条连接发一个递增 id 并让 `pending_accept`/`closed`
/// 都带上它：没有它，对端换目标重连时 worker 会在同一轮里先按新连接发起绑定、再把**上一条**连接的关闭事件当成当前连接处理，`close_link()` 正好关掉刚建好的新链路
fn accept_loop(
    listener: TcpListener,
    running: Arc<AtomicBool>,
    conn_id: Arc<AtomicU64>,
    sh: ConnShared,
) {
    while running.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((sock, addr)) => {
                if !running.load(Ordering::SeqCst) {
                    break;
                }
                let write_sock = match sock.try_clone() {
                    Ok(s) => s,
                    Err(e) => {
                        set_closed(&sh.closed, 0, format!("复制 TCP 句柄失败: {e}"));
                        continue;
                    }
                };
                let id = conn_id.fetch_add(1, Ordering::SeqCst) + 1;
                // 埋点一次 accept 恰好一条并带上连接号：之前打在 `from_stream` 里，而一条连接要构造两次（写侧 clone + 读侧），日志表现为"同一毫秒 accept 两次"
                debuglog::log!(
                    debuglog::Level::Info,
                    "lan",
                    "tcp.accept",
                    &[("conn", &id.to_string()), ("addr", &addr.to_string())]
                );
                // 顺序要紧：先挂写链路、再置「待绑定」、最后才起读循环 —— 保证 worker 喂入任何 TCP 帧之前已调用 begin_tcp_binding
                *sh.link.lock().unwrap() = Some(TcpStreamLink::from_stream(write_sock));
                *sh.pending_accept.lock().unwrap() = Some((id, addr));
                crate::say(format!("[LinkX] TCP 对端已连接 #{id} {addr}"));
                read_loop(sock, id, &sh, &running);
                *sh.link.lock().unwrap() = None;
                if !running.load(Ordering::SeqCst) {
                    break;
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(40));
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => {
                set_closed(&sh.closed, 0, format!("TCP accept 失败: {e}"));
                std::thread::sleep(Duration::from_millis(200));
            }
        }
    }
}

/// 阻塞读完整帧 → 入站队列，直至对端断开、本端判死这条连接、或服务停止。
/// **队列满的时候等，不丢帧**：TCP 是可靠字节流，从可靠流里"丢一帧"不会变成一次重传，只会变成文件里一个永久的洞，然后两端各自认为这次传输成功了。
/// 读线程停下来才是正确处置：socket 缓冲区填满、流量控制把压力顶回对端
fn read_loop(sock: TcpStream, id: u64, sh: &ConnShared, running: &Arc<AtomicBool>) {
    let mut link = TcpStreamLink::from_stream(sock);
    // 背压留痕的节流计数：真机传 221 MB 时"每帧一条"会刷出上千行，把日志本身变成噪音
    let mut parked: u64 = 0;
    while running.load(Ordering::SeqCst) {
        match link.read_raw_frame() {
            Ok(frame) => {
                let need = frame.len();
                let mut q = sh.rx.lock().unwrap();
                // 带超时地等：worker 万一不再取，这里最多卡一个周期就会因 `running` 转 false 而收线
                let mut waited = Duration::ZERO;
                while rx_full(&q, need) && running.load(Ordering::SeqCst) {
                    let (g, r) = sh
                        .rx_room
                        .wait_timeout(q, Duration::from_millis(100))
                        .unwrap();
                    q = g;
                    if r.timed_out() {
                        waited += Duration::from_millis(100);
                    }
                    parked += 1;
                    if parked % 64 == 1 {
                        // 留痕但不报警：背压是设计意图，出现只说明本机落盘跟不上对端发送
                        debuglog::log!(
                            debuglog::Level::Info,
                            "lan",
                            "tcp.rx.backpressure",
                            &[
                                ("conn", &id.to_string()),
                                ("queued", &q.len().to_string()),
                                ("episodes", &parked.to_string())
                            ]
                        );
                    }
                    if waited >= MAX_PARK_TIME {
                        // 没人取帧：不能无限期霸着这条 socket。大声判死让上层重连，否则 accept 线程被钉在这里，手机再也连不上且界面一切"正常"
                        drop(q);
                        set_closed(
                            &sh.closed,
                            id,
                            format!(
                                "入站队列 {:?} 内无人取帧，本机吃不下了，主动断开",
                                MAX_PARK_TIME
                            ),
                        );
                        return;
                    }
                }
                if !running.load(Ordering::SeqCst) {
                    return;
                }
                q.push_back((id, frame));
            }
            Err(TransportError::Io(msg)) if is_idle_io(&msg) => {
                // 每次从空闲读超时醒来都要看一眼写侧槽位：worker 判死这条连接只会 `close_link()` 放掉它那一份句柄，socket 并不会因此关掉。
                // 对端"半开"（手机被杀 / 关 Wi-Fi / Doze 冻结，没发 FIN）时这里永远等不到 EOF，而 accept 一次只维持一条连接 →
                // 手机重连只能进 backlog，**用户不重启电脑端就永远接不上**。现在最多等一个读超时周期就收尾：两个句柄都 drop 之后 socket 才真正关掉
                if sh.link.lock().unwrap().is_none() {
                    crate::say(format!("[LinkX] TCP #{id} 本端已判死，收线后回到监听"));
                    return;
                }
                continue;
            }
            Err(e) => {
                set_closed(&sh.closed, id, e.to_string());
                return;
            }
        }
    }
}

/// 读断开原因（仅在为空时写，避免覆盖首个真正原因）
fn set_closed(slot: &Arc<Mutex<Option<(u64, String)>>>, id: u64, reason: String) {
    let mut g = slot.lock().unwrap();
    if g.is_none() {
        *g = Some((id, reason));
    }
}

/// 读超时/无数据可读（不是连接故障）的 IO 错误判定；错误文本来自 `io::Error::to_string()`
fn is_idle_io(msg: &str) -> bool {
    let m = msg.to_ascii_lowercase();
    m.contains("timed out")
        || m.contains("would block")
        || m.contains("10060") // WSAETIMEDOUT
        || m.contains("10035") // WSAEWOULDBLOCK
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 入站队列只许"等"不许"丢"的回归钉：旧实现在队列满时 `pop_front()` 挤掉最旧一帧，76 MB 的视频因此少两块、电脑判"校验失败"，
    #[test]
    fn inbound_queue_presses_back_instead_of_evicting() {
        let chunk = 256 * 1024;
        let mut q: VecDeque<(u64, Vec<u8>)> = VecDeque::new();
        for i in 0..MAX_TCP_RX_FRAMES {
            q.push_back((1, vec![i as u8; chunk]));
        }
        assert!(
            rx_full(&q, chunk),
            "条数到顶必须转为背压；此时若再 pop_front 就是在可靠流上造洞"
        );
        let mut fat: VecDeque<(u64, Vec<u8>)> = VecDeque::new();
        fat.push_back((1, vec![0u8; MAX_TCP_RX_BYTES]));
        assert!(rx_full(&fat, 1), "字节到顶同样要背压");
        let mut thin: VecDeque<(u64, Vec<u8>)> = VecDeque::new();
        thin.push_back((1, vec![0u8; 1024]));
        assert!(!rx_full(&thin, 1024));
    }

    /// 空队列必须放行，哪怕这一帧自己就超预算（协议允许单帧 8 MiB）：读线程由 accept 线程同步调用，在那里等就是让整条链路钉死
    #[test]
    fn oversized_frame_still_gets_the_empty_queue() {
        let empty: VecDeque<(u64, Vec<u8>)> = VecDeque::new();
        assert!(
            !rx_full(&empty, MAX_TCP_RX_BYTES * 4),
            "空队列必须收下单帧上限这么大的帧，否则读线程会把自己钉死"
        );
        let mut one = VecDeque::new();
        one.push_back((1, vec![0u8; MAX_TCP_RX_BYTES]));
        assert!(rx_full(&one, 1), "队列里已经有东西了才谈背压");
    }
}
