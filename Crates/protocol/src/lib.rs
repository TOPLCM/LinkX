//! LinkX protocol crate — 帧头 / 消息类型 / BLE 分片
//!
//! 单一依赖方向：本 crate 不依赖 crypto/session，纯编解码与重组语义。

pub mod ble_frag;
pub mod envelope;
pub mod frame;
pub mod tlv_codec;

/// 单路径引入 Proto/linkx/v1/tlv.rs（常量单源，禁止别处复制）
#[path = "../../../Proto/linkx/v1/tlv.rs"]
pub mod proto_tlv;

pub use proto_tlv::tlv::*;

/// prost 生成的协议结构（来自 Proto/linkx/v1/*.proto）
pub mod pb {
    include!(concat!(env!("OUT_DIR"), "/linkx.v1.rs"));
}

pub use pb as linkx;

/// 消息类型常量（首发集合，独立于 TLV 常量表）
pub mod msg_type {
    pub const HELLO: u8 = 0x01;
    pub const CHALLENGE: u8 = 0x02;
    pub const REPLY: u8 = 0x03;
    pub const PAIR_CONFIRM: u8 = 0x04;
    pub const PAIR_DONE: u8 = 0x05;
    pub const CHANNEL_BIND: u8 = super::MSG_CHANNEL_BIND; // BLE 已认证通道绑定 TCP 会话（单源=Tlv）
    pub const RESUME: u8 = super::MSG_RESUME; // 断点续传请求（收端 → 发端，单源=Tlv）
    pub const IDENTITY: u8 = super::MSG_IDENTITY; // RSA-2048 身份交换 pk_der || sig(hash_h)（单源=Tlv）
    pub const NOTIFY_PUSH: u8 = 0x10;
    pub const NOTIFY_REPLY: u8 = 0x11;
    pub const NOTIFY_DISMISS: u8 = 0x12;
    pub const NOTIFY_REPLY_ACK: u8 = 0x13;
    /// 跨端配置同步（scope=cross/per_peer 的配置项，握手完成后加密交换）
    pub const CONFIG_SYNC: u8 = super::MSG_CONFIG_SYNC; // 单源=Tlv
    pub const CLIPBOARD_PUSH: u8 = 0x20;
    pub const FILE_META: u8 = 0x30;
    pub const FILE_CHUNK: u8 = 0x31;
    pub const FILE_DONE: u8 = 0x32;
    /// 收端 → 发端「停止发送」。收端对一条正在进来的流没有别的掐断手段，只能显式回话；
    /// 发端的取消走 FILE_DONE{cancelled:true}，不另发明一条消息。
    pub const FILE_CANCEL: u8 = 0x33;
    #[allow(dead_code)] // 预留，未启用
    pub const MIRROR_FRAME: u8 = 0x40;
    #[allow(dead_code)] // 预留，未启用
    pub const MIRROR_CONTROL: u8 = 0x41;
    #[allow(dead_code)] // 预留，未启用
    pub const CALL_INCOMING: u8 = 0x50;
    /// 图片/视频互传：问与答各占一个类型，收到就知道是什么，不靠"谁才会发"去猜。
    /// 取原图的**回包走 FILE_\***，不另发明大数据通道。
    /// 这五条**只走局域网 TCP**，不许降级蓝牙——蓝牙分片通道传大块会静默丢 chunk 却看似完整。
    pub const ALBUM_LIST_REQ: u8 = 0x60;
    pub const ALBUM_LIST: u8 = 0x61;
    pub const ALBUM_THUMB_REQ: u8 = 0x62;
    pub const ALBUM_THUMB: u8 = 0x63;
    pub const ALBUM_FULL_REQ: u8 = 0x64;
    pub const HEARTBEAT: u8 = 0x70;
    /// 媒体控制：手机侧把系统 MediaSession 的当前播放状态推给电脑，
    /// 电脑下发播放/切歌/音量指令。**只同步状态与指令，不搬运音频**。
    pub const MEDIA_STATE: u8 = 0x80;
    pub const MEDIA_COMMAND: u8 = 0x81;
    /// 手机设备状态（电量/充电中）。与媒体状态同一类"手机现在怎么样"的小标量，
    /// 只在变化时发一次，不做周期性心跳。
    pub const DEVICE_STATUS: u8 = 0x82;
    // 0x83 只在测试版里做过 MEDIA_COVER（媒体封面），现已整条撤除。这个号位不许复用：
    // 装过那份测试包的机器还在，复用同一个号就等于让它有两种解释。
    pub const ERROR: u8 = 0xFF;

    /// 首发支持的完整消息集合（0x01–0x09 握手/配对/身份 + 0x10–0x13 + 0x20/0x30–0x33 +
    /// 0x60–0x64 + 0x70 + 0x80–0x82 + 0xFF）
    pub fn is_v1(mt: u8) -> bool {
        matches!(
            mt,
            HELLO
                | CHALLENGE
                | REPLY
                | PAIR_CONFIRM
                | PAIR_DONE
                | CHANNEL_BIND
                | RESUME
                | IDENTITY
                | NOTIFY_PUSH
                | NOTIFY_REPLY
                | NOTIFY_DISMISS
                | NOTIFY_REPLY_ACK
                | CONFIG_SYNC
                | CLIPBOARD_PUSH
                | FILE_META
                | FILE_CHUNK
                | FILE_DONE
                | FILE_CANCEL
                | ALBUM_LIST_REQ
                | ALBUM_LIST
                | ALBUM_THUMB_REQ
                | ALBUM_THUMB
                | ALBUM_FULL_REQ
                | HEARTBEAT
                | MEDIA_STATE
                | MEDIA_COMMAND
                | DEVICE_STATUS
                | ERROR
        )
    }

