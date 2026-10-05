//! 抗重放 / 抗乱序：接收端滑动窗口——位图记录最近 64 个 seq，允许落后最高 seq 16 以内
//! 乱序，更旧或重复一律拒绝。为何必须靠窗口去重：nonce 由 seq 确定性派生
//! （`cipher::derive_nonce`），同一 seq 的密文逐字节一致，密码学本身区分不了「重放」与
//! 「新帧」。seq 取完整 u32 域（含 0），窗口以首个到达的 seq 为基准，不做人为下限。

use thiserror::Error;

/// 滑动窗口默认大小
pub const REPLAY_WINDOW_SIZE: u32 = 64;
/// 允许的最大乱序跨度
pub const REPLAY_OUT_OF_ORDER_TOLERANCE: u32 = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum ReplayError {
    #[error("重放：seq={0} 已被接收过")]
    Replay(u32),
    #[error("超出窗口：seq={seq} 过旧（当前最高 {highest}）")]
    TooOld { seq: u32, highest: u32 },
}

/// 单会话接收侧滑动窗口（线程不安全，由单条接收链路持有）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplayWindow {
    window_size: u32,
    tolerance: u32,
    /// 已接收过的最高 seq；`None` 表示尚未接收任何帧
    highest: Option<u32>,
    /// bit i 表示 seq = highest - i 是否已接收（i < 64）
    bitmap: u64,
}

impl Default for ReplayWindow {
    fn default() -> Self {
        Self::new(REPLAY_WINDOW_SIZE, REPLAY_OUT_OF_ORDER_TOLERANCE)
    }
}

impl ReplayWindow {
    /// `window_size ∈ 1..=64`（位图为 u64）；`tolerance` 会被裁剪到 `< window_size`。
    pub fn new(window_size: u32, tolerance: u32) -> Self {
        assert!((1..=64).contains(&window_size), "窗口大小须在 1..=64");
        Self {
            window_size,
            tolerance: tolerance.min(window_size - 1),
            highest: None,
            bitmap: 0,
        }
    }

    pub fn window_size(&self) -> u32 {
        self.window_size
    }

    pub fn tolerance(&self) -> u32 {
        self.tolerance
    }

    pub fn highest(&self) -> Option<u32> {
        self.highest
    }

    /// 该 seq 是否落在窗口内且已被接收（诊断用；不改变状态）
    pub fn is_seen(&self, seq: u32) -> bool {
        match self.highest {
            None => false,
            Some(h) if seq > h => false,
            Some(h) => {
                let delta = h - seq;
                // 位宽就是 window_size（构造时已保证 ≤64）：写死 64 会在窗口被调小的实例上
                // 去过位图里根本不存在的位，读到的"没见过"也就不再可信
                if delta >= self.window_size {
                    return false;
                }
                self.bitmap & (1u64 << delta) != 0
            }
        }
    }

    /// 校验一个 seq（**只读，不改变窗口状态**）。必须先「`check` → 解密（AEAD 认证）成功 →
    /// `commit`」：认证前就推窗，攻击者伪造任意 seq 即可把合法帧判为过旧丢弃（定向 DoS）。
    pub fn check(&self, seq: u32) -> Result<(), ReplayError> {
        let Some(highest) = self.highest else {
            return Ok(());
        };
        if seq > highest {
            return Ok(());
        }
        let delta = highest - seq;
        if delta > self.tolerance || delta >= self.window_size {
            return Err(ReplayError::TooOld { seq, highest });
        }
        if self.bitmap & (1u64 << delta) != 0 {
            return Err(ReplayError::Replay(seq));
        }
        Ok(())
    }

    /// 提交一个已通过 `check` 的 seq（登记到窗口）。
    pub fn commit(&mut self, seq: u32) {
        let Some(highest) = self.highest else {
            self.highest = Some(seq);
            self.bitmap = 1;
            return;
        };
        if seq > highest {
            let shift = seq - highest;
            if shift >= 64 || shift >= self.window_size {
                self.bitmap = 0;
            } else {
                self.bitmap <<= shift;
            }
            self.bitmap |= 1;
            self.highest = Some(seq);
            return;
        }
        let delta = highest - seq;
        if delta < 64 {
            self.bitmap |= 1u64 << delta;
        }
    }

