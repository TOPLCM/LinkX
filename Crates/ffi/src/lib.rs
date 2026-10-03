//! linkx_core C ABI（导出为 `linkx_core.{dll,so}`）。
//! FFI 三条铁律：① 事件只经 TxQueue 单向流出，UI 独立 pump 线程 poll，不许跨线程回调；② 谁分配
//! 谁释放——句柄由 Rust 分配回收，poll 写入的上下文缓冲由调用方预分配，ffi 只写不分配；③ 不跨
//! 边界抛异常——判空指针后 `catch_unwind`（交付构建全 profile `panic = "abort"`：包装不生效，abort 强过 panic 穿 FFI 变 UB）。

mod events;
/// Android JNI 导出（feature="jni"，构建 linkx_core.so 时启用）
#[cfg(feature = "jni")]
mod jni_bridge;

pub use events::{TxEvent, TxQueue};

use debuglog::Level;
use std::os::raw::{c_char, c_int, c_void};
use std::panic::{catch_unwind, AssertUnwindSafe};

/// Core 版本（单一真源：Cargo.toml `version`），与 Android `versionName` / Windows 版本条同源。
pub const LINKX_FFI_VERSION: &str = env!("CARGO_PKG_VERSION");
pub const FFI_CTX_CONTEXT_CAP: usize = 1024;

/// C 侧可读版本字符串（NUL 结尾），与 `LINKX_FFI_VERSION` 严格同源
const VERSION_CSTR_BYTES: &[u8] = concat!(env!("CARGO_PKG_VERSION"), "\0").as_bytes();

/// C 侧事件结构（repr(C)，UI 端 match 分派；context 固定缓冲，超长截断）。`context` 语义按 `event_kind`：
/// 1 = 1B state；2 = payload；3 = 错误文本；4 = 8B 大端 i64 毫秒时间戳（`code` 恒为 0）；5 = 无。
#[repr(C)]
pub struct FfiEvent {
    pub event_kind: c_int,
    pub peer_id: [u8; 8],
    pub msg_type: u8,
    pub seq: u32,
    pub code: c_int,
    pub context_len: u32,
    pub context: [u8; FFI_CTX_CONTEXT_CAP],
}

pub mod rc {
    use std::os::raw::c_int;
    pub const OK: c_int = 0;
    pub const EVT: c_int = 1; // poll 有事件
    pub const NONE: c_int = 2; // 无事件
    pub const PANIC: c_int = -2; // 捕获 panic
    pub const INVALID: c_int = -1;
}

pub struct FfiCtx {
    pub queue: TxQueue,
}

fn fill_fixture(ev: &TxEvent, out: &mut FfiEvent) {
    *out = FfiEvent {
        event_kind: 0,
        peer_id: [0u8; 8],
        msg_type: 0,
        seq: 0,
        code: 0,
        context_len: 0,
        context: [0u8; FFI_CTX_CONTEXT_CAP],
    };
    match ev {
        TxEvent::ConnectionStateChanged { peer_id, state } => {
            out.event_kind = 1;
            copy_peer(out, peer_id);
            out.context[0] = *state;
            out.context_len = 1;
        }
        TxEvent::MessageReceived {
            peer_id,
            msg_type,
            seq,
            payload,
        } => {
            out.event_kind = 2;
            copy_peer(out, peer_id);
            out.msg_type = *msg_type;
            out.seq = *seq;
            set_context(out, payload);
        }
        TxEvent::ErrorReported { code, context } => {
            out.event_kind = 3;
            out.code = *code;
            if let Some(c) = context {
                set_context(out, c.as_bytes());
            }
        }
        TxEvent::SessionKeyRotated { rotated_at_ms } => {
            out.event_kind = 4;
            // i64 毫秒时间戳超出 c_int 位宽：完整值以 8B 大端写入 context，`code` 不承载时间戳。
            set_context(out, &rotated_at_ms.to_be_bytes());
        }
        TxEvent::SessionTerminated { reason } => {
            out.event_kind = 5;
            out.code = *reason;
        }
    }
}

fn copy_peer(out: &mut FfiEvent, peer_id: &[u8]) {
    let n = peer_id.len().min(8);
    out.peer_id[..n].copy_from_slice(&peer_id[..n]);
}

fn set_context(out: &mut FfiEvent, data: &[u8]) {
    let n = data.len().min(FFI_CTX_CONTEXT_CAP);
    out.context[..n].copy_from_slice(&data[..n]);
    out.context_len = n as u32;
}

/// 版本号（UI 端展示/诊断）；返回指向 `LINKX_FFI_VERSION` 同源 NUL 结尾字节的静态指针
#[no_mangle]
pub extern "C" fn linkx_version() -> *const c_char {
    VERSION_CSTR_BYTES.as_ptr() as *const c_char
}

#[no_mangle]
pub extern "C" fn linkx_ffi_ctx_new() -> *mut c_void {
    catch_unwind(|| -> *mut c_void {
        let ctx = Box::new(FfiCtx {
            queue: TxQueue::new(),
        });
        debuglog::log!(Level::Info, "ffi", "ctx_new", &[]);
        Box::into_raw(ctx) as *mut c_void
    })
    .unwrap_or(std::ptr::null_mut())
}

