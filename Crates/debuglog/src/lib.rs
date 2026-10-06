//! LinkX 双端 Debug 模式日志核心：两端「设置」页一键开启后持续抓取全栈运行状态与错误，
//! 本地落盘（NDJSON + 纯文本可读版）、保留最近 4KB 现场（环形缓冲）、单文件上限滚动，
//! 并可导出到任意目录。**不做脱敏**（用户知情接受）。
//!
//! 设计口径：
//! - **平台无关**：落盘目录由平台层通过 [`enable`] 传入，核心层不硬编码平台路径。
//! - **关闭即零写盘**：入口先判 `AtomicBool`（Relaxed），关闭时「一次原子读 + 立即返回」。
//! - **绝不阻塞调用方**：日志经**有界通道**交后台线程落盘；通道满则丢弃并计数。
//! - **绝不 panic**：入口一律 `Result`/静默降级；写盘失败只置一次标志、不递归打日志，
//!   `Mutex::lock` 全用 `let Ok(..) else` 兜住（全 profile `panic=abort`，不可 unwrap）。
//! - **无第三方依赖**：JSON 转义自实现，时间戳用 `SystemTime` 算毫秒。

use std::collections::VecDeque;
use std::fmt::Write as _;
use std::fs::{self, File};
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender};
use std::sync::Mutex;
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

// ---------------------------------------------------------------- 常量

/// 环形缓冲容量（字节）：保留最近 4KB 原始行供崩溃/导出取现场
pub const RING_CAPACITY: usize = 4096;
/// 单文件上限（默认 50MB，到达后滚动覆盖）
pub const DEFAULT_MAX_FILE_BYTES: u64 = 50 * 1024 * 1024;
/// 保留的历史文件个数（`linkx-debug.1.*` … `linkx-debug.{keep}.*`）
pub const KEEP_HISTORY_FILES: usize = 2;
/// 后台落盘通道容量（有界；满则丢弃并计数）
const CHANNEL_CAPACITY: usize = 4096;
/// 当前 NDJSON 文件名（滚动后历史为 `linkx-debug.{i}.ndjson`）
const NDJSON_BASE: &str = "linkx-debug.ndjson";
/// 当前可读文本文件名（滚动后历史为 `linkx-debug.{i}.log`）
const TEXT_BASE: &str = "linkx-debug.log";
/// 导出前等待后台 flush 的最长时间
const FLUSH_TIMEOUT: Duration = Duration::from_secs(2);

// ---------------------------------------------------------------- 全局状态

static ENABLED: AtomicBool = AtomicBool::new(false);
/// 因通道满丢弃的行数（诊断口径）
static DROPPED: AtomicU64 = AtomicU64::new(0);
/// 是否发生过磁盘写失败（只置位一次，不再递归打日志）
static WRITE_FAILED: AtomicBool = AtomicBool::new(false);
/// 后台落盘发送端（None = 未开启）
static TX: Mutex<Option<SyncSender<Msg>>> = Mutex::new(None);
/// 最近一次落盘目录（保留以支持关闭后导出）
static DIR: Mutex<Option<PathBuf>> = Mutex::new(None);
static WORKER: Mutex<Option<JoinHandle<()>>> = Mutex::new(None);
/// 环形现场缓冲（跨开关保留，便于关闭后导出）
static RING: Mutex<Ring> = Mutex::new(Ring::empty());

/// 事件级别
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Info,
    Warn,
    Error,
}

impl Level {
    pub fn as_str(self) -> &'static str {
        match self {
            Level::Info => "info",
            Level::Warn => "warn",
            Level::Error => "error",
        }
    }
}

/// 后台线程消息
enum Msg {
    Line { ndjson: String, text: String },
    Flush(SyncSender<()>),
    Shutdown,
}

// ---------------------------------------------------------------- 环形缓冲

/// 4KB 环形缓冲：保留最近若干「完整行」，绕回时丢弃最旧行
struct Ring {
    lines: VecDeque<String>,
    /// 已占用字节数（含每行换行符）
    bytes: usize,
}

impl Ring {
    const fn empty() -> Self {
        Self {
            lines: VecDeque::new(),
            bytes: 0,
        }
    }

