//! The sidechain P2P messages and their bytes (`P2PServer::MessageId`, `src/p2p_server.h`;
//! readers in `P2PServer::P2PClient::on_read`, writers next to each `send` in
//! `src/p2p_server.cpp`).
//!
//! There is no envelope: the stream is a sequence of messages, each one byte of id followed by a
//! fixed or length-prefixed payload. All integers are little-endian. [`decode`] reads one message
//! from the front of a buffer and returns `Ok(None)` while it is incomplete; [`Message::encode`]
//! writes one.

use crate::block::MAX_BLOCK_SIZE;

/// `CHALLENGE_SIZE`: bytes of a handshake challenge and of a solution salt.
pub const CHALLENGE_SIZE: usize = 8;
/// `HASH_SIZE`.
pub const HASH_SIZE: usize = 32;
/// `PEER_LIST_RESPONSE_MAX_PEERS`.
pub const PEER_LIST_MAX_PEERS: usize = 16;
/// Bytes of one peer list entry: `is_v6`, 16 address bytes, `u16` port.
pub const PEER_ENTRY_SIZE: usize = 19;
/// Longest length-prefixed payload (blocks, aux job donations, Monero blocks): `MAX_BLOCK_SIZE`.
pub const MAX_PAYLOAD: usize = MAX_BLOCK_SIZE;
/// `P2P_BUF_SIZE`: the longest message (id, length, largest payload), 128 KiB.
pub const MAX_MESSAGE_SIZE: usize = 1 + 4 + MAX_PAYLOAD;

/// `P2PServer::MessageId`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum MessageId {
    /// `HANDSHAKE_CHALLENGE`
    HandshakeChallenge = 0,
    /// `HANDSHAKE_SOLUTION`
    HandshakeSolution = 1,
    /// `LISTEN_PORT`
    ListenPort = 2,
    /// `BLOCK_REQUEST`
    BlockRequest = 3,
    /// `BLOCK_RESPONSE`
    BlockResponse = 4,
    /// `BLOCK_BROADCAST`
    BlockBroadcast = 5,
    /// `PEER_LIST_REQUEST`
    PeerListRequest = 6,
    /// `PEER_LIST_RESPONSE`
    PeerListResponse = 7,
    /// `BLOCK_BROADCAST_COMPACT`
    BlockBroadcastCompact = 8,
    /// `BLOCK_NOTIFY`
    BlockNotify = 9,
    /// `AUX_JOB_DONATION`
    AuxJobDonation = 10,
    /// `MONERO_BLOCK_BROADCAST`
    MoneroBlockBroadcast = 11,
}

impl MessageId {
    /// The id for a byte, `None` above `LAST` (11).
    #[must_use]
    pub const fn from_byte(b: u8) -> Option<Self> {
        Some(match b {
            0 => Self::HandshakeChallenge,
            1 => Self::HandshakeSolution,
            2 => Self::ListenPort,
            3 => Self::BlockRequest,
            4 => Self::BlockResponse,
            5 => Self::BlockBroadcast,
            6 => Self::PeerListRequest,
            7 => Self::PeerListResponse,
            8 => Self::BlockBroadcastCompact,
            9 => Self::BlockNotify,
            10 => Self::AuxJobDonation,
            11 => Self::MoneroBlockBroadcast,
            _ => return None,
        })
    }

    /// The C++ name, for logs.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::HandshakeChallenge => "HANDSHAKE_CHALLENGE",
            Self::HandshakeSolution => "HANDSHAKE_SOLUTION",
            Self::ListenPort => "LISTEN_PORT",
            Self::BlockRequest => "BLOCK_REQUEST",
            Self::BlockResponse => "BLOCK_RESPONSE",
            Self::BlockBroadcast => "BLOCK_BROADCAST",
            Self::PeerListRequest => "PEER_LIST_REQUEST",
            Self::PeerListResponse => "PEER_LIST_RESPONSE",
            Self::BlockBroadcastCompact => "BLOCK_BROADCAST_COMPACT",
            Self::BlockNotify => "BLOCK_NOTIFY",
            Self::AuxJobDonation => "AUX_JOB_DONATION",
            Self::MoneroBlockBroadcast => "MONERO_BLOCK_BROADCAST",
        }
    }
}

/// One entry of `PEER_LIST_RESPONSE` (`P2PServer::Peer` on the wire): `is_v6`, the 16-byte
/// `raw_ip` (IPv4 as `::ffff:a.b.c.d`), the port.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PeerEntry {
    /// The address is IPv6 (any non-zero byte on the wire).
    pub is_v6: bool,
    /// `raw_ip::data`.
    pub ip: [u8; 16],
    /// Listen port.
    pub port: u16,
}

