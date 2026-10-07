//! 会话引擎：BLE 字节流 ↔ 握手 / 加密 / 业务消息
//!
//! - **平台无关**：引擎只吃「BLE 分片包字节」、吐「BLE 分片包字节」，不碰 socket / GATT，
//!   平台侧（Windows `ble_central` / Android `BlePeripheralService`）只做收发搬运。
//! - **单线程驱动**：所有时间由调用方以 `now: Instant` 显式传入（可单测、无隐式时钟）。
//! - **握手承载**：HELLO = TLV(advert_name/os/version)；CHALLENGE/REPLY = Noise XX 消息
//!   **原文**（msg2 = 98B 超出 TLV 64B 上限，故不复用 `TAG_CIPHERTEXT`）。
//! - **业务承载**：信封 + protobuf body，经 ChaCha20-Poly1305 加密后组帧。
//! - **MTU 由平台层注入**（`set_ble_mtu`）：未注入时用 `BLE_MTU`（23 = ATT 规范下限）兜底。

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use debuglog::Level;
use linkx_crypto::cipher::{
    decrypt_payload, encrypt_payload, DIR_INITIATOR_TO_RESPONDER, DIR_RESPONDER_TO_INITIATOR,
};
use linkx_crypto::identity::{
    decode_identity_payload, encode_identity_payload, fingerprint_of_public_der, verify_binding,
    DeviceIdentity,
};
use linkx_crypto::noise::{NoiseXxHandshake, Role};
use linkx_crypto::replay::ReplayWindow;
use linkx_crypto::{derive_session_key, sas_digits};
use linkx_protocol::ble_frag::{split_into_packets, BleReassembler, MTU_DEFAULT};
use linkx_protocol::envelope;
use linkx_protocol::frame::{assemble_frame, flags, parse_full_frame, FrameHeader};
use linkx_protocol::pb::{
    AlbumFullRequest, AlbumItem, AlbumList, AlbumListRequest, AlbumThumb, AlbumThumbRequest,
    ClipboardPush, ConfigSync, DeviceStatus, FileCancel, FileChunk, FileDone, FileMeta,
    MediaCommand, MediaState, NotificationDismiss, NotificationPush, NotificationReply,
    NotificationReplyAck,
};
use linkx_protocol::tlv_codec::{self, Tlv};
use linkx_protocol::{
    msg_type, MSG_IDENTITY, MSG_PAIR_CONFIRM, MSG_PAIR_DONE, TAG_ADVERT_NAME, TAG_FILE_ID,
    TAG_HELLO_SEQ, TAG_OS, TAG_RESUME_FROM, TAG_VERSION,
};
use prost::Message;
use sha2::{Digest, Sha256};

use crate::binding::{BindRole, ChannelBinding};
use crate::heartbeat::{is_ping, is_pong, ping_payload, pong_payload, Backoff, HeartbeatSpec};
use crate::pairing::PairFlow;
use crate::state::{SessionChannel, SessionEvent, SessionManager, SessionState, TofuVerdict};

/// 未注入 MTU 时的兜底分片基准（23 = ATT 规范下限，14B/包净荷）。
/// 真实值必须由平台层注入：链路实际协商到 517 却按 23 切片时，每条消息白切 37 倍碎片，
/// 一次身份交换 41 片就会顶穿 20 s 交换时限（-212）。
pub const BLE_MTU: usize = MTU_DEFAULT;
/// 允许的 MTU 上界：与分片器同源的 ATT 规范上限，避免两处各写一个 517
pub const BLE_MTU_MAX: usize = linkx_protocol::ble_frag::MAX_ATT_MTU;
/// DISCOVER 态下 Initiator 重播 HELLO 的间隔。缺了它会**永久死锁**：重连退避耗尽后两端
/// 都回到 DISCOVER，而 `tick` 只驱动 Paired / Reconnecting，DISCOVER 下没人再发 HELLO，
/// 于是两台设备安静地等对方先开口。
pub const DISCOVER_REATTACH: Duration = Duration::from_secs(5);
/// HELLO 内 advert_name 截断上限（保 TLV ≤64B）
pub const HELLO_NAME_MAX: usize = 32;
/// HELLO 里版本串的字节上限。四条 TLV 的 tag/len 占 8B，名字 32B、系统 1B、序号 8B，
/// 加起来正好压满 [`linkx_protocol::TLV_MAX_MSG`]（64B）⇒ 版本最多 15B。
/// 在这里就截断，是为了让 HELLO 的编码**不可能失败**：以前失败时退化成只发系统字节，
/// 对端当场拿不到设备名，"按名字认领已绑定设备"那条路就死了。
pub const HELLO_VERSION_MAX: usize = 15;

/// 本引擎实例的 HELLO 序号。64 位随机足够：它只需要把"同一次启动的重投"与"新一次启动"分开。
fn random_hello_seq() -> u64 {
    u64::from_be_bytes(linkx_crypto::random_bytes())
}

/// 按字节上限截断但不切断一个字符：`Vec<u8>::truncate` 会把多字节字符砍成半个，
/// 对端 `String::from_utf8` 一验就失败，整台设备的名字变成空的。
fn truncate_name(s: &str, max_bytes: usize) -> Vec<u8> {
    tlv_codec::truncate_utf8(s, max_bytes).as_bytes().to_vec()
}

/// 组一条 HELLO 的正文：名字 / 系统 / 版本 / 序号四条，两条字符串都按预算截断。
///
/// 拆成纯函数是要能被单测钉住的：名字进不了 HELLO，电脑端就认不出那台已绑定的设备
/// （它按名字认领并自动重连），而这条链路在 BLE 广播少带名字时已经断过一次。
fn encode_hello_body(name: &str, os: u8, version: &str, seq: u64) -> Vec<u8> {
    tlv_codec::encode(&[
        Tlv::buf(TAG_ADVERT_NAME, &truncate_name(name, HELLO_NAME_MAX)),
        Tlv::u8(TAG_OS, os),
        Tlv::buf(TAG_VERSION, &truncate_name(version, HELLO_VERSION_MAX)),
        Tlv::buf(TAG_HELLO_SEQ, &seq.to_be_bytes()),
    ])
    .expect("HELLO 的四条 TLV 恒在 64B 预算内（见 HELLO_VERSION_MAX）")
}
/// 握手完成后等待对端 IDENTITY 的时限（BLE 分片下 ~550B 载荷 + RSA 生成余量）
pub const IDENTITY_EXCHANGE_TIMEOUT: Duration = Duration::from_secs(20);
/// TCP 连接已建立、但对方一直不发 `CHANNEL_BIND` 的时限。**没有它会永久占死唯一的槽位**：
/// 未绑定的连接不算业务通道（TCP 心跳被 `tcp_bound` 挡着），平台层的读循环又只在
/// "本端已判死"时才退出，于是局域网里任意主机连上端口什么都不发，就能让真手机再也接不上。
pub const TCP_BIND_TIMEOUT: Duration = Duration::from_secs(20);
/// 剪贴板正文的字节上限（两端同口径，走引擎这一个闸门）。帧上限说的是"一帧能塞多大"，
/// 不是"该收多少剪贴板"；Windows 落进系统剪贴板时还要 UTF-8→UTF-16 再翻一倍。
/// 超了**拒收/拒发并出声，不截断**——截断会把"同步了"变成"同步错了"。
pub const CLIPBOARD_MAX_BYTES: usize = 32 * 1024;

/// 会话状态数值（与 `Proto/linkx/v1/tlv.rs` STATE_* 逐项一致，供 FFI/UI 使用）
pub mod state_code {
    pub const DISCOVER: u8 = 0;
    pub const HANDSHAKE: u8 = 1;
    pub const PAIRING: u8 = 2;
    pub const SAS_COMPARE: u8 = 3;
    pub const PAIRED: u8 = 4;
    pub const REPAIRED: u8 = 5;
    pub const RECONNECTING: u8 = 6;
    pub const CLOSED: u8 = 7;
}

/// 引擎错误码：-1 通用 IO / 握手；-200 用户拒绝配对；-212 设备身份校验失败；
/// -213 同名设备身份变化。
pub mod err_code {
    pub const IO_GENERIC: i32 = -1;
    pub const USER_REJECT_PAIR: i32 = -200;
    /// 身份验签失败 / 身份不可用
    pub const IDENTITY_VERIFY_FAILED: i32 = -212;
    /// 同名设备新身份（需重新配对）
    pub const TOFU_FINGERPRINT_MISMATCH: i32 = -213;
}

/// 引擎角色（Windows = Central = Initiator；Android = Peripheral = Responder）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EngineRole {
    Initiator,
    Responder,
}

/// 状态名（日志用，与 `state_code` 语义一致）
fn state_name(s: SessionState) -> &'static str {
    match s {
        SessionState::Discover => "DISCOVER",
        SessionState::Handshake => "HANDSHAKE",
        SessionState::Pairing => "PAIRING",
        SessionState::SasCompare => "SAS_COMPARE",
        SessionState::Paired => "PAIRED",
        SessionState::Repaired => "REPAIRED",
        SessionState::Reconnecting => "RECONNECTING",
        SessionState::Closed => "CLOSED",
    }
}

impl EngineRole {
    fn noise_role(self) -> Role {
        match self {
            EngineRole::Initiator => Role::Initiator,
            EngineRole::Responder => Role::Responder,
        }
    }
}

/// 已信任对端（以 RSA 身份指纹为准；`name` 用于「同名新身份」识别）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrustedPeer {
    /// 对端 RSA-2048 身份指纹（16 位小写 hex，跨重启稳定）
    pub fingerprint: String,
    /// 对端展示名（HELLO advert_name；同名不同指纹 → 提示"设备身份已变化"）
    pub name: String,
}

/// 引擎配置（一次性给定，运行期不变）
pub struct EngineConfig {
    pub role: EngineRole,
    /// 设备展示名（广播/HELLO 用）
    pub local_name: String,
    /// `TAG_OS`：1 = Android，2 = Windows
    pub os: u8,
    pub version: String,
    /// 长期身份私钥（Noise X25519 static；跨重连稳定复用，**不再用于设备识别**）
    pub local_static_sk: [u8; 32],
    /// 本机 RSA-2048 身份（PKCS#8 DER；平台层加密持久化）。
    /// 为空时引擎**临时生成一个**（仅测试/降级用；生产路径必须显式提供）。
    pub local_identity_der: Vec<u8>,
    /// 已信任对端列表（以 RSA 指纹为信任锚）
    pub trusted_peers: Vec<TrustedPeer>,
    /// BLE 心跳参数（BLE = 30s PING / 30s 超时）
    pub heartbeat: HeartbeatSpec,
}

impl EngineConfig {
    pub fn new(
        role: EngineRole,
        local_name: impl Into<String>,
        os: u8,
        version: impl Into<String>,
        local_static_sk: [u8; 32],
    ) -> Self {
        Self {
            role,
            local_name: local_name.into(),
            os,
            version: version.into(),
            local_static_sk,
            local_identity_der: Vec::new(),
            trusted_peers: Vec::new(),
            heartbeat: HeartbeatSpec::BLE,
        }
    }

    /// 注入本机 RSA-2048 身份（PKCS#8 DER；平台层从加密存储读取）
    pub fn with_identity_der(mut self, der: Vec<u8>) -> Self {
        self.local_identity_der = der;
        self
    }

    /// 注入已信任对端列表（平台层从持久化信任库读取）
    pub fn with_trusted_peers(mut self, peers: Vec<TrustedPeer>) -> Self {
        self.trusted_peers = peers;
        self
    }
}

/// 跨端配置项（`scope = cross | per_peer` 才跨端同步）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigEntryItem {
    pub key: String,
    pub value: String,
    pub scope: String,
}

/// 收到的文件分块（大载荷，独立于事件流：FFI 事件长度域为 u16，256KB 分块无法承载）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IncomingChunk {
    pub file_id: u64,
    pub index: u32,
    pub crc32: u32,
    pub data: Vec<u8>,
}

/// 一次文件传输**钉死**在哪条通道上：传输开始时选定一次并记住 file_id，分块只认这条通道。
/// 若逐帧各自判断 `tcp_bound`，同一次传输会被劈成两条链路——约 200B 的 FILE_META 挤得过 BLE，
/// 256KB 的 FILE_CHUNK 在 BLE 上要拆成约 18,700 个 14B 包，远超安卓 notify 队列上限 →
/// 分块被丢弃，用户却看到"已发送完成"。没绑定就大声失败，绝不悄悄降级。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FileChannel {
    /// 无在途文件任务
    Idle,
    /// 本次传输走 TCP（正常路径）
    Tcp(u64),
    /// 本次传输走 BLE（仅小文件；分块预算由平台侧负责）
    Ble(u64),
}

/// 取消原因兜底：空/缺位不给"没有原因"这种废话，直接给一句用户能懂的话
fn cancel_reason(raw: Option<String>) -> String {
    match raw.map(|s| s.trim().to_string()).filter(|s| !s.is_empty()) {
        Some(s) => s,
        None => "对方取消了这次传输".to_string(),
    }
}

/// 引擎上行事件（平台侧 UI 消费；Windows 直调、Android 经 JNI 取回）。
/// `Eq` 刻意不派生：相册事件直接带 prost 生成的结构体（`AlbumItem` 没有 `Eq`），比对用途
/// 只有测试里的 `matches!`，不值得再手写一层镜像结构。
#[derive(Debug, Clone, PartialEq)]
pub enum EngineEvent {
    /// 会话状态变化（值 = `state_code::*`）
    StateChanged { state: u8 },
    /// 收到对端 HELLO（设备名/系统/版本）
    PeerHello {
        name: String,
        os: u8,
        version: String,
    },
    /// 需要人工比对的 6 位 SAS（首次配对）
    SasReady { sas: u32 },
    /// 配对完成（携带对端 RSA 身份指纹，平台侧应持久化到信任库）
    PeerPaired { fingerprint: String },
    /// **同名设备呈递了新身份**（指纹变化，常见于对方重装）。
    /// UI 应弹「重新配对确认」；用户接受后调 `accept_new_fingerprint()` 进入 SAS 复核。
    IdentityChanged {
        /// 对端展示名（与信任库中同名条目匹配）
        name: String,
        /// 信任库中旧指纹
        old_fingerprint: String,
        /// 本次握手呈递的新指纹
        new_fingerprint: String,
    },
    /// 对端推送的通知
    Notification {
        package: String,
        title: String,
        text: String,
        post_ts_ms: i64,
        /// 稳定 key 哈希（0 = 无；同应用同 key 的通知应就地合并更新而非新增条目）
        key_hash: u32,
        /// 回复定位三元组 + 入口：`can_reply` 为真才可以在界面上给"回复"，
        /// 它是应用自己挂没挂 RemoteInput 的结果，不是我们的能力。
        tag: String,
        notification_id: i32,
        can_reply: bool,
        reply_action_index: i32,
        reply_result_key: String,
    },
    /// 对端请求回复一条通知（电脑 → 手机，手机侧执行 RemoteInput）
    NotifyReplyRequested {
        reply_id: u32,
        package: String,
        tag: String,
        notification_id: i32,
        action_index: i32,
        result_key: String,
        text: String,
    },
    /// 一条回复请求的回执（手机 → 电脑）。`error` 非空 = 有一句要如实显示的话。
    NotifyReplyAck {
        reply_id: u32,
        package: String,
        ok: bool,
        error: String,
    },
    /// 对端报告：这条通知已从通知栏消失（手机 → 电脑）。只报当初带过 `can_reply` 的条目，
    /// 用途是**撤掉电脑上的回复入口**——通知都没了，留着按钮就是等用户点出一次失败。
    NotifyDismissed {
        package: String,
        tag: String,
        notification_id: i32,
        key_hash: u32,
    },
    /// 对端推送的剪贴板（V1 仅纯文本）
    Clipboard { text: String },
    /// 对端（手机）当前播放状态：只带"放什么 / 放到哪 / 在不在放"，不搬运音频。
    MediaState {
        package: String,
        title: String,
        artist: String,
        album: String,
        playing: bool,
        position_ms: i64,
        duration_ms: i64,
        /// 播放倍速 ×100（1.25x → 125）：用整数而不是 f32，因为 `EngineEvent` 派生了 `Eq`
        /// 而浮点没有等价关系，倍速本身只需两位小数精度。
        speed_x100: i32,
        /// 手机当前媒体音量 0-100（电脑侧 +/- 以此为基准，不自己记账）
        volume: i32,
    },
    /// 对端（电脑）下发的播放控制指令，由平台层执行。
    MediaCommand {
        /// `media_command::Action` 的值（0=PLAY_PAUSE … 8=SET_VOLUME）
        action: i32,
        volume: i32,
        delta_ms: i64,
    },
    /// 手机设备状态（电量/充电中），手机 → 电脑。`battery` 为 -1 表示读不到。
    ///
    /// `ts_ms` 必须带上：`send_routed` 是逐帧选路的，同一台手机的前后两条状态
    /// 可能一条走 BLE 一条走 TCP，先到后到没有保证。不带时间戳就没法丢旧留新。
    DeviceStatus {
        battery: i32,
        charging: bool,
        ts_ms: i64,
    },
    /// 收到文件元信息（收端：平台层据此准备落盘并回执续传位置）
    FileMetaReceived {
        file_id: u64,
        name: String,
        size: u64,
        chunk_size: u32,
        /// 整文件 SHA-256（发送端在 META 里声明的值）；`None` = 发端流式计算，摘要改由 FILE_DONE 交付
        sha256: Option<[u8; 32]>,
        /// 整文件 CRC32（发送端声明值；收端可作快速预检）
        crc32: u32,
        /// 相册：非 0 = 这条 FILE_META 是对「取原图」的应答，值 = 照片 id。
        /// 收端据此把它送到用户选的导出目录，而不是落进收件目录。
        album_id: u64,
    },
    /// 文件传输结束（`ok = false` 时 `error` 为原因）
    FileDoneReceived {
        file_id: u64,
        ok: bool,
        error: Option<String>,
        /// 发端随结束帧交付的整文件 SHA-256；`None` = 旧端（摘要只在 FILE_META 里）
        sha256: Option<[u8; 32]>,
    },
    /// 对端请求从指定分块续传
    FileResumeRequested { file_id: u64, from_index: u32 },
    /// TCP 通道绑定完成（此后文件传输走 TCP，带宽远高于 BLE）
    TcpBound,
    /// TCP 通道绑定失败或断开（文件传输**不降级 BLE**，见 `FileChannel`）
    TcpUnbound { reason: String },
    /// 文件任务被硬失败：一次传输已经开始后 TCP 通道断开，或尚未绑定就想发分块。
    /// 单独一个事件而不塞进 `Error`：这类情况被静默吞掉过，发送侧照样显示"已完成"。
    /// 平台侧必须把它落到用户看得见的任务状态上。
    FileTaskFailed { file_id: u64, reason: String },
    /// 文件任务被**用户取消**（任一端发起都走这一条）。与 `FileTaskFailed` 分开是因为语义
    /// 不同：取消不是出错，报成失败就是把用户的主动动作说成系统故障。平台侧必须落到
    /// 「已取消」并显示原因，同时删除残留文件。
    FileTaskCancelled { file_id: u64, reason: String },
    /// 相册：对端要一页清单（只有手机侧会收到这条）
    AlbumListRequested { page: u32, per_page: u32 },
    /// 相册：一页清单到手（只有电脑侧会收到）。`error` 非空 = 这一页没取到，必须原样显示
    /// 给用户——"权限没给""相册是空的""手机侧读失败"是三件不同的事。
    AlbumPage {
        items: Vec<AlbumItem>,
        page: u32,
        total: u32,
        error: String,
    },
    /// 相册：对端要某一张缩略图（手机侧）
    AlbumThumbRequested { id: u64, edge: u32 },
    /// 相册：一张缩略图到手（电脑侧）。`error` 非空 = 这张没生成出来。
    AlbumThumb {
        id: u64,
        edge: u32,
        width: u32,
        height: u32,
        jpeg: Vec<u8>,
        error: String,
    },
    /// 相册：对端要这几张原图（手机侧）。**回包走 FILE_\***，这里只是待发清单。
    AlbumFullRequested { ids: Vec<u64> },
    /// 收到跨端配置同步
    ConfigReceived { entries: Vec<ConfigEntryItem> },
    /// 错误（code 见 `err_code`）
    Error { code: i32, context: String },
}

/// 会话引擎（单线程持有；平台侧在各自的 IO 线程里驱动）
pub struct SessionEngine {
    cfg: EngineConfig,
    mgr: SessionManager,
    reasm: BleReassembler,
    hs: Option<NoiseXxHandshake>,
    hs_writes: u8,

    session_key: Option<[u8; 32]>,
    session_id: [u8; 16],
    /// 对端 RSA 身份指纹（IDENTITY 交换完成后可用；唯一设备识别锚）
    peer_fp: Option<String>,
    pair: Option<PairFlow>,

    /// 本机 RSA-2048 身份（EngineConfig 未提供时临时生成；生成失败为 None → 拒绝配对）
    identity: Option<DeviceIdentity>,
    /// Noise 握手哈希（身份绑定签名/验签的基准）
    hs_hash: Option<[u8; 32]>,
    /// 对端 IDENTITY 公钥（SPKI DER）
    peer_identity_der: Option<Vec<u8>>,
    /// 是否已收到对端 IDENTITY（含验签通过）
    identity_received: bool,
    /// 身份裁决是否已完成（防重入）
    identity_verdict_done: bool,
    /// 身份交换等待截止（对端一直不发 IDENTITY → 超时判失败）
    identity_deadline: Option<Instant>,
    /// 是否已收到过对端 PAIR_CONFIRM（Repaired 期间收到的要在 accept 后补用，防死锁）
    peer_confirm_seen: bool,
    /// 对端展示名（HELLO；同名新身份识别用）
    peer_name: Option<String>,
    /// 对端 X25519 static 指纹（仅诊断用，不再参与信任判定）
    peer_static_fp: Option<String>,

    send_seq: u32,
    hs_seq: u32,
    /// BLE 分片组 id（每条出站帧递增，接收侧按它分组重组）
    frag_id: u16,
    /// 本端出站分片使用的 ATT MTU（初值 `BLE_MTU`，平台层经 `set_ble_mtu` 更新）
    ble_mtu: usize,
    msg_id: u128,
    recv_window: ReplayWindow,

    out: VecDeque<Vec<u8>>,
    /// TCP 通道出站队列（完整帧，不分片；文件传输用）
    out_tcp: VecDeque<Vec<u8>>,
    /// 入站文件分块队列（大载荷，独立于事件流）
    in_chunks: VecDeque<IncomingChunk>,
    events: Vec<EngineEvent>,

    /// TCP 通道绑定状态机（None = 未发起绑定）
    binding: Option<ChannelBinding>,
    /// TCP 通道是否已绑定（绑定后文件传输走 TCP）
    tcp_bound: bool,
    /// 发起绑定后的判死时点（`tick_paired` 里惰性记下，绑定成功或通道关闭即失效）
    bind_deadline: Option<Instant>,
    /// 我方 TCP nonce 是否已发出（避免双方无限互发 nonce）
    bind_tcp_nonce_sent: bool,
    /// 我方 BLE proof 是否已发出（每会话一次）
    bind_ble_proof_sent: bool,
    /// 绑定完成**之前**从 TCP 收到的业务帧（见 [`MAX_PENDING_TCP_PREBIND`]）。两端判定"绑定
    /// 完成"的时刻天然不同步，直接丢就是一次静默丢数据，故先有界暂存、绑定后原序补投。
    pending_tcp_prebind: VecDeque<Vec<u8>>,
    /// 在途文件传输绑定的通道（见 [`FileChannel`]）
    file_channel: FileChannel,
    /// 已取消的 `file_id`（FIFO，上限 [`MAX_CANCELLED_FILES`]）。取消与「分块 / 结束帧在路上」
    /// 是并发的：点下取消时对端可能已发出后面的分块甚至 FILE_DONE。这些迟到帧既不能当成新
    /// 会话（会凭空开一条接收），也不能悄悄丢掉——故在此登记，后续同 id 入站帧丢弃并计数、
    /// 出站分块一律拒绝。新的 FILE_META 清掉登记：那是一次全新传输。
    cancelled_files: VecDeque<u64>,
    /// 本机**在途接收**的 `file_id`（上限 [`MAX_RECV_OPEN_TRACKED`]）。通道锁只由**发送方**
    /// 钉，所以 TCP 断开时没有任何东西替接收方判失败：半截文件留着、任务行永远挂「接收中」。
    /// 引擎是唯一同时知道"这条接收在进行"和"它走的是 TCP"的地方，故 FILE_META 登记、
    /// FILE_DONE 注销、`on_tcp_closed` 统一大声判失败。
    recv_open: VecDeque<u64>,
    /// 因取消而丢弃的迟到帧计数（只增不清；取证「取消之后对端还在发」是否发生）
    late_frames_dropped: u64,

    sas: Option<u32>,
    hello_sent: bool,
    hello_received: bool,
    /// 是否已应答过对端 HELLO（请求/应答语义，每会话一次）
    hello_replied: bool,
    /// 本端 HELLO 的序号：起点随机，每发一条前进 1。对端用它分辨"同一条 HELLO 被重投"
    /// 与"对端要重开会话"（真重启、或断线后重新贴上来）——前者的字节与上次一模一样。
    hello_seq: u64,
    /// 本端**已经处理过**的那条对端 HELLO 的序号（跨本端复位保留：它说的是对端发的那一条）
    peer_hello_seq: Option<u64>,
    peer_done_verified: bool,
    last_rx: Option<Instant>,
    last_ping_at: Option<Instant>,
    /// TCP 通道**独立**的心跳状态。不能复用 `last_rx`——那是 BLE 的存活判据，
    /// 两条频道共用一个时间戳会让"BLE 断了但 TCP 有流量"被误判成健康（反之亦然）。
    last_tcp_rx: Option<Instant>,
    last_tcp_ping_at: Option<Instant>,
    /// DISCOVER 态下上一次重播 HELLO 的时刻（仅 Initiator 使用）
    last_discover_hello: Option<Instant>,

    /// 重连退避（1/2/4/8/16/32s × 6 → DISCOVER）
    reconnect_backoff: Backoff,
    /// 下一次重连尝试的时点（None = 不在重连退避中）
    reconnect_next_at: Option<Instant>,
    /// 本次 TOFU 裁决认定为"同名换身份"的那台设备的**旧指纹**（None = 不是漂移）。
    /// 人工确认后重写信任库时只淘汰它，别的一律不动。
    drift_old_fp: Option<String>,
}

impl SessionEngine {
    pub fn new(cfg: EngineConfig) -> Self {
        // 身份初始化：优先用平台层提供的持久化 DER，为空（测试 / 降级）则临时生成；
        // 两者都失败 → None（握手收尾时报错拒绝配对）。绝不 panic：panic=abort 会直接终止进程。
        let identity = DeviceIdentity::from_pkcs8_der(&cfg.local_identity_der)
            .ok()
            .or_else(|| DeviceIdentity::generate().ok());
        Self {
            cfg,
            mgr: SessionManager::new(SessionChannel::Ble),
            reasm: BleReassembler::new(),
            hs: None,
            hs_writes: 0,
            session_key: None,
            session_id: [0u8; 16],
            peer_fp: None,
            pair: None,
            identity,
            hs_hash: None,
            peer_identity_der: None,
            identity_received: false,
            identity_verdict_done: false,
            identity_deadline: None,
            peer_confirm_seen: false,
            peer_name: None,
            peer_static_fp: None,
            send_seq: 1,
            hs_seq: 0,
            frag_id: 0,
            ble_mtu: BLE_MTU,
            msg_id: 1,
            recv_window: ReplayWindow::default(),
            out: VecDeque::new(),
            out_tcp: VecDeque::new(),
            in_chunks: VecDeque::new(),
            events: Vec::new(),
            binding: None,
            tcp_bound: false,
            bind_deadline: None,
            bind_tcp_nonce_sent: false,
            bind_ble_proof_sent: false,
            pending_tcp_prebind: VecDeque::new(),
            file_channel: FileChannel::Idle,
            cancelled_files: VecDeque::new(),
            recv_open: VecDeque::new(),
            late_frames_dropped: 0,
            sas: None,
            hello_sent: false,
            hello_received: false,
            hello_replied: false,
            hello_seq: random_hello_seq(),
            peer_hello_seq: None,
            peer_done_verified: false,
            last_rx: None,
            last_ping_at: None,
            last_tcp_rx: None,
            last_tcp_ping_at: None,
            last_discover_hello: None,
            reconnect_backoff: Backoff::default(),
            reconnect_next_at: None,
            drift_old_fp: None,
        }
    }

    pub fn state(&self) -> SessionState {
        self.mgr.state
    }

    pub fn is_paired(&self) -> bool {
        self.mgr.is_paired()
    }

    /// 本端比对的 SAS（未进入比对阶段为 None）
    pub fn sas(&self) -> Option<u32> {
        self.sas
    }

    /// 对端 RSA 身份指纹（IDENTITY 交换完成后可用）
    pub fn peer_fingerprint(&self) -> Option<&str> {
        self.peer_fp.as_deref()
    }

    /// 本机 RSA 身份指纹（16 位 hex；UI 展示/信任库写入用）
    pub fn own_identity_fingerprint(&self) -> Option<String> {
        self.identity.as_ref().and_then(|i| i.fingerprint().ok())
    }

    /// 引擎级「已信任对端」快照（平台层持久化信任库用；仅在人工确认后写入）
    pub fn trusted_peers(&self) -> &[TrustedPeer] {
        &self.cfg.trusted_peers
    }

    /// PAIR_DONE 证据链是否已闭合（对端声明的指纹与握手所得一致）
    pub fn pair_evidence_verified(&self) -> bool {
        self.peer_done_verified
    }

    pub fn has_outbound(&self) -> bool {
        !self.out.is_empty()
    }

    /// 取出待发送的 BLE 分片包（平台侧逐包 write/notify）
    pub fn take_outbound(&mut self) -> Vec<Vec<u8>> {
        self.out.drain(..).collect()
    }

    /// 只取走至多 `n` 个待发包，其余**留在队列里**下一轮再取。Windows 侧每片都要
    /// `WriteValueAsync().get()` **阻塞等 GATT 完成**（约 40–140 ms），一次抽干会把唯一的
    /// worker 线程钉住数秒（真机峰值 30.7 s）；worker 一停就不收包、不发心跳，对端只能
    /// 在 20 s 身份时限后判 -212 重连。封顶后单轮阻塞上界 = `n × 写延迟`。
    pub fn take_outbound_n(&mut self, n: usize) -> Vec<Vec<u8>> {
        let k = n.min(self.out.len());
        self.out.drain(..k).collect()
    }

    /// 待发出站包数量（平台层据此判断链路是否在积压）
    pub fn outbound_pending(&self) -> usize {
        self.out.len()
    }

    /// 把**尚未发出**的分片按原顺序放回队首。平台层在"某一片写失败"时必须调用它：分片是
    /// 一条消息的组成部分，丢掉失败那片，接收侧凑不齐组、5 s 后整组作废——**比慢更糟的是
    /// 静默丢数据**。
    pub fn requeue_outbound_front(&mut self, packets: Vec<Vec<u8>>) {
        for pkt in packets.into_iter().rev() {
            self.out.push_front(pkt);
        }
    }

    /// 注入本端链路协商到的 ATT MTU（`[MTU_DEFAULT, BLE_MTU_MAX]`，越界忽略）；只影响
    /// **出站**分片长度。接收侧与 MTU 无关：片长由发送方决定，重组器一律按 ATT 规范上限
    /// 判超长——否则两侧 MTU 不一致时会静默丢弃对端全部合法分片。
    pub fn set_ble_mtu(&mut self, mtu: usize) {
        // 下界 23 已保证每片净流 ≥14B，无须再单独判 `stream_per_packet != 0`。
        if !(MTU_DEFAULT..=BLE_MTU_MAX).contains(&mtu) {
            return;
        }
        self.ble_mtu = mtu;
    }

