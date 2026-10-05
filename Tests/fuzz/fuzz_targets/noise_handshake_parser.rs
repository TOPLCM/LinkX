//! 6.2/6.5 fuzz 目标 3：Noise 握手消息解析零崩溃（read_message）
//! 注：snow 无状态恢复，单次 fuzz 用固定种子握手对象逐个消化输入。
#![no_main]
use libfuzzer_sys::fuzz_target;
use linkx_crypto::noise::{NoiseXxHandshake, Role};

fuzz_target!(|data: &[u8]| {
    if let Ok(mut hs) = NoiseXxHandshake::new(Role::Responder, Some(&[0u8; 32])) {
        let _ = hs.read_message(data);
    }
});