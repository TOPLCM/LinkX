//! 分块与校验：256KB/块 + 每块 CRC32 + 整文件 SHA-256
//!
//! 分层约定：**本模块不做文件 IO**——Android 侧是 SAF/ContentResolver、Windows 侧是普通文件，
//! IO 一律由平台层完成；引擎只负责「切块 / 算校验 / 累计哈希」的纯逻辑，便于单测与双端复用。
//!
//! CRC32 采用 IEEE 802.3（`crc32fast`，与 ZIP/gzip 一致）。

use debuglog::Level;
use sha2::{Digest, Sha256};
use thiserror::Error;

/// 分块大小：256KB/块
pub const CHUNK_SIZE: usize = 256 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ChunkError {
    #[error("IO 失败: {0}")]
    Io(String),
    #[error("块 CRC32 校验失败：index={index} 期望 0x{expected:08X} 实际 0x{actual:08X}")]
    CrcMismatch {
        index: u32,
        expected: u32,
        actual: u32,
    },
    #[error("块大小超上限：{0} > {CHUNK_SIZE}")]
    ChunkTooLarge(usize),
    #[error("整文件 SHA-256 校验失败")]
    Sha256Mismatch,
}

impl From<std::io::Error> for ChunkError {
    fn from(e: std::io::Error) -> Self {
        ChunkError::Io(e.to_string())
    }
}

/// 单块 CRC32（IEEE）
pub fn crc32(data: &[u8]) -> u32 {
    crc32fast::hash(data)
}

/// 单次分块结果：index + data + CRC32
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chunk {
    pub index: u32,
    pub data: Vec<u8>,
    pub crc32: u32,
}

impl Chunk {
    pub fn new(index: u32, data: Vec<u8>) -> Self {
        let crc = crc32(&data);
        Self {
            index,
            data,
            crc32: crc,
        }
    }

    pub fn len(&self) -> usize {
        self.data.len()
    }

    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    /// 自校验（收发两端均可用）
    pub fn verify(&self) -> Result<(), ChunkError> {
        let actual = crc32(&self.data);
        if actual != self.crc32 {
            // 埋点：块 CRC32 校验失败（成功逐块不记，避免刷屏）
            debuglog::log!(
                Level::Warn,
                "transfer",
                "crc.mismatch",
                &[
                    ("index", &self.index.to_string()),
                    ("expected", &format!("{:08X}", self.crc32)),
                    ("actual", &format!("{:08X}", actual)),
                ]
            );
            return Err(ChunkError::CrcMismatch {
                index: self.index,
                expected: self.crc32,
                actual,
            });
        }
        Ok(())
    }
}

/// 整文件哈希累计器（SHA-256 + CRC32，均增量，避免整文件驻留内存）
#[derive(Debug, Clone)]
pub struct FileHasher {
    sha: Sha256,
    crc: crc32fast::Hasher,
    bytes: u64,
}

impl Default for FileHasher {
    fn default() -> Self {
        Self::new()
    }
}

impl FileHasher {
    pub fn new() -> Self {
        Self {
            sha: Sha256::new(),
            crc: crc32fast::Hasher::new(),
            bytes: 0,
        }
    }

    pub fn update(&mut self, data: &[u8]) {
        self.sha.update(data);
        self.crc.update(data);
        self.bytes += data.len() as u64;
    }

    /// 已累计字节数
    pub fn bytes(&self) -> u64 {
        self.bytes
    }

    /// (SHA-256, CRC32)
    pub fn finish(self) -> ([u8; 32], u32) {
        let bytes = self.bytes;
        let mut digest = [0u8; 32];
        digest.copy_from_slice(&self.sha.finalize());
        let crc = self.crc.finalize();
        // 埋点：整文件 SHA-256/CRC32 校验结果（摘要只记前 8 hex，便于比对）
        debuglog::log!(
            Level::Info,
            "transfer",
            "hash.finish",
            &[
                ("bytes", &bytes.to_string()),
                ("crc32", &format!("{crc:08X}")),
                ("sha8", &hex_prefix(&digest)),
            ]
        );
        (digest, crc)
    }
}

/// 一次性整文件 SHA-256（小输入 / 测试用）
pub fn sha256(data: &[u8]) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(data);
    let mut out = [0u8; 32];
    out.copy_from_slice(&h.finalize());
    out
}

/// 摘要前 4 字节的 hex（8 字符；日志用，便于比对且不泄漏文件内容）
fn hex_prefix(digest: &[u8]) -> String {
    digest.iter().take(4).map(|b| format!("{b:02x}")).collect()
}

/// 整文件块数（空文件 = 0 块：FILE_META 后直接 FILE_DONE）
///
/// `chunk_size` 与 `size` 都是**对端在 FILE_META 里声明的**：0 会让 `div_ceil` 除零 panic
/// （全 profile `panic = "abort"` ⇒ 一条帧打死宿主进程），块数超出 u32 若直接 `as u32` 会
/// **回绕成 0**，于是"收满 total 块"被 0 块满足、空文件被判成完整原件。两者都不许静默发生：
/// 收侧在 `on_meta` 就把这两种声明拒掉（见 `RecvTask::on_meta` 的分块校验），这里只兜住
/// "回绕"这一半 —— 饱和成 `u32::MAX` 至多是传不完，回绕成 0 却是假成功。
pub fn chunks_total(size: u64, chunk_size: usize) -> u32 {
    if size == 0 || chunk_size == 0 {
        return 0;
    }
    u32::try_from(size.div_ceil(chunk_size as u64)).unwrap_or(u32::MAX)
}

