//! Admin (node-server) messages: `src/p2p/p2p_protocol_definitions.h` and
//! `src/p2p/p2p_protocol_types.h`.
//!
//! Field names are the epee keys and must match the C++ `KV_MEMBER(...)` names byte for
//! byte. Field order follows the C++ `serialize()` bodies so our encoding is byte-identical
//! to `LevinProtocol::encode` (the C++ reader itself is order-independent).

use std::net::{Ipv4Addr, SocketAddrV4};

use bytes::{BufMut, BytesMut};
use xkr_epee::epee_object;
use xkr_types::Hash;

use crate::blob::{FixedBlob, HashValue, PodList};

/// `basic_node_data`.
///
/// C++ order: `network_id`, `version`, `peer_id`, `local_time`, `my_port`.
/// `network_id` is a `boost::uuids::uuid` written with `s.binary(&v, 16)` -> 16-byte string.
/// On input C++ pre-sets `version = 0` before reading, so a missing `version` means 0.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BasicNodeData {
    pub network_id: [u8; 16],
    pub version: u8,
    pub peer_id: u64,
    pub local_time: u64,
    pub my_port: u32,
}

epee_object!(
    BasicNodeData,
    network_id: [u8; 16],
    version: u8 = 0_u8,
    peer_id: u64,
    local_time: u64,
    my_port: u32,
);

/// `CORE_SYNC_DATA` { `current_height`: u32 (top index + 1), `top_id`: hash } plus xkr's
/// optional `pruning_seed`.
///
/// `pruning_seed` (u32, `stripes << 8 | stripe`, 0 = full node, `0x0100` = pruned completely,
/// recent blocks only; Monero carries the same key in its `CORE_SYNC_DATA`) is an xkr addition
/// for pruned peers
/// (`docs/node-architecture.md`, "Pruned peers"). It travels in every handshake and timed sync
/// in both directions. It is written last and only when non-zero, so a full node's encoding is
/// byte-identical to the C++ one, and a missing key decodes as 0. The C++ node ignores the key:
/// `KVBinaryInputStreamSerializer` parses the whole section into a `JsonValue` tree and
/// `CORE_SYNC_DATA::serialize` only looks up `current_height` and `top_id` by name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoreSyncData {
    pub current_height: u32,
    pub top_id: Hash,
    /// Raw pruning seed of the sender (0 = full node or a peer that does not send one).
    pub pruning_seed: u32,
}

epee_object!(
    CoreSyncData,
    current_height: u32,
    top_id: Hash as HashValue,
    pruning_seed: u32 = 0_u32,
);

/// `PeerlistEntry` { `NetworkAddress adr` { u32 ip; u32 port }, u64 id, u64 `last_seen` }.
///
/// No `#pragma pack`, natural alignment gives exactly 24 bytes without padding. It only
/// ever travels inside a `serializeAsBinary` blob (raw `memcpy`, little-endian on every
/// platform the C++ node runs on).
///
/// `ip` is in network order stored in a host `u32`: `ip & 0xff` is the first octet
/// (`common::ipAddressToString`), i.e. `ip.to_le_bytes() == [a, b, c, d]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct PeerlistEntry {
    pub ip: u32,
    pub port: u32,
    pub id: u64,
    pub last_seen: u64,
}

impl PeerlistEntry {
    /// The IPv4 socket address of this entry.
    #[must_use]
    pub fn addr(&self) -> SocketAddrV4 {
        SocketAddrV4::new(Ipv4Addr::from(self.ip.to_le_bytes()), self.port as u16)
    }

    /// Builds the C++ `NetworkAddress` encoding of `addr`.
    #[must_use]
    pub fn encode_addr(addr: SocketAddrV4) -> (u32, u32) {
        (
            u32::from_le_bytes(addr.ip().octets()),
            u32::from(addr.port()),
        )
    }
}

impl FixedBlob for PeerlistEntry {
    const SIZE: usize = 24;

    fn read_blob(b: &[u8]) -> Self {
        Self {
            ip: u32::from_le_bytes(b[0..4].try_into().unwrap()),
            port: u32::from_le_bytes(b[4..8].try_into().unwrap()),
            id: u64::from_le_bytes(b[8..16].try_into().unwrap()),
            last_seen: u64::from_le_bytes(b[16..24].try_into().unwrap()),
        }
    }

    fn write_blob(&self, out: &mut BytesMut) {
        out.put_u32_le(self.ip);
        out.put_u32_le(self.port);
        out.put_u64_le(self.id);
        out.put_u64_le(self.last_seen);
    }
}

/// `connection_entry` { `NetworkAddress adr`; u64 id; bool `is_income` }.
///
/// Natural alignment: adr at 0..8, id at 8..16, `is_income` at 16, 7 bytes tail padding ->
/// `sizeof == 24`. We write the padding as zeros.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ConnectionEntry {
    pub ip: u32,
    pub port: u32,
    pub id: u64,
    pub is_income: bool,
}

impl FixedBlob for ConnectionEntry {
    const SIZE: usize = 24;

    fn read_blob(b: &[u8]) -> Self {
        Self {
            ip: u32::from_le_bytes(b[0..4].try_into().unwrap()),
            port: u32::from_le_bytes(b[4..8].try_into().unwrap()),
            id: u64::from_le_bytes(b[8..16].try_into().unwrap()),
            is_income: b[16] != 0,
        }
    }

