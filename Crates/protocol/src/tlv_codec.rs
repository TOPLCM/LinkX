//! TLV 模式：`tag(1B) len(1B) value(lenB)` 连续排列，无嵌套。
//!
//! 适用：HELLO / CHALLENGE / REPLY / PAIR_CONFIRM / PAIR_DONE / PING / PONG 等 <=64B 控制消息。
//! 解析器对任何输入不得 panic（fuzz 目标）。

use thiserror::Error;

pub const TLV_MAX_MSG: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum TlvError {
    #[error("输入在 offset {offset} 处截断（声明 len={len}）")]
    Truncated { offset: usize, len: usize },
    #[error("单独 value 的 tag 未找到 {0}")]
    TagNotFound(u8),
    #[error("消息总长超限 {0}B（上限 {TLV_MAX_MSG}B）")]
    TooLarge(usize),
    #[error("声明长度与实际不符")]
    LengthMismatch,
}

/// 单条 TLV 项
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tlv {
    pub tag: u8,
    pub value: Vec<u8>,
}

impl Tlv {
    pub fn new(tag: u8, value: impl Into<Vec<u8>>) -> Self {
        Self {
            tag,
            value: value.into(),
        }
    }
    pub fn u8(tag: u8, v: u8) -> Self {
        Self::new(tag, [v])
    }
    pub fn u16(tag: u8, v: u16) -> Self {
        Self::new(tag, v.to_be_bytes().to_vec())
    }
    pub fn buf(tag: u8, v: &[u8]) -> Self {
        Self::new(tag, v.to_vec())
    }
}

/// 编码一组 TLV（不允许超过 64B，超出返回 Err）
pub fn encode(items: &[Tlv]) -> Result<Vec<u8>, TlvError> {
    let mut out = Vec::new();
    for it in items {
        out.push(it.tag);
        let len = it.value.len();
        if len > u8::MAX as usize {
            return Err(TlvError::TooLarge(len));
        }
        out.push(len as u8);
        out.extend_from_slice(&it.value);
        if out.len() > TLV_MAX_MSG {
            return Err(TlvError::TooLarge(out.len()));
        }
    }
    Ok(out)
}

/// 解析 TLV 序列（任意输入不 panic；截断/超长视为错误）
pub fn parse(buf: &[u8]) -> Result<Vec<Tlv>, TlvError> {
    let mut out = Vec::new();
    let mut off = 0usize;
    while off < buf.len() {
        match buf[off..].len() {
            0 => break,
            1 => {
                return Err(TlvError::Truncated {
                    offset: off,
                    len: 0,
                })
            }
            _ => {
                let tag = buf[off];
                let len = buf[off + 1] as usize;
                let value_start = off + 2;
                let value_end = value_start + len;
                if value_end > buf.len() {
                    return Err(TlvError::Truncated { offset: off, len });
                }
                if out.len() >= 32 {
                    return Err(TlvError::TooLarge(buf.len()));
                }
                out.push(Tlv {
                    tag,
                    value: buf[value_start..value_end].to_vec(),
                });
                off = value_end;
            }
        }
    }
    Ok(out)
}

/// 便捷：取指定 tag 的值
pub fn get(buf: &[u8], tag: u8) -> Result<Option<Vec<u8>>, TlvError> {
    let items = parse(buf)?;
    Ok(items.into_iter().find(|t| t.tag == tag).map(|t| t.value))
}

/// 便捷：构造“单 tag 单 value”控制消息
pub fn simple(tag: u8, value: impl Into<Vec<u8>>) -> Vec<u8> {
    encode(&[Tlv::new(tag, value)]).expect("单条 TLV 必在 64B 内")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto_tlv::tlv::*;

    #[test]
    fn encode_decode_roundtrip() {
        let items = [
            Tlv::buf(TAG_ADVERT_NAME, b"pc-home"),
            Tlv::u8(TAG_OS, OS_ANDROID),
            Tlv::buf(TAG_VERSION, b"0.1.0"),
        ];
        let buf = encode(&items).unwrap();
        assert!(buf.len() <= TLV_MAX_MSG);
        let back = parse(&buf).unwrap();
        assert_eq!(back, items);

        let n = get(&buf, TAG_ADVERT_NAME).unwrap().unwrap();
        assert_eq!(n, b"pc-home");
        assert!(get(&buf, TAG_SAS).unwrap().is_none());
    }

    #[test]
    fn ping_pong_payloads() {
        let ping = simple(TAG_PING, 0u8.to_be_bytes());
        let pong = simple(TAG_PONG, 0u8.to_be_bytes());
        assert!(get(&ping, TAG_PING).unwrap().is_some());
        assert!(get(&pong, TAG_PONG).unwrap().is_some());
    }

    #[test]
    fn truncated_rejected() {
        // 声明 len 超出输入 → Truncated，不 panic
        let bad = [0x01u8, 0x0A, 0xBB, 0xCC];
        assert!(parse(&bad).is_err());
        assert!(matches!(get(&bad, 0x01), Err(TlvError::Truncated { .. })));
        // 单字节尾
        assert!(parse(&[0x01u8]).is_err());
        // 完整但总长超限：62B value（2+62=64 合法）；63B value（65）→ 拒绝
        assert!(encode(&[Tlv::new(0x01, vec![0u8; 62])]).is_ok());
        assert!(encode(&[Tlv::new(0x01, vec![0u8; 63])]).is_err());
    }

    #[test]
    fn malformed_no_panic_smoke() {
        let mut seed = 0xDEADBEEFu32;
        for _ in 0..20_000 {
            seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
            let len = (seed % 32) as usize;
            let bytes: Vec<u8> = (0..len).map(|i| ((seed >> (i % 8)) & 0xFF) as u8).collect();
            let _ = parse(&bytes);
            let _ = get(&bytes, 0x42);
            let _ = encode(&[Tlv::new((len % 255) as u8, bytes)]);
        }
    }
}
