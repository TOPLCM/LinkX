//! 传输任务状态机 + 队列 / 历史（含断点续传）
//!
//! 状态机语义：Queued(空态等待) / Running(加载中) / Paused(可恢复错误，可续传)
//! / Verifying(收端整文件校验) / Done(成功) / Failed(不可恢复错误) / Cancelled(用户取消)。
//!
//! 分层：平台层负责所有文件 IO——发送侧按 `next_chunk_range()` 读文件，接收侧按
//! `RecvAction::Write{offset,data}` 落盘（SAF 随机写或临时分片）。

use std::collections::VecDeque;

use debuglog::Level;
use linkx_protocol::linkx::{FileChunk, FileDone, FileMeta};

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

// ---------------------------------------------------------------- 接收任务

/// 接收动作：平台层按此落盘 / 应答
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecvAction {
    /// 落盘到 `offset`（平台层决定 SAF 随机写 or 临时分片）
    Write {
        index: u32,
        offset: u64,
        data: Vec<u8>,
    },
    /// 已接收过（≤ 续传点）→ 幂等跳过
    Skip { index: u32 },
    /// 序号不连续 / 块校验失败 → 请求从 `from_index` 续传（应答 body 见 `resume_body`）
    RequestResume { from_index: u32 },
    /// 块 CRC 反复失败、超过重试上限 → 终止任务（状态置 `Failed`）
    CrcRetriesExhausted { index: u32, retries: u32 },
    /// 全部块收齐且整文件校验通过
    Completed,
    /// 整文件校验失败（可整体重传：状态置 Paused）
    VerifyFailed {
        expected_sha256: [u8; 32],
        actual_sha256: [u8; 32],
    },
    /// FILE_DONE 与 FILE_META 都没给出摘要：无从确认完整性，不许判成功（状态置 Failed）
    NoDigest { actual_sha256: [u8; 32] },
    /// 对端报告失败（FILE_DONE ok=false）
    PeerFailed { error: Option<String> },
    /// 对端取消收尾（FILE_DONE ok=false + cancelled=true）：落 `Cancelled`，
    /// 平台层删残留文件并把原因显示成「已取消」——用户的主动动作不是系统故障。
    PeerCancelled { reason: String },
}

/// 摘要取用：恰好 32B 才算给了；空（缺位）或长度非法一律 `None`
fn fixed_digest(b: &[u8]) -> Option<[u8; 32]> {
    if b.len() != 32 {
        return None;
    }
    let mut out = [0u8; 32];
    out.copy_from_slice(b);
    Some(out)
}

/// 接收任务：平台层喂入已解码的 body，引擎做校验与进度推进
#[derive(Debug)]
pub struct RecvTask {
    meta: Option<FileMeta>,
    /// 经 `sanitize_file_name` 改写后的安全文件名（平台层据此 join 目标目录）
    safe_name: Option<String>,
    state: TransferState,
    next_index: u32,
    bytes_done: u64,
    hasher: FileHasher,
    /// 因块校验失败触发的重传次数（上限见 `MAX_CRC_RETRIES`）
    crc_retries: u32,
}

impl Default for RecvTask {
    fn default() -> Self {
        Self::new()
    }
}

impl RecvTask {
    pub fn new() -> Self {
        Self {
            meta: None,
            safe_name: None,
            state: TransferState::Queued,
            next_index: 0,
            bytes_done: 0,
            hasher: FileHasher::new(),
            crc_retries: 0,
        }
    }

    /// 平台层落盘时应使用的安全文件名（未收到 META 时为 None）
    pub fn file_name(&self) -> Option<&str> {
        self.safe_name.as_deref()
    }

    /// 收 FILE_META（0x30）后进入接收态。
    ///
    /// `resume_from` / `prefix`：断点续传时由平台层提供——已写入的块数，以及对已写入
    /// 前缀重新流式哈希得到的累计器（整文件 SHA-256 必须覆盖全文件，不能只哈希剩余部分）。
    pub fn on_meta(
        &mut self,
        meta: FileMeta,
        resume_from: u32,
        prefix: Option<FileHasher>,
    ) -> Result<(), TransferError> {
        // 摘要允许缺位：流式发送把整文件摘要放到 FILE_DONE，此处只认 0B（缺位）或 32B（旧端声明）
        if !matches!(meta.sha256.len(), 0 | 32) {
            return Err(TransferError::Protocol(format!(
                "FILE_META.sha256 须为 0B（摘要延到 FILE_DONE）或 32B（实际 {}）",
                meta.sha256.len()
            )));
        }
        // 进入接收态前把文件名一律改写成安全的单段名，杜绝路径穿越（平台层只用
        // safe_name 落盘），且**永不拒绝**：拒绝式写法在调用点走 `return` 且不回
        // FileDone，发送侧就此停在"传输中"，成为静默丢弃路径。
        let safe_name = crate::filename::sanitize_file_name(&meta.name);
        // 分块声明的三条判据都在这里，一条都不许漏到算术里去：
        //   0        → `div_ceil` 除零 panic（`panic = "abort"` = 一条帧打死宿主进程）
        //   > 上限   → 单块比本机能攒的缓冲还大
        //   块数超 u32 → 4 GiB / 1 B 恰好 2^32 块，窄化成 0 之后"收满 total 块"被 0 块满足，
        //                空文件判成完整原件（假成功比失败难查得多）
        if meta.chunk_size == 0
            || meta.chunk_size as usize > CHUNK_SIZE
            || meta.size.div_ceil(meta.chunk_size as u64) > u64::from(u32::MAX)
        {
            return Err(TransferError::Protocol(format!(
                "FILE_META.chunk_size 非法：{}",
                meta.chunk_size
            )));
        }
        let total = chunks_total(meta.size, meta.chunk_size as usize);
        if resume_from > total {
            return Err(TransferError::Protocol(format!(
                "续传起点 {resume_from} 超出块数 {total}"
            )));
        }
        if resume_from > 0 && prefix.is_none() {
            return Err(TransferError::Protocol(
                "续传须提供已写入前缀的哈希累计器（整文件校验覆盖全文件）".into(),
            ));
        }
        let hasher = prefix.unwrap_or_default();
        if hasher.bytes() != resume_from as u64 * meta.chunk_size as u64 {
            // 末块可能不足 chunk_size，此处仅对非末块严格校验
            let expected = (resume_from as u64 * meta.chunk_size as u64).min(meta.size);
            if hasher.bytes() != expected {
                return Err(TransferError::Protocol(format!(
                    "前缀哈希字节数 {} 与续传起点不符（期望 {expected}）",
                    hasher.bytes()
                )));
            }
        }
        self.bytes_done = hasher.bytes();
        self.hasher = hasher;
        self.next_index = resume_from;
        self.meta = Some(meta);
        self.safe_name = Some(safe_name);
        self.state = TransferState::Running;
        // 埋点：接收元信息就绪（含续传起点）
        if let Some(m) = self.meta.as_ref() {
            debuglog::log!(
                Level::Info,
                "transfer",
                "recv.meta",
                &[
                    ("file_id", &format!("{:#x}", m.file_id)),
                    ("size", &m.size.to_string()),
                    ("resume_from", &resume_from.to_string()),
                ]
            );
        }
        Ok(())
    }

