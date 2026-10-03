//! 会话状态机。
//! ```text
//! DISCOVER → HANDSHAKE → PAIRING → SAS_COMPARE → PAIRED
//!                │          ▲           │            │
//!                ▼          │(接受)      ▼            ▼
//!           REPAIRED ───────┘        REJECTED   RECONNECTING ──6 次退避失败──→ DISCOVER
//! ```
//! REPAIRED（TOFU 指纹变化）接受后回 **PAIRING** 重走 SAS 人工比对，不直接进 PAIRED。

use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionState {
    Discover,     // 广播+扫描，等待互见
    Handshake,    // Noise XX 全流程
    Pairing,      // 新对端：交换长期身份公钥（握手已隐含）并互换 PAIR_CONFIRM(SAS)
    SasCompare,   // 6 位 SAS 人工比对
    Paired,       // 主态
    Repaired,     // TOFU 指纹变化 → -213 警告，等待用户决定
    Reconnecting, // 心跳超时后的指数退避重连
    Closed,       // 主动断开 / 6 次退避失败 / TOFU 拒绝
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TofuVerdict {
    /// 首次配对，无本地指纹 → 走 PAIRING/SAS_COMPARE
    NewPeer,
    /// 指纹匹配 → 直接 PAIRED，不再比 SAS
    Matched,
    /// 指纹不匹配 → REPAIRED（-213 警告，用户决定接受/拒绝）
    Mismatch,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionEvent {
    PeerDiscovered,
    HandshakeComplete { tofu: TofuVerdict },
    SasConfirmed,          // 双方 PAIR_CONFIRM(SAS) 一致
    SasRejected,           // 比对外部显示不一致 / 用户拒绝
    FingerprintReAccepted, // 用户在 REPAIRED 中接受新指纹
    HeartbeatTimeout,
    ReconnectSucceeded,
    ReconnectExhausted, // 6 次退避全部失败 → DISCOVER
    UserDisconnect,
    ProcessKilled,
}

/// 通道（决定心跳参数）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionChannel {
    Ble,
    Tcp,
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum StateError {
    #[error("非法迁移 {from:?} --{event:?}--> 无目标")]
    IllegalTransition {
        from: SessionState,
        event: SessionEvent,
    },
}

/// 会话状态机（纯迁移表；载荷状态由调用方持有）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionManager {
    pub state: SessionState,
    pub channel: SessionChannel,
}

impl Default for SessionManager {
    fn default() -> Self {
        Self {
            state: SessionState::Discover,
            channel: SessionChannel::Ble,
        }
    }
}

impl SessionManager {
    pub fn new(channel: SessionChannel) -> Self {
        Self {
            state: SessionState::Discover,
            channel,
        }
    }

    /// 心跳超时 → RECONNECTING；退避计数由 [`crate::heartbeat::Backoff`] 持有，状态机只管迁移。
    pub fn is_paired(&self) -> bool {
        self.state == SessionState::Paired
    }

    pub fn transition(&mut self, ev: SessionEvent) -> Result<SessionState, StateError> {
        let from = self.state;
        let to = Self::next(from, ev)?;
        self.state = to;
        Ok(to)
    }

    fn next(from: SessionState, ev: SessionEvent) -> Result<SessionState, StateError> {
        use SessionEvent::*;
        use SessionState::*;
        let to = match (from, ev) {
            (Discover, PeerDiscovered) => Handshake,
            (Discover, UserDisconnect | ProcessKilled) => Closed,
            (Closed, PeerDiscovered) => Handshake, // 断开后重新发现可直接握手

            (
                Handshake,
                HandshakeComplete {
                    tofu: TofuVerdict::Matched,
                },
            ) => Paired,
            (
                Handshake,
                HandshakeComplete {
                    tofu: TofuVerdict::NewPeer,
                },
            ) => Pairing,
            (
                Handshake,
                HandshakeComplete {
                    tofu: TofuVerdict::Mismatch,
                },
            ) => Repaired,
            (Handshake, HeartbeatTimeout) => Reconnecting, // 握手期间通道丢失
            (Handshake, UserDisconnect | ProcessKilled) => Closed,

            (Pairing, SasConfirmed) => SasCompare,
            (Pairing, SasRejected) => Closed,
            // 配对阶段通道丢失也要进重连退避，不得静默卡死
            (Pairing, HeartbeatTimeout) => Reconnecting,
            (Pairing, UserDisconnect | ProcessKilled) => Closed,

            (SasCompare, SasConfirmed) => Paired,
            (SasCompare, SasRejected) => Closed,
            (SasCompare, HeartbeatTimeout) => Reconnecting, // 同上：通道丢失防卡死
            (SasCompare, UserDisconnect | ProcessKilled) => Closed,

            // ---- REPAIRED（同名设备新身份，-213）：接受 → 重走配对+SAS；拒绝 → CLOSED ----
            // 接受新身份只把状态推回 Pairing，SAS 人工比对必须重走一遍。
            (Repaired, FingerprintReAccepted) => Pairing,
            (Repaired, SasRejected) => Closed,
            (Repaired, HeartbeatTimeout) => Reconnecting,
            (Repaired, UserDisconnect | ProcessKilled) => Closed,

            (Paired, HeartbeatTimeout) => Reconnecting,
            (Paired, UserDisconnect | ProcessKilled) => Closed,

            (Reconnecting, ReconnectSucceeded) => Handshake,
            (Reconnecting, ReconnectExhausted) => Discover,
            (Reconnecting, HeartbeatTimeout) => Reconnecting, // 幂等
            (Reconnecting, UserDisconnect | ProcessKilled) => Closed,

            (Closed, UserDisconnect | ProcessKilled) => Closed, // 幂等

            _ => return Err(StateError::IllegalTransition { from, event: ev }),
        };
        Ok(to)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use SessionEvent::*;
    use SessionState::*;

    #[test]
    fn first_pair_full_flow() {
        let mut sm = SessionManager::new(SessionChannel::Ble);
        assert_eq!(sm.transition(PeerDiscovered).unwrap(), Handshake);
        assert_eq!(
            sm.transition(HandshakeComplete {
                tofu: TofuVerdict::NewPeer
            })
            .unwrap(),
            Pairing
        );
        assert_eq!(sm.transition(SasConfirmed).unwrap(), SasCompare);
        assert_eq!(sm.transition(SasConfirmed).unwrap(), Paired);
        assert!(sm.is_paired());
    }

    #[test]
    fn tofu_match_skips_pairing() {
        let mut sm = SessionManager::new(SessionChannel::Tcp);
        sm.transition(PeerDiscovered).unwrap();
        assert_eq!(
            sm.transition(HandshakeComplete {
                tofu: TofuVerdict::Matched
            })
            .unwrap(),
            Paired
        );
    }

    #[test]
    fn heartbeat_backoff_cycle() {
        let mut sm = SessionManager::new(SessionChannel::Ble);
        sm.transition(PeerDiscovered).unwrap();
        sm.transition(HandshakeComplete {
            tofu: TofuVerdict::Matched,
        })
        .unwrap();
        assert_eq!(sm.transition(HeartbeatTimeout).unwrap(), Reconnecting);
        assert_eq!(sm.transition(ReconnectSucceeded).unwrap(), Handshake);
        sm.transition(HandshakeComplete {
            tofu: TofuVerdict::Matched,
        })
        .unwrap();
        assert_eq!(sm.state, Paired);
        sm.transition(HeartbeatTimeout).unwrap();
        assert_eq!(sm.state, Reconnecting);
        assert_eq!(sm.transition(ReconnectExhausted).unwrap(), Discover);
    }

    #[test]
    fn tofu_mismatch_path() {
        let mut sm = SessionManager::new(SessionChannel::Ble);
        sm.transition(PeerDiscovered).unwrap();
        assert_eq!(
            sm.transition(HandshakeComplete {
                tofu: TofuVerdict::Mismatch
            })
            .unwrap(),
            Repaired
        );
        // 接受新身份 → 回 PAIRING（重走 SAS 复核），再经两次 SasConfirmed 才到 PAIRED
        assert_eq!(sm.transition(FingerprintReAccepted).unwrap(), Pairing);
        assert_eq!(sm.transition(SasConfirmed).unwrap(), SasCompare);
        assert_eq!(sm.transition(SasConfirmed).unwrap(), Paired);
    }

    #[test]
    fn pairing_timeout_enters_reconnect() {
        // 配对 / SAS 阶段通道丢失 → Reconnecting，不得静默卡死
        let mut sm = SessionManager::new(SessionChannel::Ble);
        sm.transition(PeerDiscovered).unwrap();
        sm.transition(HandshakeComplete {
            tofu: TofuVerdict::NewPeer,
        })
        .unwrap();
        assert_eq!(sm.transition(HeartbeatTimeout).unwrap(), Reconnecting);
    }

    #[test]
    fn illegal_changes_rejected() {
        let mut sm = SessionManager::new(SessionChannel::Ble);
        assert!(matches!(
            sm.transition(SasConfirmed),
            Err(StateError::IllegalTransition { .. })
        ));
        let mut sm2 = SessionManager::new(SessionChannel::Tcp);
        sm2.transition(PeerDiscovered).unwrap();
        sm2.transition(HandshakeComplete {
            tofu: TofuVerdict::Matched,
        })
        .unwrap();
        assert!(matches!(
            sm2.transition(FingerprintReAccepted),
            Err(StateError::IllegalTransition { .. })
        ));
    }

    #[test]
    fn reconnect_returns_to_handshake_and_can_repair() {
        let mut sm = SessionManager::new(SessionChannel::Ble);
        sm.transition(PeerDiscovered).unwrap();
        sm.transition(HandshakeComplete {
            tofu: TofuVerdict::Matched,
        })
        .unwrap();
        sm.transition(HeartbeatTimeout).unwrap();
        sm.transition(ReconnectSucceeded).unwrap();
        assert_eq!(sm.state, Handshake);
        sm.transition(HandshakeComplete {
            tofu: TofuVerdict::Matched,
        })
        .unwrap();
        assert_eq!(sm.state, Paired);
    }
}