/// 释放句柄（谁分配谁释放：Rust 侧 `Box::from_raw`）。
/// # Safety
/// `ctx` 必须来自 [`linkx_ffi_ctx_new`]，且只能释放一次。
#[no_mangle]
pub unsafe extern "C" fn linkx_ffi_ctx_free(ctx: *mut c_void) {
    if ctx.is_null() {
        return;
    }
    debuglog::log!(Level::Info, "ffi", "ctx_free", &[]);
    let _ = unsafe { Box::from_raw(ctx as *mut FfiCtx) };
}

/// 注入错误类事件（Core 诊断/测试用）
#[no_mangle]
pub extern "C" fn linkx_ffi_emit_error(ctx: *mut c_void, code: c_int) -> c_int {
    if ctx.is_null() {
        return rc::INVALID;
    }
    catch_unwind(AssertUnwindSafe(|| {
        let ctx = unsafe { &*(ctx as *const FfiCtx) };
        debuglog::log!(
            Level::Warn,
            "ffi",
            "emit_error",
            &[("code", &code.to_string())]
        );
        let ok = ctx.queue.emit(TxEvent::ErrorReported {
            code,
            context: Some("ffi-emit".into()),
        });
        if ok {
            rc::OK
        } else {
            rc::INVALID
        }
    }))
    .unwrap_or(rc::PANIC)
}

/// poll 事件：写入 C 侧 FfiEvent，返回 rc::EVT / NONE / INVALID / PANIC。
/// # Safety
/// `ctx` 必须是由本模块分配的有效句柄；`out` 必须指向一块可写的 [`FfiEvent`]。
#[no_mangle]
pub unsafe extern "C" fn linkx_ffi_poll_event(ctx: *mut c_void, out: *mut FfiEvent) -> c_int {
    if ctx.is_null() || out.is_null() {
        debuglog::log!(Level::Warn, "ffi", "poll_invalid", &[]);
        return rc::INVALID;
    }
    catch_unwind(AssertUnwindSafe(|| {
        let ctx = unsafe { &*(ctx as *const FfiCtx) };
        let out = unsafe { &mut *out };
        match ctx.queue.poll() {
            Some(ev) => {
                fill_fixture(&ev, out);
                rc::EVT
            }
            None => rc::NONE,
        }
    }))
    .unwrap_or(rc::PANIC)
}

// ---- C ABI 主体（Windows 壳直调 Rust；Android 见 jni_bridge） ----
#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::CStr;

    #[test]
    fn version_cstring_matches_const() {
        unsafe {
            assert!(!linkx_version().is_null());
            let s = CStr::from_ptr(linkx_version()).to_str().unwrap();
            assert_eq!(s, LINKX_FFI_VERSION);
            assert_eq!(LINKX_FFI_VERSION, env!("CARGO_PKG_VERSION"));
        }
    }

    #[test]
    fn session_key_rotated_keeps_full_i64() {
        // 2026 年毫秒时间戳 > i32::MAX：必须完整保留，不得截断进 code
        let ms: i64 = 1_790_000_000_000;
        let ev = TxEvent::SessionKeyRotated { rotated_at_ms: ms };
        let mut f = unsafe { std::mem::zeroed::<FfiEvent>() };
        fill_fixture(&ev, &mut f);
        assert_eq!(f.event_kind, 4);
        assert_eq!(f.code, 0, "code 不再承载被截断的时间戳");
        assert_eq!(f.context_len, 8);
        let mut b = [0u8; 8];
        b.copy_from_slice(&f.context[..8]);
        assert_eq!(i64::from_be_bytes(b), ms);
    }

    #[test]
    fn ctx_emit_and_poll_via_c_abi() {
        unsafe {
            let ctx = linkx_ffi_ctx_new();
            assert!(!ctx.is_null());
            assert_eq!(linkx_ffi_emit_error(ctx, -213), rc::OK);
            let mut ev = std::mem::zeroed::<FfiEvent>();
            let r = linkx_ffi_poll_event(ctx, &mut ev);
            assert_eq!(r, rc::EVT);
            assert_eq!(ev.event_kind, 3);
            assert_eq!(ev.code, -213);
            let r2 = linkx_ffi_poll_event(ctx, &mut ev);
            assert_eq!(r2, rc::NONE);
            linkx_ffi_ctx_free(ctx);
        }
    }

    #[test]
    fn null_pointers_rejected() {
        unsafe {
            assert_eq!(
                linkx_ffi_poll_event(std::ptr::null_mut(), std::ptr::null_mut()),
                rc::INVALID
            );
            assert_eq!(linkx_ffi_emit_error(std::ptr::null_mut(), 0), rc::INVALID);
            linkx_ffi_ctx_free(std::ptr::null_mut()); // 不崩溃
        }
    }

    #[test]
    fn internal_queue_roundtrip() {
        let q = TxQueue::new();
        q.emit(TxEvent::ConnectionStateChanged {
            peer_id: vec![1, 2],
            state: 4,
        });
        let ev = q.poll().unwrap();
        let mut f = unsafe { std::mem::zeroed::<FfiEvent>() };
        fill_fixture(&ev, &mut f);
        assert_eq!(f.event_kind, 1);
        assert_eq!(f.peer_id[0], 1);
        assert_eq!(f.context[0], 4);
    }
}
