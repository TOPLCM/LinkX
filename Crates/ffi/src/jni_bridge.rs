//! Android JNI 导出（feature = "jni"，构建 `linkx_core.so`），对应 Kotlin `com.linkx.app.NativeCore` 的 `external fun`。
//! 三条铁律：① 事件只经队列单向流出、Kotlin 侧 poll；② 句柄谁分配谁释放（`Box::into_raw`/`from_raw`）；③ 不跨边界抛异常——入口判空指针并包 `catch_unwind`，但交付构建是全 profile `panic = "abort"`，**panic 就是进程终止**，包装只在非 abort 构建下生效（详见 `lib.rs` 头注释）。
//! 编解码（Kotlin `ByteBuffer` 默认大端，逐项对齐）。出站包 `nativeDrain`：重复 `u16 pkt_len | pkt`，空数组 = 无待发。
//! 事件流 `nativePollEvents`：重复 `u16 ev_len | u8 kind | payload`，各 kind 的 payload 字段序：
//! 1 StateChanged `u8 state`；2 PeerHello `u8 os|u16 name|u16 ver`；3 SasReady `u32 sas`；4 PeerPaired `u16 fp`；5 Notification `i64 ts_ms|u32 key_hash|u16 pkg|u16 title|u16 text`；
//! 6 Clipboard `u16 text`；7 Error `i32 code|u16 ctx`；8 FileMeta `u64 id|u64 size|u32 chunk|u32 crc32|32B sha256|u16 name`（sha256 全零 = 摘要延到 FILE_DONE）；
//! 9 FileDone `u64 id|u8 ok|u16 err|u8 sha_len|sha_len B`（尾段可缺）；10 FileResume `u64 id|u32 from`；11 TcpBound；12 TcpUnbound `u16 reason`；
//! 13 Config `u16 n|(key,value,scope)*`；14 IdentityChanged `u16 name|u16 old_fp|u16 new_fp`；15 MediaCommand `i32 action|i32 volume|i64 delta_ms`；
//! 16 FileTaskFailed `u64 id|u16 reason`；17 FileTaskCancelled 同 16（**取消不是失败**，收端据此删残留文件）；18 AlbumListReq `u32 page|u32 per_page`；
//! 19 AlbumThumbReq `u64 id|u32 edge`；20 AlbumFullReq `u32 count|count × u64 id`（count ≤ 256）；
//! 21 NotifyReplyReq `u32 reply_id|i32 notification_id|i32 action_index|u16 pkg|u16 tag|u16 result_key|u16 text`。21 以下已占用，新事件从 22 起加。
//! 手机是生产者的 `MediaState`/`MediaCover`/`DeviceStatus`/回复回执 **故意不编码**——Android 收不到自己发出的东西；取消收尾（FILE_DONE{cancelled:true}）不另发 kind 9：引擎在解码处就转成 17。

use std::collections::HashSet;
// 仅调试控制面用到；不带 feature 的交付构建里它们没有使用者，单列 cfg 以免 unused import。
#[cfg(feature = "agent-debug")]
use std::collections::{BTreeMap, VecDeque};
use std::os::raw::c_int;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use jni::objects::{JByteArray, JClass, JString};
use jni::sys::{jboolean, jbyteArray, jint, jlong, jstring};
use jni::JNIEnv;
use linkx_protocol::pb::{
    AlbumItem, AlbumList, AlbumThumb, DeviceStatus, FileMeta, MediaCover, MediaState,
    NotificationDismiss, NotificationPush, NotificationReplyAck,
};
use linkx_session::engine::{EngineConfig, EngineEvent, EngineRole, SessionEngine};

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// 事件内单个字符串字段的字节上限（u16 长度域容量约束下的安全余量）
const MAX_FIELD_BYTES: usize = 16 * 1024;

/// 有效句柄表：仅在「已创建且未释放」期间登记，`with_engine` 持锁校验，从而杜绝已 free 的悬垂指针或伪造值被解引用（UB）。
fn live_handles() -> &'static Mutex<HashSet<i64>> {
    static REG: OnceLock<Mutex<HashSet<i64>>> = OnceLock::new();
    REG.get_or_init(|| Mutex::new(HashSet::new()))
}

