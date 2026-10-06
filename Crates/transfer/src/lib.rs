//! LinkX transfer crate —— 文件传输核心
//!
//! - `chunk`：256KB 分块 + 每块 CRC32 + 整文件 SHA-256（纯逻辑，不做文件 IO）
//! - `protocol`：FILE_META / FILE_CHUNK / FILE_DONE（protobuf body） + MSG_RESUME（TLV）
//! - `task`：发送侧状态机（含断点续传与流式摘要）。接收侧在平台层：Windows `RecvSession`、
//!   安卓 Kotlin 各一份 —— 落盘方式（SAF / 随机写 / 临时分片）是平台能力，共享核心不重复实现一份没人调用的
//! - `filename`：接收侧文件名净化（改名式、永不拒绝）
//!
//! 分层：文件 IO（Android SAF / Windows 文件）由平台层完成，本 crate 只处理协议与校验逻辑，
//! 因此双端可通过 FFI 复用同一套语义。

pub mod chunk;
pub mod filename;
pub mod protocol;
pub mod task;

use thiserror::Error;

pub use chunk::{
    chunk_range, chunks_total, crc32, sha256, Chunk, ChunkError, Chunker, FileHasher, CHUNK_SIZE,
};
pub use task::{FileId, SendTask, TransferDirection, TransferProgress, TransferState};

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum TransferError {
    #[error("分块/校验失败: {0}")]
    Chunk(#[from] ChunkError),
    #[error("protobuf 解码失败: {0}")]
    Decode(String),
    #[error("TLV 解析失败: {0}")]
    Tlv(String),
    #[error("缺少字段: {0}")]
    MissingField(&'static str),
    #[error("协议违规: {0}")]
    Protocol(String),
}

/// 本 crate 版本（FFI 上行用）
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_is_exposed() {
        assert_eq!(version(), env!("CARGO_PKG_VERSION"));
    }
}
