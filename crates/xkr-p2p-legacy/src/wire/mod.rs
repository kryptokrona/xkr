//! Wire types and the Levin body (`LegacyMessage`) for the XKR legacy protocol.

pub mod p2p;
pub mod protocol;

use std::fmt;

use bytes::{Buf, Bytes, BytesMut};
use xkr_epee::EpeeObject;
use xkr_levin::{BucketBuilder, BucketError, LevinBody, LevinCommand, MessageType};

use crate::constants::{LEVIN_RETCODE_SUCCESS, bucket_limits};

pub use p2p::*;
pub use protocol::*;

/// Levin command ids (`P2P_COMMANDS_POOL_BASE = 1000`, `BC_COMMANDS_POOL_BASE = 2000`).
///
/// Note XKR (TurtleCoin) numbering differs from Monero above 2007: 2008 is
/// `NOTIFY_REQUEST_TX_POOL`, 2009 `NOTIFY_NEW_LITE_BLOCK`, 2010 `NOTIFY_MISSING_TXS`, and
/// 2005 is unused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LegacyCommand {
    /// `COMMAND_HANDSHAKE` 1001
    Handshake,
    /// `COMMAND_TIMED_SYNC` 1002
    TimedSync,
    /// `COMMAND_PING` 1003
    Ping,
    /// `COMMAND_REQUEST_STAT_INFO` 1004 (debug builds only)
    RequestStatInfo,
    /// `COMMAND_REQUEST_NETWORK_STATE` 1005 (debug builds only)
    RequestNetworkState,
    /// `COMMAND_REQUEST_PEER_ID` 1006 (debug builds only)
    RequestPeerId,
    /// `NOTIFY_NEW_BLOCK` 2001
    NewBlock,
    /// `NOTIFY_NEW_TRANSACTIONS` 2002
    NewTransactions,
    /// `NOTIFY_REQUEST_GET_OBJECTS` 2003
    RequestGetObjects,
    /// `NOTIFY_RESPONSE_GET_OBJECTS` 2004
    ResponseGetObjects,
    /// `NOTIFY_REQUEST_CHAIN` 2006
    RequestChain,
    /// `NOTIFY_RESPONSE_CHAIN_ENTRY` 2007
    ResponseChainEntry,
    /// `NOTIFY_REQUEST_TX_POOL` 2008
    RequestTxPool,
    /// `NOTIFY_NEW_LITE_BLOCK` 2009
    NewLiteBlock,
    /// `NOTIFY_MISSING_TXS` 2010
    MissingTxs,
    /// Anything else.
    Unknown(u32),
}

impl From<u32> for LegacyCommand {
    fn from(value: u32) -> Self {
        match value {
            1001 => Self::Handshake,
            1002 => Self::TimedSync,
            1003 => Self::Ping,
            1004 => Self::RequestStatInfo,
            1005 => Self::RequestNetworkState,
            1006 => Self::RequestPeerId,
            2001 => Self::NewBlock,
            2002 => Self::NewTransactions,
            2003 => Self::RequestGetObjects,
            2004 => Self::ResponseGetObjects,
            2006 => Self::RequestChain,
            2007 => Self::ResponseChainEntry,
            2008 => Self::RequestTxPool,
            2009 => Self::NewLiteBlock,
            2010 => Self::MissingTxs,
            x => Self::Unknown(x),
        }
    }
}

impl From<LegacyCommand> for u32 {
    fn from(value: LegacyCommand) -> Self {
        match value {
            LegacyCommand::Handshake => 1001,
            LegacyCommand::TimedSync => 1002,
            LegacyCommand::Ping => 1003,
            LegacyCommand::RequestStatInfo => 1004,
            LegacyCommand::RequestNetworkState => 1005,
            LegacyCommand::RequestPeerId => 1006,
            LegacyCommand::NewBlock => 2001,
            LegacyCommand::NewTransactions => 2002,
            LegacyCommand::RequestGetObjects => 2003,
            LegacyCommand::ResponseGetObjects => 2004,
            LegacyCommand::RequestChain => 2006,
            LegacyCommand::ResponseChainEntry => 2007,
            LegacyCommand::RequestTxPool => 2008,
            LegacyCommand::NewLiteBlock => 2009,
            LegacyCommand::MissingTxs => 2010,
            LegacyCommand::Unknown(x) => x,
        }
    }
}

