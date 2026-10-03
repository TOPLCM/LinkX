//! 传输抽象：TCP 帧通道 + BLE 角色桩。
//! 接收侧统一走抗重放滑动窗口：已见 / 过旧 seq → `TransportError::Replay`
//! （BLE 重组路径用 `check_frame_replay` 做同源校验）。

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::time::Duration;

use debuglog::Level;
use linkx_crypto::replay::{ReplayError, ReplayWindow};
use linkx_protocol::frame::{body_len, FrameError, FrameHeader};
use thiserror::Error;

/// 防火墙放行端口（TCP 入站，仅私有网络）
pub const TRANSPORT_TCP_PORT: u16 = 55676;

/// 连接与读超时：Server 侧 accept 后也必须设读超时，否则半开连接会永久阻塞
/// `recv_frame`，长期占住线程与资源。
pub const TRANSPORT_CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
pub const TRANSPORT_READ_TIMEOUT: Duration = Duration::from_secs(15);
/// 写超时：对端不再收包时最迟这么久让 `write_all` 报错、由调用方拆链。Windows 侧整轮
/// 工作由**单条 worker 线程串行**驱动，写侧无限阻塞等于整个应用停摆。
pub const TRANSPORT_WRITE_TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum TransportError {
    #[error("IO 失败: {0}")]
    Io(String),
    #[error("帧解析失败: {0}")]
    Frame(FrameError),
    #[error("连接被关闭")]
    Eof,
    #[error("抗重放拒绝: {0}")]
    Replay(ReplayError),
}

impl From<std::io::Error> for TransportError {
    fn from(e: std::io::Error) -> Self {
        TransportError::Io(e.to_string())
    }
}

/// 帧通道：收发 [13B 帧头][payload][16B tag] 完整帧
/// 注意：tag 仅在 ENCRYPTED 标志位为 1 时存在（业务消息）；心跳（0x70）无 tag。
pub trait FrameChannel {
    /// 发送：`body` = 密文+16B tag（加密帧）或明文 payload（心跳），长度须与帧头一致
    fn send_frame(&mut self, header: FrameHeader, body: &[u8]) -> Result<(), TransportError>;
    /// 阻塞读一帧，返回 (header, body)；实现须做抗重放校验（已见 / 过旧 → `Replay`）。
    fn recv_frame(&mut self) -> Result<(FrameHeader, Vec<u8>), TransportError>;
}

pub struct TcpStreamLink {
    stream: TcpStream,
    /// 接收侧滑动窗口：拒绝重放 / 丢弃过旧帧
    replay: ReplayWindow,
}

/// 链路建立后统一设置的 socket 选项：禁 Nagle + 读超时 + 写超时（主动连接与被动 accept
/// 两条路径同口径）。三者都 `.ok()`：个别平台对某项返回不支持，不该让链路建立失败。
/// 写超时尤其必须有——worker 单线程串行，写侧无限阻塞等于整个应用停摆。
fn apply_link_socket_opts(stream: &TcpStream) {
    stream.set_nodelay(true).ok();
    stream.set_read_timeout(Some(TRANSPORT_READ_TIMEOUT)).ok();
    stream.set_write_timeout(Some(TRANSPORT_WRITE_TIMEOUT)).ok();
}

impl TcpStreamLink {
    pub fn connect(addr: &str) -> Result<Self, TransportError> {
        // 带超时的连接：`TcpStream::connect` 挂在 OS 默认的 SYN 重试上（Windows 上可达二十余秒），
        // 而调用方是单条 worker —— 它停摆期间收包、tick、心跳全饿死。`TRANSPORT_CONNECT_TIMEOUT`
        // 早就定义好了却一直没接上，等于写了个假防线。
        // 地址不是数字形式（手输了主机名）时 `parse` 失败，退回阻塞连接：名字解析要问 DNS，
        // 这里没有便宜的超时办法，宁可慢也不改变"能填主机名"这件事。
        let opened = match addr.parse::<std::net::SocketAddr>() {
            Ok(sa) => TcpStream::connect_timeout(&sa, TRANSPORT_CONNECT_TIMEOUT),
            Err(_) => TcpStream::connect(addr),
        };
        match opened {
            Ok(stream) => {
                apply_link_socket_opts(&stream);
                debuglog::log!(Level::Info, "lan", "tcp.connect", &[("peer", addr)]);
                Ok(Self {
                    stream,
                    replay: ReplayWindow::default(),
                })
            }
            Err(e) => {
                debuglog::log!(
                    Level::Warn,
                    "lan",
                    "tcp.connect.fail",
                    &[("peer", addr), ("err", &e.to_string())]
                );
                Err(e.into())
            }
        }
    }

