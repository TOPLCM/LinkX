//! 本机回环调试控制面：把「双端此刻什么状态、日志里发生了什么、能否远程触发一个动作」
//! 以 JSON/NDJSON 暴露给本机调试者与自动化脚本，替代「截图 + 猜像素 + 点 GUI」。
//!
//! 三条刻意的设计约束：
//! 1. **只绑 127.0.0.1**。安卓侧靠 `adb forward` 透出到电脑，绝不为方便而监听 `0.0.0.0`——
//!    debuglog **不脱敏**（含配对码与设备名），上网络即泄露面。
//! 2. **门禁在宿主侧的 optional dependency**：交付构建不启用 `agent-debug`，本 crate 连依赖都
//!    不参与编译，产物里不留符号与端口。
//! 3. **全 profile `panic = "abort"`**：服务端线程里任何 panic 都会杀掉整个进程，因此本文件
//!    **不得出现 unwrap/expect/裸索引/切片**，网络输入一律走 `Result` 或 `get(..)`。

use serde_json::{Map, Value};
use std::collections::BTreeMap;
use std::io::{ErrorKind, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

/// 默认端口。刻意选在 LinkX 业务端口（55676）与 LocalSend（53100/53317）之外，
/// 且允许传 0 让系统分配（多实例并行调试时用）。
pub const DEFAULT_PORT: u16 = 55699;

/// `bind` 失败时的重试次数与间隔。**只为 `AddrInUse` 重试**：Windows 端自重启（运行期功能
/// 开关）会"旧进程还没退干净、新进程已经起来"，一次失败就放弃的话新实例整个生命周期都没有
/// 控制面，而 GUI 子系统里那句 `eprintln!` 谁也看不见。
const BIND_RETRIES: u32 = 8;
const BIND_RETRY_MS: u64 = 250;

/// 单次响应最多回传的环形日志行数上限（防被 tail=999999 打爆内存）。
const MAX_TAIL: usize = 4096;
/// 读请求头的超时：客户端连上不发数据不至于永久占住线程。
const IO_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_HEAD_BYTES: usize = 8 * 1024;

/// 宿主注册的调试开关回调（抽出别名以满足 clippy `type_complexity`）。
type ToggleFn = Box<dyn Fn(bool) -> bool + Send + Sync>;

/// 宿主注册的动作回调：`(动作名, 原始 query 串) -> 结果文本`。
type ActionFn = Box<dyn Fn(&str, &str) -> Result<String, String> + Send + Sync>;

static STATE: OnceLock<Mutex<Value>> = OnceLock::new();
static COUNTERS: OnceLock<Mutex<BTreeMap<String, u64>>> = OnceLock::new();
static TOGGLE: OnceLock<Mutex<Option<ToggleFn>>> = OnceLock::new();
static ACTION: OnceLock<Mutex<Option<ActionFn>>> = OnceLock::new();
static SERVED: AtomicU64 = AtomicU64::new(0);
/// 当前在途连接数（见 `accept_loop` 的上限说明）
static ACTIVE: AtomicU64 = AtomicU64::new(0);
const MAX_ACTIVE_CONNS: u64 = 8;
static BIND_ADDR: OnceLock<Mutex<Option<SocketAddr>>> = OnceLock::new();

fn state_slot() -> &'static Mutex<Value> {
    STATE.get_or_init(|| Mutex::new(Value::Object(Map::new())))
}

fn counters_slot() -> &'static Mutex<BTreeMap<String, u64>> {
    COUNTERS.get_or_init(|| Mutex::new(BTreeMap::new()))
}

fn toggle_slot() -> &'static Mutex<Option<ToggleFn>> {
    TOGGLE.get_or_init(|| Mutex::new(None))
}

fn action_slot() -> &'static Mutex<Option<ActionFn>> {
    ACTION.get_or_init(|| Mutex::new(None))
}

/// 注册动作回调：`POST /action/<名字>?k=v&k2=v2` 会调用它。
///
/// 刻意让**宿主**决定动作语义（直接设它自己那套 UI→worker 命令字段），本 crate 只做路由与
/// 传参：控制面走的就是鼠标点击的同一条生产路径，而不是另起一套调试专用逻辑——否则又会出现
/// 「调试面正常、生产面坏掉」。
pub fn on_action<F>(f: F)
where
    F: Fn(&str, &str) -> Result<String, String> + Send + Sync + 'static,
{
    *lock_or_recover(action_slot()) = Some(Box::new(f));
}