/// 在 UTF-8 字符边界上截断到不超过 `max_bytes` 字节
fn truncate_utf8(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// 长度域为 u16：超出容量时必须显式失败，**不得静默取低位**，否则 Kotlin 侧按错误长度解析 → 协议错位/崩溃。
fn put_u16(out: &mut Vec<u8>, v: usize) -> Result<(), ()> {
    let n = u16::try_from(v).map_err(|_| ())?;
    out.extend_from_slice(&n.to_be_bytes());
    Ok(())
}

/// 写入长度前缀字符串；输入在进入前已被约束到 `MAX_FIELD_BYTES`
fn put_str(out: &mut Vec<u8>, s: &str) {
    let s = truncate_utf8(s, MAX_FIELD_BYTES);
    if put_u16(out, s.len()).is_err() {
        return; // 已被 truncate 约束，理论不可达
    }
    out.extend_from_slice(s.as_bytes());
}

/// `linkx_session_new`：创建会话引擎句柄。`identity_der` 是 **RSA-2048 设备身份**（PKCS#8 DER，Kotlin
/// 侧用 Keystore AES-GCM 加密持久化后传入解密字节）——设备识别与信任判定的唯一锚点；`trusted_tsv` 是信任库
/// （`指纹\t名称` 逐行，格式见 `linkx_session::trust`）；X25519 `sk_hex` 只做 Noise 握手，不参与设备识别。
#[no_mangle]
pub extern "system" fn Java_com_linkx_app_NativeCore_nativeSessionNew(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    role: jint,
    name: JString<'_>,
    version: JString<'_>,
    sk_hex: JString<'_>,
    identity_der: JByteArray<'_>,
    trusted_tsv: JString<'_>,
) -> jlong {
    let result = catch_unwind(AssertUnwindSafe(|| -> Option<i64> {
        let name: String = env.get_string(&name).ok()?.into();
        let version: String = env.get_string(&version).ok()?.into();
        let sk_hex: String = env.get_string(&sk_hex).ok()?.into();
        let trusted: String = env.get_string(&trusted_tsv).ok()?.into();
        let der = env.convert_byte_array(&identity_der).ok()?;
        let sk = decode_sk(&sk_hex)?;
        let role = match role {
            1 => EngineRole::Initiator,
            _ => EngineRole::Responder,
        };
        // 身份不可用（DER 非法）→ 拒绝创建引擎：宁可不连，也不能用临时身份让对端看到「新设备」。
        linkx_crypto::identity::DeviceIdentity::from_pkcs8_der(&der).ok()?;
        let cfg = EngineConfig::new(role, name, linkx_protocol::OS_ANDROID, version, sk)
            .with_identity_der(der)
            .with_trusted_peers(linkx_session::trust::parse_tsv(&trusted));
        let engine = Box::new(SessionEngine::new(cfg));
        let raw = Box::into_raw(engine) as i64;
        // 登记为「有效句柄」；未登记的句柄一律拒绝解引用
        if let Ok(mut reg) = live_handles().lock() {
            reg.insert(raw);
        }
        Some(raw)
    }))
    .ok()
    .flatten();
    result.unwrap_or(0)
}

/// 出站 TCP 队列当前深度（平台层节流用）。无引擎时返回 -1（调用方按"不能发"处理）。
#[no_mangle]
pub extern "system" fn Java_com_linkx_app_NativeCore_nativeTcpOutDepth(
    _env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
) -> jint {
    with_engine(handle, |e| e.tcp_out_depth() as jint).unwrap_or(-1)
}

/// 在途窗口大小（由核心层定，避免两端各抄一份常量抄歪）。
#[no_mangle]
pub extern "system" fn Java_com_linkx_app_NativeCore_nativeTcpOutWindow(
    _env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
) -> jint {
    with_engine(handle, |e| e.tcp_out_window() as jint).unwrap_or(0)
}

/// 从通知标题/正文里抽验证码（规则唯一真源 = `linkx_session::code_extract`）。刻意**不接引擎句柄**：它是一条
/// 纯字符串规则，两端各算各的，不需要会话状态；放核心层是因为安卓与电脑必须给同一个答案，否则用户看到的是
/// "LinkX 有时认不出码"，而根因是两端各写了一份规则。无可信结果返回 `null`（Kotlin 据此**不显示**按钮，而不是显示一个空按钮）。
#[no_mangle]
pub extern "system" fn Java_com_linkx_app_NativeCore_nativeExtractCode(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    title: JString<'_>,
    text: JString<'_>,
) -> jstring {
    let out = catch_unwind(AssertUnwindSafe(|| -> Option<String> {
        let t: String = env.get_string(&title).ok()?.into();
        let b: String = env.get_string(&text).ok()?.into();
        linkx_session::code_extract::extract_code(&t, &b).map(|c| c.digits)
    }))
    .ok()
    .flatten();
    match out {
        Some(digits) => env
            .new_string(digits)
            .map(|js| js.into_raw())
            .unwrap_or(std::ptr::null_mut()),
        None => std::ptr::null_mut(),
    }
}

/// 由 Kotlin 在**首次运行**（或旧版身份迁移）时调用：拿到 DER 后用 Keystore AES-GCM 加密落盘，
/// 之后每次启动解密后传给 `nativeSessionNew`。生成失败返回 null（Kotlin 侧应显式提示，不得静默用临时身份）。
#[no_mangle]
pub extern "system" fn Java_com_linkx_app_NativeCore_nativeIdentityGenerate(
    env: JNIEnv<'_>,
    _class: JClass<'_>,
) -> jbyteArray {
    let der = catch_unwind(AssertUnwindSafe(|| -> Option<Vec<u8>> {
        let id = linkx_crypto::identity::DeviceIdentity::generate().ok()?;
        id.to_pkcs8_der().ok()
    }))
    .ok()
    .flatten()
    .unwrap_or_default();
    if der.is_empty() {
        return std::ptr::null_mut();
    }
    env.byte_array_from_slice(&der)
        .map(|a| a.into_raw())
        .unwrap_or(std::ptr::null_mut())
}

/// `nativeIdentityFingerprint`：由 PKCS#8 DER 计算本机指纹（16 位小写 hex）。
/// 设置页展示用；DER 非法返回 null。
#[no_mangle]
pub extern "system" fn Java_com_linkx_app_NativeCore_nativeIdentityFingerprint(
    env: JNIEnv<'_>,
    _class: JClass<'_>,
    der: JByteArray<'_>,
) -> jstring {
    let fp = catch_unwind(AssertUnwindSafe(|| -> Option<String> {
        let der = env.convert_byte_array(&der).ok()?;
        linkx_crypto::identity::DeviceIdentity::from_pkcs8_der(&der)
            .ok()?
            .fingerprint()
            .ok()
    }))
    .ok()
    .flatten();
    match fp {
        Some(s) => env
            .new_string(s)
            .map(|js| js.into_raw())
            .unwrap_or(std::ptr::null_mut()),
        None => std::ptr::null_mut(),
    }
}

/// `nativeTrustUpsert`：把一次配对结果并入信任库文本，返回新的 TSV。Kotlin 只需持久化返回值
/// （格式规则收在 `linkx_session::trust`，两端同源）；`trusted_tsv` 为空 → 视为空库。
#[no_mangle]
pub extern "system" fn Java_com_linkx_app_NativeCore_nativeTrustUpsert(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    trusted_tsv: JString<'_>,
    fingerprint: JString<'_>,
    peer_name: JString<'_>,
) -> jstring {
    let out = catch_unwind(AssertUnwindSafe(|| -> Option<String> {
        let raw: String = env.get_string(&trusted_tsv).ok()?.into();
        let fp: String = env.get_string(&fingerprint).ok()?.into();
        let name: String = env.get_string(&peer_name).ok()?.into();
        let mut peers = linkx_session::trust::parse_tsv(&raw);
        linkx_session::trust::upsert(&mut peers, &fp, &name);
        Some(linkx_session::trust::to_tsv(&peers))
    }))
    .ok()
    .flatten();
    match out {
        Some(s) => env
            .new_string(s)
            .map(|js| js.into_raw())
            .unwrap_or(std::ptr::null_mut()),
        None => std::ptr::null_mut(),
    }
}

/// `nativeSetDebug`：开关 Debug 模式（`dir` 为日志落盘目录）。返回 1 = 成功，0 = 失败（目录不可写等）。
#[no_mangle]
pub extern "system" fn Java_com_linkx_app_NativeCore_nativeSetDebugEnabled(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    on: jboolean,
    dir: JString<'_>,
) -> jint {
    let ok = catch_unwind(AssertUnwindSafe(|| -> bool {
        if on == 0 {
            debuglog::disable();
            return true;
        }
        let Ok(d) = env.get_string(&dir) else {
            return false;
        };
        let d: String = d.into();
        if d.trim().is_empty() {
            return false;
        }
        debuglog::enable(d.trim()).is_ok()
    }))
    .unwrap_or(false);
    if ok {
        1
    } else {
        0
    }
}

/// `nativeExportDebug`：导出 Debug 日志到指定目录，返回导出路径（失败返回空串）。
#[no_mangle]
pub extern "system" fn Java_com_linkx_app_NativeCore_nativeExportDebug(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    dir: JString<'_>,
) -> jstring {
    let out = catch_unwind(AssertUnwindSafe(|| -> Option<String> {
        let d: String = env.get_string(&dir).ok()?.into();
        let d = d.trim().to_string();
        if d.is_empty() {
            return None;
        }
        debuglog::export_to(std::path::Path::new(&d))
            .ok()
            .map(|p| p.display().to_string())
    }))
    .ok()
    .flatten();
    match out {
        Some(s) => env
            .new_string(s)
            .map(|js| js.into_raw())
            .unwrap_or(std::ptr::null_mut()),
        None => std::ptr::null_mut(),
    }
}

fn decode_sk(hex_str: &str) -> Option<[u8; 32]> {
    let bytes = hex::decode(hex_str.trim()).ok()?;
    if bytes.len() != 32 {
        return None;
    }
    let mut sk = [0u8; 32];
    sk.copy_from_slice(&bytes);
    Some(sk)
}

/// 仅在句柄仍在有效表中时执行回调，且**持锁期间调用**——消除「校验通过后、解引用前被 `nativeSessionFree`
/// 释放」的 TOCTOU 竞态。⚠ **本函数不保护引擎内容**：`live_handles` 那把锁只管句柄有效性，下面是裸指针
/// 解引用。真正的互斥来自 Kotlin 侧 `LinkxRuntime` 方法上的 `@Synchronized`（object monitor）——Binder、
/// UI、linkx-tick 三个线程都会进来，恰好被同一把 monitor 串行化。所以**新增任何 JNI 入口都必须经
/// `LinkxRuntime` 上带 `@Synchronized` 的方法**，从 Kotlin 其他线程直调 `NativeCore.nativeXxx`
/// 就是数据竞争 / UAF。
fn with_engine<R>(handle: jlong, f: impl FnOnce(&mut SessionEngine) -> R) -> Option<R> {
    if handle == 0 {
        return None;
    }
    let reg = live_handles().lock().ok()?;
    if !reg.contains(&handle) {
        return None; // 已释放 / 伪造句柄 → 拒绝（防 UAF）
    }
    let engine = unsafe { &mut *(handle as *mut SessionEngine) };
    Some(f(engine))
}

/// 释放句柄（仅调用一次；0 与未登记句柄均容忍）
#[no_mangle]
pub extern "system" fn Java_com_linkx_app_NativeCore_nativeSessionFree(
    _env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
) {
    if handle == 0 {
        return;
    }
    // 摘表与释放必须在**同一把锁的持有期内**连着做完。先放锁再 free 会留一个窗口：另一线程
    // 刚通过 `with_engine` 的有效性校验、正要解引用，对象却在这中间被释放 —— 真 UAF，而全
    // profile `panic = "abort"` 之下它不是可恢复错误，是整个 App 进程没。
    // （Kotlin 侧 `LinkxRuntime` 的 `@Synchronized` 目前挡住了这种并发，但那道防线在本层
    //  之外、随时可能被一次重构挪走，所以这里自己闭环。）
    let mut reg = match live_handles().lock() {
        Ok(g) => g,
        Err(_) => return,
    };
    if !reg.remove(&handle) {
        return; // 幂等：未登记 / 已释放的句柄直接容忍
    }
    let _ = catch_unwind(AssertUnwindSafe(|| unsafe {
        drop(Box::from_raw(handle as *mut SessionEngine));
    }));
}

#[no_mangle]
pub extern "system" fn Java_com_linkx_app_NativeCore_nativeStart(
    _env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
) {
    let _ = catch_unwind(AssertUnwindSafe(|| {
        let _ = with_engine(handle, |e| e.start(std::time::Instant::now()));
    }));
}

#[no_mangle]
pub extern "system" fn Java_com_linkx_app_NativeCore_nativeTick(
    _env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
) {
    let _ = catch_unwind(AssertUnwindSafe(|| {
        let _ = with_engine(handle, |e| e.tick(std::time::Instant::now()));
    }));
    // 控制面快照挂在 nativeTick 上发布（Kotlin 每秒已调一次），`/state` 因此天然新鲜，
    // 安卓侧无需任何 Kotlin 配合就能读到状态机与 SAS。
    #[cfg(feature = "agent-debug")]
    let _ = catch_unwind(AssertUnwindSafe(publish_from_engines));
}

/// 喂入 GATT 写入的分片包字节
#[no_mangle]
pub extern "system" fn Java_com_linkx_app_NativeCore_nativeFeed(
    env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
    data: JByteArray<'_>,
) {
    let _ = catch_unwind(AssertUnwindSafe(|| {
        let Ok(bytes) = env.convert_byte_array(&data) else {
            return;
        };
        let _ = with_engine(handle, |e| e.feed(&bytes, std::time::Instant::now()));
    }));
}

/// 取出待发分片包（`u16 len | pkt` 重复；无待发返回空数组）
#[no_mangle]
pub extern "system" fn Java_com_linkx_app_NativeCore_nativeDrain(
    env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
) -> jbyteArray {
    let buf = catch_unwind(AssertUnwindSafe(|| {
        let mut out = Vec::new();
        let _ = with_engine(handle, |e| {
            for pkt in e.take_outbound() {
                // 分片包超 u16 容量时跳过该包（不写错长度、不误判后续帧）
                if put_u16(&mut out, pkt.len()).is_err() {
                    continue;
                }
                out.extend_from_slice(&pkt);
            }
        });
        out
    }))
    .unwrap_or_default();
    env.byte_array_from_slice(&buf)
        .map(|a| a.into_raw())
        .unwrap_or(std::ptr::null_mut())
}

/// 取出上行事件（编码见模块头注释）
#[no_mangle]
pub extern "system" fn Java_com_linkx_app_NativeCore_nativePollEvents(
    env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
) -> jbyteArray {
    let buf = catch_unwind(AssertUnwindSafe(|| {
        let mut out = Vec::new();
        let _ = with_engine(handle, |e| {
            for ev in e.take_events() {
                encode_event(&mut out, &ev);
            }
        });
        out
    }))
    .unwrap_or_default();
    env.byte_array_from_slice(&buf)
        .map(|a| a.into_raw())
        .unwrap_or(std::ptr::null_mut())
}

fn encode_event(out: &mut Vec<u8>, ev: &EngineEvent) {
    let mut body = Vec::new();
    match ev {
        EngineEvent::StateChanged { state } => {
            body.push(1);
            body.push(*state);
        }
        EngineEvent::PeerHello { name, os, version } => {
            body.push(2);
            body.push(*os);
            put_str(&mut body, name);
            put_str(&mut body, version);
        }
        EngineEvent::SasReady { sas } => {
            body.push(3);
            body.extend_from_slice(&sas.to_be_bytes());
        }
        EngineEvent::PeerPaired { fingerprint } => {
            body.push(4);
            put_str(&mut body, fingerprint);
        }
        EngineEvent::Notification {
            package,
            title,
            text,
            post_ts_ms,
            key_hash,
            // 回复元数据是手机自己填上去的，不必再读回来：手机是通知的生产者
            ..
        } => {
            body.push(5);
            body.extend_from_slice(&post_ts_ms.to_be_bytes());
            // 稳定 key 随事件下行（Kotlin 侧做「就地合并更新」）
            body.extend_from_slice(&key_hash.to_be_bytes());
            put_str(&mut body, package);
            put_str(&mut body, title);
            put_str(&mut body, text);
        }
        EngineEvent::Clipboard { text } => {
            body.push(6);
            put_str(&mut body, text);
        }
        EngineEvent::Error { code, context } => {
            body.push(7);
            body.extend_from_slice(&code.to_be_bytes());
            put_str(&mut body, context);
        }
        EngineEvent::FileMetaReceived {
            file_id,
            name,
            size,
            chunk_size,
            sha256,
            crc32,
            // `album_id` 故意不编码：相册原图只可能是手机**发出去**的那一侧的应答，手机作为收端时它恒为 0。写进流里等于让 Kotlin 多维护一个永远用不到的字段。
            album_id: _,
        } => {
            body.push(8);
            body.extend_from_slice(&file_id.to_be_bytes());
            body.extend_from_slice(&size.to_be_bytes());
            body.extend_from_slice(&chunk_size.to_be_bytes());
            body.extend_from_slice(&crc32.to_be_bytes());
            // 固定 32B 摘要；`None`（摘要延到 FILE_DONE）写成全零——SHA-256 不会命中全零原像
            body.extend_from_slice(sha256.as_ref().unwrap_or(&[0u8; 32]));
            put_str(&mut body, name);
        }
        EngineEvent::FileDoneReceived {
            file_id,
            ok,
            error,
            sha256,
        } => {
            body.push(9);
            body.extend_from_slice(&file_id.to_be_bytes());
            body.push(if *ok { 1 } else { 0 });
            put_str(&mut body, error.as_deref().unwrap_or(""));
            // 自描述尾段：u8 长度 + 摘要字节（0 = 本帧没带，Kotlin 回落 META 声明值）
            match sha256 {
                Some(s) => {
                    body.push(32);
                    body.extend_from_slice(s);
                }
                None => body.push(0),
            }
        }
        EngineEvent::FileResumeRequested {
            file_id,
            from_index,
        } => {
            body.push(10);
            body.extend_from_slice(&file_id.to_be_bytes());
            body.extend_from_slice(&from_index.to_be_bytes());
        }
        EngineEvent::TcpBound => {
            body.push(11);
        }
        EngineEvent::TcpUnbound { reason } => {
            body.push(12);
            put_str(&mut body, reason);
        }
        EngineEvent::ConfigReceived { entries } => {
            // 流对齐不变式：一条事件要么完整写进流，要么整条丢弃（与函数末尾"事件总长超
            // u16 就 return"同一条口径）。只跳过条目而留下 kind 字节，Kotlin 会把后续事件
            // 的字节当作这条的字段读，从此整条事件流错位。
            body.push(13);
            if put_u16(&mut body, entries.len()).is_err() {
                return;
            }
            for e in entries {
                put_str(&mut body, &e.key);
                put_str(&mut body, &e.value);
                put_str(&mut body, &e.scope);
            }
        }
        // 同名设备呈递新身份 → UI 必须弹「重新配对确认」
        EngineEvent::IdentityChanged {
            name,
            old_fingerprint,
            new_fingerprint,
        } => {
            body.push(14);
            put_str(&mut body, name);
            put_str(&mut body, old_fingerprint);
            put_str(&mut body, new_fingerprint);
        }
        // 播放状态只往电脑方向走（手机是生产者），Android 侧显式不编码。
        // 这里不用 `_ =>` 兜底：将来给 EngineEvent 加变体时，漏写编码要在编译期
        // 暴露出来，而不是悄悄变成"事件发了但对端解不出来"。
        EngineEvent::MediaState { .. } => return,
        EngineEvent::MediaCover { .. } => return,
        EngineEvent::DeviceStatus { .. } => return,
        // 电脑下发的播放控制指令 → Kotlin 侧交给 MediaControl 执行
        EngineEvent::MediaCommand {
            action,
            volume,
            delta_ms,
        } => {
            body.push(15);
            body.extend_from_slice(&action.to_be_bytes());
            body.extend_from_slice(&volume.to_be_bytes());
            body.extend_from_slice(&delta_ms.to_be_bytes());
        }
        // 文件任务被引擎硬失败（未绑定就发送 / 传输中 TCP 断开）→ Kotlin 必须落到任务状态。编码成独立 kind 而不是塞进 Error，因为这类失败若只存在于日志里就会静默。
        EngineEvent::FileTaskFailed { file_id, reason } => {
            body.push(16);
            body.extend_from_slice(&file_id.to_be_bytes());
            put_str(&mut body, reason);
        }
        // 用户取消（任一端发起都走这一条）：kind 与 FileTaskFailed 分开，否则手机端只能把"我按了取消"显示成"传输出错"。
        EngineEvent::FileTaskCancelled { file_id, reason } => {
            body.push(17);
            body.extend_from_slice(&file_id.to_be_bytes());
            put_str(&mut body, reason);
        }
        // 相册：手机是**应答方**，只会收到"要清单 / 要缩略图 / 要原图"这三条。
        EngineEvent::AlbumListRequested { page, per_page } => {
            body.push(18);
            body.extend_from_slice(&page.to_be_bytes());
            body.extend_from_slice(&per_page.to_be_bytes());
        }
        EngineEvent::AlbumThumbRequested { id, edge } => {
            body.push(19);
            body.extend_from_slice(&id.to_be_bytes());
            body.extend_from_slice(&edge.to_be_bytes());
        }
        EngineEvent::AlbumFullRequested { ids } => {
            body.push(20);
            // 条数有界：一次导出再多的照片也不该把事件流撑爆（超出部分由 Kotlin 侧提示分批）
            let take = ids.len().min(256);
            body.extend_from_slice(&(take as u32).to_be_bytes());
            for id in ids.iter().take(take) {
                body.extend_from_slice(&id.to_be_bytes());
            }
        }
        // 电脑要回复某条通知：手机是执行方，回完用 nativeSendNotifyReplyAck 出声。
        EngineEvent::NotifyReplyRequested {
            reply_id,
            package,
            tag,
            notification_id,
            action_index,
            result_key,
            text,
        } => {
            body.push(21);
            body.extend_from_slice(&reply_id.to_be_bytes());
            body.extend_from_slice(&notification_id.to_be_bytes());
            body.extend_from_slice(&action_index.to_be_bytes());
            put_str(&mut body, package);
            put_str(&mut body, tag);
            put_str(&mut body, result_key);
            put_str(&mut body, text);
        }
        // 回执与「通知已消失」都是手机**发出去**的，它自己永远不会收到：不写进事件流
        // （同 MediaState / AlbumPage 口径）。
        EngineEvent::NotifyReplyAck { .. } | EngineEvent::NotifyDismissed { .. } => return,
        // 清单与缩略图是手机**发出去**的应答，它自己永远不会收到：整条事件不写进流，免得 Kotlin 侧多一个"读到了但没有意义"的分支（同 MediaState / DeviceStatus 的口径）。
        EngineEvent::AlbumPage { .. } | EngineEvent::AlbumThumb { .. } => return,
    }
    // 事件总长超 u16 容量 → 丢弃该事件（保持流对齐），绝不写入被截断的长度
    if put_u16(out, body.len()).is_err() {
        return;
    }
    out.extend_from_slice(&body);
}

#[no_mangle]
pub extern "system" fn Java_com_linkx_app_NativeCore_nativeConfirmSas(
    _env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
) {
    let _ = catch_unwind(AssertUnwindSafe(|| {
        let _ = with_engine(handle, |e| e.confirm_sas());
    }));
}

#[no_mangle]
pub extern "system" fn Java_com_linkx_app_NativeCore_nativeRejectSas(
    _env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
) {
    let _ = catch_unwind(AssertUnwindSafe(|| {
        let _ = with_engine(handle, |e| e.reject_sas());
    }));
}

#[no_mangle]
pub extern "system" fn Java_com_linkx_app_NativeCore_nativeAcceptFingerprint(
    _env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
) {
    let _ = catch_unwind(AssertUnwindSafe(|| {
        let _ = with_engine(handle, |e| e.accept_new_fingerprint());
    }));
}

/// 推送剪贴板纯文本（返回 1 = 已入队，0 = 未配对/失败）
#[no_mangle]
pub extern "system" fn Java_com_linkx_app_NativeCore_nativeSendClipboard(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
    text: JString<'_>,
) -> jint {
    let sent = catch_unwind(AssertUnwindSafe(|| -> bool {
        let Ok(text) = env.get_string(&text) else {
            return false;
        };
        let text: String = text.into();
        with_engine(handle, |e| e.send_clipboard_text(&text, now_ms())).unwrap_or(false)
    }))
    .unwrap_or(false);
    sent as c_int
}

/// 会话状态（state_code::*，未初始化返回 -1）
#[no_mangle]
pub extern "system" fn Java_com_linkx_app_NativeCore_nativeState(
    _env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
) -> jint {
    catch_unwind(AssertUnwindSafe(|| {
        with_engine(handle, |e| match e.state() {
            linkx_session::SessionState::Discover => 0,
            linkx_session::SessionState::Handshake => 1,
            linkx_session::SessionState::Pairing => 2,
            linkx_session::SessionState::SasCompare => 3,
            linkx_session::SessionState::Paired => 4,
            linkx_session::SessionState::Repaired => 5,
            linkx_session::SessionState::Reconnecting => 6,
            linkx_session::SessionState::Closed => 7,
        })
        .unwrap_or(-1)
    }))
    .unwrap_or(-1)
}

/// 注入链路协商到的 ATT MTU。只由 Kotlin 的 linkx-tick 线程调用（与 `nativeTick`/`nativeDrain`
/// 同一串行驱动线程），故可直接复用 `with_engine`。调用点是 `LinkxRuntime.pump()` 而不是
/// `BluetoothGattServerCallback.onMtuChanged`——后者跑在 Binder 线程上，在那里碰引擎就是跨线程 data race。
#[no_mangle]
pub extern "system" fn Java_com_linkx_app_NativeCore_nativeSetBleMtu(
    _env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
    mtu: jint,
) {
    let _ = catch_unwind(AssertUnwindSafe(|| {
        if mtu > 0 {
            let _ = with_engine(handle, |e| e.set_ble_mtu(mtu as usize));
        }
    }));
}

/// Core 版本（Kotlin `System.loadLibrary("linkx_core")` 后可用）
#[no_mangle]
pub extern "system" fn Java_com_linkx_app_NativeCore_nativeVersion(
    env: JNIEnv<'_>,
    _class: JClass<'_>,
) -> jstring {
    catch_unwind(AssertUnwindSafe(|| {
        env.new_string(format!("LinkX Core {}", super::LINKX_FFI_VERSION))
            .ok()
            .map(|j| j.into_raw())
            .unwrap_or(std::ptr::null_mut())
    }))
    .unwrap_or(std::ptr::null_mut())
}

// ============ 媒体控制（手机当前播放 → 电脑；指令反向） ============

/// 推送当前播放状态（手机 → 电脑）。`speed_x100` 而不是浮点：跨 JNI 传 f32 要在两端各自换算，
/// 传整数倍率少一处能写错的地方（1.25x → 125）。
#[no_mangle]
pub extern "system" fn Java_com_linkx_app_NativeCore_nativeSendMediaState(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
    package: JString<'_>,
    title: JString<'_>,
    artist: JString<'_>,
    album: JString<'_>,
    playing: jboolean,
    position_ms: jlong,
    duration_ms: jlong,
    speed_x100: jint,
    volume: jint,
    ts_ms: jlong,
) -> jint {
    let sent = catch_unwind(AssertUnwindSafe(|| -> bool {
        let (Ok(package), Ok(title), Ok(artist), Ok(album)) = (
            env.get_string(&package),
            env.get_string(&title),
            env.get_string(&artist),
            env.get_string(&album),
        ) else {
            return false;
        };
        let s = MediaState {
            package: package.to_string_lossy().into_owned(),
            title: title.to_string_lossy().into_owned(),
            artist: artist.to_string_lossy().into_owned(),
            album: album.to_string_lossy().into_owned(),
            playing: playing == 1,
            position_ms,
            duration_ms,
            speed: speed_x100 as f32 / 100.0,
            volume,
            ts_ms,
        };
        with_engine(handle, |e| e.send_media_state(&s, ts_ms)).unwrap_or(false)
    }))
    .unwrap_or(false);
    jint::from(sent as u8)
}

/// 推送当前曲目封面（手机 → 电脑）。`track_key` 是"这张图属于哪首歌"的凭据，
/// 与 `nativeSendMediaState` 的 package/title/artist 三段同口径拼接；局域网未绑定时
/// 引擎直接拒发（返回 0），调用方不必自己判断链路。
#[no_mangle]
pub extern "system" fn Java_com_linkx_app_NativeCore_nativeSendMediaCover(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
    track_key: JString<'_>,
    jpeg: JByteArray<'_>,
    ts_ms: jlong,
) -> jint {
    let sent = catch_unwind(AssertUnwindSafe(|| -> bool {
        let (Ok(key), Ok(bytes)) = (env.get_string(&track_key), env.convert_byte_array(&jpeg))
        else {
            return false;
        };
        let c = MediaCover {
            track_key: key.to_string_lossy().into_owned(),
            jpeg: bytes.into(),
        };
        with_engine(handle, |e| e.send_media_cover(&c, ts_ms)).unwrap_or(false)
    }))
    .unwrap_or(false);
    jint::from(sent as u8)
}

/// 手机设备状态上报（电量 0-100 / -1 表示读不到，充电中）。与播放状态同一口径：
/// 只在变化时调用，不要每秒推。
#[no_mangle]
pub extern "system" fn Java_com_linkx_app_NativeCore_nativeSendDeviceStatus(
    _env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
    battery: jint,
    charging: jboolean,
    ts_ms: jlong,
) -> jint {
    let sent = catch_unwind(AssertUnwindSafe(|| {
        with_engine(handle, |e| {
            e.send_device_status(
                &DeviceStatus {
                    battery,
                    charging: charging == 1,
                    ts_ms,
                },
                ts_ms,
            )
        })
        .unwrap_or(false)
    }))
    .unwrap_or(false);
    jint::from(sent as u8)
}

// ============ TCP 通道 / 文件传输 / 配置同步 / 设备管理 ============

/// 通知：带稳定 key 与**回复定位三元组**的推送（`key_hash=0` 表示无 key；
/// `can_reply=false` 时后三个回复字段一律不填，对端就不会给出回复入口）
#[no_mangle]
#[allow(clippy::too_many_arguments)]
pub extern "system" fn Java_com_linkx_app_NativeCore_nativeSendNotificationKeyed(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
    package: JString<'_>,
    title: JString<'_>,
    text: JString<'_>,
    post_ts_ms: jlong,
    key_hash: jint,
    tag: JString<'_>,
    notification_id: jint,
    can_reply: jboolean,
    reply_action_index: jint,
    reply_result_key: JString<'_>,
) -> jint {
    let sent = catch_unwind(AssertUnwindSafe(|| -> bool {
        let (Ok(package), Ok(title), Ok(text), Ok(tag), Ok(reply_result_key)) = (
            env.get_string(&package),
            env.get_string(&title),
            env.get_string(&text),
            env.get_string(&tag),
            env.get_string(&reply_result_key),
        ) else {
            return false;
        };
        let n = NotificationPush {
            package: package.into(),
            title: title.into(),
            text: text.into(),
            post_ts_ms,
            key_hash: key_hash as u32,
            cover_jpeg: Default::default(),
            tag: tag.into(),
            notification_id,
            can_reply: can_reply == 1,
            reply_action_index,
            reply_result_key: reply_result_key.into(),
        };
        with_engine(handle, |e| e.send_notification(&n, now_ms())).unwrap_or(false)
    }))
    .unwrap_or(false);
    sent as c_int
}

/// 回复回执（手机 → 电脑）：`error` 非空 = 有一句要如实显示给用户的话
#[no_mangle]
pub extern "system" fn Java_com_linkx_app_NativeCore_nativeSendNotifyReplyAck(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
    reply_id: jint,
    package: JString<'_>,
    ok: jboolean,
    error: JString<'_>,
) -> jint {
    let sent = catch_unwind(AssertUnwindSafe(|| -> bool {
        let (Ok(package), Ok(error)) = (env.get_string(&package), env.get_string(&error)) else {
            return false;
        };
        let a = NotificationReplyAck {
            reply_id: reply_id as u32,
            package: package.into(),
            ok: ok == 1,
            error: error.into(),
        };
        with_engine(handle, |e| e.send_notify_reply_ack(&a, now_ms())).unwrap_or(false)
    }))
    .unwrap_or(false);
    sent as c_int
}

/// 「这条通知已经不在了」（手机 → 电脑）：电脑据此撤掉回复入口。定位仍用推送那三元组。
#[no_mangle]
pub extern "system" fn Java_com_linkx_app_NativeCore_nativeSendNotifyDismiss(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
    package: JString<'_>,
    tag: JString<'_>,
    notification_id: jint,
    key_hash: jint,
) -> jint {
    let sent = catch_unwind(AssertUnwindSafe(|| -> bool {
        let (Ok(package), Ok(tag)) = (env.get_string(&package), env.get_string(&tag)) else {
            return false;
        };
        let d = NotificationDismiss {
            package: package.into(),
            tag: tag.into(),
            notification_id,
            key_hash: key_hash as u32,
        };
        with_engine(handle, |e| e.send_notify_dismiss(&d, now_ms())).unwrap_or(false)
    }))
    .unwrap_or(false);
    sent as c_int
}

/// TCP 通道：是否有待发送帧（1 = 有）
#[no_mangle]
pub extern "system" fn Java_com_linkx_app_NativeCore_nativeHasTcpOutbound(
    _env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
) -> jint {
    catch_unwind(AssertUnwindSafe(|| {
        with_engine(handle, |e| e.has_tcp_outbound()).unwrap_or(false)
    }))
    .unwrap_or(false) as c_int
}

/// TCP 通道：取出待发送完整帧（重复 `u32 len | frame`；空数组 = 无待发）
#[no_mangle]
pub extern "system" fn Java_com_linkx_app_NativeCore_nativeDrainTcp(
    env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
) -> jbyteArray {
    let buf = catch_unwind(AssertUnwindSafe(|| {
        let mut out = Vec::new();
        let _ = with_engine(handle, |e| {
            for frame in e.take_tcp_outbound() {
                out.extend_from_slice(&(frame.len() as u32).to_be_bytes());
                out.extend_from_slice(&frame);
            }
        });
        out
    }))
    .unwrap_or_default();
    env.byte_array_from_slice(&buf)
        .map(|a| a.into_raw())
        .unwrap_or(std::ptr::null_mut())
}

/// TCP 通道：喂入收到的完整帧字节
#[no_mangle]
pub extern "system" fn Java_com_linkx_app_NativeCore_nativeFeedTcp(
    env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
    data: JByteArray<'_>,
) {
    let _ = catch_unwind(AssertUnwindSafe(|| {
        let Ok(bytes) = env.convert_byte_array(&data) else {
            return;
        };
        let _ = with_engine(handle, |e| e.feed_tcp(&bytes, std::time::Instant::now()));
    }));
}

/// TCP 通道：发起绑定（role: 1 = TcpClient 主动连接，0 = TcpServer 被动监听）
#[no_mangle]
pub extern "system" fn Java_com_linkx_app_NativeCore_nativeBeginTcpBind(
    _env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
    role: jint,
) -> jint {
    let ok = catch_unwind(AssertUnwindSafe(|| {
        let role = if role == 1 {
            linkx_session::BindRole::TcpClient
        } else {
            linkx_session::BindRole::TcpServer
        };
        with_engine(handle, |e| e.begin_tcp_binding(role)).unwrap_or(false)
    }))
    .unwrap_or(false);
    ok as c_int
}

/// TCP 通道：socket 已关闭（绑定状态复位；在途传输大声失败，不降级 BLE）
#[no_mangle]
pub extern "system" fn Java_com_linkx_app_NativeCore_nativeTcpClosed(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
    reason: JString<'_>,
) {
    let _ = catch_unwind(AssertUnwindSafe(|| {
        let reason: String = env
            .get_string(&reason)
            .map(|s| s.into())
            .unwrap_or_else(|_| "closed".to_string());
        let _ = with_engine(handle, |e| e.on_tcp_closed(reason));
    }));
}

/// TCP 通道是否已绑定（1 = 已绑定，业务可走 TCP）
#[no_mangle]
pub extern "system" fn Java_com_linkx_app_NativeCore_nativeIsTcpBound(
    _env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
) -> jint {
    catch_unwind(AssertUnwindSafe(|| {
        with_engine(handle, |e| e.is_tcp_bound()).unwrap_or(false)
    }))
    .unwrap_or(false) as c_int
}

/// 文件：发送 FILE_META。`sha256` 32B = 发端预先算好的整文件摘要；**0B = 摘要延到 FILE_DONE**（此时 proto
/// 里留空，不能填 32 个零，否则旧端会拿零值去比对必然失败）；`crc32` 为整文件 CRC32。`album_id` 0 = 普通文件
/// 传输，非 0 = 对 `ALBUM_FULL_REQ` 的应答（照片的相册 id），电脑侧据此把落点从「互传收件箱」换到「相册导出」；
/// JNI 签名没有默认参数，只能收显式值，默认 0 由 Kotlin 侧包装函数承担。
#[no_mangle]
pub extern "system" fn Java_com_linkx_app_NativeCore_nativeSendFileMeta(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
    file_id: jlong,
    name: JString<'_>,
    size: jlong,
    chunk_size: jint,
    crc32: jint,
    sha256: JByteArray<'_>,
    album_id: jlong,
) -> jint {
    let ok = catch_unwind(AssertUnwindSafe(|| -> bool {
        let Ok(name) = env.get_string(&name) else {
            return false;
        };
        let Ok(sha) = env.convert_byte_array(&sha256) else {
            return false;
        };
        if !sha.is_empty() && sha.len() != 32 {
            return false; // 长度非法：宁可不入队，也不发一个对端必定判失败的摘要
        }
        let meta = FileMeta {
            name: name.into(),
            size: size as u64,
            file_id: file_id as u64,
            chunk_size: chunk_size as u32,
            sha256: if sha.is_empty() {
                Default::default()
            } else {
                sha.to_vec().into()
            },
            crc32: crc32 as u32,
            album_id: if album_id < 0 { 0 } else { album_id as u64 },
        };
        with_engine(handle, |e| e.send_file_meta(&meta, now_ms())).unwrap_or(false)
    }))
    .unwrap_or(false);
    ok as c_int
}

/// 解析 `nativeSendAlbumList` 的 `rows`（口径见该函数注释）。单独成函数是为了让这套切分规则可被
/// 单元测试钉住：它写错了不会崩，只会让电脑显示一批字段错位的照片，那种 bug 在真机上极难归因。
fn parse_album_rows(rows: &str) -> Vec<AlbumItem> {
    let mut items = Vec::new();
    for row in rows.split('\n') {
        if row.is_empty() {
            continue;
        }
        // 从右往左取 6 段数字，剩下的整段才是 `id|name…`
        let f: Vec<&str> = row.rsplitn(7, '|').collect();
        if f.len() != 7 {
            continue; // 段数不足 = 这行压根不是八字段
        }
        let (duration, kind, height, width, mtime, size, head) =
            (f[0], f[1], f[2], f[3], f[4], f[5], f[6]);
        // id 在名字的**左边**，所以按第一个 `|` 切一次就够：名字里的 `|` 全留在右半段
        let Some((id, name)) = head.split_once('|') else {
            continue;
        };
        let (Ok(id), Ok(size), Ok(mtime), Ok(width), Ok(height), Ok(kind), Ok(duration)) = (
            id.parse::<u64>(),
            size.parse::<i64>(),
            mtime.parse::<i64>(),
            width.parse::<u32>(),
            height.parse::<u32>(),
            kind.parse::<u32>(),
            duration.parse::<i64>(),
        ) else {
            continue;
        };
        items.push(AlbumItem {
            id,
            name: name.to_string(),
            size_bytes: size,
            mtime_ms: mtime,
            width,
            height,
            kind,
            duration_ms: duration,
        });
    }
    items
}

/// 相册：发送 ALBUM_LIST（手机 → 电脑的一页清单）。1 = 已入队，0 = 没发出。`rows` 每行一条
/// `id|name|size|mtime|width|height|kind|duration_ms`，行间 `\n`。**切分口径：从右往左取 6 个数字字段
/// （duration/kind/height/width/mtime/size），剩下的 `id|name…` 再按第一个 `|` 分成 id 与 name**。选它而不选
/// 「给 name 转义」有两个理由：一是文件名里出现 `|` 完全合法，而 `id`/尺寸/时间戳是数字、永不含 `|`，这个口径
/// **无需任何转义约定就已经无歧义**；二是转义要求两端各实现一遍并逐字节一致，一边漏字符坏的就是整页清单的字段
/// 对齐（错位比丢一行难查得多）。行内的 `\n` 是行分隔符、无法在这个口径里表达，由 Kotlin 侧在拼行前替换为空格。
/// 解析不出来的行直接跳过：`rows` 只有一个生产者（`AlbumProvider`），数字段全来自 MediaStore 整型列，坏行是
/// 代码错误而不是数据问题。未绑定 TCP 时引擎自己返回 false 并留下可读错误，这里不另判一次。
#[no_mangle]
pub extern "system" fn Java_com_linkx_app_NativeCore_nativeSendAlbumList(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
    page: jint,
    total: jint,
    error: JString<'_>,
    rows: JString<'_>,
) -> jint {
    let sent = catch_unwind(AssertUnwindSafe(|| -> bool {
        let Ok(error) = env.get_string(&error) else {
            return false;
        };
        let Ok(rows) = env.get_string(&rows) else {
            return false;
        };
        let list = AlbumList {
            items: parse_album_rows(&rows.to_string_lossy()),
            page: if page < 0 { 0 } else { page as u32 },
            total: if total < 0 { 0 } else { total as u32 },
            error: error.into(),
        };
        with_engine(handle, |e| e.send_album_list(&list, now_ms())).unwrap_or(false)
    }))
    .unwrap_or(false);
    sent as c_int
}

/// 相册：发送 ALBUM_THUMB（一张缩略图，JPEG 随帧走）。1 = 已入队。`error` 非空 = 这张没生成出来，
/// `jpeg` 会被丢弃后再发（半张图 + 一句原因比空白格子更难归因）。缩略图只在内存里生成、直接发出，**不落盘**。
#[no_mangle]
pub extern "system" fn Java_com_linkx_app_NativeCore_nativeSendAlbumThumb(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
    id: jlong,
    edge: jint,
    width: jint,
    height: jint,
    jpeg: JByteArray<'_>,
    error: JString<'_>,
) -> jint {
    let sent = catch_unwind(AssertUnwindSafe(|| -> bool {
        let Ok(jpeg) = env.convert_byte_array(&jpeg) else {
            return false;
        };
        let err: String = env.get_string(&error).map(|s| s.into()).unwrap_or_default();
        let body = AlbumThumb {
            id: if id < 0 { 0 } else { id as u64 },
            edge: if edge < 0 { 0 } else { edge as u32 },
            width: if width < 0 { 0 } else { width as u32 },
            height: if height < 0 { 0 } else { height as u32 },
            jpeg: if err.is_empty() {
                jpeg.into()
            } else {
                Default::default()
            },
            error: err,
        };
        with_engine(handle, |e| e.send_album_thumb(&body, now_ms())).unwrap_or(false)
    }))
    .unwrap_or(false);
    sent as c_int
}

/// 文件：发送 FILE_CHUNK（`data` 为分块原始字节；`crc32` 由平台层计算）
#[no_mangle]
pub extern "system" fn Java_com_linkx_app_NativeCore_nativeSendFileChunk(
    env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
    file_id: jlong,
    index: jint,
    crc32: jint,
    data: JByteArray<'_>,
) -> jint {
    let ok = catch_unwind(AssertUnwindSafe(|| -> bool {
        let Ok(bytes) = env.convert_byte_array(&data) else {
            return false;
        };
        with_engine(handle, |e| {
            e.send_file_chunk(file_id as u64, index as u32, crc32 as u32, &bytes, now_ms())
        })
        .unwrap_or(false)
    }))
    .unwrap_or(false);
    ok as c_int
}

/// 文件：发送 FILE_DONE（`sha256` 为 32B 整文件摘要，长度非 32 视为「本帧不带摘要」）
#[no_mangle]
pub extern "system" fn Java_com_linkx_app_NativeCore_nativeSendFileDone(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
    file_id: jlong,
    ok: jint,
    error: JString<'_>,
    sha256: JByteArray<'_>,
) -> jint {
    let sent = catch_unwind(AssertUnwindSafe(|| -> bool {
        let err: String = env.get_string(&error).map(|s| s.into()).unwrap_or_default();
        let err_ref = if err.is_empty() {
            None
        } else {
            Some(err.as_str())
        };
        let sha: Option<[u8; 32]> = env
            .convert_byte_array(&sha256)
            .unwrap_or_default()
            .try_into()
            .ok();
        with_engine(handle, |e| {
            e.send_file_done_digest(file_id as u64, ok != 0, sha, err_ref, now_ms())
        })
        .unwrap_or(false)
    }))
    .unwrap_or(false);
    sent as c_int
}

/// 文件：发送 MSG_RESUME（请求对端从 `from_index` 续传）
#[no_mangle]
pub extern "system" fn Java_com_linkx_app_NativeCore_nativeSendFileResume(
    _env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
    file_id: jlong,
    from_index: jint,
) -> jint {
    let ok = catch_unwind(AssertUnwindSafe(|| {
        with_engine(handle, |e| {
            e.send_file_resume(file_id as u64, from_index as u32, now_ms())
        })
        .unwrap_or(false)
    }))
    .unwrap_or(false);
    ok as c_int
}

/// 文件：**本端（发送侧）取消**。引擎停发分块、释放通道锁、发 FILE_DONE{cancelled:true}，
/// 并回一条 kind 17 事件；返回 1 = 结束帧已入队。
#[no_mangle]
pub extern "system" fn Java_com_linkx_app_NativeCore_nativeCancelFileSend(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
    file_id: jlong,
    reason: JString<'_>,
) -> jint {
    let sent = catch_unwind(AssertUnwindSafe(|| -> bool {
        let reason: String = env
            .get_string(&reason)
            .map(|s| s.into())
            .unwrap_or_else(|_| "本端取消".to_string());
        with_engine(handle, |e| {
            e.cancel_file_send(file_id as u64, &reason, now_ms())
        })
        .unwrap_or(false)
    }))
    .unwrap_or(false);
    sent as c_int
}

/// 文件：**本端（接收侧）取消**。引擎发 FILE_CANCEL 让对端停手并登记该 id 已取消，
/// 残留在本地的半截文件由 Kotlin 侧删除（IO 在平台层）；返回 1 = 取消帧已入队。
#[no_mangle]
pub extern "system" fn Java_com_linkx_app_NativeCore_nativeCancelFileRecv(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
    file_id: jlong,
    reason: JString<'_>,
) -> jint {
    let sent = catch_unwind(AssertUnwindSafe(|| -> bool {
        let reason: String = env
            .get_string(&reason)
            .map(|s| s.into())
            .unwrap_or_else(|_| "本端取消".to_string());
        with_engine(handle, |e| {
            e.cancel_file_recv(file_id as u64, &reason, now_ms())
        })
        .unwrap_or(false)
    }))
    .unwrap_or(false);
    sent as c_int
}

/// 文件：取出入站分块（重复 `u32 rec_len | u64 file_id | u32 index | u32 crc32 | u32 data_len | data`）。
/// 大载荷走独立通道（不走 u16 事件流），Kotlin 侧解析后按偏移落盘。
#[no_mangle]
pub extern "system" fn Java_com_linkx_app_NativeCore_nativeTakeChunks(
    env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
) -> jbyteArray {
    encode_chunks(&env, handle, None)
}

/// 文件：只取出属于 `file_id` 的入站分块，其余留在引擎队列里。收尾一条传输时用这个：整队取走会把
/// 后一条的分块在它自己的 FILE_META 之前喂给平台层，那些分块找不到会话只能丢。
#[no_mangle]
pub extern "system" fn Java_com_linkx_app_NativeCore_nativeTakeChunksFor(
    env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
    file_id: jlong,
) -> jbyteArray {
    encode_chunks(&env, handle, Some(file_id as u64))
}

fn encode_chunks(env: &JNIEnv<'_>, handle: jlong, only: Option<u64>) -> jbyteArray {
    let buf = catch_unwind(AssertUnwindSafe(|| {
        let mut out = Vec::new();
        let _ = with_engine(handle, |e| {
            let chunks = match only {
                None => e.take_chunks(),
                Some(id) => e.take_chunks_for(id),
            };
            for c in chunks {
                let rec_len = 8 + 4 + 4 + 4 + c.data.len();
                out.extend_from_slice(&(rec_len as u32).to_be_bytes());
                out.extend_from_slice(&c.file_id.to_be_bytes());
                out.extend_from_slice(&c.index.to_be_bytes());
                out.extend_from_slice(&c.crc32.to_be_bytes());
                out.extend_from_slice(&(c.data.len() as u32).to_be_bytes());
                out.extend_from_slice(&c.data);
            }
        });
        out
    }))
    .unwrap_or_default();
    env.byte_array_from_slice(&buf)
        .map(|a| a.into_raw())
        .unwrap_or(std::ptr::null_mut())
}

/// 配置同步：发送跨端配置项（`keys/values/scopes` 三个等长字符串数组）
#[no_mangle]
pub extern "system" fn Java_com_linkx_app_NativeCore_nativeSendConfig(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
    keys: jni::objects::JObjectArray<'_>,
    values: jni::objects::JObjectArray<'_>,
    scopes: jni::objects::JObjectArray<'_>,
) -> jint {
    let ok = catch_unwind(AssertUnwindSafe(|| -> bool {
        let n = env.get_array_length(&keys).unwrap_or(0);
        let nv = env.get_array_length(&values).unwrap_or(0);
        let ns = env.get_array_length(&scopes).unwrap_or(0);
        if n != nv || n != ns {
            return false; // 三数组必须等长
        }
        let mut entries = Vec::with_capacity(n as usize);
        for i in 0..n {
            let Ok(k) = env.get_object_array_element(&keys, i) else {
                return false;
            };
            let Ok(v) = env.get_object_array_element(&values, i) else {
                return false;
            };
            let Ok(s) = env.get_object_array_element(&scopes, i) else {
                return false;
            };
            let k = JString::from(k);
            let v = JString::from(v);
            let s = JString::from(s);
            let (Ok(k), Ok(v), Ok(s)) =
                (env.get_string(&k), env.get_string(&v), env.get_string(&s))
            else {
                return false;
            };
            entries.push(linkx_session::ConfigEntryItem {
                key: k.into(),
                value: v.into(),
                scope: s.into(),
            });
        }
        with_engine(handle, |e| e.send_config(&entries, now_ms())).unwrap_or(false)
    }))
    .unwrap_or(false);
    ok as c_int
}

/// 设备管理：解绑对端（清 TOFU 信任库并复位会话；平台层须同步清除本地持久化指纹）
#[no_mangle]
pub extern "system" fn Java_com_linkx_app_NativeCore_nativeUnbind(
    _env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
) {
    let _ = catch_unwind(AssertUnwindSafe(|| {
        let _ = with_engine(handle, |e| e.unbind_peer());
    }));
}

// ==== 调试控制面胶水（仅 `agent-debug`）====
//
// 刻意放在本模块内而非独立文件：这样能直接复用私有的 `live_handles()` / `with_engine()`，
// **不为了调试去放宽生产代码的可见性**。与 Windows 侧同一门禁口径：交付构建不启用
// `agent-debug`，这些符号连编译都不参与。全部入口按三条铁律包 `catch_unwind`。

/// 日志目录由 Kotlin 传入（app 私有 `files/Logs`），Rust 不猜平台路径。
#[cfg(feature = "agent-debug")]
fn debug_log_dir() -> &'static Mutex<Option<String>> {
    static REG: OnceLock<Mutex<Option<String>>> = OnceLock::new();
    REG.get_or_init(|| Mutex::new(None))
}

