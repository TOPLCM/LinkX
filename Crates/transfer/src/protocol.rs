//! 文件传输消息编解码
//!
//! - `FILE_META` / `FILE_CHUNK` / `FILE_DONE` / `FILE_CANCEL`：protobuf body（`Proto/linkx/v1/file.proto`）
//! - `MSG_RESUME`：TLV 控制消息（≤64B；对端 FILE_CHUNK 序号不连续 → 触发 RESUME 请求）
//!
//! 本模块只产出/解析 **body**（信封与加密由 `linkx_protocol::envelope` / `StreamLink` 负责）。

use linkx_protocol::linkx::{FileCancel, FileChunk, FileDone, FileMeta};
use linkx_protocol::tlv_codec::{self, Tlv};
use linkx_protocol::{TAG_FILE_ID, TAG_RESUME_FROM};
use prost::Message;

use crate::TransferError;

// ---- FILE_META (0x30) ----

pub fn encode_meta(meta: &FileMeta) -> Vec<u8> {
    meta.encode_to_vec()
}

pub fn decode_meta(buf: &[u8]) -> Result<FileMeta, TransferError> {
    FileMeta::decode(buf).map_err(|e| TransferError::Decode(e.to_string()))
}

// ---- FILE_CHUNK (0x31) ----

pub fn encode_chunk(file_id: u64, index: u32, crc32: u32, data: &[u8]) -> Vec<u8> {
    FileChunk {
        file_id,
        index,
        data: data.to_vec().into(),
        crc32,
    }
    .encode_to_vec()
}

pub fn decode_chunk(buf: &[u8]) -> Result<FileChunk, TransferError> {
    FileChunk::decode(buf).map_err(|e| TransferError::Decode(e.to_string()))
}

// ---- FILE_DONE (0x32) ----

pub fn encode_done(file_id: u64, ok: bool, error: Option<&str>) -> Vec<u8> {
    encode_done_sha(file_id, ok, None, error)
}

/// 带整文件摘要的 FILE_DONE（发端边发分块边算，摘要在此交付）
///
/// `sha256 = None` → 该字段留空，收端回落到 FILE_META 的声明值（旧端口径）。
pub fn encode_done_sha(
    file_id: u64,
    ok: bool,
    sha256: Option<&[u8; 32]>,
    error: Option<&str>,
) -> Vec<u8> {
    encode_done_full(file_id, ok, sha256, error, false)
}

/// 取消收尾的 FILE_DONE：`ok = false` + `cancelled = true`。
///
/// 少了 cancelled 这一位，收端只能把用户的取消显示成失败。
pub fn encode_done_cancelled(file_id: u64, error: Option<&str>) -> Vec<u8> {
    encode_done_full(file_id, false, None, error, true)
}

fn encode_done_full(
    file_id: u64,
    ok: bool,
    sha256: Option<&[u8; 32]>,
    error: Option<&str>,
    cancelled: bool,
) -> Vec<u8> {
    FileDone {
        file_id,
        ok,
        error: error.map(|s| s.to_string()),
        sha256: sha256.map(|b| b.to_vec().into()).unwrap_or_default(),
        cancelled,
    }
    .encode_to_vec()
}

pub fn decode_done(buf: &[u8]) -> Result<FileDone, TransferError> {
    FileDone::decode(buf).map_err(|e| TransferError::Decode(e.to_string()))
}

// ---- FILE_CANCEL (0x33)：收端 → 发端「停止发送」 ----

pub fn encode_cancel(file_id: u64, reason: &str) -> Vec<u8> {
    FileCancel {
        file_id,
        reason: reason.to_string(),
    }
    .encode_to_vec()
}

pub fn decode_cancel(buf: &[u8]) -> Result<FileCancel, TransferError> {
    FileCancel::decode(buf).map_err(|e| TransferError::Decode(e.to_string()))
}

// ---- MSG_RESUME（TLV 控制消息） ----

/// 组装修传请求：`TAG_FILE_ID(8B BE) + TAG_RESUME_FROM(4B BE)`
pub fn encode_resume(file_id: u64, from_index: u32) -> Vec<u8> {
    tlv_codec::encode(&[
        Tlv::buf(TAG_FILE_ID, &file_id.to_be_bytes()),
        Tlv::buf(TAG_RESUME_FROM, &from_index.to_be_bytes()),
    ])
    .expect("固定两项 TLV 必然可编码")
}