    /// flags.bit4：除心跳外全部置 1
    pub fn requires_encryption(mt: u8) -> bool {
        mt != HEARTBEAT
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn v1_msg_set_is_consistent() {
        // 首发消息必须全部要求加密（心跳除外）
        let v1 = [
            msg_type::HELLO,
            msg_type::CHALLENGE,
            msg_type::REPLY,
            msg_type::PAIR_CONFIRM,
            msg_type::PAIR_DONE,
            msg_type::CHANNEL_BIND,
            msg_type::RESUME,
            msg_type::IDENTITY,
            msg_type::NOTIFY_PUSH,
            msg_type::NOTIFY_REPLY,
            msg_type::NOTIFY_REPLY_ACK,
            msg_type::NOTIFY_DISMISS,
            msg_type::CONFIG_SYNC,
            msg_type::CLIPBOARD_PUSH,
            msg_type::FILE_META,
            msg_type::FILE_CHUNK,
            msg_type::FILE_DONE,
            msg_type::FILE_CANCEL,
            msg_type::ALBUM_LIST_REQ,
            msg_type::ALBUM_LIST,
            msg_type::ALBUM_THUMB_REQ,
            msg_type::ALBUM_THUMB,
            msg_type::ALBUM_FULL_REQ,
            msg_type::MEDIA_STATE,
            msg_type::MEDIA_COMMAND,
            msg_type::DEVICE_STATUS,
            msg_type::ERROR,
        ];
        for mt in v1 {
            assert!(msg_type::is_v1(mt));
            assert!(msg_type::requires_encryption(mt));
        }
        assert!(!msg_type::requires_encryption(msg_type::HEARTBEAT));
        // 白名单与这张表必须严格相等：漏一项就是"表里有但 is_v1 不认"（上一轮就是这样把
        // 0x80–0x82 三条已上线的消息漏在"首发完整集合"之外的）。
        let accepted = (0u8..=255).filter(|m| msg_type::is_v1(*m)).count();
        assert_eq!(
            accepted,
            v1.len() + 1,
            "is_v1 与测试表不再一一对应（+1 是 HEARTBEAT）：实际 {accepted}"
        );
        // 预留消息不在首发集合内
        assert!(!msg_type::is_v1(msg_type::MIRROR_FRAME));
        assert!(!msg_type::is_v1(msg_type::CALL_INCOMING));
    }

    #[test]
    fn tlv_constants_single_source() {
        // Proto/linkx/v1/tlv.rs 顶层常量可被协议 crate 直接引用
        assert_eq!(MSG_HEARTBEAT, 0x70);
        assert_eq!(TAG_PING, 0x20);
        assert_eq!(TAG_PONG, 0x21);
        assert_eq!(MSG_CHANNEL_BIND, 0x06);
        assert_eq!(TAG_NONCE_TCP, 0x30);
        assert_eq!(TAG_BIND_TAG, 0x33);
        assert_eq!(MSG_RESUME, 0x07);
        assert_eq!(MSG_CONFIG_SYNC, 0x08);
        assert_eq!(TAG_FILE_ID, 0x34);
        assert_eq!(TAG_RESUME_FROM, 0x35);
    }

    #[test]
    fn pb_types_compile() {
        // prost 生成的结构可用（编译期代码生成冒烟）
        let n = linkx::NotificationPush {
            package: "com.example".into(),
            title: "T".into(),
            text: String::new(),
            post_ts_ms: 0,
            key_hash: 1,
            cover_jpeg: Default::default(),
            tag: "msg".into(),
            notification_id: 42,
            can_reply: true,
            reply_action_index: 0,
            reply_result_key: "reply".into(),
        };
        assert_eq!(n.package, "com.example");
    }

    #[test]
    fn notify_reply_fields_round_trip_and_old_peers_ignore_them() {
        use prost::Message;
        let body = linkx::NotificationPush {
            package: "org.telegram.messenger".into(),
            title: "张三".into(),
            text: "在吗".into(),
            post_ts_ms: 1_790_000_000_000,
            key_hash: 7,
            cover_jpeg: Default::default(),
            tag: String::new(),
            notification_id: -3,
            can_reply: true,
            reply_action_index: 1,
            reply_result_key: "key_reply_text".into(),
        }
        .encode_to_vec();
        let back = linkx::NotificationPush::decode(body.as_slice()).unwrap();
        assert_eq!(back.notification_id, -3);
        assert!(back.can_reply);
        assert_eq!(back.reply_action_index, 1);
        assert_eq!(back.reply_result_key, "key_reply_text");
        let old = linkx::NotificationPush {
            tag: String::new(),
            notification_id: 0,
            can_reply: false,
            reply_action_index: 0,
            reply_result_key: String::new(),
            ..back
        };
        let _ = linkx::NotificationPush::decode(old.encode_to_vec().as_slice()).unwrap();
        let req = linkx::NotificationReply {
            reply_id: 9,
            package: "p".into(),
            tag: "t".into(),
            notification_id: 2,
            action_index: 0,
            result_key: "k".into(),
            text: "收到".into(),
        }
        .encode_to_vec();
        let req = linkx::NotificationReply::decode(req.as_slice()).unwrap();
        assert_eq!(req.reply_id, 9);
        assert_eq!(req.text, "收到");

        let ack = linkx::NotificationReplyAck {
            reply_id: 9,
            package: "p".into(),
            ok: false,
            error: "该应用不支持回复".into(),
        }
        .encode_to_vec();
        let ack = linkx::NotificationReplyAck::decode(ack.as_slice()).unwrap();
        assert!(!ack.ok);
        assert_eq!(ack.error, "该应用不支持回复");
    }
}