/// 极简 query 解析：取 `k=v`，值按 `+`→空格、`%XX` 百分号解码；不引 percent-encoding crate。
///
/// **解码必须先攒字节、最后整体按 UTF-8 解释**：逐 `%XX` 直接 `push(n as char)` 会把 `中`
/// （`%E4%B8%AD`）变成 `ä¸­`，而调用方是剪贴板里那段用户文本，它不会二次解码——非 ASCII
/// 就在控制面入口永久损坏了。
pub fn query_param(query: &str, key: &str) -> Option<String> {
    for pair in query.split('&') {
        let (k, v) = match pair.split_once('=') {
            Some(kv) => kv,
            None => continue,
        };
        if k != key {
            continue;
        }
        return Some(decode_percent(v));
    }
    None
}

fn decode_percent(v: &str) -> String {
    let src = v.as_bytes();
    let mut bytes = Vec::with_capacity(src.len());
    let mut i = 0usize;
    while i < src.len() {
        match src[i] {
            b'+' => {
                bytes.push(b' ');
                i += 1;
            }
            // 要求 `i + 2 < len`：最后一个 `%` 若只有 1 位尾巴就不是合法转义，落到下面按字面量处理。
            b'%' if i + 2 < src.len() => {
                match (hex_byte(&src[i + 1]), hex_byte(&src[i + 2])) {
                    (Some(hi), Some(lo)) => {
                        bytes.push(hi << 4 | lo);
                        i += 3;
                    }
                    // 非法转义（`%zz`、`%2`）按字面量保留，绝不因此丢掉整个参数。
                    _ => {
                        bytes.push(b'%');
                        i += 1;
                    }
                }
            }
            c => {
                bytes.push(c);
                i += 1;
            }
        }
    }
    // 解码后的字节流可能不是合法 UTF-8（例如只送来半个多字节序列）。这里 lossy 而不是报错：
    // 控制面是观测/驱动入口，宁可回一个可见的替换字符，也不要"动作整体失败"。
    String::from_utf8(bytes).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned())
}