#[cfg(feature = "agent-debug")]
fn dbg_lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// 把所有活引擎的状态合成一份快照发布到 `/state`。含 `sas()`——**这意味着双端配对码一致性可以
/// 纯程序校验，不必读屏幕**，而"用眼睛比数字"正是最容易出假验证的环节。
#[cfg(feature = "agent-debug")]
pub(crate) fn publish_from_engines() {
    let handles: Vec<i64> = dbg_lock(live_handles()).iter().copied().collect();
    let mut engines = Vec::new();
    for h in handles {
        if let Some(v) = with_engine(h, |e| {
            serde_json::json!({
                "state": format!("{:?}", e.state()),
                "paired": e.is_paired(),
                "sas": e.sas().map(|c| format!("{c:06}")),
                "own_fp": e.own_identity_fingerprint(),
                "peer_fp": e.peer_fingerprint().map(|s| s.to_string()),
                "tcp_bound": e.is_tcp_bound(),
                "has_outbound": e.has_outbound(),
                "pair_evidence_verified": e.pair_evidence_verified(),
                "trusted_count": e.trusted_peers().len(),
                // 出站分片实际用的 MTU 与积压片数：这是判 -212（身份交换超时）是否真被修掉的直接证据——只看不发不够，得看见片数掉下来。
                "ble_mtu": e.ble_mtu(),
                "out_pending": e.outbound_pending(),
            })
        }) {
            engines.push(v);
        }
    }
    linkx_debugd::publish(serde_json::json!({
        "platform": "android",
        "engines": engines,
        "debug_enabled": debuglog::is_enabled(),
        // 宿主观测面（剪贴板 / 主题 / 开关）：没有它，"两端的值是否真的一致"仍然只能用眼睛比——那正是最容易出假验证的地方。
        "host": serde_json::Value::Object(
            dbg_lock(debug_fields())
                .iter()
                .map(|(k, v)| (k.clone(), serde_json::Value::String(v.clone())))
                .collect(),
        ),
    }));
}