    fn push(&mut self, line: &str) {
        self.lines.push_back(line.to_string());
        self.bytes += line.len() + 1;
        // 绕回：从头丢弃最旧行，直到不超容量（至少保留 1 行）
        while self.bytes > RING_CAPACITY && self.lines.len() > 1 {
            if let Some(front) = self.lines.pop_front() {
                self.bytes -= front.len() + 1;
            }
        }
        // 单行本身超过容量 → 只保留尾部 RING_CAPACITY 字节（按字符边界截断）
        if self.bytes > RING_CAPACITY {
            if let Some(last) = self.lines.pop_back() {
                let kept = truncate_tail(&last, RING_CAPACITY);
                self.bytes = kept.len() + 1;
                self.lines.push_back(kept);
            }
        }
    }

    fn snapshot(&self) -> String {
        self.lines
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn tail(&self, n: usize) -> Vec<String> {
        let skip = self.lines.len().saturating_sub(n);
        self.lines.iter().skip(skip).cloned().collect()
    }

    fn clear(&mut self) {
        self.lines.clear();
        self.bytes = 0;
    }
}

/// 保留字符串尾部至多 `max` 字节（不切断 UTF-8 字符）
fn truncate_tail(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut start = s.len() - max;
    while start < s.len() && !s.is_char_boundary(start) {
        start += 1;
    }
    s[start..].to_string()
}

// ---------------------------------------------------------------- 落盘文件（仅后台线程持有）

struct Files {
    dir: PathBuf,
    ndjson: BufWriter<File>,
    text: BufWriter<File>,
    ndjson_bytes: u64,
    text_bytes: u64,
    max_bytes: u64,
    keep: usize,
}

impl Files {
    /// 以**追加**方式打开当前文件。
    ///
    /// 以前这里"当前文件非空就先滚进历史"，本意是不覆盖上一次会话的现场。但第二实例随时会被拉起
    /// （双击图标、拖文件进来、点通知卡上的按钮都是拉起一个 `linkx.exe <参数>` 的新进程），
    /// 而运行中的那个实例还握着旧文件：改名之后它继续往被改名的 inode 里写，
    /// 于是文档里承诺的 `linkx-debug.ndjson` 变成近乎空的壳，排查的人按路径找不到日志。
    /// 追加既保住了现场，也不会有人把别人的文件挪走；字节数按盘上实际大小起算，上限照旧生效。
    fn open(dir: &Path, max_bytes: u64, keep: usize) -> io::Result<Self> {
        let base = |name: &str| {
            let path = dir.join(name);
            let bytes = fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
            let file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)?;
            io::Result::Ok((BufWriter::new(file), bytes))
        };
        let (ndjson, ndjson_bytes) = base(NDJSON_BASE)?;
        let (text, text_bytes) = base(TEXT_BASE)?;
        Ok(Self {
            dir: dir.to_path_buf(),
            ndjson,
            text,
            ndjson_bytes,
            text_bytes,
            max_bytes,
            keep,
        })
    }

    fn write_line(&mut self, ndjson: &str, text: &str) -> io::Result<()> {
        self.ndjson.write_all(ndjson.as_bytes())?;
        self.ndjson.write_all(b"\n")?;
        self.text.write_all(text.as_bytes())?;
        self.text.write_all(b"\n")?;
        self.ndjson_bytes += ndjson.len() as u64 + 1;
        self.text_bytes += text.len() as u64 + 1;
        if self.ndjson_bytes >= self.max_bytes || self.text_bytes >= self.max_bytes {
            self.rotate()?;
        }
        Ok(())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.ndjson.flush()?;
        self.text.flush()
    }

    /// 滚动覆盖：`base` → `.1` → `.2` …（超出 `keep` 的历史文件删除）
    fn rotate(&mut self) -> io::Result<()> {
        self.flush()?;
        rotate_file(&self.dir, "linkx-debug", "ndjson", self.keep)?;
        rotate_file(&self.dir, "linkx-debug", "log", self.keep)?;
        self.ndjson = BufWriter::new(File::create(self.dir.join(NDJSON_BASE))?);
        self.text = BufWriter::new(File::create(self.dir.join(TEXT_BASE))?);
        self.ndjson_bytes = 0;
        self.text_bytes = 0;
        Ok(())
    }
}

