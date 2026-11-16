//! Hyper-bridge frame codec (`docs/hyper-bridge-protocol.md`, v1).
//!
//! Every frame is a 16-byte header (`len u32, type u8, flags u8, shard u16, conn_id u64`, all
//! little-endian) followed by a body. Bodies may end with TLV extensions
//! (`tag u8 | len u16 | value`). The JS twin is `bridge/src/frames.js`; both are checked against
//! the shared golden vectors in `docs/test-vectors/hyper-bridge-frames.json`.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use bytes::{Buf, BufMut, Bytes, BytesMut};
use tokio_util::codec::{Decoder, Encoder};

/// Header length in bytes.
pub const HEADER_LEN: usize = 16;
/// Protocol major version.
pub const VERSION_MAJOR: u16 = 1;
/// Protocol minor version.
pub const VERSION_MINOR: u16 = 0;
/// Default `max_frame` (total frame size including the header).
pub const DEFAULT_MAX_FRAME: u32 = 16 * 1024 * 1024;
/// Default per-connection, per-direction credit window.
pub const DEFAULT_INITIAL_CREDIT: u32 = 1024 * 1024;
/// Smallest `max_frame` either side may propose.
pub const MIN_MAX_FRAME: u32 = 4096;
/// Noise handshake hash length used by hyperdht (BLAKE2b-512).
pub const HANDSHAKE_HASH_LEN: usize = 64;
/// Most TLV extensions decoded from one frame body. Protocol v1 defines at most three per body;
/// without a cap a 16 MiB frame of empty TLVs (3 wire bytes each, ~40 bytes decoded) cost ~220 MB
/// of heap. More than this is `BAD_FRAME`.
pub const MAX_TLVS_PER_FRAME: usize = 64;

/// Frame type bytes.
pub mod frame_type {
    pub const HELLO: u8 = 0x01;
    pub const PING: u8 = 0x02;
    pub const PONG: u8 = 0x03;
    pub const OPEN: u8 = 0x10;
    pub const OPENED: u8 = 0x11;
    pub const CLOSE: u8 = 0x12;
    pub const DATA: u8 = 0x20;
    pub const CREDIT: u8 = 0x21;
    pub const ANNOUNCE: u8 = 0x30;
    pub const UNANNOUNCE: u8 = 0x31;
    pub const LOOKUP: u8 = 0x32;
    pub const LOOKUP_RESULT: u8 = 0x33;
    pub const ACK: u8 = 0x34;
    pub const PEER_INFO: u8 = 0x40;
    pub const DHT_STATUS: u8 = 0x41;
    pub const ERROR: u8 = 0x7f;
}

/// Header flag bits.
pub mod flags {
    /// Last frame of a multi-frame reply (`LookupResult`).
    pub const END: u8 = 0x01;
    /// A receiver that does not know the frame type drops it instead of failing.
    pub const IGNORABLE: u8 = 0x80;
}

/// `Hello.features` bits.
pub mod features {
    /// The bridge understands the Hugin `mid` TLVs (`0x10`, `0x11`, `0x12`).
    pub const HUGIN_MID: u64 = 1;
}

/// TLV tags.
pub mod tlv_tag {
    /// `Hello`, `Ack`, `Opened`: a public key (node key, or the identity used for a topic), 32 B.
    pub const PUBLIC_KEY: u8 = 0x01;
    /// `Announce`, `Open`: precomputed Hugin `mid` signature, 64 B.
    pub const HUGIN_MID: u8 = 0x10;
    /// `Announce`, `Open`: Hugin topic key = seed of the topic base key pair, 32 B.
    pub const HUGIN_TOPIC_KEY: u8 = 0x11;
    /// `Announce`, `Open`: seed of the DHT key pair used for this Hugin topic, 32 B.
    pub const IDENTITY_SEED: u8 = 0x12;
}

/// `Open.open_flags` bits.
pub mod open_flags {
    /// Allow a relayed connection (needs a relay configured in the bridge).
    pub const ALLOW_RELAY: u32 = 1;
    /// Do not holepunch; only direct or LAN connections.
    pub const NO_HOLEPUNCH: u32 = 2;
}

/// `Close.reason` values.
pub mod close_reason {
    pub const NORMAL: u8 = 0;
    pub const ERROR: u8 = 1;
    pub const TIMEOUT: u8 = 2;
    pub const FIREWALL: u8 = 3;
    pub const PROTOCOL: u8 = 4;
    pub const SHUTDOWN: u8 = 5;
}

/// `Close.code` values used by the bridge (informative).
pub mod close_code {
    pub const UNSPECIFIED: u32 = 0;
    pub const PEER_NOT_FOUND: u32 = 1;
    pub const PEER_CONNECTION_FAILED: u32 = 2;
    pub const HOLEPUNCH_FAILED: u32 = 3;
    pub const TIMEOUT: u32 = 4;
    pub const MID_REJECTED: u32 = 5;
    pub const REMOTE_CLOSED: u32 = 6;
    pub const MESSAGE_TOO_LARGE: u32 = 7;
    pub const CREDIT_EXCEEDED: u32 = 8;
}

/// `DhtStatus.state` values.
pub mod dht_state {
    /// The UDP socket is bound or being bound; the DHT is bootstrapping.
    pub const STARTING: u8 = 0;
    /// The DHT is bootstrapped.
    pub const READY: u8 = 1;
    /// The DHT could not start (for example the fixed UDP port is taken).
    pub const FAILED: u8 = 2;
}