    /// 当前生效的出站 MTU
    pub fn ble_mtu(&self) -> usize {
        self.ble_mtu
    }

    /// 取出上行事件
    pub fn take_events(&mut self) -> Vec<EngineEvent> {
        std::mem::take(&mut self.events)
    }

    /// 取出入站文件分块（大载荷；平台层落盘后按需回 RESUME）
    pub fn take_chunks(&mut self) -> Vec<IncomingChunk> {
        self.in_chunks.drain(..).collect()
    }

    /// 只取走属于 `file_id` 的分块，其余**留在队列里**（用于 `FileDone` 收尾）：一次收完
    /// 多张相册原图时，若整队取走就会把后一张的分块在它自己的 FILE_META 之前喂给平台层，
    /// 那些分块找不到收件会话，只能全被判成孤儿丢弃。
    pub fn take_chunks_for(&mut self, file_id: u64) -> Vec<IncomingChunk> {
        let mut mine = Vec::new();
        let mut rest = VecDeque::new();
        for c in self.in_chunks.drain(..) {
            if c.file_id == file_id {
                mine.push(c);
            } else {
                rest.push_back(c);
            }
        }
        self.in_chunks = rest;
        mine
    }

    /// 入站分块积压多少**字节**，供平台层给 TCP 读线程施压（背压）。
    ///
    /// 引擎**不设**这条上限：封顶在平台侧（Windows 读线程到 8 MiB 就停止再收帧，见
    /// `app.rs` 的 `INBOUND_CHUNK_BYTES_MAX`），因为"读线程不再读"才是有效的背压 —— 它会
    /// 让 socket 缓冲填满、把慢下来这件事传回发送方。引擎自己丢最旧是**错的**：可靠字节流上
    /// 丢一块就是一个永久的洞，比停在原地说不出话更糟。
    /// ⚠ 已知不对称：安卓侧读路径目前没有做这个退让（分块在 `pump()` 里就地写盘，靠"边到边
    /// 排空"维持），慢盘 + 快链路下的积压量没人量过。
    /// 每次现算不记账：队列本来就短，计数器会走漏。
    pub fn in_chunks_backlog(&self) -> usize {
        self.in_chunks.iter().map(|c| c.data.len()).sum()
    }

    // ---------- TCP 通道（绑定 + 文件传输承载） ----------

    /// 是否有待发送的 TCP 完整帧
    pub fn has_tcp_outbound(&self) -> bool {
        !self.out_tcp.is_empty()
    }

    /// 取出待发送的 TCP 完整帧（平台层逐帧写入 socket）
    pub fn take_tcp_outbound(&mut self) -> Vec<Vec<u8>> {
        self.out_tcp.drain(..).collect()
    }

    /// TCP 通道是否已完成绑定（未绑定不允许承载业务消息）
    pub fn is_tcp_bound(&self) -> bool {
        self.tcp_bound
    }

    /// 发起 TCP 通道绑定（TCP socket 建立后调用；须已 PAIRED）。`TcpClient` 立即经 TCP 送
    /// 我方 nonce；`TcpServer` 等对端 nonce 到达后再回送。证明仍经 BLE 已认证通道交换
    /// （第三方无法伪造 `channel_bind_key` 的 HMAC）。
    pub fn begin_tcp_binding(&mut self, role: BindRole) -> bool {
        let Some(key) = self.session_key else {
            self.emit_error(err_code::IO_GENERIC, "会话密钥未建立，无法发起 TCP 绑定");
            return false;
        };
        if !self.is_paired() {
            self.emit_error(err_code::IO_GENERIC, "未配对，不发起 TCP 绑定");
            return false;
        }
        let role_name = match role {
            BindRole::TcpClient => "client",
            BindRole::TcpServer => "server",
        };
        debuglog::log!(
            Level::Info,
            "session",
            "tcp.bind.start",
            &[("role", role_name)]
        );
        let mut b = ChannelBinding::new(&key, role);
        if role == BindRole::TcpClient {
            match b.make_tcp_payload() {
                Ok(my) => {
                    self.send_plain_tcp(msg_type::CHANNEL_BIND, &my);
                    self.bind_tcp_nonce_sent = true;
                }
                Err(e) => {
                    self.emit_error(err_code::IO_GENERIC, format!("绑定 nonce 生成失败: {e}"));
                    return false;
                }
            }
        }
        self.binding = Some(b);
        true
    }

    /// 喂入 TCP 通道收到的完整帧字节（平台层从 socket 读取）
    pub fn feed_tcp(&mut self, frame: &[u8], now: Instant) {
        // 只记 TCP 侧的时间戳：`last_rx` 是 **BLE** 的存活判据，被 TCP 流量顶掉的话，
        // "蓝牙断了但局域网还在跑"会被误判成健康，BLE 那侧就再也不会超时重连。
        self.last_tcp_rx = Some(now);
        self.on_frame(frame, true);
    }

    /// TCP 通道断开（socket 关闭 / 错误）：**在途传输当场大声失败**，等平台重连后由用户重发。
    /// 不写"回退 BLE"：256KB 分块在 BLE 上结构上带不动，回退只会把数据丢进队列再静默扔掉。
    pub fn on_tcp_closed(&mut self, reason: impl Into<String>) {
        if self.tcp_bound || self.binding.is_some() {
            self.tcp_bound = false;
            self.binding = None;
            self.bind_tcp_nonce_sent = false;
            self.bind_ble_proof_sent = false;
            // 通道都断了，暂存的帧不可能再有意义地补投给"这条链路"——清掉并说明数量，
            // 免得留下一堆旧帧在下一次绑定时突然冒出来
            if !self.pending_tcp_prebind.is_empty() {
                debuglog::log!(
                    Level::Warn,
                    "session",
                    "tcp.prebind_drop",
                    &[("frames", &self.pending_tcp_prebind.len().to_string())]
                );
                self.pending_tcp_prebind.clear();
            }
            let reason = reason.into();
            debuglog::log!(Level::Warn, "session", "tcp.unbind", &[("reason", &reason)]);
            self.emit(EngineEvent::TcpUnbound { reason });
            if let FileChannel::Tcp(file_id) = self.file_channel {
                self.file_channel = FileChannel::Idle;
                self.emit(EngineEvent::FileTaskFailed {
                    file_id,
                    reason: "传输中 TCP 通道断开，文件未完成（不会改走蓝牙）".to_string(),
                });
            }
            // 上面那条只覆盖发送方。本机正在**接收**的文件分块只会从 TCP 进来，通道断了就
            // 永远等不到下一块——不在这里判失败，任务行会一直挂着「接收中」。
            let recvs: Vec<u64> = self.recv_open.drain(..).collect();
            for file_id in recvs {
                self.emit(EngineEvent::FileTaskFailed {
                    file_id,
                    reason: "接收中 TCP 通道断开，文件未完成（不会改走蓝牙）".to_string(),
                });
            }
        }
    }

    /// 出站 TCP 队列的在途上限（帧数）。16 × 256 KB ≈ 4 MB 在途，
    /// 足够喂满千兆链路，又不会让内存随文件大小增长。
    const TCP_OUT_WINDOW: usize = 16;

    /// 绑定完成前允许暂存的 TCP 业务帧数。绑定期只有几百毫秒，16 帧足够覆盖
    /// "对端刚绑定就连发剪贴板/通知"的正常突发；超出即视为异常并要求重连，
    /// 而不是让内存无界增长。
    const MAX_PENDING_TCP_PREBIND: usize = 16;

    /// 已取消 `file_id` 的登记上限。一次会话里用户不会取消几十次传输，
    /// 到了上限就丢最旧的登记（内存有界，4 GB 机型上也只是几百字节）。
    const MAX_CANCELLED_FILES: usize = 32;
    /// 在途接收的登记上限：一次会话里对端同时往这里推 8 个文件已属异常，
    /// 超过就不记（并打 `recv.open_overflow`），换取这张表不会随恶意帧增长。
    const MAX_RECV_OPEN_TRACKED: usize = 8;

    /// 取消之后被丢弃的迟到帧数量（分块 / 结束帧）。平台侧的控制面把它读出来，
    /// 才能区分"对端确实停了"和"对端还在发、这边在悄悄扔"。
    pub fn late_frames_dropped(&self) -> u64 {
        self.late_frames_dropped
    }

    /// 登记一次取消（FIFO 淘汰旧登记）
    fn mark_file_cancelled(&mut self, file_id: u64) {
        if self.cancelled_files.contains(&file_id) {
            return;
        }
        self.cancelled_files.push_back(file_id);
        while self.cancelled_files.len() > Self::MAX_CANCELLED_FILES {
            self.cancelled_files.pop_front();
        }
    }

    /// 一次取消是否已被登记过（重复取消不许再发一遍控制帧）
    fn file_cancel_seen(&self, file_id: u64) -> bool {
        self.cancelled_files.contains(&file_id)
    }

    /// 登记一条在途接收：入站 FILE_META 就是"对端开始往这里发"。
    fn note_recv_open(&mut self, file_id: u64) {
        if self.recv_open.contains(&file_id) {
            return;
        }
        if self.recv_open.len() >= Self::MAX_RECV_OPEN_TRACKED {
            // 对端同时开这么多接收不是正常用法；宁可不记也不能无上限涨内存。
            // 但必须说出来：没登记上的那条在断链时拿不到失败通知，界面会一直挂着。
            debuglog::log!(
                Level::Warn,
                "session",
                "recv.open_overflow",
                &[
                    ("file_id", &format!("{:#x}", file_id)),
                    ("tracked", &self.recv_open.len().to_string()),
                ]
            );
            return;
        }
        self.recv_open.push_back(file_id);
    }

    /// 注销一条在途接收（入站 FILE_DONE 不论成败都算收尾）。
    fn note_recv_closed(&mut self, file_id: u64) {
        self.recv_open.retain(|id| *id != file_id);
    }

    /// 迟到帧记账：计数 + 埋点。**绝不静默吞掉**——本项目被"一侧以为发了、
    /// 另一侧根本没解出来"烫过，丢弃必须留下可查的痕迹。
    fn note_late_frame(&mut self, kind: &str, file_id: u64, detail: String) {
        self.late_frames_dropped += 1;
        debuglog::log!(
            Level::Warn,
            "session",
            "file.late_frame",
            &[
                ("kind", kind),
                ("file_id", &format!("{:#x}", file_id)),
                ("detail", &detail),
                ("dropped", &self.late_frames_dropped.to_string()),
            ]
        );
    }

    /// 平台层据此节流：当前还有多少帧没被写走。
    pub fn tcp_out_depth(&self) -> usize {
        self.out_tcp.len()
    }

    /// 在途窗口大小（平台层算预算要用它，别让两端各抄一份常量）。
    pub fn tcp_out_window(&self) -> usize {
        Self::TCP_OUT_WINDOW
    }

    /// 出站组帧到 TCP 队列（完整帧，不分片）
    fn send_frame_tcp(&mut self, header: FrameHeader, body: &[u8]) {
        match assemble_frame(&header, body) {
            Ok(frame) => self.out_tcp.push_back(frame),
            Err(e) => self.emit_error(err_code::IO_GENERIC, format!("TCP 组帧失败: {e}")),
        }
    }

    /// TCP 通道明文帧（仅 CHANNEL_BIND / PING 等控制消息；未绑定期）
    fn send_plain_tcp(&mut self, msg_type: u8, body: &[u8]) {
        let seq = self.next_hs_seq();
        let header = FrameHeader::new(msg_type, 0, seq, body.len() as u32);
        self.send_frame_tcp(header, body);
    }

    /// TCP 通道加密帧（业务消息；与 BLE 同构，仅传输方式不同）
    fn send_encrypted_tcp(&mut self, msg_type: u8, plaintext: &[u8]) -> bool {
        // 窗口只用来给**分块**做背压（16 × 256 KB）：只有分块会成百上千地堆，而它本来就被
        // "在途预算"节流。控制帧一起撞上限就会被丢——FILE_DONE{cancelled} 落在满窗口上静默
        // 丢弃时，发端显示"已取消"而对端永远停在"传输中"。结束帧恰恰是最不能丢的那一帧。
        if msg_type == linkx_protocol::msg_type::FILE_CHUNK
            && self.out_tcp.len() >= Self::TCP_OUT_WINDOW
        {
            debuglog::log!(
                Level::Warn,
                "session",
                "tcp.queue_full",
                &[("depth", &self.out_tcp.len().to_string())]
            );
            return false;
        }
        let Some(key) = self.session_key else {
            self.emit_error(err_code::IO_GENERIC, "未配对，消息未发送");
            return false;
        };
        let Some(seq) = self.next_send_seq() else {
            self.emit_error(
                err_code::IO_GENERIC,
                "发送序号已耗尽，需重握手以免 nonce 复用",
            );
            return false;
        };
        let header = FrameHeader::default_encrypted(msg_type, seq, plaintext.len() as u32);
        let aad = header.encode();
        let dir = self.send_dir();
        match encrypt_payload(&key, dir, &self.session_id, seq, &aad, plaintext) {
            Ok(cipher) => {
                self.send_frame_tcp(header, &cipher);
                true
            }
            Err(e) => {
                self.emit_error(err_code::IO_GENERIC, format!("加密失败: {e}"));
                false
            }
        }
    }

    /// 绑定消息处理：`from_tcp = true` 为 TCP 上的 nonce 交换，
    /// `false` 为 BLE 已认证通道上的 proof。
    fn on_channel_bind(&mut self, body: &[u8], from_tcp: bool) {
        let Some(mut b) = self.binding.take() else {
            // 未发起绑定却收到 CHANNEL_BIND：忽略（可能是上一会话的残留）。埋点必须打：
            // 不打的话表现就是"绑定发起了却永远完不成"，什么都查不出来。
            debuglog::log!(
                Level::Warn,
                "session",
                "tcp.bind.orphan",
                &[("from", if from_tcp { "tcp" } else { "ble" })]
            );
            return;
        };
        debuglog::log!(
            Level::Info,
            "session",
            "tcp.bind.step",
            &[
                ("from", if from_tcp { "tcp" } else { "ble" }),
                (
                    "role",
                    match b.role() {
                        crate::binding::BindRole::TcpClient => "client",
                        crate::binding::BindRole::TcpServer => "server",
                    }
                ),
            ]
        );
        if from_tcp {
            if let Err(e) = b.on_tcp_payload(body) {
                self.emit_error(err_code::IO_GENERIC, format!("TCP nonce 交换失败: {e}"));
                self.binding = Some(b);
                return;
            }
            // 回送我方的 nonce（仅首次；避免双方无限互发）
            if !self.bind_tcp_nonce_sent {
                match b.make_tcp_payload() {
                    Ok(my) => {
                        self.send_plain_tcp(msg_type::CHANNEL_BIND, &my);
                        self.bind_tcp_nonce_sent = true;
                    }
                    Err(e) => {
                        self.emit_error(err_code::IO_GENERIC, format!("绑定 nonce 生成失败: {e}"));
                    }
                }
            }
            // 已拿到对端 nonce → 经 BLE 送我方 proof（仅首次）
            if !self.bind_ble_proof_sent {
                match b.make_proof_ble_payload() {
                    Ok(proof) => {
                        self.send_plain(msg_type::CHANNEL_BIND, &proof);
                        self.bind_ble_proof_sent = true;
                    }
                    Err(e) => {
                        self.emit_error(err_code::IO_GENERIC, format!("绑定证明生成失败: {e}"));
                    }
                }
            }
        } else {
            // BLE 上收到 proof：验证通过即判定 TCP 对端 = 已配对设备
            match b.verify_ble_proof(body) {
                Ok(()) => {
                    self.tcp_bound = true;
                    debuglog::log!(Level::Info, "session", "tcp.bound", &[]);
                    self.emit(EngineEvent::TcpBound);
                    // 绑定期到达的业务帧在此原序补投，否则就是"对端已发、我这边凭空少一条"
                    self.drain_tcp_prebind();
                }
                // 绑定早已完成又收到 proof（重复投递或迟到的一份）：保持现有绑定。
                // 把它当失败会**把刚建立的健康局域网拆掉**；nonce 路径在 `on_tcp_payload`
                // 里已经做了幂等区分，proof 路径此前漏了这一步。
                Err(crate::binding::BindError::AlreadyBound) => {
                    debuglog::log!(
                        Level::Info,
                        "session",
                        "tcp.proof_repeat",
                        &[("kept", "bound")]
                    );
                }
                Err(e) => {
                    self.emit_error(
                        err_code::IO_GENERIC,
                        format!("TCP 通道绑定失败（疑似第三方冒充）: {e}"),
                    );
                    self.tcp_bound = false;
                }
            }
        }
        self.binding = Some(b);
    }

    fn emit(&mut self, ev: EngineEvent) {
        self.events.push(ev);
    }

    fn emit_state(&mut self) {
        let code = match self.mgr.state {
            SessionState::Discover => state_code::DISCOVER,
            SessionState::Handshake => state_code::HANDSHAKE,
            SessionState::Pairing => state_code::PAIRING,
            SessionState::SasCompare => state_code::SAS_COMPARE,
            SessionState::Paired => state_code::PAIRED,
            SessionState::Repaired => state_code::REPAIRED,
            SessionState::Reconnecting => state_code::RECONNECTING,
            SessionState::Closed => state_code::CLOSED,
        };
        self.emit(EngineEvent::StateChanged { state: code });
    }

    fn emit_error(&mut self, code: i32, context: impl Into<String>) {
        let context = context.into();
        debuglog::log!(
            Level::Error,
            "session",
            "error",
            &[("code", &code.to_string()), ("ctx", &context)]
        );
        self.emit(EngineEvent::Error { code, context });
    }

    /// 状态机迁移（非法迁移不致命：记错误事件并保持原状态）
    fn transition(&mut self, ev: SessionEvent) {
        let from = self.mgr.state;
        match self.mgr.transition(ev) {
            Ok(_) => {
                debuglog::log!(
                    Level::Info,
                    "session",
                    "state.transition",
                    &[
                        ("from", state_name(from)),
                        ("to", state_name(self.mgr.state)),
                    ]
                );
                self.emit_state()
            }
            Err(e) => self.emit_error(err_code::IO_GENERIC, format!("状态迁移失败: {e}")),
        }
    }

    // ---------- 出站 ----------

    /// 本端发送用的 nonce 方向标签（发起方 / 响应方各占一个方向空间）
    fn send_dir(&self) -> u8 {
        match self.cfg.role {
            EngineRole::Initiator => DIR_INITIATOR_TO_RESPONDER,
            EngineRole::Responder => DIR_RESPONDER_TO_INITIATOR,
        }
    }

    /// 本端接收用的 nonce 方向标签（对端发送方向）
    fn recv_dir(&self) -> u8 {
        match self.cfg.role {
            EngineRole::Initiator => DIR_RESPONDER_TO_INITIATOR,
            EngineRole::Responder => DIR_INITIATOR_TO_RESPONDER,
        }
    }

    fn push_frame(&mut self, header: FrameHeader, body: &[u8]) {
        let frame = match assemble_frame(&header, body) {
            Ok(f) => f,
            Err(e) => {
                self.emit_error(err_code::IO_GENERIC, format!("组帧失败: {e}"));
                return;
            }
        };
        let msg_id = self.frag_id;
        self.frag_id = self.frag_id.wrapping_add(1);
        // 分片失败返回错误事件，不 assert 终止进程
        match split_into_packets(msg_id, self.ble_mtu, &frame) {
            Ok(packets) => {
                for pkt in packets {
                    self.out.push_back(pkt);
                }
            }
            Err(e) => self.emit_error(err_code::IO_GENERIC, format!("分片失败: {e}")),
        }
    }

    fn next_hs_seq(&mut self) -> u32 {
        let s = self.hs_seq;
        self.hs_seq = self.hs_seq.wrapping_add(1);
        s
    }

    /// 取下一个业务发送序号；`u32::MAX` 时返回 None（回绕前终止，避免 nonce 复用）
    fn next_send_seq(&mut self) -> Option<u32> {
        if self.send_seq == u32::MAX {
            return None;
        }
        let s = self.send_seq;
        self.send_seq = self.send_seq.wrapping_add(1);
        Some(s)
    }

    fn next_msg_id(&mut self) -> [u8; 16] {
        self.msg_id = self.msg_id.wrapping_add(1);
        let mut id = [0u8; 16];
        id[8..].copy_from_slice(&(self.msg_id as u64).to_be_bytes());
        // 前 8B 放设备侧熵，避免双端 msg_id 空间重合难辨
        id[..8].copy_from_slice(&self.local_dev_id());
        id
    }

    /// 本机 8B 设备 id（由长期私钥派生，稳定可复现；仅展示/去重用）
    fn local_dev_id(&self) -> [u8; 8] {
        let mut h = Sha256::new();
        h.update(b"linkx/v1/dev-id");
        h.update(self.cfg.local_static_sk);
        let d = h.finalize();
        let mut out = [0u8; 8];
        out.copy_from_slice(&d[..8]);
        out
    }

    /// 握手/发现阶段消息（明文帧）
    fn send_plain(&mut self, msg_type: u8, body: &[u8]) {
        let seq = self.next_hs_seq();
        let header = FrameHeader::new(msg_type, 0, seq, body.len() as u32);
        self.push_frame(header, body);
    }

    /// 业务消息（加密帧：信封 + protobuf body）
    fn send_encrypted(&mut self, msg_type: u8, plaintext: &[u8]) -> bool {
        let Some(key) = self.session_key else {
            self.emit_error(err_code::IO_GENERIC, "未配对，消息未发送");
            return false;
        };
        let Some(seq) = self.next_send_seq() else {
            self.emit_error(
                err_code::IO_GENERIC,
                "发送序号已耗尽，需重握手以免 nonce 复用",
            );
            return false;
        };
        // 先构造帧头作为 AEAD 的 AAD（msg_type/flags/seq/len 受完整性保护）
        let header = FrameHeader::default_encrypted(msg_type, seq, plaintext.len() as u32);
        let aad = header.encode();
        let dir = self.send_dir();
        match encrypt_payload(&key, dir, &self.session_id, seq, &aad, plaintext) {
            Ok(cipher) => {
                self.push_frame(header, &cipher);
                true
            }
            Err(e) => {
                self.emit_error(err_code::IO_GENERIC, format!("加密失败: {e}"));
                false
            }
        }
    }

    // ---------- 生命周期 ----------

    /// 启动：发本端 HELLO（双端各自广播自我介绍）
    pub fn start(&mut self, now: Instant) {
        if self.hello_sent {
            return;
        }
        self.hello_sent = true;
        self.last_rx = Some(now);
        self.send_hello();
    }

    /// 会话是否处于「已定型」状态（此时再收到 HELLO 视为对端发起新会话）。只看状态，
    /// **不**看 `hs.is_done()`：握手完成后已进入 `Pairing` / `SasCompare`，那两个阶段属
    /// 「进行中」，不应被重复 HELLO 误复位。
    fn session_settled(&self) -> bool {
        matches!(
            self.mgr.state,
            SessionState::Paired | SessionState::Reconnecting | SessionState::Closed
        )
    }

    /// 清空会话中间态（握手/密钥/序号/重放窗口/配对），保留配置、长期身份与状态机。
    fn reset_session_payload(&mut self) {
        self.hs = None;
        self.hs_writes = 0;
        self.session_key = None;
        self.session_id = [0u8; 16];
        self.peer_fp = None;
        self.pair = None;
        // 身份交换中间态随会话作废（本机身份 `self.identity` 保留）
        self.hs_hash = None;
        self.peer_identity_der = None;
        self.identity_received = false;
        self.identity_verdict_done = false;
        self.identity_deadline = None;
        self.peer_confirm_seen = false;
        self.peer_name = None;
        self.peer_static_fp = None;
        // 未走完的身份漂移不能留到下一条会话：它是"这一次要淘汰哪条旧记录"的凭据，
        // 用户没确认就换了别的设备配对，留着会让下一次 learn_trusted 静默淘汰无关的条目。
        self.drift_old_fp = None;
        self.send_seq = 1;
        self.hs_seq = 0;
        self.recv_window = ReplayWindow::default();
        // 暂存的 TCP 业务帧属于**上一条**链路的会话，跨会话补投等于往新会话里灌旧数据
        self.pending_tcp_prebind.clear();
        self.sas = None;
        self.hello_received = false;
        self.hello_replied = false;
        self.peer_done_verified = false;
        self.last_ping_at = None;
        self.last_tcp_rx = None;
        self.last_tcp_ping_at = None;
        self.last_discover_hello = None;
        // TCP 通道绑定随会话一起作废（平台层须同步关闭 socket）
        self.binding = None;
        self.tcp_bound = false;
        self.bind_deadline = None;
        self.bind_tcp_nonce_sent = false;
        self.bind_ble_proof_sent = false;
        self.file_channel = FileChannel::Idle;
        self.recv_open.clear();
        self.out_tcp.clear();
        self.in_chunks.clear();
    }

    /// 复位为全新会话（保留配置与长期身份）；状态机回到 DISCOVER
    fn reset_session(&mut self) {
        self.reset_session_payload();
        self.mgr = SessionManager::new(SessionChannel::Ble);
        self.reconnect_backoff.reset();
        self.reconnect_next_at = None;
        self.emit_state();
    }

    fn send_hello(&mut self) {
        // 每发一条前进一次：本端"重新贴上来"的那次 HELLO 必须与上一条不同号，
        // 而对端把同一条重投回来时字节不变，正好用来判重。
        self.hello_seq = self.hello_seq.wrapping_add(1);
        let body = encode_hello_body(
            &self.cfg.local_name,
            self.cfg.os,
            &self.cfg.version,
            self.hello_seq,
        );
        self.send_plain(msg_type::HELLO, &body);
    }

    /// 定时驱动：心跳 PING / 超时判定 / 重连退避 / 身份交换超时（平台侧以 ~1s 周期调用）
    pub fn tick(&mut self, now: Instant) {
        match self.mgr.state {
            SessionState::Paired => self.tick_paired(now),
            SessionState::Reconnecting => self.tick_reconnect(now),
            SessionState::Discover => self.tick_discover(now),
            _ => {}
        }
        // 握手已完成但迟迟收不到对端 IDENTITY（版本不匹配 / 对端卡死）→ 超时判失败，
        // 避免双端永远停在 Handshake 静默等待。
        if self.mgr.state == SessionState::Handshake
            && self.hs_hash.is_some()
            && !self.identity_received
        {
            let deadline = *self
                .identity_deadline
                .get_or_insert(now + IDENTITY_EXCHANGE_TIMEOUT);
            if now >= deadline {
                self.emit_error(
                    err_code::IDENTITY_VERIFY_FAILED,
                    "对端未在时限内完成设备身份交换（版本不匹配或连接异常）",
                );
                self.transition(SessionEvent::HeartbeatTimeout);
            }
        }
    }

    /// PAIRED 主态：空闲超时 → 进入重连；到点发 PING
    fn tick_paired(&mut self, now: Instant) {
        // TCP 通道独立心跳 10s/10s，只在已绑定时驱动——未绑定的 TCP 不算业务通道。
        if self.tcp_bound {
            self.tick_tcp_heartbeat(now);
        }
        // 上一条的代价是"未绑定的 TCP 没人判死"，所以这里补一个：连接已 accept、`CHANNEL_BIND`
        // 却永远等不到 ⇒ 到点主动断开。不这么做，局域网里任何主机连上端口**一个字节都不发**，
        // 就能永久占死这个唯一的槽位，真手机之后再也接不上，用户只能重启程序。
        if self.tcp_bound || self.binding.is_none() {
            self.bind_deadline = None;
        } else {
            let deadline = *self.bind_deadline.get_or_insert(now + TCP_BIND_TIMEOUT);
            if now >= deadline {
                self.on_tcp_closed("对端未在时限内完成 TCP 通道绑定");
            }
        }
        let last = self.last_rx.unwrap_or(now);
        let idle = now.saturating_duration_since(last);
        if idle >= self.cfg.heartbeat.ping_interval + self.cfg.heartbeat.timeout {
            debuglog::log!(
                Level::Warn,
                "session",
                "heartbeat.timeout",
                &[("idle_ms", &idle.as_millis().to_string())]
            );
            self.transition(SessionEvent::HeartbeatTimeout);
            self.emit_error(err_code::IO_GENERIC, "心跳超时，进入重连");
            // 启动重连退避（下一次 tick 立即尝试）
            self.reconnect_backoff.reset();
            self.reconnect_next_at = Some(now);
            return;
        }
        let ping_due = match self.last_ping_at {
            None => idle >= self.cfg.heartbeat.ping_interval,
            Some(t) => now.saturating_duration_since(t) >= self.cfg.heartbeat.ping_interval,
        };
        if ping_due {
            self.last_ping_at = Some(now);
            let body = ping_payload();
            let header = FrameHeader::new(
                msg_type::HEARTBEAT,
                0,
                self.next_hs_seq(),
                body.len() as u32,
            );
            self.push_frame(header, &body);
        }
    }

    /// DISCOVER 态自愈：Initiator 定期重播 HELLO。只由 Initiator 发（Windows 侧），
    /// Responder（Android）仍"收到 HELLO 才应答"，避免两端同时广播互相打断。
    fn tick_discover(&mut self, now: Instant) {
        if self.cfg.role != EngineRole::Initiator {
            return;
        }
        let due = match self.last_discover_hello {
            None => true,
            Some(t) => now.saturating_duration_since(t) >= DISCOVER_REATTACH,
        };
        if !due {
            return;
        }
        self.last_discover_hello = Some(now);
        debuglog::log!(Level::Info, "session", "discover.reattach", &[]);
        self.send_hello();
    }

    /// TCP 通道心跳：每 10 s 一次 PING，超过 `ping_interval + timeout` 无入站流量即判定
    /// 这条 LAN 通道已死，拆掉它——在途传输当场大声失败，**不降级 BLE**。
    /// 必须**独立**于 BLE 心跳：共用一个 `last_rx` 时 BLE 活着就会替半死的 TCP "续命"。
    fn tick_tcp_heartbeat(&mut self, now: Instant) {
        let spec = crate::heartbeat::HeartbeatSpec::TCP;
        let last = self.last_tcp_rx.unwrap_or(now);
        let idle = now.saturating_duration_since(last);
        if idle >= spec.ping_interval + spec.timeout {
            debuglog::log!(
                Level::Warn,
                "session",
                "tcp.heartbeat.timeout",
                &[("idle_ms", &idle.as_millis().to_string())]
            );
            self.on_tcp_closed(format!("TCP 心跳超时（静默 {}s）", idle.as_secs()));
            return;
        }
        let due = match self.last_tcp_ping_at {
            None => idle >= spec.ping_interval,
            Some(t) => now.saturating_duration_since(t) >= spec.ping_interval,
        };
        if due {
            self.last_tcp_ping_at = Some(now);
            let body = ping_payload();
            let seq = self.next_hs_seq();
            let header = FrameHeader::new(msg_type::HEARTBEAT, 0, seq, body.len() as u32);
            self.send_frame_tcp(header, &body);
        }
    }

    /// 重连退避驱动：按 1/2/4/8/16/32s 重发 HELLO 触发重握手；6 次耗尽 → DISCOVER。
    /// 收到对端 HELLO 即判重连成功（见 `on_hello` 的 `ReconnectSucceeded` 分支）。
    fn tick_reconnect(&mut self, now: Instant) {
        if self.reconnect_next_at.is_some_and(|t| now < t) {
            return;
        }
        match self.reconnect_backoff.next_delay() {
            Some(delay) => {
                self.send_hello();
                self.reconnect_next_at = Some(now + delay);
                debuglog::log!(
                    Level::Info,
                    "session",
                    "reconnect.attempt",
                    &[("delay_s", &delay.as_secs().to_string())]
                );
                self.emit_error(err_code::IO_GENERIC, "重连尝试：重新广播 HELLO");
            }
            None => {
                self.reconnect_next_at = None;
                debuglog::log!(Level::Warn, "session", "reconnect.exhausted", &[]);
                // 必须是**整场会话复位**，不能只 `transition(ReconnectExhausted)`：只转状态
                // 会残留 `hello_received` / `hs_seq` / 重放窗口 / 会话密钥，对端后来的 HELLO
                // 撞上 `if !self.hello_received` 这个门就被跳过，两端永远停在 DISCOVER 干等。
                self.reset_session();
                self.emit_error(err_code::IO_GENERIC, "重连 6 次失败，回到发现态");
            }
        }
    }