fn hex_byte(b: &u8) -> Option<u8> {
    match *b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// 锁中毒时不 panic（`panic = "abort"` 下 panic 即进程终止），直接取回内部值。
fn lock_or_recover<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// 宿主每轮（或状态变化时）发布一份当前状态快照；`/state` 会把它与 `counters`、`logs` 元信息合并后返回。
pub fn publish(snapshot: Value) {
    *lock_or_recover(state_slot()) = snapshot;
}

/// 读回宿主发布的快照（不含 counters）。
pub fn snapshot() -> Value {
    lock_or_recover(state_slot()).clone()
}

/// 计数器自增：用于「BLE 写成功/失败、分片丢弃、重组超时」这类**只有计数才能定案**的问题。
pub fn bump(name: &str, delta: u64) {
    let mut c = lock_or_recover(counters_slot());
    let slot = c.entry(name.to_string()).or_insert(0);
    *slot = slot.saturating_add(delta);
}

/// 计数器置值（宿主已有精确数值时用）。
pub fn set_counter(name: &str, value: u64) {
    lock_or_recover(counters_slot()).insert(name.to_string(), value);
}

/// 水位计数：只在 `value` 更大时覆盖。用于「主循环单轮最长耗时」这类**平均值会掩盖尖峰**的判据。
pub fn bump_max(name: &str, value: u64) {
    let mut c = lock_or_recover(counters_slot());
    let slot = c.entry(name.to_string()).or_insert(0);
    if value > *slot {
        *slot = value;
    }
}

/// 注册调试开关回调：`POST /debug?on=0|1` 会调用它，返回值作为「是否已生效」。
/// 刻意不让本 crate 自己调 `debuglog::enable/disable`——日志目录是平台相关决策，归宿主管。
pub fn on_debug_toggle<F>(f: F)
where
    F: Fn(bool) -> bool + Send + Sync + 'static,
{
    *lock_or_recover(toggle_slot()) = Some(Box::new(f));
}

/// 启动控制面。`port` 传 0 由系统分配；返回实际绑定地址。失败（端口占用等）**不得**让宿主崩掉，调用方只记一条日志继续跑。
pub fn start(port: u16) -> std::io::Result<SocketAddr> {
    let addr: SocketAddr = ([127, 0, 0, 1], port).into();
    let mut listener = None;
    let mut last_err = None;
    for attempt in 0..=BIND_RETRIES {
        match TcpListener::bind(addr) {
            Ok(l) => {
                listener = Some(l);
                break;
            }
            Err(e) => {
                // 只对"端口还被占着"重试；其它错误（权限等）重试也不会有结果
                if e.kind() != std::io::ErrorKind::AddrInUse || attempt == BIND_RETRIES {
                    last_err = Some(e);
                    break;
                }
                std::thread::sleep(Duration::from_millis(BIND_RETRY_MS));
                last_err = Some(e);
            }
        }
    }
    let listener = listener.ok_or_else(|| {
        last_err
            .unwrap_or_else(|| std::io::Error::new(std::io::ErrorKind::AddrInUse, "回环端口被占用"))
    })?;
    let bound = listener.local_addr()?;
    set_bound_addr(bound);
    std::thread::Builder::new()
        .name("linkx-debugd".to_string())
        .spawn(move || accept_loop(listener))?;
    Ok(bound)
}

fn set_bound_addr(a: SocketAddr) {
    let slot = BIND_ADDR.get_or_init(|| Mutex::new(None));
    *lock_or_recover(slot) = Some(a);
}

fn bound_addr() -> Option<SocketAddr> {
    let slot = BIND_ADDR.get_or_init(|| Mutex::new(None));
    *lock_or_recover(slot)
}

fn accept_loop(listener: TcpListener) {
    // incoming() 对瞬时错误会一直抛，这里显式循环并跳过坏连接：
    // 一个畸形连接不能拖垮控制面。
    for conn in listener.incoming() {
        match conn {
            Ok(stream) => {
                // 必须设上限：`handle` 里有最长 5 s 的读超时 + 线程栈开销，一个失控的轮询
                // 脚本就能把线程数堆到几百，届时调试工具本身变成宿主的资源故障源。
                let n = ACTIVE.fetch_add(1, Ordering::Relaxed) + 1;
                if n > MAX_ACTIVE_CONNS {
                    ACTIVE.fetch_sub(1, Ordering::Relaxed);
                    bump("conn_rejected_busy", 1);
                    drop(stream); // 立刻关闭，让调用方看到连接被拒而不是挂住
                    continue;
                }
                if let Err(e) = std::thread::Builder::new()
                    .name("linkx-debugd-conn".to_string())
                    .spawn(move || {
                        handle(stream);
                        ACTIVE.fetch_sub(1, Ordering::Relaxed);
                    })
                {
                    ACTIVE.fetch_sub(1, Ordering::Relaxed);
                    bump("conn_spawn_failed", 1);
                    let _ = e;
                }
            }
            Err(ref e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(_) => continue,
        }
    }
}

fn handle(mut stream: TcpStream) {
    let _ = stream.set_read_timeout(Some(IO_TIMEOUT));
    let _ = stream.set_write_timeout(Some(IO_TIMEOUT));

    let mut buf = vec![0u8; MAX_HEAD_BYTES];
    let mut filled = 0usize;
    loop {
        match stream.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(n) => {
                filled += n;
                if buf[..filled].windows(4).any(|w| w == b"\r\n\r\n") || filled >= buf.len() {
                    break;
                }
            }
            Err(_) => break,
        }
    }
    if filled == 0 {
        return;
    }
    let head = String::from_utf8_lossy(&buf[..filled]).into_owned();
    let (status, content_type, body) = route(&head);
    SERVED.fetch_add(1, Ordering::Relaxed);

    let resp = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(resp.as_bytes());
    let _ = stream.flush();
}

/// 只服务本机脚本，不接受浏览器发起的请求："只绑 127.0.0.1" 挡不住 DNS 重绑——恶意网页把
/// 域名解析到 127.0.0.1 后请求看起来就是本机发的：`GET /state` 能读走 SAS 与双端指纹，而
/// `POST /action/confirm-sas` 是 text/plain 简单请求（不触发 CORS 预检），等于**替用户点掉
/// 那道防中间人确认**。三道判据：Host 必须是回环主机名、带 `Origin`/`Referer` 的一律拒、
/// `Sec-Fetch-Site` 非 `none` 的拒；端口不参与判定（安卓经 `adb forward` 进来时 Host 是本机端口）。
fn from_browser(head: &str) -> bool {
    let mut host: Option<String> = None;
    for line in head.lines().skip(1) {
        let Some((k, v)) = line.split_once(':') else {
            continue;
        };
        let v = v.trim();
        match k.trim().to_ascii_lowercase().as_str() {
            "host" => host = Some(v.to_ascii_lowercase()),
            "origin" | "referer" => return true,
            // 只有地址栏直连才是 `none`；其余（cross-site / same-origin / same-site）都是页面发起的
            "sec-fetch-site" if !v.eq_ignore_ascii_case("none") => return true,
            _ => {}
        }
    }
    // 没有 Host 就不是合法的 HTTP/1.1 请求，留给后面的派发按 400/404 处理
    let Some(h) = host else { return false };
    let bare = match h.split_once(']') {
        // [::1]:55699 这种带方括号的 IPv6 字面量
        Some((pre, _)) => pre.trim_start_matches('[').to_string(),
        None => h.rsplit_once(':').map_or(h.clone(), |(n, _)| n.to_string()),
    };
    !is_loopback_host(&bare)
}

/// 回环主机名判定：**只认字面量，不做任何 DNS 解析**（保持同步、无外联）。
///
/// 不能写成 `starts_with("127.")`：`127.0.0.1.nip.io` 这类公共解析服务会把这种名字
/// 真的解析到 127.0.0.1，前缀判定等于把 DNS 重绑的第一道闸形同虚设。
fn is_loopback_host(name: &str) -> bool {
    if name == "localhost" || name == "::1" {
        return true;
    }
    // 127.0.0.0/8 的 IPv4 字面量：恰好四段、每段都是 0-255、首段为 127
    let mut parts = name.split('.');
    let (Some(a), Some(b), Some(c), Some(d), None) = (
        parts.next(),
        parts.next(),
        parts.next(),
        parts.next(),
        parts.next(),
    ) else {
        return false;
    };
    a == "127" && b.parse::<u8>().is_ok() && c.parse::<u8>().is_ok() && d.parse::<u8>().is_ok()
}

/// 解析请求行并派发。**输入完全不可信**，因此只用 split/get，不做任何 unwrap。
fn route(head: &str) -> (u16, &'static str, String) {
    if from_browser(head) {
        return (
            403,
            "text/plain; charset=utf-8",
            "此控制面只服务本机脚本，不接受浏览器发起的请求\n".to_string(),
        );
    }
    let Some(line) = head.lines().next() else {
        return (
            400,
            "text/plain; charset=utf-8",
            "bad request\n".to_string(),
        );
    };
    let mut it = line.split_whitespace();
    let method = it.next().unwrap_or("");
    let target = it.next().unwrap_or("");
    let (path, query) = match target.split_once('?') {
        Some((p, q)) => (p, q),
        None => (target, ""),
    };

    match (method, path) {
        ("GET", "/") => (200, "text/plain; charset=utf-8", INDEX.to_string()),
        ("GET", "/health") => (
            200,
            "application/json",
            format!(
                "{{\"ok\":true,\"served\":{}}}",
                SERVED.load(Ordering::Relaxed)
            ),
        ),
        ("GET", "/state") => (200, "application/json", state_json()),
        ("GET", "/logs") => (200, "application/x-ndjson", logs_json(query)),
        ("POST", "/debug") => (200, "application/json", toggle_json(query)),
        ("GET", "/counters") => (
            200,
            "application/json",
            serde_json::to_string(&counters_value()).unwrap_or_else(|_| "{}".to_string()),
        ),
        _ => {
            // POST /action/<名字>：动作语义完全交给宿主实现（见 on_action 注释）。
            if method == "POST" {
                if let Some(name) = path.strip_prefix("/action/") {
                    if !name.is_empty() && !name.contains('/') {
                        return action_json(name, query);
                    }
                }
            }
            (404, "text/plain; charset=utf-8", "not found\n".to_string())
        }
    }
}

/// 派发一个动作。宿主未注册处理器时明确回 400，而不是静默成功。
fn action_json(name: &str, query: &str) -> (u16, &'static str, String) {
    let result = {
        let guard = lock_or_recover(action_slot());
        match guard.as_ref() {
            Some(f) => f(name, query),
            None => Err("宿主未注册动作处理器（本构建无 /action 能力）".to_string()),
        }
    };
    // 失败的原因必须原样回给调用方：只转义 `Ok` 分支会让 `Err` 字符串被整段丢掉，脚本侧每次失败都长成 `ok:false,"result":""`——"为什么没成"这条线索在调试面上就断了。
    let payload = match &result {
        Ok(text) => text.clone(),
        Err(why) => why.clone(),
    };
    let text = json_escape(&payload);
    // `name` 取自请求行（`/action/<名字>`），和 result 一样属于不可信输入：不转义的话，
    // 一个带引号的动作名就能改写这段 JSON 的结构。
    let name = json_escape(name);
    (
        if result.is_ok() { 200 } else { 400 },
        "application/json",
        format!(
            "{{\"action\":\"{name}\",\"ok\":{},\"result\":\"{text}\"}}",
            result.is_ok()
        ),
    )
}

/// 转成 JSON 字符串字面量的内容：控制字符直接丢掉，其余按 JSON 的转义表来。
fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {}
            c => out.push(c),
        }
    }
    out
}

