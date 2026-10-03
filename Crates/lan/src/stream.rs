//! TCP 加密流（StreamLink）：局域网通道的加密承载，在帧通道之上叠加三件事：
//!
//! 1. **加解密**：业务帧 ChaCha20-Poly1305（ENCRYPTED 置位，线上 body = 密文 + 16B tag；
//!    nonce = HMAC(session_key, seq || session_id)，见 `linkx_crypto::cipher`）。
//! 2. **心跳直通**：`HEARTBEAT`(0x70) 不加密、无 tag、不参与 binding 门禁，由上层按
//!    TCP 10s PING / 10s 超时驱动。
//! 3. **Channel Binding 门禁**：未 bound 前拒收 / 拒发业务消息，仅放行 PING/PONG。
//!
//! 手动 IP 兜底复用同一入口：`parse_manual_addr` 得到地址后直接 `StreamLink::connect`。

use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

use debuglog::Level;
use linkx_crypto::cipher::{
    decrypt_payload, encrypt_payload, CipherError, DIR_INITIATOR_TO_RESPONDER,
    DIR_RESPONDER_TO_INITIATOR,
};
use linkx_crypto::noise::NOISE_KEY_LEN;
use linkx_crypto::replay::{ReplayError, ReplayWindow};
use linkx_protocol::frame::{flags, FrameHeader};
use linkx_protocol::msg_type;
use linkx_protocol::tlv_codec;
use linkx_protocol::{TAG_PING, TAG_PONG};
use thiserror::Error;

use crate::transport::{FrameChannel, TcpStreamLink, TransportError};

/// 会话 ID 长度（storage `sessions.session_id` 同为 16B，也是 nonce 的派生输入）
pub const SESSION_ID_LEN: usize = 16;

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum StreamError {
    #[error("传输失败: {0}")]
    Transport(TransportError),
    #[error("加解密失败: {0}")]
    Cipher(CipherError),
    #[error("协议违规: {0}")]
    Protocol(String),
    #[error("未通过 channel binding：TCP 通道仅允许 PING/PONG")]
    NotBound,
    #[error("抗重放拒绝: {0}")]
    Replayed(ReplayError),
    #[error("发送序号已耗尽：需重握手以免 nonce 复用")]
    SeqExhausted,
    #[error("手动地址非法: {0}")]
    BadManualAddr(String),
}

impl From<TransportError> for StreamError {
    fn from(e: TransportError) -> Self {
        StreamError::Transport(e)
    }
}

impl From<CipherError> for StreamError {
    fn from(e: CipherError) -> Self {
        StreamError::Cipher(e)
    }
}

/// 收帧结果：业务帧 payload 为解密后明文，心跳帧为线上明文 TLV
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RxFrame {
    pub msg_type: u8,
    pub flags: u8,
    pub seq: u32,
    pub payload: Vec<u8>,
    /// 心跳帧（0x70）：加密与 binding 门禁均不适用
    pub heartbeat: bool,
}

impl RxFrame {
    pub fn is_ping(&self) -> bool {
        self.heartbeat && tlv_codec::get(&self.payload, TAG_PING).is_ok_and(|v| v.is_some())
    }

    pub fn is_pong(&self) -> bool {
        self.heartbeat && tlv_codec::get(&self.payload, TAG_PONG).is_ok_and(|v| v.is_some())
    }
}

/// 收发计数（诊断 / 日志脱敏：只记类型与数量，不记正文）
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct LinkStats {
    pub sent: u64,
    pub received: u64,
    /// 因未通过 channel binding 被拒的业务帧（收 / 发各计一次口径：本端拒绝数）
    pub binding_rejected: u64,
}

/// 局域网加密流：帧通道 + 会话密钥 + binding 门禁
pub struct StreamLink {
    link: TcpStreamLink,
    key: [u8; NOISE_KEY_LEN],
    session_id: [u8; SESSION_ID_LEN],
    /// 本端发送方向标签（主动 connect 端 = 发起方）
    send_dir: u8,
    send_seq: u32,
    /// 接收侧滑动窗口（在 AEAD 认证成功后才提交）
    recv_window: ReplayWindow,
    /// 心跳帧**独立**的窗口。心跳是明文、没有 AEAD，无法"先认证再提交"；
    /// 与业务帧共用一个窗口时，任何局域网对端发一帧 `seq≈u32::MAX` 的心跳就能把窗口推顶，
    /// 此后所有合法业务帧都判 TooOld —— 文件通道被永久打断。分开就打断不了业务面。
    hb_window: ReplayWindow,
    bound: bool,
    stats: LinkStats,
}

