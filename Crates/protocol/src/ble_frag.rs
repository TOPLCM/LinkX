//! BLE 分片重组：分片头 6B = `msg_id u16 | frag_idx u16 | frag_cnt u16`（大端）；
//! MTU=23 时单包有效载荷 20B（ATT 头 3B 开销），每片净流载荷 14B。
//! 重组按 msg_id 缓冲，超时 5s、单消息上限 1MB。
//!
//! 片长是**发送方**的事：两侧各按自己协商到的 MTU（再按 [`MAX_FRAG_PACKET_LEN`] 收敛）
//! 切片；接收侧只按 ATT 规范上限判超长，见 [`max_frag_payload`]。

use std::collections::HashMap;
use std::time::{Duration, Instant};

use thiserror::Error;

use crate::frame::{FrameHeader, AEAD_TAG_LEN, FRAME_HEADER_LEN};

pub const FRAG_HEADER_LEN: usize = 6;
pub const ATT_OVERHEAD: usize = 3;
pub const MTU_DEFAULT: usize = 23;
/// ATT 规范允许的最大 MTU（BLE 4.1+ 的 517B）
pub const MAX_ATT_MTU: usize = 517;
/// 单个分片包（含 6B 分片头）的**线上字节上限**。真机实测：ATT 协商 517（规范推算可写
/// 514B）时 514B 的写只交付 512B，多余字节被**静默截断**——截断比丢包更危险：`frag_cnt`
/// 仍自洽、组满即"成功"，拼出的错误字节流被 AEAD 拒掉，表现为配对反复超时。发送侧按本上限收敛。
pub const MAX_FRAG_PACKET_LEN: usize = 512;
/// 重组超时
pub const REASSEMBLE_TIMEOUT: Duration = Duration::from_secs(5);
/// 单消息上限（防 DoS 与内存膨胀）
pub const MAX_BLE_MESSAGE: usize = 1024 * 1024;
/// 并发未完成分组上限：单组有 1MB 上限，但并发组数不设限的话，攻击者可在超时
/// 窗口内灌入大量分组 → 内存 DoS。超限时淘汰最旧分组。
pub const MAX_CONCURRENT_GROUPS: usize = 32;

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum BleFragError {
    #[error("分片包不足 {0} 字节")]
    PacketTooShort(usize),
    #[error("分片索引 {idx} 越界（cnt={cnt}）")]
    IndexOutOfRange { idx: u16, cnt: u16 },
    #[error("分组声明大小超上限（cnt={cnt} per={per}）")]
    GroupTooLarge { cnt: u16, per: u16 },
    #[error("重复分片 {0}")]
    Duplicate(u16),
    #[error("分组计数不一致（收到 {idx} 但 cnt={cnt}）")]
    CountMismatch { idx: u16, cnt: u16 },
    #[error("重组完成但帧头校验失败: {0}")]
    BadHeader(String),
    #[error("重组长度与帧头不符（header={header} 实际={actual}）")]
    LengthMismatch { header: usize, actual: usize },
    #[error("MTU 过小，单包载荷为 0（mtu={0}）")]
    MtuTooSmall(usize),
    #[error("分片数超 u16 上限（{0} 片）")]
    TooManyFragments(usize),
}

/// 给定 MTU 时一个分片可承载的"帧流"字节数：先按 [`MAX_FRAG_PACKET_LEN`] 截断再扣分片头
pub const fn stream_per_packet(mtu: usize) -> usize {
    let wire = {
        let raw = mtu.saturating_sub(ATT_OVERHEAD);
        if raw > MAX_FRAG_PACKET_LEN {
            MAX_FRAG_PACKET_LEN
        } else {
            raw
        }
    };
    wire.saturating_sub(FRAG_HEADER_LEN)
}

/// 一个分片**允许携带的最大帧流字节数**（按 ATT 规范上限 517 推导 = 508）。收侧必须
/// 用它、而不是本端 MTU 推导值判「分片是否超长」：片长由发送方链路决定，与本端协商值
/// 无关；按本端值拒收会在两侧 MTU 不一致时把对端合法分片全量静默丢弃。
pub const fn max_frag_payload() -> usize {
    stream_per_packet(MAX_ATT_MTU)
}