/// `DhtStatus.nat` values: the public port the DHT nodes saw.
pub mod nat {
    /// No samples yet.
    pub const UNKNOWN: u8 = 0;
    /// One public port for every destination.
    pub const CONSISTENT: u8 = 1;
    /// A new public port per destination (holepunching towards another such node fails).
    pub const RANDOM: u8 = 2;
}

/// `Error.code` values.
pub mod error_code {
    pub const UNKNOWN_TYPE: u16 = 1;
    pub const BAD_FRAME: u16 = 2;
    pub const UNKNOWN_CONN: u16 = 3;
    pub const CREDIT_EXCEEDED: u16 = 4;
    pub const DHT_ERROR: u16 = 5;
    pub const VERSION: u16 = 6;
}

/// Codec errors.
#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    #[error("truncated frame body")]
    Truncated,
    #[error("trailing bytes after frame body")]
    TrailingBytes,
    #[error("frame length {0} is below the header size")]
    TooShort(u32),
    #[error("frame of {len} bytes exceeds max_frame {max}")]
    TooLarge { len: usize, max: u32 },
    #[error("TLV value longer than 65535 bytes")]
    TlvTooLong,
    #[error("more than 64 TLV extensions in one frame (MAX_TLVS_PER_FRAME)")]
    TooManyTlvs,
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

// ---------------------------------------------------------------------------------------------
// connection ids

/// Connection id: bit 63 origin (1 = allocated by Bare), bits 48..62 shard, bits 0..47 counter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ConnId(pub u64);

impl ConnId {
    const ORIGIN: u64 = 1 << 63;
    const COUNTER_MASK: u64 = (1 << 48) - 1;

    /// Builds an id. `counter` must be in `1..2^48`.
    #[must_use]
    pub fn new(from_bare: bool, shard: u16, counter: u64) -> Self {
        assert!(
            (1..=Self::COUNTER_MASK).contains(&counter),
            "conn_id counter out of range"
        );
        let origin = if from_bare { Self::ORIGIN } else { 0 };
        Self(origin | (u64::from(shard & 0x7fff) << 48) | counter)
    }

    /// True if Bare allocated this id (inbound connection).
    #[must_use]
    pub fn from_bare(self) -> bool {
        self.0 & Self::ORIGIN != 0
    }

    /// Worker index encoded in the id.
    #[must_use]
    pub fn shard(self) -> u16 {
        ((self.0 >> 48) & 0x7fff) as u16
    }

    /// Per-origin, per-shard counter.
    #[must_use]
    pub fn counter(self) -> u64 {
        self.0 & Self::COUNTER_MASK
    }
}

// ---------------------------------------------------------------------------------------------
// TLV and address

/// One TLV extension.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tlv {
    pub tag: u8,
    pub value: Bytes,
}

impl Tlv {
    #[must_use]
    pub fn new(tag: u8, value: impl Into<Bytes>) -> Self {
        Self {
            tag,
            value: value.into(),
        }
    }
}

/// First TLV with `tag`.
#[must_use]
pub fn find_tlv(tlvs: &[Tlv], tag: u8) -> Option<&Bytes> {
    tlvs.iter().find(|t| t.tag == tag).map(|t| &t.value)
}

/// First TLV with `tag` as a fixed-size array (None if absent or of the wrong length).
#[must_use]
pub fn find_tlv_array<const N: usize>(tlvs: &[Tlv], tag: u8) -> Option<[u8; N]> {
    find_tlv(tlvs, tag).and_then(|v| <[u8; N]>::try_from(v.as_ref()).ok())
}

/// Address on the wire: `family u8 (0 none, 4, 6), port u16, addr [16]`. IPv4 uses
/// `addr[0..4]`, the rest is zero. The port is little-endian like every other integer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct WireAddr {
    pub family: u8,
    pub port: u16,
    pub addr: [u8; 16],
}

impl WireAddr {
    /// "No address".
    pub const NONE: Self = Self {
        family: 0,
        port: 0,
        addr: [0; 16],
    };

    #[must_use]
    pub fn from_socket_addr(sa: SocketAddr) -> Self {
        let mut addr = [0u8; 16];
        match sa.ip() {
            IpAddr::V4(v4) => {
                addr[..4].copy_from_slice(&v4.octets());
                Self {
                    family: 4,
                    port: sa.port(),
                    addr,
                }
            }
            IpAddr::V6(v6) => Self {
                family: 6,
                port: sa.port(),
                addr: v6.octets(),
            },
        }
    }

    /// `None` for family 0 or unknown families.
    #[must_use]
    pub fn to_socket_addr(&self) -> Option<SocketAddr> {
        match self.family {
            4 => {
                let ip = Ipv4Addr::new(self.addr[0], self.addr[1], self.addr[2], self.addr[3]);
                Some(SocketAddr::new(IpAddr::V4(ip), self.port))
            }
            6 => Some(SocketAddr::new(
                IpAddr::V6(Ipv6Addr::from(self.addr)),
                self.port,
            )),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------------------------
// bodies

/// `Hello` (0x01).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hello {
    pub major: u16,
    pub minor: u16,
    pub max_frame: u32,
    pub initial_credit: u32,
    pub workers: u16,
    pub worker: u16,
    pub features: u64,
    pub tlvs: Vec<Tlv>,
}

/// `Open` (0x10), Rust to Bare.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Open {
    pub request_id: u32,
    pub remote_public_key: [u8; 32],
    /// All zeros = no topic.
    pub topic: [u8; 32],
    pub open_flags: u32,
    pub tlvs: Vec<Tlv>,
}

/// `Opened` (0x11), Bare to Rust.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Opened {
    /// 0 for inbound connections (and Hyperswarm auto-dials).
    pub request_id: u32,
    pub remote_public_key: [u8; 32],
    pub handshake_hash: [u8; HANDSHAKE_HASH_LEN],
    /// All zeros = unknown.
    pub topic: [u8; 32],
    pub initiator: bool,
    pub addr: WireAddr,
    pub tlvs: Vec<Tlv>,
}

