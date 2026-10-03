//! 通用信封（外层包明文结构，加密前 payload）
//!
//! `msg_id(16B, uint128) | ts_ms(int64, UTC 毫秒) | src(8B device_id) | body(消息体)`
//!
//! 编码路径：业务层构造 typed message → `linkx_core::encode_body` → 本模块包信封
//! → ChaCha20-Poly1305 加密 → 组帧。信封长度域由 protobuf 自身承载（无需手写长度）。

use prost::Message;
use thiserror::Error;

use crate::linkx::Envelope;

/// uint128 消息 id 字节数
pub const MSG_ID_LEN: usize = 16;
/// 源设备 id 字节数
pub const SRC_LEN: usize = 8;

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum EnvelopeError {
    #[error("protobuf 解码失败: {0}")]
    Decode(String),
    #[error("msg_id 长度须为 16B（实际 {0}）")]
    BadMsgId(usize),
    #[error("src 长度须为 8B（实际 {0}）")]
    BadSrc(usize),
}

/// 组装业务消息 payload（加密前明文）
pub fn encode(msg_id: &[u8; MSG_ID_LEN], ts_ms: i64, src: &[u8; SRC_LEN], body: &[u8]) -> Vec<u8> {
    Envelope {
        msg_id: msg_id.to_vec().into(),
        ts_ms,
        src: src.to_vec().into(),
        body: body.to_vec().into(),
    }
    .encode_to_vec()
}

/// 解析信封（fuzz 目标：任意输入不 panic；长度域严格校验）
pub fn decode(buf: &[u8]) -> Result<Envelope, EnvelopeError> {
    let envelope = Envelope::decode(buf).map_err(|e| EnvelopeError::Decode(e.to_string()))?;
    if envelope.msg_id.len() != MSG_ID_LEN {
        return Err(EnvelopeError::BadMsgId(envelope.msg_id.len()));
    }
    if envelope.src.len() != SRC_LEN {
        return Err(EnvelopeError::BadSrc(envelope.src.len()));
    }
    Ok(envelope)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg_id() -> [u8; MSG_ID_LEN] {
        [0x11; MSG_ID_LEN]
    }

    fn src() -> [u8; SRC_LEN] {
        [0x22; SRC_LEN]
    }

    #[test]
    fn envelope_roundtrip() {
        let raw = encode(&msg_id(), 1_700_000_000_000, &src(), b"body-bytes");
        let e = decode(&raw).unwrap();
        assert_eq!(e.msg_id.as_ref(), &msg_id()[..]);
        assert_eq!(e.ts_ms, 1_700_000_000_000);
        assert_eq!(e.src.as_ref(), &src()[..]);
        assert_eq!(e.body.as_ref(), b"body-bytes");
    }

    #[test]
    fn empty_body_allowed() {
        let raw = encode(&msg_id(), 0, &src(), &[]);
        assert!(decode(&raw).unwrap().body.is_empty());
    }

    #[test]
    fn bad_lengths_rejected() {
        let e = Envelope {
            msg_id: vec![0u8; 8].into(), // 应为 16B
            ts_ms: 0,
            src: src().to_vec().into(),
            body: Vec::new().into(),
        };
        assert_eq!(decode(&e.encode_to_vec()), Err(EnvelopeError::BadMsgId(8)));
        let e2 = Envelope {
            msg_id: msg_id().to_vec().into(),
            ts_ms: 0,
            src: vec![0u8; 4].into(), // 应为 8B
            body: Vec::new().into(),
        };
        assert_eq!(decode(&e2.encode_to_vec()), Err(EnvelopeError::BadSrc(4)));
    }

    #[test]
    fn junk_never_panics() {
        assert!(decode(&[]).is_err());
        assert!(decode(&[0xFF; 64]).is_err());
        let mut seed = 0xC0FFEEu32;
        for _ in 0..5_000 {
            seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
            let len = (seed % 80) as usize;
            let bytes: Vec<u8> = (0..len).map(|i| ((seed >> (i % 8)) & 0xFF) as u8).collect();
            let _ = decode(&bytes);
        }
    }
}