/// 把整帧（13B 帧头 + payload + 16B tag）切成 BLE 分片包（每包 <= mtu）。
/// 不可信 MTU / 超大帧返回 `Err` 而非 panic（全 profile `panic=abort`，不能用 `assert!`）。
pub fn split_into_packets(
    msg_id: u16,
    mtu: usize,
    full_frame: &[u8],
) -> Result<Vec<Vec<u8>>, BleFragError> {
    let per = stream_per_packet(mtu);
    if per == 0 {
        return Err(BleFragError::MtuTooSmall(mtu));
    }
    let cnt = full_frame.len().div_ceil(per);
    if cnt > u16::MAX as usize {
        return Err(BleFragError::TooManyFragments(cnt));
    }

    let mut packets = Vec::with_capacity(cnt);
    for idx in 0..cnt {
        let start = idx * per;
        let end = (start + per).min(full_frame.len());
        let mut pkt = Vec::with_capacity(FRAG_HEADER_LEN + (end - start));
        pkt.extend_from_slice(&msg_id.to_be_bytes());
        pkt.extend_from_slice(&(idx as u16).to_be_bytes());
        pkt.extend_from_slice(&(cnt as u16).to_be_bytes());
        pkt.extend_from_slice(&full_frame[start..end]);
        packets.push(pkt);
    }
    Ok(packets)
}

struct Segment {
    count: u16,
    got: u16,
    /// 已入槽字节数：累积**过程中**就要守住 1MB 上限，不能等组完再算（防内存 DoS）
    got_bytes: usize,
    slots: Vec<Option<Vec<u8>>>,
    /// 最后一次"有分片进槽"的时刻。它**必须**随进展刷新：一条 256 KiB 消息在 MTU 517 下是
    /// 518 个分片，蓝牙单片在途按 30–50 ms 算要 15–25 s，若期限从建组那刻算起（绝对期限），
    /// 正在顺利传输的组会被自己的接收器半路清掉，剩下的分片再重建一个永远组不满的组 ——
    /// 症状是"大帧静默消失"，而每一片单独看都是合法的。
    progress_at: Instant,
}

/// 分组声明总长的硬上限（含帧头与 tag 的余量）
const SEGMENT_CAP: i128 = (MAX_BLE_MESSAGE + FRAME_HEADER_LEN + AEAD_TAG_LEN) as i128;

impl Segment {
    fn new(count: u16, now: Instant) -> Result<Self, BleFragError> {
        // 防 DoS：按规范最大片长估算的声明总长超 1MB 直接拒（与发送方实际 MTU 无关）。
        // 这条估算的真实作用是**给槽位表封顶**（`count ≤ 上限/最大片长 ≈ 2073` 个槽，
        // 每组约 49 KB）；代价是"恰好顶到 1 MiB"的消息会被首片拒掉——实际最大消息是 256 KiB
        // 的文件分块，落在余量里，所以不动它。
        let calc = i128::from(count) * max_frag_payload() as i128;
        if calc > SEGMENT_CAP {
            return Err(BleFragError::GroupTooLarge {
                cnt: count,
                per: max_frag_payload() as u16,
            });
        }
        Ok(Self {
            count,
            got: 0,
            got_bytes: 0,
            slots: vec![None; count as usize],
            progress_at: now,
        })
    }
}

/// BLE 分组重组器（线程不安全，由单通道任务持有；不带 MTU 状态，见 [`max_frag_payload`]）
pub struct BleReassembler {
    segs: HashMap<u16, Segment>,
}

impl BleReassembler {
    pub fn new() -> Self {
        Self {
            segs: HashMap::new(),
        }
    }