/// `Close` (0x12).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Close {
    pub reason: u8,
    pub code: u32,
    pub message: String,
}

/// `Announce` (0x30).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Announce {
    pub request_id: u32,
    pub topic: [u8; 32],
    /// 1 server, 2 client, 3 both.
    pub mode: u8,
    pub tlvs: Vec<Tlv>,
}

/// `Unannounce` (0x31).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unannounce {
    pub request_id: u32,
    pub topic: [u8; 32],
    pub tlvs: Vec<Tlv>,
}

/// `Lookup` (0x32).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lookup {
    pub request_id: u32,
    pub topic: [u8; 32],
    pub max_peers: u16,
    pub tlvs: Vec<Tlv>,
}

/// One peer in a `LookupResult`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LookupPeer {
    pub public_key: [u8; 32],
    /// First relay (DHT) node the peer announced through, not the peer's own address.
    pub addr: WireAddr,
}

/// `LookupResult` (0x33); the last frame of a reply carries [`flags::END`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LookupResult {
    pub request_id: u32,
    pub peers: Vec<LookupPeer>,
    pub tlvs: Vec<Tlv>,
}

/// `Ack` (0x34).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ack {
    pub request_id: u32,
    pub tlvs: Vec<Tlv>,
}

impl Ack {
    /// The local public key the worker used for the topic (TLV `0x01`), if present.
    #[must_use]
    pub fn local_public_key(&self) -> Option<[u8; 32]> {
        find_tlv_array(&self.tlvs, tlv_tag::PUBLIC_KEY)
    }
}

/// `PeerInfo` (0x40) reply body.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PeerInfo {
    pub rtt_us: u32,
    pub bytes_in: u64,
    pub bytes_out: u64,
    pub relayed: bool,
    pub holepunched: bool,
    pub addr: WireAddr,
}

/// `DhtStatus` (0x41), Bare to Rust: the state of the worker's DHT node. Sent after the Rust
/// side's `Hello` and whenever a field changes, always with [`flags::IGNORABLE`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DhtStatus {
    /// [`dht_state`].
    pub state: u8,
    /// Other DHT nodes cannot reach the UDP port without holepunching (meaningful once
    /// `state` is [`dht_state::READY`]).
    pub firewalled: bool,
    /// [`nat`].
    pub nat: u8,
    /// Local UDP port of the DHT server socket, 0 while unbound.
    pub udp_port: u16,
    pub tlvs: Vec<Tlv>,
}

/// `Error` (0x7f).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ErrorFrame {
    pub request_id: u32,
    pub code: u16,
    pub message: String,
}

/// A frame body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Body {
    Hello(Hello),
    Ping(u64),
    Pong(u64),
    Open(Box<Open>),
    Opened(Box<Opened>),
    Close(Close),
    Data(Bytes),
    Credit(u32),
    Announce(Box<Announce>),
    Unannounce(Unannounce),
    Lookup(Lookup),
    LookupResult(LookupResult),
    Ack(Ack),
    /// `PeerInfo` with an empty body (request, Rust to Bare).
    PeerInfoRequest,
    PeerInfo(PeerInfo),
    DhtStatus(DhtStatus),
    Error(ErrorFrame),
    /// A frame type this version does not know; kept verbatim.
    Unknown {
        frame_type: u8,
        raw: Bytes,
    },
}

impl Body {
    /// The wire type byte.
    #[must_use]
    pub fn frame_type(&self) -> u8 {
        use frame_type as t;
        match self {
            Body::Hello(_) => t::HELLO,
            Body::Ping(_) => t::PING,
            Body::Pong(_) => t::PONG,
            Body::Open(_) => t::OPEN,
            Body::Opened(_) => t::OPENED,
            Body::Close(_) => t::CLOSE,
            Body::Data(_) => t::DATA,
            Body::Credit(_) => t::CREDIT,
            Body::Announce(_) => t::ANNOUNCE,
            Body::Unannounce(_) => t::UNANNOUNCE,
            Body::Lookup(_) => t::LOOKUP,
            Body::LookupResult(_) => t::LOOKUP_RESULT,
            Body::Ack(_) => t::ACK,
            Body::PeerInfoRequest | Body::PeerInfo(_) => t::PEER_INFO,
            Body::DhtStatus(_) => t::DHT_STATUS,
            Body::Error(_) => t::ERROR,
            Body::Unknown { frame_type, .. } => *frame_type,
        }
    }
}

/// One frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub flags: u8,
    pub shard: u16,
    pub conn_id: u64,
    pub body: Body,
}

impl Frame {
    /// A control frame (`conn_id` 0, shard 0, no flags).
    #[must_use]
    pub fn control(body: Body) -> Self {
        Self {
            flags: 0,
            shard: 0,
            conn_id: 0,
            body,
        }
    }