    // ---------- 入站 ----------

    /// 喂入 BLE 分片包字节（来自 GATT 写请求 / 通知回调）
    pub fn feed(&mut self, data: &[u8], now: Instant) {
        self.last_rx = Some(now);
        // 分片长度自适应：对端能发出多长的片，就**证明**这条链路至少支持到那个尺寸，本端
        // 出站分片据此抬升（只升不降，由 `set_ble_mtu` 夹在 [23,517] 内）。用"观测"而不是
        // "查询"：查 `MaxPduSize` 要在 worker 上反复创建又立即丢弃 COM 会话对象，与堆损坏
        // 崩溃相关，且依赖各平台五花八门的 MTU 查询接口。
        if data.len() > self.ble_mtu {
            self.set_ble_mtu(data.len().min(BLE_MTU_MAX));
        }
        match self.reasm.feed(now, data) {
            Ok(Some(frame)) => self.on_frame(&frame, false),
            Ok(None) => {}
            Err(e) => self.emit_error(err_code::IO_GENERIC, format!("BLE 分片错误: {e}")),
        }
    }

    /// `from_tcp`：该帧来自 TCP 通道（决定 CHANNEL_BIND 的 nonce/proof 语义）
    fn on_frame(&mut self, frame: &[u8], from_tcp: bool) {
        let (header, body) = match parse_full_frame(frame) {
            Ok(x) => x,
            Err(e) => {
                self.emit_error(err_code::IO_GENERIC, format!("整帧解析失败: {e}"));
                return;
            }
        };
        match header.msg_type {
            msg_type::HELLO => self.on_hello(&body),
            msg_type::CHALLENGE | msg_type::REPLY => self.on_handshake(header.msg_type, &body),
            msg_type::CHANNEL_BIND => self.on_channel_bind(&body, from_tcp),
            msg_type::HEARTBEAT => self.on_heartbeat(&body, from_tcp),
            // 绑定完成之前，TCP 那条链路还没验明"socket 对端就是蓝牙那台设备"，故不采纳它的
            // 业务帧；但两端完成绑定的时刻天然不同步，直接丢就是静默丢数据——先有界暂存，
            // 绑定完成时原序补投。
            _ if from_tcp && !self.tcp_bound => self.park_tcp_prebind(frame),
            _ => self.on_encrypted(&header, &body),
        }
    }

    /// 绑定完成前收到的 TCP 业务帧：暂存待补投，满了才报错丢弃。
    fn park_tcp_prebind(&mut self, frame: &[u8]) {
        if self.pending_tcp_prebind.len() >= Self::MAX_PENDING_TCP_PREBIND {
            self.emit_error(
                err_code::IO_GENERIC,
                format!(
                    "TCP 绑定迟迟未完成，暂存业务帧已满（{} 帧），本帧丢弃——请重连",
                    Self::MAX_PENDING_TCP_PREBIND
                ),
            );
            return;
        }
        debuglog::log!(
            Level::Info,
            "session",
            "tcp.prebind_park",
            &[("queued", &(self.pending_tcp_prebind.len() + 1).to_string())]
        );
        self.pending_tcp_prebind.push_back(frame.to_vec());
    }

    /// 绑定完成：把暂存的 TCP 业务帧按到达顺序补投。
    fn drain_tcp_prebind(&mut self) {
        let held = std::mem::take(&mut self.pending_tcp_prebind);
        if !held.is_empty() {
            debuglog::log!(
                Level::Info,
                "session",
                "tcp.prebind_flush",
                &[("frames", &held.len().to_string())]
            );
        }
        for frame in held {
            let (header, body) = match parse_full_frame(&frame) {
                Ok(x) => x,
                Err(e) => {
                    self.emit_error(err_code::IO_GENERIC, format!("补投时整帧解析失败: {e}"));
                    continue;
                }
            };
            self.on_encrypted(&header, &body);
        }
    }

    fn on_hello(&mut self, body: &[u8]) {
        let name = tlv_codec::get(body, TAG_ADVERT_NAME)
            .ok()
            .flatten()
            // 收端也按同一个上限截：发端截断只说明"我不会发长的"，不说明"我收到的不会长"。
            // 截的时候退到一个字符的开头，劈开多字节字符会让对端名字整个变成空的。
            .map(|mut v| {
                let mut end = v.len().min(HELLO_NAME_MAX);
                while end > 0 && end < v.len() && (v[end] & 0xC0) == 0x80 {
                    end -= 1;
                }
                v.truncate(end);
                v
            })
            .and_then(|v| String::from_utf8(v).ok())
            .unwrap_or_default();
        let os = tlv_codec::get(body, TAG_OS)
            .ok()
            .flatten()
            .and_then(|v| v.first().copied())
            .unwrap_or(0);
        let version = tlv_codec::get(body, TAG_VERSION)
            .ok()
            .flatten()
            .and_then(|v| String::from_utf8(v.to_vec()).ok())
            .unwrap_or_default();
        // 对端这一次 HELLO 的序号（旧版本不带这个 tag → None）
        let seq = tlv_codec::get(body, TAG_HELLO_SEQ)
            .ok()
            .flatten()
            .and_then(|v| <[u8; 8]>::try_from(v).ok())
            .map(u64::from_be_bytes);
        // 已定型状态（已配对 / 重连中 / 已关闭）又收到 HELLO → 对端发起了**新会话**（例如它
        // 重建了引擎）。必须复位再走一遍握手，否则本端以「旧会话状态」忽略新握手 → 双方卡死；
        // 处于 Handshake / Pairing / SasCompare（进行中）时不复位，那多半是重复 HELLO。
        // 序号解决的是"同一条 HELLO 被重投两次"：字节没变的那条不复位（复位会把在途传输打死）。
        // **没解决**另一件事：局域网里任意主机发个 HELLO（换个序号、或干脆不带序号）照样能把
        // 已配对会话打回 DISCOVER。要免疫那个，复位得走认证路径，属于协议级改动，另案。
        if self.hello_received && self.session_settled() {
            if self.mgr.state == SessionState::Reconnecting {
                // 本端处于重连退避时见到对端 → 判定重连成功（RECONNECTING → HANDSHAKE）
                self.reset_session_payload();
                self.reconnect_backoff.reset();
                self.reconnect_next_at = None;
                self.transition(SessionEvent::ReconnectSucceeded);
            } else if seq.is_some() && seq == self.peer_hello_seq {
                debuglog::log!(
                    Level::Info,
                    "session",
                    "peer.hello_repeat",
                    &[("reset", "skipped")]
                );
            } else {
                // PAIRED / CLOSED：整场会话作废，回到 DISCOVER 重新开始
                self.reset_session();
            }
        }
        if seq.is_some() {
            // 记下对端实例序号：本端复位不能把它一起清掉，它标识的是**对端**那个实例
            self.peer_hello_seq = seq;
        }
        if !self.hello_received {
            self.hello_received = true;
            self.peer_name = Some(name.clone()); // 同名新身份识别用
            if matches!(
                self.mgr.state,
                SessionState::Discover | SessionState::Closed
            ) {
                self.transition(SessionEvent::PeerDiscovered);
            }
            // HELLO 请求 / 应答语义：本端 HELLO 可能在「尚无连接时」发出而丢失（Android 起得
            // 比 Windows 连上早），故收到对端 HELLO 必须**回一个**，否则 Initiator 等不到对端
            // HELLO 就不发 msg1，握手死锁。
            if !self.hello_replied {
                self.hello_replied = true;
                self.send_hello();
            }
            debuglog::log!(
                Level::Info,
                "session",
                "peer.hello",
                &[("name", &name), ("os", &os.to_string()), ("ver", &version),]
            );
            self.emit(EngineEvent::PeerHello { name, os, version });
        }
        // Initiator 收到 HELLO 后立即发 msg1（Noise XX 第一段）
        if self.cfg.role == EngineRole::Initiator && self.hs.is_none() {
            self.begin_handshake();
            self.write_next_handshake();
        }
    }

    fn begin_handshake(&mut self) {
        if self.hs.is_some() {
            return;
        }
        if matches!(
            self.mgr.state,
            SessionState::Discover | SessionState::Closed
        ) {
            self.transition(SessionEvent::PeerDiscovered);
        }
        match NoiseXxHandshake::new(self.cfg.role.noise_role(), Some(&self.cfg.local_static_sk)) {
            Ok(hs) => self.hs = Some(hs),
            Err(e) => {
                self.emit_error(err_code::IO_GENERIC, format!("Noise 初始化失败: {e}"));
            }
        }
    }

    /// 按当前进度写出下一段握手消息（msg1/msg2/msg3 → CHALLENGE/REPLY/CHALLENGE）
    fn write_next_handshake(&mut self) {
        let Some(hs) = self.hs.as_mut() else {
            return;
        };
        let msg = match hs.write_message(&[]) {
            Ok(m) => m,
            Err(e) => {
                self.emit_error(err_code::IO_GENERIC, format!("握手写出失败: {e}"));
                return;
            }
        };
        self.hs_writes += 1;
        let kind = if self.cfg.role == EngineRole::Initiator && self.hs_writes == 1 {
            msg_type::CHALLENGE // msg1 = e_i
        } else if self.cfg.role == EngineRole::Responder && self.hs_writes == 1 {
            msg_type::REPLY // msg2 = e_r || es_r
        } else {
            msg_type::CHALLENGE // msg3 = s_i || se_i
        };
        self.send_plain(kind, &msg);
        if self.hs.as_ref().is_some_and(|h| h.is_done()) {
            self.finish_handshake();
        }
    }

    fn on_handshake(&mut self, kind: u8, body: &[u8]) {
        // 角色 ↔ 消息类型守卫（防角色错配造成的无意义解析）
        let expected_by_responder = kind == msg_type::CHALLENGE;
        let expected_by_initiator = kind == msg_type::REPLY;
        match self.cfg.role {
            EngineRole::Responder if !expected_by_responder => {
                self.emit_error(err_code::IO_GENERIC, "Responder 收到非 CHALLENGE 握手段");
                return;
            }
            EngineRole::Initiator if !expected_by_initiator => {
                self.emit_error(err_code::IO_GENERIC, "Initiator 收到非 REPLY 握手段");
                return;
            }
            _ => {}
        }
        self.begin_handshake();
        let read_ok = match self.hs.as_mut() {
            Some(hs) => match hs.read_message(body) {
                Ok(_) => true,
                Err(e) => {
                    self.emit_error(err_code::IO_GENERIC, format!("握手读入失败: {e}"));
                    false
                }
            },
            None => false,
        };
        if !read_ok {
            return;
        }
        if self.hs.as_ref().is_some_and(|h| h.is_done()) {
            self.finish_handshake();
            return;
        }
        // 未完成 → 继续写下一段（Responder 读 msg1 后写 msg2；Initiator 读 msg2 后写 msg3）
        self.write_next_handshake();
    }

    /// 握手完成：派生会话密钥 / SAS → **发起 RSA 身份交换**。
    /// 「信任裁决」不在此处做，等双方 IDENTITY 交换、验签通过后（[`Self::maybe_complete_identity`]）
    /// 再判：把**持久身份**绑定到**本次握手哈希**，中间人无法用旧公钥顶替（签名覆盖 hash_h）。
    fn finish_handshake(&mut self) {
        let Some(hs) = self.hs.as_mut() else {
            return;
        };
        let (hash, peer_static) = match hs.finish() {
            Ok(x) => x,
            Err(e) => {
                self.emit_error(err_code::IO_GENERIC, format!("握手收尾失败: {e}"));
                return;
            }
        };
        // 会话密钥的 ikm 必须是 DH 出来的秘密（Split(ck) 的两把传输密钥），握手哈希只做 salt：
        // `h` 全由线路字节算出，录到包的人能自己重算，拿它当 ikm 等于链路上没有加密。
        let transport = match hs.transport_split() {
            Ok(t) => t,
            Err(e) => {
                self.emit_error(err_code::IO_GENERIC, format!("会话密钥派生失败: {e}"));
                return;
            }
        };
        let key = derive_session_key(&hash, (&transport.0, &transport.1));
        let sid_hash = {
            let mut h = Sha256::new();
            h.update(b"linkx/v1/session-id");
            h.update(hash);
            h.finalize()
        };
        let mut sid = [0u8; 16];
        sid.copy_from_slice(&sid_hash[..16]);
        self.session_id = sid;
        self.session_key = Some(key);
        self.hs_hash = Some(hash);
        // 对端 X25519 static 指纹仅作诊断留痕，不参与信任判定
        self.peer_static_fp = Some(linkx_crypto::fingerprint(&peer_static));
        self.sas = Some(sas_digits(&hash));
        self.pair = Some(PairFlow::new_from_hash(&hash));
        debuglog::log!(
            Level::Info,
            "session",
            "handshake.done",
            &[
                // 6 位 SAS 是配对鉴别里**人工核对的那一半**，不落盘：「导出 Debug 日志」是把
                // 文件交给别人的动作，留下码等于留下可复用的配对凭据。自动化取这个值走 `/state`
                // 的 `sas` 字段，不从日志里读。
                (
                    "sas_present",
                    if self.sas.is_some() { "true" } else { "false" }
                ),
                ("static_fp", self.peer_static_fp.as_deref().unwrap_or("")),
            ]
        );

        if self.identity.is_none() {
            self.emit_error(
                err_code::IDENTITY_VERIFY_FAILED,
                "本机设备身份不可用（RSA 初始化失败），已拒绝配对",
            );
            self.transition(SessionEvent::UserDisconnect);
            return;
        }
        // 双方在各自握手收尾后立即发 IDENTITY；裁决在双方都收到并验签后统一进行
        self.send_identity();
        // 极端乱序兼容：若对端 IDENTITY 已先到（理论上不会，防御性处理）
        self.maybe_complete_identity();
    }

    /// 发出本端 IDENTITY：`pk_der || sig(hash_h)`（经会话密钥加密）
    fn send_identity(&mut self) {
        let (Some(identity), Some(hash)) = (self.identity.as_ref(), self.hs_hash) else {
            return;
        };
        let (Ok(pk), Ok(sig)) = (identity.public_der(), identity.sign_binding(&hash)) else {
            self.emit_error(err_code::IDENTITY_VERIFY_FAILED, "本机身份签名失败");
            return;
        };
        let Some(body) = encode_identity_payload(&pk, &sig) else {
            self.emit_error(
                err_code::IDENTITY_VERIFY_FAILED,
                "本机身份载荷超出长度前缀上限",
            );
            return;
        };
        let _ = self.send_encrypted(MSG_IDENTITY, &body);
    }

    /// 收到对端 IDENTITY：验签（覆盖 hash_h）→ 记录对端身份 → 尝试完成裁决
    fn on_identity(&mut self, plaintext: &[u8]) {
        let Some(hash) = self.hs_hash else {
            self.emit_error(err_code::IO_GENERIC, "IDENTITY 先于握手完成到达，已丢弃");
            return;
        };
        let Some((pk, sig)) = decode_identity_payload(plaintext) else {
            self.emit_error(err_code::IO_GENERIC, "IDENTITY 载荷畸形");
            return;
        };
        if !verify_binding(&pk, &hash, &sig) {
            // -212：身份校验失败（签名不覆盖本次握手 / 公钥不匹配 / 被替换）
            self.emit_error(
                err_code::IDENTITY_VERIFY_FAILED,
                "设备身份校验失败（签名与本次握手不匹配），已终止配对",
            );
            self.transition(SessionEvent::UserDisconnect);
            return;
        }
        self.peer_fp = Some(fingerprint_of_public_der(&pk));
        self.peer_identity_der = Some(pk);
        self.identity_received = true;
        debuglog::log!(
            Level::Info,
            "session",
            "identity.received",
            &[
                ("ok", "true"),
                ("fp", self.peer_fp.as_deref().unwrap_or("")),
            ]
        );
        self.maybe_complete_identity();
    }

    /// 双方身份都就绪后做信任裁决（幂等；由 finish_handshake / on_identity 共同触发）
    fn maybe_complete_identity(&mut self) {
        if self.identity_verdict_done || self.hs_hash.is_none() || !self.identity_received {
            return;
        }
        self.identity_verdict_done = true;
        let fp = self.peer_fp.clone().unwrap_or_default();
        let name = self.peer_name.clone().unwrap_or_default();

        // 以 **RSA 指纹**为信任锚
        let trusted = self
            .cfg
            .trusted_peers
            .iter()
            .find(|t| t.fingerprint == fp)
            .cloned();
        let renamed = self
            .cfg
            .trusted_peers
            .iter()
            .find(|t| !t.name.is_empty() && t.name == name && t.fingerprint != fp)
            .cloned();

        let verdict = if trusted.is_some() {
            TofuVerdict::Matched
        } else if renamed.is_some() {
            // 同名不同指纹：对方大概率重装（身份轮换）→ 需人工重新配对确认
            TofuVerdict::Mismatch
        } else {
            TofuVerdict::NewPeer
        };
        let verdict_name = match verdict {
            TofuVerdict::Matched => "matched",
            TofuVerdict::NewPeer => "new_peer",
            TofuVerdict::Mismatch => "mismatch",
        };
        debuglog::log!(
            Level::Info,
            "session",
            "tofu.verdict",
            &[("verdict", verdict_name), ("fp", &fp)]
        );
        // 不在裁决处学习信任：只有人工确认（confirm_sas）后才写入信任库
        self.transition(SessionEvent::HandshakeComplete { tofu: verdict });
        match verdict {
            TofuVerdict::Matched => {
                // 已信任设备**无感重连**：不发 SAS 弹窗，直接完成
                self.emit(EngineEvent::PeerPaired {
                    fingerprint: fp.clone(),
                });
                if !self.send_pair_done() {
                    self.report_pair_done_lost();
                }
            }
            TofuVerdict::NewPeer => {
                // 全新设备：互换 PAIR_CONFIRM(SAS) → 人工比对
                if !self.send_pair_confirm() {
                    self.note_pairing_frame_lost("PAIR_CONFIRM");
                }
            }
            TofuVerdict::Mismatch => {
                let old = renamed.map(|t| t.fingerprint).unwrap_or_default();
                self.drift_old_fp = Some(old.clone());
                self.emit(EngineEvent::IdentityChanged {
                    name: name.clone(),
                    old_fingerprint: old.clone(),
                    new_fingerprint: fp.clone(),
                });
                self.emit_error(
                    err_code::TOFU_FINGERPRINT_MISMATCH,
                    format!("设备「{name}」身份已变化（旧 {old} → 新 {fp}），需重新配对确认"),
                );
                // 注意：不主动发 PAIR_CONFIRM —— 等用户 `accept_new_fingerprint()` 后再进入 SAS 复核
            }
        }
    }

    /// 人工确认后写入信任库。淘汰只看**指纹**：同一台设备换身份时只把它那条旧记录换掉，
    /// 别的条目一律不动。不能按"名字相同就替换"——同型号安卓机的名字本就一样，配第二台会
    /// 静默抹掉第一台的信任记录；名字不能当信任判据。
    fn learn_trusted(&mut self, fp: String) {
        let name = self.peer_name.clone().unwrap_or_default();
        let drift = self.drift_old_fp.take();
        self.cfg
            .trusted_peers
            .retain(|t| t.fingerprint != fp && Some(t.fingerprint.as_str()) != drift.as_deref());
        self.cfg.trusted_peers.push(TrustedPeer {
            fingerprint: fp,
            name,
        });
    }

    /// 配对过程中的某条帧没能出去。**发给谁、为什么**由 `send_encrypted` 自己报，这里只留一条
    /// 定位到"是哪一步断了"的埋点：两端各自停在哪儿，事后只看这条就够
    fn note_pairing_frame_lost(&mut self, frame: &str) {
        debuglog::log!(
            Level::Warn,
            "session",
            "pair.frame_lost",
            &[("frame", frame)]
        );
    }

    /// 本机已认定配对完成、PAIR_DONE 却没送出去：信任照写（身份已经人工核对过，回滚等于
    /// 让用户重走一遍 SAS），但"对端还停在配对那一步"必须让界面说得出，否则就是
    /// 一边显示已配对、一边一直在转圈
    fn report_pair_done_lost(&mut self) {
        self.note_pairing_frame_lost("PAIR_DONE");
        self.emit_error(
            err_code::IO_GENERIC,
            "配对确认没送到对端：本机已记下这台设备，对端会停在配对那一步，请让两端重新连接一次",
        );
    }

    /// 发 PAIR_CONFIRM。返回值 = 这条到底出去了没有（`send_encrypted` 失败时自己会报原因）
    fn send_pair_confirm(&mut self) -> bool {
        let Some(flow) = self.pair.clone() else {
            return false;
        };
        // 统一走 send_encrypted（方向标签 + 帧头 AAD 一次性处理）
        let body = flow.confirm_plaintext();
        self.send_encrypted(MSG_PAIR_CONFIRM, &body)
    }

    /// 发 PAIR_DONE。`false` = 没出去：本机已经算配对完成，对端却还停在等人核对 SAS 的那一步
    fn send_pair_done(&mut self) -> bool {
        let (Some(flow), Some(peer_fp)) = (self.pair.clone(), self.peer_fp.clone()) else {
            return false;
        };
        let body = flow.pair_done_plaintext(&peer_fp);
        self.send_encrypted(MSG_PAIR_DONE, &body)
    }

    /// 用户确认 SAS 一致（首次配对的最后人工闸门）→ PAIRED + PAIR_DONE + 写入信任库
    pub fn confirm_sas(&mut self) {
        if self.mgr.state != SessionState::SasCompare {
            self.emit_error(err_code::IO_GENERIC, "当前不在 SAS 比对阶段");
            return;
        }
        self.transition(SessionEvent::SasConfirmed);
        if !self.send_pair_done() {
            self.report_pair_done_lost();
        }
        if let Some(fp) = self.peer_fp.clone() {
            // 仅在此刻（人工确认 SAS 一致）把对端身份写入信任库；跨重启由平台侧持久化
            // （Windows trust.json / Android SharedPreferences）。
            self.learn_trusted(fp.clone());
            debuglog::log!(
                Level::Info,
                "session",
                "pair.confirm",
                &[
                    // 确认这一步更不该把码留在盘上——此刻它刚被两端核对过。
                    ("sas_present", "true"),
                    ("fp", &fp),
                ]
            );
            self.emit(EngineEvent::PeerPaired { fingerprint: fp });
        }
    }

    /// 用户拒绝 SAS（或拒绝接受新身份）
    pub fn reject_sas(&mut self) {
        match self.mgr.state {
            SessionState::SasCompare | SessionState::Pairing | SessionState::Repaired => {
                self.transition(SessionEvent::SasRejected);
                self.emit_error(err_code::USER_REJECT_PAIR, "已拒绝本次配对");
                self.session_key = None;
            }
            _ => self.emit_error(err_code::IO_GENERIC, "当前不在配对阶段"),
        }
    }

    /// 同名设备身份变化后，用户接受「重新配对」：Repaired → Pairing（重新互换
    /// PAIR_CONFIRM）→ SasCompare 人工比对 → Paired。绝不跳过 SAS 复核。
    pub fn accept_new_fingerprint(&mut self) {
        if self.mgr.state != SessionState::Repaired {
            self.emit_error(err_code::IO_GENERIC, "当前不在身份复核阶段");
            return;
        }
        self.transition(SessionEvent::FingerprintReAccepted); // → Pairing
        if !self.send_pair_confirm() {
            self.note_pairing_frame_lost("PAIR_CONFIRM");
        }
        // 防死锁：Repaired 期间对端（NewPeer 视角）已发过 PAIR_CONFIRM 而当时无法处理；
        // 此处若它已在手，直接推进到 SAS 比对，避免双端互相等待。
        if self.peer_confirm_seen && self.mgr.state == SessionState::Pairing {
            self.transition(SessionEvent::SasConfirmed); // Pairing → SasCompare
            if let Some(sas) = self.sas {
                self.emit(EngineEvent::SasReady { sas });
            }
        }
    }

    /// 用户主动断开
    pub fn user_disconnect(&mut self) {
        self.transition(SessionEvent::UserDisconnect);
        self.session_key = None;
    }

    fn on_heartbeat(&mut self, body: &[u8], from_tcp: bool) {
        if is_pong(body) {
            // PONG 仅用于刷新 last_rx（feed 已统一更新时间戳）
            return;
        }
        if is_ping(body) {
            // 未配对时收到 PING：静默忽略，等配对完成后对端会重发。把它报成"心跳载荷畸形"
            // 是把时序正常说成对方在发垃圾，界面上会凭空多出错误条。
            if !self.is_paired() {
                return;
            }
            let pong = pong_payload();
            // PONG 必须**从哪条频道来就回哪条去**：不分频道一律 `push_frame`（BLE 出站队列）
            // 时，TCP 上的 PING 会得到一个 BLE 上的 PONG，TCP 侧 `last_rx` 永远刷不新，
            // 10s/10s 的 TCP 心跳形同虚设。
            let seq = self.next_hs_seq();
            let header = FrameHeader::new(msg_type::HEARTBEAT, 0, seq, pong.len() as u32);
            if from_tcp {
                self.send_frame_tcp(header, &pong);
            } else {
                self.push_frame(header, &pong);
            }
            return;
        }
        self.emit_error(err_code::IO_GENERIC, "心跳载荷畸形");
    }

    fn on_encrypted(&mut self, header: &FrameHeader, body: &[u8]) {
        if header.flags & flags::ENCRYPTED == 0 {
            self.emit_error(err_code::IO_GENERIC, "业务消息未加密，已丢弃");
            return;
        }
        let Some(key) = self.session_key else {
            self.emit_error(err_code::IO_GENERIC, "会话密钥未建立，加密消息已丢弃");
            return;
        };
        // 先「只读校验」不提交，待解密（AEAD 认证）成功后再提交窗口：否则伪造任意 seq 的帧
        // 就能推窗，把合法帧判为过旧而丢弃（定向 DoS）。
        if let Err(e) = self.recv_window.check(header.seq) {
            self.emit_error(err_code::IO_GENERIC, format!("抗重放拒绝: {e}"));
            return;
        }
        // 帧头即 AAD；接收方向标签
        let aad = header.encode();
        let dir = self.recv_dir();
        let plaintext = match decrypt_payload(&key, dir, &self.session_id, header.seq, &aad, body) {
            Ok(p) => p,
            Err(e) => {
                self.emit_error(err_code::IO_GENERIC, format!("解密失败: {e}"));
                return;
            }
        };
        // 认证通过 → 提交窗口（此刻才登记，防伪造帧推窗）
        self.recv_window.commit(header.seq);
        // 握手完成 ≠ 配对完成：拿到会话密钥不等于本端接受了她。配对协议自身的三条消息必须
        // 在 PAIRED 之前放行，其余业务消息一律先拒 —— 门禁统一放在分派入口，各处理器里原有的
        // 具体措辞（"播放指令未执行"之类）退为二次防线；以后新增消息类型不必记得再补一次。
        if !self.is_paired()
            && !matches!(
                header.msg_type,
                MSG_IDENTITY | MSG_PAIR_CONFIRM | MSG_PAIR_DONE
            )
        {
            self.emit_error(err_code::IO_GENERIC, "尚未配对，业务消息未采纳");
            return;
        }
        match header.msg_type {
            MSG_IDENTITY => self.on_identity(&plaintext),
            MSG_PAIR_CONFIRM => self.on_pair_confirm(header.seq, &plaintext),
            MSG_PAIR_DONE => self.on_pair_done(header.seq, &plaintext),
            msg_type::NOTIFY_PUSH => self.on_notify_push(&plaintext),
            msg_type::NOTIFY_REPLY => self.on_notify_reply(&plaintext),
            msg_type::NOTIFY_DISMISS => self.on_notify_dismiss(&plaintext),
            msg_type::NOTIFY_REPLY_ACK => self.on_notify_reply_ack(&plaintext),
            msg_type::CLIPBOARD_PUSH => self.on_clipboard_push(&plaintext),
            msg_type::MEDIA_STATE => self.on_media_state(&plaintext),
            msg_type::MEDIA_COMMAND => self.on_media_command(&plaintext),
            msg_type::DEVICE_STATUS => self.on_device_status(&plaintext),
            msg_type::FILE_META => self.on_file_meta(&plaintext),
            msg_type::FILE_CHUNK => self.on_file_chunk(&plaintext),
            msg_type::FILE_DONE => self.on_file_done(&plaintext),
            msg_type::FILE_CANCEL => self.on_file_cancel(&plaintext),
            msg_type::ALBUM_LIST_REQ
            | msg_type::ALBUM_LIST
            | msg_type::ALBUM_THUMB_REQ
            | msg_type::ALBUM_THUMB
            | msg_type::ALBUM_FULL_REQ => self.on_album(header.msg_type, &plaintext),
            msg_type::RESUME => self.on_file_resume(&plaintext),
            msg_type::CONFIG_SYNC => self.on_config_sync(&plaintext),
            other => self.emit_error(
                err_code::IO_GENERIC,
                format!("未知业务消息类型 0x{other:02X}"),
            ),
        }
    }

    /// 相册五条消息的唯一出口：**只走局域网 TCP，未绑定就大声失败**。蓝牙兜底在这里不是
    /// "降级"而是"必然失败"（缩略图几十 KB、原图几 MB），"看起来发了其实丢了"比报错更糟。
    fn send_album(&mut self, mt: u8, body: &[u8], now_ms: i64) -> bool {
        if !self.is_paired() {
            return false;
        }
        if !self.tcp_bound {
            self.emit_error(
                err_code::IO_GENERIC,
                "相册需要局域网通道：TCP 尚未就绪，这条请求没有发出".to_string(),
            );
            return false;
        }
        let msg_id = self.next_msg_id();
        let src = self.local_dev_id();
        let payload = envelope::encode(&msg_id, now_ms, &src, body);
        self.send_encrypted_tcp(mt, &payload)
    }

    /// 电脑 → 手机：要第 `page` 页清单（0 起）
    pub fn send_album_list_request(&mut self, page: u32, per_page: u32, now_ms: i64) -> bool {
        let body = AlbumListRequest { page, per_page }.encode_to_vec();
        self.send_album(msg_type::ALBUM_LIST_REQ, &body, now_ms)
    }

    /// 手机 → 电脑：一页清单
    pub fn send_album_list(&mut self, list: &AlbumList, now_ms: i64) -> bool {
        self.send_album(msg_type::ALBUM_LIST, &list.encode_to_vec(), now_ms)
    }

    /// 电脑 → 手机：要一张缩略图
    pub fn send_album_thumb_request(&mut self, id: u64, edge: u32, now_ms: i64) -> bool {
        let body = AlbumThumbRequest { id, edge }.encode_to_vec();
        self.send_album(msg_type::ALBUM_THUMB_REQ, &body, now_ms)
    }

    /// 手机 → 电脑：一张缩略图（JPEG 随帧走，**不落盘**是产品口径不是实现细节）
    pub fn send_album_thumb(&mut self, thumb: &AlbumThumb, now_ms: i64) -> bool {
        self.send_album(msg_type::ALBUM_THUMB, &thumb.encode_to_vec(), now_ms)
    }

