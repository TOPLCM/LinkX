//! 传输任务状态机 + 队列 / 历史（含断点续传）
//!
//! 状态机语义：Queued(空态等待) / Running(加载中) / Paused(可恢复错误，可续传)
//! / Verifying(收端整文件校验) / Done(成功) / Failed(不可恢复错误) / Cancelled(用户取消)。
//!
//! 分层：平台层负责所有文件 IO——发送侧按 `next_chunk_range()` 读文件，接收侧按
//! `RecvAction::Write{offset,data}` 落盘（SAF 随机写或临时分片）。

use debuglog::Level;
use linkx_protocol::linkx::FileMeta;

use crate::chunk::{chunk_range, chunks_total, crc32, FileHasher, CHUNK_SIZE};
use crate::protocol;
use crate::TransferError;

pub type FileId = u64;

/// 单块 CRC 校验失败的最大重传次数：超限 → `Failed`，
/// 防止恶意/损坏对端以「永远校验失败」触发无限重传（放大 / 死循环）。
pub const MAX_CRC_RETRIES: u32 = 5;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransferDirection {
    Send,
    Recv,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransferState {
    Queued,
    Running,
    /// 中断自动暂停 → 可续传（对应界面「网络中断，点此续传」）
    Paused,
    Verifying,
    Done,
    Failed,
    Cancelled,
}

impl TransferState {
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Done | Self::Failed | Self::Cancelled)
    }

    pub fn is_resumable(self) -> bool {
        self == Self::Paused
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TransferProgress {
    pub bytes_done: u64,
    pub total_bytes: u64,
    pub chunks_done: u32,
    pub chunks_total: u32,
}

impl TransferProgress {
    /// 0..=100（空文件直接 100）
    pub fn percent(&self) -> u8 {
        if self.total_bytes == 0 {
            return 100;
        }
        ((self.bytes_done.min(self.total_bytes) * 100) / self.total_bytes) as u8
    }
}

// ---------------------------------------------------------------- 发送任务

/// 发送任务：平台层供数据，引擎做校验与协议编码
#[derive(Debug, Clone)]
pub struct SendTask {
    pub file_id: FileId,
    pub name: String,
    pub size: u64,
    /// FILE_META 里声明的整文件 (SHA-256, CRC32)；`None` = 摘要延到 FILE_DONE
    pub declared: Option<([u8; 32], u32)>,
    pub chunk_size: usize,
    pub state: TransferState,
    next_index: u32,
    bytes_sent: u64,
    /// 出站增量摘要：分块发出即喂入，全程不把文件读第二遍、也不整文件驻留内存
    digest: FileHasher,
    /// 已定稿摘要（`finish` 可能被重复调用，故缓存一次 finalize）
    digest_final: Option<([u8; 32], u32)>,
    /// 续传时待补进摘要的前缀字节数（见 `resume_from`）
    prefix_pending: u64,
}

impl SendTask {
    /// 旧口径：整文件摘要已知（发端预先算好），FILE_META 直接带上。
    ///
    /// `chunk_size` 来自对端可控输入，这里不用 `assert!`：全 profile `panic=abort`
    /// 下它是进程级终止，改为返回 `Result` 由调用方决定。
    pub fn new(
        file_id: FileId,
        name: impl Into<String>,
        size: u64,
        sha256: [u8; 32],
        crc32: u32,
        chunk_size: usize,
    ) -> Result<Self, TransferError> {
        Ok(Self {
            declared: Some((sha256, crc32)),
            ..Self::base(file_id, name, size, chunk_size)?
        })
    }

    /// 流式口径（大文件首帧不等待）：发 FILE_META 前不读文件，摘要随分块增量算出，
    /// 由 `finish()` 放进 FILE_DONE。
    pub fn streaming(
        file_id: FileId,
        name: impl Into<String>,
        size: u64,
        chunk_size: usize,
    ) -> Result<Self, TransferError> {
        Self::base(file_id, name, size, chunk_size)
    }

    fn base(
        file_id: FileId,
        name: impl Into<String>,
        size: u64,
        chunk_size: usize,
    ) -> Result<Self, TransferError> {
        if chunk_size == 0 || chunk_size > CHUNK_SIZE {
            return Err(TransferError::Protocol(format!(
                "分块大小须在 1..={CHUNK_SIZE}（实际 {chunk_size}）"
            )));
        }
        Ok(Self {
            file_id,
            name: name.into(),
            size,
            declared: None,
            chunk_size,
            state: TransferState::Queued,
            next_index: 0,
            bytes_sent: 0,
            digest: FileHasher::new(),
            digest_final: None,
            prefix_pending: 0,
        })
    }

    pub fn chunks_total(&self) -> u32 {
        chunks_total(self.size, self.chunk_size)
    }

    /// FILE_META body（0x30）：流式口径下摘要留空（长度 0），收端改从 FILE_DONE 取
    pub fn meta_body(&self) -> Vec<u8> {
        let (sha, crc) = match self.declared {
            Some((s, c)) => (s.to_vec(), c),
            None => (Vec::new(), 0),
        };
        protocol::encode_meta(&FileMeta {
            name: self.name.clone(),
            size: self.size,
            file_id: self.file_id,
            chunk_size: self.chunk_size as u32,
            sha256: sha.into(),
            crc32: crc,
            album_id: 0,
        })
    }

    /// 已喂进摘要的字节数（= 覆盖进度，用于断言「首块发出前没读过整个文件」）
    pub fn digest_bytes(&self) -> u64 {
        self.digest.bytes()
    }

    /// 续传时尚未补进摘要的前缀字节数
    pub fn prefix_pending(&self) -> u64 {
        self.prefix_pending
    }

    pub fn start(&mut self) {
        self.state = TransferState::Running;
    }

    pub fn pause(&mut self) {
        if !self.state.is_terminal() {
            self.state = TransferState::Paused;
        }
    }

    pub fn next_index(&self) -> u32 {
        self.next_index
    }

    /// 下一块在文件中的字节区间（平台层据此读文件）；已发完 → None
    pub fn next_chunk_range(&self) -> Option<(u64, u64)> {
        chunk_range(self.next_index, self.chunk_size, self.size)
    }

    /// 编码下一块为 FILE_CHUNK body；`data` 长度须与 `next_chunk_range()` 一致。
    /// 已发完 → `Ok(None)`
    pub fn make_chunk_body(&mut self, data: &[u8]) -> Result<Option<Vec<u8>>, TransferError> {
        if self.state != TransferState::Running {
            return Err(TransferError::Protocol(
                "发送任务未处于 Running 状态".into(),
            ));
        }
        let Some((start, end)) = self.next_chunk_range() else {
            return Ok(None);
        };
        let expect = (end - start) as usize;
        if data.len() != expect {
            return Err(TransferError::Protocol(format!(
                "第 {} 块长度应为 {expect}B，实际 {}B",
                self.next_index,
                data.len()
            )));
        }
        let body = protocol::encode_chunk(self.file_id, self.next_index, crc32(data), data);
        // 埋点：发送分块进度（节流：每 16 块一条，只记 index/len）
        if self.next_index.is_multiple_of(16) {
            debuglog::log!(
                Level::Info,
                "transfer",
                "send.chunk",
                &[
                    ("file_id", &format!("{:#x}", self.file_id)),
                    ("index", &self.next_index.to_string()),
                    ("len", &data.len().to_string()),
                ]
            );
        }
        self.note_sent(data)?;
        self.next_index += 1;
        self.bytes_sent += data.len() as u64;
        Ok(Some(body))
    }

    /// 记下一段「已经发出」的字节：增量喂摘要。
    ///
    /// 平台层不经本函数编码发帧（Windows 直接把分块交给 `send_file_chunk`）时**必须**调用它，
    /// 否则 FILE_DONE 的摘要会缺一段——那是「一侧以为发了、另一侧根本没解出来」的又一种形态。
    pub fn note_sent(&mut self, data: &[u8]) -> Result<(), TransferError> {
        if self.prefix_pending != 0 {
            return Err(TransferError::Protocol(format!(
                "续传前缀仍有 {}B 未补入摘要",
                self.prefix_pending
            )));
        }
        self.digest.update(data);
        self.digest_final = None;
        Ok(())
    }

    /// 续传补算：把跳过未重发的字节喂进摘要（不发帧）。
    ///
    /// 口径：续传**不保留**中断前的累计器，而是清空后由平台层重开文件、顺序把
    /// `[0, 起点)` 前缀喂进来。这样最终摘要恰好覆盖每个字节恰好一次，与从未中断的路径逐字节等价。
    pub fn fold_prefix(&mut self, data: &[u8]) -> Result<(), TransferError> {
        let n = data.len() as u64;
        if n > self.prefix_pending {
            return Err(TransferError::Protocol(format!(
                "补入前缀超出待补字节（{n} > {}）",
                self.prefix_pending
            )));
        }
        self.digest.update(data);
        self.prefix_pending -= n;
        self.digest_final = None;
        Ok(())
    }

    /// 断点续传：由收端 `MSG_RESUME` 指引起点；index 须落在 [0, chunks_total]
    pub fn resume_from(&mut self, index: u32) -> Result<(), TransferError> {
        if index > self.chunks_total() {
            return Err(TransferError::Protocol(format!(
                "续传起点 {index} 超出块数 {}",
                self.chunks_total()
            )));
        }
        self.next_index = index;
        self.bytes_sent = (index as u64 * self.chunk_size as u64).min(self.size);
        self.digest = FileHasher::new();
        self.digest_final = None;
        self.prefix_pending = self.bytes_sent;
        self.state = TransferState::Running;
        // 埋点：RESUME 采纳（发端按收端指示的起点续传）
        debuglog::log!(
            Level::Info,
            "transfer",
            "resume.adopt",
            &[
                ("file_id", &format!("{:#x}", self.file_id)),
                ("index", &index.to_string()),
            ]
        );
        Ok(())
    }

    /// 整文件摘要：`None` = 尚未覆盖全文件（分块没发完 / 续传前缀没补全）。
    /// 拿不到摘要时不许发 `ok=true` 的 FILE_DONE——宁可让对方大声失败。
    pub fn whole_file_digest(&mut self) -> Option<([u8; 32], u32)> {
        if let Some(d) = self.declared {
            return Some(d);
        }
        if self.prefix_pending != 0 || self.digest.bytes() != self.size {
            return None;
        }
        if self.digest_final.is_none() {
            self.digest_final = Some(self.digest.clone().finish());
        }
        self.digest_final
    }

    /// FILE_DONE body（0x32）；同时落终态。成功时带上增量算出的整文件摘要。
    ///
    /// "拿不到摘要就不许报成功"这条写在 [`Self::whole_file_digest`] 的文档里，这里把它做进
    /// 函数而不是留给调用方自觉：调用纪律会被下一次重构忘掉，而一条 `ok=true` 的空摘要回执
    /// 在对端看来就是"传完了"。
    pub fn finish(&mut self, ok: bool, error: Option<&str>) -> Vec<u8> {
        let sha = if ok {
            self.whole_file_digest().map(|d| d.0)
        } else {
            None
        };
        let ok = ok && sha.is_some();
        self.state = if ok {
            TransferState::Done
        } else {
            TransferState::Failed
        };
        let error = match error {
            Some(e) => Some(e.to_string()),
            None if !ok => Some("整文件摘要没算全，未报告成功".to_string()),
            None => None,
        };
        protocol::encode_done_sha(self.file_id, ok, sha.as_ref(), error.as_deref())
    }

    /// 用户取消收尾：落 `Cancelled`（不是 `Failed`），FILE_DONE 带 `cancelled = true`。
    ///
    /// 摘要一律不交付：只发了一半的文件算不出整文件摘要，交付一个错摘要比不交付更坏。
    pub fn cancel(&mut self, reason: &str) -> Vec<u8> {
        self.state = TransferState::Cancelled;
        protocol::encode_done_cancelled(self.file_id, Some(reason))
    }

    pub fn progress(&self) -> TransferProgress {
        TransferProgress {
            bytes_done: self.bytes_sent,
            total_bytes: self.size,
            chunks_done: self.next_index,
            chunks_total: self.chunks_total(),
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::chunk::{chunks_total, Chunker, FileHasher};
    use std::io::Cursor;

    fn payload(len: usize) -> Vec<u8> {
        (0..len as u32).map(|i| (i % 251) as u8).collect()
    }

    fn make_sender(data: &[u8], chunk_size: usize) -> SendTask {
        let mut h = FileHasher::new();
        h.update(data);
        let (sha, crc) = h.finish();
        SendTask::new(7, "file.bin", data.len() as u64, sha, crc, chunk_size).unwrap()
    }

    #[test]
    fn send_cancel_is_not_a_failure_and_delivers_no_digest() {
        let data = payload(CHUNK_SIZE + 100);
        let mut sender = make_sender(&data, CHUNK_SIZE);
        sender.start();
        let body = sender.cancel("用户在电脑上取消了");
        assert_eq!(sender.state, TransferState::Cancelled);
        assert!(sender.state.is_terminal());
        let d = protocol::decode_done(&body).unwrap();
        assert!(!d.ok, "取消不是成功");
        assert!(
            d.cancelled,
            "取消必须在 FILE_DONE 里说清楚，否则收端只能报失败"
        );
        assert_eq!(d.error.as_deref(), Some("用户在电脑上取消了"));
        assert!(d.sha256.is_empty(), "半截文件的摘要不得当成整文件摘要交付");
    }

    #[test]
    fn send_task_rejects_wrong_length_and_guards_state() {
        let data = payload(1000);
        let mut sender = make_sender(&data, CHUNK_SIZE);
        // 未 start → 拒发
        assert!(matches!(
            sender.make_chunk_body(&data),
            Err(TransferError::Protocol(_))
        ));
        sender.start();
        assert!(matches!(
            sender.make_chunk_body(&data[..999]),
            Err(TransferError::Protocol(_))
        ));
        // 续传起点越界
        assert!(matches!(
            sender.resume_from(5),
            Err(TransferError::Protocol(_))
        ));
        sender.pause();
        assert!(sender.state.is_resumable());
    }

    #[test]
    fn progress_percent_edges() {
        assert_eq!(TransferProgress::default().percent(), 100); // 空文件
        let p = TransferProgress {
            bytes_done: 1,
            total_bytes: 3,
            chunks_done: 0,
            chunks_total: 1,
        };
        assert_eq!(p.percent(), 33);
        let over = TransferProgress {
            bytes_done: 999,
            total_bytes: 100,
            chunks_done: 0,
            chunks_total: 1,
        };
        assert_eq!(over.percent(), 100); // 钳制
    }

    #[test]
    fn chunks_total_is_consistent_with_chunker() {
        for size in [0usize, 1, CHUNK_SIZE - 1, CHUNK_SIZE, CHUNK_SIZE + 1] {
            let mut chunker = Chunker::new(Cursor::new(vec![0u8; size]));
            let n = std::iter::from_fn(|| chunker.next_chunk().unwrap()).count();
            assert_eq!(
                n as u32,
                chunks_total(size as u64, CHUNK_SIZE),
                "size={size}"
            );
        }
    }

    #[test]
    fn send_task_new_rejects_bad_chunk_size_without_panic() {
        assert!(SendTask::new(1, "a", 0, [0u8; 32], 0, 0).is_err());
        assert!(SendTask::new(1, "a", 0, [0u8; 32], 0, CHUNK_SIZE + 1).is_err());
        assert!(SendTask::new(1, "a", 0, [0u8; 32], 0, CHUNK_SIZE).is_ok());
        assert!(SendTask::new(1, "a", 0, [0u8; 32], 0, 1).is_ok());
    }
}
