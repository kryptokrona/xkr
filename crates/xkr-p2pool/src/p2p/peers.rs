//! Peer lists, the protocol version record, bans and the broadcast throttle
//! (`P2PServer::P2PClient::on_peer_list_request` / `on_peer_list_response`, `P2PServer::Peer`,
//! `TCPServer::ban` / `is_banned`, `P2PServer::throttle_broadcast`).

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::{Duration, Instant};

use super::message::{PEER_LIST_MAX_PEERS, PeerEntry};

/// `PROTOCOL_VERSION_1_0`.
pub const PROTOCOL_1_0: u32 = 0x0001_0000;
/// `PROTOCOL_VERSION_1_1`: compact broadcasts.
pub const PROTOCOL_1_1: u32 = 0x0001_0001;
/// `PROTOCOL_VERSION_1_2`: `BLOCK_NOTIFY` instead of echoing a broadcast.
pub const PROTOCOL_1_2: u32 = 0x0001_0002;
/// `SUPPORTED_PROTOCOL_VERSION` (1.4), the version xkrd announces.
pub const SUPPORTED_PROTOCOL: u32 = crate::PROTOCOL_VERSION;

/// `MAX_PEER_LIST_SIZE`: known peers kept.
pub const MAX_PEER_LIST_SIZE: usize = 5000;
/// A known peer not seen for this long is forgotten (`P2PServer::update_peer_connections`).
pub const PEER_EXPIRY: Duration = Duration::from_hours(1);
/// `PEER_REQUEST_DELAY`: peer list answers to one peer at most this often (else an empty list),
/// and our requests to one peer every 60 to 120 s.
pub const PEER_REQUEST_DELAY: Duration = Duration::from_mins(1);

/// `raw_ip::ipv4_prefix`.
pub const IPV4_PREFIX: [u8; 12] = [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0xff, 0xff];

/// `raw_ip::localhost_ipv4` or `localhost_ipv6`.
#[must_use]
pub fn is_localhost(ip: &[u8; 16]) -> bool {
    *ip == [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0xff, 0xff, 127, 0, 0, 1]
        || *ip == Ipv6Addr::LOCALHOST.octets()
}

/// `raw_ip::is_ipv4_prefix`.
#[must_use]
pub fn is_ipv4_prefix(ip: &[u8; 16]) -> bool {
    ip[..12] == IPV4_PREFIX
}

impl PeerEntry {
    /// The entry for a socket address (IPv4 as `::ffff:a.b.c.d` with `is_v6 = false`).
    #[must_use]
    pub fn from_socket_addr(addr: SocketAddr) -> Self {
        let (is_v6, ip) = match addr.ip() {
            IpAddr::V4(v4) => (false, v4.to_ipv6_mapped().octets()),
            IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
                Some(v4) => (false, v4.to_ipv6_mapped().octets()),
                None => (true, v6.octets()),
            },
        };
        Self {
            is_v6,
            ip,
            port: addr.port(),
        }
    }

    /// The socket address of the entry.
    #[must_use]
    pub fn socket_addr(&self) -> SocketAddr {
        let ip = if self.is_v6 {
            IpAddr::V6(Ipv6Addr::from(self.ip))
        } else {
            IpAddr::V4(Ipv4Addr::new(
                self.ip[12],
                self.ip[13],
                self.ip[14],
                self.ip[15],
            ))
        };
        SocketAddr::new(ip, self.port)
    }

    /// `P2PServer::Peer::normalize`: an IPv6 entry carrying an IPv4-mapped address becomes IPv4;
    /// an IPv4 entry gets the `::ffff:` prefix.
    #[must_use]
    pub fn normalized(mut self) -> Self {
        if self.is_v6 && is_ipv4_prefix(&self.ip) {
            self.is_v6 = false;
        } else if !self.is_v6 {
            self.ip[..12].copy_from_slice(&IPV4_PREFIX);
        }
        self
    }
}