/// 单类文件的滚动：删除最旧历史 → 逐级后移 → 当前文件滚为 `.1`
fn rotate_file(dir: &Path, stem: &str, ext: &str, keep: usize) -> io::Result<()> {
    let keep = keep.max(1);
    let _ = fs::remove_file(dir.join(format!("{stem}.{keep}.{ext}")));
    for i in (1..keep).rev() {
        let from = dir.join(format!("{stem}.{i}.{ext}"));
        if from.exists() {
            let to = dir.join(format!("{stem}.{}.{ext}", i + 1));
            fs::rename(&from, &to)?;
        }
    }
    let base = dir.join(format!("{stem}.{ext}"));
    if base.exists() {
        fs::rename(&base, dir.join(format!("{stem}.1.{ext}")))?;
    }
    Ok(())
}

// ---------------------------------------------------------------- 后台线程

fn writer_loop(rx: Receiver<Msg>, mut files: Files) {
    loop {
        match rx.recv() {
            Ok(Msg::Line { ndjson, text }) => {
                write_one(&mut files, &ndjson, &text);
                // 批量排空当前积压，摊销 flush 开销
                loop {
                    match rx.try_recv() {
                        Ok(Msg::Line { ndjson, text }) => write_one(&mut files, &ndjson, &text),
                        Ok(Msg::Flush(ack)) => {
                            let _ = files.flush();
                            let _ = ack.send(());
                        }
                        Ok(Msg::Shutdown) => {
                            let _ = files.flush();
                            return;
                        }
                        Err(_) => break,
                    }
                }
                let _ = files.flush();
            }
            Ok(Msg::Flush(ack)) => {
                let _ = files.flush();
                let _ = ack.send(());
            }
            Ok(Msg::Shutdown) => {
                let _ = files.flush();
                return;
            }
            // 所有发送端已释放 → 收尾退出
            Err(_) => {
                let _ = files.flush();
                return;
            }
        }
    }
}

/// 写一行；失败只置位失败标志，绝不递归打日志
fn write_one(files: &mut Files, ndjson: &str, text: &str) {
    if files.write_line(ndjson, text).is_err() {
        WRITE_FAILED.store(true, Ordering::Relaxed);
    }
}

// ---------------------------------------------------------------- 开关

/// 开启 Debug 模式（默认上限：50MB/文件，保留 2 个历史文件）。
///
/// `dir` = 平台层提供的落盘目录（不存在会被创建）。重复调用幂等。
pub fn enable(dir: impl AsRef<Path>) -> Result<(), String> {
    enable_with_limits(dir, DEFAULT_MAX_FILE_BYTES, KEEP_HISTORY_FILES)
}

/// 开启 Debug 模式（可配置单文件上限与历史文件个数；供测试/特殊部署）。
pub fn enable_with_limits(
    dir: impl AsRef<Path>,
    max_file_bytes: u64,
    keep_history: usize,
) -> Result<(), String> {
    if ENABLED.load(Ordering::Acquire) {
        return Ok(()); // 幂等
    }
    let dir = dir.as_ref().to_path_buf();
    fs::create_dir_all(&dir).map_err(|e| format!("创建日志目录失败: {e}"))?;
    let max_bytes = max_file_bytes.max(256);
    let files = Files::open(&dir, max_bytes, keep_history.max(1))
        .map_err(|e| format!("打开日志文件失败: {e}"))?;

    let (tx, rx) = sync_channel::<Msg>(CHANNEL_CAPACITY);
    let handle = thread::Builder::new()
        .name("linkx-debuglog".to_string())
        .spawn(move || writer_loop(rx, files))
        .map_err(|e| format!("启动日志线程失败: {e}"))?;

    {
        let Ok(mut g) = TX.lock() else {
            let _ = tx.send(Msg::Shutdown);
            return Err("日志状态锁不可用".into());
        };
        *g = Some(tx);
    }
    if let Ok(mut g) = DIR.lock() {
        *g = Some(dir);
    }
    if let Ok(mut g) = WORKER.lock() {
        *g = Some(handle);
    }
    if let Ok(mut r) = RING.lock() {
        r.clear(); // 新会话重新取现场
    }
    DROPPED.store(0, Ordering::Relaxed);
    WRITE_FAILED.store(false, Ordering::Relaxed);
    ENABLED.store(true, Ordering::Release);
    Ok(())
}