    pub fn bind(addr: &str) -> Result<TcpListener, TransportError> {
        Ok(TcpListener::bind(addr)?)
    }

    /// Server 侧构造：同步设置读超时，半开连接才不会永久阻塞
    pub fn from_stream(stream: TcpStream) -> Self {
        // 监听口为能优雅停止设了 `set_nonblocking(true)`，这里显式改回阻塞：读写两侧都按
        // 阻塞语义写。半帧落网等于这条流从此永久错位，不能靠平台默认行为赌。
        stream.set_nonblocking(false).ok();
        apply_link_socket_opts(&stream);
        // 这里**不打** `tcp.accept`：一条入站连接会构造两次 `TcpStreamLink`（写侧 try_clone
        // + 读侧各一次），日志会出现"同一毫秒 accept 两次"而把人引向"对端建了两条连接"
        // 这个错误方向。连接建立的埋点由调用方（`accept_loop`）打，一次 accept 一条。
        Self {
            stream,
            replay: ReplayWindow::default(),
        }
    }

    pub fn peer_addr(&self) -> Result<String, TransportError> {
        Ok(self.stream.peer_addr()?.to_string())
    }

    pub fn replay_window(&self) -> &ReplayWindow {
        &self.replay
    }

    /// 读一整帧但**不做**抗重放校验：`StreamLink` 以此实现「解密成功后才提交窗口」。
    pub fn read_frame(&mut self) -> Result<(FrameHeader, Vec<u8>), TransportError> {
        let mut head = [0u8; 13];
        if let Err(e) = read_exact_or_eof(&mut self.stream, &mut head) {
            debuglog::log!(
                Level::Warn,
                "lan",
                "tcp.read_error",
                &[("stage", "header"), ("err", &e.to_string())]
            );
            return Err(e);
        }
        let header = match FrameHeader::parse(&head) {
            Ok(h) => h,
            Err(e) => {
                debuglog::log!(
                    Level::Warn,
                    "lan",
                    "tcp.read_error",
                    &[("stage", "parse"), ("err", &e.to_string())]
                );
                return Err(TransportError::Frame(e));
            }
        };
        let mut body = vec![0u8; body_len(&header)];
        if let Err(e) = read_exact_or_eof(&mut self.stream, &mut body) {
            debuglog::log!(
                Level::Warn,
                "lan",
                "tcp.read_error",
                &[("stage", "body"), ("err", &e.to_string())]
            );
            return Err(e);
        }
        Ok((header, body))
    }

    /// 「超出窗口直接丢弃」语义：跳过重放 / 过旧帧继续读，最多 `max_skips` 帧，超出报
    /// `Replay`（防被恶意帧流拖死）。本方法**读到帧即提交窗口**，只适用于无 AEAD 认证的
    /// 裸帧路径；带加密的路径用 `read_frame` + 上层解密后再提交。
    pub fn recv_frame_discarding_replays(
        &mut self,
        max_skips: usize,
    ) -> Result<(FrameHeader, Vec<u8>), TransportError> {
        let mut skipped = 0usize;
        loop {
            let (header, body) = self.read_frame()?;
            match self.replay.check_and_update(header.seq) {
                Ok(()) => return Ok((header, body)),
                Err(e) => {
                    // 帧已完整消费，流保持对齐；丢弃后继续
                    skipped += 1;
                    if skipped > max_skips {
                        return Err(TransportError::Replay(e));
                    }
                }
            }
        }
    }

    /// 只读校验（不提交窗口）——供上层在认证成功后自行 `commit`
    pub fn check_seq(&self, seq: u32) -> Result<(), TransportError> {
        self.replay.check(seq).map_err(TransportError::Replay)
    }

