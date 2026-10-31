//! LinkX lan crate — 传输层。
//!
//! - `discovery`：UDP 广播发现承载（信标 + peers 超时表 + 手动 IP 兜底）
//! - `transport`：TCP 帧通道（`TcpStreamLink`，含抗重放）+ 手动兜底地址解析

pub mod discovery;
pub mod transport;

pub use discovery::{
    directed_broadcast, BeaconError, BroadcastReport, DiscoveryBeacon, DiscoveryError,
    DiscoveryStats, DiscoveryTick, PeerEntry, PeerTable, UdpDiscovery, DISCOVERY_BROADCAST_ADDR,
    DISCOVERY_BROADCAST_INTERVAL, DISCOVERY_MAX_BEACON_LEN, DISCOVERY_PEER_TIMEOUT,
    DISCOVERY_UDP_PORT,
};
pub use transport::{
    parse_manual_addr, BleUuid, FrameChannel, TcpStreamLink, TransportError, TRANSPORT_TCP_PORT,
};