/// 关闭 Debug 模式：停止接收、冲刷并回收后台线程（幂等）。关闭后零写盘。
pub fn disable() {
    if !ENABLED.swap(false, Ordering::AcqRel) {
        return;
    }
    let tx = match TX.lock() {
        Ok(mut g) => g.take(),
        Err(_) => None,
    };
    if let Some(tx) = tx {
        let _ = tx.send(Msg::Shutdown);
    }
    let handle = match WORKER.lock() {
        Ok(mut g) => g.take(),
        Err(_) => None,
    };
    if let Some(h) = handle {
        let _ = h.join();
    }
}

/// 是否处于开启状态
pub fn is_enabled() -> bool {
    ENABLED.load(Ordering::Acquire)
}

/// 阻塞至后台把已入队日志冲刷落盘（超时返回 false）；关闭状态下返回 false。
pub fn flush() -> bool {
    let tx = match TX.lock() {
        Ok(g) => g.clone(),
        Err(_) => None,
    };
    let Some(tx) = tx else {
        return false;
    };
    let (ack_tx, ack_rx) = sync_channel::<()>(0);
    if tx.send(Msg::Flush(ack_tx)).is_err() {
        return false;
    }
    ack_rx.recv_timeout(FLUSH_TIMEOUT).is_ok()
}

// ---------------------------------------------------------------- 埋点入口

/// 记录一条事件的宏包装：**先判开关再求值字段表达式**，保证关闭状态下
/// 连 `to_string()` 等参数构造都不会发生（真正的「一次原子读 + 立即返回」）。
///
/// 用法与 [`log`] 一致：`debuglog::log!(Level::Info, "session", "state.transition", &[("to", &s)])`。
#[macro_export]
macro_rules! log {
    ($level:expr, $module:expr, $event:expr, $fields:expr) => {{
        if $crate::is_enabled() {
            $crate::log($level, $module, $event, $fields);
        }
    }};
}

/// 记录一条事件（核心埋点入口）。
///
/// 关闭状态下仅一次原子读即返回（零写盘、几乎零开销）。字段值会做 JSON 转义；
/// 落盘为异步（有界通道，满则丢弃并计数），**不阻塞调用方**。
/// 若字段值需要临时构造（如 `to_string()`），请改用 [`log!`] 宏以免在关闭状态下仍求值。
pub fn log(level: Level, module: &str, event: &str, fields: &[(&str, &str)]) {
    if !ENABLED.load(Ordering::Relaxed) {
        return;
    }
    let tx = match TX.lock() {
        Ok(g) => g.clone(),
        Err(_) => return,
    };
    let Some(tx) = tx else {
        return;
    };
    let ts = now_ms();
    let ndjson = build_ndjson(ts, level, module, event, fields);
    let text = build_text(ts, level, module, event, fields);
    if let Ok(mut r) = RING.lock() {
        r.push(&ndjson);
    }
    if tx.try_send(Msg::Line { ndjson, text }).is_err() {
        DROPPED.fetch_add(1, Ordering::Relaxed);
    }
}

/// 因通道满被丢弃的日志行数（诊断口径）
pub fn dropped_count() -> u64 {
    DROPPED.load(Ordering::Relaxed)
}

/// 是否发生过磁盘写失败（只置位一次）
pub fn write_failed() -> bool {
    WRITE_FAILED.load(Ordering::Relaxed)
}

/// 最近一次落盘目录（未开启过为 None）
pub fn log_dir() -> Option<PathBuf> {
    match DIR.lock() {
        Ok(g) => g.clone(),
        Err(_) => None,
    }
}

// ---------------------------------------------------------------- 环形现场