    /// 电脑 → 手机：要这几张原图（手机侧的回包是普通 FILE_META/CHUNK/DONE）
    pub fn send_album_full_request(&mut self, ids: &[u64], now_ms: i64) -> bool {
        let body = AlbumFullRequest { ids: ids.to_vec() }.encode_to_vec();
        self.send_album(msg_type::ALBUM_FULL_REQ, &body, now_ms)
    }

    fn on_album(&mut self, mt: u8, plaintext: &[u8]) {
        let bad = |e: String| format!("相册消息(0x{mt:02x})解析失败: {e}");
        let env = match envelope::decode(plaintext) {
            Ok(e) => e,
            Err(e) => {
                self.emit_error(err_code::IO_GENERIC, bad(e.to_string()));
                return;
            }
        };
        let plaintext = env.body.as_ref();
        match mt {
            msg_type::ALBUM_LIST_REQ => match AlbumListRequest::decode(plaintext) {
                Ok(q) => self.emit(EngineEvent::AlbumListRequested {
                    page: q.page,
                    per_page: q.per_page,
                }),
                Err(e) => self.emit_error(err_code::IO_GENERIC, bad(e.to_string())),
            },
            msg_type::ALBUM_LIST => match AlbumList::decode(plaintext) {
                Ok(v) => self.emit(EngineEvent::AlbumPage {
                    items: v.items,
                    page: v.page,
                    total: v.total,
                    error: v.error,
                }),
                Err(e) => self.emit_error(err_code::IO_GENERIC, bad(e.to_string())),
            },
            msg_type::ALBUM_THUMB_REQ => match AlbumThumbRequest::decode(plaintext) {
                Ok(q) => self.emit(EngineEvent::AlbumThumbRequested {
                    id: q.id,
                    edge: q.edge,
                }),
                Err(e) => self.emit_error(err_code::IO_GENERIC, bad(e.to_string())),
            },
            msg_type::ALBUM_THUMB => match AlbumThumb::decode(plaintext) {
                Ok(v) => self.emit(EngineEvent::AlbumThumb {
                    id: v.id,
                    edge: v.edge,
                    width: v.width,
                    height: v.height,
                    jpeg: v.jpeg.to_vec(),
                    error: v.error,
                }),
                Err(e) => self.emit_error(err_code::IO_GENERIC, bad(e.to_string())),
            },
            _ => match AlbumFullRequest::decode(plaintext) {
                Ok(v) => self.emit(EngineEvent::AlbumFullRequested { ids: v.ids }),
                Err(e) => self.emit_error(err_code::IO_GENERIC, bad(e.to_string())),
            },
        }
    }

    fn on_pair_confirm(&mut self, _seq: u32, plaintext: &[u8]) {
        // 解密成功已证明对端掌握同一会话密钥（seq nonce + AEAD）；此处再比对 SAS 一致性
        let peer_sas = match self
            .pair
            .as_ref()
            .map(|_| crate::pairing::PairConfirm::parse(plaintext))
        {
            Some(Ok(pc)) => pc.sas,
            _ => {
                self.emit_error(err_code::IO_GENERIC, "PAIR_CONFIRM 载荷畸形");
                return;
            }
        };
        match (self.sas, Some(peer_sas)) {
            (Some(mine), Some(peer)) if mine == peer => {}
            (Some(_), Some(_)) => {
                self.transition(SessionEvent::SasRejected);
                self.emit_error(err_code::IO_GENERIC, "PAIR_CONFIRM 的 SAS 与本端不一致");
                // 与用户手点「拒绝」同口径：密钥一并丢掉。否则状态已经判死，会话密钥还在，
                // 之后到达的业务帧仍能被解开并被采纳（终态成了摆设）。
                self.session_key = None;
                return;
            }
            _ => return,
        }
        match self.mgr.state {
            SessionState::Pairing => {
                self.peer_confirm_seen = true;
                self.transition(SessionEvent::SasConfirmed); // Pairing → SasCompare
                if let Some(sas) = self.sas {
                    self.emit(EngineEvent::SasReady { sas });
                }
            }
            SessionState::SasCompare => {
                self.peer_confirm_seen = true; // 已在比对阶段：幂等
            }
            SessionState::Repaired => {
                // 对端（NewPeer 视角）已发 PAIR_CONFIRM，但本端还在等用户「重新配对」决策；
                // 先记住，待 `accept_new_fingerprint()` 时补用（防死锁）。
                self.peer_confirm_seen = true;
            }
            SessionState::Paired => {
                // 幂等：对端仍是「首次配对」视角（例如它清过数据/换了身份）→
                // 回 PAIR_CONFIRM（让对端能看到 SAS 做人工比对）+ PAIR_DONE（帮其完成），
                // 同时把 SAS 上报本端 UI，保证「人工比对」这一步在两侧都真实发生。
                if !self.send_pair_confirm() {
                    self.note_pairing_frame_lost("PAIR_CONFIRM");
                }
                if !self.send_pair_done() {
                    self.report_pair_done_lost();
                }
                if let Some(sas) = self.sas {
                    self.emit(EngineEvent::SasReady { sas });
                }
            }
            _ => {}
        }
    }

    /// 本端 RSA 身份指纹（PAIR_DONE 载荷校验基准）。取不到就是 `None`：把它折叠成空串
    /// 会让"对端声明了一个空指纹"等于校验通过。
    fn own_fingerprint(&self) -> Option<String> {
        self.identity.as_ref().and_then(|i| i.fingerprint().ok())
    }

    fn on_pair_done(&mut self, _seq: u32, plaintext: &[u8]) {
        if self.pair.is_none() || self.session_key.is_none() {
            // 状态不对就丢掉：留一条埋点，否则"对端说配完了、我这没反应"无从对照
            debuglog::log!(
                Level::Warn,
                "session",
                "pair.done_dropped",
                &[("paired", if self.pair.is_some() { "1" } else { "0" })]
            );
            return;
        }
        // 解密成功已证明对端掌握会话密钥；载荷是**对端视角的本端指纹**，故须与本端自身
        // 指纹比对——比对 peer_fp 是方向性错误（会永远判失败）。
        let fp = match tlv_codec::get(plaintext, linkx_protocol::TAG_FINGERPRINT) {
            Ok(Some(v)) => String::from_utf8(v.to_vec()).unwrap_or_default(),
            _ => {
                self.emit_error(err_code::IO_GENERIC, "PAIR_DONE 载荷畸形");
                return;
            }
        };
        let Some(own) = self.own_fingerprint() else {
            self.emit_error(err_code::IO_GENERIC, "本机身份不可用，PAIR_DONE 未采纳");
            return;
        };
        if own == fp {
            self.peer_done_verified = true;
        } else {
            self.emit_error(
                err_code::TOFU_FINGERPRINT_MISMATCH,
                "PAIR_DONE 声明的本端指纹与本机身份不一致",
            );
        }
    }

    fn on_notify_push(&mut self, plaintext: &[u8]) {
        let env = match envelope::decode(plaintext) {
            Ok(e) => e,
            Err(e) => {
                self.emit_error(err_code::IO_GENERIC, format!("通知信封解析失败: {e}"));
                return;
            }
        };
        match NotificationPush::decode(env.body.as_ref()) {
            Ok(n) => self.emit(EngineEvent::Notification {
                package: n.package,
                title: n.title,
                text: n.text,
                post_ts_ms: n.post_ts_ms,
                // 稳定 key 透传给平台层做「就地合并更新」
                key_hash: n.key_hash,
                tag: n.tag,
                notification_id: n.notification_id,
                can_reply: n.can_reply,
                reply_action_index: n.reply_action_index,
                reply_result_key: n.reply_result_key,
            }),
            Err(e) => self.emit_error(err_code::IO_GENERIC, format!("通知解析失败: {e}")),
        }
    }

    /// 电脑 → 手机：回复请求。这条**会真的把文字送进对端某个应用**，所以配对门比播放指令更硬：
    /// 未配对直接不执行（握手完成 ≠ 配对完成，SAS 没核对过就替用户说话是不行的）。
    fn on_notify_reply(&mut self, plaintext: &[u8]) {
        if !self.is_paired() {
            self.emit_error(err_code::IO_GENERIC, "尚未配对，回复请求未执行");
            return;
        }
        let env = match envelope::decode(plaintext) {
            Ok(e) => e,
            Err(e) => {
                self.emit_error(err_code::IO_GENERIC, format!("回复请求信封解析失败: {e}"));
                return;
            }
        };
        match NotificationReply::decode(env.body.as_ref()) {
            Ok(r) => self.emit(EngineEvent::NotifyReplyRequested {
                reply_id: r.reply_id,
                package: r.package,
                tag: r.tag,
                notification_id: r.notification_id,
                action_index: r.action_index,
                result_key: r.result_key,
                text: r.text,
            }),
            Err(e) => self.emit_error(err_code::IO_GENERIC, format!("回复请求解析失败: {e}")),
        }
    }

    /// 手机 → 电脑：一条通知消失了。这里**不判配对**——它是"少了一个入口"的收敛信号，
    /// 收到就多撤一个按钮，收不到电脑也只是继续显示一个点了会报错的入口，没有更坏的情况。
    fn on_notify_dismiss(&mut self, plaintext: &[u8]) {
        let env = match envelope::decode(plaintext) {
            Ok(e) => e,
            Err(e) => {
                self.emit_error(err_code::IO_GENERIC, format!("通知消失信封解析失败: {e}"));
                return;
            }
        };
        match NotificationDismiss::decode(env.body.as_ref()) {
            Ok(d) => self.emit(EngineEvent::NotifyDismissed {
                package: d.package,
                tag: d.tag,
                notification_id: d.notification_id,
                key_hash: d.key_hash,
            }),
            Err(e) => self.emit_error(err_code::IO_GENERIC, format!("通知消失解析失败: {e}")),
        }
    }

    fn on_notify_reply_ack(&mut self, plaintext: &[u8]) {
        let env = match envelope::decode(plaintext) {
            Ok(e) => e,
            Err(e) => {
                self.emit_error(err_code::IO_GENERIC, format!("回复回执信封解析失败: {e}"));
                return;
            }
        };
        match NotificationReplyAck::decode(env.body.as_ref()) {
            Ok(a) => self.emit(EngineEvent::NotifyReplyAck {
                reply_id: a.reply_id,
                package: a.package,
                ok: a.ok,
                error: a.error,
            }),
            Err(e) => self.emit_error(err_code::IO_GENERIC, format!("回复回执解析失败: {e}")),
        }
    }

    fn on_clipboard_push(&mut self, plaintext: &[u8]) {
        let env = match envelope::decode(plaintext) {
            Ok(e) => e,
            Err(e) => {
                self.emit_error(err_code::IO_GENERIC, format!("剪贴板信封解析失败: {e}"));
                return;
            }
        };
        match ClipboardPush::decode(env.body.as_ref()) {
            Ok(c) => {
                if c.data.len() > CLIPBOARD_MAX_BYTES {
                    // 只报长度不报内容：剪贴板正文常是密码与验证码
                    self.emit_error(
                        err_code::IO_GENERIC,
                        format!(
                            "对端推来 {} KB 剪贴板内容，超过 {} KB 上限，已拒收",
                            c.data.len() / 1024,
                            CLIPBOARD_MAX_BYTES / 1024
                        ),
                    );
                    return;
                }
                let text = String::from_utf8_lossy(&c.data).to_string();
                self.emit(EngineEvent::Clipboard { text });
            }
            Err(e) => self.emit_error(err_code::IO_GENERIC, format!("剪贴板解析失败: {e}")),
        }
    }

    // ---------- 文件传输入站 ----------

    fn on_file_meta(&mut self, plaintext: &[u8]) {
        let env = match envelope::decode(plaintext) {
            Ok(e) => e,
            Err(e) => {
                self.emit_error(err_code::IO_GENERIC, format!("文件元信息信封解析失败: {e}"));
                return;
            }
        };
        match FileMeta::decode(env.body.as_ref()) {
            Ok(m) => {
                // 新的 FILE_META = 一次全新的传输：清掉同名 id 的取消登记，
                // 否则"取消后对方重发同一个文件"会被当成迟到帧整段扔掉（那是静默失败）。
                if self.file_cancel_seen(m.file_id) {
                    self.cancelled_files.retain(|id| *id != m.file_id);
                    debuglog::log!(
                        Level::Info,
                        "session",
                        "file.meta.after_cancel",
                        &[("file_id", &format!("{:#x}", m.file_id))]
                    );
                }
                // 摘要只认两种：32B（发端预先算好）或 0B（发端边发边算，摘要延到 FILE_DONE）
                let sha = match m.sha256.len() {
                    0 => None,
                    32 => {
                        let mut b = [0u8; 32];
                        b.copy_from_slice(&m.sha256);
                        Some(b)
                    }
                    other => {
                        self.emit_error(
                            err_code::IO_GENERIC,
                            format!("文件摘要长度非法（{other}B，须 0B 或 32B）"),
                        );
                        return;
                    }
                };
                // 日志只记 ID / 大小 / 块大小，不记文件名与摘要
                debuglog::log!(
                    Level::Info,
                    "session",
                    "file.meta",
                    &[
                        ("file_id", &format!("{:#x}", m.file_id)),
                        ("size", &m.size.to_string()),
                        ("chunk", &m.chunk_size.to_string()),
                        ("sha_deferred", if sha.is_none() { "true" } else { "false" }),
                    ]
                );
                self.note_recv_open(m.file_id);
                self.emit(EngineEvent::FileMetaReceived {
                    file_id: m.file_id,
                    name: m.name,
                    size: m.size,
                    chunk_size: m.chunk_size,
                    sha256: sha,
                    crc32: m.crc32,
                    album_id: m.album_id,
                });
            }
            Err(e) => self.emit_error(err_code::IO_GENERIC, format!("文件元信息解析失败: {e}")),
        }
    }

    fn on_file_chunk(&mut self, plaintext: &[u8]) {
        let env = match envelope::decode(plaintext) {
            Ok(e) => e,
            Err(e) => {
                self.emit_error(err_code::IO_GENERIC, format!("文件分块信封解析失败: {e}"));
                return;
            }
        };
        match FileChunk::decode(env.body.as_ref()) {
            Ok(c) => {
                // 取消之后仍在路上分块：丢掉并计数。不能开新会话（这一路根本没有 META 在等），
                // 也不能不吭声丢掉。
                if self.file_cancel_seen(c.file_id) {
                    self.note_late_frame(
                        "chunk",
                        c.file_id,
                        format!("第 {} 块，{} B", c.index, c.data.len()),
                    );
                    return;
                }
                // 节流：每 64 块记一条；只记 index / len，不记内容
                if c.index.is_multiple_of(64) {
                    debuglog::log!(
                        Level::Info,
                        "session",
                        "file.chunk",
                        &[
                            ("file_id", &format!("{:#x}", c.file_id)),
                            ("index", &c.index.to_string()),
                            ("len", &c.data.len().to_string()),
                        ]
                    );
                }
                // 大载荷走独立队列：FFI 事件流的长度域是 u16，装不下 256KB 分块
                self.in_chunks.push_back(IncomingChunk {
                    file_id: c.file_id,
                    index: c.index,
                    crc32: c.crc32,
                    data: c.data.to_vec(),
                });
            }
            Err(e) => self.emit_error(err_code::IO_GENERIC, format!("文件分块解析失败: {e}")),
        }
    }

    fn on_file_done(&mut self, plaintext: &[u8]) {
        let env = match envelope::decode(plaintext) {
            Ok(e) => e,
            Err(e) => {
                self.emit_error(err_code::IO_GENERIC, format!("文件结束信封解析失败: {e}"));
                return;
            }
        };
        match FileDone::decode(env.body.as_ref()) {
            Ok(d) => {
                let sha = match d.sha256.len() {
                    0 => None,
                    32 => {
                        let mut b = [0u8; 32];
                        b.copy_from_slice(&d.sha256);
                        Some(b)
                    }
                    other => {
                        self.emit_error(
                            err_code::IO_GENERIC,
                            format!("文件结束帧摘要长度非法（{other}B，须 0B 或 32B）"),
                        );
                        return;
                    }
                };
                debuglog::log!(
                    Level::Info,
                    "session",
                    "file.done",
                    &[
                        ("file_id", &format!("{:#x}", d.file_id)),
                        ("ok", if d.ok { "true" } else { "false" }),
                        ("has_err", if d.error.is_some() { "true" } else { "false" }),
                        ("has_sha", if sha.is_some() { "true" } else { "false" }),
                        ("cancelled", if d.cancelled { "true" } else { "false" }),
                    ]
                );
                // 对端的结束帧一到，本机的这条接收就不再"在途"了（成功、失败、取消收尾都算）：
                // 注销登记，断链时才不会对一条已经落定的传输补发失败通知。
                self.note_recv_closed(d.file_id);
                // 本端已经取消过这个 id：这条是对端还没收到取消时发来的结束帧，丢掉并计数，
                // 不许再把它当成一次正常收尾（那会覆盖掉「已取消」，甚至写出已完成）。
                if self.file_cancel_seen(d.file_id) {
                    self.note_late_frame("done", d.file_id, format!("ok={}", d.ok));
                    return;
                }
                // 对端取消收尾：上报「已取消」而不是 FileDoneReceived —— 收端据此走
                // 删除残留文件那条路，而不是把用户的取消显示成失败。
                if d.cancelled {
                    self.mark_file_cancelled(d.file_id);
                    self.emit(EngineEvent::FileTaskCancelled {
                        file_id: d.file_id,
                        reason: cancel_reason(d.error),
                    });
                    return;
                }
                self.emit(EngineEvent::FileDoneReceived {
                    file_id: d.file_id,
                    ok: d.ok,
                    error: d.error,
                    sha256: sha,
                })
            }
            Err(e) => self.emit_error(err_code::IO_GENERIC, format!("文件结束解析失败: {e}")),
        }
    }

    /// FILE_CANCEL（收端 → 发端「停止发送」）：只对在途的那一次传输生效。本端没在发这个
    /// `file_id` 时报出来丢掉——既不当成一次新会话，也不静默吞掉。
    fn on_file_cancel(&mut self, plaintext: &[u8]) {
        let env = match envelope::decode(plaintext) {
            Ok(e) => e,
            Err(e) => {
                self.emit_error(err_code::IO_GENERIC, format!("取消帧信封解析失败: {e}"));
                return;
            }
        };
        let c = match FileCancel::decode(env.body.as_ref()) {
            Ok(c) => c,
            Err(e) => {
                self.emit_error(err_code::IO_GENERIC, format!("取消帧解析失败: {e}"));
                return;
            }
        };
        let pinned = matches!(self.file_channel, FileChannel::Tcp(id) | FileChannel::Ble(id) if id == c.file_id);
        if !pinned {
            self.note_late_frame(
                "cancel",
                c.file_id,
                "本端没有正在发送这个文件（取消帧与在途传输对不上）".to_string(),
            );
            return;
        }
        let reason = cancel_reason(Some(c.reason));
        debuglog::log!(
            Level::Warn,
            "session",
            "file.cancel.recv",
            &[
                ("file_id", &format!("{:#x}", c.file_id)),
                (
                    "channel",
                    match self.file_channel {
                        FileChannel::Tcp(_) => "tcp",
                        FileChannel::Ble(_) => "ble",
                        FileChannel::Idle => "idle",
                    }
                ),
            ]
        );
        // 停手：登记取消（后续同 id 分块一律拒绝）→ 释放通道锁 → 上报「已取消」
        self.mark_file_cancelled(c.file_id);
        self.release_file_channel(c.file_id);
        self.emit(EngineEvent::FileTaskCancelled {
            file_id: c.file_id,
            reason,
        });
    }

    /// 续传请求：载荷为 TLV `TAG_FILE_ID + TAG_RESUME_FROM`
    fn on_file_resume(&mut self, plaintext: &[u8]) {
        let env = match envelope::decode(plaintext) {
            Ok(e) => e,
            Err(e) => {
                self.emit_error(err_code::IO_GENERIC, format!("续传请求信封解析失败: {e}"));
                return;
            }
        };
        let body = env.body.as_ref();
        let file_id = match tlv_codec::get(body, TAG_FILE_ID) {
            Ok(Some(v)) if v.len() == 8 => {
                let mut b = [0u8; 8];
                b.copy_from_slice(&v);
                u64::from_be_bytes(b)
            }
            _ => {
                self.emit_error(err_code::IO_GENERIC, "续传请求缺少合法 TAG_FILE_ID");
                return;
            }
        };
        let from_index = match tlv_codec::get(body, TAG_RESUME_FROM) {
            Ok(Some(v)) if v.len() == 4 => {
                let mut b = [0u8; 4];
                b.copy_from_slice(&v);
                u32::from_be_bytes(b)
            }
            _ => {
                self.emit_error(err_code::IO_GENERIC, "续传请求缺少合法 TAG_RESUME_FROM");
                return;
            }
        };
        // `FILE_DONE` 出口时通道锁已随一次正常收尾释放。迟到的续传请求说的就是"那句说早了"：
        // 不把 file_id 重新钉回通道，补发的每一块都会被 `channel_allows` 判成"没先发
        // FILE_META"，续传在协议层就发不出去。只在通道**空闲**时钉：被别的文件占着绝不抢；
        // 真收到 bogus 请求时，下一条 FILE_META 会覆盖钉痕，不会留下"占着通道没人用"。
        if self.tcp_bound && matches!(self.file_channel, FileChannel::Idle) {
            self.file_channel = FileChannel::Tcp(file_id);
        }
        debuglog::log!(
            Level::Info,
            "session",
            "file.resume",
            &[
                ("file_id", &format!("{:#x}", file_id)),
                ("from", &from_index.to_string()),
                ("repin", &self.tcp_bound.to_string()),
            ]
        );
        self.emit(EngineEvent::FileResumeRequested {
            file_id,
            from_index,
        });
    }

    /// 跨端配置同步入站
    fn on_config_sync(&mut self, plaintext: &[u8]) {
        let env = match envelope::decode(plaintext) {
            Ok(e) => e,
            Err(e) => {
                self.emit_error(err_code::IO_GENERIC, format!("配置同步信封解析失败: {e}"));
                return;
            }
        };
        match ConfigSync::decode(env.body.as_ref()) {
            Ok(c) => {
                let entries = c
                    .entries
                    .into_iter()
                    .map(|e| ConfigEntryItem {
                        key: e.key,
                        value: e.value,
                        scope: e.scope,
                    })
                    .collect();
                self.emit(EngineEvent::ConfigReceived { entries });
            }
            Err(e) => self.emit_error(err_code::IO_GENERIC, format!("配置同步解析失败: {e}")),
        }
    }

    // ---------- 业务发送 ----------

    /// 推送通知（Android → Windows，V1 唯一通知方向；敏感过滤在平台侧）
    pub fn send_notification(&mut self, n: &NotificationPush, now_ms: i64) -> bool {
        if !self.is_paired() {
            return false;
        }
        let body = n.encode_to_vec();
        let msg_id = self.next_msg_id();
        let src = self.local_dev_id();
        let payload = envelope::encode(&msg_id, now_ms, &src, &body);
        self.send_encrypted(msg_type::NOTIFY_PUSH, &payload)
    }

    /// 回复一条对端通知（电脑 → 手机）。返回 false = 这条**根本没出去**（未配对/链路未就绪），
    /// 调用方要当场出声；返回 true 只代表请求已送达对端，成没成看回执。
    pub fn send_notify_reply(&mut self, r: &NotificationReply, now_ms: i64) -> bool {
        self.send_routed(msg_type::NOTIFY_REPLY, &r.encode_to_vec(), now_ms, false)
    }

    /// 回复回执（手机 → 电脑）。与请求同一条路由，保证两端在一条通知上走的通道一致。
    pub fn send_notify_reply_ack(&mut self, a: &NotificationReplyAck, now_ms: i64) -> bool {
        self.send_routed(
            msg_type::NOTIFY_REPLY_ACK,
            &a.encode_to_vec(),
            now_ms,
            false,
        )
    }

    /// 「这条通知已经不在了」（手机 → 电脑）。**跟推送走同一条通道**，不用 `send_routed`：
    /// TCP 会让它比还在 BLE 队列里的推送先到，电脑先撤一个还不存在的入口、随后又被那条推送装回去。
    pub fn send_notify_dismiss(&mut self, d: &NotificationDismiss, now_ms: i64) -> bool {
        if !self.is_paired() {
            return false;
        }
        let msg_id = self.next_msg_id();
        let src = self.local_dev_id();
        let payload = envelope::encode(&msg_id, now_ms, &src, &d.encode_to_vec());
        self.send_encrypted(msg_type::NOTIFY_DISMISS, &payload)
    }

    /// 推送当前播放状态（手机 → 电脑）。平台层只在**状态变化**时调用（切歌、播放 / 暂停、
    /// 每 5 s 一次的进度心跳），不能每 tick 都推：BLE 是窄口，位置刷太密会挤掉通知与剪贴板。
    pub fn send_media_state(&mut self, s: &MediaState, now_ms: i64) -> bool {
        if !self.is_paired() {
            return false;
        }
        // 与 [`send_media_command`] 同理：手机在后台时蓝牙最先断，播放状态也要能走局域网，
        // 否则电脑上的媒体页会停在最后一帧
        self.send_routed(msg_type::MEDIA_STATE, &s.encode_to_vec(), now_ms, false)
    }

    /// 下发播放控制指令（电脑 → 手机），由对端平台层执行。路由用 `send_routed` 而不是蓝牙
    /// 独占：**手机退到后台时最先没的就是这条 BLE 链路**（会换地址、会掐广播），局域网 socket
    /// 照常活着；钉在蓝牙上等于"只在 App 前台时可控制"。指令几十字节，不抢文件带宽。
    pub fn send_media_command(
        &mut self,
        action: i32,
        volume: i32,
        delta_ms: i64,
        now_ms: i64,
    ) -> bool {
        if !self.is_paired() {
            return false;
        }
        let c = MediaCommand {
            action,
            volume,
            delta_ms,
        };
        self.send_routed(msg_type::MEDIA_COMMAND, &c.encode_to_vec(), now_ms, false)
    }

    /// 推送手机设备状态（电量 / 充电中）：**只在变化时发**。电量是"越小越要准"的读数，
    /// TCP 已绑定时优先走局域网，免得和文件传输抢 BLE 窄口（它恰好在大文件传输期间最该显示）。
    pub fn send_device_status(&mut self, s: &DeviceStatus, now_ms: i64) -> bool {
        if !self.is_paired() {
            return false;
        }
        self.send_routed(msg_type::DEVICE_STATUS, &s.encode_to_vec(), now_ms, false)
    }

    /// 收到播放状态。解不开必须报错，不能静默丢：那会让电脑显示的还是上一首，
    /// 而用户以为已经切歌了。
    fn on_media_state(&mut self, plaintext: &[u8]) {
        // 未配对前一律不收：握手完成即有会话密钥，SAS 还没核对对端就能推内容，
        // 那时屏幕上"正在播放什么"来自一个尚未被本端接受的连接。
        if !self.is_paired() {
            self.emit_error(err_code::IO_GENERIC, "尚未配对，播放状态未采纳");
            return;
        }
        let env = match envelope::decode(plaintext) {
            Ok(e) => e,
            Err(e) => {
                self.emit_error(err_code::IO_GENERIC, format!("播放状态信封解析失败: {e}"));
                return;
            }
        };
        match MediaState::decode(env.body.as_ref()) {
            Ok(s) => self.emit(EngineEvent::MediaState {
                package: s.package,
                title: s.title,
                artist: s.artist,
                album: s.album,
                playing: s.playing,
                position_ms: s.position_ms,
                duration_ms: s.duration_ms,
                speed_x100: (s.speed * 100.0).round() as i32,
                volume: s.volume,
            }),
            Err(e) => self.emit_error(err_code::IO_GENERIC, format!("播放状态解析失败: {e}")),
        }
    }

    /// 收到手机设备状态。未配对前不收（与播放状态同一口径：电量也是隐私）。
    fn on_device_status(&mut self, plaintext: &[u8]) {
        if !self.is_paired() {
            self.emit_error(err_code::IO_GENERIC, "尚未配对，设备状态未采纳");
            return;
        }
        let env = match envelope::decode(plaintext) {
            Ok(e) => e,
            Err(e) => {
                self.emit_error(err_code::IO_GENERIC, format!("设备状态信封解析失败: {e}"));
                return;
            }
        };
        match DeviceStatus::decode(env.body.as_ref()) {
            Ok(s) => self.emit(EngineEvent::DeviceStatus {
                battery: s.battery,
                charging: s.charging,
                ts_ms: s.ts_ms,
            }),
            Err(e) => self.emit_error(err_code::IO_GENERIC, format!("设备状态解析失败: {e}")),
        }
    }

    fn on_media_command(&mut self, plaintext: &[u8]) {
        // 这条比状态更硬：指令会真的改对端的播放与系统音量。握手完成≠配对完成，
        // SAS 还没核对就执行，等于让一个尚未被本端接受的连接拨动用户的设备。
        if !self.is_paired() {
            self.emit_error(err_code::IO_GENERIC, "尚未配对，播放指令未执行");
            return;
        }
        let env = match envelope::decode(plaintext) {
            Ok(e) => e,
            Err(e) => {
                self.emit_error(err_code::IO_GENERIC, format!("播放指令信封解析失败: {e}"));
                return;
            }
        };
        match MediaCommand::decode(env.body.as_ref()) {
            Ok(c) => self.emit(EngineEvent::MediaCommand {
                action: c.action,
                volume: c.volume,
                delta_ms: c.delta_ms,
            }),
            Err(e) => self.emit_error(err_code::IO_GENERIC, format!("播放指令解析失败: {e}")),
        }
    }

    /// 推送剪贴板纯文本（V1 仅纯文本）
    pub fn send_clipboard_text(&mut self, text: &str, now_ms: i64) -> bool {
        if !self.is_paired() {
            return false;
        }
        if text.len() > CLIPBOARD_MAX_BYTES {
            self.emit_error(
                err_code::IO_GENERIC,
                format!(
                    "剪贴板内容 {} KB 超过 {} KB 上限，未发送",
                    text.len() / 1024,
                    CLIPBOARD_MAX_BYTES / 1024
                ),
            );
            return false;
        }
        let c = ClipboardPush {
            kind: linkx_protocol::pb::clipboard_push::Kind::Text as i32,
            data: text.as_bytes().to_vec().into(),
            mime: "text/plain".to_string(),
            ts_ms: now_ms,
        };
        let body = c.encode_to_vec();
        let msg_id = self.next_msg_id();
        let src = self.local_dev_id();
        let payload = envelope::encode(&msg_id, now_ms, &src, &body);
        self.send_encrypted(msg_type::CLIPBOARD_PUSH, &payload)
    }

    // ---------- 文件传输发送（TCP 优先，小消息可回退 BLE） ----------

    /// 统一出口：TCP 已绑定 → 走 TCP；否则回退 BLE。**回退只对小载荷成立**：文件 META 与
    /// 分块在未绑定时直接大声失败，绝不悄悄挤进窄口（通知 / 剪贴板 / 媒体报文本来就小）。
    /// `tcp_only`：在途传输的分块 / 续传锁死在 [`FileChannel`] 钉定的那条通道上；已钉在 TCP
    /// 而 TCP 随后断了时，这里**不会**把它悄悄改走 BLE——那正是"META 到了、分块没到、UI 却
    /// 报已完成"的成因。
    fn send_routed(&mut self, mt: u8, body: &[u8], now_ms: i64, tcp_only: bool) -> bool {
        if !self.is_paired() {
            return false;
        }
        let msg_id = self.next_msg_id();
        let src = self.local_dev_id();
        let payload = envelope::encode(&msg_id, now_ms, &src, body);
        let via_tcp = self.tcp_bound
            && (!tcp_only || matches!(self.file_channel, FileChannel::Tcp(_) | FileChannel::Idle));
        debuglog::log!(
            Level::Info,
            "session",
            "file.route",
            &[
                ("msg", &format!("0x{mt:02x}")),
                ("channel", if via_tcp { "tcp" } else { "ble" }),
                ("tcp_bound", if self.tcp_bound { "1" } else { "0" }),
                ("locked", if tcp_only { "1" } else { "0" }),
            ]
        );
        if via_tcp {
            self.send_encrypted_tcp(mt, &payload)
        } else {
            self.send_encrypted(mt, &payload)
        }
    }