/// 启动控制面（只绑 127.0.0.1）。返回实际监听地址；失败返回空串，绝不影响主功能。
/// `log_dir` 为 app 私有日志目录；`/debug` 端点据此开关全栈日志。
#[cfg(feature = "agent-debug")]
#[no_mangle]
pub extern "system" fn Java_com_linkx_app_NativeCore_nativeDebugdStart(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    log_dir: JString<'_>,
    port: jint,
) -> jstring {
    let mut bound = String::new();
    let _ = catch_unwind(AssertUnwindSafe(|| {
        if let Ok(s) = env.get_string(&log_dir) {
            let dir = s.to_string_lossy().into_owned();
            if !dir.is_empty() {
                *dbg_lock(debug_log_dir()) = Some(dir);
            }
        }
        linkx_debugd::on_debug_toggle(|on| {
            if !on {
                debuglog::disable();
                return true;
            }
            let guard = dbg_lock(debug_log_dir());
            match guard.as_deref() {
                Some(p) => {
                    let _ = std::fs::create_dir_all(p);
                    debuglog::enable(p).is_ok()
                }
                None => false,
            }
        });
        // 动作只**入队**，绝不在 debugd 的 accept 线程上碰引擎。理由不是"只有 linkx-tick 一个线程
        // 能碰引擎"（那说法是错的，见 `with_engine` 注释），而是 **HTTP 线程不在 Kotlin 侧
        // `@Synchronized` 的保护范围内**：它直调 `with_engine` 就是一个绕过那把锁的裸入口，与
        // Binder / UI / tick 线程并发解引用同一个引擎，即数据竞争 / UAF。所以由
        // `nativeDebugTakeRequest` 在 `LinkxRuntime.pump()`（带 `@Synchronized`）里取走，
        // 再走既有生产方法（sendClipboard / confirmSas / …）。
        linkx_debugd::on_action(|name, query| {
            let mut q = dbg_lock(debug_requests());
            if q.len() >= 32 {
                return Err("动作队列已满，稍后重试".to_string());
            }
            q.push_back((name.to_string(), query.to_string()));
            Ok("已入队，由 linkx-tick 线程执行".to_string())
        });
        if let Ok(addr) = linkx_debugd::start(port.unsigned_abs() as u16) {
            bound = addr.to_string();
        }
    }));
    match env.new_string(bound) {
        Ok(s) => s.into_raw(),
        Err(_) => std::ptr::null_mut(),
    }
}

