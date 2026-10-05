package com.linkx.app

/**
 * TLV / 协议常量镜像：
 * 与 Proto/linkx/v1/tlv.rs 及 Crates/lan/src/transport.rs::BleUuid 逐项一致，禁止漂移。
 * 改任何一项必须同步三处，并由 check-protocol-sync 脚本校验。
 */
object Tlv {
    // 控制消息 type
    const val MSG_HELLO = 0x01
    const val MSG_CHALLENGE = 0x02
    const val MSG_REPLY = 0x03
    const val MSG_PAIR_CONFIRM = 0x04
    const val MSG_PAIR_DONE = 0x05
    const val MSG_CHANNEL_BIND = 0x06      // BLE 已认证通道绑定 TCP 会话
    const val MSG_RESUME = 0x07            // 断点续传请求（收端 → 发端）
    const val MSG_HEARTBEAT = 0x70

    // TLV tag
    const val TAG_ADVERT_NAME = 0x01
    const val TAG_OS = 0x02
    const val TAG_VERSION = 0x03
    const val TAG_CIPHERTEXT = 0x10
    const val TAG_SAS = 0x11
    const val TAG_PING = 0x20
    const val TAG_PONG = 0x21
    const val TAG_NONCE_TCP = 0x30
    const val TAG_DEVICE_ID = 0x31
    const val TAG_FINGERPRINT = 0x32
    const val TAG_BIND_TAG = 0x33           // channel binding 签名
    const val TAG_FILE_ID = 0x34            // 断点续传文件 id（fixed64）
    const val TAG_RESUME_FROM = 0x35        // 续传起始分块 index（u32）
    const val TAG_HELLO_SEQ = 0x36          // HELLO 会话序号（u64，区分真重启与迟到重投）

    const val OS_ANDROID = 1
    const val OS_WINDOWS = 2

    // BLE GATT（Crates/lan BleUuid 镜像：产品自有 UUID 命名空间 "LX"）
    object Ble {
        const val SERVICE = "4c584c00-0000-1000-8000-00805f9b34fb"
        const val CHAR_TX = "4c584c01-0000-1000-8000-00805f9b34fb"
        const val CHAR_EVT = "4c584c02-0000-1000-8000-00805f9b34fb"
    }
}