    /// FILE_META：文件元信息（name/size/sha256/crc32/chunk_size）
    ///
    /// 这是**一次传输的起点**：在此刻选定并记住通道。之后同 `file_id` 的分块只认这条通道。
    pub fn send_file_meta(&mut self, meta: &FileMeta, now_ms: i64) -> bool {
        if !self.is_paired() {
            return false;
        }
        // 通道选择：TCP 已绑定走 TCP；未绑定且文件足够小才允许 BLE 兜底。预算口径：BLE 每包
        // 净荷最小 14B，256KB 一块要约 18,700 个包，而安卓 notify 队列上限 512 且丢最旧——
        // 大文件走 BLE 结构上不可能成功，未绑定时**大声失败**而不是硬塞。
        // 块数在此本地推导（session 不依赖 transfer），上取整语义同 chunks_total。
        let chunk = if meta.chunk_size == 0 {
            262_144
        } else {
            meta.chunk_size as u64
        };
        let chunks = meta.size.div_ceil(chunk) as usize;
        let ble_budget_ok = self.tcp_bound || chunks <= 2;
        self.file_channel = if self.tcp_bound {
            FileChannel::Tcp(meta.file_id)
        } else if ble_budget_ok {
            FileChannel::Ble(meta.file_id)
        } else {
            FileChannel::Idle
        };
        debuglog::log!(
            Level::Info,
            "session",
            "file.meta.send",
            &[
                ("file_id", &format!("{:#x}", meta.file_id)),
                ("size", &meta.size.to_string()),
                ("channel", if self.tcp_bound { "tcp" } else { "ble" }),
                ("chunks", &chunks.to_string()),
            ]
        );
        if !self.tcp_bound && !ble_budget_ok {
            self.emit(EngineEvent::FileTaskFailed {
                file_id: meta.file_id,
                reason: "TCP 数据通道尚未就绪，文件未发送（等待局域网通道重连）".to_string(),
            });
            return false;
        }
        let body = meta.encode_to_vec();
        self.send_routed(msg_type::FILE_META, &body, now_ms, false)
    }

    /// FILE_CHUNK：单个数据分块（`crc32` 由平台层按 `data` 计算后传入）
    pub fn send_file_chunk(
        &mut self,
        file_id: u64,
        index: u32,
        crc32: u32,
        data: &[u8],
        now_ms: i64,
    ) -> bool {
        // 已取消：不再发块，**也不报失败**（用户取消不是出错）。计数留痕，
        // 平台侧据此看到"取消之后还在递分块"确实发生过。
        if self.file_cancel_seen(file_id) {
            self.note_late_frame(
                "chunk-send",
                file_id,
                format!("取消后仍被递来第 {index} 块（{} B）", data.len()),
            );
            return false;
        }
        // 通道锁：只允许发给 send_file_meta 钉定的那条链路；不一致就大声失败。
        if !self.channel_allows(file_id, true) {
            return false;
        }
        let body = FileChunk {
            file_id,
            index,
            crc32,
            data: data.to_vec().into(),
        }
        .encode_to_vec();
        self.send_routed(msg_type::FILE_CHUNK, &body, now_ms, true)
    }

    /// 本次 `file_id` 的传输是否还允许继续发送；返回 `false` 时同时发
    /// [`EngineEvent::FileTaskFailed`] 让平台层把任务标失败。**绝不能让分块静默丢失**。
    fn channel_allows(&mut self, file_id: u64, is_chunk: bool) -> bool {
        match self.file_channel {
            FileChannel::Tcp(id) if id == file_id => {
                if self.tcp_bound {
                    return true;
                }
                // 传输途中 TCP 断了：不降级 BLE，直接失败
                self.file_channel = FileChannel::Idle;
                self.emit(EngineEvent::FileTaskFailed {
                    file_id,
                    reason: "传输中 TCP 通道断开，文件未完成（不会改走蓝牙）".to_string(),
                });
                false
            }
            FileChannel::Ble(id) if id == file_id => true,
            FileChannel::Idle => {
                // 只有分块路径需要报错：META/DONE/RESUME 之外的调用方不该被这条打断
                if is_chunk {
                    self.emit(EngineEvent::FileTaskFailed {
                        file_id,
                        reason: "未先发送 FILE_META 就发来了分块，文件未发送".to_string(),
                    });
                }
                false
            }
            // 别的 file_id 正占着通道（并发发送）：同样拒绝，不共享同一次锁定
            _ => {
                if is_chunk {
                    self.emit(EngineEvent::FileTaskFailed {
                        file_id,
                        reason: "当前已有另一条文件传输占用通道，稍后再试".to_string(),
                    });
                }
                false
            }
        }
    }

    /// 释放通道锁（一次传输结束：FILE_DONE 发出、被硬失败、或会话作废）
    fn release_file_channel(&mut self, file_id: u64) {
        if matches!(self.file_channel, FileChannel::Tcp(id) | FileChannel::Ble(id) if id == file_id)
        {
            self.file_channel = FileChannel::Idle;
        }
    }

    /// FILE_DONE：文件传输结束（`ok=false` 时 `error` 为原因）
    pub fn send_file_done(
        &mut self,
        file_id: u64,
        ok: bool,
        error: Option<&str>,
        now_ms: i64,
    ) -> bool {
        self.send_file_done_digest(file_id, ok, None, error, now_ms)
    }

    /// FILE_DONE 带整文件摘要（发端边发边算，在这一帧交付）。`sha256 = None` 时收端回落到
    /// FILE_META 的声明值。
    pub fn send_file_done_digest(
        &mut self,
        file_id: u64,
        ok: bool,
        sha256: Option<[u8; 32]>,
        error: Option<&str>,
        now_ms: i64,
    ) -> bool {
        self.emit_file_done(file_id, ok, sha256, error, false, now_ms)
    }

    /// FILE_DONE 出站的唯一实现。`cancelled = true` 时收端显示「已取消」并删残留文件，
    /// 而不是把用户的取消报成失败。
    fn emit_file_done(
        &mut self,
        file_id: u64,
        ok: bool,
        sha256: Option<[u8; 32]>,
        error: Option<&str>,
        cancelled: bool,
        now_ms: i64,
    ) -> bool {
        debuglog::log!(
            Level::Info,
            "session",
            "file.done.send",
            &[
                ("file_id", &format!("{:#x}", file_id)),
                ("ok", if ok { "true" } else { "false" }),
                ("has_sha", if sha256.is_some() { "true" } else { "false" }),
                ("cancelled", if cancelled { "true" } else { "false" }),
            ]
        );
        let body = FileDone {
            file_id,
            ok,
            error: error.map(|s| s.to_string()),
            sha256: sha256.map(|b| b.to_vec().into()).unwrap_or_default(),
            cancelled,
        }
        .encode_to_vec();
        let r = self.send_routed(msg_type::FILE_DONE, &body, now_ms, false);
        // 一次传输到此结束：释放通道锁，否则下一条文件会被误判为"通道被占用"
        self.release_file_channel(file_id);
        r
    }

    /// 本端（发送侧）取消一次在途传输。**顺序是约束不是风格**：登记取消（之后同 id 分块被拒
    /// 且**不报失败**）→ 发 FILE_DONE{ok:false, cancelled:true} → 释放通道锁（不释放下一条
    /// 文件就发不出去）→ 上报 `FileTaskCancelled`。**永不删除源文件**。
    /// 返回 false = 结束帧没发出，对端会停在"传输中"，调用方必须把这句话讲给用户。
    pub fn cancel_file_send(&mut self, file_id: u64, reason: &str, now_ms: i64) -> bool {
        let sent = if self.file_cancel_seen(file_id) {
            // 已经取消过（例如对端的 FILE_CANCEL 先到）：不再重复发结束帧
            false
        } else {
            self.mark_file_cancelled(file_id);
            self.emit_file_done(file_id, false, None, Some(reason), true, now_ms)
        };
        debuglog::log!(
            Level::Warn,
            "session",
            "file.cancel.send",
            &[
                ("file_id", &format!("{:#x}", file_id)),
                ("done_sent", if sent { "true" } else { "false" }),
            ]
        );
        self.emit(EngineEvent::FileTaskCancelled {
            file_id,
            // 结束帧没发出去是一次真实的对外失效，必须写进用户看得见的原因里
            reason: if sent {
                reason.to_string()
            } else {
                format!("{reason}（结束帧未能发出，对方那侧可能还停在传输中）")
            },
        });
        sent
    }

    /// 本端（接收侧）取消一次在途接收：收端没有别的办法掐断正在进来的流，只能发 FILE_CANCEL
    /// 让发端停手。登记后仍在路上的分块 / 结束帧会被丢弃并计数；残留文件由平台层删除（删除
    /// 失败要说进原因里）。返回 false = FILE_CANCEL 没能发出（未配对 / 链路已断）。
    pub fn cancel_file_recv(&mut self, file_id: u64, reason: &str, now_ms: i64) -> bool {
        let sent = if self.file_cancel_seen(file_id) {
            false
        } else {
            self.mark_file_cancelled(file_id);
            self.send_file_cancel(file_id, reason, now_ms)
        };
        debuglog::log!(
            Level::Warn,
            "session",
            "file.cancel.recv_local",
            &[
                ("file_id", &format!("{:#x}", file_id)),
                ("cancel_sent", if sent { "true" } else { "false" }),
            ]
        );
        self.emit(EngineEvent::FileTaskCancelled {
            file_id,
            reason: if sent {
                reason.to_string()
            } else {
                format!("{reason}（取消帧未能发出，对方可能还在继续发送）")
            },
        });
        sent
    }

    /// FILE_CANCEL：收端 → 发端「停止发送」
    pub fn send_file_cancel(&mut self, file_id: u64, reason: &str, now_ms: i64) -> bool {
        let body = FileCancel {
            file_id,
            reason: reason.to_string(),
        }
        .encode_to_vec();
        self.send_routed(msg_type::FILE_CANCEL, &body, now_ms, false)
    }

    /// 请求对端从 `from_index` 续传（收端发现分块不连续时调用）
    pub fn send_file_resume(&mut self, file_id: u64, from_index: u32, now_ms: i64) -> bool {
        debuglog::log!(
            Level::Info,
            "session",
            "file.resume.send",
            &[
                ("file_id", &format!("{:#x}", file_id)),
                ("from", &from_index.to_string()),
            ]
        );
        let body = tlv_codec::encode(&[
            Tlv::buf(TAG_FILE_ID, &file_id.to_be_bytes()),
            Tlv::buf(TAG_RESUME_FROM, &from_index.to_be_bytes()),
        ])
        .unwrap_or_default();
        self.send_routed(msg_type::RESUME, &body, now_ms, false)
    }

    // ---------- 跨端配置同步 ----------

    /// 推送跨端配置项（仅 `scope = cross | per_peer` 的项应传入）
    pub fn send_config(&mut self, entries: &[ConfigEntryItem], now_ms: i64) -> bool {
        if !self.is_paired() {
            return false;
        }
        let msg = ConfigSync {
            entries: entries
                .iter()
                .map(|e| linkx_protocol::pb::ConfigEntry {
                    key: e.key.clone(),
                    value: e.value.clone(),
                    scope: e.scope.clone(),
                })
                .collect(),
        };
        let body = msg.encode_to_vec();
        self.send_routed(msg_type::CONFIG_SYNC, &body, now_ms, false)
    }

    // ---------- 设备生命周期 ----------

    /// 设备解绑：清除全部已信任身份并复位会话。平台层须同步清除本地持久化的信任列表，
    /// 否则下次连接会以旧指纹重连。
    pub fn unbind_peer(&mut self) {
        self.cfg.trusted_peers.clear();
        self.peer_fp = None;
        self.reset_session();
    }
}