    /// A frame about one connection.
    #[must_use]
    pub fn conn(conn_id: u64, body: Body) -> Self {
        Self {
            flags: 0,
            shard: ConnId(conn_id).shard(),
            conn_id,
            body,
        }
    }

    /// Encoded size in bytes (header included).
    #[must_use]
    pub fn encoded_len(&self) -> usize {
        HEADER_LEN + body_len(&self.body)
    }

    /// Appends the encoded frame to `dst`.
    pub fn encode_into(&self, dst: &mut BytesMut) -> Result<(), FrameError> {
        let len = self.encoded_len();
        dst.reserve(len);
        dst.put_u32_le((len - 4) as u32);
        dst.put_u8(self.body.frame_type());
        dst.put_u8(self.flags);
        dst.put_u16_le(self.shard);
        dst.put_u64_le(self.conn_id);
        encode_body(&self.body, dst)
    }

    /// Encodes into a fresh buffer.
    pub fn to_bytes(&self) -> Result<Bytes, FrameError> {
        let mut b = BytesMut::with_capacity(self.encoded_len());
        self.encode_into(&mut b)?;
        Ok(b.freeze())
    }

    /// Decodes one complete frame (`len` field included).
    pub fn decode(mut buf: Bytes) -> Result<Self, FrameError> {
        if buf.len() < HEADER_LEN {
            return Err(FrameError::Truncated);
        }
        let len = buf.get_u32_le() as usize;
        if len != buf.len() {
            return Err(FrameError::Truncated);
        }
        let ty = buf.get_u8();
        let flags = buf.get_u8();
        let shard = buf.get_u16_le();
        let conn_id = buf.get_u64_le();
        let body = decode_body(ty, buf)?;
        Ok(Self {
            flags,
            shard,
            conn_id,
            body,
        })
    }
}

fn tlvs_len(tlvs: &[Tlv]) -> usize {
    tlvs.iter().map(|t| 3 + t.value.len()).sum()
}

fn body_len(body: &Body) -> usize {
    match body {
        Body::Hello(h) => 24 + tlvs_len(&h.tlvs),
        Body::Ping(_) | Body::Pong(_) => 8,
        Body::Open(o) => 72 + tlvs_len(&o.tlvs),
        Body::Opened(o) => 4 + 32 + HANDSHAKE_HASH_LEN + 32 + 1 + 19 + tlvs_len(&o.tlvs),
        Body::Close(c) => 5 + c.message.len(),
        Body::Data(d) => d.len(),
        Body::Credit(_) => 4,
        Body::Announce(a) => 37 + tlvs_len(&a.tlvs),
        Body::Unannounce(u) => 36 + tlvs_len(&u.tlvs),
        Body::Lookup(l) => 38 + tlvs_len(&l.tlvs),
        Body::LookupResult(r) => 6 + r.peers.len() * 51 + tlvs_len(&r.tlvs),
        Body::Ack(a) => 4 + tlvs_len(&a.tlvs),
        Body::PeerInfoRequest => 0,
        Body::PeerInfo(_) => 41,
        Body::DhtStatus(d) => 5 + tlvs_len(&d.tlvs),
        Body::Error(e) => 6 + e.message.len(),
        Body::Unknown { raw, .. } => raw.len(),
    }
}

fn put_tlvs(dst: &mut BytesMut, tlvs: &[Tlv]) -> Result<(), FrameError> {
    for t in tlvs {
        let len = u16::try_from(t.value.len()).map_err(|_| FrameError::TlvTooLong)?;
        dst.put_u8(t.tag);
        dst.put_u16_le(len);
        dst.put_slice(&t.value);
    }
    Ok(())
}

fn put_addr(dst: &mut BytesMut, a: &WireAddr) {
    dst.put_u8(a.family);
    dst.put_u16_le(a.port);
    dst.put_slice(&a.addr);
}

