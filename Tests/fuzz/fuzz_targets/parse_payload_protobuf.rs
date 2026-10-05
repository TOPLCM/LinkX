//! 6.2/6.5 fuzz 目标 2：protobuf payload 解析零崩溃（prost decode）
#![no_main]
use libfuzzer_sys::fuzz_target;
use linkx_protocol::pb::{
    ClipboardPush, FileChunk, FileMeta, NotificationDismiss, NotificationPush, NotificationReply,
    NotificationReplyAck,
};
use prost::Message;

fuzz_target!(|data: &[u8]| {
    let _ = NotificationPush::decode(data);
    let _ = NotificationReply::decode(data);
    let _ = NotificationDismiss::decode(data);
    let _ = NotificationReplyAck::decode(data);
    let _ = ClipboardPush::decode(data);
    let _ = FileMeta::decode(data);
    let _ = FileChunk::decode(data);
});