fn counters_value() -> Value {
    let c = lock_or_recover(counters_slot());
    let mut m = Map::new();
    for (k, v) in c.iter() {
        m.insert(k.clone(), Value::from(*v));
    }
    Value::Object(m)
}

fn state_json() -> String {
    let mut root = match snapshot() {
        Value::Object(m) => m,
        _ => Map::new(),
    };
    root.insert("counters".to_string(), counters_value());
    root.insert(
        "debug_enabled".to_string(),
        Value::from(debuglog::is_enabled()),
    );
    root.insert(
        "log_dropped".to_string(),
        Value::from(debuglog::dropped_count()),
    );
    root.insert(
        "log_write_failed".to_string(),
        Value::from(debuglog::write_failed()),
    );
    if let Some(p) = debuglog::log_dir() {
        root.insert("log_dir".to_string(), Value::from(p.display().to_string()));
    }
    root.insert(
        "bind".to_string(),
        Value::from(match bound_addr() {
            Some(a) => a.to_string(),
            None => String::new(),
        }),
    );
    serde_json::to_string_pretty(&Value::Object(root)).unwrap_or_else(|_| "{}".to_string())
}

fn logs_json(query: &str) -> String {
    let mut n: usize = 200;
    for pair in query.split('&') {
        if let Some(v) = pair.strip_prefix("tail=") {
            if let Ok(parsed) = v.parse::<usize>() {
                n = parsed.clamp(1, MAX_TAIL);
            }
        }
    }
    debuglog::flush();
    debuglog::ring_tail(n).join("\n")
}