/// A decoded message. Block payloads are opaque bytes here; the sidechain parses them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Message {
    /// `HANDSHAKE_CHALLENGE`: 8 challenge bytes, then the sender's peer id.
    HandshakeChallenge {
        /// Random challenge (a `u64` written low byte first).
        challenge: [u8; CHALLENGE_SIZE],
        /// Sender's peer id.
        peer_id: u64,
    },
    /// `HANDSHAKE_SOLUTION`: `keccak(challenge || consensus id || salt)` and the salt.
    HandshakeSolution {
        /// The hash.
        solution: [u8; HASH_SIZE],
        /// The salt.
        salt: [u8; CHALLENGE_SIZE],
    },
    /// `LISTEN_PORT`: the sender's external port as an `i32`.
    ListenPort(i32),
    /// `BLOCK_REQUEST`: a share id, all zero for "your chain tip".
    BlockRequest([u8; HASH_SIZE]),
    /// `BLOCK_RESPONSE`: a full share, empty when the peer has none.
    BlockResponse(Vec<u8>),
    /// `BLOCK_BROADCAST`: a full (or pruned) share.
    BlockBroadcast(Vec<u8>),
    /// `PEER_LIST_REQUEST`.
    PeerListRequest,
    /// `PEER_LIST_RESPONSE`: at most 16 entries.
    PeerListResponse(Vec<PeerEntry>),
    /// `BLOCK_BROADCAST_COMPACT`: a share with its transaction list in the compact form.
    BlockBroadcastCompact(Vec<u8>),
    /// `BLOCK_NOTIFY`: a share id the sender has.
    BlockNotify([u8; HASH_SIZE]),
    /// `AUX_JOB_DONATION`: read and dropped (never sent by xkrd).
    AuxJobDonation(Vec<u8>),
    /// `MONERO_BLOCK_BROADCAST`: read and dropped (never sent by xkrd).
    MoneroBlockBroadcast(Vec<u8>),
}

/// A message that cannot be read; the peer is banned (`on_read` returns false).
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum WireError {
    /// Id above `MONERO_BLOCK_BROADCAST`.
    #[error("unknown message id {0}")]
    UnknownId(u8),
    /// A length prefix above `MAX_BLOCK_SIZE`.
    #[error("{id:?} of {len} bytes is too big")]
    TooBig {
        /// Message.
        id: MessageId,
        /// Announced length.
        len: u32,
    },
    /// A peer list with more than 16 entries.
    #[error("peer list with {0} entries")]
    TooManyPeers(u8),
}

impl Message {
    /// The message id.
    #[must_use]
    pub const fn id(&self) -> MessageId {
        match self {
            Self::HandshakeChallenge { .. } => MessageId::HandshakeChallenge,
            Self::HandshakeSolution { .. } => MessageId::HandshakeSolution,
            Self::ListenPort(_) => MessageId::ListenPort,
            Self::BlockRequest(_) => MessageId::BlockRequest,
            Self::BlockResponse(_) => MessageId::BlockResponse,
            Self::BlockBroadcast(_) => MessageId::BlockBroadcast,
            Self::PeerListRequest => MessageId::PeerListRequest,
            Self::PeerListResponse(_) => MessageId::PeerListResponse,
            Self::BlockBroadcastCompact(_) => MessageId::BlockBroadcastCompact,
            Self::BlockNotify(_) => MessageId::BlockNotify,
            Self::AuxJobDonation(_) => MessageId::AuxJobDonation,
            Self::MoneroBlockBroadcast(_) => MessageId::MoneroBlockBroadcast,
        }
    }

    /// Appends the message bytes to `out`, as the C++ writers do. Payloads longer than
    /// `MAX_BLOCK_SIZE` and peer lists longer than 16 entries are a caller error (they
    /// panic); xkrd only sends what it has checked.
    pub fn encode(&self, out: &mut Vec<u8>) {
        out.push(self.id() as u8);
        match self {
            Self::HandshakeChallenge { challenge, peer_id } => {
                out.extend_from_slice(challenge);
                out.extend_from_slice(&peer_id.to_le_bytes());
            }
            Self::HandshakeSolution { solution, salt } => {
                out.extend_from_slice(solution);
                out.extend_from_slice(salt);
            }
            Self::ListenPort(port) => out.extend_from_slice(&port.to_le_bytes()),
            Self::BlockRequest(id) | Self::BlockNotify(id) => out.extend_from_slice(id),
            Self::BlockResponse(blob)
            | Self::BlockBroadcast(blob)
            | Self::BlockBroadcastCompact(blob)
            | Self::AuxJobDonation(blob)
            | Self::MoneroBlockBroadcast(blob) => {
                assert!(blob.len() <= MAX_PAYLOAD, "payload above MAX_BLOCK_SIZE");
                out.extend_from_slice(&(blob.len() as u32).to_le_bytes());
                out.extend_from_slice(blob);
            }
            Self::PeerListRequest => {}
            Self::PeerListResponse(peers) => {
                assert!(
                    peers.len() <= PEER_LIST_MAX_PEERS,
                    "peer list above 16 entries"
                );
                out.push(peers.len() as u8);
                for p in peers {
                    out.push(u8::from(p.is_v6));
                    out.extend_from_slice(&p.ip);
                    out.extend_from_slice(&p.port.to_le_bytes());
                }
            }
        }
    }