    pub fn file_id(&self) -> Option<FileId> {
        self.meta.as_ref().map(|m| m.file_id)
    }

    pub fn state(&self) -> TransferState {
        self.state
    }

    pub fn crc_retries(&self) -> u32 {
        self.crc_retries
    }

    /// 已连续接收到的块数（= 续传起点）
    pub fn resume_from_index(&self) -> u32 {
        self.next_index
    }

    /// 收 FILE_CHUNK（0x31）
    pub fn on_chunk(&mut self, chunk: FileChunk) -> Result<RecvAction, TransferError> {
        let meta = self
            .meta
            .as_ref()
            .ok_or(TransferError::Protocol("尚未收到 FILE_META".into()))?;
        if chunk.file_id != meta.file_id {
            return Err(TransferError::Protocol(format!(
                "FILE_CHUNK.file_id 不匹配：{} != {}",
                chunk.file_id, meta.file_id
            )));
        }
        if self.state.is_terminal() {
            return Err(TransferError::Protocol("任务已结束，拒绝继续接收".into()));
        }
        if chunk.index < self.next_index {
            return Ok(RecvAction::Skip { index: chunk.index });
        }
        if chunk.index > self.next_index {
            // 序号不连续 → 请求续传
            debuglog::log!(
                Level::Warn,
                "transfer",
                "resume.request",
                &[
                    ("file_id", &format!("{:#x}", chunk.file_id)),
                    ("from", &self.next_index.to_string()),
                    ("reason", "gap"),
                ]
            );
            return Ok(RecvAction::RequestResume {
                from_index: self.next_index,
            });
        }
        let chunk_size = meta.chunk_size as usize;
        let size = meta.size;
        let (start, end) = chunk_range(chunk.index, chunk_size, size)
            .ok_or_else(|| TransferError::Protocol(format!("块序号越界：{}", chunk.index)))?;
        let expect = (end - start) as usize;
        if chunk.data.len() != expect {
            return Err(TransferError::Protocol(format!(
                "第 {} 块长度应为 {expect}B，实际 {}B",
                chunk.index,
                chunk.data.len()
            )));
        }
        // 每块 CRC32；不符 → 请求重传该块（等价于「从本块续传」，块未落盘、进度不推进）。
        // 重传次数设上限，超限直接终止，防恶意对端无限触发重传（放大/死循环）。
        let actual = crc32(&chunk.data);
        if actual != chunk.crc32 {
            self.crc_retries += 1;
            if self.crc_retries > MAX_CRC_RETRIES {
                self.state = TransferState::Failed;
                // 埋点：块校验反复失败、重传上限耗尽 → 终止
                debuglog::log!(
                    Level::Error,
                    "transfer",
                    "recv.crc_exhausted",
                    &[
                        ("index", &chunk.index.to_string()),
                        ("retries", &self.crc_retries.to_string()),
                    ]
                );
                return Ok(RecvAction::CrcRetriesExhausted {
                    index: chunk.index,
                    retries: self.crc_retries,
                });
            }
            // 埋点：块 CRC32 校验失败 → 请求重传该块
            debuglog::log!(
                Level::Warn,
                "transfer",
                "recv.crc_mismatch",
                &[
                    ("index", &chunk.index.to_string()),
                    ("retries", &self.crc_retries.to_string()),
                ]
            );
            return Ok(RecvAction::RequestResume {
                from_index: self.next_index,
            });
        }
        self.hasher.update(&chunk.data);
        self.bytes_done += chunk.data.len() as u64;
        // 埋点：接收分块进度（节流：每 64 块一条，只记 index/len）
        if chunk.index.is_multiple_of(64) {
            debuglog::log!(
                Level::Info,
                "transfer",
                "recv.chunk",
                &[
                    ("index", &chunk.index.to_string()),
                    ("len", &chunk.data.len().to_string()),
                ]
            );
        }
        self.next_index += 1;
        Ok(RecvAction::Write {
            index: chunk.index,
            offset: start,
            data: chunk.data.to_vec(),
        })
    }