/// 环形缓冲全量快照（最近 4KB 原始行）
pub fn ring_snapshot() -> String {
    match RING.lock() {
        Ok(r) => r.snapshot(),
        Err(_) => String::new(),
    }
}

/// 环形缓冲最近 `n` 行
pub fn ring_tail(n: usize) -> Vec<String> {
    match RING.lock() {
        Ok(r) => r.tail(n),
        Err(_) => Vec::new(),
    }
}

// ---------------------------------------------------------------- 导出

/// 导出当前日志 + 环形现场快照到 `dir`（文件名含时间戳），**不删除原日志**。
///
/// 成功返回导出目录路径（即 `dir`）。关闭状态下仍可导出（依赖已保留的目录与环形现场）。
pub fn export_to(dir: &Path) -> Result<PathBuf, String> {
    let src_dir = match DIR.lock() {
        Ok(g) => g.clone(),
        Err(_) => None,
    }
    .ok_or_else(|| "Debug 日志从未开启，无可导出内容".to_string())?;

    // 已开启则先冲刷，保证磁盘上是完整现场
    if ENABLED.load(Ordering::Relaxed) {
        let _ = flush();
    }

    fs::create_dir_all(dir).map_err(|e| format!("创建导出目录失败: {e}"))?;
    let ts = now_ms();
    let mut exported = 0usize;

    for (base, ext) in [(NDJSON_BASE, "ndjson"), (TEXT_BASE, "log")] {
        let src = src_dir.join(base);
        if src.is_file() {
            let dst = dir.join(format!("linkx-debug-export-{ts}.{ext}"));
            fs::copy(&src, &dst).map_err(|e| format!("复制 {base} 失败: {e}"))?;
            exported += 1;
        }
    }

    // 环形现场（崩溃时最后 4KB）
    let snap = match RING.lock() {
        Ok(r) => r.snapshot(),
        Err(_) => String::new(),
    };
    if !snap.is_empty() {
        let dst = dir.join(format!("linkx-debug-ring-{ts}.log"));
        fs::write(&dst, snap.as_bytes()).map_err(|e| format!("写出环形快照失败: {e}"))?;
        exported += 1;
    }

    if exported == 0 {
        return Err("暂无可导出的日志文件".into());
    }
    Ok(dir.to_path_buf())
}

// ---------------------------------------------------------------- 格式化

/// 当前时间戳（Unix 毫秒；系统时钟异常时降级为 0，绝不 panic）
fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// 构造 NDJSON 行：`{"ts_ms":..,"level":..,"module":..,"event":..,"fields":{..}}`
fn build_ndjson(
    ts: i64,
    level: Level,
    module: &str,
    event: &str,
    fields: &[(&str, &str)],
) -> String {
    let mut s = String::with_capacity(128);
    let _ = write!(
        s,
        "{{\"ts_ms\":{ts},\"level\":\"{}\",\"module\":\"",
        level.as_str()
    );
    escape_json(module, &mut s);
    s.push_str("\",\"event\":\"");
    escape_json(event, &mut s);
    s.push_str("\",\"fields\":{");
    for (i, (k, v)) in fields.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        s.push('"');
        escape_json(k, &mut s);
        s.push_str("\":\"");
        escape_json(v, &mut s);
        s.push('"');
    }
    s.push_str("}}");
    s
}

/// 构造纯文本可读行：`[<ts_ms>] LEVEL module event k=v ...`
fn build_text(ts: i64, level: Level, module: &str, event: &str, fields: &[(&str, &str)]) -> String {
    let mut s = String::with_capacity(96);
    let _ = write!(
        s,
        "[{ts}] {} {module} {event}",
        level.as_str().to_ascii_uppercase()
    );
    for (k, v) in fields {
        s.push(' ');
        s.push_str(k);
        s.push('=');
        s.push_str(&sanitize_text(v));
    }
    s
}

/// 文本版字段值：控制字符（含换行）替换为空格，保持单行可读
fn sanitize_text(v: &str) -> String {
    v.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

/// JSON 字符串转义（自实现，不依赖 serde；不 panic）
fn escape_json(src: &str, out: &mut String) {
    for c in src.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0C}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
}

// ---------------------------------------------------------------- 测试