/// 解析续传请求 → (file_id, from_index)
pub fn decode_resume(buf: &[u8]) -> Result<(u64, u32), TransferError> {
    let file_id = tlv_codec::get(buf, TAG_FILE_ID)
        .map_err(|e| TransferError::Tlv(e.to_string()))?
        .ok_or(TransferError::MissingField("TAG_FILE_ID"))?;
    let from = tlv_codec::get(buf, TAG_RESUME_FROM)
        .map_err(|e| TransferError::Tlv(e.to_string()))?
        .ok_or(TransferError::MissingField("TAG_RESUME_FROM"))?;
    if file_id.len() != 8 {
        return Err(TransferError::Protocol(format!(
            "TAG_FILE_ID 须为 8B（实际 {}）",
            file_id.len()
        )));
    }
    if from.len() != 4 {
        return Err(TransferError::Protocol(format!(
            "TAG_RESUME_FROM 须为 4B（实际 {}）",
            from.len()
        )));
    }
    let mut id = [0u8; 8];
    id.copy_from_slice(&file_id);
    let mut idx = [0u8; 4];
    idx.copy_from_slice(&from);
    Ok((u64::from_be_bytes(id), u32::from_be_bytes(idx)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn meta_roundtrip() {
        let m = FileMeta {
            name: "报告.pdf".into(),
            size: 1_234_567,
            file_id: 0x0102030405060708,
            chunk_size: 262_144,
            sha256: vec![0xAB; 32].into(),
            crc32: 0xDEAD_BEEF,
            album_id: 0,
        };
        let raw = encode_meta(&m);
        assert_eq!(decode_meta(&raw).unwrap(), m);
        assert!(decode_meta(&[0xFF; 8]).is_err());
    }

    /// 摘要延后：FILE_META 的 sha256 允许为空（长度 0），字段号不变
    #[test]
    fn meta_without_digest_roundtrip() {
        let m = FileMeta {
            name: "big.iso".into(),
            size: 8_600_000_000,
            file_id: 1,
            chunk_size: 262_144,
            sha256: Default::default(),
            crc32: 0,
            album_id: 0,
        };
        let d = decode_meta(&encode_meta(&m)).unwrap();
        assert!(d.sha256.is_empty());
        assert_eq!(d.size, m.size);
    }

    #[test]
    fn chunk_roundtrip_with_crc() {
        let data = vec![9u8; 1024];
        let crc = crate::chunk::crc32(&data);
        let raw = encode_chunk(7, 3, crc, &data);
        let c = decode_chunk(&raw).unwrap();
        assert_eq!(c.file_id, 7);
        assert_eq!(c.index, 3);
        assert_eq!(c.data.as_ref(), data.as_slice());
        assert_eq!(c.crc32, crc);
    }

    #[test]
    fn done_roundtrip() {
        let raw = encode_done(42, false, Some("disk full"));
        let d = decode_done(&raw).unwrap();
        assert_eq!(d.file_id, 42);
        assert!(!d.ok);
        assert_eq!(d.error.as_deref(), Some("disk full"));
        assert!(d.sha256.is_empty(), "不带摘要的 FILE_DONE 摘要字段须为空");
        let ok = decode_done(&encode_done(42, true, None)).unwrap();
        assert!(ok.ok);
        assert!(ok.error.is_none());
    }

    /// FILE_DONE 摘要：新端带、旧端不带，两侧都能解（前向/后向兼容）
    #[test]
    fn done_digest_roundtrip_and_legacy_compat() {
        let sha = crate::chunk::sha256(b"payload");
        let d = decode_done(&encode_done_sha(7, true, Some(&sha), None)).unwrap();
        assert_eq!(d.sha256.as_ref(), sha.as_slice());
        assert_eq!(d.error, None);

        // 旧端编码的字节（没有 tag 4）在新端解出来 = 空摘要 → 收端回落 FILE_META
        let legacy = decode_done(&encode_done(7, true, None)).unwrap();
        assert!(legacy.sha256.is_empty());
        assert_eq!(legacy.file_id, 7);
    }

    /// FILE_DONE 的 cancelled 位：取消收尾要能与失败区分；旧端不带该字段 → 按未取消处理
    #[test]
    fn done_cancelled_flag_roundtrip_and_legacy_compat() {
        let d = decode_done(&encode_done_cancelled(9, Some("用户在接收端取消了"))).unwrap();
        assert_eq!(d.file_id, 9);
        assert!(!d.ok, "取消不是成功");
        assert!(d.cancelled, "取消收尾必须带上 cancelled");
        assert_eq!(d.error.as_deref(), Some("用户在接收端取消了"));
        assert!(d.sha256.is_empty(), "取消收尾没有完整文件，不许交付摘要");

        // 旧端字节（没有 tag 5）在新端解出来 = 未取消 → 语义完全不变
        let legacy = decode_done(&encode_done(9, false, Some("disk full"))).unwrap();
        assert!(!legacy.cancelled);
        assert_eq!(legacy.error.as_deref(), Some("disk full"));
    }

    #[test]
    fn cancel_roundtrip() {
        let raw = encode_cancel(0xDEAD_BEEF, "手机侧用户取消");
        let c = decode_cancel(&raw).unwrap();
        assert_eq!(c.file_id, 0xDEAD_BEEF);
        assert_eq!(c.reason, "手机侧用户取消");
        assert!(matches!(
            decode_cancel(&[0xFF; 8]),
            Err(TransferError::Decode(_))
        ));
    }

    #[test]
    fn resume_roundtrip_and_validation() {
        let raw = encode_resume(0x1122_3344_5566_7788, 1234);
        assert!(raw.len() <= 64, "控制消息须 <=64B（4.5.1）");
        assert_eq!(decode_resume(&raw).unwrap(), (0x1122_3344_5566_7788, 1234));
        // 缺字段 / 长度非法
        let missing = tlv_codec::encode(&[Tlv::buf(TAG_FILE_ID, &1u64.to_be_bytes())]).unwrap();
        assert!(matches!(
            decode_resume(&missing),
            Err(TransferError::MissingField(_))
        ));
        let short_id = tlv_codec::encode(&[
            Tlv::buf(TAG_FILE_ID, &[0u8; 4]),
            Tlv::buf(TAG_RESUME_FROM, &0u32.to_be_bytes()),
        ])
        .unwrap();
        assert!(matches!(
            decode_resume(&short_id),
            Err(TransferError::Protocol(_))
        ));
        assert!(decode_resume(&[]).is_err());
    }
}
