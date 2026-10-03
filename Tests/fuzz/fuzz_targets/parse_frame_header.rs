//! 6.2/6.5 fuzz 目标 1：FrameHeader::parse / parse_full_frame 零崩溃保证
#![no_main]
use libfuzzer_sys::fuzz_target;
use linkx_protocol::frame::{parse_full_frame, FrameHeader};

fuzz_target!(|data: &[u8]| {
    let _ = FrameHeader::parse(data);
    let _ = parse_full_frame(data);
});