#[cfg(test)]
mod tests {
    use super::*;

    /// 触碰全局状态的用例共享一把锁（避免并行用例互相干扰）
    static TEST_LOCK: Mutex<()> = Mutex::new(());
    static SEQ: AtomicU64 = AtomicU64::new(0);

    fn temp_dir(tag: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        p.push(format!("linkx-debuglog-{tag}-{}-{n}", std::process::id()));
        let _ = fs::remove_dir_all(&p);
        p
    }

    fn guard() -> std::sync::MutexGuard<'static, ()> {
        TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    fn ring_wraps_and_keeps_recent_lines() {
        let mut r = Ring::empty();
        for i in 0..100 {
            r.push(&format!("line-{i:03}-{}", "x".repeat(90)));
        }
        assert!(
            r.snapshot().len() <= RING_CAPACITY,
            "环形必须绕回且不超 4KB"
        );
        let tail = r.tail(5);
        assert_eq!(tail.len(), 5);
        assert!(tail[4].starts_with("line-099"), "尾部应为最新行");
        // 单行超长 → 截尾且保持 UTF-8 合法
        let mut r2 = Ring::empty();
        r2.push(&"中".repeat(RING_CAPACITY));
        let snap = r2.snapshot();
        assert!(snap.len() <= RING_CAPACITY);
        assert!(std::str::from_utf8(snap.as_bytes()).is_ok());
        // 空缓冲快照为空
        assert_eq!(Ring::empty().snapshot(), "");
        assert!(Ring::empty().tail(3).is_empty());
    }

    #[test]
    fn ndjson_line_format_and_escaping() {
        let line = build_ndjson(
            1234,
            Level::Info,
            "session",
            "state.transition",
            &[("from", "PAIRING"), ("to", "SAS_COMPARE")],
        );
        assert_eq!(
            line,
            r#"{"ts_ms":1234,"level":"info","module":"session","event":"state.transition","fields":{"from":"PAIRING","to":"SAS_COMPARE"}}"#
        );
        // 引号 / 反斜杠 / 换行 / 控制字符转义
        let esc = build_ndjson(
            1,
            Level::Warn,
            "m\"od",
            "e\\v",
            &[("k\"y", "a\nb"), ("c", "\u{1}")],
        );
        assert!(esc.contains("\"level\":\"warn\""));
        assert!(esc.contains("\"module\":\"m\\\"od\""));
        assert!(esc.contains("\"event\":\"e\\\\v\""));
        assert!(esc.contains("\"k\\\"y\":\"a\\nb\""));
        assert!(esc.contains("\"c\":\"\\u0001\""));
        // 无字段
        assert_eq!(
            build_ndjson(0, Level::Error, "ffi", "e", &[]),
            r#"{"ts_ms":0,"level":"error","module":"ffi","event":"e","fields":{}}"#
        );
        // 文本版为单行（换行被替换）
        let text = build_text(5, Level::Error, "session", "err", &[("ctx", "a\nb")]);
        assert_eq!(text, "[5] ERROR session err ctx=a b");
    }