/// 第 `index` 块在文件中的字节区间 `[start, end)`；越界返回 `None`
pub fn chunk_range(index: u32, chunk_size: usize, total_size: u64) -> Option<(u64, u64)> {
    let start = index as u64 * chunk_size as u64;
    if start >= total_size {
        return None;
    }
    let end = (start + chunk_size as u64).min(total_size);
    Some((start, end))
}

/// 流式分块读取器（平台层把文件流交给它；不一次性读入整文件）
pub struct Chunker<R: std::io::Read> {
    reader: R,
    chunk_size: usize,
    index: u32,
    finished: bool,
}

impl<R: std::io::Read> Chunker<R> {
    pub fn new(reader: R) -> Self {
        Self::with_chunk_size(reader, CHUNK_SIZE)
    }

    pub fn with_chunk_size(reader: R, chunk_size: usize) -> Self {
        assert!(chunk_size > 0, "分块大小必须为正");
        Self {
            reader,
            chunk_size,
            index: 0,
            finished: false,
        }
    }

    /// 读下一块；EOF 返回 `None`
    pub fn next_chunk(&mut self) -> Result<Option<Chunk>, ChunkError> {
        if self.finished {
            return Ok(None);
        }
        let mut buf = vec![0u8; self.chunk_size];
        let mut filled = 0usize;
        while filled < self.chunk_size {
            match self.reader.read(&mut buf[filled..]) {
                Ok(0) => break,
                Ok(n) => filled += n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(ChunkError::Io(e.to_string())),
            }
        }
        if filled == 0 {
            self.finished = true;
            return Ok(None);
        }
        buf.truncate(filled);
        let chunk = Chunk::new(self.index, buf);
        // 埋点：分块读取进度（节流：每 16 块一条，不记内容）
        if self.index.is_multiple_of(16) {
            debuglog::log!(
                Level::Info,
                "transfer",
                "chunk.read",
                &[
                    ("index", &chunk.index.to_string()),
                    ("len", &chunk.len().to_string()),
                ]
            );
        }
        self.index += 1;
        Ok(Some(chunk))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn crc32_known_answers() {
        // IEEE 802.3 KAT（与 zip/gzip/python zlib.crc32 一致）
        assert_eq!(crc32(b""), 0x0000_0000);
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(
            crc32(b"The quick brown fox jumps over the lazy dog"),
            0x414F_A339
        );
    }

    #[test]
    fn sha256_known_answers() {
        assert_eq!(
            hex(&sha256(b"")),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            hex(&sha256(b"abc")),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }

    #[test]
    fn chunker_splits_and_checks() {
        // 700_000B = 262144 + 262144 + 175712 → 3 块，末块不足
        let data: Vec<u8> = (0..700_000u32).map(|i| (i % 251) as u8).collect();
        let mut chunker = Chunker::new(Cursor::new(data.clone()));
        let mut chunks = Vec::new();
        while let Some(c) = chunker.next_chunk().unwrap() {
            chunks.push(c);
        }
        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[0].len(), CHUNK_SIZE);
        assert_eq!(chunks[1].len(), CHUNK_SIZE);
        assert_eq!(chunks[2].len(), 175_712);
        for (i, c) in chunks.iter().enumerate() {
            assert_eq!(c.index, i as u32);
            c.verify().unwrap();
            let (start, end) = chunk_range(c.index, CHUNK_SIZE, data.len() as u64).unwrap();
            assert_eq!(c.data, &data[start as usize..end as usize]);
        }
        assert_eq!(chunks_total(data.len() as u64, CHUNK_SIZE), 3);
    }

    #[test]
    fn chunker_exact_multiple_and_empty() {
        let data = vec![7u8; CHUNK_SIZE * 2];
        let mut chunker = Chunker::new(Cursor::new(data));
        let n = std::iter::from_fn(|| chunker.next_chunk().unwrap()).count();
        assert_eq!(n, 2);
        // 空文件 → 0 块
        let mut empty = Chunker::new(Cursor::new(Vec::<u8>::new()));
        assert_eq!(empty.next_chunk().unwrap(), None);
        assert_eq!(chunks_total(0, CHUNK_SIZE), 0);
        assert_eq!(chunk_range(0, CHUNK_SIZE, 0), None);
    }

    #[test]
    fn crc_mismatch_detected() {
        let mut c = Chunk::new(3, vec![1, 2, 3, 4]);
        c.verify().unwrap();
        c.data[0] ^= 0xFF; // 篡改
        assert!(matches!(
            c.verify(),
            Err(ChunkError::CrcMismatch { index: 3, .. })
        ));
    }

    #[test]
    fn file_hasher_matches_one_shot() {
        let data: Vec<u8> = (0..300_000u32).map(|i| (i % 97) as u8).collect();
        let mut h = FileHasher::new();
        for part in data.chunks(4096) {
            h.update(part);
        }
        assert_eq!(h.bytes(), data.len() as u64);
        let (sha, crc) = h.finish();
        assert_eq!(sha, sha256(&data));
        assert_eq!(crc, crc32(&data));
    }
}
