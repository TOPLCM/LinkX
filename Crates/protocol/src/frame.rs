//! 统一帧头（13 字节，大端序，仅元数据）：[frame_header(13B)] [ciphertext(nB)] [aead_tag(16B)]

use crate::msg_type;
use thiserror::Error;

pub const FRAME_HEADER_LEN: usize = 13;
pub const AEAD_TAG_LEN: usize = 16;
pub const MAGIC: u16 = 0x4C58; // "LX"
pub const PROTOCOL_VERSION: u8 = 0x01;

/// 单帧 payload 上限 = **未认证输入直接决定的分配大小**（`payload_len` 由对端声明，读帧照着 `vec![0u8; len]`）。
/// 最大合法帧是一条 `FILE_CHUNK`（256 KiB + 序号/CRC/信封 + tag），1 MiB 已四倍余量，再放只是放大单次分配额。（BLE 重组层另有上限）
pub const MAX_FRAME_PAYLOAD: u32 = 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum FrameError {
    #[error("输入不足 {0} 字节")]
    TooShort(usize),
    #[error("magic 不匹配（0x{magic:04X}）")]
    BadMagic { magic: u16 },
    #[error("协议版本不匹配（0x{version:02X}）")]
    BadVersion { version: u8 },
    #[error("payload_len 超出上限 {0}")]
    PayloadTooLarge(u32),
    #[error("帧长校验失败：需要 {need} 但只有 {have}")]
    Truncated { need: usize, have: usize },
}

/// flags 位定义
pub mod flags {
    pub const KEYFRAME: u8 = 1 << 0; // 镜像 IDR 帧
    pub const ACK: u8 = 1 << 1; // 应用层 ACK
    pub const FIN: u8 = 1 << 2; // 分块末尾
    pub const RETRY: u8 = 1 << 3; // 重传标记
    pub const ENCRYPTED: u8 = 1 << 4; // 除心跳外全部置 1
    pub const COMPRESSED: u8 = 1 << 5; // zstd（暂未启用）
    pub const CHANNEL_BIND: u8 = 1 << 6; // channel binding 上下文
    pub const DEBUG_JSON: u8 = 1 << 7; // JSON debug 模式
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameHeader {
    pub version: u8,
    pub msg_type: u8,
    pub flags: u8,
    pub seq: u32,
    /// ciphertext 字节数（不含尾部 16B AEAD tag）
    pub payload_len: u32,
}

impl FrameHeader {
    pub const MAGIC: u16 = MAGIC;

    pub fn new(msg_type: u8, flags: u8, seq: u32, payload_len: u32) -> Self {
        Self {
            version: PROTOCOL_VERSION,
            msg_type,
            flags,
            seq,
            payload_len,
        }
    }

    /// 生成"正确默认值"帧头：需加密的消息类型自动置位 ENCRYPTED
    pub fn default_encrypted(msg_type: u8, seq: u32, payload_len: u32) -> Self {
        let f = if msg_type::requires_encryption(msg_type) {
            flags::ENCRYPTED
        } else {
            0
        };
        Self::new(msg_type, f, seq, payload_len)
    }

    /// 编码为 13B 大端
    pub fn encode(&self) -> [u8; FRAME_HEADER_LEN] {
        let mut b = [0u8; FRAME_HEADER_LEN];
        b[0..2].copy_from_slice(&MAGIC.to_be_bytes());
        b[2] = self.version;
        b[3] = self.msg_type;
        b[4] = self.flags;
        b[5..9].copy_from_slice(&self.seq.to_be_bytes());
        b[9..13].copy_from_slice(&self.payload_len.to_be_bytes());
        b
    }

    /// 解析 13B 帧头（任何输入不得 panic，fuzz 目标）
    pub fn parse(bytes: &[u8]) -> Result<Self, FrameError> {
        if bytes.len() < FRAME_HEADER_LEN {
            return Err(FrameError::TooShort(bytes.len()));
        }
        let magic = u16::from_be_bytes([bytes[0], bytes[1]]);
        if magic != MAGIC {
            return Err(FrameError::BadMagic { magic });
        }
        let version = bytes[2];
        if version != PROTOCOL_VERSION {
            return Err(FrameError::BadVersion { version });
        }
        let payload_len = u32::from_be_bytes([bytes[9], bytes[10], bytes[11], bytes[12]]);
        if payload_len > MAX_FRAME_PAYLOAD {
            return Err(FrameError::PayloadTooLarge(payload_len));
        }
        Ok(Self {
            version,
            msg_type: bytes[3],
            flags: bytes[4],
            seq: u32::from_be_bytes([bytes[5], bytes[6], bytes[7], bytes[8]]),
            payload_len,
        })
    }

