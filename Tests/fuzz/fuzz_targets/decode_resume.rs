//! fuzz 目标 5：断点续传请求（MSG_RESUME）的 TLV 解码对任意输入不崩溃。
//! 这条路径吃的是链路上收到的字节：解码必须要么给出一个自洽的 (file_id, from_index)，
//! 要么明确报错，绝不能 panic，也绝不能把半个请求当成请求。
#![no_main]
use libfuzzer_sys::fuzz_target;
use linkx_transfer::protocol::{decode_resume, encode_resume};

fuzz_target!(|data: &[u8]| {
    let _ = decode_resume(data);
    // 自己编一条回去必须能原样解出来
    if data.len() >= 12 {
        let file_id = u64::from_be_bytes(data[..8].try_into().unwrap());
        let from_index = u32::from_be_bytes(data[8..12].try_into().unwrap());
        let back = decode_resume(&encode_resume(file_id, from_index)).unwrap();
        assert_eq!(back, (file_id, from_index), "RESUME 编解码不自洽");
    }
});
