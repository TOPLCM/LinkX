//! 心跳与超时。
//!
//! | 通道 | 心跳间隔 | 单次超时 | 断连判定 | 恢复策略 |
//! |---|---|---|---|---|
//! | BLE | 30s PING | 30s 无 PONG | → RECONNECTING | 指数退避 1..32s × 6 → DISCOVER |
//! | TCP | 10s PING | 10s 无 PONG | → RECONNECTING | 同上 |

use std::time::Duration;

use linkx_protocol::tlv_codec;
use linkx_protocol::{TAG_PING, TAG_PONG};

/// 心跳会话参数（按通道取 PING 间隔与超时）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HeartbeatSpec {
    pub ping_interval: Duration,
    pub timeout: Duration,
}

impl HeartbeatSpec {
    pub const BLE: Self = Self {
        ping_interval: Duration::from_secs(30),
        timeout: Duration::from_secs(30),
    };
    pub const TCP: Self = Self {
        ping_interval: Duration::from_secs(10),
        timeout: Duration::from_secs(10),
    };
}

/// 指数退避序列 1/2/4/8/16/32s，最多 6 次，全部失败 → DISCOVER
#[derive(Debug, Default)]
pub struct Backoff {
    attempt: u8,
}

impl Backoff {
    pub const MAX_ATTEMPTS: u8 = 6;

    /// 下一次退避时长；返回 None 表示已耗尽（→ DISCOVER）
    pub fn next_delay(&mut self) -> Option<Duration> {
        if self.attempt >= Self::MAX_ATTEMPTS {
            return None;
        }
        let secs = 1u64 << self.attempt; // 1,2,4,8,16,32
        self.attempt += 1;
        Some(Duration::from_secs(secs))
    }

    pub fn reset(&mut self) {
        self.attempt = 0;
    }

    pub fn exhausted(&self) -> bool {
        self.attempt >= Self::MAX_ATTEMPTS
    }
}

/// PING/PONG payload（复用 HEARTBEAT 消息类型，由 TLV 区分）
pub fn ping_payload() -> Vec<u8> {
    tlv_codec::simple(TAG_PING, 0u8.to_be_bytes())
}

pub fn pong_payload() -> Vec<u8> {
    tlv_codec::simple(TAG_PONG, 0u8.to_be_bytes())
}

pub fn is_ping(payload: &[u8]) -> bool {
    tlv_codec::get(payload, TAG_PING).is_ok_and(|v| v.is_some())
}

pub fn is_pong(payload: &[u8]) -> bool {
    tlv_codec::get(payload, TAG_PONG).is_ok_and(|v| v.is_some())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_sequence_matches_spec() {
        let mut b = Backoff::default();
        let mut got = Vec::new();
        while let Some(d) = b.next_delay() {
            got.push(d);
        }
        assert_eq!(
            got,
            vec![
                Duration::from_secs(1),
                Duration::from_secs(2),
                Duration::from_secs(4),
                Duration::from_secs(8),
                Duration::from_secs(16),
                Duration::from_secs(32),
            ]
        );
        assert!(b.exhausted());
        b.reset();
        assert!(!b.exhausted());
        assert_eq!(b.next_delay(), Some(Duration::from_secs(1)));
    }

    #[test]
    fn ping_pong_payload_roundtrip() {
        assert!(is_ping(&ping_payload()));
        assert!(!is_pong(&ping_payload()));
        assert!(is_pong(&pong_payload()));
        assert!(!is_ping(&pong_payload()));
        assert!(!is_ping(&[0x99, 0x01]));
    }
}