    pub fn commit_seq(&mut self, seq: u32) {
        self.replay.commit(seq);
    }

    /// 读出**完整帧字节**（13B 帧头 + body），可整体交给 `SessionEngine::feed_tcp`；
    /// 抗重放窗口的提交留给引擎在解密成功之后做。
    pub fn read_raw_frame(&mut self) -> Result<Vec<u8>, TransportError> {
        let mut head = [0u8; 13];
        read_exact_or_eof(&mut self.stream, &mut head)?;
        let header = FrameHeader::parse(&head).map_err(TransportError::Frame)?;
        let mut body = vec![0u8; body_len(&header)];
        read_exact_or_eof(&mut self.stream, &mut body)?;
        let mut out = Vec::with_capacity(13 + body.len());
        out.extend_from_slice(&head);
        out.extend_from_slice(&body);
        Ok(out)
    }

    /// 写出一个由 `SessionEngine::take_tcp_outbound` 产出的完整帧（加解密已由引擎完成）
    pub fn write_raw(&mut self, frame: &[u8]) -> Result<(), TransportError> {
        self.stream.write_all(frame)?;
        Ok(())
    }
}

impl FrameChannel for TcpStreamLink {
    fn send_frame(&mut self, header: FrameHeader, body: &[u8]) -> Result<(), TransportError> {
        if body.len() != body_len(&header) {
            return Err(TransportError::Frame(FrameError::Truncated {
                need: body_len(&header),
                have: body.len(),
            }));
        }
        let mut frame = Vec::with_capacity(13 + body.len());
        frame.extend_from_slice(&header.encode());
        frame.extend_from_slice(body);
        self.stream.write_all(&frame)?;
        Ok(())
    }

    /// 读一帧并做抗重放校验（无 AEAD 可依赖，故读到即提交）；重放 / 过旧返回 `Replay`。
    fn recv_frame(&mut self) -> Result<(FrameHeader, Vec<u8>), TransportError> {
        let (header, body) = self.read_frame()?;
        self.replay
            .check_and_update(header.seq)
            .map_err(TransportError::Replay)?;
        Ok((header, body))
    }
}

/// BLE 接收路径接入点：重组出整帧后调用，与 TCP 路径共用同一套滑动窗口语义。
pub fn check_frame_replay(
    window: &mut ReplayWindow,
    frame: &[u8],
) -> Result<FrameHeader, TransportError> {
    let header = FrameHeader::parse(frame).map_err(TransportError::Frame)?;
    window
        .check_and_update(header.seq)
        .map_err(TransportError::Replay)?;
    Ok(header)
}

fn read_exact_or_eof<R: Read>(r: &mut R, buf: &mut [u8]) -> Result<(), TransportError> {
    let mut read = 0usize;
    while read < buf.len() {
        let n = r.read(&mut buf[read..])?;
        if n == 0 {
            return Err(TransportError::Eof);
        }
        read += n;
    }
    Ok(())
}

/// BLE UUID 常量（产品自有命名空间 "LX"）
#[allow(non_snake_case)]
pub mod BleUuid {
    /// GATT Service：`4C584C00-0000-1000-8000-00805F9B34FB`
    pub const SERVICE: &str = "4c584c00-0000-1000-8000-00805f9b34fb";
    /// Central→Peripheral 写指令（普通 MTU 包 / 分片流写入）
    pub const CHAR_TX: &str = "4c584c01-0000-1000-8000-00805f9b34fb";
    /// Peripheral→Central 通知（分片流读出）
    pub const CHAR_EVT: &str = "4c584c02-0000-1000-8000-00805f9b34fb";
}

pub trait BleCentralReceiver {
    fn on_connect(&mut self, peer: &str);
}

pub trait BlePeripheralSender {
    fn notify(&mut self, payload: &[u8]) -> Result<(), TransportError>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use linkx_protocol::msg_type::CLIPBOARD_PUSH;

    fn tag16() -> Vec<u8> {
        vec![0xEE; 16]
    }

