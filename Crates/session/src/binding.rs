//! Channel Binding：证明「TCP 连接的对端」与「BLE 已配对设备」是同一台物理设备，
//! 防止同局域网第三方冒充（哪怕 TCP 端口明文监听）。
//! 对称双向，消息 = CHANNEL_BIND 帧：nonce 经 TCP（不可信）交换，proof（HMAC）只能经
//! 已认证的 BLE 通道送达——第三方没有 `channel_bind_key`，伪造不了也截获不了证明。
//! - 每台设备**独立判定**：验证过对方 proof ⇔ 对方持共享密钥 ⇔ 是配对设备。
//! - 未 bound 前 TCP 只允许 `PING` / `CHANNEL_BIND`，业务消息一律拒绝（见 `allow_application`）。
//! - nonce 一次一用：bound 后再收到任何 proof 一律拒绝（防重放）。

use linkx_crypto::binding::{
    channel_bind_tag, derive_channel_bind_key, generate_bind_nonce, CHANNEL_BIND_NONCE_LEN,
};
use linkx_protocol::proto_tlv::tlv::{TAG_BIND_TAG, TAG_NONCE_TCP};
use linkx_protocol::tlv_codec::{encode, parse, Tlv, TlvError};
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BindRole {
    TcpClient,
    TcpServer,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BindStep {
    AwaitPeerNonce,
    AwaitPeerProof,
    Bound,
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum BindError {
    #[error("channel binding 未就绪：{0}")]
    NotReady(&'static str),
    #[error("TLV 编解码失败: {0}")]
    Tlv(#[from] TlvError),
    #[error("nonce 长度非法（期望 {CHANNEL_BIND_NONCE_LEN}B）")]
    BadNonceLen,
    #[error("对端 nonce 与本地不匹配（第三方冒充 TCP 对端）")]
    NonceMismatch,
    #[error("绑定签名校验失败（channel_bind_key 不一致或签名被篡改）")]
    TagMismatch,
    #[error("绑定已完成后再次收到证明（重放）")]
    AlreadyBound,
}

/// 绑定状态机：纯逻辑、无 IO，消息收发由调用方对接 TCP / BLE 通道
#[derive(Debug, Clone)]
pub struct ChannelBinding {
    role: BindRole,
    key: [u8; 32],
    my_nonce: Option<[u8; CHANNEL_BIND_NONCE_LEN]>,
    peer_nonce: Option<[u8; CHANNEL_BIND_NONCE_LEN]>,
    step: BindStep,
}

impl ChannelBinding {
    /// `session_key` = 配对会话密钥（Noise XX 输出派生，双端共享）
    pub fn new(session_key: &[u8; 32], role: BindRole) -> Self {
        Self {
            role,
            key: derive_channel_bind_key(session_key),
            my_nonce: None,
            peer_nonce: None,
            step: BindStep::AwaitPeerNonce,
        }
    }

    pub fn role(&self) -> BindRole {
        self.role
    }
    pub fn step(&self) -> BindStep {
        self.step
    }
    pub fn is_bound(&self) -> bool {
        self.step == BindStep::Bound
    }
    /// 业务消息门禁：未绑定一律拒绝（心跳与 CHANNEL_BIND 由调用方放行）
    pub fn allow_application(&self) -> bool {
        self.is_bound()
    }

    /// 生成我方 nonce 并构造 TCP 载荷 `TLV[nonce_tcp]`（首次调用生成随机 nonce，幂等）
    pub fn make_tcp_payload(&mut self) -> Result<Vec<u8>, BindError> {
        let nonce = self.my_nonce()?;
        Ok(encode(&[Tlv::buf(TAG_NONCE_TCP, &nonce[..])])?)
    }

    /// 处理从 TCP 收到的对端 nonce。**已绑定 (Bound) 时收到 nonce 不是错误**：BLE 快于
    /// TCP 时本端会先 bound、之后才收到对方 nonce，此处若拒绝就再也发不出自己的 proof，
    /// 对端永远绑不上（表现为"一端已绑定、另一端绑不上"）。安全性没有放松：防重放的真正
    /// 对象是 proof（拒绝在 `verify_ble_proof`）；已绑定后又收到**不同**的 nonce 仍拒绝，
    /// 否则这里成了 HMAC 签名 oracle；proof 每周期最多发一次由调用方保证。
    pub fn on_tcp_payload(&mut self, payload: &[u8]) -> Result<(), BindError> {
        let nonce = extract_nonce(payload)?;
        if self.step == BindStep::Bound && self.peer_nonce.is_some_and(|p| p != nonce) {
            return Err(BindError::AlreadyBound);
        }
        if self.peer_nonce == Some(nonce) {
            return Ok(()); // 对端重复应答（幂等，不视为错误）
        }
        self.peer_nonce = Some(nonce);
        if self.my_nonce.is_none() {
            self.my_nonce = Some(generate_bind_nonce());
        }
        // 已绑定不得退回 AwaitPeerProof：那会让 `allow_application()` 反悔，把正在跑业务
        // 的 TCP 通道重新关上门。
        if self.step != BindStep::Bound {
            self.step = BindStep::AwaitPeerProof;
        }
        Ok(())
    }

    /// 构造经 BLE 已认证通道发送的 proof（需已收到对端 nonce，即 TCP 载荷已交换）。
    pub fn make_proof_ble_payload(&mut self) -> Result<Vec<u8>, BindError> {
        let peer = self
            .peer_nonce
            .ok_or(BindError::NotReady("尚未收到对端 nonce"))?;
        if self.my_nonce.is_none() {
            self.my_nonce = Some(generate_bind_nonce());
        }
        let tag = channel_bind_tag(&self.key, &peer[..]);
        Ok(encode(&[
            Tlv::buf(TAG_NONCE_TCP, &peer[..]),
            Tlv::buf(TAG_BIND_TAG, &tag[..]),
        ])?)
    }

    /// 验证经 BLE 收到的对端 proof：nonce 必须等于**我方**发出的 nonce，验证通过 → bound。
    pub fn verify_ble_proof(&mut self, payload: &[u8]) -> Result<(), BindError> {
        if self.step == BindStep::Bound {
            return Err(BindError::AlreadyBound); // nonce 一次一用，重放拒绝
        }
        let my = self
            .my_nonce
            .ok_or(BindError::NotReady("尚未发出我方 nonce"))?;
        let (nonce, tag) = extract_proof(payload)?;
        if nonce != my {
            return Err(BindError::NonceMismatch);
        }
        let expect = channel_bind_tag(&self.key, &my[..]);
        // 折叠异或而不是 `!=`：切片比较在第一个不同字节就返回，MAC 的比对照例上不该带时间差
        let diff = tag
            .iter()
            .zip(expect.iter())
            .fold(0u8, |acc, (a, b)| acc | (a ^ b));
        if diff != 0 {
            return Err(BindError::TagMismatch);
        }
        self.step = BindStep::Bound;
        Ok(())
    }

    fn my_nonce(&mut self) -> Result<[u8; CHANNEL_BIND_NONCE_LEN], BindError> {
        match self.my_nonce {
            Some(n) => Ok(n),
            None => {
                let n = generate_bind_nonce();
                self.my_nonce = Some(n);
                Ok(n)
            }
        }
    }
}

fn extract_nonce(payload: &[u8]) -> Result<[u8; CHANNEL_BIND_NONCE_LEN], BindError> {
    let items = parse(payload)?;
    match items.into_iter().find(|t| t.tag == TAG_NONCE_TCP) {
        Some(t) => t.value.try_into().map_err(|_| BindError::BadNonceLen),
        None => Err(BindError::NotReady("载荷缺少 TAG_NONCE_TCP")),
    }
}

fn extract_proof(payload: &[u8]) -> Result<([u8; CHANNEL_BIND_NONCE_LEN], [u8; 16]), BindError> {
    let items = parse(payload)?;
    let nonce = items
        .iter()
        .find(|t| t.tag == TAG_NONCE_TCP)
        .map(|t| t.value.as_slice())
        .ok_or(BindError::NotReady("proof 缺少 TAG_NONCE_TCP"))?;
    let tag = items
        .iter()
        .find(|t| t.tag == TAG_BIND_TAG)
        .map(|t| t.value.as_slice())
        .ok_or(BindError::NotReady("proof 缺少 TAG_BIND_TAG"))?;
    let nonce: [u8; CHANNEL_BIND_NONCE_LEN] =
        nonce.try_into().map_err(|_| BindError::BadNonceLen)?;
    let tag: [u8; 16] = tag.try_into().map_err(|_| BindError::BadNonceLen)?;
    Ok((nonce, tag))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn k() -> [u8; 32] {
        [0x42u8; 32]
    }

    fn full_bind(client: &mut ChannelBinding, server: &mut ChannelBinding) {
        let c_challenge = client.make_tcp_payload().unwrap();
        server.on_tcp_payload(&c_challenge).unwrap();
        let s_reply = server.make_tcp_payload().unwrap();
        client.on_tcp_payload(&s_reply).unwrap();
        let s_proof = server.make_proof_ble_payload().unwrap();
        let c_proof = client.make_proof_ble_payload().unwrap();
        client.verify_ble_proof(&s_proof).unwrap();
        server.verify_ble_proof(&c_proof).unwrap();
    }

    #[test]
    fn full_bind_roundtrip() {
        let mut c = ChannelBinding::new(&k(), BindRole::TcpClient);
        let mut s = ChannelBinding::new(&k(), BindRole::TcpServer);
        assert!(!c.is_bound() && !s.is_bound());
        full_bind(&mut c, &mut s);
        assert!(c.is_bound());
        assert!(s.is_bound());
        assert!(c.allow_application() && s.allow_application());
    }

    #[test]
    fn replay_after_bound_rejected() {
        let mut c = ChannelBinding::new(&k(), BindRole::TcpClient);
        let mut s = ChannelBinding::new(&k(), BindRole::TcpServer);
        full_bind(&mut c, &mut s);
        let s_proof = s.make_proof_ble_payload().unwrap();
        assert_eq!(s_proof.len(), 2 + CHANNEL_BIND_NONCE_LEN + 2 + 16);
        let mut c2 = ChannelBinding::new(&k(), BindRole::TcpClient);
        let c_challenge = c2.make_tcp_payload().unwrap();
        let mut s2 = ChannelBinding::new(&k(), BindRole::TcpServer);
        s2.on_tcp_payload(&c_challenge).unwrap();
        c2.on_tcp_payload(&s2.make_tcp_payload().unwrap()).unwrap();
        let s2_proof = s2.make_proof_ble_payload().unwrap();
        c2.verify_ble_proof(&s2_proof).unwrap();
        assert!(matches!(
            c2.verify_ble_proof(&s2_proof),
            Err(BindError::AlreadyBound)
        ));
    }

    #[test]
    fn tampered_tag_rejected() {
        let mut c = ChannelBinding::new(&k(), BindRole::TcpClient);
        let mut s = ChannelBinding::new(&k(), BindRole::TcpServer);
        let c_challenge = c.make_tcp_payload().unwrap();
        s.on_tcp_payload(&c_challenge).unwrap();
        c.on_tcp_payload(&s.make_tcp_payload().unwrap()).unwrap();
        let mut s_proof = s.make_proof_ble_payload().unwrap();
        let last = s_proof.len() - 1;
        s_proof[last] ^= 0x01;
        assert!(matches!(
            c.verify_ble_proof(&s_proof),
            Err(BindError::TagMismatch)
        ));
    }

    #[test]
    fn third_party_spoof_fails() {
        // 威胁场景：第三方 C' 冒充 TCP 客户端连上服务器 S。C' 没有 channel_bind_key，
        // 伪造的 proof 签名必然不符 → 校验失败 → 不放开业务消息。
        let mut server = ChannelBinding::new(&k(), BindRole::TcpServer);
        let mut attacker = ChannelBinding::new(&[0x99u8; 32], BindRole::TcpClient);
        let c_fake = attacker.make_tcp_payload().unwrap();
        server.on_tcp_payload(&c_fake).unwrap();
        let s_reply = server.make_tcp_payload().unwrap();
        attacker.on_tcp_payload(&s_reply).unwrap();
        // C' 拿到服务器 nonce 后经 BLE 伪造 proof（错 key → tag 错）
        let fake = attacker.make_proof_ble_payload().unwrap();
        assert!(matches!(
            server.verify_ble_proof(&fake),
            Err(BindError::TagMismatch)
        ));
        assert!(!server.is_bound());
        assert!(!server.allow_application());
    }

    #[test]
    fn wrong_nonce_rejected() {
        let mut c = ChannelBinding::new(&k(), BindRole::TcpClient);
        let mut s = ChannelBinding::new(&k(), BindRole::TcpServer);
        let c_challenge = c.make_tcp_payload().unwrap();
        s.on_tcp_payload(&c_challenge).unwrap();
        c.on_tcp_payload(&s.make_tcp_payload().unwrap()).unwrap();
        let mut s_proof = s.make_proof_ble_payload().unwrap();
        // 只改 nonce 首字节而不改 tag → 对端判 NonceMismatch（而非 TagMismatch）
        s_proof[2] ^= 0x01;
        assert!(matches!(
            c.verify_ble_proof(&s_proof),
            Err(BindError::NonceMismatch)
        ));
    }

    #[test]
    fn bound_gate_blocks_before_verify() {
        let mut c = ChannelBinding::new(&k(), BindRole::TcpClient);
        let mut s = ChannelBinding::new(&k(), BindRole::TcpServer);
        let c_challenge = c.make_tcp_payload().unwrap();
        s.on_tcp_payload(&c_challenge).unwrap();
        c.on_tcp_payload(&s.make_tcp_payload().unwrap()).unwrap();
        assert!(!c.allow_application());
        assert!(!s.allow_application());
    }

    #[test]
    fn malformed_payloads_never_panic() {
        let mut c = ChannelBinding::new(&k(), BindRole::TcpClient);
        let mut s = ChannelBinding::new(&k(), BindRole::TcpServer);
        let c_challenge = c.make_tcp_payload().unwrap();
        s.on_tcp_payload(&c_challenge).unwrap();
        c.on_tcp_payload(&s.make_tcp_payload().unwrap()).unwrap();

        for bad in [
            &[][..],
            &[0x01u8][..],
            &[TAG_NONCE_TCP][..],
            &[TAG_NONCE_TCP, 0x10, 0x11][..],
            &[0xEE, 0x10, 0x11, 0x12][..],
        ] {
            assert!(s.on_tcp_payload(bad).is_err());
            assert!(c.verify_ble_proof(bad).is_err());
        }
        let big = [0x30u8; 70];
        assert!(s.on_tcp_payload(&big).is_err());
        assert!(c.verify_ble_proof(&big).is_err());
    }

    #[test]
    fn tcp_payload_is_single_tag_tlv() {
        let mut c = ChannelBinding::new(&k(), BindRole::TcpClient);
        let pl = c.make_tcp_payload().unwrap();
        let items = parse(&pl).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].tag, TAG_NONCE_TCP);
        assert_eq!(items[0].value.len(), CHANNEL_BIND_NONCE_LEN);
    }

    #[test]
    fn idempotent_tcp_payload_keeps_nonce() {
        let mut c = ChannelBinding::new(&k(), BindRole::TcpClient);
        let p1 = c.make_tcp_payload().unwrap();
        let p2 = c.make_tcp_payload().unwrap();
        assert_eq!(p1, p2); // 重申挑战幂等（同一 nonce）
    }
}