    /// 校验并登记一个 seq：首次/前进/窗口内乱序 → `Ok`；重放或过旧 → `Err`。
    pub fn check_and_update(&mut self, seq: u32) -> Result<(), ReplayError> {
        self.check(seq)?;
        self.commit(seq);
        Ok(())
    }

    pub fn accept(&mut self, seq: u32) -> bool {
        self.check_and_update(seq).is_ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn in_order_accepted_and_replay_rejected() {
        let mut w = ReplayWindow::default();
        for seq in 1..=200u32 {
            assert_eq!(w.check_and_update(seq), Ok(()), "seq={seq} 应被接受");
        }
        assert_eq!(w.highest(), Some(200));
        assert_eq!(w.check_and_update(200), Err(ReplayError::Replay(200)));
        assert_eq!(w.check_and_update(199), Err(ReplayError::Replay(199)));
    }

    #[test]
    fn out_of_order_within_tolerance_accepted() {
        let mut w = ReplayWindow::default();
        assert_eq!(w.check_and_update(100), Ok(()));
        assert_eq!(w.check_and_update(84), Ok(()));
        assert!(w.is_seen(84));
        assert_eq!(w.check_and_update(84), Err(ReplayError::Replay(84)));
        assert_eq!(
            w.check_and_update(83),
            Err(ReplayError::TooOld {
                seq: 83,
                highest: 100
            })
        );
    }

    #[test]
    fn too_old_beyond_window_dropped() {
        let mut w = ReplayWindow::default();
        assert_eq!(w.check_and_update(1000), Ok(()));
        assert_eq!(
            w.check_and_update(935),
            Err(ReplayError::TooOld {
                seq: 935,
                highest: 1000
            })
        );
        assert_eq!(w.highest(), Some(1000));
        assert!(!w.is_seen(935));
    }

    #[test]
    fn forward_jump_resets_window() {
        let mut w = ReplayWindow::default();
        assert_eq!(w.check_and_update(5), Ok(()));
        assert_eq!(w.check_and_update(5_000), Ok(()));
        assert_eq!(w.highest(), Some(5_000));
        assert!(!w.is_seen(5));
        assert!(matches!(
            w.check_and_update(5),
            Err(ReplayError::TooOld { .. })
        ));
        assert_eq!(w.check_and_update(4_999), Ok(()));
    }

    #[test]
    fn seq_zero_is_legal_baseline() {
        let mut w = ReplayWindow::default();
        assert_eq!(w.check_and_update(0), Ok(()));
        assert_eq!(w.highest(), Some(0));
        assert!(w.is_seen(0));
        assert_eq!(w.check_and_update(0), Err(ReplayError::Replay(0)));
        assert_eq!(w.check_and_update(1), Ok(()));
    }

    #[test]
    fn tolerance_clamped_to_window() {
        let w = ReplayWindow::new(4, 99);
        assert_eq!(w.window_size(), 4);
        assert_eq!(w.tolerance(), 3);
    }

    #[test]
    fn fully_out_of_order_burst_settles() {
        let mut w = ReplayWindow::default();
        assert_eq!(w.check_and_update(10), Ok(()));
        for seq in 1..=9u32 {
            assert_eq!(w.check_and_update(seq), Ok(()), "seq={seq}");
        }
        for seq in 1..=10u32 {
            assert!(matches!(
                w.check_and_update(seq),
                Err(ReplayError::Replay(_))
            ));
        }
    }

    #[test]
    fn overflow_wraparound_forward_accepted() {
        let mut w = ReplayWindow::default();
        assert_eq!(w.check_and_update(u32::MAX - 1), Ok(()));
        assert_eq!(w.check_and_update(u32::MAX), Ok(()));
        assert_eq!(w.highest(), Some(u32::MAX));
        assert_eq!(
            w.check_and_update(u32::MAX),
            Err(ReplayError::Replay(u32::MAX))
        );
    }

    /// 未提交（模拟解密失败）的 check 不得推窗，后续合法帧仍被接受
    #[test]
    fn uncommitted_checks_do_not_advance_window() {
        let mut w = ReplayWindow::default();
        assert_eq!(w.check_and_update(100), Ok(()));
        assert_eq!(w.highest(), Some(100));
        assert_eq!(w.check(9_999), Ok(()));
        assert_eq!(w.highest(), Some(100), "未提交的 check 不得推窗");
        assert_eq!(w.check_and_update(101), Ok(()));
        assert_eq!(w.check_and_update(95), Ok(()));
    }
}
