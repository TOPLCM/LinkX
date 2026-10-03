//! TLV 常量定义：常量单源（本文件），禁止在别处重复定义。
//! 结构：`tag(1B) | len(1B) | value(lenB)` 连续排列，无嵌套；适用 HELLO / CHALLENGE /
//! REPLY / PAIR_CONFIRM / PAIR_DONE / PING / PONG 等 <=64B 控制消息。
//! 跨端对齐：Kotlin 侧（Platforms/Android/app/.../Tlv.kt）必须与此文件逐项一致。
//! PING/PONG 用 TLV payload tag 区分而非 flags 位——flags 8 位已全部占用（bit7=JSON debug）。
//! Channel Binding：`MSG_CHANNEL_BIND` 携带 nonce_tcp 与签名，把 TCP 会话绑定到 BLE 已配对
//! 设备；`TAG_BIND_TAG` = `HMAC(channel_bind_key, nonce_tcp)` 截断（派生见
//! Crates/crypto/src/binding.rs）。

pub mod tlv {
    // ---- 控制消息 type ----
    pub const MSG_HELLO: u8 = 0x01;
    pub const MSG_CHALLENGE: u8 = 0x02;
    pub const MSG_REPLY: u8 = 0x03;
    pub const MSG_PAIR_CONFIRM: u8 = 0x04;
    pub const MSG_PAIR_DONE: u8 = 0x05;
    pub const MSG_CHANNEL_BIND: u8 = 0x06; // BLE 已认证通道绑定 TCP 会话
    pub const MSG_RESUME: u8 = 0x07; // 断点续传请求（收端告知发端从第几块续传）
    pub const MSG_CONFIG_SYNC: u8 = 0x08; // 跨端配置同步（scope=cross/per_peer 的配置项）
    pub const MSG_IDENTITY: u8 = 0x09; // RSA-2048 身份交换（pk_der || sig(hash_h)）
    pub const MSG_HEARTBEAT: u8 = 0x70;

    // ---- TLV tag ----
    pub const TAG_ADVERT_NAME: u8 = 0x01;
    pub const TAG_OS: u8 = 0x02; // 0=unknown 1=android 2=windows
    pub const TAG_VERSION: u8 = 0x03;
    pub const TAG_CIPHERTEXT: u8 = 0x10; // Noise 握手消息封装（msg1/2/3）
    pub const TAG_SAS: u8 = 0x11;
    pub const TAG_PING: u8 = 0x20;
    pub const TAG_PONG: u8 = 0x21;
    pub const TAG_NONCE_TCP: u8 = 0x30; // channel binding
    pub const TAG_DEVICE_ID: u8 = 0x31;
    pub const TAG_FINGERPRINT: u8 = 0x32;
    pub const TAG_BIND_TAG: u8 = 0x33; // channel binding 签名
    pub const TAG_FILE_ID: u8 = 0x34; // 断点续传：文件 id（fixed64，8B 大端）
    pub const TAG_RESUME_FROM: u8 = 0x35; // 断点续传：起始分块 index（u32，4B 大端）

    // ---- 通道/状态常量 ----
    pub const OS_ANDROID: u8 = 1;
    pub const OS_WINDOWS: u8 = 2;

    // ---- 会话相关 ----
    pub const STATE_DISCOVER: u8 = 0;
    pub const STATE_HANDSHAKE: u8 = 1;
    pub const STATE_PAIRING: u8 = 2;
    pub const STATE_SAS_COMPARE: u8 = 3;
    pub const STATE_PAIRED: u8 = 4;
    pub const STATE_REPAIRED: u8 = 5;
    pub const STATE_RECONNECTING: u8 = 6;
    pub const STATE_CLOSED: u8 = 7;
}