/// 跨线程信箱：debugd accept 线程 → Kotlin linkx-tick 线程。
#[cfg(feature = "agent-debug")]
fn debug_requests() -> &'static Mutex<VecDeque<(String, String)>> {
    static REG: OnceLock<Mutex<VecDeque<(String, String)>>> = OnceLock::new();
    REG.get_or_init(|| Mutex::new(VecDeque::new()))
}

/// Kotlin 侧回传的只读观测字段（剪贴板内容、主题、开关状态……）。走「宿主填、控制面转发」而不是
/// 给 debugd 加平台语义：控制面必须与平台无关，否则 Windows/安卓两套 `/state` 会长出不一致字段面。
#[cfg(feature = "agent-debug")]
fn debug_fields() -> &'static Mutex<BTreeMap<String, String>> {
    static REG: OnceLock<Mutex<BTreeMap<String, String>>> = OnceLock::new();
    REG.get_or_init(|| Mutex::new(BTreeMap::new()))
}

/// tick 线程取走一个待执行动作；无待办返回空串。返回格式 `name\t<query>`：query 保持原始
/// `k=v&k=v` 由 Kotlin 自己解，免得 Rust 侧替平台决定参数语义。
#[cfg(feature = "agent-debug")]
#[no_mangle]
pub extern "system" fn Java_com_linkx_app_NativeCore_nativeDebugTakeRequest(
    env: JNIEnv<'_>,
    _class: JClass<'_>,
) -> jstring {
    let s = catch_unwind(AssertUnwindSafe(|| {
        dbg_lock(debug_requests())
            .pop_front()
            .map(|(n, q)| format!("{n}\t{q}"))
            .unwrap_or_default()
    }))
    .unwrap_or_default();
    match env.new_string(s) {
        Ok(v) => v.into_raw(),
        Err(_) => std::ptr::null_mut(),
    }
}