fn encode_body(body: &Body, dst: &mut BytesMut) -> Result<(), FrameError> {
    match body {
        Body::Hello(h) => {
            dst.put_u16_le(h.major);
            dst.put_u16_le(h.minor);
            dst.put_u32_le(h.max_frame);
            dst.put_u32_le(h.initial_credit);
            dst.put_u16_le(h.workers);
            dst.put_u16_le(h.worker);
            dst.put_u64_le(h.features);
            put_tlvs(dst, &h.tlvs)?;
        }
        Body::Ping(n) | Body::Pong(n) => dst.put_u64_le(*n),
        Body::Open(o) => {
            dst.put_u32_le(o.request_id);
            dst.put_slice(&o.remote_public_key);
            dst.put_slice(&o.topic);
            dst.put_u32_le(o.open_flags);
            put_tlvs(dst, &o.tlvs)?;
        }
        Body::Opened(o) => {
            dst.put_u32_le(o.request_id);
            dst.put_slice(&o.remote_public_key);
            dst.put_slice(&o.handshake_hash);
            dst.put_slice(&o.topic);
            dst.put_u8(u8::from(o.initiator));
            put_addr(dst, &o.addr);
            put_tlvs(dst, &o.tlvs)?;
        }
        Body::Close(c) => {
            dst.put_u8(c.reason);
            dst.put_u32_le(c.code);
            dst.put_slice(c.message.as_bytes());
        }
        Body::Data(d) => dst.put_slice(d),
        Body::Credit(n) => dst.put_u32_le(*n),
        Body::Announce(a) => {
            dst.put_u32_le(a.request_id);
            dst.put_slice(&a.topic);
            dst.put_u8(a.mode);
            put_tlvs(dst, &a.tlvs)?;
        }
        Body::Unannounce(u) => {
            dst.put_u32_le(u.request_id);
            dst.put_slice(&u.topic);
            put_tlvs(dst, &u.tlvs)?;
        }
        Body::Lookup(l) => {
            dst.put_u32_le(l.request_id);
            dst.put_slice(&l.topic);
            dst.put_u16_le(l.max_peers);
            put_tlvs(dst, &l.tlvs)?;
        }
        Body::LookupResult(r) => {
            dst.put_u32_le(r.request_id);
            let count = u16::try_from(r.peers.len()).map_err(|_| FrameError::TlvTooLong)?;
            dst.put_u16_le(count);
            for p in &r.peers {
                dst.put_slice(&p.public_key);
                put_addr(dst, &p.addr);
            }
            put_tlvs(dst, &r.tlvs)?;
        }
        Body::Ack(a) => {
            dst.put_u32_le(a.request_id);
            put_tlvs(dst, &a.tlvs)?;
        }
        Body::PeerInfoRequest => {}
        Body::PeerInfo(p) => {
            dst.put_u32_le(p.rtt_us);
            dst.put_u64_le(p.bytes_in);
            dst.put_u64_le(p.bytes_out);
            dst.put_u8(u8::from(p.relayed));
            dst.put_u8(u8::from(p.holepunched));
            put_addr(dst, &p.addr);
        }
        Body::DhtStatus(d) => {
            dst.put_u8(d.state);
            dst.put_u8(u8::from(d.firewalled));
            dst.put_u8(d.nat);
            dst.put_u16_le(d.udp_port);
            put_tlvs(dst, &d.tlvs)?;
        }
        Body::Error(e) => {
            dst.put_u32_le(e.request_id);
            dst.put_u16_le(e.code);
            dst.put_slice(e.message.as_bytes());
        }
        Body::Unknown { raw, .. } => dst.put_slice(raw),
    }
    Ok(())
}

/// Checked reader over a body.
struct Rd(Bytes);

impl Rd {
    fn need(&self, n: usize) -> Result<(), FrameError> {
        if self.0.len() < n {
            Err(FrameError::Truncated)
        } else {
            Ok(())
        }
    }
    fn u8(&mut self) -> Result<u8, FrameError> {
        self.need(1)?;
        Ok(self.0.get_u8())
    }
    fn u16(&mut self) -> Result<u16, FrameError> {
        self.need(2)?;
        Ok(self.0.get_u16_le())
    }
    fn u32(&mut self) -> Result<u32, FrameError> {
        self.need(4)?;
        Ok(self.0.get_u32_le())
    }
    fn u64(&mut self) -> Result<u64, FrameError> {
        self.need(8)?;
        Ok(self.0.get_u64_le())
    }
    fn arr<const N: usize>(&mut self) -> Result<[u8; N], FrameError> {
        self.need(N)?;
        let mut out = [0u8; N];
        self.0.copy_to_slice(&mut out);
        Ok(out)
    }
    fn addr(&mut self) -> Result<WireAddr, FrameError> {
        let family = self.u8()?;
        let port = self.u16()?;
        let addr = self.arr::<16>()?;
        Ok(WireAddr { family, port, addr })
    }
    fn text(&mut self) -> String {
        let rest = std::mem::take(&mut self.0);
        String::from_utf8_lossy(&rest).into_owned()
    }
    fn tlvs(&mut self) -> Result<Vec<Tlv>, FrameError> {
        let mut out = Vec::new();
        while !self.0.is_empty() {
            if out.len() >= MAX_TLVS_PER_FRAME {
                return Err(FrameError::TooManyTlvs);
            }
            if self.0.len() < 3 {
                return Err(FrameError::Truncated);
            }
            let tag = self.0.get_u8();
            let len = usize::from(self.0.get_u16_le());
            self.need(len)?;
            out.push(Tlv {
                tag,
                value: self.0.split_to(len),
            });
        }
        Ok(out)
    }
    fn end(&self) -> Result<(), FrameError> {
        if self.0.is_empty() {
            Ok(())
        } else {
            Err(FrameError::TrailingBytes)
        }
    }
}