impl StreamLink {
    /// 主动连接（首次 TCP 必须由 Client 侧发起；手动 IP 兜底同路径）
    pub fn connect(
        addr: &str,
        key: [u8; NOISE_KEY_LEN],
        session_id: [u8; SESSION_ID_LEN],
    ) -> Result<Self, StreamError> {
        let link = TcpStreamLink::connect(addr)?;
        debuglog::log!(Level::Info, "lan", "stream.connect", &[("peer", addr)]);
        Ok(Self::from_stream(link, key, session_id))
    }

    /// 由已建立的 TCP 连接构造（Server 侧 accept 后使用）；初始未 bound
    pub fn from_stream(
        link: TcpStreamLink,
        key: [u8; NOISE_KEY_LEN],
        session_id: [u8; SESSION_ID_LEN],
    ) -> Self {
        Self {
            link,
            key,
            session_id,
            // 主动 connect / accept 的角色由调用方语义确定：
            // `connect()` 走本构造函数（发起方）；Server 侧应使用 `from_stream_as_responder`。
            send_dir: DIR_INITIATOR_TO_RESPONDER,
            send_seq: 0,
            recv_window: ReplayWindow::default(),
            hb_window: ReplayWindow::default(),
            bound: false,
            stats: LinkStats::default(),
        }
    }

    /// Server 侧构造（accept 后）：发送方向为「响应方 → 发起方」（域分离）
    pub fn from_stream_as_responder(
        link: TcpStreamLink,
        key: [u8; NOISE_KEY_LEN],
        session_id: [u8; SESSION_ID_LEN],
    ) -> Self {
        let mut s = Self::from_stream(link, key, session_id);
        s.send_dir = DIR_RESPONDER_TO_INITIATOR;
        s
    }

    /// 本端接收方向标签（对端发送方向）
    fn recv_dir(&self) -> u8 {
        if self.send_dir == DIR_INITIATOR_TO_RESPONDER {
            DIR_RESPONDER_TO_INITIATOR
        } else {
            DIR_INITIATOR_TO_RESPONDER
        }
    }

    /// 绑定判定通过（由 `session::ChannelBinding` 状态机在 proof 校验成功后调用）
    pub fn mark_bound(&mut self) {
        self.bound = true;
        debuglog::log!(Level::Info, "lan", "stream.bound", &[]);
    }

    pub fn is_bound(&self) -> bool {
        self.bound
    }

    pub fn peer_addr(&self) -> Result<String, StreamError> {
        Ok(self.link.peer_addr()?)
    }

    pub fn stats(&self) -> LinkStats {
        self.stats
    }

    /// 取下一个业务发送序号；`u32::MAX` 时返回 `SeqExhausted`（回绕前终止，绝不复用 nonce）
    fn take_send_seq(&mut self) -> Result<u32, StreamError> {
        if self.send_seq == u32::MAX {
            return Err(StreamError::SeqExhausted);
        }
        let s = self.send_seq;
        self.send_seq = self.send_seq.wrapping_add(1);
        Ok(s)
    }

    /// 发送业务消息（自动加密）；返回本次 seq
    pub fn send(&mut self, msg_type: u8, plaintext: &[u8]) -> Result<u32, StreamError> {
        if msg_type == msg_type::HEARTBEAT {
            return self.send_heartbeat(plaintext);
        }
        if !self.bound {
            self.stats.binding_rejected += 1;
            debuglog::log!(
                Level::Warn,
                "lan",
                "stream.reject_send",
                &[("mt", &format!("{:#04X}", msg_type))]
            );
            return Err(StreamError::NotBound);
        }
        let seq = self.take_send_seq()?;
        // 帧头即 AAD；发送方向域标签
        let header = FrameHeader::new(msg_type, flags::ENCRYPTED, seq, plaintext.len() as u32);
        let aad = header.encode();
        let ciphertext = encrypt_payload(
            &self.key,
            self.send_dir,
            &self.session_id,
            seq,
            &aad,
            plaintext,
        )?;
        self.link.send_frame(header, &ciphertext)?;
        self.stats.sent += 1;
        Ok(seq)
    }