/// 兜底常量自检（编译期对齐 tlv.rs）
const _: () = {
    assert!(MSG_PAIR_CONFIRM == linkx_protocol::msg_type::PAIR_CONFIRM);
    assert!(MSG_PAIR_DONE == linkx_protocol::msg_type::PAIR_DONE);
};

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn now() -> Instant {
        Instant::now()
    }

    /// 内存 BLE 管道：把 A 的出站包逐包喂给 B（模拟 Windows ↔ Android 的 GATT UART）
    fn pump(a: &mut SessionEngine, b: &mut SessionEngine, t: Instant, rounds: usize) {
        for _ in 0..rounds {
            let mut moved = false;
            for pkt in a.take_outbound() {
                b.feed(&pkt, t);
                moved = true;
            }
            for pkt in b.take_outbound() {
                a.feed(&pkt, t);
                moved = true;
            }
            if !moved {
                return;
            }
        }
    }

    /// 测试共享 RSA 身份（生成慢，OnceLock 缓存；0 = Windows 侧，1 = Android 侧）：同一侧的
    /// 所有引擎实例共用同一身份，模拟「同一设备跨重启身份不变」。
    fn test_identity(which: u8) -> Vec<u8> {
        use std::sync::OnceLock;
        static ID_WIN: OnceLock<Vec<u8>> = OnceLock::new();
        static ID_AND: OnceLock<Vec<u8>> = OnceLock::new();
        let cell = if which == 0 { &ID_WIN } else { &ID_AND };
        cell.get_or_init(|| DeviceIdentity::generate().unwrap().to_pkcs8_der().unwrap())
            .clone()
    }

    /// 某侧身份指纹（0 = Windows，1 = Android）
    fn fp_of_side(which: u8) -> String {
        DeviceIdentity::from_pkcs8_der(&test_identity(which))
            .unwrap()
            .fingerprint()
            .unwrap()
    }

    fn pair_engines(
        known_win: Option<String>,
        known_and: Option<String>,
    ) -> (SessionEngine, SessionEngine) {
        let mut win_cfg = EngineConfig::new(
            EngineRole::Initiator,
            "pc-home",
            linkx_protocol::OS_WINDOWS,
            "0.3.0",
            [0xA1; 32],
        )
        .with_identity_der(test_identity(0));
        if let Some(fp) = known_win {
            win_cfg = win_cfg.with_trusted_peers(vec![TrustedPeer {
                fingerprint: fp,
                name: "pixel-7".into(),
            }]);
        }
        let mut and_cfg = EngineConfig::new(
            EngineRole::Responder,
            "pixel-7",
            linkx_protocol::OS_ANDROID,
            "0.3.0",
            [0xB2; 32],
        )
        .with_identity_der(test_identity(1));
        if let Some(fp) = known_and {
            and_cfg = and_cfg.with_trusted_peers(vec![TrustedPeer {
                fingerprint: fp,
                name: "pc-home".into(),
            }]);
        }
        (SessionEngine::new(win_cfg), SessionEngine::new(and_cfg))
    }

    fn drain_events(e: &mut SessionEngine) -> Vec<EngineEvent> {
        e.take_events()
    }

    fn has_error(evs: &[EngineEvent], code: i32) -> bool {
        evs.iter()
            .any(|e| matches!(e, EngineEvent::Error { code: c, .. } if *c == code))
    }

    #[test]
    fn first_pair_full_flow_over_ble_pipe() {
        let t = now();
        let (mut win, mut and) = pair_engines(None, None);
        win.start(t);
        and.start(t);
        pump(&mut win, &mut and, t, 64);

        assert_eq!(win.state(), SessionState::SasCompare, "windows 应待比对");
        assert_eq!(and.state(), SessionState::SasCompare, "android 应待比对");
        assert_eq!(win.sas(), and.sas(), "SAS 必须一致");
        let win_evs = drain_events(&mut win);
        let and_evs = drain_events(&mut and);
        assert!(win_evs
            .iter()
            .any(|e| matches!(e, EngineEvent::PeerHello { name, .. } if name == "pixel-7")));
        assert!(and_evs
            .iter()
            .any(|e| matches!(e, EngineEvent::PeerHello { name, .. } if name == "pc-home")));
        assert!(win_evs
            .iter()
            .any(|e| matches!(e, EngineEvent::SasReady { .. })));
        assert!(and_evs
            .iter()
            .any(|e| matches!(e, EngineEvent::SasReady { .. })));

        win.confirm_sas();
        and.confirm_sas();
        pump(&mut win, &mut and, t, 16);
        assert!(win.is_paired() && and.is_paired());
        assert_eq!(win.peer_fingerprint(), Some(fp_of_side(1)).as_deref());
        assert_eq!(and.peer_fingerprint(), Some(fp_of_side(0)).as_deref());
        assert!(
            win.pair_evidence_verified() && and.pair_evidence_verified(),
            "PAIR_DONE 证据链闭合"
        );
        assert_eq!(win.trusted_peers().len(), 1);
        assert_eq!(win.trusted_peers()[0].fingerprint, fp_of_side(1));
        assert_eq!(and.trusted_peers()[0].fingerprint, fp_of_side(0));
    }

    /// HELLO 的四条 TLV 在**最坏情况**（名字与版本都长到超出预算）也必须装进 64B，
    /// 而且名字与序号都在。以前编不下就退化成只发系统字节：对端拿不到名字，
    /// "按名字认领已绑定设备并自动重连"当场失效，而这条链路已经断过一次。
    #[test]
    fn hello_body_keeps_name_and_seq_even_for_the_longest_inputs() {
        let name = "小".repeat(40); // 120 字节，远超 32B 预算
        let version = "9".repeat(60);
        let body = encode_hello_body(&name, linkx_protocol::OS_ANDROID, &version, 0x0102);
        assert!(
            body.len() <= tlv_codec::TLV_MAX_MSG,
            "HELLO 正文 {}B 超出 {}B 预算",
            body.len(),
            tlv_codec::TLV_MAX_MSG
        );
        let got_name = tlv_codec::get(&body, TAG_ADVERT_NAME)
            .unwrap()
            .unwrap_or_default();
        let back = String::from_utf8(got_name.to_vec()).expect("截断不能切断一个字符");
        assert!(!back.is_empty(), "名字整条丢了：自动重连会认不出设备");
        assert!(name.starts_with(&back), "截出来的必须是名字的开头一段");
        let seq = tlv_codec::get(&body, TAG_HELLO_SEQ)
            .unwrap()
            .unwrap_or_default();
        assert_eq!(seq.len(), 8, "序号丢了就退化成「每条 HELLO 都是新会话」");
        assert_eq!(u64::from_be_bytes(seq.try_into().unwrap()), 0x0102);
        assert!(
            tlv_codec::get(&body, TAG_VERSION)
                .unwrap()
                .is_some_and(|v| v.len() <= HELLO_VERSION_MAX),
            "版本串必须按预算截"
        );
    }

    #[test]
    fn hello_reply_recovers_when_peer_started_before_link() {
        // 时序：Android App 先启动并发出 HELLO（此时无连接 → 丢失），Windows 稍后才连接。
        let t = now();
        let (mut win, mut and) = pair_engines(None, None);
        and.start(t);
        let _lost = and.take_outbound(); // 模拟「无 Central 连接」时 HELLO 丢失
        win.start(t);
        pump(&mut win, &mut and, t, 64);
        // 若引擎不等 HELLO 应答就会死锁在 Handshake —— 此处必须走到 SAS 比对
        assert_eq!(win.state(), SessionState::SasCompare);
        assert_eq!(and.state(), SessionState::SasCompare);
        let win_evs = drain_events(&mut win);
        assert!(
            win_evs
                .iter()
                .any(|e| matches!(e, EngineEvent::PeerHello { name, .. } if name == "pixel-7")),
            "Windows 必须拿到对端设备名（HELLO 应答）"
        );
    }

    #[test]
    fn reconnect_with_stored_fingerprints_skips_sas() {
        // 双方均已持久化 **RSA 身份指纹**：Windows 重建引擎后应免 SAS 直通 PAIRED。
        let t = now();
        let win_fp = fp_of_side(1); // Windows 记录的对端（Android）身份指纹
        let and_fp = fp_of_side(0); // Android 记录的对端（Windows）身份指纹

        let and_cfg = EngineConfig::new(
            EngineRole::Responder,
            "pixel-7",
            linkx_protocol::OS_ANDROID,
            "0.3.0",
            [0xB2; 32],
        )
        .with_identity_der(test_identity(1))
        .with_trusted_peers(vec![TrustedPeer {
            fingerprint: and_fp,
            name: "pc-home".into(),
        }]);
        let mut and = SessionEngine::new(and_cfg);

        let win_cfg = EngineConfig::new(
            EngineRole::Initiator,
            "pc-home",
            linkx_protocol::OS_WINDOWS,
            "0.3.0",
            [0xA1; 32],
        )
        .with_identity_der(test_identity(0))
        .with_trusted_peers(vec![TrustedPeer {
            fingerprint: win_fp,
            name: "pixel-7".into(),
        }]);
        let mut win = SessionEngine::new(win_cfg);

        win.start(t);
        and.start(t);
        pump(&mut win, &mut and, t, 64);
        assert!(win.is_paired() && and.is_paired(), "指纹匹配 → 免 SAS 直通");
        assert!(!drain_events(&mut win)
            .iter()
            .any(|e| matches!(e, EngineEvent::SasReady { .. })));
        assert!(win.send_clipboard_text("after-reconnect", 1));
        pump(&mut win, &mut and, t, 16);
        let evs = drain_events(&mut and);
        assert!(evs
            .iter()
            .any(|e| matches!(e, EngineEvent::Clipboard { text } if text == "after-reconnect")));
    }

    #[test]
    fn reconnect_when_peer_engine_still_paired_self_heals() {
        // 对端引擎仍活且处 PAIRED、本端重建引擎：不能卡死。本端先按 NewPeer 走 SAS，
        // 比对侧（旧引擎）回 PAIR_CONFIRM + 上报 SAS → 人工确认后双双 PAIRED。
        let t = now();
        let (mut win, mut and) = pair_engines(None, None);
        win.start(t);
        and.start(t);
        pump(&mut win, &mut and, t, 64);
        win.confirm_sas();
        and.confirm_sas();
        pump(&mut win, &mut and, t, 16);
        assert!(win.is_paired() && and.is_paired());
        let _ = drain_events(&mut win);
        let _ = drain_events(&mut and);

        // Windows 重建引擎（信任库为空，模拟「本端记录丢失」）：同一台设备身份不变，
        // 对端（旧引擎）仍记得它 → 免 SAS
        let mut win2 = SessionEngine::new(
            EngineConfig::new(
                EngineRole::Initiator,
                "pc-home",
                linkx_protocol::OS_WINDOWS,
                "0.3.0",
                [0xA1; 32],
            )
            .with_identity_der(test_identity(0)),
        );
        win2.start(t);
        pump(&mut win2, &mut and, t, 64);
        assert_eq!(
            win2.state(),
            SessionState::SasCompare,
            "新引擎按首次配对走 SAS"
        );
        assert_eq!(and.state(), SessionState::Paired, "旧引擎保持 PAIRED");
        assert!(
            drain_events(&mut and)
                .iter()
                .any(|e| matches!(e, EngineEvent::SasReady { .. })),
            "旧引擎须上报 SAS 供人工比对"
        );
        win2.confirm_sas();
        pump(&mut win2, &mut and, t, 16);
        assert!(win2.is_paired() && and.is_paired());
        assert!(and.pair_evidence_verified(), "PAIR_DONE 证据链闭合");
    }

    #[test]
    fn notification_and_clipboard_delivered() {
        let t = now();
        let (mut win, mut and) = pair_engines(None, None);
        win.start(t);
        and.start(t);
        pump(&mut win, &mut and, t, 64);
        win.confirm_sas();
        and.confirm_sas();
        pump(&mut win, &mut and, t, 16);
        let _ = drain_events(&mut win);
        let _ = drain_events(&mut and);

        // Android → Windows：通知（回复定位三元组与推送同一条消息，不另发明一条）
        let n = NotificationPush {
            package: "com.example.chat".into(),
            title: "小明".into(),
            text: "在吗？".into(),
            post_ts_ms: 1_790_000_000_000,
            key_hash: 7,
            cover_jpeg: Default::default(),
            tag: "chat".into(),
            notification_id: 21,
            can_reply: true,
            reply_action_index: 0,
            reply_result_key: "key_reply".into(),
        };
        assert!(and.send_notification(&n, 1_790_000_000_000));
        pump(&mut win, &mut and, t, 16);
        let evs = drain_events(&mut win);
        assert!(evs.iter().any(|e| matches!(
            e,
            EngineEvent::Notification {
                package,
                title,
                text,
                tag,
                notification_id,
                can_reply,
                reply_result_key,
                ..
            } if package == "com.example.chat"
                && title == "小明"
                && text == "在吗？"
                && tag == "chat"
                && *notification_id == 21
                && *can_reply
                && reply_result_key == "key_reply"
        )));

        // Windows → Android：剪贴板
        assert!(win.send_clipboard_text("hello from pc", 1));
        pump(&mut win, &mut and, t, 16);
        let evs = drain_events(&mut and);
        assert!(evs
            .iter()
            .any(|e| matches!(e, EngineEvent::Clipboard { text } if text == "hello from pc")));
    }

    #[test]
    fn tofu_matched_skips_sas() {
        let t = now();
        // 双方已互存 RSA 身份指纹：已信任设备无感重连
        let (mut win, mut and) = pair_engines(Some(fp_of_side(1)), Some(fp_of_side(0)));
        win.start(t);
        and.start(t);
        pump(&mut win, &mut and, t, 64);
        assert!(win.is_paired() && and.is_paired(), "指纹匹配 → 直接 PAIRED");
        let evs = drain_events(&mut win);
        assert!(!evs
            .iter()
            .any(|e| matches!(e, EngineEvent::SasReady { .. })));
        assert!(evs
            .iter()
            .any(|e| matches!(e, EngineEvent::PeerPaired { .. })));
    }

    /// 同名设备呈递新身份 → -213 + `IdentityChanged` + 等用户决策；接受后必须**重走 SAS
    /// 复核**，不得直接跳 PAIRED。
    #[test]
    fn tofu_mismatch_reports_213_and_requires_sas_reconfirm() {
        let t = now();
        // win 信任库里有个旧身份（deadbeef…，同名 pixel-7）；对端本次呈递新身份 → Mismatch
        let (mut win, mut and) = pair_engines(Some("deadbeefdeadbeef".into()), None);
        win.start(t);
        and.start(t);
        pump(&mut win, &mut and, t, 64);
        assert_eq!(win.state(), SessionState::Repaired);
        let evs = drain_events(&mut win);
        assert!(has_error(&evs, -213));
        assert!(
            evs.iter().any(|e| matches!(
                e,
                EngineEvent::IdentityChanged { name, old_fingerprint, new_fingerprint }
                    if name == "pixel-7"
                        && old_fingerprint == "deadbeefdeadbeef"
                        && new_fingerprint.as_str() == fp_of_side(1)
            )),
            "必须发出 IdentityChanged（UI 据此弹「重新配对确认」）"
        );
        assert!(!win.is_paired(), "用户未决策前不得进入 PAIRED");

        win.accept_new_fingerprint();
        pump(&mut win, &mut and, t, 16);
        assert_eq!(win.state(), SessionState::SasCompare, "接受后进入 SAS 复核");
        assert_eq!(
            and.state(),
            SessionState::SasCompare,
            "对端同步进入 SAS 复核"
        );
        assert_eq!(win.sas(), and.sas(), "SAS 必须一致");
        win.confirm_sas();
        and.confirm_sas();
        pump(&mut win, &mut and, t, 16);
        assert!(win.is_paired() && and.is_paired());
        assert_eq!(win.trusted_peers().len(), 1);
        assert_eq!(win.trusted_peers()[0].fingerprint, fp_of_side(1));
    }

    #[test]
    fn sas_reject_closes_session() {
        let t = now();
        let (mut win, mut and) = pair_engines(None, None);
        win.start(t);
        and.start(t);
        pump(&mut win, &mut and, t, 64);
        win.reject_sas();
        assert_eq!(win.state(), SessionState::Closed);
        let evs = drain_events(&mut win);
        assert!(has_error(&evs, -200));
    }

    #[test]
    fn heartbeat_ping_pong_keeps_paired() {
        let t = now();
        let (mut win, mut and) = pair_engines(None, None);
        win.start(t);
        and.start(t);
        pump(&mut win, &mut and, t, 64);
        win.confirm_sas();
        and.confirm_sas();
        pump(&mut win, &mut and, t, 16);
        assert!(win.is_paired());

        // 31s 空闲 → 触发 PING；对端回 PONG；30s 后仍未超时
        let t1 = t + Duration::from_secs(31);
        win.tick(t1);
        pump(&mut win, &mut and, t1, 8);
        assert!(win.is_paired());
        let t2 = t1 + Duration::from_secs(30);
        win.tick(t2);
        assert!(win.is_paired(), "对端 PONG 已刷新活跃时间，不应超时");
    }

    /// 未配对态收到 PING 不许报错：对端在配对完成前就会发心跳，把它说成"载荷畸形"是给用户
    /// 凭空添一条错误。
    #[test]
    fn unpaired_ping_is_ignored_not_reported() {
        let t = now();
        let (mut win, _and) = pair_engines(None, None);
        win.start(t);
        assert!(!win.is_paired(), "本用例要的就是未配对态");
        win.on_heartbeat(&ping_payload(), true);
        let evs = drain_events(&mut win);
        assert!(
            !has_error(&evs, err_code::IO_GENERIC),
            "未配对时的 PING 不应产生错误"
        );
    }

    #[test]
    fn heartbeat_timeout_moves_to_reconnecting() {
        let t = now();
        let (mut win, mut and) = pair_engines(None, None);
        win.start(t);
        and.start(t);
        pump(&mut win, &mut and, t, 64);
        win.confirm_sas();
        and.confirm_sas();
        pump(&mut win, &mut and, t, 16);
        let t1 = t + Duration::from_secs(61); // > 30s 间隔 + 30s 超时
        win.tick(t1);
        assert_eq!(win.state(), SessionState::Reconnecting);
    }

    #[test]
    fn tampered_encrypted_frame_is_rejected_without_panic() {
        let t = now();
        let (mut win, mut and) = pair_engines(None, None);
        win.start(t);
        and.start(t);
        pump(&mut win, &mut and, t, 64);
        win.confirm_sas();
        and.confirm_sas();
        pump(&mut win, &mut and, t, 16);
        let _ = drain_events(&mut win);

        assert!(and.send_clipboard_text("tamper-me", 1));
        let mut pkts = and.take_outbound();
        // 篡改最后一个分片的内容字节（密文/Tag）
        let last = pkts.last_mut().expect("应有分片");
        let n = last.len();
        last[n - 1] ^= 0x5A;
        for p in pkts {
            win.feed(&p, t);
        }
        let evs = drain_events(&mut win);
        assert!(!evs
            .iter()
            .any(|e| matches!(e, EngineEvent::Clipboard { .. })));
        assert!(evs.iter().any(|e| matches!(e, EngineEvent::Error { .. })));
        assert!(win.is_paired(), "单帧被拒不应导致会话崩溃");
    }

    #[test]
    fn replay_same_frame_rejected() {
        let t = now();
        let (mut win, mut and) = pair_engines(None, None);
        win.start(t);
        and.start(t);
        pump(&mut win, &mut and, t, 64);
        win.confirm_sas();
        and.confirm_sas();
        pump(&mut win, &mut and, t, 16);
        let _ = drain_events(&mut win);

        assert!(and.send_notification(
            &NotificationPush {
                package: "p".into(),
                title: "t".into(),
                text: String::new(),
                post_ts_ms: 0,
                key_hash: 0,
                cover_jpeg: Default::default(),
                ..Default::default()
            },
            0
        ));
        let pkts = and.take_outbound();
        for p in &pkts {
            win.feed(p, t);
        }
        let first = drain_events(&mut win);
        assert_eq!(
            first
                .iter()
                .filter(|e| matches!(e, EngineEvent::Notification { .. }))
                .count(),
            1
        );
        for p in &pkts {
            win.feed(p, t);
        }
        let second = drain_events(&mut win);
        assert!(!second
            .iter()
            .any(|e| matches!(e, EngineEvent::Notification { .. })));
        assert!(has_error(&second, err_code::IO_GENERIC));
    }

    #[test]
    fn garbage_input_never_panics() {
        let t = now();
        let cfg = EngineConfig::new(
            EngineRole::Responder,
            "android",
            linkx_protocol::OS_ANDROID,
            "0.3.0",
            [0x11; 32],
        )
        .with_identity_der(test_identity(1));
        let mut e = SessionEngine::new(cfg);
        e.start(t);
        let _ = e.take_outbound();
        let mut seed = 0x1234_5678u32;
        for _ in 0..2_000 {
            seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
            let len = (seed % 40) as usize;
            let bytes: Vec<u8> = (0..len).map(|i| ((seed >> (i % 8)) & 0xFF) as u8).collect();
            e.feed(&bytes, t);
        }
        let _ = e.take_events();
        assert!(!e.is_paired());
    }

    /// 反向用例：人工确认 SAS **之前**不得学习对端身份；拒绝也不得学习
    #[test]
    fn tofu_fingerprint_learned_only_after_sas_confirm() {
        let t = now();
        let (mut win, mut and) = pair_engines(None, None);
        win.start(t);
        and.start(t);
        pump(&mut win, &mut and, t, 64);
        // 已进入 SAS 比对，但用户尚未确认 → 信任库必须仍为空
        assert_eq!(win.state(), SessionState::SasCompare);
        assert!(
            win.trusted_peers().is_empty(),
            "确认前不得学习对端身份（否则拒绝也会污染信任库）"
        );
        win.reject_sas();
        assert!(win.trusted_peers().is_empty());

        let (mut win2, mut and2) = pair_engines(None, None);
        win2.start(t);
        and2.start(t);
        pump(&mut win2, &mut and2, t, 64);
        win2.confirm_sas();
        assert_eq!(
            win2.trusted_peers().first().map(|t| t.fingerprint.as_str()),
            Some(fp_of_side(1)).as_deref(),
            "人工确认 SAS 后应写入信任库"
        );
    }

    /// 心跳超时 → 退避重连 → 对端应答 → 自愈回 PAIRED
    #[test]
    fn reconnect_after_timeout_self_heals() {
        let t = now();
        let (mut win, mut and) = pair_engines(None, None);
        win.start(t);
        and.start(t);
        pump(&mut win, &mut and, t, 64);
        win.confirm_sas();
        and.confirm_sas();
        pump(&mut win, &mut and, t, 16);
        assert!(win.is_paired() && and.is_paired());
        let _ = drain_events(&mut win);
        let _ = drain_events(&mut and);

        let t1 = t + Duration::from_secs(61);
        win.tick(t1);
        assert_eq!(win.state(), SessionState::Reconnecting);

        // 重连退避到期 → 重发 HELLO（第一次 tick 立即尝试），对端应答并重新握手
        win.tick(t1);
        pump(&mut win, &mut and, t1, 96);

        assert_ne!(win.state(), SessionState::Reconnecting, "不得卡在重连态");
        assert!(
            win.is_paired() && and.is_paired(),
            "重连后应自愈回到 PAIRED"
        );
    }

    // ---------- TCP 通道 / 文件传输 / 配置同步 ----------

    /// 内存 TCP 管道：把一端的 TCP 完整帧喂给另一端
    fn pump_tcp(a: &mut SessionEngine, b: &mut SessionEngine, t: Instant, rounds: usize) {
        for _ in 0..rounds {
            let mut moved = false;
            for f in a.take_tcp_outbound() {
                b.feed_tcp(&f, t);
                moved = true;
            }
            for f in b.take_tcp_outbound() {
                a.feed_tcp(&f, t);
                moved = true;
            }
            if !moved {
                return;
            }
        }
    }

    /// 配对（BLE）→ 完成 TCP 通道绑定，返回双端引擎
    fn paired_and_bound() -> (SessionEngine, SessionEngine) {
        let t = now();
        let (mut win, mut and) = pair_engines(None, None);
        win.start(t);
        and.start(t);
        pump(&mut win, &mut and, t, 64);
        win.confirm_sas();
        and.confirm_sas();
        pump(&mut win, &mut and, t, 16);
        assert!(win.is_paired() && and.is_paired());
        let _ = drain_events(&mut win);
        let _ = drain_events(&mut and);

        // TCP 建链后：Windows 为主动方（TcpClient），Android 为监听方（TcpServer）
        assert!(win.begin_tcp_binding(BindRole::TcpClient));
        assert!(and.begin_tcp_binding(BindRole::TcpServer));
        // nonce 经 TCP 交换，proof 经 BLE 已认证通道
        pump_tcp(&mut win, &mut and, t, 16);
        pump(&mut win, &mut and, t, 16);
        pump_tcp(&mut win, &mut and, t, 16);
        pump(&mut win, &mut and, t, 16);
        (win, and)
    }

    #[test]
    fn tcp_channel_binding_makes_both_sides_bound() {
        let (win, and) = paired_and_bound();
        assert!(win.is_tcp_bound(), "Windows 侧 TCP 绑定应完成");
        assert!(and.is_tcp_bound(), "Android 侧 TCP 绑定应完成");
    }

    /// TCP 绑定完成之前，绑定侧**不得采纳**业务帧，但也不能静默丢——绑定一完成就要原序
    /// 补投。这个窗口真实存在：两端各自判定"绑定完成"的时刻天然不同步，刚绑好的一方立刻
    /// 推剪贴板，另一方可能还差最后一帧 proof。
    #[test]
    fn tcp_business_frame_arriving_before_binding_is_parked_then_replayed() {
        let t = now();
        let (mut win, mut and) = paired_and_bound();

        // 先趁双方都绑好取一条**真实的** TCP 业务帧：`send_device_status` 走 `send_routed`，
        // 绑定后才排进 TCP 队列；剪贴板 / 媒体状态是 BLE 独占，取不到
        assert!(and.send_device_status(
            &DeviceStatus {
                battery: 77,
                charging: true,
                ts_ms: 0,
            },
            0,
        ));
        let biz = and
            .take_tcp_outbound()
            .into_iter()
            .find(|f| match parse_full_frame(f) {
                Ok((h, _)) => h.msg_type == msg_type::DEVICE_STATUS,
                Err(_) => false,
            })
            .expect("安卓侧应产出一条 TCP 电量帧");

        // 把两端都退回"未绑定"，模拟窗口期
        win.on_tcp_closed("测试：退回未绑定");
        and.on_tcp_closed("测试：退回未绑定");
        assert!(!win.is_tcp_bound());
        let _ = drain_events(&mut win);

        win.feed_tcp(&biz, t);
        let evs = drain_events(&mut win);
        assert!(
            !evs.iter()
                .any(|e| matches!(e, EngineEvent::DeviceStatus { .. })),
            "未绑定时不得采纳 TCP 业务帧，实际事件：{evs:?}"
        );
        assert!(
            !evs.iter().any(|e| matches!(e, EngineEvent::Error { .. })),
            "窗口期到达属正常竞态，不该报错，实际事件：{evs:?}"
        );

        // 重新绑定：那条帧必须原样到达，而不是凭空消失
        assert!(win.begin_tcp_binding(BindRole::TcpClient));
        assert!(and.begin_tcp_binding(BindRole::TcpServer));
        pump_tcp(&mut win, &mut and, t, 16);
        pump(&mut win, &mut and, t, 16);
        pump_tcp(&mut win, &mut and, t, 16);
        assert!(win.is_tcp_bound(), "重新绑定应完成");
        let evs = drain_events(&mut win);
        assert!(
            evs.iter()
                .any(|e| matches!(e, EngineEvent::DeviceStatus { battery: 77, .. })),
            "绑定完成后必须补投暂存的 TCP 业务帧，实际事件：{evs:?}"
        );
    }

    /// 暂存必须有界：对端在绑定完成前猛推业务帧时，宁可报错也不能让内存无界增长。
    #[test]
    fn tcp_prebind_park_is_bounded() {
        let t = now();
        let (mut win, mut and) = paired_and_bound();
        win.on_tcp_closed("测试：退回未绑定");
        let _ = drain_events(&mut win);

        // 帧体内容在这一点上无关紧要：闸门在解密之前，业务帧只会进暂存队列
        let frame = {
            let body = vec![0u8; 12];
            let header = FrameHeader::new(msg_type::CLIPBOARD_PUSH, 0, 1, body.len() as u32);
            assemble_frame(&header, &body).unwrap()
        };
        for _ in 0..(SessionEngine::MAX_PENDING_TCP_PREBIND + 5) {
            win.feed_tcp(&frame, t);
        }
        let evs = drain_events(&mut win);
        assert!(
            evs.iter()
                .any(|e| matches!(e, EngineEvent::Error { context, .. }
                if context.contains("暂存业务帧已满"))),
            "超出上限必须出声，实际事件：{evs:?}"
        );
        assert!(
            win.take_tcp_outbound().is_empty(),
            "暂存不得反过来产生出站流量"
        );
        let _ = &mut and;
    }

    /// **BLE 比 TCP 快**时的绑定顺序：Client 先收到 Server 经 BLE 发来的 proof 而当场
    /// bound，之后才收到 Server 经 TCP 发来的 nonce。若 `on_tcp_payload` 在 Bound 时无条件
    /// 拒绝，Client 就永远不发自己的 proof，Server 侧干等——表现为"一端已绑定、另一端绑不上"。
    #[test]
    fn binding_completes_when_ble_proof_arrives_before_tcp_nonce() {
        let t = now();
        let (mut win, mut and) = pair_engines(None, None);
        win.start(t);
        and.start(t);
        pump(&mut win, &mut and, t, 64);
        win.confirm_sas();
        and.confirm_sas();
        pump(&mut win, &mut and, t, 16);
        let _ = drain_events(&mut win);
        let _ = drain_events(&mut and);

        // and = TcpClient（主动连），win = TcpServer（监听方）
        assert!(and.begin_tcp_binding(BindRole::TcpClient));
        assert!(win.begin_tcp_binding(BindRole::TcpServer));

        for f in and.take_tcp_outbound() {
            win.feed_tcp(&f, t);
        }
        // Server 在同一次处理里发出 nonce_S(TCP) 与 proof_C(BLE)：先只把 **BLE** 投给
        // Client，Client 当场 bound，此时它还没见过 nonce_S。
        pump(&mut win, &mut and, t, 8);
        assert!(and.is_tcp_bound(), "Client 应先完成绑定");
        assert!(!win.is_tcp_bound(), "Server 此时还等不到第 3 步 proof");

        // 现在才把 nonce_S 经 TCP 送到 Client：Client 必须补发 proof_S。
        for f in win.take_tcp_outbound() {
            and.feed_tcp(&f, t);
        }
        pump(&mut win, &mut and, t, 8);

        assert!(
            win.is_tcp_bound(),
            "Client 收到迟到的 nonce_S 后必须补发 proof，否则 Server 永远绑不上"
        );
        assert!(and.is_tcp_bound(), "Client 侧绑定不应被退回");
    }

    /// 已绑定后收到一个**不同**的 nonce 仍然要拒绝——否则对端可以拿我们当
    /// HMAC(key, 任意值) 的签名 oracle。
    #[test]
    fn bound_state_still_rejects_a_different_nonce() {
        let t = now();
        let (win, mut and) = paired_and_bound();
        assert!(win.is_tcp_bound());
        let rogue = {
            let body = tlv_codec::encode(&[Tlv::buf(
                linkx_protocol::proto_tlv::tlv::TAG_NONCE_TCP,
                &[7u8; 16],
            )])
            .unwrap();
            let header = FrameHeader::new(msg_type::CHANNEL_BIND, 0, 999, body.len() as u32);
            assemble_frame(&header, &body).unwrap()
        };
        and.feed_tcp(&rogue, t);
        let evs = drain_events(&mut and);
        assert!(
            evs.iter()
                .any(|e| matches!(e, EngineEvent::Error { context, .. }
                if context.contains("nonce 交换失败"))),
            "已绑定后收到不同 nonce 应报错，实际事件：{evs:?}"
        );
        assert!(
            and.is_tcp_bound(),
            "拒绝一个伪造 nonce 不应把已建立的绑定打掉"
        );
    }

    /// TCP 上收到的 PING 必须**从 TCP 回 PONG**：一律 `push_frame`（BLE 出站队列）时 TCP 的
    /// `last_rx` 永远刷不新，心跳形同虚设——这条用例是那个缺陷的回归锁。
    #[test]
    fn tcp_ping_is_answered_on_tcp_not_ble() {
        let t = now();
        let (mut win, mut and) = paired_and_bound();
        let _ = &mut and;
        let ping = {
            let body = ping_payload();
            let header = FrameHeader::new(msg_type::HEARTBEAT, 0, 1, body.len() as u32);
            assemble_frame(&header, &body).unwrap()
        };
        let before_tcp = win.take_tcp_outbound().len();
        let before_ble = win.take_outbound().len();
        win.feed_tcp(&ping, t);
        assert_eq!(
            win.take_outbound().len(),
            before_ble,
            "TCP 收到的 PING 不得回在 BLE 队列上"
        );
        assert!(
            win.take_tcp_outbound().len() > before_tcp,
            "TCP 收到的 PING 必须从 TCP 回 PONG"
        );
    }

    /// TCP 静默超过 `ping_interval + timeout` 必须拆掉这条 LAN 通道，
    /// 但**不能**连带把 BLE 会话也打回重连（BLE 仍是活的）。
    #[test]
    fn tcp_heartbeat_timeout_tears_down_tcp_only() {
        let t = now();
        let (mut win, _and) = paired_and_bound();
        assert!(win.is_tcp_bound());
        let spec = crate::heartbeat::HeartbeatSpec::TCP;
        // 推进超过阈值且不喂任何 TCP 入站；期间只喂 BLE 流量续命 BLE
        win.tick(t + spec.ping_interval + spec.timeout + Duration::from_secs(1));
        assert!(
            !win.is_tcp_bound(),
            "TCP 静默超时后应解除绑定，让文件传输回退 BLE"
        );
        assert!(win.is_paired(), "TCP 死了不代表 BLE 会话结束");
    }

    /// 剪贴板正文超限：拒发并且**出声**。只测发侧是因为收侧那道闸门在同一处判据上，
    /// 而构造一条"合法加密 + 超限正文"的入站帧需要绕过发送口，收益不值当。
    #[test]
    fn oversized_clipboard_is_refused_loudly() {
        let (mut win, _and) = paired_pair();
        let _ = drain_events(&mut win);
        let big = "x".repeat(CLIPBOARD_MAX_BYTES + 1);
        assert!(!win.send_clipboard_text(&big, 1), "超限的剪贴板不许发出去");
        assert!(
            drain_events(&mut win)
                .iter()
                .any(|e| matches!(e, EngineEvent::Error { .. })),
            "拒发必须出声，否则用户以为已经同步了"
        );
        assert!(win.send_clipboard_text("刚好在限内", 1));
    }

    /// 已 accept 但对方永不发 `CHANNEL_BIND`（局域网里任意主机连上端口**一个字节都不发**）
    /// 必须在时限内判死：未绑定的连接不被 TCP 心跳视为业务通道，没有这个超时就等于
    /// 永久占住唯一的槽位，真手机再也接不上，用户只能重启程序。
    #[test]
    fn unbound_tcp_connection_times_out_instead_of_hogging_the_slot() {
        let t = now();
        let (mut win, _and) = paired_pair();
        assert!(win.begin_tcp_binding(BindRole::TcpServer));
        win.tick(t);
        win.tick(t + TCP_BIND_TIMEOUT);
        let evs = drain_events(&mut win);
        assert!(
            evs.iter()
                .any(|e| matches!(e, EngineEvent::TcpUnbound { .. })),
            "绑定超时没出声，平台层就不会 close_link，槽位被永久占住"
        );
        assert!(!win.is_tcp_bound());
    }

    /// BLE 流量不得替 TCP "续命"：两条频道共用一个时间戳时，只要 BLE 还在跳，
    /// 半死的 TCP 会被一直当成健康通道使用。
    #[test]
    fn ble_traffic_does_not_mask_tcp_idle() {
        let t = now();
        let (mut win, mut and) = paired_and_bound();
        let spec = crate::heartbeat::HeartbeatSpec::TCP;
        // 只走 BLE 心跳（证明 BLE 侧一直活着）
        for i in 1..=3 {
            let body = ping_payload();
            let header =
                FrameHeader::new(msg_type::HEARTBEAT, 0, 100 + i as u32, body.len() as u32);
            let f = assemble_frame(&header, &body).unwrap();
            win.feed(&f, t + Duration::from_secs(i * 5));
            let _ = win.take_outbound();
            and.tick(t + Duration::from_secs(i * 5));
        }
        assert!(win.is_paired(), "BLE 侧应仍然健康");
        win.tick(t + spec.ping_interval + spec.timeout + Duration::from_secs(1));
        assert!(
            !win.is_tcp_bound(),
            "BLE 有流量不能替 TCP 续命，TCP 静默超时仍须解除绑定"
        );
    }

    #[test]
    fn tcp_binding_rejected_for_third_party_session_key() {
        // 第三方（错误 session_key）无法通过 TCP 绑定：proof 的 HMAC 必然不符。
        let t = now();
        let (mut win, mut and) = pair_engines(None, None);
        win.start(t);
        and.start(t);
        pump(&mut win, &mut and, t, 64);
        win.confirm_sas();
        and.confirm_sas();
        pump(&mut win, &mut and, t, 16);
        let _ = drain_events(&mut win);

        // Android 侧伪造：直接塞入一个「自己内部绑定状态」的 proof（key 不同）
        assert!(win.begin_tcp_binding(BindRole::TcpClient));
        // 攻击者持有不同 session_key 时无法生成合法 proof；此处用垃圾 proof 模拟
        win.feed_tcp(&tlv_codec::encode(&[Tlv::u8(0x01, 0x02)]).unwrap(), t);

        assert!(!win.is_tcp_bound(), "非法 TCP 载荷不得完成绑定");
    }

    #[test]
    fn file_transfer_over_tcp_roundtrip_with_crc() {
        let t = now();
        let (mut win, mut and) = paired_and_bound();
        assert!(win.is_tcp_bound() && and.is_tcp_bound());
        let _ = drain_events(&mut win);
        let _ = drain_events(&mut and);

        let file_id = 0xABCD_1234u64;
        let payload: Vec<u8> = (0..1000u32).map(|i| (i % 251) as u8).collect();

        let ok = win.send_file_meta(
            &FileMeta {
                name: "报告.pdf".into(),
                size: payload.len() as u64,
                file_id,
                chunk_size: 262_144,
                sha256: vec![0x11; 32].into(),
                crc32: 0,
                album_id: 0,
            },
            t.elapsed().as_millis() as i64,
        );
        assert!(ok, "FILE_META 应发送成功");
        pump_tcp(&mut win, &mut and, t, 8);
        let evs = drain_events(&mut and);
        let meta = evs
            .iter()
            .find_map(|e| match e {
                EngineEvent::FileMetaReceived { name, size, .. } => Some((name.clone(), *size)),
                _ => None,
            })
            .expect("收端应收到 FILE_META");
        assert_eq!(meta.0, "报告.pdf");
        assert_eq!(meta.1, payload.len() as u64);

        let expect_crc = crc32_of(&payload);
        assert!(win.send_file_chunk(file_id, 0, expect_crc, &payload, 0));
        pump_tcp(&mut win, &mut and, t, 8);
        let chunks = and.take_chunks();
        let got = chunks
            .iter()
            .find(|c| c.file_id == file_id)
            .expect("收端应收到 FILE_CHUNK");
        assert_eq!(got.index, 0);
        assert_eq!(got.crc32, expect_crc, "CRC 应一致");
        assert_eq!(got.data, payload, "分块数据应逐字节一致");

        assert!(win.send_file_done(file_id, true, None, 0));
        pump_tcp(&mut win, &mut and, t, 8);
        let evs = drain_events(&mut and);
        assert!(evs.iter().any(|e| matches!(
            e,
            EngineEvent::FileDoneReceived {
                ok: true,
                error: None,
                ..
            }
        )));
    }

    /// `FILE_DONE` 出口时通道锁随一次正常收尾释放；**迟到的** `RESUME` 必须把这条文件重新
    /// 钉回通道，否则补发的每一块都会被 `channel_allows` 判成"没先发 FILE_META"，续传在协议
    /// 层就发不出去，平台侧再怎么重启都是空转。
    #[test]
    fn late_resume_repins_channel_so_the_resend_can_actually_flow() {
        let t = now();
        let (mut win, mut and) = paired_and_bound();
        let _ = drain_events(&mut win);
        let _ = drain_events(&mut and);

        let file_id = 0x5E07_0001u64;
        let payload: Vec<u8> = (0..64u32).map(|i| (i * 3) as u8).collect();
        let crc = crc32_of(&payload);
        assert!(win.send_file_meta(
            &FileMeta {
                name: "clip.mp4".into(),
                size: payload.len() as u64,
                file_id,
                chunk_size: 262_144,
                sha256: vec![0x22; 32].into(),
                crc32: 0,
                album_id: 0,
            },
            t.elapsed().as_millis() as i64,
        ));
        pump_tcp(&mut win, &mut and, t, 8);
        let _ = drain_events(&mut and);
        assert!(win.send_file_chunk(file_id, 0, crc, &payload, 0));
        // 发端先说了"发完了"：通道锁就在这一步释放
        assert!(win.send_file_done(file_id, true, None, 0));
        pump_tcp(&mut win, &mut and, t, 8);
        let _ = drain_events(&mut and);

        // 收端清点出自己的洞，在这之后才发出续传请求（迟到的 RESUME）
        assert!(and.send_file_resume(file_id, 0, t.elapsed().as_millis() as i64));
        pump_tcp(&mut win, &mut and, t, 8);
        let evs = drain_events(&mut win);
        assert!(
            evs.iter().any(|e| matches!(
                e,
                EngineEvent::FileResumeRequested { file_id: f, from_index: 0 } if *f == file_id
            )),
            "迟到的续传请求必须上报给平台"
        );
        assert!(
            win.send_file_chunk(file_id, 0, crc, &payload, 0),
            "迟到续传之后补发的分块要还能发得出去，否则这条路径结构上不可能成功"
        );
        assert!(win.send_file_done_digest(file_id, true, Some([0x22; 32]), None, 0));
        pump_tcp(&mut win, &mut and, t, 8);
        let got = and.take_chunks();
        assert!(
            got.iter().any(|c| c.file_id == file_id && c.index == 0),
            "补发的块要真能到得了对端"
        );
    }

    /// FILE_DONE 的摘要必须原样过线；FILE_META 不带摘要时上报 `None`（不伪造全零值）
    #[test]
    fn file_done_digest_crosses_tcp_and_meta_may_omit_digest() {
        let t = now();
        let (mut win, mut and) = paired_and_bound();
        let _ = drain_events(&mut win);
        let _ = drain_events(&mut and);
        let file_id = 0xF00D_0001u64;

        assert!(win.send_file_meta(
            &FileMeta {
                name: "big.bin".into(),
                size: 8_600_000_000,
                file_id,
                chunk_size: 262_144,
                sha256: Default::default(), // 流式发端：META 里没有摘要
                crc32: 0,
                album_id: 0,
            },
            0,
        ));
        pump_tcp(&mut win, &mut and, t, 8);
        let evs = drain_events(&mut and);
        assert!(
            evs.iter().any(|e| matches!(
                e,
                EngineEvent::FileMetaReceived {
                    sha256: None,
                    size: 8_600_000_000,
                    ..
                }
            )),
            "摘要缺位须作为 None 上报，收端才会等 FILE_DONE：实际 {evs:?}"
        );

        let sha = [7u8; 32];
        assert!(win.send_file_done_digest(file_id, true, Some(sha), None, 0));
        pump_tcp(&mut win, &mut and, t, 8);
        let evs = drain_events(&mut and);
        assert!(
            evs.iter().any(|e| matches!(
                e,
                EngineEvent::FileDoneReceived {
                    ok: true,
                    sha256: Some(s),
                    ..
                } if *s == sha
            )),
            "FILE_DONE 的摘要须原样到达收端：实际 {evs:?}"
        );
    }

    /// 发端取消：两端都落 `FileTaskCancelled`（不是 `FileTaskFailed`），且通道锁被释放。
    /// 断言"不是失败"是验收点：取消是用户的主动动作，报成失败就是谎报系统故障。
    #[test]
    fn sender_cancel_reports_cancelled_on_both_sides() {
        let t = now();
        let (mut win, mut and) = paired_and_bound();
        let _ = drain_events(&mut win);
        let _ = drain_events(&mut and);
        let file_id = 0xC0FF_EE01u64;
        let payload = vec![3u8; 64];
        assert!(win.send_file_meta(
            &FileMeta {
                name: "a.bin".into(),
                size: payload.len() as u64,
                file_id,
                chunk_size: 262_144,
                sha256: Default::default(),
                crc32: 0,
                album_id: 0,
            },
            0,
        ));
        pump_tcp(&mut win, &mut and, t, 8);
        let _ = drain_events(&mut and);
        assert!(win.send_file_chunk(file_id, 0, crc32_of(&payload), &payload, 0));

        assert!(
            win.cancel_file_send(file_id, "电脑上取消了", 0),
            "取消的结束帧应入队"
        );
        pump_tcp(&mut win, &mut and, t, 8);

        let evs = drain_events(&mut win);
        assert!(
            evs.iter().any(|e| matches!(
                e,
                EngineEvent::FileTaskCancelled { file_id: f, reason }
                    if *f == file_id && reason == "电脑上取消了"
            )),
            "发端应上报取消：{evs:?}"
        );
        assert!(
            !evs.iter()
                .any(|e| matches!(e, EngineEvent::FileTaskFailed { .. })),
            "取消不是失败：{evs:?}"
        );

        let evs = drain_events(&mut and);
        assert!(
            evs.iter().any(|e| matches!(
                e,
                EngineEvent::FileTaskCancelled { file_id: f, reason }
                    if *f == file_id && reason == "电脑上取消了"
            )),
            "收端应把 FILE_DONE{{cancelled:true}} 报成取消：{evs:?}"
        );
        assert!(
            !evs.iter()
                .any(|e| matches!(e, EngineEvent::FileDoneReceived { .. })),
            "取消收尾不得再以普通结束帧出现（那会被读成失败）：{evs:?}"
        );
        assert!(
            !evs.iter()
                .any(|e| matches!(e, EngineEvent::FileTaskFailed { .. })),
            "取消不是失败：{evs:?}"
        );

        // 通道锁必须随取消释放，否则下一条文件永远发不出去
        let next = 0xC0FF_EE02u64;
        assert!(win.send_file_meta(
            &FileMeta {
                name: "b.bin".into(),
                size: 8,
                file_id: next,
                chunk_size: 262_144,
                sha256: Default::default(),
                crc32: 0,
                album_id: 0,
            },
            0,
        ));
        assert!(
            win.send_file_chunk(next, 0, crc32_of(&[1u8; 8]), &[1u8; 8], 0),
            "取消后下一条文件应能立刻开传（通道锁已释放）"
        );
    }

    /// 收端取消：FILE_CANCEL 让发端真的停手，两端同样只报取消。
    #[test]
    fn receiver_cancel_stops_the_sender_and_reports_cancelled_on_both_sides() {
        let t = now();
        let (mut win, mut and) = paired_and_bound();
        let _ = drain_events(&mut win);
        let _ = drain_events(&mut and);
        let file_id = 0xC0FF_EE03u64;
        assert!(win.send_file_meta(
            &FileMeta {
                name: "video.mp4".into(),
                size: 8_600_000_000,
                file_id,
                chunk_size: 262_144,
                sha256: Default::default(),
                crc32: 0,
                album_id: 0,
            },
            0,
        ));
        pump_tcp(&mut win, &mut and, t, 8);
        let _ = drain_events(&mut and);

        assert!(
            and.cancel_file_recv(file_id, "手机上取消了", 0),
            "取消帧应入队"
        );
        pump_tcp(&mut win, &mut and, t, 8);

        let evs = drain_events(&mut and);
        assert!(
            evs.iter().any(|e| matches!(
                e,
                EngineEvent::FileTaskCancelled { file_id: f, .. } if *f == file_id
            )),
            "收端本地就该落取消：{evs:?}"
        );
        let evs = drain_events(&mut win);
        assert!(
            evs.iter().any(|e| matches!(
                e,
                EngineEvent::FileTaskCancelled { file_id: f, reason }
                    if *f == file_id && reason == "手机上取消了"
            )),
            "发端收到 FILE_CANCEL 后应停手并上报取消：{evs:?}"
        );
        assert!(
            !evs.iter()
                .any(|e| matches!(e, EngineEvent::FileTaskFailed { .. })),
            "被对端取消不是本端失败：{evs:?}"
        );

        // 停手：发端此后递上来的分块被拒，且**不**变成 FileTaskFailed
        assert!(
            !win.send_file_chunk(file_id, 1, 0, &[9u8; 8], 0),
            "取消后不得再往外发分块"
        );
        let evs = drain_events(&mut win);
        assert!(
            !evs.iter()
                .any(|e| matches!(e, EngineEvent::FileTaskFailed { .. })),
            "取消后的分块拒绝必须保持取消语义：{evs:?}"
        );
        assert_eq!(win.late_frames_dropped(), 1, "被拒的出站分块要计数留痕");

        let next = 0xC0FF_EE04u64;
        assert!(win.send_file_meta(
            &FileMeta {
                name: "c.bin".into(),
                size: 8,
                file_id: next,
                chunk_size: 262_144,
                sha256: Default::default(),
                crc32: 0,
                album_id: 0,
            },
            0,
        ));
        assert!(win.send_file_chunk(next, 0, crc32_of(&[2u8; 8]), &[2u8; 8], 0));
    }

    /// TCP 断开时要替本机**在途接收**判失败；已经收完的不许再补一刀。通道锁只有发送方会钉，
    /// 所以断链那一刻发送方拿得到 `FileTaskFailed`，接收方过去什么也拿不到——电脑早判失败，
    /// 手机那一行永远挂着「接收中」。登记 / 注销放在引擎里，它是唯一知道这条接收还开着的一方。
    #[test]
    fn tcp_close_fails_inflight_receive_but_not_a_settled_one() {
        let t = now();
        let (mut win, mut and) = paired_and_bound();
        let _ = drain_events(&mut win);
        let inflight = 0xB058_0001u64;
        let settled = 0xB058_0002u64;
        let meta = |file_id: u64| FileMeta {
            name: "case.bin".into(),
            size: 1024,
            file_id,
            chunk_size: 256,
            sha256: Default::default(),
            crc32: 0,
            album_id: 0,
        };

        // 两条接收：一条停在半路，一条已经收到对端的结束帧
        assert!(win.send_file_meta(&meta(inflight), 0));
        pump_tcp(&mut win, &mut and, t, 8);
        assert!(win.send_file_meta(&meta(settled), 0));
        pump_tcp(&mut win, &mut and, t, 8);
        assert!(win.send_file_done(settled, true, None, 0));
        pump_tcp(&mut win, &mut and, t, 8);
        let _ = drain_events(&mut and);
        assert_eq!(
            and.recv_open.iter().copied().collect::<Vec<_>>(),
            vec![inflight],
            "落定的那条该注销登记，半路的留着"
        );

        and.on_tcp_closed("测试：对端拔线");
        let evs = drain_events(&mut and);
        assert!(
            evs.iter().any(
                |e| matches!(e, EngineEvent::FileTaskFailed { file_id, .. } if *file_id == inflight)
            ),
            "断链必须替在途接收判失败：{evs:?}"
        );
        assert!(
            !evs.iter().any(
                |e| matches!(e, EngineEvent::FileTaskFailed { file_id, .. } if *file_id == settled)
            ),
            "已经收完的传输不许被补一刀：{evs:?}"
        );
        assert!(
            and.recv_open.is_empty(),
            "判过失败后登记要清空，否则下一次断链会重复报错"
        );
    }

    /// 取消与在途帧的竞态：迟到分块/结束帧一律丢弃并计数，不当成新会话、不静默吞掉。
    #[test]
    fn late_chunk_and_done_after_cancel_are_dropped_and_counted() {
        let t = now();
        let (mut win, mut and) = paired_and_bound();
        let _ = drain_events(&mut win);
        let _ = drain_events(&mut and);
        let file_id = 0xC0FF_EE05u64;
        assert!(win.send_file_meta(
            &FileMeta {
                name: "big.iso".into(),
                size: 1024,
                file_id,
                chunk_size: 262_144,
                sha256: Default::default(),
                crc32: 0,
                album_id: 0,
            },
            0,
        ));
        pump_tcp(&mut win, &mut and, t, 8);
        let _ = drain_events(&mut and);
        let _ = and.take_chunks();

        // 收端先取消，发端那边块已经在路上（取消那一刻无法召回）
        assert!(and.cancel_file_recv(file_id, "手机上取消了", 0));
        let payload = vec![7u8; 32];
        assert!(win.send_file_chunk(file_id, 1, crc32_of(&payload), &payload, 0));
        assert!(win.send_file_done(file_id, true, None, 0));
        pump_tcp(&mut win, &mut and, t, 8);

        assert!(
            and.take_chunks()
                .iter()
                .all(|c| c.file_id != file_id || c.index != 1),
            "取消后的迟到分块不得进平台队列（否则落盘一个没人要的文件）"
        );
        let evs = drain_events(&mut and);
        assert!(
            !evs.iter()
                .any(|e| matches!(e, EngineEvent::FileMetaReceived { .. })),
            "迟到分块不得被当成一次新会话：{evs:?}"
        );
        assert!(
            !evs.iter()
                .any(|e| matches!(e, EngineEvent::FileDoneReceived { .. })),
            "迟到的结束帧不得覆盖已落的取消状态：{evs:?}"
        );
        assert_eq!(
            and.late_frames_dropped(),
            2,
            "分块与结束帧各丢一次都要计数（丢弃必须可查）"
        );
    }

    /// FILE_DONE 的 cancelled 位向后兼容：proto3 的 `false` 不上线，所以 `cancelled = false`
    /// 编出来的就是旧端那份字节，语义逐字节一致。
    #[test]
    fn legacy_file_done_without_cancelled_field_behaves_as_before() {
        let t = now();
        let (mut win, mut and) = paired_and_bound();
        let _ = drain_events(&mut win);
        let _ = drain_events(&mut and);
        let file_id = 0xC0FF_EE06u64;
        assert!(win.send_file_meta(
            &FileMeta {
                name: "d.bin".into(),
                size: 8,
                file_id,
                chunk_size: 262_144,
                sha256: vec![0x11; 32].into(),
                crc32: 0,
                album_id: 0,
            },
            0,
        ));
        pump_tcp(&mut win, &mut and, t, 8);
        let _ = drain_events(&mut and);

        // 旧端口径的失败收尾（没有 tag 5）：仍走 FileDoneReceived，不产生取消事件
        assert!(win.send_file_done(file_id, false, Some("磁盘满了"), 0));
        pump_tcp(&mut win, &mut and, t, 8);
        let evs = drain_events(&mut and);
        assert!(
            evs.iter().any(|e| matches!(
                e,
                EngineEvent::FileDoneReceived {
                    file_id: f,
                    ok: false,
                    error: Some(msg),
                    ..
                } if *f == file_id && msg == "磁盘满了"
            )),
            "旧端的失败 FILE_DONE 须原样上报：{evs:?}"
        );
        assert!(
            !evs.iter()
                .any(|e| matches!(e, EngineEvent::FileTaskCancelled { .. })),
            "旧端没说要取消，凭空造出取消事件就是假消息：{evs:?}"
        );
        assert_eq!(and.late_frames_dropped(), 0);
    }

    /// 已配对但 TCP 未绑定：**大文件必须大声失败**，不得降级走 BLE。
    ///
    /// 根治闸门：旧行为是 FILE_META 挤得过 BLE、256KB 分块被 BLE 队列丢弃，用户看到"已完成"
    /// 而对端只有 0 字节文件。
    #[test]
    fn large_file_refused_when_tcp_unbound() {
        let t = now();
        let (mut win, mut and) = pair_engines(None, None);
        win.start(t);
        and.start(t);
        pump(&mut win, &mut and, t, 64);
        win.confirm_sas();
        and.confirm_sas();
        pump(&mut win, &mut and, t, 16);
        assert!(win.is_paired(), "应已配对");
        assert!(!win.is_tcp_bound(), "本用例前提：TCP 未绑定");
        let _ = drain_events(&mut win);

        let ok = win.send_file_meta(
            &FileMeta {
                name: "big.bin".into(),
                size: 5 * 1024 * 1024,
                file_id: 0x1,
                chunk_size: 262_144,
                sha256: vec![0u8; 32].into(),
                crc32: 0,
                album_id: 0,
            },
            0,
        );
        assert!(
            !ok,
            "TCP 未绑定时 5 MB 文件不得发送（旧行为：降级 BLE → 分块静默丢失）"
        );
        let evs = drain_events(&mut win);
        assert!(
            evs.iter()
                .any(|e| matches!(e, EngineEvent::FileTaskFailed { file_id: 1, .. })),
            "必须产生用户可见的失败事件，不能静默：实际事件 = {evs:?}"
        );
        assert!(
            !win.send_file_chunk(1, 0, 0, &[7u8; 1024], 0),
            "未绑定时的分块必须被拒"
        );
    }

    /// 单块小文件在未绑定时仍允许走 BLE（保留既有小载荷能力，别一刀切）
    #[test]
    fn small_file_still_allowed_over_ble_when_unbound() {
        let t = now();
        let (mut win, mut and) = pair_engines(None, None);
        win.start(t);
        and.start(t);
        pump(&mut win, &mut and, t, 64);
        win.confirm_sas();
        and.confirm_sas();
        pump(&mut win, &mut and, t, 16);
        let _ = drain_events(&mut win);
        let _ = drain_events(&mut and);

        assert!(
            win.send_file_meta(
                &FileMeta {
                    name: "note.txt".into(),
                    size: 1000,
                    file_id: 0x2,
                    chunk_size: 262_144,
                    sha256: vec![0u8; 32].into(),
                    crc32: 0,
                    album_id: 0,
                },
                0,
            ),
            "单块小文件在未绑定时应可经 BLE 送出"
        );
        assert!(
            win.send_file_chunk(2, 0, 0, &[3u8; 1000], 0),
            "已钉在 BLE 的分块应放行"
        );
        pump(&mut win, &mut and, t, 16);
        let evs = drain_events(&mut and);
        assert!(
            evs.iter()
                .any(|e| matches!(e, EngineEvent::FileMetaReceived { file_id: 2, .. })),
            "对端应收到元信息：{evs:?}"
        );
    }

    /// 传输途中 TCP 断开：在途任务当场失败，绝不改走 BLE
    #[test]
    fn mid_transfer_tcp_close_fails_task_without_ble_fallback() {
        let (mut win, mut and) = paired_and_bound();
        let _ = drain_events(&mut win);
        let _ = drain_events(&mut and);

        assert!(
            win.send_file_meta(
                &FileMeta {
                    name: "movie.mp4".into(),
                    size: 5 * 1024 * 1024,
                    file_id: 0x3,
                    chunk_size: 262_144,
                    sha256: vec![0u8; 32].into(),
                    crc32: 0,
                    album_id: 0,
                },
                0,
            ),
            "已绑定 TCP 时应可开始传输"
        );
        assert!(win.send_file_chunk(3, 0, 0, &[9u8; 4096], 0));
        let _ = drain_events(&mut win);

        win.on_tcp_closed("测试：链路断开");
        let evs = drain_events(&mut win);
        assert!(
            evs.iter()
                .any(|e| matches!(e, EngineEvent::TcpUnbound { .. })),
            "应上报 TCP 解绑"
        );
        assert!(
            evs.iter()
                .any(|e| matches!(e, EngineEvent::FileTaskFailed { file_id: 3, .. })),
            "在途文件必须被硬失败而不是悄悄改走 BLE：{evs:?}"
        );
        assert!(
            !win.send_file_chunk(3, 1, 0, &[9u8; 4096], 0),
            "失败后后续分块必须被拒"
        );
        let _ = and;
    }

    /// 分块不得先于 FILE_META —— 防止"没有接收会话的分块"这种孤儿数据静默累积
    #[test]
    fn chunk_before_meta_is_refused() {
        let (mut win, mut and) = paired_and_bound();
        let _ = drain_events(&mut win);
        let _ = drain_events(&mut and);

        assert!(
            !win.send_file_chunk(0x99, 0, 0, &[1u8; 10], 0),
            "未发 META 就发分块必须被拒"
        );
        let evs = drain_events(&mut win);
        assert!(
            evs.iter()
                .any(|e| matches!(e, EngineEvent::FileTaskFailed { file_id: 0x99, .. })),
            "孤儿分块要大声报错而不是进 in_chunks 等超时：{evs:?}"
        );
        let _ = and;
    }

    /// 一条传输收尾时**只能取走自己的分块**，其余留在队列里。现场：一次导出 3 张相册原图，
    /// 手机连着发 META₁·块₁·DONE₁·META₂·块₂·DONE₂；处理 DONE₁ 时整队取走就会在第 2 张的 META
    /// 之前把它的块全喂进来，那些块找不到收件会话只能判成孤儿丢弃。
    #[test]
    fn take_chunks_for_leaves_other_files_queued() {
        let t = now();
        let (mut win, mut and) = paired_and_bound();
        for id in [1u64, 2] {
            assert!(
                and.send_file_meta(
                    &FileMeta {
                        name: format!("f{id}.bin"),
                        size: 8,
                        file_id: id,
                        chunk_size: 8,
                        sha256: vec![0u8; 32].into(),
                        crc32: 0,
                        album_id: 0,
                    },
                    0,
                ),
                "第 {id} 条 META 应发出"
            );
            assert!(
                and.send_file_chunk(id, 0, 0, &[id as u8; 8], 0),
                "第 {id} 条分块应发出"
            );
        }
        pump_tcp(&mut win, &mut and, t, 8);

        let one = win.take_chunks_for(1);
        assert_eq!(one.len(), 1, "只应取走 file 1 的分块");
        assert_eq!(one[0].file_id, 1);
        let rest = win.take_chunks();
        assert_eq!(
            rest.len(),
            1,
            "file 2 的分块必须还留在队列里，等它自己的 META 被处理"
        );
        assert_eq!(rest[0].file_id, 2);
    }
    /// 测试用 CRC32（与 transfer::chunk::crc32 同算法，避免 session 依赖 transfer）
    fn crc32_of(data: &[u8]) -> u32 {
        let mut crc = 0xFFFF_FFFFu32;
        for &b in data {
            crc ^= b as u32;
            for _ in 0..8 {
                let mask = (crc & 1).wrapping_neg();
                crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
            }
        }
        !crc
    }

    #[test]
    fn file_resume_request_parsed() {
        let t = now();
        let (mut win, mut and) = paired_and_bound();
        let _ = drain_events(&mut win);
        let _ = drain_events(&mut and);

        let file_id = 0x1122_3344_5566_7788u64;
        assert!(win.send_file_resume(file_id, 7, 0));
        pump_tcp(&mut win, &mut and, t, 8);
        let evs = drain_events(&mut and);
        assert!(evs.iter().any(|e| matches!(
            e,
            EngineEvent::FileResumeRequested {
                file_id: id,
                from_index: 7,
            } if *id == file_id
        )));
    }

    #[test]
    fn config_sync_roundtrip() {
        let t = now();
        let (mut win, mut and) = paired_and_bound();
        let _ = drain_events(&mut win);
        let _ = drain_events(&mut and);

        let entries = vec![
            ConfigEntryItem {
                key: "notify.blacklist".into(),
                value: "com.example.app".into(),
                scope: "cross".into(),
            },
            ConfigEntryItem {
                key: "clip.sensitive.skip".into(),
                value: "1".into(),
                scope: "per_peer".into(),
            },
        ];
        assert!(win.send_config(&entries, 0));
        pump_tcp(&mut win, &mut and, t, 8);
        let evs = drain_events(&mut and);
        let got = evs
            .iter()
            .find_map(|e| match e {
                EngineEvent::ConfigReceived { entries } => Some(entries.clone()),
                _ => None,
            })
            .expect("收端应收到配置同步");
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].key, "notify.blacklist");
        assert_eq!(got[0].value, "com.example.app");
        assert_eq!(got[1].scope, "per_peer");
    }

    #[test]
    fn tcp_close_resets_binding_state() {
        let (mut win, and) = paired_and_bound();
        assert!(win.is_tcp_bound());
        let _ = drain_events(&mut win);
        win.on_tcp_closed("socket closed");
        assert!(!win.is_tcp_bound(), "TCP 断开后绑定状态必须复位");
        let evs = drain_events(&mut win);
        assert!(evs
            .iter()
            .any(|e| matches!(e, EngineEvent::TcpUnbound { .. })));
        let _ = and;
    }

    #[test]
    fn unbind_peer_clears_trust_and_session() {
        let t = now();
        let (mut win, mut and) = pair_engines(None, None);
        win.start(t);
        and.start(t);
        pump(&mut win, &mut and, t, 64);
        win.confirm_sas();
        and.confirm_sas();
        pump(&mut win, &mut and, t, 16);
        assert!(win.is_paired());
        win.unbind_peer();
        assert!(!win.is_paired(), "解绑后不得仍处 PAIRED");
        assert!(win.trusted_peers().is_empty(), "解绑须清除信任库");
    }

    #[test]
    fn notification_key_hash_is_forwarded() {
        let t = now();
        let (mut win, mut and) = pair_engines(None, None);
        win.start(t);
        and.start(t);
        pump(&mut win, &mut and, t, 64);
        win.confirm_sas();
        and.confirm_sas();
        pump(&mut win, &mut and, t, 16);
        let _ = drain_events(&mut win);

        let _ = and.send_notification(
            &NotificationPush {
                package: "com.tencent.mm".into(),
                title: "微信".into(),
                text: "新消息".into(),
                post_ts_ms: 1_700_000_000_000,
                key_hash: 0xDEAD_BEEF,
                cover_jpeg: Default::default(),
                ..Default::default()
            },
            0,
        );
        pump(&mut win, &mut and, t, 32);
        let evs = drain_events(&mut win);
        let n = evs
            .iter()
            .find_map(|e| match e {
                EngineEvent::Notification { key_hash, .. } => Some(*key_hash),
                _ => None,
            })
            .expect("应收到通知");
        assert_eq!(n, 0xDEAD_BEEF, "稳定 key 必须透传到平台层");
    }

    #[test]
    fn notify_reply_round_trip() {
        let t = now();
        let (mut win, mut and) = paired_pair();
        let _ = drain_events(&mut win);
        let _ = drain_events(&mut and);

        // 电脑 → 手机：回复请求（定位三元组 + 要填的 key + 正文，一条都不能丢）
        assert!(win.send_notify_reply(
            &NotificationReply {
                reply_id: 3,
                package: "org.telegram.messenger".into(),
                tag: String::new(),
                notification_id: 77,
                action_index: 0,
                result_key: "key_reply_text".into(),
                text: "马上到".into(),
            },
            1,
        ));
        pump(&mut win, &mut and, t, 16);
        let evs = drain_events(&mut and);
        assert!(evs.iter().any(|e| matches!(
            e,
            EngineEvent::NotifyReplyRequested {
                reply_id: 3,
                package,
                notification_id: 77,
                result_key,
                text,
                ..
            } if package == "org.telegram.messenger"
                && result_key == "key_reply_text"
                && text == "马上到"
        )));

        // 手机 → 电脑：失败回执也要原样回到界面（"该应用不支持回复"是给用户看的）
        assert!(and.send_notify_reply_ack(
            &NotificationReplyAck {
                reply_id: 3,
                package: "org.telegram.messenger".into(),
                ok: false,
                error: "该应用不支持回复".into(),
            },
            1,
        ));
        pump(&mut win, &mut and, t, 16);
        let evs = drain_events(&mut win);
        assert!(evs.iter().any(|e| matches!(
            e,
            EngineEvent::NotifyReplyAck { reply_id: 3, ok: false, error, .. }
                if error == "该应用不支持回复"
        )));
    }

    #[test]
    fn notify_reply_is_not_sent_while_unpaired() {
        // 这条会真的把文字送进对端某个应用，未配对时连"发出去"都不许发生
        let (mut win, _and) = pair_engines(None, None);
        assert!(!win.send_notify_reply(
            &NotificationReply {
                reply_id: 1,
                package: "com.example".into(),
                tag: String::new(),
                notification_id: 1,
                action_index: 0,
                result_key: "k".into(),
                text: "hi".into(),
            },
            1,
        ));
    }

    #[test]
    fn notify_dismiss_reaches_the_pc() {
        let t = now();
        let (mut win, mut and) = paired_pair();
        let _ = drain_events(&mut win);
        let _ = drain_events(&mut and);
        assert!(and.send_notify_dismiss(
            &NotificationDismiss {
                package: "com.android.messaging".into(),
                tag: "sms".into(),
                notification_id: 42,
                key_hash: 7,
            },
            1,
        ));
        pump(&mut win, &mut and, t, 16);
        let got = drain_events(&mut win).iter().any(|e| {
            matches!(
                e,
                EngineEvent::NotifyDismissed { package, tag, notification_id: 42, key_hash: 7 }
                    if package == "com.android.messaging" && tag == "sms"
            )
        });
        assert!(
            got,
            "通知消失没到电脑，回复入口就会留在一条已经不存在的通知上"
        );
    }

    /// 已配对管道（供下面几条 MTU / 封顶用例复用）
    fn paired_pair() -> (SessionEngine, SessionEngine) {
        let t = now();
        let (mut win, mut and) = pair_engines(None, None);
        win.start(t);
        and.start(t);
        pump(&mut win, &mut and, t, 64);
        win.confirm_sas();
        and.confirm_sas();
        pump(&mut win, &mut and, t, 16);
        (win, and)
    }

    #[test]
    fn ble_mtu_setter_rejects_out_of_range() {
        let (mut win, _and) = paired_pair();
        assert_eq!(win.ble_mtu(), BLE_MTU, "初值必须是保守的 23");
        for bad in [0usize, 1, 22, BLE_MTU_MAX + 1, usize::MAX] {
            win.set_ble_mtu(bad);
            assert_eq!(win.ble_mtu(), BLE_MTU, "越界值 {bad} 不得改变现值");
        }
        win.set_ble_mtu(BLE_MTU_MAX);
        assert_eq!(win.ble_mtu(), BLE_MTU_MAX);
    }

    /// -212 的机理回归：协商到 517 却按 23 切片，一条消息白切 37 倍片数，而 Central 每片都
    /// 要阻塞写一次 → 单轮 flush 秒级 → 收包与心跳被饿死。
    #[test]
    fn negotiated_mtu_collapses_fragment_count() {
        let payload = "x".repeat(2000);

        let (mut win, _and) = paired_pair();
        assert!(win.send_clipboard_text(&payload, 1));
        let at23 = win.take_outbound().len();

        let (mut win, _and) = paired_pair();
        win.set_ble_mtu(BLE_MTU_MAX);
        assert!(win.send_clipboard_text(&payload, 1));
        let at517 = win.take_outbound().len();

        assert!(
            at517 * 6 <= at23,
            "MTU 23→517 应显著减少片数：at23={at23} at517={at517}"
        );
    }

    /// 两侧 MTU 不一致必须仍能重组：链路一侧升级到 517、另一侧仍按 23 发片，
    /// 接收侧的 `per` 只是容量提示，绝不能成为拒收理由。
    #[test]
    fn asymmetric_mtu_still_delivers() {
        let t = now();
        let (mut win, mut and) = paired_pair();
        win.set_ble_mtu(BLE_MTU_MAX); // 只升 Windows 侧
        assert!(win.send_clipboard_text("跨 MTU 的载荷", 1));
        pump(&mut win, &mut and, t, 16);
        let evs = drain_events(&mut and);
        assert!(evs
            .iter()
            .any(|e| matches!(e, EngineEvent::Clipboard { text } if text == "跨 MTU 的载荷")));

        // 反方向：对端仍按 23 切片发过来
        assert!(and.send_clipboard_text("小片方向", 1));
        pump(&mut win, &mut and, t, 16);
        let evs = drain_events(&mut win);
        assert!(evs
            .iter()
            .any(|e| matches!(e, EngineEvent::Clipboard { text } if text == "小片方向")));
    }

    /// 每轮封顶：取走 n 片后其余必须**留在队列**且顺序不变。丢包的话接收侧分组永远凑不齐 →
    /// 消息静默消失（比慢更糟）。
    #[test]
    fn take_outbound_n_is_bounded_lossless_and_fifo() {
        let (mut win, _and) = paired_pair();
        win.set_ble_mtu(BLE_MTU); // 刻意留在 23，制造多片
        assert!(win.send_clipboard_text(&"y".repeat(400), 1));

        // 只验「序号语义」，不比字节：两台引擎的会话密钥 / nonce 不同，密文天然不等。
        let total = win.outbound_pending();
        assert!(total > 20, "400B 在 MTU23 下应切出 >20 片，实际 {total}");

        let mut got: Vec<Vec<u8>> = Vec::new();
        for _ in 0..100 {
            let chunk = win.take_outbound_n(8);
            if chunk.is_empty() {
                break;
            }
            assert!(chunk.len() <= 8, "单轮封顶被突破：{}", chunk.len());
            got.extend(chunk);
        }
        assert_eq!(got.len(), total, "封顶 drain 不得丢片");
        assert_eq!(win.outbound_pending(), 0);

        let ids: Vec<(u16, u16, u16)> = got
            .iter()
            .map(|p| {
                (
                    u16::from_be_bytes([p[0], p[1]]),
                    u16::from_be_bytes([p[2], p[3]]),
                    u16::from_be_bytes([p[4], p[5]]),
                )
            })
            .collect();
        let cnt = ids[0].2;
        assert_eq!(cnt as usize, total, "frag_cnt 应等于总片数");
        for (i, &(msg_id, idx, c)) in ids.iter().enumerate() {
            assert_eq!(msg_id, ids[0].0, "同一条消息必须同 msg_id（第 {i} 片）");
            assert_eq!(c, cnt, "frag_cnt 必须一致");
            assert_eq!(
                idx as usize, i,
                "封顶 drain 必须保持 FIFO（第 {i} 片 idx={idx}）"
            );
        }
    }

    /// 分片长度自适应：收到比当前假设更长的对端分片，就证明链路支持到那个尺寸，本端出站
    /// 分片随之抬升（只升不降、夹在规范上限内）。用观测代替 `MaxPduSize` 查询：那个 COM 会话
    /// 对象的创建 / 丢弃与堆损坏崩溃相关，而"对端能发多长"本身就是链路能力的直接证据。
    #[test]
    fn inbound_fragment_length_raises_outbound_mtu() {
        const HDR: usize = 6; // 分片头：msg_id u16 | idx u16 | cnt u16
        let t = now();
        let (mut win, _and) = paired_pair();
        assert_eq!(win.ble_mtu(), BLE_MTU, "初值应为保守的 23");

        let frag = |id: u16, len: usize| -> Vec<u8> {
            let mut p = vec![0x5Au8; len];
            p[0..2].copy_from_slice(&id.to_be_bytes());
            p[2..4].copy_from_slice(&0u16.to_be_bytes());
            p[4..6].copy_from_slice(&2u16.to_be_bytes());
            p
        };

        // 对端发来一片 512B（HyperOS 实测的可写上限）
        win.feed(&frag(9001, 512), t);
        assert!(
            win.ble_mtu() > BLE_MTU,
            "收到长片后出站分片长度应抬升，实际 {}",
            win.ble_mtu()
        );
        let raised = win.ble_mtu();

        // 随后收到短片不得把已抬升的值降回去
        win.feed(&frag(9002, HDR + 4), t);
        assert_eq!(win.ble_mtu(), raised, "只升不降");

        // 不可信输入：超长片必须被夹在规范上限内
        win.feed(&frag(9003, 100_000), t);
        assert!(
            win.ble_mtu() <= BLE_MTU_MAX,
            "MTU 不得越过规范上限，实际 {}",
            win.ble_mtu()
        );
    }

    /// DISCOVER 态自愈：退避耗尽后两端都回到 DISCOVER，而 `tick` 只驱动 Paired /
    /// Reconnecting——DISCOVER 下没人再发 HELLO，两台设备就会安静地互相等，永久死锁。
    #[test]
    fn discover_state_reattaches_after_reconnect_exhausted() {
        let t = now();
        let (mut win, mut and) = pair_engines(None, None);
        win.start(t);
        and.start(t);
        pump(&mut win, &mut and, t, 64);
        win.confirm_sas();
        and.confirm_sas();
        pump(&mut win, &mut and, t, 16);
        assert!(win.is_paired() && and.is_paired());

        // 两端同时静默：各自心跳超时 → 退避 6 次耗尽 → 双双回到 DISCOVER。
        let mut clock = t;
        for _ in 0..12 {
            clock += Duration::from_secs(40);
            win.tick(clock);
            and.tick(clock);
        }
        assert_eq!(win.state(), SessionState::Discover, "win 应已耗尽退避");
        assert_eq!(and.state(), SessionState::Discover, "and 应已耗尽退避");

        // 死锁自检：不再重播的话，两端会永远停在 DISCOVER
        let _ = win.take_outbound();
        let _ = and.take_outbound();
        clock += DISCOVER_REATTACH + Duration::from_secs(1);
        win.tick(clock);
        assert!(
            !win.take_outbound().is_empty(),
            "DISCOVER 态下 Initiator 必须重播 HELLO，否则两端互相干等"
        );

        for _ in 0..10 {
            clock += DISCOVER_REATTACH + Duration::from_secs(1);
            win.tick(clock);
            and.tick(clock);
            pump(&mut win, &mut and, clock, 32);
            if win.sas().is_some() || and.sas().is_some() {
                win.confirm_sas();
                and.confirm_sas();
                pump(&mut win, &mut and, clock, 32);
            }
            if win.is_paired() && and.is_paired() {
                return; // 自愈成功
            }
        }
        panic!(
            "DISCOVER 重播 HELLO 后未能自愈：win={:?} and={:?}",
            win.state(),
            and.state()
        );
    }

    /// 播放状态（手机 → 电脑）与播放指令（电脑 → 手机）双向都要真的送达：刻意不写"发出去
    /// 没报错"，而是**在对端的事件队列里看到它**——最容易出的问题是"一侧以为发了、另一侧没解出来"。
    #[test]
    fn media_state_and_command_roundtrip() {
        let t = now();
        // 业务时间戳是 epoch 毫秒，`t` 是 Instant（喂帧用的逻辑时钟），两者不能混用。
        const TM: i64 = 1_790_000_000_000;
        let (mut win, mut and) = pair_engines(None, None);
        win.start(t);
        and.start(t);
        pump(&mut win, &mut and, t, 64);
        win.confirm_sas();
        and.confirm_sas();
        pump(&mut win, &mut and, t, 16);
        let _ = drain_events(&mut win);
        let _ = drain_events(&mut and);

        // 手机 → 电脑：播放状态
        assert!(and.send_media_state(
            &MediaState {
                package: "com.netease.cloudmusic".into(),
                title: "夜曲".into(),
                artist: "周杰伦".into(),
                album: "十一月的萧邦".into(),
                playing: true,
                position_ms: 42_000,
                duration_ms: 227_000,
                speed: 1.25,
                ts_ms: TM,
                volume: 60,
            },
            TM
        ));
        let pkts = and.take_outbound();
        assert!(!pkts.is_empty(), "应有分片产出");
        for p in &pkts {
            win.feed(p, t);
        }
        let evs = drain_events(&mut win);
        let got = evs
            .iter()
            .find_map(|e| match e {
                EngineEvent::MediaState {
                    title,
                    artist,
                    playing,
                    position_ms,
                    speed_x100,
                    volume,
                    ..
                } => Some((
                    title.clone(),
                    artist.clone(),
                    *playing,
                    *position_ms,
                    *speed_x100,
                    *volume,
                )),
                _ => None,
            })
            .expect("电脑侧应收到 MediaState 事件");
        assert_eq!(got.0, "夜曲", "标题必须原样到达（含中文）");
        assert_eq!(got.1, "周杰伦");
        assert!(got.2, "playing 必须为真");
        assert_eq!(got.3, 42_000);
        assert_eq!(got.4, 125, "1.25 倍速应换算成 125");
        assert_eq!(got.5, 60, "音量必须原样到达（电脑侧 +/- 以它为基准）");

        // 电脑 → 手机：播放指令
        assert!(win.send_media_command(
            linkx_protocol::pb::media_command::Action::Next as i32,
            0,
            0,
            TM
        ));
        for p in &win.take_outbound() {
            and.feed(p, t);
        }
        let got_cmd = drain_events(&mut and)
            .into_iter()
            .find_map(|e| match e {
                EngineEvent::MediaCommand { action, .. } => Some(action),
                _ => None,
            })
            .expect("手机侧应收到 MediaCommand 事件");
        assert_eq!(
            got_cmd,
            linkx_protocol::pb::media_command::Action::Next as i32
        );
    }

    /// 媒体必须能走局域网：手机退到后台时最先断的是蓝牙（换地址、掐广播），钉死在蓝牙上
    /// 等于"只在 App 前台时可控制"。这条用例锁住路由，防止以后又被改回 `send_encrypted`。
    #[test]
    fn media_prefers_the_lan_channel_when_tcp_is_bound() {
        let t = now();
        const TM: i64 = 1_790_000_000_000;
        let (mut win, mut and) = paired_and_bound();
        assert!(win.is_tcp_bound() && and.is_tcp_bound());
        let _ = drain_events(&mut win);
        let _ = drain_events(&mut and);

        // 电脑 → 手机：指令排进 TCP 队列，并且**经 TCP 真的送达**
        assert!(win.send_media_command(
            linkx_protocol::pb::media_command::Action::Next as i32,
            0,
            0,
            TM
        ));
        let out = win.take_tcp_outbound();
        assert!(
            out.iter().any(|f| matches!(parse_full_frame(f), Ok((h, _))
                if h.msg_type == msg_type::MEDIA_COMMAND)),
            "已绑定时播放指令应排进 TCP 队列"
        );
        for f in &out {
            and.feed_tcp(f, t);
        }
        assert!(
            drain_events(&mut and)
                .iter()
                .any(|e| matches!(e, EngineEvent::MediaCommand { .. })),
            "手机侧应经 TCP 收到 MediaCommand"
        );

        // 手机 → 电脑：播放状态同理
        assert!(and.send_media_state(
            &MediaState {
                package: "com.netease.cloudmusic".into(),
                title: "夜曲".into(),
                artist: "周杰伦".into(),
                album: "十一月的萧邦".into(),
                playing: true,
                position_ms: 1_000,
                duration_ms: 227_000,
                speed: 1.0,
                ts_ms: TM,
                volume: 42,
            },
            TM
        ));
        let out = and.take_tcp_outbound();
        assert!(
            out.iter().any(|f| matches!(parse_full_frame(f), Ok((h, _))
                if h.msg_type == msg_type::MEDIA_STATE)),
            "已绑定时播放状态应排进 TCP 队列"
        );
        for f in &out {
            win.feed_tcp(f, t);
        }
        assert!(
            drain_events(&mut win)
                .iter()
                .any(|e| matches!(e, EngineEvent::MediaState { .. })),
            "电脑侧应经 TCP 收到 MediaState"
        );
    }

    /// 相册契约：问与答各走一次局域网 TCP，两端拿到的都是**类型明确**的事件。
    #[test]
    fn album_list_and_thumb_round_trip_over_tcp() {
        let t = now();
        const TM: i64 = 1_790_000_000_000;
        let (mut win, mut and) = paired_and_bound();
        let _ = drain_events(&mut win);
        let _ = drain_events(&mut and);

        assert!(win.send_album_list_request(2, 60, TM), "清单请求应发出");
        let out = win.take_tcp_outbound();
        assert!(
            out.iter().any(|f| matches!(parse_full_frame(f), Ok((h, _))
                if h.msg_type == msg_type::ALBUM_LIST_REQ)),
            "相册请求必须走 TCP 队列"
        );
        for f in &out {
            and.feed_tcp(f, t);
        }
        let req = drain_events(&mut and)
            .into_iter()
            .find_map(|e| match e {
                EngineEvent::AlbumListRequested { page, per_page } => Some((page, per_page)),
                _ => None,
            })
            .expect("手机侧应收到明确的清单请求事件");
        assert_eq!(req, (2, 60));

        let reply = AlbumList {
            items: vec![
                AlbumItem {
                    id: 77,
                    name: "IMG_0001.jpg".into(),
                    size_bytes: 4_000_000,
                    mtime_ms: TM,
                    width: 4000,
                    height: 3000,
                    kind: 0,
                    duration_ms: 0,
                },
                // 视频：类型与时长必须原样过一遍线（老电脑端读不到这两段也照样渲染）
                AlbumItem {
                    id: 78,
                    name: "VID_2026.mp4".into(),
                    size_bytes: 88_000_000,
                    mtime_ms: TM,
                    width: 1920,
                    height: 1080,
                    kind: 1,
                    duration_ms: 23_450,
                },
            ],
            page: 2,
            total: 121,
            error: String::new(),
        };
        assert!(and.send_album_list(&reply, TM));
        for f in &and.take_tcp_outbound() {
            win.feed_tcp(f, t);
        }
        let page = drain_events(&mut win)
            .into_iter()
            .find_map(|e| match e {
                EngineEvent::AlbumPage {
                    items,
                    page,
                    total,
                    error,
                } => Some((items, page, total, error)),
                _ => None,
            })
            .expect("电脑侧应收到清单页");
        assert_eq!(page.1, 2);
        assert_eq!(page.2, 121);
        assert!(page.3.is_empty(), "成功时不该带错误");
        assert_eq!(page.0.first().map(|i| i.id), Some(77));
        assert_eq!(page.0.len(), 2, "两条都得过线，不能被截断成一条");
        let vid = page
            .0
            .iter()
            .find(|i| i.id == 78)
            .expect("视频那条必须原样到达");
        assert_eq!(
            (vid.kind, vid.duration_ms),
            (1, 23_450),
            "类型与时长是电脑画时长角标的全部依据，丢了就只能显示成照片"
        );

        // 缩略图：请求 → 带 JPEG 的应答
        assert!(win.send_album_thumb_request(77, 256, TM));
        for f in &win.take_tcp_outbound() {
            and.feed_tcp(f, t);
        }
        assert!(
            drain_events(&mut and)
                .iter()
                .any(|e| matches!(e, EngineEvent::AlbumThumbRequested { id: 77, edge: 256 })),
            "手机侧应收到缩略图请求"
        );
        assert!(and.send_album_thumb(
            &AlbumThumb {
                id: 77,
                edge: 256,
                width: 256,
                height: 192,
                jpeg: vec![0xFF, 0xD8, 0xFF, 0xD9].into(),
                error: String::new(),
            },
            TM
        ));
        for f in &and.take_tcp_outbound() {
            win.feed_tcp(f, t);
        }
        let got = drain_events(&mut win)
            .into_iter()
            .find_map(|e| match e {
                EngineEvent::AlbumThumb {
                    id, jpeg, error, ..
                } => Some((id, jpeg, error)),
                _ => None,
            })
            .expect("电脑侧应收到缩略图");
        assert_eq!(got.0, 77);
        assert_eq!(got.1, vec![0xFF, 0xD8, 0xFF, 0xD9]);
        assert!(got.2.is_empty());
    }

    /// 相册一律不许降级蓝牙：TCP 没绑定就是"这条请求没发出"，不是"改走蓝牙试试"。
    #[test]
    fn album_requests_fail_loudly_when_tcp_not_bound() {
        let t = now();
        let (mut win, mut and) = pair_engines(None, None);
        win.start(t);
        and.start(t);
        pump(&mut win, &mut and, t, 64);
        win.confirm_sas();
        and.confirm_sas();
        pump(&mut win, &mut and, t, 16);
        assert!(!win.is_tcp_bound(), "前提：本用例就是没绑定的情况");
        assert!(win.is_paired(), "前提：本用例是已配对但没绑定 TCP");
        assert!(
            !win.send_album_list_request(0, 60, t.elapsed().as_millis() as i64),
            "未绑定 TCP 时相册请求不得发出"
        );
        let evs = drain_events(&mut win);
        assert!(
            evs.iter()
                .any(|e| matches!(e, EngineEvent::Error { context, .. }
                    if context.contains("TCP"))),
            "必须留下可读原因，不能静默失败；实际 = {evs:?}"
        );
    }

    /// 满窗口只该挡分块。曾经的症状是"电脑点了取消，手机永远停在传输中"——结束帧撞在满
    /// 窗口上被丢，而它恰恰是一整条传输里最不能丢的那一帧。
    #[test]
    fn saturated_tcp_window_still_delivers_the_done_frame() {
        let t = now();
        const TM: i64 = 1_790_000_000_000;
        let (mut win, mut and) = paired_and_bound();
        let _ = drain_events(&mut win);
        let _ = drain_events(&mut and);

        let mut sent = 0u32;
        assert!(
            win.send_file_meta(
                &FileMeta {
                    name: "取消我.bin".into(),
                    size: 4096 * 64,
                    file_id: 0x42,
                    chunk_size: 4096,
                    sha256: vec![0u8; 32].into(),
                    crc32: 0,
                    album_id: 0,
                },
                TM
            ),
            "FILE_META 应发送成功（它同时把这次传输钉到 TCP 通道上）"
        );
        while win.send_file_chunk(0x42, sent, 0, &[0u8; 4096], TM) {
            sent += 1;
        }
        assert!(
            sent >= SessionEngine::TCP_OUT_WINDOW as u32 - 1,
            "窗口应能被分块灌满（实际只进了 {sent} 个）"
        );
        assert!(
            !win.send_file_chunk(0x42, sent, 0, &[0u8; 4096], TM),
            "分块在满窗口上必须被拒，否则背压形同虚设"
        );
        assert!(
            win.send_file_done_digest(0x42, true, None, None, TM),
            "FILE_DONE 不得因窗口满被丢"
        );

        let out = win.take_tcp_outbound();
        assert!(
            out.iter().any(|f| matches!(parse_full_frame(f), Ok((h, _))
                if h.msg_type == msg_type::FILE_DONE)),
            "结束帧应真的排进 TCP 队列"
        );
        for f in &out {
            and.feed_tcp(f, t);
        }
        assert!(
            drain_events(&mut and)
                .iter()
                .any(|e| matches!(e, EngineEvent::FileDoneReceived { .. })),
            "对端应收到结束帧，而不是永远等在「传输中」"
        );
    }

    /// 未配对时一律不发媒体帧：那等于把"正在听什么"明文送给链路对端。
    #[test]
    fn media_state_requires_paired() {
        let t = now();
        const TM: i64 = 1_790_000_000_000;
        let (mut win, mut and) = pair_engines(None, None);
        win.start(t);
        and.start(t);
        pump(&mut win, &mut and, t, 4);
        assert!(!and.send_media_state(&MediaState::default(), TM));
        assert!(!win.send_media_command(0, 0, 0, TM));
    }

    /// 电量上报双向门禁：未配对不发、不采纳；配对后原样到达。
    #[test]
    fn device_status_roundtrip_and_pairing_gate() {
        let t = now();
        const TM: i64 = 1_790_000_000_000;
        let (mut win, mut and) = pair_engines(None, None);
        win.start(t);
        and.start(t);
        pump(&mut win, &mut and, t, 4);
        assert!(
            !and.send_device_status(
                &DeviceStatus {
                    battery: 88,
                    charging: false,
                    ts_ms: TM
                },
                TM
            ),
            "未配对不得上报电量"
        );

        pump(&mut win, &mut and, t, 64);
        win.confirm_sas();
        and.confirm_sas();
        pump(&mut win, &mut and, t, 16);
        let _ = drain_events(&mut win);
        let _ = drain_events(&mut and);

        assert!(and.send_device_status(
            &DeviceStatus {
                battery: 88,
                charging: true,
                ts_ms: TM
            },
            TM
        ));
        // send_routed：TCP 未绑定时回退 BLE 出口，这里两条都要喂进对端
        let mut pkts = and.take_outbound();
        pkts.extend(and.take_tcp_outbound());
        assert!(!pkts.is_empty(), "应有设备状态帧产出");
        for p in &pkts {
            win.feed(p, t);
        }
        let got = drain_events(&mut win)
            .into_iter()
            .find_map(|e| match e {
                EngineEvent::DeviceStatus {
                    battery,
                    charging,
                    ts_ms,
                } => Some((battery, charging, ts_ms)),
                _ => None,
            })
            .expect("电脑侧应收到 DeviceStatus 事件");
        assert_eq!(got, (88, true, TM), "电量、充电态与时间戳必须原样到达");
    }

    /// **收侧**门禁：握手完成 ≠ 配对完成。SAS 还没在本端核对时收到的播放指令一律不采纳——
    /// 否则用户正在比对六位码，对端已经能拨动本机音量与切歌。这条测入向路径：删掉
    /// `on_media_command` 里的 `is_paired` 判掉，本测试必须红（出向用例挡不住）。
    #[test]
    fn media_command_is_not_executed_before_local_side_paired() {
        let t = now();
        const TM: i64 = 1_790_000_000_000;
        let (mut win, mut and) = pair_engines(None, None);
        win.start(t);
        and.start(t);
        pump(&mut win, &mut and, t, 32);
        // 只让电脑确认 SAS：手机此时仍在 SasCompare（人工比对没做完）
        win.confirm_sas();
        pump(&mut win, &mut and, t, 32);
        assert!(win.is_paired(), "电脑已确认 SAS，应进入 Paired");
        assert!(
            !and.is_paired(),
            "手机没核对 SAS，不能因为对端确认了就自认已配对"
        );

        assert!(
            win.send_media_command(
                linkx_protocol::pb::media_command::Action::SetVolume as i32,
                11,
                0,
                TM
            ),
            "已配对的电脑侧可以发指令"
        );
        for p in win.take_outbound() {
            and.feed(&p, t);
        }
        let evs = drain_events(&mut and);
        assert!(
            !evs.iter()
                .any(|e| matches!(e, EngineEvent::MediaCommand { .. })),
            "未配对的手机侧不得把播放指令交给平台执行"
        );

        // 手机核对 SAS 后，同一条链路应恢复正常（证明上面不是把通道整个弄坏了）
        and.confirm_sas();
        pump(&mut win, &mut and, t, 32);
        assert!(and.is_paired(), "手机确认 SAS 后应进入 Paired");
        assert!(win.send_media_command(
            linkx_protocol::pb::media_command::Action::Next as i32,
            0,
            0,
            TM
        ));
        for p in win.take_outbound() {
            and.feed(&p, t);
        }
        assert!(
            drain_events(&mut and)
                .iter()
                .any(|e| matches!(e, EngineEvent::MediaCommand { .. })),
            "配对完成后播放指令必须照常送达"
        );
    }

    /// 握手完成 ≠ 配对完成：本端还没核对 SAS，对端就不能推通知、写剪贴板、开始往磁盘落文件。
    /// 门禁在 `on_encrypted` 的分派入口统一把关，这里把每条业务消息都过一遍。
    /// 相册五条不在列：它们只走局域网 TCP，而未配对的一方连 TCP 都绑不上（`begin_tcp_binding`
    /// 就挡着），帧只会进绑定前的暂存队列 —— 那条路径另有测试守着。
    #[test]
    fn business_messages_are_not_processed_before_local_side_paired() {
        let t = now();
        const TM: i64 = 1_790_000_000_000;
        let file_id = 0x0A0B_0C0Du64;
        let payload: Vec<u8> = vec![7u8; 64];
        let meta = || FileMeta {
            name: "报告.pdf".into(),
            size: payload.len() as u64,
            file_id,
            chunk_size: 262_144,
            sha256: vec![0x11; 32].into(),
            crc32: 0,
            album_id: 0,
        };

        // 只让电脑完成人工比对：手机停在 SasCompare，两边都已握手、都握有会话密钥
        let (mut win, mut and) = pair_engines(None, None);
        win.start(t);
        and.start(t);
        pump(&mut win, &mut and, t, 32);
        win.confirm_sas();
        pump(&mut win, &mut and, t, 32);
        assert!(
            win.is_paired() && !and.is_paired(),
            "前提：只有一端完成了人工比对"
        );
        let _ = drain_events(&mut win);
        let _ = drain_events(&mut and);

        /// 电脑发的那条已被手机收到：不得产出业务事件，但必须大声报错（不许静默丢）
        fn refused(
            win: &mut SessionEngine,
            and: &mut SessionEngine,
            t: Instant,
            what: &str,
            leaks: impl Fn(&EngineEvent) -> bool,
        ) {
            for p in win.take_outbound() {
                and.feed(&p, t);
            }
            let evs = drain_events(and);
            assert!(!evs.iter().any(&leaks), "未配对的一侧不得采纳 {}", what);
            assert!(
                has_error(&evs, err_code::IO_GENERIC),
                "{} 被拒必须大声报错，不能静默丢",
                what
            );
        }

        assert!(win.send_notification(
            &NotificationPush {
                package: "com.example.chat".into(),
                title: "小明".into(),
                text: "在吗？".into(),
                post_ts_ms: TM,
                key_hash: 7,
                cover_jpeg: Default::default(),
                tag: "chat".into(),
                notification_id: 21,
                can_reply: false,
                reply_action_index: 0,
                reply_result_key: String::new(),
            },
            TM
        ));
        refused(&mut win, &mut and, t, "NOTIFY_PUSH", |e| {
            matches!(e, EngineEvent::Notification { .. })
        });

        assert!(win.send_notify_dismiss(
            &NotificationDismiss {
                package: "com.example.chat".into(),
                tag: "chat".into(),
                notification_id: 21,
                key_hash: 7,
            },
            TM
        ));
        refused(&mut win, &mut and, t, "NOTIFY_DISMISS", |e| {
            matches!(e, EngineEvent::NotifyDismissed { .. })
        });

        assert!(win.send_notify_reply_ack(
            &NotificationReplyAck {
                reply_id: 3,
                package: "com.example.chat".into(),
                ok: true,
                error: String::new(),
            },
            TM
        ));
        refused(&mut win, &mut and, t, "NOTIFY_REPLY_ACK", |e| {
            matches!(e, EngineEvent::NotifyReplyAck { .. })
        });

        assert!(win.send_clipboard_text("未配对就想写我剪贴板", TM));
        refused(&mut win, &mut and, t, "CLIPBOARD_PUSH", |e| {
            matches!(e, EngineEvent::Clipboard { .. })
        });

        assert!(win.send_file_meta(&meta(), TM));
        refused(&mut win, &mut and, t, "FILE_META", |e| {
            matches!(e, EngineEvent::FileMetaReceived { .. })
        });

        // FILE_CHUNK 的出口不是事件而是待写分块：上一条 META 已把发端通道钉住，这里单独验
        assert!(win.send_file_chunk(file_id, 0, crc32_of(&payload), &payload, TM));
        for p in win.take_outbound() {
            and.feed(&p, t);
        }
        assert!(and.take_chunks().is_empty(), "未配对的一侧不得接收文件分块");
        assert!(
            has_error(&drain_events(&mut and), err_code::IO_GENERIC),
            "FILE_CHUNK 被拒必须大声报错"
        );

        assert!(win.send_file_done(file_id, true, None, TM));
        refused(&mut win, &mut and, t, "FILE_DONE", |e| {
            matches!(e, EngineEvent::FileDoneReceived { .. })
        });

        assert!(win.send_file_cancel(file_id, "对端取消", TM));
        refused(&mut win, &mut and, t, "FILE_CANCEL", |e| {
            matches!(e, EngineEvent::FileTaskCancelled { .. })
        });

        assert!(win.send_file_resume(file_id, 3, TM));
        refused(&mut win, &mut and, t, "RESUME", |e| {
            matches!(e, EngineEvent::FileResumeRequested { .. })
        });

        // 反向对照：手机核对完 SAS，同一条链路上的剪贴板必须照常送达（证明上面没把通道弄坏）
        and.confirm_sas();
        pump(&mut win, &mut and, t, 32);
        assert!(and.is_paired(), "手机核对 SAS 后应进入 Paired");
        let _ = drain_events(&mut win);
        let _ = drain_events(&mut and);
        assert!(win.send_clipboard_text("配对之后就正常了", TM));
        for p in win.take_outbound() {
            and.feed(&p, t);
        }
        assert!(
            drain_events(&mut and).iter().any(|e| matches!(
                e,
                EngineEvent::Clipboard { text } if text == "配对之后就正常了"
            )),
            "配对完成后业务消息必须照常采纳"
        );
    }
    /// HELLO 的序号只用来认"是不是同一条"：已经处理过的那条再投一次，不该把会话第二次打死；
    /// 序号变了（对端真重启、或断线后重新贴上来）才复位重握手。
    #[test]
    fn duplicated_hello_does_not_reset_the_session_twice() {
        fn hello_body(seq: Option<u64>) -> Vec<u8> {
            let mut items = vec![
                Tlv::buf(TAG_ADVERT_NAME, b"pixel-7"),
                Tlv::u8(TAG_OS, linkx_protocol::OS_ANDROID),
                Tlv::buf(TAG_VERSION, b"0.5.1"),
            ];
            if let Some(s) = seq {
                items.push(Tlv::buf(TAG_HELLO_SEQ, &s.to_be_bytes()));
            }
            tlv_codec::encode(&items).unwrap()
        }

        let t = now();
        let (mut win, mut and) = pair_engines(None, None);
        win.start(t);
        and.start(t);
        pump(&mut win, &mut and, t, 32);
        win.confirm_sas();
        and.confirm_sas();
        pump(&mut win, &mut and, t, 32);
        assert!(win.is_paired() && and.is_paired(), "前提：两端都已配对");

        let seq = 0x1122_3344_5566_7788u64;
        // 本端已经处理过序号为 seq 的那一条（配对过程中收到的）
        win.peer_hello_seq = Some(seq);
        // ① 同一条迟到重投：会话保持，密钥还在，记下的序号也不该被改写
        win.on_hello(&hello_body(Some(seq)));
        assert!(win.is_paired(), "同一条 HELLO 的重投不得作废会话");
        assert!(win.session_key.is_some(), "重投之后会话密钥应还在");
        assert_eq!(win.peer_hello_seq, Some(seq), "重投不该改写已记下的序号");

        // ② 序号变了 = 对端要重开会话 → 复位，回 DISCOVER 重握手
        win.on_hello(&hello_body(Some(seq + 1)));
        assert!(
            !win.is_paired(),
            "新一次 HELLO 必须让本端复位，否则两边各抱着旧会话卡死"
        );
        assert_eq!(win.peer_hello_seq, Some(seq + 1));
    }

    /// 旧版本对端的 HELLO 没有序号：只能按老办法每次当新会话（兼容优先）
    #[test]
    fn legacy_hello_without_seq_still_resets() {
        let t = now();
        let (mut win, mut and) = pair_engines(None, None);
        win.start(t);
        and.start(t);
        pump(&mut win, &mut and, t, 32);
        win.confirm_sas();
        and.confirm_sas();
        pump(&mut win, &mut and, t, 32);
        assert!(win.is_paired(), "前提：两端都已配对");
        let legacy = tlv_codec::encode(&[
            Tlv::buf(TAG_ADVERT_NAME, b"old-phone"),
            Tlv::u8(TAG_OS, linkx_protocol::OS_ANDROID),
            Tlv::buf(TAG_VERSION, b"0.4.5"),
        ])
        .unwrap();
        let before = win.peer_hello_seq;
        win.on_hello(&legacy);
        assert!(
            !win.is_paired(),
            "无序号的旧式 HELLO 仍触发复位（保持旧行为）"
        );
        assert_eq!(
            win.peer_hello_seq, before,
            "旧式 HELLO 没有序号，不该改写已记下的对端序号"
        );
    }

    /// TCP 的流量只推进 TCP 自己的计时，不顶掉 BLE 的存活判据 —— 否则"蓝牙已断、局域网还活着"
    /// 会被读成两条链路都健康，BLE 那侧永远等不到超时重连。
    #[test]
    fn tcp_traffic_does_not_keep_the_ble_liveness_clock() {
        let t = now();
        let (mut win, mut and) = paired_and_bound();
        assert!(
            win.is_tcp_bound() && and.is_tcp_bound(),
            "前提：局域网已绑定"
        );
        let ble_before = win.last_rx;

        // TCP 侧来回一趟心跳（心跳节奏 10s，取刚过一次的点）
        let t2 = t + Duration::from_secs(11);
        win.tick(t2);
        pump_tcp(&mut win, &mut and, t2, 4);
        assert_eq!(win.last_rx, ble_before, "TCP 帧不该刷新 BLE 的活跃时间");
        assert_eq!(win.last_tcp_rx, Some(t2), "TCP 侧的活跃时间应照常推进");
    }

    /// 未走完的身份漂移不能跨会话残留：它是"这一次要淘汰哪条旧记录"的凭据，用户没确认就
    /// 去配别的设备，留着会让下一次 `learn_trusted()` 静默淘汰那条无关的信任记录。
    #[test]
    fn abandoned_identity_drift_does_not_survive_the_session() {
        let t = now();
        let (mut win, mut and) = pair_engines(None, None);
        win.start(t);
        and.start(t);
        pump(&mut win, &mut and, t, 32);
        win.confirm_sas();
        and.confirm_sas();
        pump(&mut win, &mut and, t, 32);
        assert!(win.is_paired() && and.is_paired(), "前提：两端都已配对");
        let peer_fp = win.peer_fp.clone().expect("已配对应有对端指纹");
        assert!(
            win.cfg
                .trusted_peers
                .iter()
                .any(|p| p.fingerprint == peer_fp),
            "配对完成应写入信任库"
        );

        // 身份漂移挂起（用户还没点「重新配对」），随后本端复位重来
        win.drift_old_fp = Some(peer_fp.clone());
        win.reset_session();
        assert!(
            win.drift_old_fp.is_none(),
            "复位必须清掉未消费的漂移凭据，否则会带走无关设备的信任记录"
        );

        // 复位后与另一台新设备完成配对：原那条记录不该被顺手淘汰
        let (mut win2, mut and2) = pair_engines(None, None);
        win2.start(t);
        and2.start(t);
        pump(&mut win2, &mut and2, t, 32);
        win2.confirm_sas();
        and2.confirm_sas();
        pump(&mut win2, &mut and2, t, 32);
        win2.cfg.trusted_peers.insert(
            0,
            TrustedPeer {
                fingerprint: peer_fp.clone(),
                name: "旧手机".into(),
            },
        );
        let before = win2.cfg.trusted_peers.len();
        win2.learn_trusted("ffffffffffffffff".into());
        assert_eq!(
            win2.cfg.trusted_peers.len(),
            before + 1,
            "新身份只该新增一条，不该淘汰未确认漂移的旧条目"
        );
        assert!(win2
            .cfg
            .trusted_peers
            .iter()
            .any(|p| p.fingerprint == peer_fp));
    }

    /// 重复/迟到的 BLE proof 不该拆掉已经建好的局域网绑定：`verify_ble_proof` 第一步就按
    /// "已 Bound"判 `AlreadyBound`，这不是冒充，是同一份证明的第二次到达。
    #[test]
    fn duplicate_ble_proof_keeps_the_healthy_tcp_binding() {
        let (mut win, _and) = paired_and_bound();
        assert!(win.is_tcp_bound(), "前提：局域网已绑定");
        let _ = drain_events(&mut win);

        win.on_channel_bind(&[linkx_protocol::TAG_NONCE_TCP, 0], false);
        assert!(win.is_tcp_bound(), "重复 proof 不该拆掉健康的局域网绑定");
        assert!(
            !drain_events(&mut win)
                .iter()
                .any(|e| matches!(e, EngineEvent::Error { .. })),
            "重复 proof 不该报成绑定失败"
        );
    }

    /// 真冒充（nonce 对不上）仍然必须当场拆链
    #[test]
    fn forged_ble_proof_still_breaks_the_binding() {
        let t = now();
        let (mut win, mut and) = pair_engines(None, None);
        win.start(t);
        and.start(t);
        pump(&mut win, &mut and, t, 32);
        win.confirm_sas();
        and.confirm_sas();
        pump(&mut win, &mut and, t, 32);
        assert!(win.begin_tcp_binding(BindRole::TcpClient));
        assert!(and.begin_tcp_binding(BindRole::TcpServer));
        pump_tcp(&mut win, &mut and, t, 16);
        pump(&mut win, &mut and, t, 16);
        assert!(win.is_tcp_bound(), "前提：正常绑定已完成");
        let _ = drain_events(&mut win);

        // 已 Bound 之后再来一份**内容不同**的 proof：仍是 AlreadyBound（先查状态再解析），
        // 而绑定未完成时的伪造 proof 走 TagMismatch 分支，下面这条用例覆盖后者
        let (mut w2, mut a2) = pair_engines(None, None);
        w2.start(t);
        a2.start(t);
        pump(&mut w2, &mut a2, t, 32);
        w2.confirm_sas();
        a2.confirm_sas();
        pump(&mut w2, &mut a2, t, 32);
        assert!(w2.begin_tcp_binding(BindRole::TcpClient));
        assert!(a2.begin_tcp_binding(BindRole::TcpServer));
        pump_tcp(&mut w2, &mut a2, t, 16);
        let _ = drain_events(&mut w2);
        // 伪造一份 proof：nonce 用 w2 发出去的那条（已通过 TCP 交换拿到），tag 乱填
        w2.on_channel_bind(
            &[
                linkx_protocol::TAG_NONCE_TCP,
                8,
                1,
                2,
                3,
                4,
                5,
                6,
                7,
                8,
                linkx_protocol::TAG_BIND_TAG,
                16,
                0,
                0,
                0,
                0,
                0,
                0,
                0,
                0,
                0,
                0,
                0,
                0,
                0,
                0,
                0,
                0,
                0,
            ],
            false,
        );
        assert!(
            !w2.is_tcp_bound(),
            "未完成状态收到伪造 proof 必须拒绝，且不得置为已绑定"
        );
        assert!(
            drain_events(&mut w2)
                .iter()
                .any(|e| matches!(e, EngineEvent::Error { .. })),
            "伪造 proof 必须大声报错"
        );
    }

    /// SAS 判不一致之后必须丢掉会话密钥：状态迁移只是"不再采纳业务消息"，
    /// 密钥留着等于链路层面还能被继续解密的既成事实。
    #[test]
    fn sas_mismatch_clears_the_session_key() {
        let t = now();
        let (mut win, mut and) = pair_engines(None, None);
        win.start(t);
        and.start(t);
        pump(&mut win, &mut and, t, 32);
        let sas = and.sas.expect("两端应已算出 SAS");
        assert_eq!(win.sas, Some(sas), "前提：两端 SAS 一致");
        assert!(
            and.session_key.is_some(),
            "前提：握手已完成、会话密钥已建立"
        );
        let _ = drain_events(&mut and);

        // 人为制造不一致（等价于中间人改过展示码）
        and.sas = Some((sas + 1) % 1_000_000);
        win.send_pair_confirm();
        for p in win.take_outbound() {
            and.feed(&p, t);
        }
        let evs = drain_events(&mut and);
        assert!(
            evs.iter().any(|e| matches!(e, EngineEvent::Error { .. })),
            "SAS 不一致必须出声"
        );
        assert!(!and.is_paired(), "不一致的一侧不得进入 Paired");
        assert!(
            and.session_key.is_none(),
            "SAS 判不一致后必须清掉会话密钥，否则终态还能解密业务帧"
        );
    }

    /// 配对最后一步的校验基准不能塌成空串：本机身份取不到指纹时，"对端声明了一个空指纹"
    /// 会让等号成立，于是这道人工闸门被自动放行。
    #[test]
    fn pair_done_is_rejected_when_own_fingerprint_is_unavailable() {
        let (mut win, _and) = paired_pair();
        let flow = win.pair.clone().expect("前提：配对流程还在");
        win.peer_done_verified = false;

        // 对照组：身份在位时空指纹只是"不一致"，走的是原来的那条报错
        win.on_pair_done(1, &flow.pair_done_plaintext(""));
        assert!(
            !win.peer_done_verified,
            "本机指纹非空时，空指纹载荷不得判为通过"
        );
        let _ = win.take_events();

        win.identity = None;
        win.on_pair_done(2, &flow.pair_done_plaintext(""));
        assert!(
            !win.peer_done_verified,
            "本机身份不可用时，空指纹载荷不得判为通过"
        );
        let evs = win.take_events();
        assert!(
            evs.iter()
                .any(|e| matches!(e, EngineEvent::Error { context, .. }
                if context.contains("本机身份不可用"))),
            "取不到基准指纹必须出声，实际事件：{evs:?}"
        );
    }

    /// 广播名超上限时的两端口径：发端截断不得劈开多字节字符（劈开了对端 `from_utf8` 失败，
    /// 整台设备的名字会变成空的），收端也要按同一个上限截（发端截断不代表收到的不会超长）。
    #[test]
    fn oversized_advert_name_is_capped_without_breaking_characters() {
        // 发端：ASCII 超长截到上限；CJK 超长退到字符边界
        assert_eq!(
            truncate_name(&"A".repeat(100), HELLO_NAME_MAX),
            vec![b'A'; HELLO_NAME_MAX]
        );
        let cjk = truncate_name(&"智".repeat(20), HELLO_NAME_MAX);
        assert!(
            cjk.len() < HELLO_NAME_MAX,
            "32 落在一个 3 字节字符中间，应退到边界"
        );
        assert_eq!(String::from_utf8(cjk.clone()).unwrap().chars().count(), 10);

        // 收端：40 字节的 ASCII 名字截到 32，且仍然是一条正常的 PeerHello
        let t = now();
        let (mut win, _) = pair_engines(None, None);
        win.start(t);
        let long = "B".repeat(40);
        let body = tlv_codec::encode(&[
            Tlv::buf(TAG_ADVERT_NAME, long.as_bytes()),
            Tlv::u8(TAG_OS, linkx_protocol::OS_ANDROID),
            Tlv::buf(TAG_VERSION, b"0.5.1"),
        ])
        .unwrap();
        win.on_hello(&body);
        let evs = drain_events(&mut win);
        let got = evs
            .iter()
            .find_map(|e| match e {
                EngineEvent::PeerHello { name, .. } => Some(name.clone()),
                _ => None,
            })
            .expect("HELLO 应上报 PeerHello");
        assert_eq!(got.len(), HELLO_NAME_MAX, "收端应按同一上限截断");

        // 收端遇到劈成半截的多字节字符：整段坏字节不采纳，宁可名字为空
        let (mut win2, _) = pair_engines(None, None);
        win2.start(t);
        let broken = vec![TAG_ADVERT_NAME, 3, b'X', 0xE6, b'Y'];
        win2.on_hello(&broken);
        assert!(drain_events(&mut win2).iter().any(|e| matches!(
            e,
            EngineEvent::PeerHello { name, .. } if name.is_empty()
        )));
    }
}
