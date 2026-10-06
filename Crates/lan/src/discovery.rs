//! 发现信标（HELLO 明文广播）+ UDP 承载。
//! - 信标 payload = TLV[TAG_ADVERT_NAME, TAG_OS, TAG_VERSION]（≤64B，TLV 上限内）
//! - 3s 广播一次；10s 未见信标 → 判定对端离线；手动 IP 兜底条目不超时、并作单播目标持续探测
//! - 驱动模型：非阻塞 socket + 显式 `now`（调用方定时驱动 `pump` / `broadcast_if_due`），
//!   超时判定不依赖真实休眠，故可单测、双端复用同一套时间语义。

use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
use std::time::{Duration, Instant};

use debuglog::Level;
use linkx_protocol::tlv_codec::{self, Tlv};
use linkx_protocol::{TAG_ADVERT_NAME, TAG_OS, TAG_VERSION};
use thiserror::Error;

/// UDP 发现端口（与 TCP `TRANSPORT_TCP_PORT` 同号，打包时一并加防火墙规则）
pub const DISCOVERY_UDP_PORT: u16 = 55676;
/// 有限广播地址（IPv4；V1 局域网发现仅做 IPv4）
pub const DISCOVERY_BROADCAST_ADDR: Ipv4Addr = Ipv4Addr::BROADCAST;
/// 信标广播间隔（3s，取最短通道节拍的同量级）
pub const DISCOVERY_BROADCAST_INTERVAL: Duration = Duration::from_secs(3);
/// 对端超时（10s ≈ 连续 3 次信标未达 → 离线；手动兜底条目不受此限制）
pub const DISCOVERY_PEER_TIMEOUT: Duration = Duration::from_secs(10);
/// 单个信标数据报上限（HELLO 属 TLV 模式 ≤64B，超长一律视为垃圾包）
pub const DISCOVERY_MAX_BEACON_LEN: usize = 64;

/// 由 IPv4 地址与前缀长度算子网的**定向广播**地址（192.168.1.24/24 → 192.168.1.255）。
///
/// 为什么需要它：`255.255.255.255` 是"有限广播"，Windows 只按路由表挑**一个**出口网卡发一次。
/// 装了 VMware / Docker / WSL 的机器上那个出口经常是虚拟网卡，信标就永远到不了手机所在的 WLAN
/// —— 而发送本身返回成功，所以"发没发出去"根本看不出来。定向广播的目的地址落在具体网段里，
/// 路由表会替每个网段各挑对网卡，这才是多网卡环境唯一可靠的发法。
///
/// `/0`（默认路由）的"定向广播"就是 `255.255.255.255`，那条本来就在发，留在这里只会让
/// `32 - prefix` 移出界；`/31` 与 `/32` 没有广播语义；回环、链路本地（APIPA 的 169.254/16）、
/// 组播与全零地址发出去也不会有人听，一律 `None`。
/// **私网段（192.168/16、10/8）必须留着** —— 那正是家庭与办公室局域网，
/// 用 `Ipv4Addr::is_global()` 过滤会把它一起滤掉。
pub fn directed_broadcast(ip: Ipv4Addr, prefix: u8) -> Option<Ipv4Addr> {
    if prefix == 0 || prefix >= 31 || ip.is_loopback() || ip.is_link_local() || ip.is_multicast() {
        return None;
    }
    // 到这里 prefix 一定在 1..=30，`32 - prefix` 不可能移出界
    let mask = u32::MAX << (32 - u32::from(prefix));
    let bcast = u32::from(ip) | !mask;
    let out = Ipv4Addr::from(bcast);
    (!out.is_unspecified() && !out.is_loopback()).then_some(out)
}

/// `pump` 的接收缓冲：必须装得下任意一个 IPv4 UDP 数据报——不是留余量，是跨平台正确性。
/// Windows 上 recv_from 缓冲小于数据报时**不会**像 Linux 那样静默截断投递，而是返回
/// WSAEMSGSIZE 并消费掉该包，于是本轮提前结束，**同一轮后续到达的正常信标一起被吞掉**。
pub const DISCOVERY_MAX_DATAGRAM_LEN: usize = 65_535;

/// 对端表条目上限（见 `PeerTable::push_capped`）：正常局域网里几十台设备已经是极限，
/// 这个数只为挡住"源地址无穷多"把表和网络扫描成本一起拖大。
pub const MAX_PEER_ENTRIES: usize = 64;

