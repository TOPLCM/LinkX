//! 桥接层 TX 事件队列
//!
//! Core（Rust 异步）→ 事件队列（mpsc）→ UI pump 线程 poll → dispatch。
//! 不用回调/响应式库：跨 FFI 引用计数难管；单向流 + 批量分派更安全。

use std::sync::mpsc::{channel, Receiver, Sender};

use debuglog::Level;

/// Core → UI 事件
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TxEvent {
    ConnectionStateChanged {
        peer_id: Vec<u8>,
        state: u8,
    },
    MessageReceived {
        peer_id: Vec<u8>,
        msg_type: u8,
        seq: u32,
        payload: Vec<u8>,
    },
    ErrorReported {
        code: i32,
        context: Option<String>,
    },
    SessionKeyRotated {
        rotated_at_ms: i64,
    },
    SessionTerminated {
        reason: i32,
    },
}

impl TxEvent {
    /// 事件类型序号（日志用；payload 不入日志）
    pub fn kind(&self) -> u8 {
        match self {
            TxEvent::ConnectionStateChanged { .. } => 1,
            TxEvent::MessageReceived { .. } => 2,
            TxEvent::ErrorReported { .. } => 3,
            TxEvent::SessionKeyRotated { .. } => 4,
            TxEvent::SessionTerminated { .. } => 5,
        }
    }
}

/// 事件队列（一对多非必要：单 UI pump 消费者）
pub struct TxQueue {
    pub sender: Sender<TxEvent>,
    receiver: Receiver<TxEvent>,
}

impl Default for TxQueue {
    fn default() -> Self {
        Self::new()
    }
}

impl TxQueue {
    pub fn new() -> Self {
        let (sender, receiver) = channel();
        Self { sender, receiver }
    }

    /// 非阻塞取事件（UI pump 主循环调用）
    pub fn poll(&self) -> Option<TxEvent> {
        self.receiver.try_recv().ok()
    }

    /// 阻塞取事件（有超时；供调试/测试）
    pub fn recv_timeout(&self, ms: u64) -> Option<TxEvent> {
        use std::time::Duration;
        self.receiver.recv_timeout(Duration::from_millis(ms)).ok()
    }

    pub fn emit(&self, ev: TxEvent) -> bool {
        let kind = ev.kind();
        let ok = self.sender.send(ev).is_ok();
        // 埋点：TX 队列入队（只记事件类型与结果，不记 payload 内容）
        debuglog::log!(
            Level::Info,
            "ffi",
            "tx.emit",
            &[
                ("kind", &kind.to_string()),
                ("ok", if ok { "true" } else { "false" }),
            ]
        );
        ok
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queue_poll_and_emit() {
        let q = TxQueue::new();
        assert!(q.poll().is_none());
        assert!(q.emit(TxEvent::ErrorReported {
            code: -213,
            context: Some("tofu".into())
        }));
        let ev = q.poll().unwrap();
        assert_eq!(
            ev,
            TxEvent::ErrorReported {
                code: -213,
                context: Some("tofu".into())
            }
        );
        assert!(q.poll().is_none());
    }
}