    /// The message bytes.
    #[must_use]
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        self.encode(&mut out);
        out
    }
}

fn array<const N: usize>(b: &[u8]) -> [u8; N] {
    let mut a = [0u8; N];
    a.copy_from_slice(&b[..N]);
    a
}

/// Reads a `u32` length prefix and its payload at `buf[1..]`; `Ok(None)` while incomplete. The
/// length is checked as soon as its 4 bytes are there.
fn length_prefixed(buf: &[u8], id: MessageId) -> Result<Option<(Vec<u8>, usize)>, WireError> {
    if buf.len() < 5 {
        return Ok(None);
    }
    let len = u32::from_le_bytes(array(&buf[1..5]));
    if len as usize > MAX_PAYLOAD {
        return Err(WireError::TooBig { id, len });
    }
    let end = 5 + len as usize;
    if buf.len() < end {
        return Ok(None);
    }
    Ok(Some((buf[5..end].to_vec(), end)))
}

/// Reads the message at the front of `buf`: `Ok(Some((message, bytes used)))`, `Ok(None)` when
/// more bytes are needed, or an error for a message that can never be valid. Mirrors the
/// `switch` in `P2PServer::P2PClient::on_read` (`src/p2p_server.cpp:2345-2624`); the checks that
/// depend on the connection state live in [`super::session`].
pub fn decode(buf: &[u8]) -> Result<Option<(Message, usize)>, WireError> {
    let Some(&first) = buf.first() else {
        return Ok(None);
    };
    let id = MessageId::from_byte(first).ok_or(WireError::UnknownId(first))?;
    let need = |n: usize| buf.len() > n;
    let msg = match id {
        MessageId::HandshakeChallenge => {
            if !need(CHALLENGE_SIZE + 8) {
                return Ok(None);
            }
            let challenge = array(&buf[1..]);
            let peer_id = u64::from_le_bytes(array(&buf[1 + CHALLENGE_SIZE..]));
            (
                Message::HandshakeChallenge { challenge, peer_id },
                1 + CHALLENGE_SIZE + 8,
            )
        }
        MessageId::HandshakeSolution => {
            if !need(HASH_SIZE + CHALLENGE_SIZE) {
                return Ok(None);
            }
            let solution = array(&buf[1..]);
            let salt = array(&buf[1 + HASH_SIZE..]);
            (
                Message::HandshakeSolution { solution, salt },
                1 + HASH_SIZE + CHALLENGE_SIZE,
            )
        }
        MessageId::ListenPort => {
            if !need(4) {
                return Ok(None);
            }
            (Message::ListenPort(i32::from_le_bytes(array(&buf[1..]))), 5)
        }
        MessageId::BlockRequest | MessageId::BlockNotify => {
            if !need(HASH_SIZE) {
                return Ok(None);
            }
            let h = array(&buf[1..]);
            let m = if id == MessageId::BlockRequest {
                Message::BlockRequest(h)
            } else {
                Message::BlockNotify(h)
            };
            (m, 1 + HASH_SIZE)
        }
        MessageId::BlockResponse
        | MessageId::BlockBroadcast
        | MessageId::BlockBroadcastCompact
        | MessageId::AuxJobDonation
        | MessageId::MoneroBlockBroadcast => {
            let Some((blob, used)) = length_prefixed(buf, id)? else {
                return Ok(None);
            };
            let m = match id {
                MessageId::BlockResponse => Message::BlockResponse(blob),
                MessageId::BlockBroadcast => Message::BlockBroadcast(blob),
                MessageId::BlockBroadcastCompact => Message::BlockBroadcastCompact(blob),
                MessageId::AuxJobDonation => Message::AuxJobDonation(blob),
                _ => Message::MoneroBlockBroadcast(blob),
            };
            (m, used)
        }
        MessageId::PeerListRequest => (Message::PeerListRequest, 1),
        MessageId::PeerListResponse => {
            if buf.len() < 2 {
                return Ok(None);
            }
            let n = buf[1];
            if usize::from(n) > PEER_LIST_MAX_PEERS {
                return Err(WireError::TooManyPeers(n));
            }
            let used = 2 + usize::from(n) * PEER_ENTRY_SIZE;
            if buf.len() < used {
                return Ok(None);
            }
            let peers = buf[2..used]
                .as_chunks::<PEER_ENTRY_SIZE>()
                .0
                .iter()
                .map(|e| PeerEntry {
                    is_v6: e[0] != 0,
                    ip: array(&e[1..17]),
                    port: u16::from_le_bytes([e[17], e[18]]),
                })
                .collect();
            (Message::PeerListResponse(peers), used)
        }
    };
    Ok(Some(msg))
}