    /// 心跳直通（无 ENCRYPTED、无 tag、不受 binding 门禁约束）
    pub fn send_heartbeat(&mut self, plaintext: &[u8]) -> Result<u32, StreamError> {
        let seq = self.take_send_seq()?;
        let header = FrameHeader::new(msg_type::HEARTBEAT, 0, seq, plaintext.len() as u32);
        self.link.send_frame(header, plaintext)?;
        self.stats.sent += 1;
        Ok(seq)
    }

    pub fn send_ping(&mut self) -> Result<u32, StreamError> {
        self.send_heartbeat(&tlv_codec::simple(TAG_PING, 0u8.to_be_bytes()))
    }

    pub fn send_pong(&mut self) -> Result<u32, StreamError> {
        self.send_heartbeat(&tlv_codec::simple(TAG_PONG, 0u8.to_be_bytes()))
    }

    /// 收一帧（严格：重放 / 过旧帧直接报错）
    pub fn recv(&mut self) -> Result<RxFrame, StreamError> {
        self.recv_inner(None)
    }

    /// 收一帧（「超出窗口直接丢弃」语义：跳过重放帧继续读，最多 `max_skips` 帧）
    pub fn recv_discarding_replays(&mut self, max_skips: usize) -> Result<RxFrame, StreamError> {
        self.recv_inner(Some(max_skips))
    }

    fn recv_inner(&mut self, max_skips: Option<usize>) -> Result<RxFrame, StreamError> {
        let mut skipped = 0usize;
        loop {
            // 用「裸读」拿到帧，抗重放窗口在本层于**解密之后**提交：窗口一旦在认证前
            // 提交，伪造帧就能把它推顶。
            let (header, body) = self.link.read_frame()?;
            match self.handle_frame(header, &body) {
                Ok(rx) => return Ok(rx),
                Err(StreamError::Replayed(e)) => {
                    if max_skips.is_some() && skipped < max_skips.unwrap() {
                        skipped += 1;
                        continue;
                    }
                    return Err(StreamError::Replayed(e));
                }
                Err(e) => return Err(e),
            }
        }
    }

    /// 单帧处理：心跳直通；业务帧「只读校验 → 解密 → 提交窗口 → binding 门禁」
    fn handle_frame(&mut self, header: FrameHeader, body: &[u8]) -> Result<RxFrame, StreamError> {
        let encrypted = header.flags & flags::ENCRYPTED != 0;
        if header.msg_type == msg_type::HEARTBEAT {
            if encrypted {
                return Err(StreamError::Protocol("心跳帧不得置 ENCRYPTED 标志".into()));
            }
            // 心跳无 AEAD，无法"先认证再提交"，所以它只能用自己的窗口：
            // 混进业务窗口就等于把"打断整条文件通道"的开关交给任意局域网对端。
            self.hb_window
                .check_and_update(header.seq)
                .map_err(StreamError::Replayed)?;
            self.stats.received += 1;
            return Ok(RxFrame {
                msg_type: header.msg_type,
                flags: header.flags,
                seq: header.seq,
                payload: body.to_vec(),
                heartbeat: true,
            });
        }
        if !encrypted {
            return Err(StreamError::Protocol(format!(
                "业务帧 {:#04X} 缺少 ENCRYPTED 标志",
                header.msg_type
            )));
        }
        // 先只读校验（不提交），解密成功后再提交，防伪造帧推窗
        self.recv_window
            .check(header.seq)
            .map_err(StreamError::Replayed)?;
        let aad = header.encode();
        let dir = self.recv_dir();
        let plaintext = decrypt_payload(&self.key, dir, &self.session_id, header.seq, &aad, body)?;
        self.recv_window.commit(header.seq);
        if !self.bound {
            self.stats.binding_rejected += 1;
            debuglog::log!(
                Level::Warn,
                "lan",
                "stream.reject_recv",
                &[("mt", &format!("{:#04X}", header.msg_type))]
            );
            return Err(StreamError::NotBound);
        }
        self.stats.received += 1;
        Ok(RxFrame {
            msg_type: header.msg_type,
            flags: header.flags,
            seq: header.seq,
            payload: plaintext,
            heartbeat: false,
        })
    }
}