    fn write_blob(&self, out: &mut BytesMut) {
        out.put_u32_le(self.ip);
        out.put_u32_le(self.port);
        out.put_u64_le(self.id);
        out.put_u8(u8::from(self.is_income));
        out.put_bytes(0, 7);
    }
}

/// `COMMAND_HANDSHAKE::request` (ID 1001).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HandshakeRequest {
    pub node_data: BasicNodeData,
    pub payload_data: CoreSyncData,
}

epee_object!(
    HandshakeRequest,
    node_data: BasicNodeData,
    payload_data: CoreSyncData,
);

/// `COMMAND_HANDSHAKE::response` (ID 1001).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HandshakeResponse {
    pub node_data: BasicNodeData,
    pub payload_data: CoreSyncData,
    /// `serializeAsBinary(local_peerlist, "local_peerlist", s)`
    pub local_peerlist: Vec<PeerlistEntry>,
}

epee_object!(
    HandshakeResponse,
    node_data: BasicNodeData,
    payload_data: CoreSyncData,
    local_peerlist: Vec<PeerlistEntry> as PodList<PeerlistEntry>,
);

/// `COMMAND_TIMED_SYNC::request` (ID 1002).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimedSyncRequest {
    pub payload_data: CoreSyncData,
}

epee_object!(
    TimedSyncRequest,
    payload_data: CoreSyncData,
);

/// `COMMAND_TIMED_SYNC::response` (ID 1002).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimedSyncResponse {
    pub local_time: u64,
    pub payload_data: CoreSyncData,
    pub local_peerlist: Vec<PeerlistEntry>,
}

epee_object!(
    TimedSyncResponse,
    local_time: u64,
    payload_data: CoreSyncData,
    local_peerlist: Vec<PeerlistEntry> as PodList<PeerlistEntry>,
);

/// An object without fields (`COMMAND_PING::request`, `COMMAND_REQUEST_PEER_ID::request`).
/// Encodes as the 9-byte header plus a zero field count.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct EmptyMessage;

epee_object!(EmptyMessage,);

/// `COMMAND_PING::response` (ID 1003): { status: "OK", `peer_id` }.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PingResponse {
    pub status: String,
    pub peer_id: u64,
}

epee_object!(
    PingResponse,
    status: String,
    peer_id: u64,
);

/// `proof_of_trust` { `peer_id`, time, sign: `crypto::Signature` (64-byte string) }.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProofOfTrust {
    pub peer_id: u64,
    pub time: u64,
    pub sign: [u8; 64],
}

epee_object!(
    ProofOfTrust,
    peer_id: u64,
    time: u64,
    sign: [u8; 64],
);

/// `CoreStatistics` (`src/cryptonote_core/core_statistics.h`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoreStatistics {
    pub tx_pool_size: u64,
    pub blockchain_height: u64,
    pub mining_speed: u64,
    pub alternative_blocks: u64,
    pub top_block_id_str: String,
}

epee_object!(
    CoreStatistics,
    tx_pool_size: u64,
    blockchain_height: u64,
    mining_speed: u64,
    alternative_blocks: u64,
    top_block_id_str: String,
);

/// `COMMAND_REQUEST_STAT_INFO::request` (ID 1004, only with `ALLOW_DEBUG_COMMANDS`, which
/// release builds do not define — such peers answer `-6`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatInfoRequest {
    pub tr: ProofOfTrust,
}

epee_object!(
    StatInfoRequest,
    tr: ProofOfTrust,
);

/// `COMMAND_REQUEST_STAT_INFO::response` (ID 1004).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatInfoResponse {
    pub version: String,
    pub os_version: String,
    pub connections_count: u64,
    pub incoming_connections_count: u64,
    pub payload_info: CoreStatistics,
}

epee_object!(
    StatInfoResponse,
    version: String,
    os_version: String,
    connections_count: u64,
    incoming_connections_count: u64,
    payload_info: CoreStatistics,
);

/// `COMMAND_REQUEST_NETWORK_STATE::request` (ID 1005, debug only).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetworkStateRequest {
    pub tr: ProofOfTrust,
}

epee_object!(
    NetworkStateRequest,
    tr: ProofOfTrust,
);

/// `COMMAND_REQUEST_NETWORK_STATE::response` (ID 1005, debug only).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetworkStateResponse {
    pub local_peerlist_white: Vec<PeerlistEntry>,
    pub local_peerlist_gray: Vec<PeerlistEntry>,
    pub connections_list: Vec<ConnectionEntry>,
    pub my_id: u64,
    pub local_time: u64,
}

epee_object!(
    NetworkStateResponse,
    local_peerlist_white: Vec<PeerlistEntry> as PodList<PeerlistEntry>,
    local_peerlist_gray: Vec<PeerlistEntry> as PodList<PeerlistEntry>,
    connections_list: Vec<ConnectionEntry> as PodList<ConnectionEntry>,
    my_id: u64,
    local_time: u64,
);

/// `COMMAND_REQUEST_PEER_ID::response` (ID 1006, debug only).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerIdResponse {
    pub my_id: u64,
}

epee_object!(
    PeerIdResponse,
    my_id: u64,
);