fn decode_body(ty: u8, raw: Bytes) -> Result<Body, FrameError> {
    use frame_type as t;
    let mut r = Rd(raw);
    let body = match ty {
        t::HELLO => Body::Hello(Hello {
            major: r.u16()?,
            minor: r.u16()?,
            max_frame: r.u32()?,
            initial_credit: r.u32()?,
            workers: r.u16()?,
            worker: r.u16()?,
            features: r.u64()?,
            tlvs: r.tlvs()?,
        }),
        t::PING => {
            let n = r.u64()?;
            r.end()?;
            Body::Ping(n)
        }
        t::PONG => {
            let n = r.u64()?;
            r.end()?;
            Body::Pong(n)
        }
        t::OPEN => Body::Open(Box::new(Open {
            request_id: r.u32()?,
            remote_public_key: r.arr()?,
            topic: r.arr()?,
            open_flags: r.u32()?,
            tlvs: r.tlvs()?,
        })),
        t::OPENED => Body::Opened(Box::new(Opened {
            request_id: r.u32()?,
            remote_public_key: r.arr()?,
            handshake_hash: r.arr()?,
            topic: r.arr()?,
            initiator: r.u8()? != 0,
            addr: r.addr()?,
            tlvs: r.tlvs()?,
        })),
        t::CLOSE => Body::Close(Close {
            reason: r.u8()?,
            code: r.u32()?,
            message: r.text(),
        }),
        t::DATA => Body::Data(std::mem::take(&mut r.0)),
        t::CREDIT => {
            let n = r.u32()?;
            r.end()?;
            Body::Credit(n)
        }
        t::ANNOUNCE => Body::Announce(Box::new(Announce {
            request_id: r.u32()?,
            topic: r.arr()?,
            mode: r.u8()?,
            tlvs: r.tlvs()?,
        })),
        t::UNANNOUNCE => Body::Unannounce(Unannounce {
            request_id: r.u32()?,
            topic: r.arr()?,
            tlvs: r.tlvs()?,
        }),
        t::LOOKUP => Body::Lookup(Lookup {
            request_id: r.u32()?,
            topic: r.arr()?,
            max_peers: r.u16()?,
            tlvs: r.tlvs()?,
        }),
        t::LOOKUP_RESULT => {
            let request_id = r.u32()?;
            let count = r.u16()?;
            let mut peers = Vec::with_capacity(usize::from(count).min(1024));
            for _ in 0..count {
                peers.push(LookupPeer {
                    public_key: r.arr()?,
                    addr: r.addr()?,
                });
            }
            Body::LookupResult(LookupResult {
                request_id,
                peers,
                tlvs: r.tlvs()?,
            })
        }
        t::ACK => Body::Ack(Ack {
            request_id: r.u32()?,
            tlvs: r.tlvs()?,
        }),
        t::PEER_INFO => {
            if r.0.is_empty() {
                Body::PeerInfoRequest
            } else {
                let p = PeerInfo {
                    rtt_us: r.u32()?,
                    bytes_in: r.u64()?,
                    bytes_out: r.u64()?,
                    relayed: r.u8()? != 0,
                    holepunched: r.u8()? != 0,
                    addr: r.addr()?,
                };
                r.end()?;
                Body::PeerInfo(p)
            }
        }
        t::DHT_STATUS => Body::DhtStatus(DhtStatus {
            state: r.u8()?,
            firewalled: r.u8()? != 0,
            nat: r.u8()?,
            udp_port: r.u16()?,
            tlvs: r.tlvs()?,
        }),
        t::ERROR => Body::Error(ErrorFrame {
            request_id: r.u32()?,
            code: r.u16()?,
            message: r.text(),
        }),
        other => Body::Unknown {
            frame_type: other,
            raw: std::mem::take(&mut r.0),
        },
    };
    Ok(body)
}

// ---------------------------------------------------------------------------------------------
// tokio codec

/// `tokio_util` codec for frames over a byte stream.
#[derive(Debug, Clone, Copy)]
pub struct FrameCodec {
    /// Largest accepted/produced frame, header included.
    pub max_frame: u32,
}

impl Default for FrameCodec {
    fn default() -> Self {
        Self {
            max_frame: DEFAULT_MAX_FRAME,
        }
    }
}

impl FrameCodec {
    #[must_use]
    pub fn new(max_frame: u32) -> Self {
        Self { max_frame }
    }
}

impl Decoder for FrameCodec {
    type Item = Frame;
    type Error = FrameError;

    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Frame>, FrameError> {
        if src.len() < 4 {
            return Ok(None);
        }
        let len = u32::from_le_bytes([src[0], src[1], src[2], src[3]]);
        if (len as usize) < HEADER_LEN - 4 {
            return Err(FrameError::TooShort(len));
        }
        // In u64, so the length prefix plus its 4 bytes fits on every target.
        let total = u64::from(len) + 4;
        if total > u64::from(self.max_frame) {
            return Err(FrameError::TooLarge {
                len: usize::try_from(total).unwrap_or(usize::MAX),
                max: self.max_frame,
            });
        }
        let total = total as usize;
        if src.len() < total {
            src.reserve(total - src.len());
            return Ok(None);
        }
        let frame = src.split_to(total).freeze();
        Frame::decode(frame).map(Some)
    }
}

impl Encoder<Frame> for FrameCodec {
    type Error = FrameError;