/// 手动 IP 兜底：接受 `192.168.1.5`（默认 TCP 端口）或 `192.168.1.5:55676`
pub fn parse_manual_addr(input: &str) -> Result<SocketAddr, StreamError> {
    let text = input.trim();
    if text.is_empty() {
        return Err(StreamError::BadManualAddr("地址为空".into()));
    }
    if let Ok(addr) = text.parse::<SocketAddr>() {
        return Ok(addr);
    }
    match text.parse::<std::net::IpAddr>() {
        Ok(ip) => Ok(SocketAddr::new(ip, crate::transport::TRANSPORT_TCP_PORT)),
        Err(_) => Err(StreamError::BadManualAddr(format!("无法解析 {text:?}"))),
    }
}

/// TCP 可达性探测（手动 IP 输入后的前置校验；不可达返回 `Ok(false)`）
pub fn probe_manual(addr: SocketAddr, timeout: Duration) -> Result<bool, StreamError> {
    match TcpStream::connect_timeout(&addr, timeout) {
        Ok(_) => Ok(true),
        Err(_) => Ok(false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::TRANSPORT_TCP_PORT;
    use std::io::{Read, Write};
    use std::net::{IpAddr, Ipv4Addr, TcpListener};

    fn key() -> [u8; NOISE_KEY_LEN] {
        [0x42; NOISE_KEY_LEN]
    }

    fn sid() -> [u8; SESSION_ID_LEN] {
        *b"linkx-session-01"
    }

    fn listener() -> (TcpListener, String) {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap().to_string();
        (l, addr)
    }

    /// 服务端构造（响应方角色，方向与客户端分离）
    fn server_link(sock: TcpStream) -> StreamLink {
        StreamLink::from_stream_as_responder(TcpStreamLink::from_stream(sock), key(), sid())
    }

    /// 以「发起方方向」向裸通道写一帧合法加密业务帧（模拟真实对端/攻击者）
    fn write_encrypted_business(raw: &mut TcpStream, seq: u32, pt: &[u8]) {
        let header = FrameHeader::new(
            msg_type::CLIPBOARD_PUSH,
            flags::ENCRYPTED,
            seq,
            pt.len() as u32,
        );
        let aad = header.encode();
        let ct =
            encrypt_payload(&key(), DIR_INITIATOR_TO_RESPONDER, &sid(), seq, &aad, pt).unwrap();
        raw.write_all(&header.encode()).unwrap();
        raw.write_all(&ct).unwrap();
    }

    #[test]
    fn encrypted_roundtrip_after_binding() {
        let (l, addr) = listener();
        let server = std::thread::spawn(move || {
            let (sock, _) = l.accept().unwrap();
            let mut link = server_link(sock);
            assert!(!link.is_bound());
            link.mark_bound();
            assert!(link.is_bound());
            let f = link.recv().unwrap();
            (f.msg_type, f.seq, f.payload, f.heartbeat)
        });
        let mut client = StreamLink::connect(&addr, key(), sid()).unwrap();
        client.mark_bound();
        let seq = client
            .send(msg_type::CLIPBOARD_PUSH, b"hello linkx")
            .unwrap();
        assert_eq!(seq, 0); // 首个业务帧 seq = 0
        let (mt, rseq, payload, hb) = server.join().unwrap();
        assert_eq!(mt, msg_type::CLIPBOARD_PUSH);
        assert_eq!(rseq, 0);
        assert_eq!(payload, b"hello linkx");
        assert!(!hb);
        assert_eq!(client.stats().sent, 1);
    }

    #[test]
    fn binding_gate_blocks_business_allows_heartbeat() {
        // 发送侧门禁：未 bound 时拒发业务、放行 PING
        let (l, addr) = listener();
        let server = std::thread::spawn(move || {
            let (sock, _) = l.accept().unwrap();
            let mut link = server_link(sock);
            link.recv().unwrap()
        });
        let mut client = StreamLink::connect(&addr, key(), sid()).unwrap();
        assert!(matches!(
            client.send(msg_type::CLIPBOARD_PUSH, b"x"),
            Err(StreamError::NotBound)
        ));
        assert_eq!(client.stats().binding_rejected, 1);
        client.send_ping().unwrap();
        assert!(server.join().unwrap().is_ping());
    }

    #[test]
    fn binding_gate_rejects_inbound_business_from_raw_peer() {
        // 接收侧门禁：对端绕过门禁直接写合法加密业务帧 → 本端拒收
        let (l, addr) = listener();
        let server = std::thread::spawn(move || {
            let (sock, _) = l.accept().unwrap();
            let mut link = server_link(sock);
            let ping = link.recv().unwrap();
            let denied = link.recv();
            assert_eq!(link.stats().binding_rejected, 1);
            (ping, denied)
        });
        let mut raw = TcpStream::connect(&addr).unwrap();
        let ping = tlv_codec::simple(TAG_PING, 0u8.to_be_bytes());
        let hb = FrameHeader::new(msg_type::HEARTBEAT, 0, 0, ping.len() as u32);
        raw.write_all(&hb.encode()).unwrap();
        raw.write_all(&ping).unwrap();
        write_encrypted_business(&mut raw, 1, b"x");
        let (ping_frame, denied) = server.join().unwrap();
        assert!(ping_frame.is_ping());
        assert!(matches!(denied, Err(StreamError::NotBound)));
    }

    #[test]
    fn forged_heartbeat_cannot_brick_the_business_window() {
        // 心跳是明文、没有 AEAD，无法"先认证再提交"。若它与业务帧共用一个抗重放窗口，
        // 局域网上任一主机发一帧 seq≈u32::MAX 的心跳就能把窗口推顶，此后所有合法业务帧
        // 都判 TooOld —— 整条文件通道被永久打断。分开窗口后这不再可能。
        let (l, addr) = listener();
        let server = std::thread::spawn(move || {
            let (sock, _) = l.accept().unwrap();
            let mut link = server_link(sock);
            link.mark_bound();
            let hb = link.recv().unwrap();
            let biz = link.recv().expect("心跳之后业务帧仍应可解");
            (hb.is_ping(), biz.msg_type, biz.payload)
        });
        let mut raw = TcpStream::connect(&addr).unwrap();
        let ping = tlv_codec::simple(TAG_PING, 0u8.to_be_bytes());
        let hb = FrameHeader::new(msg_type::HEARTBEAT, 0, u32::MAX, ping.len() as u32);
        raw.write_all(&hb.encode()).unwrap();
        raw.write_all(&ping).unwrap();
        write_encrypted_business(&mut raw, 0, b"still works");
        let (was_ping, mt, payload) = server.join().unwrap();
        assert!(was_ping);
        assert_eq!(mt, msg_type::CLIPBOARD_PUSH);
        assert_eq!(payload, b"still works");
    }

    #[test]
    fn heartbeat_on_wire_is_plaintext_without_tag() {
        let (l, addr) = listener();
        let server = std::thread::spawn(move || {
            let (mut sock, _) = l.accept().unwrap();
            let mut buf = [0u8; 13];
            sock.read_exact(&mut buf).unwrap();
            let header = FrameHeader::parse(&buf).unwrap();
            let mut body = vec![0u8; linkx_protocol::frame::body_len(&header)];
            sock.read_exact(&mut body).unwrap();
            (header, body)
        });
        let mut client = StreamLink::connect(&addr, key(), sid()).unwrap();
        client.send_ping().unwrap(); // 未 bound 也能发
        let (header, body) = server.join().unwrap();
        assert_eq!(header.msg_type, msg_type::HEARTBEAT);
        assert_eq!(header.flags & flags::ENCRYPTED, 0);
        assert_eq!(body.len(), header.payload_len as usize); // 无 16B tag
        assert!(tlv_codec::get(&body, TAG_PING).unwrap().is_some());
    }

    #[test]
    fn client_can_send_encrypted_frame_before_binding_rejected_by_peer() {
        // 服务端未 bound + 客户端已 bound（模拟绑定状态不同步）→ 服务端拒收
        let (l, addr) = listener();
        let server = std::thread::spawn(move || {
            let (sock, _) = l.accept().unwrap();
            let mut link = server_link(sock);
            link.recv()
        });
        let mut client = StreamLink::connect(&addr, key(), sid()).unwrap();
        client.mark_bound();
        client.send(msg_type::CLIPBOARD_PUSH, b"x").unwrap();
        assert!(matches!(server.join().unwrap(), Err(StreamError::NotBound)));
    }

    #[test]
    fn tampered_ciphertext_detected() {
        // 裸通道写入被篡改的密文 → 服务端完整性校验失败
        let (l, addr) = listener();
        let server = std::thread::spawn(move || {
            let (sock, _) = l.accept().unwrap();
            let mut link = server_link(sock);
            link.mark_bound();
            link.recv()
        });
        let mut raw = TcpStream::connect(&addr).unwrap();
        let header = FrameHeader::new(
            msg_type::CLIPBOARD_PUSH,
            flags::ENCRYPTED,
            5,
            b"payload".len() as u32,
        );
        let aad = header.encode();
        let mut ct = encrypt_payload(
            &key(),
            DIR_INITIATOR_TO_RESPONDER,
            &sid(),
            5,
            &aad,
            b"payload",
        )
        .unwrap();
        ct[0] ^= 0x01; // 篡改密文
        raw.write_all(&header.encode()).unwrap();
        raw.write_all(&ct).unwrap();
        assert!(matches!(
            server.join().unwrap(),
            Err(StreamError::Cipher(CipherError::Integrity))
        ));
    }

    #[test]
    fn direction_mismatch_breaks_decryption() {
        // 反向用例：服务端若以「发起方」方向构造（角色写反），
        // 就解不出客户端（发起方）发出的帧 → 必须有明确失败而非静默接受。
        let (l, addr) = listener();
        let server = std::thread::spawn(move || {
            let (sock, _) = l.accept().unwrap();
            // 故意用错误角色（与本应是响应方的服务端相反）
            let mut link = StreamLink::from_stream(TcpStreamLink::from_stream(sock), key(), sid());
            link.mark_bound();
            link.recv()
        });
        let mut client = StreamLink::connect(&addr, key(), sid()).unwrap();
        client.mark_bound();
        client.send(msg_type::CLIPBOARD_PUSH, b"x").unwrap();
        assert!(matches!(
            server.join().unwrap(),
            Err(StreamError::Cipher(CipherError::Integrity))
        ));
    }

    #[test]
    fn plaintext_business_frame_rejected() {
        // 业务帧必须置 ENCRYPTED；明文业务帧直接判协议违规
        let (l, addr) = listener();
        let server = std::thread::spawn(move || {
            let (sock, _) = l.accept().unwrap();
            let mut link = server_link(sock);
            link.mark_bound();
            link.recv()
        });
        let mut raw = TcpStream::connect(&addr).unwrap();
        let header = FrameHeader::new(msg_type::CLIPBOARD_PUSH, 0, 1, 3);
        raw.write_all(&header.encode()).unwrap();
        raw.write_all(b"abc").unwrap();
        assert!(matches!(
            server.join().unwrap(),
            Err(StreamError::Protocol(_))
        ));
    }

    #[test]
    fn replay_rejected_and_discarding_mode_recovers() {
        // 同一加密帧二次到达 → Replayed；discarding 模式跳过重放帧继续
        let (l, addr) = listener();
        let server = std::thread::spawn(move || {
            let (sock, _) = l.accept().unwrap();
            let mut link = server_link(sock);
            link.mark_bound();
            let first = link.recv().unwrap().seq;
            let second = link.recv();
            let third = link.recv_discarding_replays(4).unwrap().seq;
            (first, second, third)
        });
        let mut raw = TcpStream::connect(&addr).unwrap();
        for _ in 0..2 {
            write_encrypted_business(&mut raw, 1, b"a");
        }
        write_encrypted_business(&mut raw, 2, b"b");
        let (first, second, third) = server.join().unwrap();
        assert_eq!(first, 1);
        assert!(matches!(
            second,
            Err(StreamError::Replayed(ReplayError::Replay(1)))
        ));
        assert_eq!(third, 2);
    }

    /// 反向用例：发送序号接近 u32::MAX → 直接终止而非回绕复用 nonce
    #[test]
    fn seq_exhaustion_terminates_instead_of_wrapping() {
        let (l, addr) = listener();
        std::thread::spawn(move || {
            let _ = l.accept();
            // 保持连接存活，避免客户端写触发对端复位
            std::thread::sleep(Duration::from_millis(200));
        });
        let mut client = StreamLink::connect(&addr, key(), sid()).unwrap();
        client.mark_bound();
        // 逼近回绕
        client.send_seq = u32::MAX;
        assert!(matches!(
            client.send(msg_type::CLIPBOARD_PUSH, b"x"),
            Err(StreamError::SeqExhausted)
        ));
        // 心跳同样受约束（其 seq 也参与 nonce）
        assert!(matches!(
            client.send_heartbeat(&[0x20, 0x01, 0x00, 0x01]),
            Err(StreamError::SeqExhausted)
        ));
    }

    #[test]
    fn manual_addr_parsing() {
        let a = parse_manual_addr(" 192.168.1.5 ").unwrap();
        assert_eq!(
            a,
            SocketAddr::new(
                IpAddr::V4(Ipv4Addr::new(192, 168, 1, 5)),
                TRANSPORT_TCP_PORT
            )
        );
        let b = parse_manual_addr("192.168.1.5:60000").unwrap();
        assert_eq!(b.port(), 60000);
        let c = parse_manual_addr("::1").unwrap();
        assert!(c.is_ipv6());
        assert!(matches!(
            parse_manual_addr(""),
            Err(StreamError::BadManualAddr(_))
        ));
        assert!(matches!(
            parse_manual_addr("not-an-ip"),
            Err(StreamError::BadManualAddr(_))
        ));
    }

    #[test]
    fn probe_manual_reports_reachability() {
        let (l, addr) = listener();
        let sock_addr: SocketAddr = addr.parse().unwrap();
        assert!(probe_manual(sock_addr, Duration::from_millis(300)).unwrap());
        let free: SocketAddr = {
            let l2 = TcpListener::bind("127.0.0.1:0").unwrap();
            l2.local_addr().unwrap()
        }; // 释放后探测不可达
        assert!(!probe_manual(free, Duration::from_millis(300)).unwrap());
        drop(l);
    }

    /// 组合冒烟：UDP 发现 → 取对端地址 → 建 TCP 加密流 → 双向业务 + 心跳
    #[test]
    fn lan_channel_discovery_then_stream_loopback() {
        use crate::discovery::{DiscoveryBeacon, UdpDiscovery};

        let (l, addr) = listener();
        let server = std::thread::spawn(move || {
            let (sock, _) = l.accept().unwrap();
            let mut link = server_link(sock);
            link.mark_bound();
            let f = link.recv().unwrap();
            link.send_pong().unwrap();
            let g = link.recv().unwrap();
            (f.payload, g.is_ping())
        });

        // 发现侧：真实 UDP 往返拿到对端地址（127.0.0.1:<port>）
        let mut disc = UdpDiscovery::bind_addr("127.0.0.1:0".parse().unwrap()).unwrap();
        let mut probe = UdpDiscovery::bind_addr("127.0.0.1:0".parse().unwrap()).unwrap();
        let now = std::time::Instant::now();
        probe.add_target(disc.local_addr().unwrap());
        disc.add_target(probe.local_addr().unwrap());
        probe
            .broadcast(
                &DiscoveryBeacon {
                    advert_name: "phone".into(),
                    os: 0,
                    version: "0.1.0".into(),
                },
                now,
            )
            .unwrap();
        let tick = disc.pump(now);
        assert_eq!(tick.updated.len(), 1);

        // 通道侧：手动/直连地址 = TCP 监听地址（同一物理设备语义）
        let mut client = StreamLink::connect(&addr, key(), sid()).unwrap();
        client.mark_bound();
        client.send(msg_type::CLIPBOARD_PUSH, b"from-pc").unwrap();
        client.send_ping().unwrap();

        let (payload, peer_saw_ping) = server.join().unwrap();
        assert_eq!(payload, b"from-pc");
        assert!(peer_saw_ping);
    }
}