    #[test]
    fn a_second_start_appends_instead_of_moving_the_live_file_away() {
        let _g = guard();
        disable();
        let dir = temp_dir("append");
        let base = dir.join(NDJSON_BASE);

        enable_with_limits(&dir, 4096, KEEP_HISTORY_FILES).unwrap();
        log(Level::Info, "test", "first.start", &[]);
        assert!(flush());
        disable();
        let after_first = fs::read_to_string(&base).unwrap();
        assert!(
            after_first.contains("first.start"),
            "第一次开的现场要还在当前文件里"
        );

        // 第二实例拉起（双击图标 / 点通知卡按钮都是）：不许把运行中实例的文件改名挪走
        enable_with_limits(&dir, 4096, KEEP_HISTORY_FILES).unwrap();
        log(Level::Info, "test", "second.start", &[]);
        assert!(flush());
        disable();

        let after_second = fs::read_to_string(&base).unwrap();
        assert!(
            after_second.contains("first.start") && after_second.contains("second.start"),
            "两次现场都该留在同一个当前文件里：{after_second}"
        );
        assert!(
            !dir.join("linkx-debug.1.ndjson").exists(),
            "重启不该产生历史文件——运行中的实例还握着被改名的那个 inode"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn rotation_caps_total_occupancy() {
        let _g = guard();
        disable();
        let dir = temp_dir("rotate");
        enable_with_limits(&dir, 512, KEEP_HISTORY_FILES).unwrap();
        for i in 0..300u32 {
            log(Level::Info, "test", "rotate", &[("i", &i.to_string())]);
        }
        assert!(flush(), "flush 应成功");
        // export 前不删原日志：先检查滚动产物
        let ndjson_files: Vec<PathBuf> = fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.to_string_lossy().ends_with(".ndjson"))
            .collect();
        assert!(
            ndjson_files.len() <= 1 + KEEP_HISTORY_FILES,
            "当前 + 历史不应超过 {}，实际 {}",
            1 + KEEP_HISTORY_FILES,
            ndjson_files.len()
        );
        assert!(ndjson_files.len() >= 2, "应已发生滚动");
        assert!(dir.join("linkx-debug.1.ndjson").exists());
        for p in &ndjson_files {
            let len = fs::metadata(p).unwrap().len();
            assert!(len <= 512 + 256, "{} 超限：{len}B", p.display());
        }
        assert!(!write_failed(), "常规滚动不应触发写失败");
        disable();
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn export_produces_files_and_keeps_originals() {
        let _g = guard();
        disable();
        let src = temp_dir("export-src");
        let out = temp_dir("export-dst");
        enable(&src).unwrap();
        log(Level::Info, "test", "hello", &[("a", "b")]);
        log(Level::Error, "test", "boom", &[("code", "-1")]);
        let exported = export_to(&out).unwrap();
        assert_eq!(exported, out);
        let names: Vec<String> = fs::read_dir(&out)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert!(names
            .iter()
            .any(|n| n.starts_with("linkx-debug-export-") && n.ends_with(".ndjson")));
        assert!(names
            .iter()
            .any(|n| n.starts_with("linkx-debug-export-") && n.ends_with(".log")));
        assert!(names.iter().any(|n| n.starts_with("linkx-debug-ring-")));
        let ndjson = names
            .iter()
            .find(|n| n.ends_with(".ndjson"))
            .expect("应有导出 NDJSON");
        let content = fs::read_to_string(out.join(ndjson)).unwrap();
        assert!(content.contains("\"event\":\"hello\""));
        assert!(content.contains("\"event\":\"boom\""));
        // 原日志不删除
        assert!(src.join(NDJSON_BASE).exists());
        // 关闭后仍可导出（环形现场保留）
        disable();
        let out2 = temp_dir("export-dst2");
        assert!(export_to(&out2).is_ok(), "关闭后应仍可导出");
        let _ = fs::remove_dir_all(&src);
        let _ = fs::remove_dir_all(&out);
        let _ = fs::remove_dir_all(&out2);
    }

    #[test]
    fn disabled_state_writes_nothing() {
        let _g = guard();
        disable();
        // 清掉上个用例残留的目录记忆，验证「从未开启」语义
        if let Ok(mut d) = DIR.lock() {
            *d = None;
        }
        let dir = temp_dir("disabled");
        // 未开启直接埋点：不创建目录、不落盘
        log(Level::Error, "test", "should-not-write", &[("x", "1")]);
        assert!(!dir.exists(), "关闭状态不得创建日志目录");
        assert!(!is_enabled());
        assert!(export_to(&dir).is_err(), "无历史目录时导出应报错");
        assert!(!dir.exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn enable_is_idempotent_and_disable_safe() {
        let _g = guard();
        disable();
        let dir = temp_dir("idem");
        assert!(enable(&dir).is_ok());
        assert!(enable(&dir).is_ok(), "重复开启应幂等");
        assert!(is_enabled());
        log(Level::Info, "test", "e", &[("k", "v")]);
        assert!(flush());
        disable();
        assert!(!is_enabled());
        disable(); // 二次关闭不 panic
        assert!(log_dir().is_some());
        let _ = fs::remove_dir_all(&dir);
    }
}