/// 宿主把一项观测值并入 `/state`（同名覆盖）。
#[cfg(feature = "agent-debug")]
#[no_mangle]
pub extern "system" fn Java_com_linkx_app_NativeCore_nativeDebugSetField(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    key: JString<'_>,
    value: JString<'_>,
) {
    let _ = catch_unwind(AssertUnwindSafe(|| {
        let Ok(k) = env.get_string(&key) else {
            return;
        };
        let Ok(v) = env.get_string(&value) else {
            return;
        };
        let k = k.to_string_lossy().into_owned();
        let v = v.to_string_lossy().into_owned();
        if !k.is_empty() {
            dbg_lock(debug_fields()).insert(k, v);
        }
    }));
}

/// Kotlin 回报计数（BLE notify 发了几片、丢了几片等）：分片是否真的发出去、`onNotificationSent`
/// 回了什么状态只有 Kotlin 侧知道。负数 delta 表示"水位"（只在更大时覆盖），用于延迟类指标。
#[cfg(feature = "agent-debug")]
#[no_mangle]
pub extern "system" fn Java_com_linkx_app_NativeCore_nativeDebugCounter(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    name: JString<'_>,
    delta: jlong,
) {
    let _ = catch_unwind(AssertUnwindSafe(|| {
        let Ok(s) = env.get_string(&name) else {
            return;
        };
        let key = s.to_string_lossy().into_owned();
        if key.is_empty() {
            return;
        }
        if delta < 0 {
            linkx_debugd::bump_max(&key, delta.unsigned_abs());
        } else {
            linkx_debugd::bump(&key, delta as u64);
        }
    }));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 相册清单的行口径：从右往左取数字段，所以**文件名里的 `|` 能原样往返**。
    /// 这条规则错了不崩、只是字段错位，真机上极难归因，因此在编译期之外钉一次。
    #[test]
    fn album_rows_split_from_right_keep_pipes_in_name() {
        let rows = "77|IMG_0001.jpg|4000000|1700000000000|4000|3000|0|0\n\
                    78|a|b|c.jpg|10|20|3|4|0|0\n\
                    80|VID_2026|clip.mp4|88000000|1700000100000|1920|1080|1|23450\n\
                    79|坏行不该出现|oops\n\
                    \n";
        let items = parse_album_rows(rows);
        assert_eq!(items.len(), 3, "字段不齐的行只能跳过，不能错位");
        assert_eq!(items[0].id, 77);
        assert_eq!(items[0].name, "IMG_0001.jpg");
        assert_eq!((items[0].width, items[0].height), (4000, 3000));
        assert_eq!(
            (items[0].kind, items[0].duration_ms),
            (0, 0),
            "照片没有时长"
        );
        assert_eq!(items[1].id, 78);
        assert_eq!(items[1].name, "a|b|c.jpg", "名字里的竖线必须整段留下");
        assert_eq!(items[1].size_bytes, 10);
        assert_eq!((items[1].width, items[1].height), (3, 4));
        // 视频：名字带竖线 + kind=1 + 时长，三段同时成立才算口径没被字段数变化打乱
        assert_eq!(items[2].id, 80);
        assert_eq!(items[2].name, "VID_2026|clip.mp4");
        assert_eq!((items[2].kind, items[2].duration_ms), (1, 23450));
    }

    /// 反向用例：长度超 u16 → 显式失败，不静默截断
    #[test]
    fn u16_length_overflow_is_rejected_not_truncated() {
        let mut out = Vec::new();
        assert!(put_u16(&mut out, 0x1_0000).is_err());
        assert!(out.is_empty(), "失败时不得写入任何字节");
        assert!(put_u16(&mut out, u16::MAX as usize).is_ok());
        assert_eq!(out, vec![0xFF, 0xFF]);
    }

    /// 字符串字段在编码前被截断到安全上限，且落在 UTF-8 字符边界
    #[test]
    fn long_string_field_truncated_on_char_boundary() {
        let long = "中".repeat(200_000); // 远超 MAX_FIELD_BYTES
        let mut out = Vec::new();
        put_str(&mut out, &long);
        let len = u16::from_be_bytes([out[0], out[1]]) as usize;
        assert_eq!(len, out.len() - 2);
        assert!(len <= MAX_FIELD_BYTES);
        assert!(std::str::from_utf8(&out[2..]).is_ok());
    }

    /// 超大事件也必须能安全编码：每字段截断后总长恒在 u16 容量内，流保持对齐（Kotlin 不会读到错长度）。
    #[test]
    fn oversized_event_still_encodes_within_u16_bounds() {
        let huge = EngineEvent::Notification {
            package: "a".repeat(MAX_FIELD_BYTES * 2),
            title: "b".repeat(MAX_FIELD_BYTES * 2),
            text: "c".repeat(MAX_FIELD_BYTES * 2),
            post_ts_ms: 1_790_000_000_000,
            key_hash: 0x1234_5678,
            // 回复定位字段给真实尺寸：本例证的是"三个正文字段同时超长也不能撑爆 u16 长度域"，
            // 再叠两段超长标识串就变成另一件事（那种事件整体被丢弃，见下一例）
            tag: "chat".into(),
            notification_id: 7,
            can_reply: true,
            reply_action_index: 0,
            reply_result_key: "key_reply".into(),
        };
        let mut out = Vec::new();
        encode_event(&mut out, &huge);
        let ev_len = u16::from_be_bytes([out[0], out[1]]) as usize;
        assert_eq!(ev_len, out.len() - 2, "长度域必须与实际负载一致");
        assert!(ev_len <= u16::MAX as usize);
        assert_eq!(out[2], 5);
        let mut off = 3 + 8 + 4;
        for _ in 0..3 {
            let n = u16::from_be_bytes([out[off], out[off + 1]]) as usize;
            off += 2 + n;
            assert!(off <= out.len());
        }
        assert_eq!(off, out.len(), "三个字段恰好铺满负载（无多余/缺失字节）");
    }

    /// kind 21 的字段序是 Kotlin 侧 `decodeEvent` 的契约，改一个字节就要两端一起改
    #[test]
    fn notify_reply_request_event_layout_matches_contract() {
        let ev = EngineEvent::NotifyReplyRequested {
            reply_id: 3,
            package: "org.telegram.messenger".into(),
            tag: "t".into(),
            notification_id: 77,
            action_index: 1,
            result_key: "key_reply_text".into(),
            text: "马上到".into(),
        };
        let mut out = Vec::new();
        encode_event(&mut out, &ev);
        assert_eq!(u16::from_be_bytes([out[0], out[1]]) as usize, out.len() - 2);
        assert_eq!(out[2], 21);
        let mut off = 3;
        let take = |n: usize, off: &mut usize| {
            let s = out[*off..*off + n].to_vec();
            *off += n;
            s
        };
        assert_eq!(take(4, &mut off), 3u32.to_be_bytes());
        assert_eq!(take(4, &mut off), 77i32.to_be_bytes());
        assert_eq!(take(4, &mut off), 1i32.to_be_bytes());
        for expect in ["org.telegram.messenger", "t", "key_reply_text", "马上到"] {
            let n = u16::from_be_bytes([out[off], out[off + 1]]) as usize;
            off += 2;
            assert_eq!(String::from_utf8(take(n, &mut off)).unwrap(), expect);
        }
        assert_eq!(off, out.len());
    }

    /// 回执是手机自己发出去的，永远不该出现在下行事件流里
    #[test]
    fn notify_reply_ack_is_not_encoded_into_the_event_stream() {
        let mut out = Vec::new();
        encode_event(
            &mut out,
            &EngineEvent::NotifyReplyAck {
                reply_id: 3,
                package: "p".into(),
                ok: true,
                error: String::new(),
            },
        );
        assert!(out.is_empty());
    }

    /// 编码器遇超限负载时丢弃整条事件（防御性，保证流对齐）
    #[test]
    fn oversized_payload_writes_no_partial_frame() {
        let mut out = Vec::new();
        assert!(put_u16(&mut out, usize::from(u16::MAX) + 1).is_err());
        assert!(out.is_empty());
    }

    /// 反向用例：未登记 / 已释放 / 伪造句柄一律拒绝，不解引用
    #[test]
    fn unregistered_and_freed_handles_are_rejected() {
        assert!(with_engine(0xDEAD_BEEF, |_| ()).is_none());
        assert!(with_engine(-1, |_| ()).is_none());
        assert!(with_engine(0, |_| ()).is_none());

        // 登记 → 可用；释放 → 不可用。身份/信任本应经 `with_identity_der`/`with_trusted_peers`
        // 注入，这里只验证句柄表语义，故用无身份的默认配置（仅测试路径）。
        let cfg = EngineConfig::new(
            EngineRole::Responder,
            "t",
            linkx_protocol::OS_ANDROID,
            "0.1.0",
            [0x11; 32],
        );
        let raw = Box::into_raw(Box::new(SessionEngine::new(cfg))) as i64;
        live_handles().lock().unwrap().insert(raw);
        assert!(with_engine(raw, |e| e.state()).is_some());

        assert!(live_handles().lock().unwrap().remove(&raw));
        unsafe { drop(Box::from_raw(raw as *mut SessionEngine)) };
        assert!(
            with_engine(raw, |e| e.state()).is_none(),
            "已释放句柄必须被拒（防 UAF）"
        );
        assert!(!live_handles().lock().unwrap().remove(&raw));
    }

    /// kind 13（配置同步）条目数超 u16 时必须**整条丢弃**。只写 kind 字节而不写计数体，
    /// Kotlin 会把下一条事件的字节当作这条的字段读，整条事件流从此错位——这类错位不崩，
    /// 只是每个后续事件都长错样子，真机上极难归因。
    #[test]
    fn oversized_config_event_is_dropped_whole_not_half_written() {
        let entries = (0..(usize::from(u16::MAX) + 1))
            .map(|i| linkx_session::engine::ConfigEntryItem {
                key: format!("k{i}"),
                value: String::new(),
                scope: "cross".into(),
            })
            .collect();
        let mut out = Vec::new();
        encode_event(&mut out, &EngineEvent::ConfigReceived { entries });
        assert!(
            out.is_empty(),
            "超限的配置事件一个字节都不该写进流：{out:?}"
        );

        // 紧接着的一条正常事件必须从头就完整（证明上一条没留下半截头）
        let mut out2 = Vec::new();
        encode_event(&mut out2, &EngineEvent::TcpBound);
        assert_eq!(out2, vec![0x00, 0x01, 11], "后续事件应保持自身格式");
    }
}