    /// 送入一个分片包；组满时返回完整帧字节（帧头+payload+tag）。任意输入不得 panic。
    pub fn feed(&mut self, now: Instant, packet: &[u8]) -> Result<Option<Vec<u8>>, BleFragError> {
        if packet.len() < FRAG_HEADER_LEN {
            return Err(BleFragError::PacketTooShort(packet.len()));
        }
        let msg_id = u16::from_be_bytes([packet[0], packet[1]]);
        let idx = u16::from_be_bytes([packet[2], packet[3]]);
        let cnt = u16::from_be_bytes([packet[4], packet[5]]);
        if cnt == 0 || idx >= cnt {
            return Err(BleFragError::IndexOutOfRange { idx, cnt });
        }

        self.sweep(now);

        // 新分组先过 `Segment::new` 的校验，再动淘汰：一串声明超限的非法首片不该在被拒之前
        // 顺手把在途的合法分组挤掉
        if !self.segs.contains_key(&msg_id) {
            let seg = Segment::new(cnt, now)?;
            // 并发分组数达上限时淘汰最旧（原因见 MAX_CONCURRENT_GROUPS 注释）
            if self.segs.len() >= MAX_CONCURRENT_GROUPS {
                self.evict_oldest();
            }
            self.segs.insert(msg_id, seg);
        }
        let seg = self.segs.get_mut(&msg_id).expect("刚插入必然存在");
        if seg.count != cnt {
            self.segs.remove(&msg_id);
            return Err(BleFragError::CountMismatch { idx, cnt });
        }
        let chunk = &packet[FRAG_HEADER_LEN..];
        if chunk.len() > max_frag_payload() || seg.got_bytes + chunk.len() > SEGMENT_CAP as usize {
            // 超规范上限：视为恶意，整组丢弃（上限不能按本端 MTU 收紧，见 max_frag_payload）
            self.segs.remove(&msg_id);
            return Ok(None);
        }

        if seg.slots[idx as usize].is_some() {
            return Err(BleFragError::Duplicate(idx));
        }
        seg.slots[idx as usize] = Some(chunk.to_vec());
        seg.got += 1;
        seg.got_bytes += chunk.len();
        // 有进展就续期：`progress_at` 同时是超时判据与淘汰判据（见字段注释）
        seg.progress_at = now;

        if seg.got < seg.count {
            return Ok(None);
        }
        // 组完：按序拼接。总长上限已在入槽时按 `got_bytes` 守住，这里无须再判一次。
        let mut frame = Vec::with_capacity(seg.got_bytes);
        for slot in seg.slots.iter() {
            let part = slot.as_ref().expect("got==count 保证全槽有值");
            frame.extend_from_slice(part);
        }
        self.segs.remove(&msg_id);

        let header =
            FrameHeader::parse(&frame).map_err(|e| BleFragError::BadHeader(e.to_string()))?;
        let expect = header.full_frame_len();
        if frame.len() != expect {
            return Err(BleFragError::LengthMismatch {
                header: expect,
                actual: frame.len(),
            });
        }
        Ok(Some(frame))
    }

    /// 清掉「多久没有进展」超时的分组。用 `checked_duration_since`：调用方送进来的 `now`
    /// 早于某组的记录时刻（同一批分片复用旧时间戳）时 `duration_since` 会 panic，而全 profile
    /// `panic = "abort"` ⇒ 一次时间戳回退就能把宿主进程打死。这种输入按"刚有进展"处理。
    pub fn sweep(&mut self, now: Instant) {
        self.segs.retain(|_, seg| {
            now.checked_duration_since(seg.progress_at)
                .is_none_or(|idle| idle < REASSEMBLE_TIMEOUT)
        });
    }

    /// 淘汰「最久没进展」的分组（并发数到达上限时调用）。正在传的那组每片都刷新进展，所以
    /// 被踢的一定是 stalled 的那组；按建组时刻排会反过来把在传的大帧踢掉，等于奖励后进者。
    fn evict_oldest(&mut self) {
        if let Some(idle) = self
            .segs
            .iter()
            .min_by_key(|(_, seg)| seg.progress_at)
            .map(|(id, _)| *id)
        {
            self.segs.remove(&idle);
        }
    }

    pub fn pending_count(&self) -> usize {
        self.segs.len()
    }
}

impl Default for BleReassembler {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame;
    use crate::msg_type;

    fn make_frame(payload: &[u8], seq: u32, mt: u8) -> Vec<u8> {
        let h = frame::FrameHeader::default_encrypted(mt, seq, payload.len() as u32);
        let mut body = payload.to_vec();
        body.extend_from_slice(&[0xCD; 16]);
        frame::assemble_frame(&h, &body).expect("测试帧组装")
    }