fn toggle_json(query: &str) -> String {
    let mut want = true;
    for pair in query.split('&') {
        if let Some(v) = pair.strip_prefix("on=") {
            want = v != "0";
        }
    }
    let applied = {
        let slot = toggle_slot();
        let guard = lock_or_recover(slot);
        match guard.as_ref() {
            Some(f) => f(want),
            None => false,
        }
    };
    format!(
        "{{\"requested\":{},\"applied\":{},\"enabled\":{}}}",
        want,
        applied,
        debuglog::is_enabled()
    )
}

const INDEX: &str = "\
LinkX debug control plane (loopback only)

  GET  /state                 宿主状态快照 + counters + 日志器健康位
  GET  /logs?tail=N           环形缓冲 NDJSON（默认 200 行，上限 4096）
  POST /debug?on=0|1          开关全栈调试日志（回调由宿主注册）
  POST /action/<名字>?k=v     触发宿主动作（语义由宿主定义，走生产命令路径）
  GET  /counters              仅计数器
  GET  /health                存活探针

注意：本接口无鉴权，仅因只绑 127.0.0.1 才可接受。交付构建必须不启用 agent-debug。
";

#[cfg(test)]
mod tests {
    use super::*;

    fn request(method: &str, path: &str) -> String {
        // 走真实 TCP，确保「无 panic 路径」这条在测试里也成立。
        let addr = start(0).expect("bind loopback");
        let mut s = TcpStream::connect(addr).expect("connect");
        let req = format!("{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n");
        s.write_all(req.as_bytes()).expect("write");
        let mut out = Vec::new();
        s.read_to_end(&mut out).expect("read");
        String::from_utf8_lossy(&out).into_owned()
    }