    /// 完整帧长度 = 13 + payload_len + (ENCRYPTED ? 16 : 0)
    pub fn full_frame_len(&self) -> usize {
        FRAME_HEADER_LEN + body_len(self)
    }
}

/// 线上 body = payload + (ENCRYPTED ? 16B tag : 0)；心跳（0x70）无 tag
pub fn body_len(header: &FrameHeader) -> usize {
    let tag = if header.flags & flags::ENCRYPTED != 0 {
        AEAD_TAG_LEN
    } else {
        0
    };
    header.payload_len as usize + tag
}

/// 组装完整帧（body = 密文+16B tag 或心跳明文 payload，长度须与帧头一致）
pub fn assemble_frame(header: &FrameHeader, body: &[u8]) -> Result<Vec<u8>, FrameError> {
    if body.len() != body_len(header) {
        return Err(FrameError::Truncated {
            need: body_len(header),
            have: body.len(),
        });
    }
    let mut out = Vec::with_capacity(header.full_frame_len());
    out.extend_from_slice(&header.encode());
    out.extend_from_slice(body);
    Ok(out)
}

/// 从流中读出一整帧（TCP/接口层）：先 13B 头，再 payload+16B tag（fuzz 目标）
pub fn parse_full_frame(buf: &[u8]) -> Result<(FrameHeader, Vec<u8>), FrameError> {
    let header = FrameHeader::parse(buf)?;
    let need = header.full_frame_len();
    if buf.len() < need {
        return Err(FrameError::Truncated {
            need,
            have: buf.len(),
        });
    }
    let body = buf[FRAME_HEADER_LEN..need].to_vec(); // ciphertext + 16B tag
    Ok((header, body))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_roundtrip() {
        let h = FrameHeader::default_encrypted(msg_type::CLIPBOARD_PUSH, 7, 1234);
        let b = h.encode();
        let h2 = FrameHeader::parse(&b).unwrap();
        assert_eq!(h, h2);
        assert_eq!(h.flags & flags::ENCRYPTED, flags::ENCRYPTED);
    }

    #[test]
    fn header_layout_matches_spec() {
        let h = FrameHeader::new(0x10, 0x10, 0x01020304, 0x05060708);
        let b = h.encode();
        assert_eq!(b[0..2], [0x4C, 0x58]);
        assert_eq!(b[2], 0x01); // version
        assert_eq!(b[3], 0x10); // type
        assert_eq!(b[4], 0x10); // flags
        assert_eq!(b[5..9], [0x01, 0x02, 0x03, 0x04]); // seq BE
        assert_eq!(b[9..13], [0x05, 0x06, 0x07, 0x08]); // payload_len BE
    }

    #[test]
    fn parse_rejects_junk() {
        assert!(FrameHeader::parse(&[0x00, 0x00, 0x01]).is_err()); // 太短
        assert!(FrameHeader::parse(&[
            0x00, 0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b
        ])
        .is_err()); // magic 错
        let mut h = FrameHeader::default_encrypted(0x10, 1, 0).encode();
        h[2] = 0x02; // 版本错
        assert!(FrameHeader::parse(&h).is_err());
        let mut h2 = FrameHeader::default_encrypted(0x10, 1, 0).encode();
        h2[9] = 0xFF; // 超上限
        assert!(FrameHeader::parse(&h2).is_err());
    }

    #[test]
    fn full_frame_and_budget() {
        let h = FrameHeader::default_encrypted(msg_type::CLIPBOARD_PUSH, 0, 1024);
        let cipher = vec![0xAB; 1024];
        let mut body = cipher.clone();
        body.extend_from_slice(&[0u8; AEAD_TAG_LEN]);
        assert_eq!(h.full_frame_len(), 13 + 1024 + 16);
        let frame = assemble_frame(&h, &body).unwrap();
        assert_eq!(frame.len(), h.full_frame_len());
        let (h2, rbody) = parse_full_frame(&frame).unwrap();
        assert_eq!(h2, h);
        assert_eq!(rbody, body);
        assert_eq!(&rbody[..1024], cipher.as_slice());
        // body 缺 tag → 拒绝
        assert!(assemble_frame(&h, &cipher).is_err());
        // 少一个字节 → Truncated
        assert!(parse_full_frame(&frame[..frame.len() - 1]).is_err());
    }

    #[test]
    fn fuzz_smoke_never_panics() {
        // 随机输入解析不崩溃（正式 fuzz 见 Tests/fuzz）
        let mut seed = 0x9E3779B9u32;
        for _ in 0..50_000u32 {
            seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
            let len = (seed % 64) as usize;
            let bytes: Vec<u8> = (0..len).map(|i| ((seed >> (i % 8)) & 0xFF) as u8).collect();
            let _ = FrameHeader::parse(&bytes);
            let _ = parse_full_frame(&bytes);
        }
        // 长度上限边界（1MB 级别的帧不 panic）
        let h = FrameHeader::default_encrypted(0x10, 0, MAX_FRAME_PAYLOAD);
        let _ = h.encode();
    }
}