    #[test]
    fn budget_matches_spec() {
        let per = stream_per_packet(MTU_DEFAULT);
        assert_eq!(per, 14);
        let frame = make_frame(&[0x11; 1], 0, msg_type::CLIPBOARD_PUSH);
        assert_eq!(frame.len(), 30);
        let pkts = split_into_packets(0x42, MTU_DEFAULT, &frame).unwrap();
        assert_eq!(pkts.len(), 3); // 14+14+2
        assert_eq!(pkts[0].len(), FRAG_HEADER_LEN + 13 + 1); // 帧头首片
        assert_eq!(pkts[1].len(), FRAG_HEADER_LEN + 14); // 中间片
        assert_eq!(pkts[2].len(), FRAG_HEADER_LEN + 2); // 末片 2B + tag
        let last_body = &pkts[2][FRAG_HEADER_LEN..];
        let frame_tail = &frame[frame.len() - last_body.len()..];
        assert_eq!(last_body, frame_tail);
    }

    #[test]
    fn roundtrip_1kb() {
        let payload = vec![0x5A; 1024];
        let frame = make_frame(&payload, 99, msg_type::NOTIFY_PUSH);
        let pkts = split_into_packets(7, MTU_DEFAULT, &frame).unwrap();
        let expected = frame.len().div_ceil(stream_per_packet(MTU_DEFAULT));
        assert_eq!(pkts.len(), expected);

        let mut r = BleReassembler::new();
        let t0 = Instant::now();
        for p in &pkts {
            let out = r.feed(t0, p).unwrap();
            if p == pkts.last().unwrap() {
                let full = out.expect("组完后应返回整帧");
                assert_eq!(full, frame);
            } else {
                assert!(out.is_none());
            }
        }
        assert_eq!(r.pending_count(), 0);
    }

    #[test]
    fn out_of_order_and_interleaved() {
        let mut r = BleReassembler::new();
        let t0 = Instant::now();
        let f1 = make_frame(&[0x01; 200], 1, msg_type::CLIPBOARD_PUSH);
        let f2 = make_frame(&[0x02; 200], 2, msg_type::NOTIFY_PUSH);
        let p1 = split_into_packets(0x10, MTU_DEFAULT, &f1).unwrap();
        let p2 = split_into_packets(0x20, MTU_DEFAULT, &f2).unwrap();

        let done = [false, false];
        for (packets, idx) in [(&p2, 1usize), (&p1, 0usize), (&p1, 0usize), (&p2, 1usize)] {
            let p = &packets[0];
            let got = r.feed(t0, p);
            if done[idx] {
                assert!(got.is_ok());
            }
        }
        let mut r2 = BleReassembler::new();
        let mut seq = Vec::new();
        for i in 0..p1.len() {
            seq.push((0x10, i));
            if i < p2.len() {
                seq.push((0x20, i));
            }
        }
        for (id, i) in seq {
            let pkt = if id == 0x10 { &p1[i] } else { &p2[i] };
            let out = r2.feed(t0, pkt).unwrap();
            let is_last_of_group = i == {
                if id == 0x10 {
                    p1.len() - 1
                } else {
                    p2.len() - 1
                }
            };
            if is_last_of_group {
                assert!(out.is_some(), "id={id} 应组完");
            }
        }
        assert_eq!(r2.pending_count(), 0);
    }

    #[test]
    fn timeout_expiry() {
        let frame = make_frame(&[0x01; 100], 1, msg_type::HELLO);
        let pkts = split_into_packets(0x77, MTU_DEFAULT, &frame).unwrap();
        let mut r = BleReassembler::new();
        r.feed(Instant::now(), &pkts[0]).unwrap();
        assert_eq!(r.pending_count(), 1);
        let later = Instant::now() + REASSEMBLE_TIMEOUT + Duration::from_millis(1);
        r.sweep(later);
        assert_eq!(r.pending_count(), 0);
        for p in &pkts {
            let out = r.feed(later, p).unwrap();
            if p == pkts.last().unwrap() {
                assert_eq!(out.unwrap(), frame);
            }
        }
    }

