//! 文件收发会话与辅助（分层：`linkx-transfer` 只管分块 / CRC32 / SHA-256 纯逻辑，**文件 IO 在本壳完成**）。收发编排（何时发 META、何时落盘、何时续传）仍由 `app::Worker` 驱动。

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use linkx_transfer::{Chunker, FileHasher, SendTask};

use crate::app::now_ms;

pub(crate) struct SendSession {
    pub(crate) file_id: u64,
    /// 引擎任务：分块发出时同步喂入增量摘要（FILE_DONE 交付整文件摘要，不再预扫整文件）
    pub(crate) task: SendTask,
    pub(crate) chunker: Chunker<fs::File>,
    pub(crate) name: String,
    pub(crate) total_chunks: u32,
    pub(crate) sent_chunks: u32,
    pub(crate) resume_from: Option<u32>,
    pub(crate) path: PathBuf,
    /// 本端为"对端发现的洞"重启过几次补发（上限 `MAX_RESUME_TRIES`，与收端预算同口径）：没有它，一个反复重放的迟到 RESUME 能把发送循环拖着无限补发
    pub(crate) resume_rounds: u32,
    /// 分块全部发出后等待对端 FILE_DONE 回执的截止时刻（毫秒，0 = 尚未进入等待）。**「发完」不等于「对方收好了」**：
    /// 旧代码在 EOF 直接标已完成，于是分块半路丢失时用户看到的仍是成功
    pub(crate) ack_deadline_ms: i64,
}

impl SendSession {
    pub(crate) fn percent(&self) -> u8 {
        percent_of(self.sent_chunks as u64, self.total_chunks as u64)
    }
}

pub(crate) struct RecvSession {
    pub(crate) file_id: u64,
    pub(crate) name: String,
    pub(crate) path: PathBuf,
    pub(crate) file: fs::File,
    pub(crate) size: u64,
    pub(crate) chunk_size: u32,
    /// 下一个期望分块（严格连续 → 直接决定落盘偏移）
    pub(crate) next_index: u32,
    pub(crate) received: u64,
    pub(crate) hasher: FileHasher,
    pub(crate) expect_sha256: Option<[u8; 32]>,
    pub(crate) album_id: u64,
    pub(crate) resume_tries: u32,
    /// 已经请求过、还没看到进展的那个起点。有了它，预算才按"轮次"算而不是按"到多少块"算：
    /// 一个空洞之后的每一块都会再触发一次续传请求，292 块的视频能在几毫秒里把 8 次预算烧光并放弃接收
    pub(crate) pending_resume: Option<u32>,
    /// 收到 FILE_DONE 但本端有洞时的**暂缓收尾**截止时刻（毫秒，0 = 不在暂缓）。旧实现在这里直接摘会话、回执"不完整"，
    /// 于是发端那条 `FILE_DONE` 成了终审 —— 它发完就再没有人读续传请求，迟到的续传结构上不可能被满足。现在有预算就先不收尾
    pub(crate) resume_hold_ms: i64,
    /// **早到**的分块（洞之后的那些），等洞补上再按序喂进摘要器。落盘是随机写（偏移 = `index × chunk_size`），顺序只对增量 SHA-256
    /// 有意义，所以"来早了"从来不该被丢掉 —— 旧实现直接丢，等于把一次乱序升级成"必须靠续传救回来"。超出 `AHEAD_BYTES_MAX` 才回落成续传请求
    pub(crate) ahead: std::collections::BTreeMap<u32, Vec<u8>>,
    pub(crate) ahead_bytes: usize,
}

pub(crate) fn new_file_id() -> u64 {
    static SEQ: AtomicU64 = AtomicU64::new(1);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed) & 0xFFFF;
    ((now_ms().max(1) as u64) << 16) | seq
}

pub(crate) fn file_base_name(path: &str) -> String {
    Path::new(path.trim())
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "待发送文件".to_string())
}

pub(crate) fn percent_of(done: u64, total: u64) -> u8 {
    if total == 0 {
        return 100;
    }
    ((done.min(total) * 100) / total) as u8
}

/// 打开分块读取器，并顺序跳过 `skip` 块（续传时保持 `Chunker` 的 index 与文件偏移一致）。
/// 跳过的前缀**不重发**但必须喂进 `task` 的摘要累计器 —— 否则 FILE_DONE 交付的整文件摘要会缺一段，收端必然判不符（那是假失败，比真失败更难查）
pub(crate) fn open_chunker(
    p: &Path,
    skip: u32,
    task: &mut SendTask,
) -> Result<Chunker<fs::File>, String> {
    let f = fs::File::open(p).map_err(|e| format!("打开文件失败: {e}"))?;
    let mut c = Chunker::new(f);
    for _ in 0..skip {
        match c.next_chunk() {
            Ok(Some(chunk)) => {
                if task.prefix_pending() > 0 {
                    task.fold_prefix(&chunk.data)
                        .map_err(|e| format!("续传补算摘要失败: {e}"))?;
                }
            }
            Ok(None) => return Err("续传起点超过文件长度".to_string()),
            Err(e) => return Err(format!("读取文件失败: {e}")),
        }
    }
    Ok(c)
}

/// 单张拖出的载荷目录 `%TEMP%\LinkX`。**这是拖拽载荷，不是缓存**：拖放一开始 shell 就要能读到这个文件，所以必须先落这一份，拖完（或放弃）立即删；缩略图只留内存
pub(crate) fn album_drag_dir() -> PathBuf {
    std::env::temp_dir().join("LinkX")
}