/// The protocol version record in slot 0 of the first `PEER_LIST_RESPONSE` on a connection
/// (`on_peer_list_request`, `src/p2p_server.cpp:3394-3409`): `is_v6 = 0`, address bytes
/// `[0..4]` protocol version, `[4..8]` software version, `[8..12]` software id, `[12..16]`
/// `ff ff ff ff`, port `0xffff`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VersionRecord {
    /// Protocol version (`major << 16 | minor`).
    pub protocol: u32,
    /// Software version (`major << 16 | minor << 8 | patch`).
    pub software_version: u32,
    /// Software id (`SoftwareID`; xkrd: [`crate::SOFTWARE_ID`]).
    pub software_id: u32,
}

impl VersionRecord {
    /// What xkrd announces: protocol 1.4, [`crate::SOFTWARE_VERSION`], [`crate::SOFTWARE_ID`]
    /// (`XKRD`).
    #[must_use]
    pub const fn ours() -> Self {
        Self {
            protocol: SUPPORTED_PROTOCOL,
            software_version: crate::SOFTWARE_VERSION,
            software_id: crate::SOFTWARE_ID,
        }
    }

    /// The record as a peer list entry.
    #[must_use]
    pub fn to_entry(self) -> PeerEntry {
        let mut ip = [0u8; 16];
        ip[0..4].copy_from_slice(&self.protocol.to_le_bytes());
        ip[4..8].copy_from_slice(&self.software_version.to_le_bytes());
        ip[8..12].copy_from_slice(&self.software_id.to_le_bytes());
        ip[12..16].copy_from_slice(&[0xff; 4]);
        PeerEntry {
            is_v6: false,
            ip,
            port: 0xffff,
        }
    }

    /// The record carried by an IPv4 entry whose last four address bytes are `ff ff ff ff` and
    /// whose port is `0xffff` (`on_peer_list_response`, `src/p2p_server.cpp:3479-3500`).
    #[must_use]
    pub fn from_entry(e: &PeerEntry) -> Option<Self> {
        if e.is_v6 || e.ip[12..16] != [0xff; 4] || e.port != 0xffff {
            return None;
        }
        let word = |i: usize| u32::from_le_bytes([e.ip[i], e.ip[i + 1], e.ip[i + 2], e.ip[i + 3]]);
        Some(Self {
            protocol: word(0),
            software_version: word(4),
            software_id: word(8),
        })
    }
}

/// The protocol version spoken with a peer after it announced `announced`: another major
/// version means 1.0; the same major means `max(current, min(announced, 1.4))`, never a
/// downgrade.
#[must_use]
pub fn negotiate_protocol(current: u32, announced: u32) -> u32 {
    if announced >> 16 == SUPPORTED_PROTOCOL >> 16 {
        current.max(announced.min(SUPPORTED_PROTOCOL))
    } else {
        PROTOCOL_1_0
    }
}

/// A received peer list after the filter of `on_peer_list_response`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ReceivedPeers {
    /// The version record, if the list carried one.
    pub version: Option<VersionRecord>,
    /// Usable addresses, normalized.
    pub peers: Vec<PeerEntry>,
}

/// Filters a received peer list (`on_peer_list_response`, `src/p2p_server.cpp:3452-3537`):
/// localhost and port 0 are skipped; an IPv6 entry with an IPv4-mapped address counts as IPv4;
/// IPv4 addresses in `0.0.0.0/8`, `127.0.0.0/8` and `224.0.0.0/3` are skipped, one of them may
/// be the version record.
#[must_use]
pub fn filter_received(entries: &[PeerEntry]) -> ReceivedPeers {
    let mut out = ReceivedPeers::default();
    for e in entries {
        if is_localhost(&e.ip) || e.port == 0 {
            continue;
        }
        let mut p = *e;
        if p.is_v6 && is_ipv4_prefix(&p.ip) {
            p.is_v6 = false;
        }
        if !p.is_v6 {
            let first = p.ip[12];
            if first == 0 || first == 127 || first >= 224 {
                if let Some(v) = VersionRecord::from_entry(&p) {
                    out.version = Some(v);
                }
                continue;
            }
        }
        out.peers.push(p.normalized());
    }
    out
}