impl fmt::Display for LegacyCommand {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}({})", u32::from(*self))
    }
}

impl LevinCommand for LegacyCommand {
    fn bucket_size_limit(&self) -> u64 {
        match self {
            Self::Handshake | Self::TimedSync | Self::RequestNetworkState => bucket_limits::ADMIN,
            Self::Ping | Self::RequestStatInfo | Self::RequestPeerId => bucket_limits::SMALL,
            Self::RequestChain => bucket_limits::REQUEST_CHAIN,
            Self::ResponseChainEntry => bucket_limits::RESPONSE_CHAIN,
            Self::RequestGetObjects => bucket_limits::REQUEST_GET_OBJECTS,
            Self::NewLiteBlock | Self::MissingTxs | Self::RequestTxPool => bucket_limits::LITE,
            Self::NewBlock
            | Self::NewTransactions
            | Self::ResponseGetObjects
            | Self::Unknown(_) => bucket_limits::BULK,
        }
    }

    fn is_handshake(&self) -> bool {
        matches!(self, Self::Handshake)
    }
}

/// Admin requests (`have_to_return_data = true`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdminRequest {
    Handshake(HandshakeRequest),
    TimedSync(TimedSyncRequest),
    Ping,
    StatInfo(StatInfoRequest),
    NetworkState(NetworkStateRequest),
    PeerId,
}

impl AdminRequest {
    #[must_use]
    pub const fn command(&self) -> LegacyCommand {
        match self {
            Self::Handshake(_) => LegacyCommand::Handshake,
            Self::TimedSync(_) => LegacyCommand::TimedSync,
            Self::Ping => LegacyCommand::Ping,
            Self::StatInfo(_) => LegacyCommand::RequestStatInfo,
            Self::NetworkState(_) => LegacyCommand::RequestNetworkState,
            Self::PeerId => LegacyCommand::RequestPeerId,
        }
    }
}

/// Admin responses (`flags = RESPONSE`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdminResponse {
    Handshake(HandshakeResponse),
    TimedSync(TimedSyncResponse),
    Ping(PingResponse),
    StatInfo(StatInfoResponse),
    NetworkState(NetworkStateResponse),
    PeerId(PeerIdResponse),
}

impl AdminResponse {
    #[must_use]
    pub const fn command(&self) -> LegacyCommand {
        match self {
            Self::Handshake(_) => LegacyCommand::Handshake,
            Self::TimedSync(_) => LegacyCommand::TimedSync,
            Self::Ping(_) => LegacyCommand::Ping,
            Self::StatInfo(_) => LegacyCommand::RequestStatInfo,
            Self::NetworkState(_) => LegacyCommand::RequestNetworkState,
            Self::PeerId(_) => LegacyCommand::RequestPeerId,
        }
    }
}

/// Block-chain protocol notifications.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProtocolMessage {
    NewBlock(NewBlock),
    NewTransactions(NewTransactions),
    RequestGetObjects(RequestGetObjects),
    ResponseGetObjects(ResponseGetObjects),
    RequestChain(RequestChain),
    ResponseChainEntry(ResponseChainEntry),
    RequestTxPool(RequestTxPool),
    NewLiteBlock(NewLiteBlock),
    MissingTxs(MissingTxs),
}

impl ProtocolMessage {
    #[must_use]
    pub const fn command(&self) -> LegacyCommand {
        match self {
            Self::NewBlock(_) => LegacyCommand::NewBlock,
            Self::NewTransactions(_) => LegacyCommand::NewTransactions,
            Self::RequestGetObjects(_) => LegacyCommand::RequestGetObjects,
            Self::ResponseGetObjects(_) => LegacyCommand::ResponseGetObjects,
            Self::RequestChain(_) => LegacyCommand::RequestChain,
            Self::ResponseChainEntry(_) => LegacyCommand::ResponseChainEntry,
            Self::RequestTxPool(_) => LegacyCommand::RequestTxPool,
            Self::NewLiteBlock(_) => LegacyCommand::NewLiteBlock,
            Self::MissingTxs(_) => LegacyCommand::MissingTxs,
        }
    }