    /// 期限判的是"多久没进展"，不是"从建组起总共过了多久"：一条要传十几秒的大帧必须能活着
    /// 传完。按建组时刻算绝对期限时，256 KiB 分块在 MTU 517 下是 518 片，蓝牙单片在途就要
    /// 15–25 s ⇒ 组会在顺利传输的中途被自己的接收器清掉，症状是"大帧静默消失"。
    #[test]
    fn timeout_renews_on_every_progress() {
        let frame = make_frame(&[0x02; 400], 1, msg_type::HELLO);
        let pkts = split_into_packets(0x77, MTU_DEFAULT, &frame).unwrap();
        assert!(
            pkts.len() > 2,
            "这条用例要跨过多于一个期限长度，分片太少没意义"
        );
        let mut r = BleReassembler::new();
        let mut t = Instant::now();
        let step = REASSEMBLE_TIMEOUT / 2; // 片间隔 2.5s：任何时刻都没"闲"满 5s
        let mut out = None;
        for p in &pkts {
            t += step;
            if let Some(f) = r.feed(t, p).unwrap() {
                out = Some(f);
            }
        }
        assert_eq!(
            out.expect("分片全到齐却没组出帧（总耗时超过绝对期限就会这样）"),
            frame
        );
        // 对照：停手超过期限，组必须被清掉——续期不等于永不过期
        t += REASSEMBLE_TIMEOUT + Duration::from_millis(1);
        r.sweep(t);
        assert_eq!(r.pending_count(), 0, "无进展超时后还留着组");
    }

    #[test]
    fn oversized_group_rejected() {
        // 估算基准是规范上限而非本端 MTU，这条防线与两侧协商到什么 MTU 无关
        let mut pkt = Vec::new();
        pkt.extend_from_slice(&0x0001u16.to_be_bytes()); // msg_id
        pkt.extend_from_slice(&0x0000u16.to_be_bytes()); // idx=0
        pkt.extend_from_slice(&0xFFFFu16.to_be_bytes()); // cnt=65535
        pkt.extend_from_slice(&[0u8; 8183]);
        let mut r = BleReassembler::new();
        assert!(matches!(
            r.feed(Instant::now(), &pkt),
            Err(BleFragError::GroupTooLarge { .. })
        ));
    }

    /// 收侧分片长度上限来自 ATT 规范而非本端 MTU：MTU 不一致（23 vs 517）时对端大片必须原样重组
    #[test]
    fn large_fragment_from_bigger_mtu_peer_is_reassembled() {
        let frame = make_frame(&[0x42; 600], 0, msg_type::CLIPBOARD_PUSH);
        let pkts = split_into_packets(7, MAX_ATT_MTU, &frame).unwrap();
        assert!(pkts.len() >= 2, "600B 载荷应切成多片");
        assert!(
            pkts.iter()
                .any(|p| p.len() - FRAG_HEADER_LEN > stream_per_packet(MTU_DEFAULT)),
            "应存在超过 14B 净流的大片，才能验证收侧容忍度"
        );

        let mut r = BleReassembler::new();
        let mut got = None;
        for p in &pkts {
            if let Some(f) = r.feed(Instant::now(), p).unwrap() {
                got = Some(f);
            }
        }
        assert_eq!(got.as_deref(), Some(frame.as_slice()), "大片必须原样重组");
    }

    #[test]
    fn chunk_beyond_spec_ceiling_is_dropped() {
        let mut pkt = vec![0u8; FRAG_HEADER_LEN];
        pkt[0..2].copy_from_slice(&9u16.to_be_bytes()); // msg_id
        pkt[2..4].copy_from_slice(&0u16.to_be_bytes()); // idx
        pkt[4..6].copy_from_slice(&1u16.to_be_bytes()); // cnt=1
        pkt.extend_from_slice(&[0u8; max_frag_payload() + 1]);
        let mut r = BleReassembler::new();
        assert!(
            r.feed(Instant::now(), &pkt).unwrap().is_none(),
            "超规范上限的分片不得被接受"
        );
    }