    /// 收 FILE_DONE（0x32）：对端已完成（`ok=false` 表示对端报错）
    pub fn on_done(&mut self, done: FileDone) -> Result<RecvAction, TransferError> {
        let meta = self
            .meta
            .as_ref()
            .ok_or(TransferError::Protocol("尚未收到 FILE_META".into()))?;
        if done.file_id != meta.file_id {
            return Err(TransferError::Protocol(format!(
                "FILE_DONE.file_id 不匹配：{} != {}",
                done.file_id, meta.file_id
            )));
        }
        if !done.ok {
            if done.cancelled {
                self.state = TransferState::Cancelled;
                // 埋点：对端取消收尾
                debuglog::log!(
                    Level::Info,
                    "transfer",
                    "recv.peer_cancelled",
                    &[("file_id", &format!("{:#x}", done.file_id))]
                );
                return Ok(RecvAction::PeerCancelled {
                    reason: match done.error {
                        Some(s) if !s.trim().is_empty() => s,
                        _ => "对方取消了这次传输".to_string(),
                    },
                });
            }
            self.state = TransferState::Failed;
            // 埋点：对端报告传输失败
            debuglog::log!(
                Level::Warn,
                "transfer",
                "recv.peer_failed",
                &[("file_id", &format!("{:#x}", done.file_id))]
            );
            return Ok(RecvAction::PeerFailed { error: done.error });
        }
        self.state = TransferState::Verifying;
        let total = chunks_total(meta.size, meta.chunk_size as usize);
        if self.next_index < total {
            self.state = TransferState::Paused;
            // 埋点：收尾时块数不足 → 请求续传
            debuglog::log!(
                Level::Warn,
                "transfer",
                "resume.request",
                &[
                    ("file_id", &format!("{:#x}", done.file_id)),
                    ("from", &self.next_index.to_string()),
                    ("reason", "incomplete"),
                ]
            );
            return Ok(RecvAction::RequestResume {
                from_index: self.next_index,
            });
        }
        // 摘要长度非法：既不是「缺位(0B)」也不是「完整(32B)」→ 协议违规，不许猜
        if !(done.sha256.is_empty() || done.sha256.len() == 32) {
            return Err(TransferError::Protocol(format!(
                "FILE_DONE.sha256 须为 0B 或 32B（实际 {}）",
                done.sha256.len()
            )));
        }
        // 校验源到收尾时才定：FILE_DONE 的流式摘要优先，缺位才回落到 FILE_META 的声明值（旧端）
        let expected = fixed_digest(&done.sha256).or_else(|| fixed_digest(&meta.sha256));
        let (actual_sha, _actual_crc) = self.hasher.clone().finish();
        let Some(expected) = expected else {
            self.state = TransferState::Failed;
            // 埋点：两帧都没有摘要，完整性无从确认
            debuglog::log!(
                Level::Error,
                "transfer",
                "recv.no_digest",
                &[("file_id", &format!("{:#x}", done.file_id))]
            );
            return Ok(RecvAction::NoDigest {
                actual_sha256: actual_sha,
            });
        };
        if actual_sha != expected {
            self.state = TransferState::Paused;
            // 埋点：整文件 SHA-256 校验失败
            debuglog::log!(
                Level::Error,
                "transfer",
                "recv.verify_failed",
                &[("file_id", &format!("{:#x}", done.file_id))]
            );
            return Ok(RecvAction::VerifyFailed {
                expected_sha256: expected,
                actual_sha256: actual_sha,
            });
        }
        self.state = TransferState::Done;
        // 埋点：整文件校验通过、传输完成
        debuglog::log!(
            Level::Info,
            "transfer",
            "recv.completed",
            &[
                ("file_id", &format!("{:#x}", done.file_id)),
                ("bytes", &meta.size.to_string()),
            ]
        );
        Ok(RecvAction::Completed)
    }

    /// `RecvAction::RequestResume` 对应的 MSG_RESUME body（收端 → 发端）
    pub fn resume_body(&self, from_index: u32) -> Result<Vec<u8>, TransferError> {
        let file_id = self
            .file_id()
            .ok_or(TransferError::Protocol("尚未收到 FILE_META".into()))?;
        Ok(protocol::encode_resume(file_id, from_index))
    }

    /// 对端的 FILE_DONE 回执 body（收端确认/否认整文件校验结果）
    pub fn done_reply(&self, ok: bool, error: Option<&str>) -> Result<Vec<u8>, TransferError> {
        let file_id = self
            .file_id()
            .ok_or(TransferError::Protocol("尚未收到 FILE_META".into()))?;
        Ok(protocol::encode_done(file_id, ok, error))
    }

    pub fn progress(&self) -> TransferProgress {
        let (total_bytes, total_chunks) = match &self.meta {
            Some(m) => (m.size, chunks_total(m.size, m.chunk_size as usize)),
            None => (0, 0),
        };
        TransferProgress {
            bytes_done: self.bytes_done,
            total_bytes,
            chunks_done: self.next_index,
            chunks_total: total_chunks,
        }
    }
}

// ---------------------------------------------------------------- 队列 / 历史

/// 队列项（UI 列表数据源）
#[derive(Debug, Clone)]
pub struct QueueItem {
    pub queue_id: u64,
    pub direction: TransferDirection,
    pub name: String,
    pub size: u64,
    pub state: TransferState,
    pub progress: TransferProgress,
}

/// 传输队列（FIFO + 状态 + 进度）
#[derive(Debug, Default)]
pub struct TransferQueue {
    items: VecDeque<QueueItem>,
    next_id: u64,
}

impl TransferQueue {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn enqueue(
        &mut self,
        direction: TransferDirection,
        name: impl Into<String>,
        size: u64,
    ) -> u64 {
        self.next_id += 1;
        let id = self.next_id;
        self.items.push_back(QueueItem {
            queue_id: id,
            direction,
            name: name.into(),
            size,
            state: TransferState::Queued,
            progress: TransferProgress {
                total_bytes: size,
                ..TransferProgress::default()
            },
        });
        id
    }

    pub fn items(&self) -> impl Iterator<Item = &QueueItem> {
        self.items.iter()
    }

    pub fn get(&self, queue_id: u64) -> Option<&QueueItem> {
        self.items.iter().find(|i| i.queue_id == queue_id)
    }

    pub fn get_mut(&mut self, queue_id: u64) -> Option<&mut QueueItem> {
        self.items.iter_mut().find(|i| i.queue_id == queue_id)
    }

    pub fn update_progress(&mut self, queue_id: u64, progress: TransferProgress) -> bool {
        match self.get_mut(queue_id) {
            Some(item) => {
                item.progress = progress;
                true
            }
            None => false,
        }
    }

    pub fn set_state(&mut self, queue_id: u64, state: TransferState) -> bool {
        match self.get_mut(queue_id) {
            Some(item) => {
                item.state = state;
                true
            }
            None => false,
        }
    }

    pub fn remove(&mut self, queue_id: u64) -> bool {
        let before = self.items.len();
        self.items.retain(|i| i.queue_id != queue_id);
        self.items.len() != before
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// 进行中数量（Running / Verifying）——驱动 UI「N 个任务进行中」
    pub fn active_count(&self) -> usize {
        self.items
            .iter()
            .filter(|i| matches!(i.state, TransferState::Running | TransferState::Verifying))
            .count()
    }

    pub fn queued_count(&self) -> usize {
        self.items
            .iter()
            .filter(|i| i.state == TransferState::Queued)
            .count()
    }

    /// 取出下一个待执行项（FIFO，队列调度用；不改变状态）
    pub fn next_queued(&self) -> Option<&QueueItem> {
        self.items.iter().find(|i| i.state == TransferState::Queued)
    }
}

/// 传输历史条目
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryEntry {
    pub queue_id: u64,
    pub direction: TransferDirection,
    pub name: String,
    pub size: u64,
    /// 终态：Done / Failed / Cancelled
    pub outcome: TransferState,
    pub ts_ms: i64,
    /// 端到端校验是否通过（收端整文件校验 / 发端对端回执）
    pub verified: bool,
    pub error: Option<String>,
}

/// 传输历史（有界环形，默认 50 条，与通知历史口径一致）
#[derive(Debug)]
pub struct TransferHistory {
    capacity: usize,
    entries: VecDeque<HistoryEntry>,
}

impl Default for TransferHistory {
    fn default() -> Self {
        Self::new(Self::DEFAULT_CAPACITY)
    }
}

impl TransferHistory {
    pub const DEFAULT_CAPACITY: usize = 50;