    #[test]
    fn tcp_frame_roundtrip_local() {
        let listener = TcpStreamLink::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let server = std::thread::spawn(move || {
            let (sock, _) = listener.accept().unwrap();
            let mut link = TcpStreamLink::from_stream(sock);
            link.recv_frame().unwrap()
        });

        let mut client = TcpStreamLink::connect(&addr).unwrap();
        let hdr = FrameHeader::default_encrypted(CLIPBOARD_PUSH, 1, 4);
        let mut body = vec![0x01, 0x02, 0x03, 0x04]; // 密文
        body.extend_from_slice(&tag16()); // +16B AEAD tag
        client.send_frame(hdr, &body).unwrap();

        let (rhdr, rbody) = server.join().unwrap();
        assert_eq!(rhdr, hdr);
        assert_eq!(rbody.len(), 4 + 16);
        assert_eq!(&rbody[..4], &[0x01, 0x02, 0x03, 0x04]);
        assert_eq!(&rbody[4..], &tag16()[..]);
    }

    #[test]
    fn heartbeat_frame_has_no_tag() {
        let listener = TcpStreamLink::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let server = std::thread::spawn(move || {
            let (sock, _) = listener.accept().unwrap();
            let mut link = TcpStreamLink::from_stream(sock);
            link.recv_frame().unwrap()
        });
        let mut client = TcpStreamLink::connect(&addr).unwrap();
        let hdr = FrameHeader::new(linkx_protocol::msg_type::HEARTBEAT, 0, 7, 4);
        client.send_frame(hdr, &[0x20, 0x01, 0x00, 0x01]).unwrap(); // TLV PING
        let (rhdr, rbody) = server.join().unwrap();
        assert_eq!(rhdr, hdr);
        assert_eq!(rbody, vec![0x20, 0x01, 0x00, 0x01]);
    }

    #[test]
    fn replay_frame_is_rejected() {
        let listener = TcpStreamLink::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let server = std::thread::spawn(move || {
            let (sock, _) = listener.accept().unwrap();
            let mut link = TcpStreamLink::from_stream(sock);
            let first = link.recv_frame();
            let second = link.recv_frame();
            (first.is_ok(), second)
        });
        let mut client = TcpStreamLink::connect(&addr).unwrap();
        let hdr = FrameHeader::default_encrypted(CLIPBOARD_PUSH, 5, 2);
        let mut body = vec![0xAA, 0xBB];
        body.extend_from_slice(&tag16());
        client.send_frame(hdr, &body).unwrap();
        client.send_frame(hdr, &body).unwrap(); // 重放同一帧
        let (first_ok, second) = server.join().unwrap();
        assert!(first_ok);
        assert!(matches!(
            second,
            Err(TransportError::Replay(ReplayError::Replay(5)))
        ));
    }

    #[test]
    fn too_old_frame_is_rejected() {
        let listener = TcpStreamLink::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let server = std::thread::spawn(move || {
            let (sock, _) = listener.accept().unwrap();
            let mut link = TcpStreamLink::from_stream(sock);
            let _ = link.recv_frame(); // seq=100
            link.recv_frame() // seq=50 → 过旧
        });
        let mut client = TcpStreamLink::connect(&addr).unwrap();
        for seq in [100u32, 50u32] {
            let hdr = FrameHeader::default_encrypted(CLIPBOARD_PUSH, seq, 1);
            let mut body = vec![0x11];
            body.extend_from_slice(&tag16());
            client.send_frame(hdr, &body).unwrap();
        }
        assert!(matches!(
            server.join().unwrap(),
            Err(TransportError::Replay(ReplayError::TooOld { seq: 50, .. }))
        ));
    }

    #[test]
    fn discarding_replays_skips_to_next_valid() {
        let listener = TcpStreamLink::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let server = std::thread::spawn(move || {
            let (sock, _) = listener.accept().unwrap();
            let mut link = TcpStreamLink::from_stream(sock);
            let a = link.recv_frame_discarding_replays(4).unwrap().0.seq;
            let b = link.recv_frame_discarding_replays(4).unwrap().0.seq;
            (a, b)
        });
        let mut client = TcpStreamLink::connect(&addr).unwrap();
        for seq in [1u32, 1u32, 2u32] {
            let hdr = FrameHeader::default_encrypted(CLIPBOARD_PUSH, seq, 1);
            let mut body = vec![0x22];
            body.extend_from_slice(&tag16());
            client.send_frame(hdr, &body).unwrap();
        }
        assert_eq!(server.join().unwrap(), (1, 2));
    }

