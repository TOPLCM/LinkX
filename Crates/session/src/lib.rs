//! LinkX session crate：会话状态机、心跳与退避、配对流程、TCP 通道绑定与信任库。

pub mod binding;
/// 通知正文里的验证码提取（两端共用规则）
pub mod code_extract;
pub mod engine;
pub mod heartbeat;
pub mod pairing;
pub mod state;
pub mod trust;

pub use binding::{BindError, BindRole, BindStep, ChannelBinding};
pub use engine::{
    ConfigEntryItem, EngineConfig, EngineEvent, EngineRole, IncomingChunk, SessionEngine,
    TrustedPeer,
};
pub use heartbeat::{is_ping, is_pong, ping_payload, pong_payload, Backoff, HeartbeatSpec};
pub use pairing::{PairConfirm, PairFlow};
pub use state::{SessionChannel, SessionEvent, SessionManager, SessionState, TofuVerdict};
pub use trust::{is_valid_fingerprint, parse_tsv, to_tsv, upsert, FINGERPRINT_HEX_LEN};