    pub fn new(capacity: usize) -> Self {
        assert!(capacity > 0, "历史容量必须为正");
        Self {
            capacity,
            entries: VecDeque::new(),
        }
    }

    /// 追加一条；溢出时返回被淘汰的最旧条目
    pub fn push(&mut self, entry: HistoryEntry) -> Option<HistoryEntry> {
        let evicted = if self.entries.len() >= self.capacity {
            self.entries.pop_front()
        } else {
            None
        };
        self.entries.push_back(entry);
        evicted
    }

    /// 最新在前
    pub fn entries(&self) -> impl Iterator<Item = &HistoryEntry> {
        self.entries.iter().rev()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn clear(&mut self) {
        self.entries.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chunk::{chunks_total, sha256, Chunker, FileHasher};
    use std::io::Cursor;

    fn payload(len: usize) -> Vec<u8> {
        (0..len as u32).map(|i| (i % 251) as u8).collect()
    }

    /// 「拿不到摘要就不许报成功」现在写在函数里而不是调用纪律里：摘要没算全时
    /// `finish(true, ..)` 也必须落成失败，否则对端收到的是一条"传完了"的空摘要回执。
    #[test]
    fn finish_without_complete_digest_reports_failure() {
        let mut t = SendTask::streaming(7, "a.bin", 1024, 256).unwrap();
        let done = protocol::decode_done(&t.finish(true, None)).unwrap();
        assert!(!done.ok, "流式任务一块都没发，不能报成功");
        assert!(done.sha256.is_empty(), "更不该带出摘要");
        assert!(
            done.error.unwrap_or_default().contains("摘要"),
            "要说清为什么从成功改成了失败"
        );
        assert_eq!(t.state, TransferState::Failed);

        // 预先声明摘要的旧口径不受影响
        let mut old = SendTask::new(8, "b.bin", 4, [1u8; 32], 2, 256).unwrap();
        let done = protocol::decode_done(&old.finish(true, None)).unwrap();
        assert!(done.ok, "META 里就带摘要的口径照旧成功");
        assert_eq!(&done.sha256[..], &[1u8; 32][..]);
        // 失败时调用方给的原因原样保留，不被这里改写
        let mut t3 = SendTask::streaming(9, "c.bin", 4, 256).unwrap();
        let done = protocol::decode_done(&t3.finish(false, Some("链路断了"))).unwrap();
        assert_eq!(done.error.unwrap_or_default(), "链路断了");
    }

    /// 端到端（内存内）：发送任务产出 body → 接收任务消费
    fn drive(sender: &mut SendTask, data: &[u8]) -> Vec<(u32, RecvAction)> {
        let mut recv = RecvTask::new();
        let mut actions = Vec::new();
        recv.on_meta(protocol::decode_meta(&sender.meta_body()).unwrap(), 0, None)
            .unwrap();
        let mut chunker = Chunker::with_chunk_size(Cursor::new(data.to_vec()), sender.chunk_size);
        while let Some(c) = chunker.next_chunk().unwrap() {
            let body = sender.make_chunk_body(&c.data).unwrap().unwrap();
            let chunk = protocol::decode_chunk(&body).unwrap();
            actions.push((chunk.index, recv.on_chunk(chunk).unwrap()));
        }
        let done_body = sender.finish(true, None);
        actions.push((
            u32::MAX,
            recv.on_done(protocol::decode_done(&done_body).unwrap())
                .unwrap(),
        ));
        actions
    }

    fn make_sender(data: &[u8], chunk_size: usize) -> SendTask {
        let mut h = FileHasher::new();
        h.update(data);
        let (sha, crc) = h.finish();
        SendTask::new(7, "file.bin", data.len() as u64, sha, crc, chunk_size).unwrap()
    }

    fn stream_chunks(sender: &mut SendTask, recv: &mut RecvTask, data: &[u8], from: u32) {
        let mut chunker = Chunker::with_chunk_size(Cursor::new(data.to_vec()), sender.chunk_size);
        // 丢弃 from 之前的块（收端已由前缀累计器覆盖）
        for _ in 0..from {
            chunker.next_chunk().unwrap();
        }
        while let Some(c) = chunker.next_chunk().unwrap() {
            let body = sender.make_chunk_body(&c.data).unwrap().unwrap();
            recv.on_chunk(protocol::decode_chunk(&body).unwrap())
                .expect("续传后逐块应可接收");
        }
    }

    /// 流式发送的收端不应先知道摘要：META 里是空的
    fn streaming_pair(data: &[u8]) -> (SendTask, RecvTask) {
        let mut sender = SendTask::streaming(7, "big.bin", data.len() as u64, CHUNK_SIZE).unwrap();
        sender.start();
        let meta = protocol::decode_meta(&sender.meta_body()).unwrap();
        assert!(meta.sha256.is_empty(), "流式 FILE_META 不该带整文件摘要");
        let mut recv = RecvTask::new();
        recv.on_meta(meta, 0, None).unwrap();
        (sender, recv)
    }

    #[test]
    fn happy_path_with_per_chunk_crc() {
        let data = payload(CHUNK_SIZE + 12_345); // 2 块
        let mut sender = make_sender(&data, CHUNK_SIZE);
        sender.start();
        let actions = drive(&mut sender, &data);
        assert_eq!(actions.len(), 3);
        assert!(matches!(
            actions[0].1,
            RecvAction::Write {
                index: 0,
                offset: 0,
                ..
            }
        ));
        match &actions[1].1 {
            RecvAction::Write { index, offset, .. } => {
                assert_eq!(*index, 1);
                assert_eq!(*offset, CHUNK_SIZE as u64);
            }
            other => panic!("期望第 2 块 Write，实际 {other:?}"),
        }
        assert_eq!(actions[2].1, RecvAction::Completed);
        assert_eq!(sender.state, TransferState::Done);
        assert_eq!(sender.progress().percent(), 100);
    }

    #[test]
    fn empty_file_completes_immediately() {
        let data: Vec<u8> = Vec::new();
        let mut sender = make_sender(&data, CHUNK_SIZE);
        sender.start();
        assert_eq!(sender.chunks_total(), 0);
        assert_eq!(sender.next_chunk_range(), None);
        assert!(sender.make_chunk_body(&[]).unwrap().is_none());
        let actions = drive(&mut sender, &data);
        assert_eq!(actions.len(), 1);
        assert_eq!(actions[0].1, RecvAction::Completed);
        assert_eq!(sender.progress().percent(), 100);
    }

    /// 大文件首帧不再等整文件摘要：META 立即发、摘要随分块增量算、FILE_DONE 交付，
    /// 收端据此校验（FILE_META 全程没有摘要，只能来自 DONE）。
    #[test]
    fn streaming_roundtrip_verifies_done_digest() {
        let data = payload(CHUNK_SIZE * 2 + 5); // 3 块
        let (mut sender, mut recv) = streaming_pair(&data);

        // 发 META 时一个字节都没读过（旧实现：此处已经把整个文件哈希完）
        assert_eq!(sender.digest_bytes(), 0);
        let mut chunker = Chunker::with_chunk_size(Cursor::new(data.clone()), CHUNK_SIZE);
        let mut produced = 0usize;
        while let Some(c) = chunker.next_chunk().unwrap() {
            let body = sender.make_chunk_body(&c.data).unwrap().unwrap();
            produced += 1;
            if produced == 1 {
                // 首块已发出，摘要只覆盖首块 —— 证明没有整文件预读
                assert_eq!(sender.digest_bytes(), CHUNK_SIZE as u64);
                assert!(
                    sender.whole_file_digest().is_none(),
                    "未发完不该有整文件摘要"
                );
            }
            recv.on_chunk(protocol::decode_chunk(&body).unwrap())
                .unwrap();
        }
        assert_eq!(produced, 3);

        let done_body = sender.finish(true, None);
        let done = protocol::decode_done(&done_body).unwrap();
        assert_eq!(
            done.sha256.as_ref(),
            sha256(&data).as_slice(),
            "FILE_DONE 必须带上整文件摘要"
        );
        assert_eq!(recv.on_done(done).unwrap(), RecvAction::Completed);
        assert_eq!(recv.state(), TransferState::Done);
        assert_eq!(sender.state, TransferState::Done);
    }

    /// 摘要不符 → 可见失败（VerifyFailed），落 `Completed` 就是骗用户
    #[test]
    fn streaming_done_digest_mismatch_is_visible_failure() {
        let data = payload(CHUNK_SIZE + 7);
        let (mut sender, mut recv) = streaming_pair(&data);
        // 收端拿到的是被换掉一字节的内容：块 CRC 一并被篡改者重算，块级校验查不出来
        let mut tampered = data.clone();
        tampered[CHUNK_SIZE] ^= 0xFF;
        let mut chunker = Chunker::with_chunk_size(Cursor::new(data.clone()), CHUNK_SIZE);
        while let Some(c) = chunker.next_chunk().unwrap() {
            let body = sender.make_chunk_body(&c.data).unwrap().unwrap();
            let mut chunk = protocol::decode_chunk(&body).unwrap();
            if chunk.index == 1 {
                let bad = tampered[CHUNK_SIZE..].to_vec();
                chunk.crc32 = crc32(&bad);
                chunk.data = bad.into();
            }
            recv.on_chunk(chunk).unwrap();
        }
        let done = protocol::decode_done(&sender.finish(true, None)).unwrap();
        match recv.on_done(done).unwrap() {
            RecvAction::VerifyFailed {
                expected_sha256,
                actual_sha256,
            } => {
                assert_eq!(expected_sha256, sha256(&data));
                assert_eq!(actual_sha256, sha256(&tampered));
            }
            other => panic!("摘要不符必须判 VerifyFailed，实际 {other:?}"),
        }
        assert_eq!(recv.state(), TransferState::Paused);
    }

    /// 续传摘要口径：`resume_from` 清空累计器，由平台层把跳过的前缀重新喂进来，
    /// 最终摘要与「从未中断」逐字节等价。
    #[test]
    fn streaming_resume_digest_covers_skipped_prefix() {
        let data = payload(CHUNK_SIZE * 3 + 11); // 4 块
        let mut sender = SendTask::streaming(7, "r.bin", data.len() as u64, CHUNK_SIZE).unwrap();
        sender.start();
        // 前 2 块正常发出
        let mut sent: u32 = 0;
        while sent < 2 {
            let (s, e) = chunk_range(sent, CHUNK_SIZE, data.len() as u64).unwrap();
            sender
                .make_chunk_body(&data[s as usize..e as usize])
                .unwrap()
                .unwrap();
            sent += 1;
        }
        assert_eq!(sender.digest_bytes(), CHUNK_SIZE as u64 * 2);

        // 收端要第 2 块（index=1）重传
        sender.resume_from(1).unwrap();
        assert_eq!(sender.prefix_pending(), CHUNK_SIZE as u64);
        // 前缀没补就想发块 → 明确报错，不许少算一段摘要
        assert!(matches!(
            sender.make_chunk_body(&data[CHUNK_SIZE..CHUNK_SIZE * 2]),
            Err(TransferError::Protocol(_))
        ));

        // 收端已落盘 index=0，续传从 1 开始（前缀累计器由平台层重算提供）
        let mut prefix = FileHasher::new();
        prefix.update(&data[..CHUNK_SIZE]);
        let meta = protocol::decode_meta(&sender.meta_body()).unwrap();
        let mut recv = RecvTask::new();
        recv.on_meta(meta, 1, Some(prefix)).unwrap();

        // 平台层重开文件顺序跳过前缀：跳过的字节喂进摘要（不发帧）
        sender.fold_prefix(&data[..CHUNK_SIZE]).unwrap();
        assert_eq!(sender.prefix_pending(), 0);
        stream_chunks(&mut sender, &mut recv, &data, 1);

        let done = protocol::decode_done(&sender.finish(true, None)).unwrap();
        assert_eq!(done.sha256.as_ref(), sha256(&data).as_slice());
        assert_eq!(recv.on_done(done).unwrap(), RecvAction::Completed);
    }

    /// 旧端互通：META 带摘要、FILE_DONE 不带 → 收端回落到 META 声明值
    #[test]
    fn legacy_meta_digest_fallback_still_completes() {
        let data = payload(CHUNK_SIZE + 100);
        let mut sender = make_sender(&data, CHUNK_SIZE);
        sender.start();
        let meta = protocol::decode_meta(&sender.meta_body()).unwrap();
        assert_eq!(meta.sha256.len(), 32, "旧口径 META 必须带摘要");
        let mut recv = RecvTask::new();
        recv.on_meta(meta, 0, None).unwrap();
        stream_chunks(&mut sender, &mut recv, &data, 0);
        let old_done = FileDone {
            file_id: 7,
            ok: true,
            error: None,
            sha256: Default::default(),
            cancelled: false,
        };
        assert_eq!(recv.on_done(old_done).unwrap(), RecvAction::Completed);
        // 增量累计器与预先声明的摘要覆盖同一批字节
        assert_eq!(sender.digest_bytes(), data.len() as u64);
    }

    /// 两帧都没有摘要：完整性无从确认，不许假装完成
    #[test]
    fn missing_digest_everywhere_is_not_completed() {
        let data = payload(1000);
        let (mut sender, mut recv) = streaming_pair(&data);
        stream_chunks(&mut sender, &mut recv, &data, 0);
        // 发送端算不出摘要（例如分块与摘要不同步）→ FILE_DONE 该字段留空
        let empty_done = FileDone {
            file_id: 7,
            ok: true,
            error: None,
            sha256: Default::default(),
            cancelled: false,
        };
        assert_eq!(
            recv.on_done(empty_done).unwrap(),
            RecvAction::NoDigest {
                actual_sha256: sha256(&data)
            }
        );
        assert_eq!(recv.state(), TransferState::Failed);
    }

    /// 取消收尾：发端落 `Cancelled`（不是 `Failed`）、FILE_DONE 带 `cancelled`、不交付摘要
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

    /// 收端收到取消收尾：状态 `Cancelled` + `PeerCancelled`，与 `PeerFailed` 严格区分
    #[test]
    fn recv_peer_cancelled_is_not_peer_failed() {
        let data = payload(1000);
        let (mut sender, mut recv) = streaming_pair(&data);
        sender.start();
        stream_chunks(&mut sender, &mut recv, &data, 0);
        let body = protocol::encode_done_cancelled(7, Some("手机侧取消了"));
        let act = recv.on_done(protocol::decode_done(&body).unwrap()).unwrap();
        assert_eq!(
            act,
            RecvAction::PeerCancelled {
                reason: "手机侧取消了".into()
            }
        );
        assert_eq!(recv.state(), TransferState::Cancelled);

        // 没有原因也不能空着：给一句能看的话
        let mut recv2 = RecvTask::new();
        recv2
            .on_meta(protocol::decode_meta(&sender.meta_body()).unwrap(), 0, None)
            .unwrap();
        let body2 = protocol::encode_done_cancelled(7, None);
        match recv2
            .on_done(protocol::decode_done(&body2).unwrap())
            .unwrap()
        {
            RecvAction::PeerCancelled { reason } => assert!(!reason.is_empty()),
            other => panic!("取消收尾必须走 PeerCancelled，实际 {other:?}"),
        }
    }

    /// 旧端（不带 cancelled 字段）的失败 FILE_DONE 仍然走 PeerFailed，语义不变
    #[test]
    fn legacy_peer_failed_is_unaffected_by_cancel_field() {
        let data = payload(1000);
        let (mut sender, mut recv) = streaming_pair(&data);
        sender.start();
        let body = protocol::encode_done(7, false, Some("磁盘满了"));
        assert_eq!(
            recv.on_done(protocol::decode_done(&body).unwrap()).unwrap(),
            RecvAction::PeerFailed {
                error: Some("磁盘满了".into())
            }
        );
        assert_eq!(recv.state(), TransferState::Failed);
    }

    #[test]
    fn out_of_order_chunk_triggers_resume_request() {
        let data = payload(1000);
        let mut sender = make_sender(&data, CHUNK_SIZE);
        sender.start();
        let mut recv = RecvTask::new();
        recv.on_meta(protocol::decode_meta(&sender.meta_body()).unwrap(), 0, None)
            .unwrap();
        // 直接喂第 2 块（跳过第 1 块）→ 请求从 0 续传
        let chunk = FileChunk {
            file_id: 7,
            index: 1,
            data: vec![0u8; 500].into(),
            crc32: crc32(&[0u8; 500]),
        };
        assert_eq!(
            recv.on_chunk(chunk).unwrap(),
            RecvAction::RequestResume { from_index: 0 }
        );
        assert_eq!(recv.resume_from_index(), 0);
        assert_eq!(
            protocol::decode_resume(&recv.resume_body(0).unwrap()).unwrap(),
            (7, 0)
        );
    }

    #[test]
    fn duplicate_chunk_is_skipped_idempotently() {
        let data = payload(600);
        let mut sender = make_sender(&data, CHUNK_SIZE);
        sender.start();
        let mut recv = RecvTask::new();
        recv.on_meta(protocol::decode_meta(&sender.meta_body()).unwrap(), 0, None)
            .unwrap();
        let body = sender.make_chunk_body(&data).unwrap().unwrap();
        let chunk = protocol::decode_chunk(&body).unwrap();
        assert!(matches!(
            recv.on_chunk(chunk.clone()).unwrap(),
            RecvAction::Write { index: 0, .. }
        ));
        assert_eq!(recv.on_chunk(chunk).unwrap(), RecvAction::Skip { index: 0 });
        assert_eq!(recv.crc_retries(), 0);
    }

    #[test]
    fn crc_mismatch_requests_retransmit_of_same_block() {
        let data = payload(600);
        let mut sender = make_sender(&data, CHUNK_SIZE);
        sender.start();
        let mut recv = RecvTask::new();
        recv.on_meta(protocol::decode_meta(&sender.meta_body()).unwrap(), 0, None)
            .unwrap();
        let mut chunk =
            protocol::decode_chunk(&sender.make_chunk_body(&data).unwrap().unwrap()).unwrap();
        chunk.crc32 ^= 0xFFFF; // 模拟线上块损坏
        assert_eq!(
            recv.on_chunk(chunk).unwrap(),
            RecvAction::RequestResume { from_index: 0 }
        );
        assert_eq!(recv.crc_retries(), 1);
        assert_eq!(recv.progress().bytes_done, 0); // 坏块不推进进度
    }

    #[test]
    fn whole_file_mismatch_marks_paused() {
        let data = payload(600);
        let mut sender = make_sender(&data, CHUNK_SIZE);
        sender.start();
        let mut meta = protocol::decode_meta(&sender.meta_body()).unwrap();
        meta.sha256 = vec![0u8; 32].into(); // 伪造错误的整文件哈希
        let chunk =
            protocol::decode_chunk(&sender.make_chunk_body(&data).unwrap().unwrap()).unwrap();

        // 旧端 FILE_DONE 不带摘要 → 校验源就是被伪造的 META 值：必须判失败
        let mut recv = RecvTask::new();
        recv.on_meta(meta.clone(), 0, None).unwrap();
        recv.on_chunk(chunk.clone()).unwrap();
        let old_done = FileDone {
            file_id: 7,
            ok: true,
            error: None,
            sha256: Default::default(),
            cancelled: false,
        };
        assert!(matches!(
            recv.on_done(old_done).unwrap(),
            RecvAction::VerifyFailed { .. }
        ));
        assert_eq!(recv.state(), TransferState::Paused);

        // 新端 FILE_DONE 带真摘要 → 以 DONE 为准，META 里的旧声明不再参与判定
        let mut recv2 = RecvTask::new();
        recv2.on_meta(meta, 0, None).unwrap();
        recv2.on_chunk(chunk).unwrap();
        let done = protocol::decode_done(&sender.finish(true, None)).unwrap();
        assert_eq!(done.sha256.as_ref(), sha256(&data).as_slice());
        assert_eq!(recv2.on_done(done).unwrap(), RecvAction::Completed);
    }

    #[test]
    fn peer_failure_reported() {
        let data = payload(10);
        let mut sender = make_sender(&data, CHUNK_SIZE);
        sender.start();
        let mut recv = RecvTask::new();
        recv.on_meta(protocol::decode_meta(&sender.meta_body()).unwrap(), 0, None)
            .unwrap();
        let done = protocol::encode_done(7, false, Some("disk full"));
        assert_eq!(
            recv.on_done(protocol::decode_done(&done).unwrap()).unwrap(),
            RecvAction::PeerFailed {
                error: Some("disk full".into())
            }
        );
        assert_eq!(recv.state(), TransferState::Failed);
    }

    #[test]
    fn resume_from_middle_block_and_verify_whole_file() {
        let data = payload(CHUNK_SIZE + 1_000); // 2 块：262144 + 1000
        let mut sender = make_sender(&data, CHUNK_SIZE);
        sender.start();
        // 平台层模拟：已写入完整第 1 块，续传从 index=1 开始
        let mut prefix = FileHasher::new();
        prefix.update(&data[..CHUNK_SIZE]);
        let mut recv = RecvTask::new();
        recv.on_meta(
            protocol::decode_meta(&sender.meta_body()).unwrap(),
            1,
            Some(prefix),
        )
        .unwrap();
        assert_eq!(recv.resume_from_index(), 1);
        assert_eq!(recv.progress().bytes_done, CHUNK_SIZE as u64);
        // 发端按 RESUME 指引起点续传：累计器清零，先把跳过的前缀补进摘要
        sender.resume_from(1).unwrap();
        assert_eq!(sender.next_index(), 1);
        sender.fold_prefix(&data[..CHUNK_SIZE]).unwrap();
        assert_eq!(
            sender.next_chunk_range(),
            Some((CHUNK_SIZE as u64, data.len() as u64))
        );
        let chunk = protocol::decode_chunk(
            &sender
                .make_chunk_body(&data[CHUNK_SIZE..])
                .unwrap()
                .unwrap(),
        )
        .unwrap();
        assert!(matches!(
            recv.on_chunk(chunk).unwrap(),
            RecvAction::Write { index: 1, .. }
        ));
        assert_eq!(
            recv.on_done(protocol::decode_done(&sender.finish(true, None)).unwrap())
                .unwrap(),
            RecvAction::Completed
        );
        assert_eq!(recv.progress().percent(), 100);
    }

    #[test]
    fn resume_without_prefix_hasher_rejected() {
        let data = payload(10);
        let mut sender = make_sender(&data, CHUNK_SIZE);
        sender.start();
        let mut recv = RecvTask::new();
        assert!(matches!(
            recv.on_meta(protocol::decode_meta(&sender.meta_body()).unwrap(), 1, None),
            Err(TransferError::Protocol(_))
        ));
        // 续传起点越界
        let mut recv2 = RecvTask::new();
        assert!(matches!(
            recv2.on_meta(
                protocol::decode_meta(&sender.meta_body()).unwrap(),
                9,
                Some(FileHasher::new())
            ),
            Err(TransferError::Protocol(_))
        ));
    }

    #[test]
    fn bad_meta_rejected() {
        let mut recv = RecvTask::new();
        let bad_sha = FileMeta {
            name: "x".into(),
            size: 1,
            file_id: 1,
            chunk_size: CHUNK_SIZE as u32,
            sha256: vec![0u8; 16].into(), // 非 32B
            crc32: 0,
            album_id: 0,
        };
        assert!(matches!(
            recv.on_meta(bad_sha, 0, None),
            Err(TransferError::Protocol(_))
        ));
        let bad_chunk_size = FileMeta {
            name: "x".into(),
            size: 1,
            file_id: 1,
            chunk_size: (CHUNK_SIZE + 1) as u32, // 超上限
            sha256: vec![0u8; 32].into(),
            crc32: 0,
            album_id: 0,
        };
        assert!(matches!(
            recv.on_meta(bad_chunk_size, 0, None),
            Err(TransferError::Protocol(_))
        ));
        // 4 GiB / 1 B = 2^32 块：块数窄化成 u32 会回绕成 0，"收满 total 块"被 0 块满足 ⇒
        // 一个空文件能被判成完整原件。这条声明必须在入口就拒，不进算术。
        let blocks_overflow_u32 = FileMeta {
            name: "x".into(),
            size: 4 * 1024 * 1024 * 1024,
            file_id: 1,
            chunk_size: 1,
            sha256: vec![0u8; 32].into(),
            crc32: 0,
            album_id: 0,
        };
        assert!(matches!(
            recv.on_meta(blocks_overflow_u32, 0, None),
            Err(TransferError::Protocol(_))
        ));
        assert_eq!(
            chunks_total(4 * 1024 * 1024 * 1024, 1),
            u32::MAX,
            "块数超出 u32 只许饱和、不许回绕成 0"
        );
        // 未收到 META 就收块
        let mut recv2 = RecvTask::new();
        assert!(matches!(
            recv2.on_chunk(FileChunk {
                file_id: 1,
                index: 0,
                data: vec![1].into(),
                crc32: 0
            }),
            Err(TransferError::Protocol(_))
        ));
    }

    #[test]
    fn queue_and_history_behaviour() {
        let mut q = TransferQueue::new();
        let a = q.enqueue(TransferDirection::Send, "a.bin", 100);
        let b = q.enqueue(TransferDirection::Recv, "b.bin", 200);
        let c = q.enqueue(TransferDirection::Send, "c.bin", 300);
        assert_eq!(q.len(), 3);
        assert_eq!(q.queued_count(), 3);
        assert_eq!(q.next_queued().unwrap().queue_id, a);
        q.set_state(a, TransferState::Running);
        q.set_state(b, TransferState::Running);
        q.update_progress(
            b,
            TransferProgress {
                bytes_done: 50,
                total_bytes: 200,
                chunks_done: 1,
                chunks_total: 4,
            },
        );
        assert_eq!(q.active_count(), 2);
        assert_eq!(q.queued_count(), 1);
        assert_eq!(q.get(b).unwrap().progress.percent(), 25);
        assert_eq!(q.next_queued().unwrap().queue_id, c);
        assert!(q.remove(c));
        assert!(!q.remove(c)); // 二次幂等
        assert_eq!(q.len(), 2);

        let mut h = TransferHistory::new(2);
        let entry = |id: u64| HistoryEntry {
            queue_id: id,
            direction: TransferDirection::Send,
            name: format!("f{id}"),
            size: 10,
            outcome: TransferState::Done,
            ts_ms: id as i64,
            verified: true,
            error: None,
        };
        assert!(h.push(entry(1)).is_none());
        assert!(h.push(entry(2)).is_none());
        assert_eq!(h.push(entry(3)).unwrap().queue_id, 1); // 淘汰最旧
        let ids: Vec<u64> = h.entries().map(|e| e.queue_id).collect();
        assert_eq!(ids, vec![3, 2]); // 最新在前
        assert_eq!(h.len(), 2);
        h.clear();
        assert!(h.is_empty());
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

    /// 反向用例：路径穿越 / 非法字符 / 保留设备名都必须被**塌成安全名并照常接收**。
    /// "拒绝"式写法会让接收侧静默丢帧、发送侧永远停在"传输中"。
    #[test]
    fn hostile_file_names_collapse_and_still_accept() {
        for bad in [
            "../evil.sh",
            "..\\evil.exe",
            "/etc/passwd",
            "C:\\Windows\\evil.dll",
            "a/b.txt",
            "..",
            ".",
            "",
            "   ",
            "con.txt",
            "NUL",
            "bad\u{0}name",
            "bad\nname",
            "report:a.txt",
            "report.",
        ] {
            let mut recv = RecvTask::new();
            let meta = FileMeta {
                name: bad.into(),
                size: 10,
                file_id: 1,
                chunk_size: CHUNK_SIZE as u32,
                sha256: vec![0u8; 32].into(),
                crc32: 0,
                album_id: 0,
            };
            recv.on_meta(meta, 0, None)
                .unwrap_or_else(|e| panic!("{bad:?} 应被改名接收，实际报错：{e}"));
            let safe = recv.file_name().expect("改名后必须留下安全名");
            assert!(!safe.is_empty(), "{bad:?} 改出了空名");
            assert!(
                !safe.contains('/') && !safe.contains('\\'),
                "{bad:?} → {safe:?} 仍含分隔符"
            );
            assert_ne!(safe, "..", "{bad:?} 改出了可穿越的段");
            // 断言**显式**按 Windows 规则跑一遍，不跟着 `Rules::current()` 走：在 Linux CI 上
            // 它会退化成 Fat（不查保留设备名），那条"`con.txt`/`NUL` 必须被改掉"的不变量
            // 就永远测不到——一条只在开发机上绿的测试等于没有测试。
            let win = crate::filename::sanitize(bad, crate::filename::Rules::Windows);
            assert!(
                crate::filename::is_valid(&win, crate::filename::Rules::Windows),
                "{bad:?} 按 Windows 规则改名后仍不合法：{win:?}"
            );
        }
    }

    /// 反向用例：持续坏块 → 重传至上限后置 Failed（不再无限重传）
    #[test]
    fn crc_retry_cap_terminates_task() {
        let data = payload(600);
        let mut sender = make_sender(&data, CHUNK_SIZE);
        sender.start();
        let mut recv = RecvTask::new();
        recv.on_meta(protocol::decode_meta(&sender.meta_body()).unwrap(), 0, None)
            .unwrap();
        let mut chunk =
            protocol::decode_chunk(&sender.make_chunk_body(&data).unwrap().unwrap()).unwrap();
        chunk.crc32 ^= 0xFFFF; // 持续损坏
        for i in 1..=MAX_CRC_RETRIES {
            assert_eq!(
                recv.on_chunk(chunk.clone()).unwrap(),
                RecvAction::RequestResume { from_index: 0 },
                "第 {i} 次仍在重试窗口内"
            );
            assert_eq!(recv.crc_retries(), i);
        }
        // 超过上限 → 终止
        assert_eq!(
            recv.on_chunk(chunk).unwrap(),
            RecvAction::CrcRetriesExhausted {
                index: 0,
                retries: MAX_CRC_RETRIES + 1
            }
        );
        assert_eq!(recv.state(), TransferState::Failed);
        // 终态后拒绝继续接收
        assert!(matches!(
            recv.on_chunk(FileChunk {
                file_id: 7,
                index: 0,
                data: vec![0u8; 600].into(),
                crc32: 0,
            }),
            Err(TransferError::Protocol(_))
        ));
    }

    /// 反向用例：非法分块大小 → Err 而非 panic
    #[test]
    fn send_task_new_rejects_bad_chunk_size_without_panic() {
        assert!(SendTask::new(1, "a", 0, [0u8; 32], 0, 0).is_err());
        assert!(SendTask::new(1, "a", 0, [0u8; 32], 0, CHUNK_SIZE + 1).is_err());
        assert!(SendTask::new(1, "a", 0, [0u8; 32], 0, CHUNK_SIZE).is_ok());
        assert!(SendTask::new(1, "a", 0, [0u8; 32], 0, 1).is_ok());
    }
}