    /// Approximate encoded size in bytes (blobs and hashes plus a small allowance for keys and
    /// length prefixes), used to bound outbound queues by bytes without encoding.
    #[must_use]
    pub fn size_hint(&self) -> usize {
        const OVERHEAD: usize = 64;
        const PER_ITEM: usize = 8;
        let blobs = |v: &[Vec<u8>]| v.iter().map(|b| b.len() + PER_ITEM).sum::<usize>();
        let block = |b: &RawBlockLegacy| b.block.len() + blobs(&b.txs) + PER_ITEM;
        OVERHEAD
            + match self {
                Self::NewBlock(m) => block(&m.block),
                Self::NewTransactions(m) => blobs(&m.txs),
                Self::RequestGetObjects(m) => 32 * (m.txs.len() + m.blocks.len()),
                Self::ResponseGetObjects(m) => {
                    blobs(&m.txs)
                        + m.blocks.iter().map(block).sum::<usize>()
                        + 32 * m.missed_ids.len()
                }
                Self::RequestChain(m) => 32 * m.block_ids.len(),
                Self::ResponseChainEntry(m) => 32 * m.block_ids.len(),
                Self::RequestTxPool(m) => 32 * m.txs.len(),
                Self::NewLiteBlock(m) => m.block_template.len(),
                Self::MissingTxs(m) => 32 * m.missing_txs.len(),
            }
    }
}

/// One Levin message of the XKR protocol.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LegacyMessage {
    Request(AdminRequest),
    Response(AdminResponse),
    Notification(ProtocolMessage),
    /// Decode only: a response with an empty body. The C++ node sends this with return
    /// code `-6` for requests it has no handler for (the codec does not expose the code).
    EmptyResponse(LegacyCommand),
    /// Decode only: a request whose command we do not know. Must be answered with
    /// [`LegacyMessage::ErrorResponse`] (`-6`), as the C++ node does.
    UnknownRequest(LegacyCommand),
    /// Decode only: a notification whose command we do not know (ignored).
    UnknownNotification(LegacyCommand),
    /// Encode only: an empty-bodied response with an explicit Levin return code.
    ErrorResponse {
        command: LegacyCommand,
        return_code: i32,
    },
}

fn decode<T: EpeeObject, B: Buf>(buf: &mut B) -> Result<T, BucketError> {
    xkr_epee::from_bytes(buf).map_err(|e| BucketError::BodyDecodingError(Box::new(e)))
}

fn encode<T: EpeeObject>(val: T) -> Result<Bytes, BucketError> {
    xkr_epee::to_bytes(val)
        .map(BytesMut::freeze)
        .map_err(|e| BucketError::BodyDecodingError(Box::new(e)))
}

impl LevinBody for LegacyMessage {
    type Command = LegacyCommand;