/// 清掉 `%TEMP%\LinkX` 里遗留的拖拽载荷（上一次没拖完就退出、或中途崩溃留下的），只在启动时跑一次。
/// 这个目录是相册"关掉就没有"口径唯一的例外；只删本目录下的**文件**，不递归、不动子目录
pub(crate) fn sweep_album_drag_dir() {
    let Ok(entries) = std::fs::read_dir(album_drag_dir()) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_file() {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// 收件目录内的安全落盘路径：重名时追加 ` (n)`，绝不覆盖已有文件。"查一下不存在"与"创建"之间是有窗口的——
/// 另一条连接、同步盘、用户手工建的文件都能挤进去，而调用方随后的 `File::create` 会**静默截断**那个已存在的文件；
/// 所以用 `create_new` 把两步并成一次原子操作。999 个序号全占时返回 `None` 按"拒收"处理：返回已存在的路径等于重写旧文件。
pub(crate) fn unique_path(dir: &Path, name: &str) -> Option<PathBuf> {
    let base = Path::new(name);
    let stem = base
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| name.to_string());
    let ext = base.extension().map(|s| s.to_string_lossy().to_string());
    for n in 0..1000u32 {
        let cand = if n == 0 {
            dir.join(base)
        } else {
            let fname = match &ext {
                Some(e) if !e.is_empty() => format!("{stem} ({n}).{e}"),
                _ => format!("{stem} ({n})"),
            };
            dir.join(fname)
        };
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&cand)
        {
            Ok(_) => return Some(cand),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            // 目录不可写、路径过长这类真错误：再试下一个序号没有意义，如实按"没拿到路径"处理
            Err(_) => return None,
        }
    }
    None
}

/// 目标目录所在卷的可用字节（`GetDiskFreeSpaceExW`）。读不到就返回 `None` —— 「查不到余量」不等于「没空间」，
/// 把它当成拒绝会误伤正常传输。这个检查只为挡住"一次传输把盘写爆"；真写不下时 `land_chunk` 的写失败路径照样会大声收尾
pub(crate) fn volume_free_bytes(dir: &Path) -> Option<u64> {
    let mut avail = 0u64;
    let mut total = 0u64;
    let mut free = 0u64;
    let name = windows::core::HSTRING::from(dir.to_string_lossy().as_ref());
    // SAFETY: 三个出参是本函数拥有的 `u64`，地址有效；`name` 在本次调用期间一定活着。失败原因不用管：读不到余量就是要返回 `None`
    unsafe {
        windows::Win32::Storage::FileSystem::GetDiskFreeSpaceExW(
            &name,
            Some(&mut avail),
            Some(&mut total),
            Some(&mut free),
        )
    }
    .map(|_| avail)
    .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 剩余空间读得到就给数、读不到给 `None`：**不许**把"读不到"变成"没空间" —— 后者会让一次正常的接收被误拒成"空间不足"这种假原因
    #[test]
    fn volume_free_bytes_reports_or_shrugs_but_never_invents_zero() {
        let got = volume_free_bytes(&std::env::temp_dir());
        assert!(got.is_some(), "临时目录所在卷应读得到余量");
        assert!(got.unwrap() > 0, "可用字节应为正数");
        assert_eq!(
            volume_free_bytes(Path::new(r"\\?\LINKX-NO-SUCH-PATH-9f3a")),
            None,
            "不存在的路径要返回 None，而不是 Some(0)"
        );
    }

    #[test]
    fn progress_percent_is_clamped() {
        assert_eq!(percent_of(0, 0), 100);
        assert_eq!(percent_of(0, 100), 0);
        assert_eq!(percent_of(33, 100), 33);
        assert_eq!(percent_of(999, 100), 100);
    }

    #[test]
    fn file_ids_are_nonzero_and_unique() {
        let a = new_file_id();
        let b = new_file_id();
        assert_ne!(a, 0);
        assert_ne!(a, b);
    }

    #[test]
    fn unique_path_never_overwrites() {
        let dir = std::env::temp_dir().join("linkx-test-inbox");
        let _ = fs::create_dir_all(&dir);
        let first = unique_path(&dir, "报告.pdf").unwrap();
        assert_eq!(first.file_name().unwrap(), "报告.pdf");
        fs::write(&first, b"x").unwrap();
        let second = unique_path(&dir, "报告.pdf").unwrap();
        assert_ne!(second, first);
        assert_eq!(second.file_name().unwrap(), "报告 (1).pdf");
        let _ = fs::remove_file(&first);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn open_chunker_skips_prefix() {
        use linkx_transfer::{sha256, CHUNK_SIZE};
        let p = std::env::temp_dir().join("linkx-test-chunker.bin");
        let data = vec![7u8; CHUNK_SIZE + 10];
        fs::write(&p, &data).unwrap();
        let mut task = SendTask::streaming(1, "t.bin", data.len() as u64, CHUNK_SIZE).unwrap();
        task.start();
        task.resume_from(1).unwrap();
        let mut c = open_chunker(&p, 1, &mut task).unwrap();
        let chunk = c.next_chunk().unwrap().expect("应还有第 2 块");
        assert_eq!(chunk.index, 1, "跳过前缀后 index 必须是 1");
        assert_eq!(chunk.len(), 10);
        assert!(c.next_chunk().unwrap().is_none());
        assert_eq!(task.prefix_pending(), 0);
        task.note_sent(&chunk.data).unwrap();
        let (sha, _crc) = task
            .whole_file_digest()
            .expect("全部分块发完就该有整文件摘要");
        assert_eq!(sha, sha256(&data));
        assert!(open_chunker(&p, 5, &mut task).is_err());
        let _ = fs::remove_file(&p);
    }
}