    /// MTU 517 时**任何**分片包都不得超过 [`MAX_FRAG_PACKET_LEN`]（真机静默截断的教训）
    #[test]
    fn packet_never_exceeds_wire_cap_at_max_mtu() {
        for body_len in [0usize, 1, 200, 500, 505, 506, 507, 1000, 4096] {
            let frame = make_frame(&vec![0x5A; body_len], 0, msg_type::CLIPBOARD_PUSH);
            let pkts = split_into_packets(1, MAX_ATT_MTU, &frame).unwrap();
            for p in &pkts {
                assert!(
                    p.len() <= MAX_FRAG_PACKET_LEN,
                    "MTU={MAX_ATT_MTU} body={body_len} 切出 {}B 的包，超过线上上限 {MAX_FRAG_PACKET_LEN}",
                    p.len()
                );
            }
            let mut r = BleReassembler::new();
            let mut got = None;
            for p in &pkts {
                if let Some(f) = r.feed(Instant::now(), p).unwrap() {
                    got = Some(f);
                }
            }
            assert_eq!(
                got.as_deref(),
                Some(frame.as_slice()),
                "body={body_len} 往返不一致"
            );
        }
        assert_eq!(stream_per_packet(MTU_DEFAULT), 14);
    }

    #[test]
    fn malformed_no_panic_smoke() {
        let mut r = BleReassembler::new();
        let mut seed = 0x12345678u32;
        for _ in 0..20_000 {
            seed = seed.wrapping_mul(1103515245).wrapping_add(12345);
            let len = (seed % 40) as usize;
            let bytes: Vec<u8> = (0..len).map(|i| ((seed >> (i % 8)) & 0xFF) as u8).collect();
            let _ = r.feed(Instant::now(), &bytes);
        }
    }

    #[test]
    fn split_returns_error_not_panic() {
        let frame = make_frame(&[0x11; 16], 0, msg_type::CLIPBOARD_PUSH);
        // MTU <= 分片头+ATT 开销 → 单包载荷为 0 → Err
        assert!(matches!(
            split_into_packets(1, FRAG_HEADER_LEN + ATT_OVERHEAD, &frame),
            Err(BleFragError::MtuTooSmall(_))
        ));
        assert!(matches!(
            split_into_packets(1, 1, &frame),
            Err(BleFragError::MtuTooSmall(_))
        ));
        assert!(split_into_packets(1, MTU_DEFAULT, &frame).is_ok());
    }

    #[test]
    fn concurrent_group_budget_enforced() {
        let t0 = Instant::now();
        let mut r = BleReassembler::new();
        // 灌入远超上限的「半截分组」（每个 msg_id 只喂首片，永不组完）
        for id in 0..(MAX_CONCURRENT_GROUPS as u16 * 4) {
            let frame = make_frame(&[0x01; 100], 0, msg_type::HELLO);
            let pkts = split_into_packets(id, MTU_DEFAULT, &frame).unwrap();
            let _ = r.feed(t0, &pkts[0]); // 只喂首片
        }
        assert!(
            r.pending_count() <= MAX_CONCURRENT_GROUPS,
            "并发分组数必须被上限约束（实际 {}）",
            r.pending_count()
        );
    }

    /// 淘汰只许发生在"这条分组本身合法"之后：一串声明超限的首片不该顺手挤掉在途分组。
    #[test]
    fn invalid_first_fragment_does_not_evict_inflight_groups() {
        let t0 = Instant::now();
        let mut r = BleReassembler::new();
        for id in 0..MAX_CONCURRENT_GROUPS as u16 {
            let frame = make_frame(&[0x01; 100], 0, msg_type::HELLO);
            let pkts = split_into_packets(id, MTU_DEFAULT, &frame).unwrap();
            r.feed(t0, &pkts[0]).expect("合法首片应被收下");
        }
        assert_eq!(r.pending_count(), MAX_CONCURRENT_GROUPS, "前提：表已灌满");

        // 非法首片：cnt 大到声明总长超过单消息上限
        let mut bad = [0u8; FRAG_HEADER_LEN];
        bad[0..2].copy_from_slice(&999u16.to_be_bytes());
        bad[4..6].copy_from_slice(&u16::MAX.to_be_bytes());
        assert!(matches!(
            r.feed(t0, &bad),
            Err(BleFragError::GroupTooLarge { .. })
        ));
        assert_eq!(
            r.pending_count(),
            MAX_CONCURRENT_GROUPS,
            "被拒绝的非法分组不得挤掉任何一个在途分组"
        );
    }
}
