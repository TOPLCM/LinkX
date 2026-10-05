//! LinkX Core 自检入口（真机联调前代替 UI 的冒烟测试）
//!
//! 运行：cargo run -p linkx-app（或 cargo run）
//! 输出：LinkX Core sanity —— N/N PASS，exit code 0/1。

use linkx_crypto::cipher::{decrypt_payload, encrypt_payload};
use linkx_crypto::noise::run_xx_pair;
use linkx_crypto::{derive_session_key, fingerprint, sas_digits};
use linkx_lan::transport::{BleUuid, FrameChannel, TcpStreamLink, TRANSPORT_TCP_PORT};
use linkx_protocol::ble_frag::{split_into_packets, BleReassembler, MTU_DEFAULT};
use linkx_protocol::frame::{assemble_frame, FrameHeader};
use linkx_protocol::msg_type;
use linkx_protocol::tlv_codec;
use linkx_protocol::{MSG_HELLO, OS_ANDROID};
use linkx_session::heartbeat::{is_ping, is_pong, ping_payload, pong_payload, Backoff};
use linkx_session::state::{
    SessionChannel, SessionEvent, SessionManager, SessionState, TofuVerdict,
};
use linkx_storage::LinkxStore;
use prost::Message;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

fn main() {
    let t0 = Instant::now();
    println!("== LinkX Core sanity ==");
    let mut passed = 0u32;
    let mut total = 0u32;

    macro_rules! run {
        ($name:expr, $body:expr) => {{
            total += 1;
            let ok = $body;
            if ok {
                passed += 1;
            }
            println!("  [{}] {}", if ok { "PASS" } else { "FAIL" }, $name);
        }};
    }

    // 1. 帧头 13B 编解码
    run!("frame header roundtrip", {
        let h = FrameHeader::default_encrypted(msg_type::CLIPBOARD_PUSH, 42, 1024);
        FrameHeader::parse(&h.encode()).is_ok_and(|x| x == h)
    });

    // 2. BLE 分片重组（MTU=23 预算）
    run!("BLE frag MTU=23 roundtrip 1KB", {
        let hdr = FrameHeader::default_encrypted(msg_type::NOTIFY_PUSH, 3, 1024);
        let mut body = vec![0x5A; 1024];
        body.extend_from_slice(&[0u8; 16]); // +16B AEAD tag
        let frame = assemble_frame(&hdr, &body).unwrap();
        let pkts =
            split_into_packets(0x77, MTU_DEFAULT, &frame).expect("分片失败：返回 Err 而非 panic");
        let mut r = BleReassembler::new();
        let t = Instant::now();
        let mut ok = pkts.len() == frame.len().div_ceil(14);
        for p in &pkts {
            if let Some(full) = r.feed(t, p).unwrap_or(None) {
                ok &= full == frame;
            }
        }
        ok && r.pending_count() == 0
    });

    // 3. ChaCha20-Poly1305 加解密（方向标识 + 帧头作 AAD）
    run!("crypto encrypt/decrypt", {
        let key = [0x11; 32];
        let aad = b"frame-header\0";
        let ct = encrypt_payload(
            &key,
            linkx_crypto::cipher::DIR_INITIATOR_TO_RESPONDER,
            b"session1",
            1,
            aad,
            b"hello",
        )
        .unwrap();
        decrypt_payload(
            &key,
            linkx_crypto::cipher::DIR_INITIATOR_TO_RESPONDER,
            b"session1",
            1,
            aad,
            &ct,
        )
        .is_ok_and(|p| p == b"hello")
    });

    // 4. Noise XX 双端握手（两端得到同一哈希/SAS/会话密钥）
    run!("Noise XX pair derives shared secrets", {
        let (ih, rh, irs, rrs) = run_xx_pair(&[0xAA; 32], &[0xBB; 32]).unwrap();
        ih == rh
            && derive_session_key(&ih) == derive_session_key(&rh)
            && sas_digits(&ih) == sas_digits(&rh)
            && fingerprint(&irs).len() == 16
            && fingerprint(&rrs).len() == 16
    });

    // 5. 会话状态机（首次配对路径）
    run!("session state machine first-pair", {
        let mut sm = SessionManager::new(SessionChannel::Ble);
        sm.transition(SessionEvent::PeerDiscovered).is_ok()
            && sm
                .transition(SessionEvent::HandshakeComplete {
                    tofu: TofuVerdict::NewPeer,
                })
                .is_ok()
            && sm.transition(SessionEvent::SasConfirmed).is_ok()
            && sm
                .transition(SessionEvent::SasConfirmed)
                .is_ok_and(|s| s == SessionState::Paired)
    });

    // 6. 心跳退避
    run!("heartbeat backoff 1..32s x6", {
        let mut b = Backoff::default();
        let mut sum = 0u64;
        while let Some(d) = b.next_delay() {
            sum += d.as_secs();
        }
        sum == 63 && b.exhausted()
    });
    run!("ping/pong TLV payload", {
        is_ping(&ping_payload()) && is_pong(&pong_payload()) && !is_ping(&pong_payload())
    });

    // 7. TLV 与发现信标
    run!("discovery beacon TLV", {
        let b = linkx_lan::DiscoveryBeacon {
            advert_name: "pc-home".into(),
            os: OS_ANDROID,
            version: "0.1.0".into(),
        };
        linkx_lan::DiscoveryBeacon::decode(&b.encode().unwrap()).is_ok_and(|x| x == b)
    });

    // 8. protobuf 生成（编译期）
    run!("prost protobuf types", {
        use linkx_protocol::pb::NotificationPush;
        let n = NotificationPush {
            package: "com.example".into(),
            title: "t".into(),
            text: String::new(),
            post_ts_ms: 0,
            key_hash: 0,
            cover_jpeg: Default::default(),
            tag: String::new(),
            notification_id: 0,
            can_reply: false,
            reply_action_index: 0,
            reply_result_key: String::new(),
        };
        let mut buf = Vec::new();
        <NotificationPush as Message>::encode(&n, &mut buf).is_ok() && !buf.is_empty()
    });

    // 9. 本地存储 TOFU 持久化
    run!("storage TOFU persist", {
        let store = LinkxStore::open_in_memory().unwrap();
        store
            .upsert_pair(&[0x0F; 8], "aabbccddeeff0011", "pc", now_ms())
            .unwrap();
        store.get_pair_fingerprint(&[0x0F; 8]).unwrap() == Some("aabbccddeeff0011".into())
    });

    // 10. FFI 事件队列 + C ABI
    run!("ffi tx queue + C ABI", {
        let ctx = linkx_core::linkx_ffi_ctx_new();
        let mut ok = !ctx.is_null();
        ok &= linkx_core::linkx_ffi_emit_error(ctx, -213) == 0;
        let mut ev = unsafe { std::mem::zeroed::<linkx_core::FfiEvent>() };
        ok &= unsafe { linkx_core::linkx_ffi_poll_event(ctx, &mut ev) } == 1;
        ok &= ev.code == -213;
        unsafe { linkx_core::linkx_ffi_ctx_free(ctx) };
        ok
    });

    // 11. TCP 帧流（本地环回冒烟）
    run!("tcp frame channel loopback", {
        let listener = TcpStreamLink::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let srv = std::thread::spawn(move || {
            let (s, _) = listener.accept().unwrap();
            TcpStreamLink::from_stream(s).recv_frame().unwrap()
        });
        let mut c = TcpStreamLink::connect(&addr).unwrap();
        let hdr = FrameHeader::default_encrypted(msg_type::CLIPBOARD_PUSH, 1, 4);
        let mut body = vec![1, 2, 3, 4];
        body.extend_from_slice(&[0xEE; 16]); // 密文 + 16B AEAD tag
        c.send_frame(hdr, &body).unwrap();
        let (rh, rb) = srv.join().unwrap();
        rh == hdr && rb == body
    });

    // 12. 常量一致性（端口 / BLE UUID 骨架）
    run!("constants aligned", {
        TRANSPORT_TCP_PORT == 55676
            && BleUuid::SERVICE.starts_with("4c584c00")
            && msg_type::HEARTBEAT == 0x70
            && MSG_HELLO == 0x01
            && tlv_codec::TLV_MAX_MSG == 64
    });

    // 13. 抗重放滑动窗口（窗口 64 / 乱序 ±16）
    run!("anti-replay sliding window", {
        use linkx_crypto::replay::ReplayWindow;
        let mut w = ReplayWindow::default();
        let mut ok = w.check_and_update(100).is_ok();
        ok &= w.check_and_update(84).is_ok(); // 乱序 ±16 边界内 → 接受
        ok &= w.check_and_update(84).is_err(); // 重复 → 重放拒绝
        ok &= w.check_and_update(50).is_err(); // 落后 > 16 → 过旧丢弃
        ok
    });

    // 14. 版本单一真源（FFI C 字符串 == Rust 常量 == 跨端 versionName）
    run!("version single source", {
        let c = unsafe { std::ffi::CStr::from_ptr(linkx_core::linkx_version()) };
        c.to_str().is_ok_and(|s| s == linkx_core::LINKX_FFI_VERSION)
            && linkx_core::LINKX_FFI_VERSION == env!("CARGO_PKG_VERSION")
    });

    let elapsed = t0.elapsed();
    println!("== {}/{} PASS in {:.1?} ==", passed, total, elapsed);
    if passed != total {
        std::process::exit(1);
    }
    println!(
        "LinkX Core sanity OK (ffi v{})",
        linkx_core::LINKX_FFI_VERSION
    );
}