    #[test]
    fn ble_reassembled_frame_replay_hook() {
        let hdr = FrameHeader::default_encrypted(CLIPBOARD_PUSH, 9, 1);
        let mut body = vec![0x33];
        body.extend_from_slice(&tag16());
        let frame = linkx_protocol::frame::assemble_frame(&hdr, &body).unwrap();
        let mut w = ReplayWindow::default();
        assert_eq!(check_frame_replay(&mut w, &frame).unwrap(), hdr);
        assert!(matches!(
            check_frame_replay(&mut w, &frame),
            Err(TransportError::Replay(ReplayError::Replay(9)))
        ));
        // 畸形帧不 panic
        assert!(matches!(
            check_frame_replay(&mut w, &[0u8; 3]),
            Err(TransportError::Frame(_))
        ));
    }

    #[test]
    fn tcp_eof_and_truncation() {
        let listener = TcpStreamLink::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let server = std::thread::spawn(move || {
            let (sock, _) = listener.accept().unwrap();
            let mut link = TcpStreamLink::from_stream(sock);
            let r = link.recv_frame();
            matches!(r, Err(TransportError::Eof) | Err(TransportError::Io(_)))
        });
        // 只发半帧（13B 头 + 7B body），随后断连
        let mut client = TcpStreamLink::connect(&addr).unwrap();
        let hdr = FrameHeader::default_encrypted(CLIPBOARD_PUSH, 2, 100);
        let mut frame = Vec::new();
        frame.extend_from_slice(&hdr.encode());
        frame.extend_from_slice(&[0u8; 100]);
        frame.extend_from_slice(&tag16());
        client.stream.write_all(&frame[..20]).unwrap();
        client.stream.shutdown(std::net::Shutdown::Both).ok();
        assert!(server.join().unwrap());
    }

    /// 传输面不变量：**慢对端下大帧必须整帧写出，对端收到的字节数一字节不差**（截断＝静默数据
    /// 损坏）。注意本用例**并不能**复现 `os error 10035`：Windows 上 `accept()` 出的连接实测
    /// **不继承**监听口的非阻塞模式，删掉 `from_stream` 里的 `set_nonblocking(false)` 它照样通过
    /// （已用"删掉修复再跑"验证过）——守的是"不许静默截断"这条底线，不靠本用例定位那个错误。
    /// 复现要点：对端**先不读**。缓冲填满之后还继续写才分得出"等排空"与"直接失败"；边连边排空
    /// 的话回环上根本填不满缓冲，那种测试等于没测。
    #[test]
    fn accepted_link_writes_large_frames_when_peer_is_slow() {
        use std::io::Read as _;
        let listener = TcpStreamLink::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();

        // 对端连上后先晾 1.5 s（期间不读），再开始排空
        let peer = std::thread::spawn(move || {
            let mut c = TcpStream::connect(addr).unwrap();
            std::thread::sleep(Duration::from_millis(1500));
            let mut buf = vec![0u8; 64 * 1024];
            let mut total = 0usize;
            while let Ok(n) = c.read(&mut buf) {
                if n == 0 {
                    break;
                }
                total += n;
            }
            total
        });

        let (sock, _) = loop {
            match listener.accept() {
                Ok(v) => break v,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(2))
                }
                Err(e) => panic!("accept 失败: {e}"),
            }
        };
        let write_sock = sock.try_clone().unwrap();
        let mut link = TcpStreamLink::from_stream(write_sock);
        // 两帧共 2 MB：第一帧填满发送缓冲后，第二帧必然要等对端排空
        let frame = vec![0xA5u8; 1024 * 1024];
        link.write_raw(&frame).expect("第一帧写出失败");
        link.write_raw(&frame)
            .expect("大帧必须完整写出（继承非阻塞时这里会 10035）");
        // 读侧原始 socket 也必须放掉：对端靠 EOF 退出排空循环，只 drop(link) 会永久阻塞 join。
        drop(link);
        drop(sock);

        assert_eq!(
            peer.join().unwrap(),
            frame.len() * 2,
            "对端收到的字节数必须等于两帧总长，少一个字节都是静默损坏"
        );
    }
}