    fn get(path: &str) -> String {
        request("GET", path)
    }

    fn post(path: &str) -> String {
        request("POST", path)
    }

    #[test]
    fn state_merges_host_snapshot_and_counters() {
        publish(serde_json::json!({"phase": "Paired", "idle_ms": 1234}));
        bump("ble_write_ok", 3);
        let body = get("/state");
        assert!(body.contains("\"phase\": \"Paired\""), "{body}");
        assert!(body.contains("ble_write_ok"), "{body}");
        assert!(body.contains("debug_enabled"), "{body}");
    }

    #[test]
    fn malformed_request_never_panics() {
        // 半截请求行、垃圾字节、超长路径：都必须回包而不是 panic（panic=abort 即进程死）。
        let addr = start(0).expect("bind");
        for junk in [
            "\r\n\r\n",
            "G",
            "GET /\u{1F600}\u{0000} HTTP/1.1\r\n\r\n",
            "POST /x?y=",
        ] {
            if let Ok(mut s) = TcpStream::connect(addr) {
                let _ = s.write_all(junk.as_bytes());
                let mut buf = [0u8; 64];
                let _ = s.read(&mut buf);
            }
        }
        assert!(get("/health").contains("\"ok\":true"));
    }

    #[test]
    fn tail_param_is_clamped_not_fatal() {
        assert!(get("/logs?tail=999999").starts_with("HTTP/1.1 200"));
        assert!(get("/logs?tail=abc").starts_with("HTTP/1.1 200"));
        assert!(get("/logs?tail=0").starts_with("HTTP/1.1 200"));
    }

    #[test]
    fn unknown_path_is_404() {
        assert!(get("/../../Windows/System32").starts_with("HTTP/1.1 404"));
    }

    #[test]
    fn query_param_handles_plus_and_percent() {
        assert_eq!(
            query_param("a=1&text=hi+there", "text").as_deref(),
            Some("hi there")
        );
        assert_eq!(
            query_param("addr=6ACECA31548C", "addr").as_deref(),
            Some("6ACECA31548C")
        );
        assert_eq!(query_param("a=1", "b"), None);
        // 非法转义按字面量保留，但绝不能因此把参数判成"不存在"
        assert_eq!(query_param("x=%zz", "x").as_deref(), Some("%zz"));
        assert_eq!(query_param("x=abc%2", "x").as_deref(), Some("abc%2"));
    }

    /// 回归：多字节 UTF-8 必须在控制面入口一次解对——逐 `%XX` push 会把 `中文` 变成 Latin-1 乱码，而调用方（剪贴板里的任意用户文本）不会再解码第二次。
    #[test]
    fn query_param_decodes_multibyte_utf8() {
        for text in ["中文αβγ", "你好，LinkX 👋", "Ñoño", "a b%c3%a9"] {
            let q = format!("text={}", pct_encode(text));
            assert_eq!(
                query_param(&q, "text").as_deref(),
                Some(text),
                "往返应还原原文：{text:?}"
            );
        }
        // 未转义的裸 UTF-8（有人直接 curl 塞中文）同样必须正确
        assert_eq!(
            query_param("text=中文αβγ", "text").as_deref(),
            Some("中文αβγ")
        );
        // 只送来半个多字节序列：不得 panic，也不得吞掉参数
        assert!(query_param("x=%E4%B8", "x").is_some());
    }

