//! fuzz 目标 5：断点续传请求（MSG_RESUME）的 TLV 解码对任意输入不崩溃。
//! 这条路径吃的是链路上收到的字节：解码必须要么给出一个自洽的 (file_id, from_index)，
//! 要么明确报错，绝不能 panic，也绝不能把半个请求当成请求。
//! `corpus/decode_resume/` 里放了刻意的种子（空、11 字节差一个、13 字节多一个、全零、全 F），
//! 12 字节这条边界靠覆盖率引导长不出来。
#![no_main]
use libfuzzer_sys::fuzz_target;
use linkx_transfer::protocol::{decode_resume, encode_resume};

fuzz_target!(|data: &[u8]| {
    // "不 panic" 本身就是断言：libFuzzer 会把任何 panic 报成失败
    if let Ok((file_id, from_index)) = decode_resume(data) {
        // 解出来的两个字段再编回去必须能原样解出来：编解码口径不是一条，就是"半个请求被当成了请求"
        let again = decode_resume(&encode_resume(file_id, from_index)).expect("自编的 RESUME 解不开");
        assert_eq!(again, (file_id, from_index), "RESUME 编解码不互逆");
    }
    // 自己编一条回去必须能原样解出来
    if data.len() >= 12 {
        let file_id = u64::from_be_bytes(data[..8].try_into().unwrap());
        let from_index = u32::from_be_bytes(data[8..12].try_into().unwrap());
        let back = decode_resume(&encode_resume(file_id, from_index)).unwrap();
        assert_eq!(back, (file_id, from_index), "RESUME 编解码不自洽");
    }
});