/// How many peers a peer list answer carries: `min(16, max(1, connections / 4))`.
#[must_use]
pub fn peers_to_send(num_connections: usize) -> usize {
    (num_connections / 4).clamp(1, PEER_LIST_MAX_PEERS)
}

/// Picks up to `target` of `candidates` at random (reservoir sampling, as
/// `on_peer_list_request`): the first `target` fill the reservoir, the `n`-th candidate then
/// replaces slot `k = floor(r * n / 2^64)` when `k < target`.
#[must_use]
pub fn select_peers(
    candidates: &[PeerEntry],
    target: usize,
    mut random: impl FnMut() -> u64,
) -> Vec<PeerEntry> {
    let mut picked: Vec<PeerEntry> = Vec::with_capacity(target.min(candidates.len()));
    for (i, c) in candidates.iter().enumerate() {
        if picked.len() < target {
            picked.push(*c);
            continue;
        }
        let n = (i + 1) as u128;
        let k = ((u128::from(random()) * n) >> 64) as usize;
        if k < target {
            picked[k] = *c;
        }
    }
    picked
}

/// Builds the entries of a peer list answer: on the first answer of a connection the version
/// record takes slot 0 (replacing the first pick, or as the only entry).
#[must_use]
pub fn response_entries(
    mut picked: Vec<PeerEntry>,
    first: bool,
    version: VersionRecord,
) -> Vec<PeerEntry> {
    if first {
        if picked.is_empty() {
            picked.push(version.to_entry());
        } else {
            picked[0] = version.to_entry();
        }
    }
    picked
}

/// The known peers (`P2PServer::m_peerList`, saved as `p2pool_peers.txt`).
#[derive(Debug, Default)]
pub struct PeerBook {
    peers: Vec<(PeerEntry, Instant)>,
}

impl PeerBook {
    /// An empty list.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds peers or refreshes their last-seen time; at most [`MAX_PEER_LIST_SIZE`], banned
    /// and localhost addresses are not added. Returns how many were new.
    pub fn add(
        &mut self,
        entries: &[PeerEntry],
        now: Instant,
        banned: impl Fn(&PeerEntry) -> bool,
    ) -> usize {
        let mut added = 0;
        for e in entries {
            let e = e.normalized();
            if let Some(known) = self.peers.iter_mut().find(|(p, _)| p.ip == e.ip) {
                known.1 = now;
                continue;
            }
            if self.peers.len() < MAX_PEER_LIST_SIZE && !is_localhost(&e.ip) && !banned(&e) {
                self.peers.push((e, now));
                added += 1;
            }
        }
        added
    }

    /// Forgets peers not seen for [`PEER_EXPIRY`].
    pub fn expire(&mut self, now: Instant) {
        self.peers
            .retain(|(_, seen)| now.saturating_duration_since(*seen) <= PEER_EXPIRY);
    }

    /// Removes one address (`remove_peer_from_list`).
    pub fn remove(&mut self, ip: &[u8; 16]) {
        self.peers.retain(|(p, _)| p.ip != *ip);
    }

    /// The known peers.
    pub fn entries(&self) -> impl Iterator<Item = &PeerEntry> {
        self.peers.iter().map(|(p, _)| p)
    }

    /// Number of known peers.
    #[must_use]
    pub fn len(&self) -> usize {
        self.peers.len()
    }

    /// No known peers.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.peers.is_empty()
    }
}

/// `DEFAULT_BAN_TIME`: 60 s.
pub const BAN_TIME: Duration = Duration::from_mins(1);
/// `BAN_FAULT_THRESHOLD`: the 5th rejected block on one connection bans the peer.
pub const BAN_FAULT_THRESHOLD: u32 = 5;