    fn encode(&mut self, item: Frame, dst: &mut BytesMut) -> Result<(), FrameError> {
        let len = item.encoded_len();
        if len > self.max_frame as usize {
            return Err(FrameError::TooLarge {
                len,
                max: self.max_frame,
            });
        }
        item.encode_into(dst)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOLDEN: &str = include_str!("../../../docs/test-vectors/hyper-bridge-frames.json");

    fn golden() -> Vec<(String, Vec<u8>)> {
        let v: serde_json::Value = serde_json::from_str(GOLDEN).unwrap();
        v["vectors"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| {
                (
                    e["name"].as_str().unwrap().to_owned(),
                    hex::decode(e["hex"].as_str().unwrap()).unwrap(),
                )
            })
            .collect()
    }

    fn v4(a: u8, b: u8, c: u8, d: u8, port: u16) -> WireAddr {
        let mut addr = [0u8; 16];
        addr[..4].copy_from_slice(&[a, b, c, d]);
        WireAddr {
            family: 4,
            port,
            addr,
        }
    }

    const BARE: u64 = 1 << 63;

    /// The same frames as `bridge/test/vectors.js`, written by hand.
    fn hand_written(name: &str) -> Frame {
        let seq64: Vec<u8> = (0u8..64).collect();
        let pk11 = Tlv::new(tlv_tag::PUBLIC_KEY, vec![0x11; 32]);
        match name {
            "hello" => Frame::control(Body::Hello(Hello {
                major: 1,
                minor: 0,
                max_frame: 16_777_216,
                initial_credit: 1_048_576,
                workers: 1,
                worker: 0,
                features: 1,
                tlvs: vec![pk11],
            })),
            "ping" => Frame::control(Body::Ping(0x0102_0304_0506_0708)),
            "pong" => Frame::control(Body::Pong(0x0102_0304_0506_0708)),
            "open" => Frame::conn(
                1,
                Body::Open(Box::new(Open {
                    request_id: 7,
                    remote_public_key: [0xaa; 32],
                    topic: [0xbb; 32],
                    open_flags: 1,
                    tlvs: vec![Tlv::new(tlv_tag::HUGIN_TOPIC_KEY, vec![0xcc; 32])],
                })),
            ),
            "opened_inbound_v4" => Frame::conn(
                BARE | 1,
                Body::Opened(Box::new(Opened {
                    request_id: 0,
                    remote_public_key: [0xaa; 32],
                    handshake_hash: [0xdd; 64],
                    topic: [0; 32],
                    initiator: false,
                    addr: v4(192, 168, 1, 10, 49737),
                    tlvs: vec![],
                })),
            ),
            "opened_outbound_v6" => Frame::conn(
                2,
                Body::Opened(Box::new(Opened {
                    request_id: 8,
                    remote_public_key: [0xaa; 32],
                    handshake_hash: [0xdd; 64],
                    topic: [0xbb; 32],
                    initiator: true,
                    addr: WireAddr::from_socket_addr("[2001:db8::1]:443".parse().unwrap()),
                    tlvs: vec![pk11],
                })),
            ),
            "close" => Frame::conn(
                1,
                Body::Close(Close {
                    reason: 3,
                    code: 5,
                    message: "mid rejected".into(),
                }),
            ),
            "data" => Frame::conn(1, Body::Data(Bytes::from_static(b"hello"))),
            "data_empty" => Frame::conn(BARE | 2, Body::Data(Bytes::new())),
            "credit" => Frame::conn(1, Body::Credit(65536)),
            "announce_hugin" => Frame::control(Body::Announce(Box::new(Announce {
                request_id: 9,
                topic: [0xbb; 32],
                mode: 3,
                tlvs: vec![
                    Tlv::new(tlv_tag::HUGIN_MID, seq64),
                    Tlv::new(tlv_tag::HUGIN_TOPIC_KEY, vec![0xcc; 32]),
                ],
            }))),
            "announce_plain" => Frame::control(Body::Announce(Box::new(Announce {
                request_id: 1,
                topic: [0x01; 32],
                mode: 1,
                tlvs: vec![],
            }))),
            "unannounce" => Frame::control(Body::Unannounce(Unannounce {
                request_id: 10,
                topic: [0xbb; 32],
                tlvs: vec![],
            })),
            "lookup" => Frame::control(Body::Lookup(Lookup {
                request_id: 11,
                topic: [0xbb; 32],
                max_peers: 32,
                tlvs: vec![],
            })),
            "lookup_result" => Frame {
                flags: flags::END,
                ..Frame::control(Body::LookupResult(LookupResult {
                    request_id: 11,
                    peers: vec![
                        LookupPeer {
                            public_key: [0xaa; 32],
                            addr: v4(1, 2, 3, 4, 5),
                        },
                        LookupPeer {
                            public_key: [0xee; 32],
                            addr: WireAddr::NONE,
                        },
                    ],
                    tlvs: vec![],
                }))
            },
            "ack" => Frame::control(Body::Ack(Ack {
                request_id: 9,
                tlvs: vec![pk11],
            })),
            "peer_info_request" => Frame::conn(1, Body::PeerInfoRequest),
            "peer_info" => Frame::conn(
                BARE | 1,
                Body::PeerInfo(PeerInfo {
                    rtt_us: 1500,
                    bytes_in: 1000,
                    bytes_out: 2000,
                    relayed: false,
                    holepunched: true,
                    addr: v4(10, 0, 0, 1, 1234),
                }),
            ),
            "dht_status" => Frame {
                flags: flags::IGNORABLE,
                ..Frame::control(Body::DhtStatus(DhtStatus {
                    state: dht_state::READY,
                    firewalled: true,
                    nat: nat::RANDOM,
                    udp_port: 49737,
                    tlvs: vec![],
                }))
            },
            "error" => Frame::control(Body::Error(ErrorFrame {
                request_id: 9,
                code: 5,
                message: "dht".into(),
            })),
            "unknown_ignorable" => Frame {
                flags: flags::IGNORABLE,
                shard: 2,
                conn_id: 0,
                body: Body::Unknown {
                    frame_type: 0x55,
                    raw: Bytes::from_static(&[1, 2, 3]),
                },
            },
            other => panic!("no hand-written frame for golden vector {other}"),
        }
    }

    #[test]
    fn golden_vectors_encode() {
        let all = golden();
        assert_eq!(all.len(), 21);
        for (name, bytes) in &all {
            let enc = hand_written(name).to_bytes().unwrap();
            assert_eq!(hex::encode(&enc), hex::encode(bytes), "encode {name}");
        }
    }

    #[test]
    fn golden_vectors_decode() {
        for (name, bytes) in golden() {
            let dec = Frame::decode(Bytes::from(bytes.clone())).unwrap();
            assert_eq!(dec, hand_written(&name), "decode {name}");
        }
    }

    #[test]
    fn hand_checked_bytes() {
        // ping: len 20, type 2, flags 0, shard 0, conn 0, nonce LE
        let f = Frame::control(Body::Ping(0x0102_0304_0506_0708))
            .to_bytes()
            .unwrap();
        assert_eq!(
            f.as_ref(),
            &[
                20, 0, 0, 0, 0x02, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, //
                8, 7, 6, 5, 4, 3, 2, 1
            ]
        );
        // credit 65536 on conn 1
        let f = Frame::conn(1, Body::Credit(65536)).to_bytes().unwrap();
        assert_eq!(
            f.as_ref(),
            &[
                16, 0, 0, 0, 0x21, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0
            ]
        );
        // inbound conn id has bit 63 set: last header byte 0x80
        let f = Frame::conn(BARE | 2, Body::Data(Bytes::new()))
            .to_bytes()
            .unwrap();
        assert_eq!(
            f.as_ref(),
            &[12, 0, 0, 0, 0x20, 0, 0, 0, 2, 0, 0, 0, 0, 0, 0, 0x80]
        );
    }

    #[test]
    fn codec_streams_all_vectors_byte_by_byte() {
        let all = golden();
        let mut stream = Vec::new();
        for (_, b) in &all {
            stream.extend_from_slice(b);
        }
        let mut codec = FrameCodec::default();
        let mut buf = BytesMut::new();
        let mut got = Vec::new();
        for byte in stream {
            buf.put_u8(byte);
            while let Some(f) = codec.decode(&mut buf).unwrap() {
                got.push(f);
            }
        }
        assert_eq!(got.len(), all.len());
        for (f, (name, _)) in got.iter().zip(&all) {
            assert_eq!(f, &hand_written(name));
        }
        // encoder round trip through the codec
        let mut out = BytesMut::new();
        for f in got {
            codec.encode(f, &mut out).unwrap();
        }
        let joined: Vec<u8> = all.iter().flat_map(|(_, b)| b.clone()).collect();
        assert_eq!(out.as_ref(), joined.as_slice());
    }

    #[test]
    fn codec_limits() {
        let mut codec = FrameCodec::new(4096);
        let big = Frame::conn(1, Body::Data(Bytes::from(vec![0u8; 4096])));
        let mut out = BytesMut::new();
        assert!(matches!(
            codec.encode(big.clone(), &mut out),
            Err(FrameError::TooLarge { .. })
        ));
        let mut src = BytesMut::from(big.to_bytes().unwrap().as_ref());
        assert!(matches!(
            codec.decode(&mut src),
            Err(FrameError::TooLarge { .. })
        ));
        let mut tiny = BytesMut::from(&[4u8, 0, 0, 0, 1, 0, 0, 0][..]);
        assert!(matches!(
            codec.decode(&mut tiny),
            Err(FrameError::TooShort(4))
        ));
    }

    #[test]
    fn truncated_and_trailing() {
        // ack with its last TLV byte cut off
        let (_, ack) = golden().into_iter().find(|(n, _)| n == "ack").unwrap();
        let mut cut = ack[..ack.len() - 1].to_vec();
        let len = (cut.len() - 4) as u32;
        cut[..4].copy_from_slice(&len.to_le_bytes());
        assert!(matches!(
            Frame::decode(Bytes::from(cut)),
            Err(FrameError::Truncated)
        ));
        // ping with one extra byte
        let mut ping = Frame::control(Body::Ping(1)).to_bytes().unwrap().to_vec();
        ping.push(0);
        let len = (ping.len() - 4) as u32;
        ping[..4].copy_from_slice(&len.to_le_bytes());
        assert!(matches!(
            Frame::decode(Bytes::from(ping)),
            Err(FrameError::TrailingBytes)
        ));
    }

    #[test]
    fn unknown_tlvs_are_kept() {
        let f = Frame::control(Body::Ack(Ack {
            request_id: 1,
            tlvs: vec![
                Tlv::new(0xfe, vec![9, 9]),
                Tlv::new(tlv_tag::PUBLIC_KEY, vec![7; 32]),
            ],
        }));
        let d = Frame::decode(f.to_bytes().unwrap()).unwrap();
        let Body::Ack(ack) = d.body else {
            panic!("not an ack")
        };
        assert_eq!(ack.tlvs.len(), 2);
        assert_eq!(ack.local_public_key(), Some([7; 32]));
        assert_eq!(find_tlv(&ack.tlvs, 0x02), None);
    }

    #[test]
    fn conn_id_layout() {
        let id = ConnId::new(true, 3, 42);
        assert!(id.from_bare());
        assert_eq!(id.shard(), 3);
        assert_eq!(id.counter(), 42);
        assert_eq!(ConnId::new(false, 0, 1).0, 1);
        assert_eq!(Frame::conn(id.0, Body::Credit(1)).shard, 3);
    }

    #[test]
    fn wire_addr_round_trip() {
        for s in ["192.168.1.10:49737", "[2001:db8::1]:443", "[::1]:1"] {
            let sa: SocketAddr = s.parse().unwrap();
            assert_eq!(WireAddr::from_socket_addr(sa).to_socket_addr(), Some(sa));
        }
        assert_eq!(WireAddr::NONE.to_socket_addr(), None);
    }
}