    /// 测试用百分号编码（避免为一条测试引入 percent-encoding 依赖）。
    fn pct_encode(s: &str) -> String {
        s.bytes()
            .map(|b| match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' => {
                    (b as char).to_string()
                }
                _ => format!("%{b:02X}"),
            })
            .collect()
    }

    /// 动作路由必须真的被派发：无宿主处理器时回 400 + 明确原因，而不是 404 或静默 200。
    #[test]
    fn action_route_is_dispatched_not_silently_404() {
        let body = post("/action/__probe__");
        assert!(
            body.starts_with("HTTP/1.1 200") || body.starts_with("HTTP/1.1 400"),
            "{body}"
        );
        assert!(body.contains("\"action\":\"__probe__\""), "{body}");
    }

    /// 动作名取自请求行（`/action/<名字>`），不转义就等于让调用方改写响应的 JSON 结构。
    #[test]
    fn action_name_cannot_forge_the_response_json() {
        let name = "x\",\"ok\":true,\"injected\":\"y";
        let (_, _, body) = action_json(name, "");
        let v: serde_json::Value = serde_json::from_str(&body).expect("响应必须是合法 JSON");
        assert_eq!(v["action"], name, "动作名应原样待在 action 字段里");
        assert_eq!(v["ok"], false, "宿主未注册处理器时不该被改写成 ok:true");
        assert!(
            v.get("injected").is_none(),
            "注入出来的字段不该存在：{body}"
        );
    }

    /// 动作失败时，原因必须出现在响应体里：`action_json` 若只转义 `Ok` 分支，`Err` 的字符串会被整段丢弃，脚本侧每次失败都长成 `ok:false,"result":""`。
    #[test]
    fn failed_action_carries_its_reason() {
        let (_, _, body) = action_json("__no_such_action__", "");
        if body.contains("\"ok\":false") {
            assert!(
                !body.contains("\"result\":\"\""),
                "动作失败却没有给出任何原因：{body}"
            );
        }
    }

    /// 本机脚本发起的请求必须放行（这是控制面唯一的使用方式）
    #[test]
    fn loopback_script_requests_are_served() {
        for head in [
            "GET /state HTTP/1.1\r\nHost: 127.0.0.1:55699\r\n\r\n",
            // adb forward 那侧的端口不是 55699，端口不参与判定
            "POST /action/connect HTTP/1.1\r\nHost: 127.0.0.1:55700\r\n\r\n",
            "GET /health HTTP/1.1\r\nHost: localhost\r\n\r\n",
            "GET /logs?tail=50 HTTP/1.1\r\nHost: [::1]:55699\r\n\r\n",
        ] {
            assert!(!from_browser(head), "误伤本机脚本请求：{head:?}");
            let (status, _, _) = route(head);
            assert_ne!(status, 403, "本机脚本请求被当成浏览器请求：{head:?}");
        }
    }

    /// DNS 重绑 / 跨站请求必须挡住：负向对照，缺了它这条安全边界等于没有。
    #[test]
    fn browser_shaped_requests_are_refused() {
        // ① Host 不是回环名 —— 恶意网页把 linkx.test 解析到 127.0.0.1 后的样子
        let rebind = "GET /state HTTP/1.1\r\nHost: linkx.test:55699\r\n\r\n";
        assert!(from_browser(rebind), "DNS 重绑的 Host 没被拒");
        assert_eq!(route(rebind).0, 403);
        // ①b 以 127. 开头但根本不是 IP 字面量的名字：*.nip.io 这类公共解析服务会把
        //     "127.0.0.1.nip.io" 真的解析到 127.0.0.1，前缀匹配等于把这道闸敞开。
        for host in [
            "127.0.0.1.nip.io:55699",
            "127.1",
            "127.0.0.1.evil.example",
            "127.0.0.999",
        ] {
            let head = format!("GET /state HTTP/1.1\r\nHost: {host}\r\n\r\n");
            assert!(from_browser(&head), "非字面量回环名没被拒：{host}");
            assert_eq!(route(&head).0, 403, "非字面量回环名应走 403：{host}");
        }
        // ② 带 Origin —— 跨源 fetch/XHR/表单 POST 都带，confirm-sas 是简单请求不触发预检
        let xss = "POST /action/confirm-sas HTTP/1.1\r\nHost: 127.0.0.1:55699\r\n\
                   Origin: http://evil.example\r\nContent-Type: text/plain\r\n\r\n";
        assert!(from_browser(xss), "带 Origin 的跨站动作没被拒");
        assert_eq!(route(xss).0, 403);
        // ③ Sec-Fetch-Site 非 none —— 从别的页面发过来的
        let cross = "GET /state HTTP/1.1\r\nHost: 127.0.0.1:55699\r\n\
                     Sec-Fetch-Site: cross-site\r\n\r\n";
        assert!(from_browser(cross), "跨站 Sec-Fetch-Site 没被拒");
        // ④ Referer 同样是一眼可辨的浏览器痕迹
        assert!(from_browser(
            "GET /state HTTP/1.1\r\nHost: 127.0.0.1:55699\r\nReferer: http://evil.example/\r\n\r\n"
        ));
    }
}