/// What a ban applies to: an IP address (IPv6 by its /64 prefix) for TCP peers, the DHT public
/// key for Hyperswarm peers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum BanKey {
    /// `raw_ip`, IPv6 cut to /64.
    Ip([u8; 16]),
    /// Hyperswarm public key.
    Key([u8; 32]),
}

impl BanKey {
    /// The key of an address (`TCPServer::ban`): IPv6 outside the IPv4 prefix keeps only its
    /// first 8 bytes. `None` for localhost, which is never banned.
    #[must_use]
    pub fn ip(addr: IpAddr) -> Option<Self> {
        let e = PeerEntry::from_socket_addr(SocketAddr::new(addr, 0));
        if is_localhost(&e.ip) {
            return None;
        }
        let mut ip = e.ip;
        if e.is_v6 && !is_ipv4_prefix(&ip) {
            ip[8..].fill(0);
        }
        Some(Self::Ip(ip))
    }
}

/// Banned peers with their expiry (`TCPServer::m_bans`).
#[derive(Debug, Default)]
pub struct BanList {
    bans: HashMap<BanKey, Instant>,
}

impl BanList {
    /// No bans.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Bans `key` for [`BAN_TIME`] from `now`.
    pub fn ban(&mut self, key: BanKey, now: Instant) {
        self.ban_for(key, now, BAN_TIME);
    }

    /// Bans `key` for `time` from `now`.
    pub fn ban_for(&mut self, key: BanKey, now: Instant, time: Duration) {
        self.bans.insert(key, now + time);
    }

    /// Whether `key` is banned at `now`; an expired ban is removed.
    pub fn is_banned(&mut self, key: &BanKey, now: Instant) -> bool {
        match self.bans.get(key) {
            Some(until) if now < *until => true,
            Some(_) => {
                self.bans.remove(key);
                false
            }
            None => false,
        }
    }

    /// Drops expired bans.
    pub fn prune(&mut self, now: Instant) {
        self.bans.retain(|_, until| now < *until);
    }

    /// Number of bans held (expired ones included until pruned).
    #[must_use]
    pub fn len(&self) -> usize {
        self.bans.len()
    }

    /// No bans held.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.bans.is_empty()
    }
}

/// `BroadcastThrottle::LIMITS`: `(time units, broadcasts)`; one time unit is a tenth of the
/// sidechain block time.
pub const THROTTLE_LIMITS: [(u64, u32); 3] = [(1, 10), (60, 60), (600, 180)];

/// The broadcast throttle per sender (`P2PServer::throttle_broadcast`): more broadcasts than a
/// limit within its window bans the sender. Keyed by [`BanKey`] (TCP: IP, IPv6 /64).
#[derive(Debug)]
pub struct BroadcastThrottle {
    unit_us: u64,
    senders: HashMap<BanKey, [(u64, u32); 3]>,
}

impl BroadcastThrottle {
    /// A throttle for a sidechain with `block_time` seconds per share.
    #[must_use]
    pub fn new(block_time: u64) -> Self {
        Self {
            unit_us: block_time * 1_000_000 / 10,
            senders: HashMap::new(),
        }
    }

    /// Counts one broadcast from `key` at `now_us` (microseconds); `true` means over a limit.
    pub fn over_limit(&mut self, key: BanKey, now_us: u64) -> bool {
        let data = self.senders.entry(key).or_insert([(0, 0); 3]);
        for (slot, (units, max)) in data.iter_mut().zip(THROTTLE_LIMITS) {
            if now_us >= slot.0 + units * self.unit_us {
                *slot = (now_us, 1);
            } else {
                slot.1 += 1;
                if slot.1 > max {
                    return true;
                }
            }
        }
        false
    }

    /// Forgets senders whose shortest window started before the longest window
    /// (`clean_broadcast_throttle_data`).
    pub fn clean(&mut self, now_us: u64) {
        let timeout = THROTTLE_LIMITS[2].0 * self.unit_us;
        if now_us < timeout {
            return;
        }
        let cutoff = now_us - timeout;
        self.senders.retain(|_, d| d[0].0 >= cutoff);
    }
}