    fn decode_message<B: Buf>(
        body: &mut B,
        ty: MessageType,
        command: LegacyCommand,
    ) -> Result<Self, BucketError> {
        use LegacyCommand as C;
        Ok(match ty {
            MessageType::Request => Self::Request(match command {
                C::Handshake => AdminRequest::Handshake(decode(body)?),
                C::TimedSync => AdminRequest::TimedSync(decode(body)?),
                C::Ping => {
                    decode::<EmptyMessage, _>(body)?;
                    AdminRequest::Ping
                }
                C::RequestStatInfo => AdminRequest::StatInfo(decode(body)?),
                C::RequestNetworkState => AdminRequest::NetworkState(decode(body)?),
                C::RequestPeerId => {
                    decode::<EmptyMessage, _>(body)?;
                    AdminRequest::PeerId
                }
                other => return Ok(Self::UnknownRequest(other)),
            }),
            MessageType::Response => {
                if !body.has_remaining() {
                    return Ok(Self::EmptyResponse(command));
                }
                Self::Response(match command {
                    C::Handshake => AdminResponse::Handshake(decode(body)?),
                    C::TimedSync => AdminResponse::TimedSync(decode(body)?),
                    C::Ping => AdminResponse::Ping(decode(body)?),
                    C::RequestStatInfo => AdminResponse::StatInfo(decode(body)?),
                    C::RequestNetworkState => AdminResponse::NetworkState(decode(body)?),
                    C::RequestPeerId => AdminResponse::PeerId(decode(body)?),
                    _ => return Err(BucketError::UnknownCommand),
                })
            }
            MessageType::Notification => Self::Notification(match command {
                C::NewBlock => ProtocolMessage::NewBlock(decode(body)?),
                C::NewTransactions => ProtocolMessage::NewTransactions(decode(body)?),
                C::RequestGetObjects => ProtocolMessage::RequestGetObjects(decode(body)?),
                C::ResponseGetObjects => ProtocolMessage::ResponseGetObjects(decode(body)?),
                C::RequestChain => ProtocolMessage::RequestChain(decode(body)?),
                C::ResponseChainEntry => ProtocolMessage::ResponseChainEntry(decode(body)?),
                C::RequestTxPool => ProtocolMessage::RequestTxPool(decode(body)?),
                C::NewLiteBlock => ProtocolMessage::NewLiteBlock(decode(body)?),
                C::MissingTxs => ProtocolMessage::MissingTxs(decode(body)?),
                other => return Ok(Self::UnknownNotification(other)),
            }),
        })
    }

    fn encode(self, builder: &mut BucketBuilder<LegacyCommand>) -> Result<(), BucketError> {
        let (ty, return_code, command, body) = match self {
            Self::Request(req) => {
                let command = req.command();
                let body = match req {
                    AdminRequest::Handshake(v) => encode(v)?,
                    AdminRequest::TimedSync(v) => encode(v)?,
                    AdminRequest::Ping | AdminRequest::PeerId => encode(EmptyMessage)?,
                    AdminRequest::StatInfo(v) => encode(v)?,
                    AdminRequest::NetworkState(v) => encode(v)?,
                };
                (MessageType::Request, 0, command, body)
            }
            Self::Response(res) => {
                let command = res.command();
                let body = match res {
                    AdminResponse::Handshake(v) => encode(v)?,
                    AdminResponse::TimedSync(v) => encode(v)?,
                    AdminResponse::Ping(v) => encode(v)?,
                    AdminResponse::StatInfo(v) => encode(v)?,
                    AdminResponse::NetworkState(v) => encode(v)?,
                    AdminResponse::PeerId(v) => encode(v)?,
                };
                (MessageType::Response, LEVIN_RETCODE_SUCCESS, command, body)
            }
            Self::Notification(msg) => {
                let command = msg.command();
                let body = match msg {
                    ProtocolMessage::NewBlock(v) => encode(v)?,
                    ProtocolMessage::NewTransactions(v) => encode(v)?,
                    ProtocolMessage::RequestGetObjects(v) => encode(v)?,
                    ProtocolMessage::ResponseGetObjects(v) => encode(v)?,
                    ProtocolMessage::RequestChain(v) => encode(v)?,
                    ProtocolMessage::ResponseChainEntry(v) => encode(v)?,
                    ProtocolMessage::RequestTxPool(v) => encode(v)?,
                    ProtocolMessage::NewLiteBlock(v) => encode(v)?,
                    ProtocolMessage::MissingTxs(v) => encode(v)?,
                };
                (MessageType::Notification, 0, command, body)
            }
            Self::ErrorResponse {
                command,
                return_code,
            } => (MessageType::Response, return_code, command, Bytes::new()),
            Self::EmptyResponse(_) | Self::UnknownRequest(_) | Self::UnknownNotification(_) => {
                return Err(BucketError::UnknownCommand);
            }
        };
        builder.set_message_type(ty);
        builder.set_return_code(return_code);
        builder.set_command(command);
        builder.set_body(body);
        Ok(())
    }
}

#[cfg(test)]
mod tests;