/// 这一轮信标要发往哪些地址：有限广播 + 本机各子网的定向广播 + 手动单播目标。
///
/// 单独成函数是因为多网卡这件事只能靠这张表判对错：发往 `255.255.255.255` 在装了 VMware 的
/// 机器上**照样返回成功**（只是从虚拟网卡出去了），所以"sent 计数"证明不了任何事。
/// 手动 IP 那批是单播、端口来自对端上报，既不能改端口也不能被去重吃掉。
fn beacon_targets(
    port: u16,
    subnet_targets: &[Ipv4Addr],
    extra_targets: &[SocketAddr],
) -> Vec<SocketAddr> {
    let mut addrs: Vec<SocketAddr> =
        vec![SocketAddr::new(IpAddr::V4(DISCOVERY_BROADCAST_ADDR), port)];
    for ip in subnet_targets {
        let t = SocketAddr::new(IpAddr::V4(*ip), port);
        if !addrs.contains(&t) {
            addrs.push(t);
        }
    }
    for t in extra_targets {
        if !addrs.contains(t) {
            addrs.push(*t);
        }
    }
    addrs
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveryBeacon {
    pub advert_name: String,
    pub os: u8,
    pub version: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum BeaconError {
    #[error("TLV 解析失败: {0}")]
    Tlv(String),
    #[error("缺少必需字段（advert_name/os/version）")]
    MissingField,
    #[error("{0}")]
    Decode(String),
}

impl DiscoveryBeacon {
    pub fn encode(&self) -> Result<Vec<u8>, BeaconError> {
        // 信标受 64B（TLV 上限）约束，较长设备名会让 encode 返回 TooLarge → 对端发现失败；
        // 这里按 TLV 开销（name 2 + os 3 + version 2 = 7B）在字符边界上主动截断，保证总能编出来。
        const BEACON_FIXED_OVERHEAD: usize = 2 + (2 + 1) + 2;
        const VERSION_MAX: usize = 16;
        let budget = DISCOVERY_MAX_BEACON_LEN.saturating_sub(BEACON_FIXED_OVERHEAD);
        let version = truncate_utf8(&self.version, budget.min(VERSION_MAX));
        let name = truncate_utf8(&self.advert_name, budget.saturating_sub(version.len()));

        tlv_codec::encode(&[
            Tlv::buf(TAG_ADVERT_NAME, name.as_bytes()),
            Tlv::u8(TAG_OS, self.os),
            Tlv::buf(TAG_VERSION, version.as_bytes()),
        ])
        .map_err(|e| BeaconError::Tlv(e.to_string()))
    }

    /// 解析（fuzz 目标：任意输入不 panic）
    pub fn decode(buf: &[u8]) -> Result<Self, BeaconError> {
        let items = tlv_codec::parse(buf).map_err(|e| BeaconError::Tlv(e.to_string()))?;
        let mut name = None;
        let mut os = None;
        let mut ver = None;
        for it in items {
            match it.tag {
                TAG_ADVERT_NAME => name = Some(String::from_utf8_lossy(&it.value).into_owned()),
                TAG_OS => os = Some(it.value.first().copied().unwrap_or(0)),
                TAG_VERSION => ver = Some(String::from_utf8_lossy(&it.value).into_owned()),
                _ => {}
            }
        }
        Ok(Self {
            advert_name: name.ok_or(BeaconError::MissingField)?,
            os: os.ok_or(BeaconError::MissingField)?,
            version: ver.ok_or(BeaconError::MissingField)?,
        })
    }
}

/// 在 UTF-8 字符边界上截断到不超过 `max_bytes` 字节
fn truncate_utf8(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum DiscoveryError {
    #[error("IO 失败: {0}")]
    Io(String),
    #[error("信标编码失败: {0}")]
    Beacon(BeaconError),
}

impl From<io::Error> for DiscoveryError {
    fn from(e: io::Error) -> Self {
        DiscoveryError::Io(e.to_string())
    }
}

impl From<BeaconError> for DiscoveryError {
    fn from(e: BeaconError) -> Self {
        DiscoveryError::Beacon(e)
    }
}

/// 已发现对端（按 IP 去重：同一设备可能经多张网卡被看到）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerEntry {
    pub addr: SocketAddr,
    /// 最近一次信标内容；手动兜底条目在收到首个信标前为 `None`
    pub beacon: Option<DiscoveryBeacon>,
    /// 用户手动添加（搜不到时的兜底入口）；此类条目不随超时清理
    pub manual: bool,
    pub last_seen: Instant,
    pub updates: u64,
}

#[derive(Debug, Clone)]
pub struct PeerTable {
    timeout: Duration,
    peers: Vec<PeerEntry>,
}

impl Default for PeerTable {
    fn default() -> Self {
        Self::new(DISCOVERY_PEER_TIMEOUT)
    }
}

impl PeerTable {
    pub fn new(timeout: Duration) -> Self {
        Self {
            timeout,
            peers: Vec::new(),
        }
    }

    pub fn timeout(&self) -> Duration {
        self.timeout
    }

    pub fn len(&self) -> usize {
        self.peers.len()
    }

    pub fn is_empty(&self) -> bool {
        self.peers.is_empty()
    }

    pub fn entries(&self) -> &[PeerEntry] {
        &self.peers
    }

    pub fn get(&self, ip: IpAddr) -> Option<&PeerEntry> {
        self.peers.iter().find(|p| p.addr.ip() == ip)
    }

    /// 未超时的对端（手动条目恒在内）
    pub fn live(&self, now: Instant) -> Vec<&PeerEntry> {
        self.peers
            .iter()
            .filter(|p| !self.is_stale(p, now))
            .collect()
    }

    /// 登记/刷新一条信标来源；同 IP 复用条目（返回刷新后的快照，`new` = 是否新增）。
    /// `None` = 这是一条新来源但没进表（表满且全是手动条目）：调用方不该把它当设备呈现。
    pub fn upsert_beacon(
        &mut self,
        addr: SocketAddr,
        beacon: DiscoveryBeacon,
        now: Instant,
    ) -> Option<(PeerEntry, bool)> {
        if let Some(p) = self.peers.iter_mut().find(|p| p.addr.ip() == addr.ip()) {
            p.addr = addr;
            p.beacon = Some(beacon);
            p.last_seen = now;
            p.updates += 1;
            return Some((p.clone(), false));
        }
        let entry = PeerEntry {
            addr,
            beacon: Some(beacon),
            manual: false,
            last_seen: now,
            updates: 1,
        };
        // 进了表才算"新设备"：否则日志与列表上会冒出一个没人跟踪的幽灵条目
        if self.push_capped(entry.clone()) {
            Some((entry, true))
        } else {
            None
        }
    }

    /// 有界插入：满了挤掉"最久没见"的非手动条目，而不是拒绝新来者。
    /// 信标只有 64 B，但源地址没有上限——不设限的代价不是崩溃，是设备列表被垃圾淹没
    /// 加上每轮线性扫描变慢（与 `ble_frag` 的并发分组上限同一套手法）。
    fn push_capped(&mut self, entry: PeerEntry) -> bool {
        if self.peers.len() < MAX_PEER_ENTRIES {
            self.peers.push(entry);
            return true;
        }
        let victim = self
            .peers
            .iter()
            .enumerate()
            .filter(|(_, p)| !p.manual)
            .min_by_key(|(_, p)| p.last_seen)
            .map(|(i, _)| i);
        // 全是用户手动加的条目就不许被垃圾挤掉：这时宁可少显示一个自动发现的设备
        if let Some(i) = victim {
            self.peers.remove(i);
            self.peers.push(entry);
            return true;
        }
        false
    }

    /// 手动 IP 兜底：登记用户输入的地址（无信标信息，不随超时清理）
    pub fn upsert_manual(&mut self, addr: SocketAddr, now: Instant) -> PeerEntry {
        if let Some(p) = self.peers.iter_mut().find(|p| p.addr.ip() == addr.ip()) {
            // 已由广播发现 → 只标记 manual（保留信标信息），不重置 last_seen
            p.manual = true;
            return p.clone();
        }
        let entry = PeerEntry {
            addr,
            beacon: None,
            manual: true,
            last_seen: now,
            updates: 1,
        };
        self.push_capped(entry.clone());
        entry
    }

    pub fn remove(&mut self, ip: IpAddr) -> bool {
        let before = self.peers.len();
        self.peers.retain(|p| p.addr.ip() != ip);
        self.peers.len() != before
    }

    /// 清理超时的非手动条目，返回被移除的地址（手动条目保留，由用户显式移除）
    pub fn prune(&mut self, now: Instant) -> Vec<SocketAddr> {
        let timeout = self.timeout;
        let mut expired = Vec::new();
        self.peers.retain(|p| {
            let stale = !p.manual && now.saturating_duration_since(p.last_seen) > timeout;
            if stale {
                expired.push(p.addr);
            }
            !stale
        });
        expired
    }

    fn is_stale(&self, p: &PeerEntry, now: Instant) -> bool {
        !p.manual && now.saturating_duration_since(p.last_seen) > self.timeout
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct DiscoveryStats {
    pub beacons_sent: u64,
    pub beacons_received: u64,
    /// 解析失败 / 超长的垃圾数据报（静默丢弃，不 panic）
    pub malformed: u64,
    /// 因为"这一轮已经处理够多来源"而提前停表的次数（见 `pump` 里的每轮上限）
    pub round_capped: u64,
}

/// 单次广播结果（`failed` 常见于无广播路由的沙箱 / 关网场景，仅用于日志）
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct BroadcastReport {
    pub sent: usize,
    pub failed: usize,
}

#[derive(Debug, Default)]
pub struct DiscoveryTick {
    pub updated: Vec<PeerEntry>,
    pub expired: Vec<SocketAddr>,
}

/// UDP 发现承载：广播 + 监听 + peers 超时表 + 手动 IP 兜底
#[derive(Debug)]
pub struct UdpDiscovery {
    socket: UdpSocket,
    port: u16,
    peers: PeerTable,
    /// 除有限广播外的额外单播目标（手动 IP / 测试）
    extra_targets: Vec<SocketAddr>,
    /// 本机各子网的定向广播地址。多网卡时由调用方定期重算喂进来（见 [`directed_broadcast`]）；
    /// 空表就是旧行为：只发 `255.255.255.255`。
    subnet_targets: Vec<Ipv4Addr>,
    interval: Duration,
    last_broadcast: Option<Instant>,
    stats: DiscoveryStats,
}

impl UdpDiscovery {
    /// 绑定 `0.0.0.0:port`（收广播须绑通配地址）
    pub fn bind(port: u16) -> Result<Self, DiscoveryError> {
        Self::bind_addr(SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), port))
    }

    pub fn bind_addr(addr: SocketAddr) -> Result<Self, DiscoveryError> {
        let socket = UdpSocket::bind(addr)?;
        // 广播开关失败不致命（部分平台/网络受限），退化为仅单播
        socket.set_broadcast(true).ok();
        socket.set_nonblocking(true)?;
        let port = socket.local_addr()?.port();
        debuglog::log!(
            Level::Info,
            "lan",
            "udp.bind",
            &[("port", &port.to_string())]
        );
        Ok(Self {
            port,
            socket,
            peers: PeerTable::default(),
            extra_targets: Vec::new(),
            subnet_targets: Vec::new(),
            interval: DISCOVERY_BROADCAST_INTERVAL,
            last_broadcast: None,
            stats: DiscoveryStats::default(),
        })
    }

    pub fn local_addr(&self) -> Result<SocketAddr, DiscoveryError> {
        Ok(self.socket.local_addr()?)
    }

    /// 本机监听端口（bind 传 0 时为内核分配的实际端口）
    pub fn port(&self) -> u16 {
        self.port
    }

    pub fn peers(&self) -> &PeerTable {
        &self.peers
    }

    pub fn stats(&self) -> DiscoveryStats {
        self.stats
    }

    pub fn broadcast_interval(&self) -> Duration {
        self.interval
    }

    pub fn set_broadcast_interval(&mut self, interval: Duration) {
        self.interval = interval;
    }

    /// 追加单播目标（手动 IP 兜底 / 定向探测；广播不通时的可用路径）
    pub fn add_target(&mut self, addr: SocketAddr) {
        if !self.extra_targets.contains(&addr) {
            self.extra_targets.push(addr);
        }
    }

    /// 换一批本机子网的定向广播目标。变了才落一条埋点：插拔网线、开关 VMware 都会改这张表，
    /// 排查"手机搜不到电脑"时这是第一条要对的时间线。
    pub fn set_subnet_targets(&mut self, targets: Vec<Ipv4Addr>) {
        if targets == self.subnet_targets {
            return;
        }
        let list = targets
            .iter()
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join(",");
        self.subnet_targets = targets;
        debuglog::log!(Level::Info, "lan", "udp.subnets", &[("to", &list)]);
    }

    pub fn targets(&self) -> &[SocketAddr] {
        &self.extra_targets
    }

    /// 手动 IP 兜底：入 peers（`manual=true`，不超时）+ 作为单播目标
    pub fn add_manual_peer(&mut self, addr: SocketAddr, now: Instant) -> PeerEntry {
        self.add_target(addr);
        self.peers.upsert_manual(addr, now)
    }

    /// 立即广播一次（有限广播 + 本机每个子网的定向广播 + 全部单播目标）
    pub fn broadcast(
        &mut self,
        beacon: &DiscoveryBeacon,
        now: Instant,
    ) -> Result<BroadcastReport, DiscoveryError> {
        let payload = beacon.encode()?;
        let mut report = BroadcastReport::default();
        let addrs = beacon_targets(self.port, &self.subnet_targets, &self.extra_targets);
        for target in addrs {
            match self.socket.send_to(&payload, target) {
                Ok(_) => report.sent += 1,
                Err(_) => report.failed += 1,
            }
        }
        self.stats.beacons_sent += report.sent as u64;
        self.last_broadcast = Some(now);
        Ok(report)
    }

    /// 到达间隔才广播（调用方按 tick 驱动；未到间隔返回 `None`）
    pub fn broadcast_if_due(
        &mut self,
        beacon: &DiscoveryBeacon,
        now: Instant,
    ) -> Result<Option<BroadcastReport>, DiscoveryError> {
        if let Some(t) = self.last_broadcast {
            if now.saturating_duration_since(t) < self.interval {
                return Ok(None);
            }
        }
        self.broadcast(beacon, now).map(Some)
    }

    /// 非阻塞收包（清空当前待处理数据报）+ 超时清理；垃圾包只计数不报错
    pub fn pump(&mut self, now: Instant) -> DiscoveryTick {
        let mut tick = DiscoveryTick::default();
        let mut buf = [0u8; DISCOVERY_MAX_DATAGRAM_LEN];
        let mut seen_ips: Vec<IpAddr> = Vec::new();
        loop {
            let (n, from) = match self.socket.recv_from(&mut buf) {
                Ok(v) => v,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                // 其余 IO 错误（Windows 广播可能回灌 ICMP 端口不可达）：有意 break——未知
                // 错误下 continue 有忙等风险，未读走的数据报由下一轮 pump 续读。
                Err(_) => break,
            };
            if n > DISCOVERY_MAX_BEACON_LEN {
                self.stats.malformed += 1;
                continue;
            }
            let Ok(beacon) = DiscoveryBeacon::decode(&buf[..n]) else {
                self.stats.malformed += 1;
                continue;
            };
            self.stats.beacons_received += 1;
            // 同一轮内同 IP 只登记一次（多网卡重复到达）
            if seen_ips.contains(&from.ip()) {
                continue;
            }
            seen_ips.push(from.ip());
            let addr = SocketAddr::new(from.ip(), from.port());
            let Some((entry, is_new)) = self.peers.upsert_beacon(addr, beacon, now) else {
                continue;
            };
            if is_new {
                debuglog::log!(
                    Level::Info,
                    "lan",
                    "udp.peer.up",
                    &[("addr", &entry.addr.to_string())]
                );
            }
            tick.updated.push(entry);
            // 一轮的处理量必须有上限：`MAX_PEER_ENTRIES` 封的是表，不是这一轮的工作量。
            // 源地址是无穷的（伪造 UDP 源 IP 零成本），drain-to-WouldBlock 会让被灌的那一轮
            // 一直跑下去，`seen_ips` 的线性查重同时变成 O(n²)。剩下的数据报留在 socket
            // 缓冲区里，下一轮继续读 —— 慢一点没关系，这一轮不许没完。
            if tick.updated.len() >= MAX_PEER_ENTRIES {
                self.stats.round_capped += 1;
                break;
            }
        }
        tick.expired = self.peers.prune(now);
        if !tick.expired.is_empty() {
            debuglog::log!(
                Level::Info,
                "lan",
                "udp.peer.down",
                &[("count", &tick.expired.len().to_string())]
            );
        }
        tick
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use linkx_protocol::OS_ANDROID;

    /// 对端表必须有上限：源地址是无穷的，没上限就等于让"每轮扫表"的线性成本和设备列表
    /// 一起被垃圾条目拖大。
    #[test]
    fn peer_table_is_bounded() {
        let mut t = PeerTable::new(DISCOVERY_PEER_TIMEOUT);
        let base = Instant::now();
        for i in 0..(MAX_PEER_ENTRIES * 3) as u32 {
            let addr = SocketAddr::from((
                Ipv4Addr::new(10, 0, (i / 251) as u8, (i % 251) as u8 + 1),
                DISCOVERY_UDP_PORT,
            ));
            t.upsert_beacon(addr, beacon("x"), base + Duration::from_millis(i as u64));
        }
        assert!(
            t.len() <= MAX_PEER_ENTRIES,
            "对端表越写越大：{} 条",
            t.len()
        );
    }

    /// 表满且挤不掉任何条目时，这条来源根本没进表：既然没进表，就不许以"新设备"的身份
    /// 出现在日志和本轮列表里（否则设备列表里会冒出一个没人跟踪、点不动的幽灵条目）。
    #[test]
    fn rejected_beacon_is_not_reported_as_a_new_peer() {
        let mut t = PeerTable::new(DISCOVERY_PEER_TIMEOUT);
        let base = Instant::now();
        for i in 0..MAX_PEER_ENTRIES as u32 {
            t.upsert_manual(
                SocketAddr::from((
                    Ipv4Addr::new(10, 0, (i / 251) as u8, (i % 251) as u8 + 1),
                    0,
                )),
                base,
            );
        }
        assert_eq!(t.len(), MAX_PEER_ENTRIES, "前提：手动条目把表灌满");
        assert!(
            t.upsert_beacon(loopback(60001), beacon("junk"), base)
                .is_none(),
            "没进表的来源不得回报成新增"
        );
        assert_eq!(t.len(), MAX_PEER_ENTRIES, "手动条目不许被信标挤掉");
    }

    fn beacon(name: &str) -> DiscoveryBeacon {
        DiscoveryBeacon {
            advert_name: name.into(),
            os: OS_ANDROID,
            version: "0.1.0".into(),
        }
    }

    fn loopback(port: u16) -> SocketAddr {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port)
    }

    #[test]
    fn beacon_roundtrip() {
        let b = beacon("pc-home");
        let buf = b.encode().unwrap();
        assert!(buf.len() <= DISCOVERY_MAX_BEACON_LEN);
        assert_eq!(DiscoveryBeacon::decode(&buf).unwrap(), b);
    }

    /// 反向用例：超长设备名/版本必须能编码（截断），不得因超 64B 而失败
    #[test]
    fn long_device_name_beacon_encodes_within_limit() {
        let b = DiscoveryBeacon {
            advert_name: "A".repeat(200),
            os: OS_ANDROID,
            version: "9".repeat(60),
        };
        let buf = b.encode().expect("超长名称必须能编码（截断）");
        assert!(buf.len() <= DISCOVERY_MAX_BEACON_LEN);
        let back = DiscoveryBeacon::decode(&buf).unwrap();
        assert!(back.advert_name.starts_with('A'));
        // 中文（多字节）名称也要在字符边界截断且不 panic
        let cjk = DiscoveryBeacon {
            advert_name: "很长的中文设备名称".repeat(20),
            os: OS_ANDROID,
            version: "0.1.0".into(),
        };
        let buf2 = cjk.encode().unwrap();
        assert!(buf2.len() <= DISCOVERY_MAX_BEACON_LEN);
        assert!(DiscoveryBeacon::decode(&buf2).is_ok());
    }

    #[test]
    fn malformed_no_panic() {
        let mut seed = 0x11223344u32;
        for _ in 0..10_000 {
            seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
            let len = (seed % 40) as usize;
            let bytes: Vec<u8> = (0..len).map(|i| ((seed >> (i % 8)) & 0xFF) as u8).collect();
            let _ = DiscoveryBeacon::decode(&bytes);
        }
        let bad = tlv_codec::encode(&[Tlv::u8(TAG_OS, 1)]).unwrap();
        assert!(DiscoveryBeacon::decode(&bad).is_err());
    }

    #[test]
    fn udp_unicast_discovery_roundtrip() {
        // 手动 IP / 定向探测路径：A 发单播 → B pump 后看到 A
        let mut a = UdpDiscovery::bind_addr(loopback(0)).unwrap();
        let mut b = UdpDiscovery::bind_addr(loopback(0)).unwrap();
        a.add_target(loopback(b.port()));

        let t0 = Instant::now();
        let report = a.broadcast(&beacon("phone"), t0).unwrap();
        assert!(
            report.sent >= 1,
            "至少单播目标发送成功（广播受沙箱路由影响）"
        );

        let tick = b.pump(t0);
        assert_eq!(tick.updated.len(), 1);
        assert_eq!(
            tick.updated[0].beacon.as_ref().unwrap().advert_name,
            "phone"
        );
        assert_eq!(tick.updated[0].addr.port(), a.port());
        assert!(!tick.updated[0].manual);
        assert_eq!(b.peers().len(), 1);
        assert_eq!(b.stats().beacons_received, 1);
    }

    #[test]
    fn peer_expiry_by_timeout() {
        let mut table = PeerTable::new(Duration::from_secs(10));
        let t0 = Instant::now();
        table.upsert_beacon(loopback(55676), beacon("phone"), t0);
        assert_eq!(table.live(t0 + Duration::from_secs(9)).len(), 1);
        assert_eq!(table.prune(t0 + Duration::from_secs(9)), Vec::new());
        let expired = table.prune(t0 + Duration::from_secs(11));
        assert_eq!(expired, vec![loopback(55676)]);
        assert!(table.is_empty());
    }

    #[test]
    fn manual_peer_persists_and_upgrades() {
        let mut table = PeerTable::default();
        let t0 = Instant::now();
        let entry = table.upsert_manual(loopback(55676), t0);
        assert!(entry.manual);
        assert!(entry.beacon.is_none());
        // 超时也不清理（用户显式移除）
        assert_eq!(table.prune(t0 + Duration::from_secs(3600)), Vec::new());
        assert_eq!(table.len(), 1);
        // 收到信标 → 补全信息，manual 标记保留
        let (e, new) = table
            .upsert_beacon(loopback(55676), beacon("pc"), t0 + Duration::from_secs(5))
            .expect("表里就有条目，必然返回快照");
        assert!(!new);
        assert!(e.manual);
        assert_eq!(e.beacon.unwrap().advert_name, "pc");
        assert_eq!(e.updates, 2);
        assert_eq!(table.prune(t0 + Duration::from_secs(3600)), Vec::new());
        assert_eq!(table.len(), 1);
        assert!(table.remove(IpAddr::V4(Ipv4Addr::LOCALHOST)));
        assert!(table.is_empty());
    }

    #[test]
    fn dedupe_by_ip_across_ports() {
        let mut table = PeerTable::default();
        let t0 = Instant::now();
        let (e1, new1) = table
            .upsert_beacon(loopback(55676), beacon("pc"), t0)
            .expect("空表就该收下第一条");
        assert!(new1);
        let (e2, new2) = table
            .upsert_beacon(loopback(60000), beacon("pc"), t0 + Duration::from_secs(3))
            .expect("同 IP 复用条目");
        assert!(!new2);
        assert_eq!(table.len(), 1);
        assert_eq!(e1.updates, 1);
        assert_eq!(e2.updates, 2);
        assert_eq!(e2.addr.port(), 60000); // 端口以最新为准
    }

    #[test]
    fn junk_datagram_ignored_and_counted() {
        let mut b = UdpDiscovery::bind_addr(loopback(0)).unwrap();
        let raw = UdpSocket::bind(loopback(0)).unwrap();
        raw.send_to(b"not-a-tlv-at-all", loopback(b.port()))
            .unwrap();
        // 超长数据报（> 64B）同样拒绝
        raw.send_to(&[0xAAu8; 200], loopback(b.port())).unwrap();
        let tick = b.pump(Instant::now());
        assert!(tick.updated.is_empty());
        assert_eq!(b.stats().malformed, 2);
        assert!(b.peers().is_empty());
    }

    /// 回归：超长垃圾包不得吞掉同一轮里后续到达的正常信标。Windows 上 recv_from 缓冲
    /// 小于数据报时报 WSAEMSGSIZE 并消费该包（非 Linux 的静默截断），因此终止本轮 drain
    /// 就会丢掉后续信标——Linux 上恰好通过，故该不变量必须由双端共同的用例锁定。
    #[test]
    fn oversized_datagram_does_not_swallow_following_beacon() {
        let mut b = UdpDiscovery::bind_addr(loopback(0)).unwrap();
        let raw = UdpSocket::bind(loopback(0)).unwrap();
        let to = loopback(b.port());
        raw.send_to(&[0xAAu8; 200], to).unwrap();
        raw.send_to(&beacon("phone").encode().unwrap(), to).unwrap();
        let tick = b.pump(Instant::now());
        assert_eq!(b.stats().malformed, 1, "超长包须计入 malformed");
        assert_eq!(tick.updated.len(), 1, "同轮后续合法信标不得被吞掉");
        assert_eq!(b.peers().len(), 1);
    }

    #[test]
    fn broadcast_is_throttled_by_interval() {
        let mut a = UdpDiscovery::bind_addr(loopback(0)).unwrap();
        a.set_broadcast_interval(Duration::from_secs(3));
        let t0 = Instant::now();
        assert!(a.broadcast_if_due(&beacon("pc"), t0).unwrap().is_some());
        assert!(a
            .broadcast_if_due(&beacon("pc"), t0 + Duration::from_millis(2_999))
            .unwrap()
            .is_none());
        assert!(a
            .broadcast_if_due(&beacon("pc"), t0 + Duration::from_secs(3))
            .unwrap()
            .is_some());
        // 报告口径：sent + failed = 目标数（有限广播 + 单播目标）
        let r = a.broadcast(&beacon("pc"), t0).unwrap();
        assert_eq!(r.sent + r.failed, 1 + a.targets().len());
    }

    #[test]
    fn expired_peer_reported_in_tick() {
        let mut a = UdpDiscovery::bind_addr(loopback(0)).unwrap();
        let mut b = UdpDiscovery::bind_addr(loopback(0)).unwrap();
        a.add_target(loopback(b.port()));
        let t0 = Instant::now();
        a.broadcast(&beacon("phone"), t0).unwrap();
        assert_eq!(b.pump(t0).updated.len(), 1);
        let tick = b.pump(t0 + Duration::from_secs(11));
        assert!(tick.updated.is_empty());
        assert_eq!(tick.expired.len(), 1);
        assert!(b.peers().is_empty());
    }

    /// 定向广播地址算错是**静默**的：地址照样发得出去，只是没有对端在听。
    /// 所以每个分支都要有值断言，尤其"私网段必须留着"这一条 —— 用 `is_global()` 过滤
    /// 会把 192.168/16 与 10/8 整个滤掉，那正是本产品唯一工作的环境。
    #[test]
    fn directed_broadcast_covers_the_real_lans_and_skips_the_silent_ones() {
        let v4 = |o: [u8; 4]| Ipv4Addr::from(o);
        assert_eq!(
            directed_broadcast(v4([192, 168, 1, 24]), 24),
            Some(v4([192, 168, 1, 255])),
            "家庭 WLAN 最常见的那一段"
        );
        assert_eq!(
            directed_broadcast(v4([10, 0, 3, 7]), 8),
            Some(v4([10, 255, 255, 255])),
            "10/8 私网不许被当成「非全局」滤掉"
        );
        assert_eq!(
            directed_broadcast(v4([172, 16, 4, 9]), 20),
            Some(v4([172, 16, 15, 255]))
        );
        assert_eq!(
            directed_broadcast(v4([192, 168, 56, 1]), 24),
            Some(v4([192, 168, 56, 255])),
            "VMware 宿主网卡也发：多一个没人听的广播，比漏掉真网卡便宜"
        );
        assert_eq!(
            directed_broadcast(v4([192, 168, 1, 30]), 28),
            Some(v4([192, 168, 1, 31])),
            "非字节对齐的前缀也要算对：.30/28 落在 16–31 这一块"
        );
        for (ip, pfx) in [
            ([127, 0, 0, 1], 8u8),
            ([169, 254, 1, 9], 16),
            ([224, 0, 0, 1], 4),
            // 默认路由：它的"定向广播"就是 255.255.255.255，那条本来就在发
            ([192, 168, 1, 9], 0),
            ([192, 168, 1, 9], 31),
            ([192, 168, 1, 9], 32),
            ([192, 168, 1, 9], 33),
        ] {
            assert_eq!(
                directed_broadcast(v4(ip), pfx),
                None,
                "{}/{} 不该产生广播目标",
                Ipv4Addr::from(ip),
                pfx
            );
        }
    }

    /// `32 - prefix` 移出界是这条函数唯一能把自己弄崩的地方：debug 下 panic（我们的
    /// profile 是 `panic = "abort"`，等于整个进程没了），优化后悄悄把**本机自己的地址**
    /// 当广播目标发出去 —— 发送照样返回成功，信标却永远到不了手机。系统给的
    /// `OnLinkPrefixLength` 是 u8，隧道/PPP 网卡报什么值不由我们决定，所以要全档扫一遍。
    #[test]
    fn directed_broadcast_survives_every_prefix_the_os_might_report() {
        let v4 = |o: [u8; 4]| Ipv4Addr::from(o);
        let ip = v4([192, 168, 1, 24]);
        for prefix in 0u8..=40 {
            let out = directed_broadcast(ip, prefix);
            if prefix == 0 || prefix >= 31 {
                assert_eq!(out, None, "{prefix} 不该有广播目标");
                continue;
            }
            let b = out.unwrap_or(ip);
            assert_ne!(b, ip, "{prefix} 算出了本机地址本身，等于静默不发");
            assert!(
                !b.is_unspecified() && !b.is_loopback(),
                "{prefix} 算出了不能用的目标"
            );
        }
    }

    /// 发往 `255.255.255.255` 在 VMware/Docker 机器上返回成功却到不了手机，所以"sent 计数"
    /// 证明不了多网卡这一课 —— 判据只能是这张目标表里**每个网段都在**。
    #[test]
    fn beacon_targets_fan_out_to_every_subnet_and_keep_manual_unicast_ports() {
        let port = 55676u16;
        let s = |o: [u8; 4]| SocketAddr::new(IpAddr::V4(Ipv4Addr::from(o)), port);
        let subnets = vec![
            Ipv4Addr::new(192, 168, 1, 255),
            Ipv4Addr::new(192, 168, 56, 255),
            Ipv4Addr::new(192, 168, 1, 255), // 同网段两块网卡：只留一份
        ];
        // 手动 IP 用的是对端报上来的端口，不能被统一改成自己的端口
        let manual = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 8, 0, 5)), 41000);
        let got = beacon_targets(port, &subnets, &[manual]);
        assert_eq!(
            got,
            vec![
                s([255, 255, 255, 255]),
                s([192, 168, 1, 255]),
                s([192, 168, 56, 255]),
                manual
            ],
            "有限广播打底、每个网段各一份、手动单播连端口一起原样保留"
        );
        // 空表 = 旧行为，一条不多发
        assert_eq!(
            beacon_targets(port, &[], &[]),
            vec![s([255, 255, 255, 255])]
        );
    }
}
