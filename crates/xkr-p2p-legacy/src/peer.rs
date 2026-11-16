//! Tokio connection layer for C++ (Levin) peers.
//!
//! Life cycle of one connection:
//!
//! 1. [`LegacyPeer::connect`] (outbound) or [`LegacyPeer::accept`] (inbound) runs the
//!    `COMMAND_HANDSHAKE` exchange and validates the remote (`NodeServer::handshake` /
//!    `handle_handshake`). An inbound connection may instead be a C++ *back-ping*
//!    (`try_ping`: a fresh connection carrying one `COMMAND_PING`), answered and closed.
//! 2. [`LegacyPeer::spawn`] starts one task per connection that
//!    - answers `COMMAND_TIMED_SYNC` / `COMMAND_PING` requests itself,
//!    - replies `-6` (`ERROR_CONNECTION_HANDLER_NOT_DEFINED`) to anything else, like C++,
//!    - sends a `COMMAND_TIMED_SYNC` every [`LegacyConfig::timed_sync_interval`] (60 s),
//!    - correlates admin responses with requests (Levin has no request id; the C++ node
//!      answers in order, so the oldest pending request with the same command wins),
//!    - drops the peer if a response does not arrive within the invoke timeout,
//!    - forwards protocol notifications and sync data to the node as [`PeerEvent`]s.
//!
//! Shared state is not locked: the task reads our `CORE_SYNC_DATA` and the peer-list head
//! from `watch` channels that the node's single owner task publishes.

use std::{
    collections::VecDeque,
    net::SocketAddr,
    sync::{
        Arc,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use futures::{
    SinkExt, StreamExt,
    stream::{SplitSink, SplitStream},
};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    net::TcpStream,
    sync::{
        OwnedSemaphorePermit, Semaphore,
        mpsc::{self, error::TrySendError},
        oneshot, watch,
    },
    task::JoinHandle,
    time::{Instant, MissedTickBehavior, interval, interval_at, timeout},
};
use tokio_util::codec::Framed;
use xkr_consensus::params::Network;
use xkr_levin::{BucketError, LevinMessage, LevinMessageCodec, Protocol};

use crate::{
    constants::{
        HANDSHAKE_TIMEOUT, INBOUND_HANDSHAKE_TIMEOUT, LEVIN_DEFAULT_MAX_PACKET_SIZE,
        LEVIN_ERROR_HANDLER_NOT_DEFINED, LEVIN_PROTOCOL_VERSION, LEVIN_SIGNATURE,
        MAX_LARGE_READERS, P2P_CONNECTION_MAX_WRITE_BUFFER_SIZE, P2P_CURRENT_VERSION,
        P2P_DEFAULT_CONNECTION_TIMEOUT, P2P_DEFAULT_HANDSHAKE_INTERVAL, P2P_DEFAULT_INVOKE_TIMEOUT,
        P2P_DEFAULT_PEERS_IN_HANDSHAKE, P2P_IDLE_TIMEOUT, P2P_LOCAL_GRAY_PEERLIST_LIMIT,
        P2P_LOCAL_WHITE_PEERLIST_LIMIT, P2P_MINIMUM_VERSION, PING_OK_RESPONSE_STATUS_TEXT,
        SOLICITED_MAX_PACKET_SIZE, UNSOLICITED_MAX_PACKET_SIZE,
    },
    network::network_id,
    peerlist::PeerList,
    ratelimit::{RateLimiter, Verdict},
    wire::{
        AdminRequest, AdminResponse, BasicNodeData, CoreSyncData, HandshakeRequest,
        HandshakeResponse, LegacyCommand, LegacyMessage, PeerlistEntry, PingResponse,
        ProtocolMessage, TimedSyncRequest, TimedSyncResponse,
    },
};

/// Any byte stream the Levin protocol can run over: a TCP socket, or (phase 1 of the
/// Hyperswarm transport, see `docs/node-architecture.md`) an encrypted Hyperswarm stream
/// exposed by `xkr-p2p-hyper` as `AsyncRead + AsyncWrite`.
pub trait PeerStream: AsyncRead + AsyncWrite + Unpin + Send + 'static {}

impl<T: AsyncRead + AsyncWrite + Unpin + Send + 'static> PeerStream for T {}

/// A type-erased [`PeerStream`]; connections are not generic so the node can keep TCP and
/// Hyperswarm peers in one table.
pub type BoxedPeerStream = Box<dyn PeerStream>;

type LegacyFramed = Framed<BoxedPeerStream, LevinMessageCodec<LegacyMessage>>;
type LegacySink = SplitSink<LegacyFramed, LevinMessage<LegacyMessage>>;
type LegacyStream = SplitStream<LegacyFramed>;

/// Connection settings; defaults are the C++ constants.
#[derive(Debug, Clone)]
pub struct LegacyConfig {
    /// `basic_node_data.network_id`
    pub network_id: [u8; 16],
    /// Our random 64-bit peer id (`m_config.m_peer_id`).
    pub peer_id: u64,
    /// Port we announce (`my_port`); 0 = hidden (`--hide-my-port`), C++ then never
    /// back-pings or whitelists us.
    pub my_port: u32,
    /// TCP connect timeout (`P2P_DEFAULT_CONNECTION_TIMEOUT`, 5 s).
    pub connect_timeout: Duration,
    /// Handshake timeout (`connection_timeout * 3` = 15 s in C++).
    pub handshake_timeout: Duration,
    /// Whole back-ping budget (`connection_timeout * 2` = 10 s in C++ `try_ping`).
    pub ping_timeout: Duration,
    /// Request/response timeout (`P2P_DEFAULT_INVOKE_TIMEOUT`, 2 min).
    pub invoke_timeout: Duration,
    /// Drop the connection when nothing was received for this long ([`P2P_IDLE_TIMEOUT`],
    /// 5 min). Both sides send a timed sync every minute, so a healthy peer never hits it.
    pub idle_timeout: Duration,
    /// Timed sync period (`P2P_DEFAULT_HANDSHAKE_INTERVAL`, 60 s).
    pub timed_sync_interval: Duration,
    /// Max Levin body (`LEVIN_DEFAULT_MAX_PACKET_SIZE`, 100 MB).
    pub max_packet_size: u64,
    /// Max body before a handshake was seen (C++ has none; 256 KiB like Monero/Cuprate).
    pub max_packet_size_before_handshake: u64,
    /// Peer-list entries we send in handshake/timed-sync responses (250).
    pub peers_in_handshake: usize,
    /// `P2P_LOCAL_WHITE_PEERLIST_LIMIT`
    pub white_list_limit: usize,
    /// `P2P_LOCAL_GRAY_PEERLIST_LIMIT`
    pub gray_list_limit: usize,
    /// `--allow-local-ip`
    pub allow_local_ip: bool,
    /// Outbound queue length in messages ([`PeerError::WriteBufferFull`] beyond it).
    pub outbound_queue: usize,
    /// Outbound queue bound in bytes, like the C++ write queue
    /// (`P2P_CONNECTION_MAX_WRITE_BUFFER_SIZE`, 32 MiB): a notification that would take the
    /// queued bytes above it fails with [`PeerError::WriteBufferFull`] (the node then drops
    /// the peer, as C++ interrupts the connection).
    pub outbound_queue_bytes: usize,
    /// Inbound connections must send their first message (handshake or back-ping) this soon
    /// ([`INBOUND_HANDSHAKE_TIMEOUT`]).
    pub inbound_handshake_timeout: Duration,
    /// Read limit for bulk messages we did not ask for
    /// ([`crate::constants::UNSOLICITED_MAX_PACKET_SIZE`]); exceeding it closes the connection
    /// without a ban.
    pub unsolicited_max_packet_size: u64,
    /// Read limit while one of our requests is outstanding
    /// ([`crate::constants::SOLICITED_MAX_PACKET_SIZE`]), for connections holding one of
    /// `large_readers`.
    pub solicited_max_packet_size: u64,
    /// Node-wide permits for connections reading with `solicited_max_packet_size`
    /// ([`crate::constants::MAX_LARGE_READERS`]); clones of a config share them.
    pub large_readers: Arc<Semaphore>,
}

impl LegacyConfig {
    /// Defaults for `network` with our `peer_id` and announced port.
    #[must_use]
    pub fn new(network: Network, peer_id: u64, my_port: u32) -> Self {
        Self {
            network_id: network_id(network),
            peer_id,
            my_port,
            connect_timeout: P2P_DEFAULT_CONNECTION_TIMEOUT,
            handshake_timeout: HANDSHAKE_TIMEOUT,
            ping_timeout: P2P_DEFAULT_CONNECTION_TIMEOUT * 2,
            invoke_timeout: P2P_DEFAULT_INVOKE_TIMEOUT,
            idle_timeout: P2P_IDLE_TIMEOUT,
            timed_sync_interval: P2P_DEFAULT_HANDSHAKE_INTERVAL,
            max_packet_size: LEVIN_DEFAULT_MAX_PACKET_SIZE,
            max_packet_size_before_handshake: 256 * 1024,
            peers_in_handshake: P2P_DEFAULT_PEERS_IN_HANDSHAKE,
            white_list_limit: P2P_LOCAL_WHITE_PEERLIST_LIMIT,
            gray_list_limit: P2P_LOCAL_GRAY_PEERLIST_LIMIT,
            allow_local_ip: false,
            outbound_queue: 1024,
            outbound_queue_bytes: P2P_CONNECTION_MAX_WRITE_BUFFER_SIZE,
            inbound_handshake_timeout: INBOUND_HANDSHAKE_TIMEOUT,
            unsolicited_max_packet_size: UNSOLICITED_MAX_PACKET_SIZE,
            solicited_max_packet_size: SOLICITED_MAX_PACKET_SIZE,
            large_readers: Arc::new(Semaphore::new(MAX_LARGE_READERS)),
        }
    }

    /// A Levin codec with this config's limits and `read_limit` as its per-connection read
    /// limit (see [`ReadGate`]).
    fn codec(&self, read_limit: &Arc<AtomicU64>) -> LevinMessageCodec<LegacyMessage> {
        LevinMessageCodec::new(self.protocol()).with_read_limit(read_limit.clone())
    }

    /// Levin settings for the codec.
    #[must_use]
    pub fn protocol(&self) -> Protocol {
        Protocol {
            version: LEVIN_PROTOCOL_VERSION,
            signature: LEVIN_SIGNATURE,
            max_packet_size_before_handshake: self.max_packet_size_before_handshake,
            max_packet_size: self.max_packet_size,
        }
    }

    /// An empty peer list with this config's limits.
    #[must_use]
    pub fn peer_list(&self) -> PeerList {
        let mut pl = PeerList::new(self.allow_local_ip);
        pl.white_limit = self.white_list_limit;
        pl.gray_limit = self.gray_list_limit;
        pl
    }

    /// `NodeServer::get_local_node_data`.
    #[must_use]
    pub fn local_node_data(&self, now: u64) -> BasicNodeData {
        BasicNodeData {
            network_id: self.network_id,
            version: P2P_CURRENT_VERSION,
            peer_id: self.peer_id,
            local_time: now,
            my_port: self.my_port,
        }
    }
}

/// Node state a connection needs to answer requests, published by the node.
#[derive(Debug, Clone)]
pub struct LocalState {
    /// Our `CORE_SYNC_DATA` (height = top index + 1, top hash).
    pub core_sync: watch::Receiver<CoreSyncData>,
    /// `PeerList::head(peers_in_handshake)` snapshot.
    pub peerlist_head: watch::Receiver<Arc<[PeerlistEntry]>>,
}

impl LocalState {
    fn sync_data(&self) -> CoreSyncData {
        self.core_sync.borrow().clone()
    }

    fn peerlist(&self) -> Vec<PeerlistEntry> {
        self.peerlist_head.borrow().to_vec()
    }
}

/// What we learned about the remote in the handshake.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerInfo {
    pub addr: SocketAddr,
    pub inbound: bool,
    pub peer_id: u64,
    /// P2P version (lite blocks from 4).
    pub version: u8,
    /// Announced listening port (0 = hidden).
    pub my_port: u32,
    pub core_sync: CoreSyncData,
}

/// Connection errors.
#[derive(Debug, thiserror::Error)]
pub enum PeerError {
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),
    #[error("levin error: {0}")]
    Levin(#[from] BucketError),
    #[error("connect timed out")]
    ConnectTimeout,
    #[error("handshake timed out")]
    HandshakeTimeout,
    #[error("no response to {0} within the invoke timeout")]
    RequestTimeout(LegacyCommand),
    #[error("peer is on another network ({0:02x?})")]
    WrongNetwork([u8; 16]),
    #[error("peer P2P version {0} is below the minimum")]
    VersionTooOld(u8),
    #[error("connected to ourselves")]
    SelfConnection,
    #[error("unexpected message: {0}")]
    Unexpected(&'static str),
    #[error("unsolicited response for {0}")]
    UnexpectedResponse(LegacyCommand),
    #[error("peer has no handler for {0}")]
    NotHandled(LegacyCommand),
    #[error("bad ping response")]
    BadPing,
    #[error("outbound queue full")]
    WriteBufferFull,
    #[error("nothing received within the idle timeout")]
    Idle,
    #[error("peer did not take a message within the invoke timeout")]
    WriteTimeout,
    #[error("peer exceeded the message rate limits")]
    RateLimited,
    #[error("connection closed")]
    Closed,
}

impl PeerError {
    /// Whether the remote broke the protocol (malformed or oversized Levin bucket, a body
    /// that does not decode, an unsolicited response, a second handshake). The node bans such
    /// peers; plain I/O errors, timeouts and closes are not violations.
    #[must_use]
    pub fn is_protocol_violation(&self) -> bool {
        match self {
            // Above the per-connection read limit (something we did not ask for is larger than
            // any regular unsolicited message): closed, not banned.
            Self::Levin(e) => {
                !matches!(e, BucketError::IO(_) | BucketError::BucketExceededReadLimit)
            }
            Self::Unexpected(_)
            | Self::UnexpectedResponse(_)
            | Self::BadPing
            | Self::RateLimited => true,
            Self::Io(_)
            | Self::ConnectTimeout
            | Self::HandshakeTimeout
            | Self::RequestTimeout(_)
            | Self::WrongNetwork(_)
            | Self::VersionTooOld(_)
            | Self::SelfConnection
            | Self::NotHandled(_)
            | Self::WriteBufferFull
            | Self::Idle
            | Self::WriteTimeout
            | Self::Closed => false,
        }
    }
}

/// Something a connection reports to the node.
#[derive(Debug, Clone)]
pub struct PeerEvent {
    pub conn_id: u64,
    pub kind: PeerEventKind,
}

#[derive(Debug, Clone)]
pub enum PeerEventKind {
    /// Peer's `CORE_SYNC_DATA` from a timed sync (feed to `sync::SyncInput::TimedSync`).
    CoreSync(CoreSyncData),
    /// A timed-sync peer list; merge with `PeerList::merge_remote(entries, remote_time, now)`
    /// and disconnect on error (C++ `handle_remote_peerlist` failure).
    RemotePeerList {
        entries: Vec<PeerlistEntry>,
        remote_time: u64,
    },
    /// A block-chain notification (feed to `sync::SyncInput::Message`).
    Protocol(ProtocolMessage),
    /// The remote broke the protocol ([`PeerError::is_protocol_violation`]); sent right
    /// before the final [`PeerEventKind::Closed`] so the node can ban the address.
    Misbehaved(String),
    /// The connection ended (with the error text, if any).
    Closed(Option<String>),
}

/// A handshaken connection, not yet running.
pub struct LegacyPeer {
    framed: LegacyFramed,
    /// The codec's read limit, adjusted by the connection task ([`ReadGate`]).
    read_limit: Arc<AtomicU64>,
    pub info: PeerInfo,
}

impl std::fmt::Debug for LegacyPeer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LegacyPeer")
            .field("info", &self.info)
            .finish_non_exhaustive()
    }
}

/// Result of a successful handshake.
#[derive(Debug)]
pub struct Handshaked {
    pub peer: LegacyPeer,
    /// The remote's peer list (outbound only; requests carry none).
    pub remote_peerlist: Vec<PeerlistEntry>,
    /// The remote's `local_time`, for `PeerList::merge_remote`.
    pub remote_time: u64,
}

/// Result of [`LegacyPeer::accept`].
#[derive(Debug)]
pub enum Accepted {
    /// Boxed: a [`Handshaked`] is a few hundred bytes, `Pinged` carries nothing.
    Peer(Box<Handshaked>),
    /// The remote only back-pinged us; the connection is done.
    Pinged,
}

#[derive(Debug)]
enum Outbound {
    /// A notification and the bytes it counts in the queue.
    Notify(ProtocolMessage, usize),
    Request(
        AdminRequest,
        oneshot::Sender<Result<AdminResponse, PeerError>>,
    ),
    Close,
}

/// Cheap handle to a running connection.
#[derive(Debug, Clone)]
pub struct PeerHandle {
    pub conn_id: u64,
    tx: mpsc::Sender<Outbound>,
    /// Bytes of notifications queued and not yet written.
    queued: Arc<AtomicUsize>,
    max_queued: usize,
}

impl PeerHandle {
    /// Queues a notification without waiting. Fails when the queue is full, in messages or in
    /// bytes ([`LegacyConfig::outbound_queue_bytes`], the C++ write-queue limit; the node should
    /// then drop the peer, like C++ on write-queue overflow) or closed.
    pub fn notify(&self, msg: ProtocolMessage) -> Result<(), PeerError> {
        let size = msg.size_hint();
        let before = self.queued.fetch_add(size, Ordering::Relaxed);
        if before.saturating_add(size) > self.max_queued {
            self.queued.fetch_sub(size, Ordering::Relaxed);
            return Err(PeerError::WriteBufferFull);
        }
        self.tx.try_send(Outbound::Notify(msg, size)).map_err(|e| {
            self.queued.fetch_sub(size, Ordering::Relaxed);
            match e {
                TrySendError::Full(_) => PeerError::WriteBufferFull,
                TrySendError::Closed(_) => PeerError::Closed,
            }
        })
    }

    /// Bytes of notifications queued for this peer and not yet written.
    #[must_use]
    pub fn queued_bytes(&self) -> usize {
        self.queued.load(Ordering::Relaxed)
    }

    /// Sends an admin request and waits for its response (or the invoke timeout).
    pub async fn request(&self, req: AdminRequest) -> Result<AdminResponse, PeerError> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(Outbound::Request(req, tx))
            .await
            .map_err(|_| PeerError::Closed)?;
        rx.await.map_err(|_| PeerError::Closed)?
    }

    /// Asks the connection task to close.
    pub fn close(&self) {
        let _ = self.tx.try_send(Outbound::Close);
    }

    /// Whether the connection task is gone.
    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.tx.is_closed()
    }
}

/// Unix time in seconds.
#[must_use]
pub fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn check_remote(cfg: &LegacyConfig, node: &BasicNodeData) -> Result<(), PeerError> {
    if node.network_id != cfg.network_id {
        return Err(PeerError::WrongNetwork(node.network_id));
    }
    if node.version < P2P_MINIMUM_VERSION {
        return Err(PeerError::VersionTooOld(node.version));
    }
    if node.peer_id == cfg.peer_id {
        return Err(PeerError::SelfConnection);
    }
    Ok(())
}

/// Writes one message. A peer that does not take it within [`P2P_DEFAULT_INVOKE_TIMEOUT`] (the
/// time it would wait for our answer anyway) fails the connection: a stalled write would
/// otherwise block the connection loop, its idle and request deadlines and `Close`, forever.
async fn send(sink: &mut LegacySink, msg: LegacyMessage) -> Result<(), PeerError> {
    timeout(
        P2P_DEFAULT_INVOKE_TIMEOUT,
        sink.send(LevinMessage::Body(msg)),
    )
    .await
    .map_err(|_| PeerError::WriteTimeout)??;
    Ok(())
}

impl LegacyPeer {
    /// Outbound connection + `COMMAND_HANDSHAKE` (C++ `try_to_connect_and_handshake_with_new_peer`
    /// and `handshake`).
    pub async fn connect(
        addr: SocketAddr,
        cfg: &LegacyConfig,
        local: &LocalState,
    ) -> Result<Handshaked, PeerError> {
        let stream = timeout(cfg.connect_timeout, TcpStream::connect(addr))
            .await
            .map_err(|_| PeerError::ConnectTimeout)??;
        stream.set_nodelay(true)?;
        Self::connect_stream(stream, addr, cfg, local).await
    }

    /// Outbound `COMMAND_HANDSHAKE` over an already connected stream of any transport
    /// (e.g. a Hyperswarm connection). `addr` is only used for bookkeeping ([`PeerInfo::addr`]);
    /// transports without IP addresses pass a synthetic one.
    pub async fn connect_stream<S: PeerStream>(
        stream: S,
        addr: SocketAddr,
        cfg: &LegacyConfig,
        local: &LocalState,
    ) -> Result<Handshaked, PeerError> {
        let boxed: BoxedPeerStream = Box::new(stream);
        let read_limit = Arc::new(AtomicU64::new(cfg.unsolicited_max_packet_size));
        let mut framed = Framed::new(boxed, cfg.codec(&read_limit));

        let req = HandshakeRequest {
            node_data: cfg.local_node_data(unix_now()),
            payload_data: local.sync_data(),
        };
        framed
            .send(LevinMessage::Body(LegacyMessage::Request(
                AdminRequest::Handshake(req),
            )))
            .await?;

        // C++ `LevinProtocol::invoke` reads exactly one command and requires it to be the
        // response, so anything else first is a protocol error.
        let resp: HandshakeResponse = match timeout(cfg.handshake_timeout, framed.next())
            .await
            .map_err(|_| PeerError::HandshakeTimeout)?
        {
            None => return Err(PeerError::Closed),
            Some(Err(e)) => return Err(e.into()),
            Some(Ok(LegacyMessage::Response(AdminResponse::Handshake(r)))) => r,
            Some(Ok(_)) => return Err(PeerError::Unexpected("expected a handshake response")),
        };
        check_remote(cfg, &resp.node_data)?;

        Ok(Handshaked {
            peer: Self {
                framed,
                read_limit,
                info: PeerInfo {
                    addr,
                    inbound: false,
                    peer_id: resp.node_data.peer_id,
                    version: resp.node_data.version,
                    my_port: resp.node_data.my_port,
                    core_sync: resp.payload_data,
                },
            },
            remote_peerlist: resp.local_peerlist,
            remote_time: resp.node_data.local_time,
        })
    }

    /// Inbound connection: answer a handshake (C++ `handle_handshake`) or a back-ping.
    ///
    /// Unlike C++ we do not back-ping the remote's `my_port` before answering (the remote
    /// would wait for it); the node calls [`ping`] afterwards in the background and
    /// whitelists the address on success (`xkrd::p2p`).
    pub async fn accept(
        stream: TcpStream,
        addr: SocketAddr,
        cfg: &LegacyConfig,
        local: &LocalState,
    ) -> Result<Accepted, PeerError> {
        stream.set_nodelay(true)?;
        Self::accept_stream(stream, addr, cfg, local).await
    }

    /// Inbound handshake (or back-ping) over an already accepted stream of any transport.
    /// See [`Self::connect_stream`] for `addr`.
    pub async fn accept_stream<S: PeerStream>(
        stream: S,
        addr: SocketAddr,
        cfg: &LegacyConfig,
        local: &LocalState,
    ) -> Result<Accepted, PeerError> {
        let boxed: BoxedPeerStream = Box::new(stream);
        let read_limit = Arc::new(AtomicU64::new(cfg.unsolicited_max_packet_size));
        let mut framed = Framed::new(boxed, cfg.codec(&read_limit));

        // An inbound connection must speak at once (C++ sends its handshake or back-ping
        // right after connecting), so a silent one holds a pending slot only briefly.
        let first = timeout(cfg.inbound_handshake_timeout, framed.next())
            .await
            .map_err(|_| PeerError::HandshakeTimeout)?;
        let req = match first {
            None => return Err(PeerError::Closed),
            Some(Err(e)) => return Err(e.into()),
            Some(Ok(LegacyMessage::Request(AdminRequest::Ping))) => {
                framed
                    .send(LevinMessage::Body(LegacyMessage::Response(
                        AdminResponse::Ping(PingResponse {
                            status: PING_OK_RESPONSE_STATUS_TEXT.to_owned(),
                            peer_id: cfg.peer_id,
                        }),
                    )))
                    .await?;
                return Ok(Accepted::Pinged);
            }
            Some(Ok(LegacyMessage::Request(AdminRequest::Handshake(r)))) => r,
            Some(Ok(_)) => return Err(PeerError::Unexpected("expected a handshake request")),
        };
        check_remote(cfg, &req.node_data)?;

        let resp = HandshakeResponse {
            node_data: cfg.local_node_data(unix_now()),
            payload_data: local.sync_data(),
            local_peerlist: local.peerlist(),
        };
        framed
            .send(LevinMessage::Body(LegacyMessage::Response(
                AdminResponse::Handshake(resp),
            )))
            .await?;

        Ok(Accepted::Peer(Box::new(Handshaked {
            peer: Self {
                framed,
                read_limit,
                info: PeerInfo {
                    addr,
                    inbound: true,
                    peer_id: req.node_data.peer_id,
                    version: req.node_data.version,
                    my_port: req.node_data.my_port,
                    core_sync: req.payload_data,
                },
            },
            remote_peerlist: Vec::new(),
            remote_time: req.node_data.local_time,
        })))
    }

    /// Starts the connection task. `events` receives everything the node must act on;
    /// a final [`PeerEventKind::Closed`] is always sent.
    #[must_use]
    pub fn spawn(
        self,
        cfg: LegacyConfig,
        local: LocalState,
        conn_id: u64,
        events: mpsc::Sender<PeerEvent>,
    ) -> (PeerHandle, JoinHandle<Result<(), PeerError>>) {
        let (tx, rx) = mpsc::channel(cfg.outbound_queue.max(1));
        let queued = Arc::new(AtomicUsize::new(0));
        let handle = PeerHandle {
            conn_id,
            tx,
            queued: queued.clone(),
            max_queued: cfg.outbound_queue_bytes,
        };
        let task = tokio::spawn(self.run(cfg, local, conn_id, events, rx, queued));
        (handle, task)
    }

    async fn run(
        self,
        cfg: LegacyConfig,
        local: LocalState,
        conn_id: u64,
        events: mpsc::Sender<PeerEvent>,
        mut rx: mpsc::Receiver<Outbound>,
        queued: Arc<AtomicUsize>,
    ) -> Result<(), PeerError> {
        let (mut sink, mut stream) = self.framed.split();
        let mut conn = ConnState {
            pending: VecDeque::new(),
            gate: ReadGate::new(&cfg, self.read_limit),
            limiter: RateLimiter::new(std::time::Instant::now()),
            queued,
        };

        let result = drive(
            &cfg,
            &local,
            conn_id,
            &events,
            &mut rx,
            &mut sink,
            &mut stream,
            &mut conn,
        )
        .await;

        for p in conn.pending {
            if let Some(reply) = p.reply {
                let _ = reply.send(Err(PeerError::Closed));
            }
        }
        if let Err(e) = &result
            && e.is_protocol_violation()
        {
            let _ = events
                .send(PeerEvent {
                    conn_id,
                    kind: PeerEventKind::Misbehaved(e.to_string()),
                })
                .await;
        }
        let _ = events
            .send(PeerEvent {
                conn_id,
                kind: PeerEventKind::Closed(result.as_ref().err().map(ToString::to_string)),
            })
            .await;
        result
    }
}

/// Per-connection state of the running task.
#[derive(Debug)]
struct ConnState {
    /// Admin requests waiting for their response.
    pending: VecDeque<Pending>,
    /// The read limit.
    gate: ReadGate,
    /// Incoming message rates.
    limiter: RateLimiter,
    /// Bytes queued by [`PeerHandle::notify`], given back as they are written.
    queued: Arc<AtomicUsize>,
}

/// The per-connection read limit. Bulk messages we did not ask for may be at most
/// `unsolicited_max_packet_size` (8 MiB). While one of our requests whose answer can be large is
/// outstanding (`NOTIFY_REQUEST_GET_OBJECTS`, `NOTIFY_REQUEST_TX_POOL`, `NOTIFY_MISSING_TXS`) and a
/// node-wide `large_readers` permit is free, the limit is `solicited_max_packet_size` (32 MiB, the
/// most any C++ node sends). The codec reads the limit when a bucket header arrives.
#[derive(Debug)]
struct ReadGate {
    limit: Arc<AtomicU64>,
    small: u64,
    large: u64,
    permits: Arc<Semaphore>,
    permit: Option<OwnedSemaphorePermit>,
    /// Outstanding `NOTIFY_REQUEST_GET_OBJECTS`.
    objects: u32,
    /// Outstanding `NOTIFY_REQUEST_TX_POOL` / `NOTIFY_MISSING_TXS` (answered with
    /// `NOTIFY_NEW_TRANSACTIONS`, or not at all when the peer's pool is empty).
    txs: u32,
    /// Outstanding block requests are forgotten after the invoke timeout ...
    objects_until: Option<Instant>,
    /// ... transaction requests after [`TX_ANSWER_WINDOW`] (an empty pool is never answered,
    /// and the permit should not wait two minutes for that).
    txs_until: Option<Instant>,
    timeout: Duration,
}

/// How long we wait for the `NOTIFY_NEW_TRANSACTIONS` answering our `NOTIFY_REQUEST_TX_POOL` /
/// `NOTIFY_MISSING_TXS` with the solicited read limit.
const TX_ANSWER_WINDOW: Duration = Duration::from_secs(30);

impl ReadGate {
    fn new(cfg: &LegacyConfig, limit: Arc<AtomicU64>) -> Self {
        let small = cfg.unsolicited_max_packet_size;
        limit.store(small, Ordering::Relaxed);
        Self {
            limit,
            small,
            large: cfg.solicited_max_packet_size.max(small),
            permits: cfg.large_readers.clone(),
            permit: None,
            objects: 0,
            txs: 0,
            objects_until: None,
            txs_until: None,
            timeout: cfg.invoke_timeout,
        }
    }

    /// We are about to send `msg`.
    fn on_send(&mut self, msg: &ProtocolMessage, now: Instant) {
        match msg {
            ProtocolMessage::RequestGetObjects(_) => {
                self.objects = self.objects.saturating_add(1);
                self.objects_until = Some(now + self.timeout);
            }
            ProtocolMessage::RequestTxPool(_) | ProtocolMessage::MissingTxs(_) => {
                self.txs = self.txs.saturating_add(1);
                self.txs_until = Some(now + TX_ANSWER_WINDOW.min(self.timeout));
            }
            _ => return,
        }
        if self.permit.is_none() {
            self.permit = self.permits.clone().try_acquire_owned().ok();
        }
        if self.permit.is_some() {
            self.limit.store(self.large, Ordering::Relaxed);
        }
    }

    /// Whether `msg` answers one of our outstanding requests (which it then consumes).
    fn on_receive(&mut self, msg: &ProtocolMessage) -> bool {
        let solicited = match msg {
            ProtocolMessage::ResponseGetObjects(_) if self.objects > 0 => {
                self.objects -= 1;
                true
            }
            ProtocolMessage::NewTransactions(_) if self.txs > 0 => {
                self.txs -= 1;
                true
            }
            _ => false,
        };
        if solicited && self.objects == 0 && self.txs == 0 {
            self.disarm();
        }
        solicited
    }

    /// Forgets requests that were not answered in time.
    fn expire(&mut self, now: Instant) {
        if self.objects_until.is_some_and(|u| now >= u) {
            self.objects = 0;
            self.objects_until = None;
        }
        if self.txs_until.is_some_and(|u| now >= u) {
            self.txs = 0;
            self.txs_until = None;
        }
        if self.objects == 0 && self.txs == 0 && self.permit.is_some() {
            self.disarm();
        }
    }

    fn disarm(&mut self) {
        self.objects_until = None;
        self.txs_until = None;
        self.permit = None;
        self.limit.store(self.small, Ordering::Relaxed);
    }
}

/// An admin request waiting for its response.
#[derive(Debug)]
struct Pending {
    command: LegacyCommand,
    deadline: Instant,
    reply: Option<oneshot::Sender<Result<AdminResponse, PeerError>>>,
}

#[allow(clippy::too_many_arguments)]
async fn drive(
    cfg: &LegacyConfig,
    local: &LocalState,
    conn_id: u64,
    events: &mpsc::Sender<PeerEvent>,
    rx: &mut mpsc::Receiver<Outbound>,
    sink: &mut LegacySink,
    stream: &mut LegacyStream,
    conn: &mut ConnState,
) -> Result<(), PeerError> {
    let mut timed_sync = interval_at(
        Instant::now() + cfg.timed_sync_interval,
        cfg.timed_sync_interval,
    );
    timed_sync.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut deadline_check = interval(Duration::from_secs(1));
    deadline_check.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut last_received = Instant::now();

    loop {
        tokio::select! {
            incoming = stream.next() => {
                let msg = match incoming {
                    None => return Ok(()),
                    Some(Err(e)) => return Err(e.into()),
                    Some(Ok(m)) => m,
                };
                last_received = Instant::now();
                handle_incoming(cfg, local, conn_id, events, sink, conn, msg).await?;
            }
            outbound = rx.recv() => match outbound {
                None | Some(Outbound::Close) => return Ok(()),
                Some(Outbound::Notify(msg, size)) => {
                    conn.gate.on_send(&msg, Instant::now());
                    let sent = send(sink, LegacyMessage::Notification(msg)).await;
                    conn.queued.fetch_sub(size, Ordering::Relaxed);
                    sent?;
                }
                Some(Outbound::Request(req, reply)) => {
                    let command = req.command();
                    send(sink, LegacyMessage::Request(req)).await?;
                    conn.pending.push_back(Pending {
                        command,
                        deadline: Instant::now() + cfg.invoke_timeout,
                        reply: Some(reply),
                    });
                }
            },
            _ = timed_sync.tick() => {
                // NodeServer::timedSync
                let req = TimedSyncRequest { payload_data: local.sync_data() };
                send(sink, LegacyMessage::Request(AdminRequest::TimedSync(req))).await?;
                conn.pending.push_back(Pending {
                    command: LegacyCommand::TimedSync,
                    deadline: Instant::now() + cfg.invoke_timeout,
                    reply: None,
                });
            }
            _ = deadline_check.tick() => {
                let now = Instant::now();
                if let Some(p) = conn.pending.iter().find(|p| p.deadline <= now) {
                    return Err(PeerError::RequestTimeout(p.command));
                }
                conn.gate.expire(now);
                if now.duration_since(last_received) >= cfg.idle_timeout {
                    return Err(PeerError::Idle);
                }
            }
        }
    }
}

async fn emit(
    events: &mpsc::Sender<PeerEvent>,
    conn_id: u64,
    kind: PeerEventKind,
) -> Result<(), PeerError> {
    events
        .send(PeerEvent { conn_id, kind })
        .await
        .map_err(|_| PeerError::Closed)
}

/// Applies a rate-limit verdict: `Ok(true)` = process the message, `Ok(false)` = drop it.
fn admit(verdict: Verdict) -> Result<bool, PeerError> {
    match verdict {
        Verdict::Allow => Ok(true),
        Verdict::Drop => Ok(false),
        Verdict::Violation => Err(PeerError::RateLimited),
    }
}

async fn handle_incoming(
    cfg: &LegacyConfig,
    local: &LocalState,
    conn_id: u64,
    events: &mpsc::Sender<PeerEvent>,
    sink: &mut LegacySink,
    conn: &mut ConnState,
    msg: LegacyMessage,
) -> Result<(), PeerError> {
    let now = std::time::Instant::now();
    let pending = &mut conn.pending;
    match msg {
        LegacyMessage::Request(AdminRequest::TimedSync(req)) => {
            if !admit(conn.limiter.request(LegacyCommand::TimedSync, now))? {
                return Ok(());
            }
            // NodeServer::handle_timed_sync
            let resp = TimedSyncResponse {
                local_time: unix_now(),
                payload_data: local.sync_data(),
                local_peerlist: local.peerlist(),
            };
            send(
                sink,
                LegacyMessage::Response(AdminResponse::TimedSync(resp)),
            )
            .await?;
            emit(events, conn_id, PeerEventKind::CoreSync(req.payload_data)).await
        }
        LegacyMessage::Request(AdminRequest::Ping) => {
            if !admit(conn.limiter.request(LegacyCommand::Ping, now))? {
                return Ok(());
            }
            let resp = PingResponse {
                status: PING_OK_RESPONSE_STATUS_TEXT.to_owned(),
                peer_id: cfg.peer_id,
            };
            send(sink, LegacyMessage::Response(AdminResponse::Ping(resp))).await
        }
        LegacyMessage::Request(AdminRequest::Handshake(_)) => {
            // "COMMAND_HANDSHAKE came, but seems that connection already have associated
            // peer_id (double COMMAND_HANDSHAKE?)" -> shutdown
            Err(PeerError::Unexpected("double handshake"))
        }
        LegacyMessage::Request(req) => {
            // Debug-only commands: release C++ builds have no handler either.
            if !admit(conn.limiter.request(req.command(), now))? {
                return Ok(());
            }
            reply_not_handled(sink, req.command()).await
        }
        LegacyMessage::UnknownRequest(command) => {
            if !admit(conn.limiter.unknown(now))? {
                return Ok(());
            }
            reply_not_handled(sink, command).await
        }
        LegacyMessage::Response(resp) => {
            let command = resp.command();
            let Some(pos) = pending.iter().position(|p| p.command == command) else {
                return Err(PeerError::UnexpectedResponse(command));
            };
            let p = pending.remove(pos).expect("position is in range");
            if let AdminResponse::TimedSync(ts) = &resp {
                // NodeServer::handleTimedSyncResponse
                emit(
                    events,
                    conn_id,
                    PeerEventKind::RemotePeerList {
                        entries: ts.local_peerlist.clone(),
                        remote_time: ts.local_time,
                    },
                )
                .await?;
                emit(
                    events,
                    conn_id,
                    PeerEventKind::CoreSync(ts.payload_data.clone()),
                )
                .await?;
            }
            if let Some(reply) = p.reply {
                let _ = reply.send(Ok(resp));
            }
            Ok(())
        }
        LegacyMessage::EmptyResponse(command) => {
            let Some(pos) = pending.iter().position(|p| p.command == command) else {
                return Err(PeerError::UnexpectedResponse(command));
            };
            let p = pending.remove(pos).expect("position is in range");
            match p.reply {
                Some(reply) => {
                    let _ = reply.send(Err(PeerError::NotHandled(command)));
                    Ok(())
                }
                // An empty timed-sync response is a decode failure for C++ -> shutdown.
                None => Err(PeerError::NotHandled(command)),
            }
        }
        LegacyMessage::Notification(m) => {
            let solicited = conn.gate.on_receive(&m);
            if !admit(conn.limiter.notification(&m, solicited, now))? {
                return Ok(());
            }
            emit(events, conn_id, PeerEventKind::Protocol(m)).await
        }
        LegacyMessage::UnknownNotification(_) => {
            admit(conn.limiter.unknown(now))?;
            Ok(())
        }
        LegacyMessage::ErrorResponse { .. } => Ok(()),
    }
}

async fn reply_not_handled(sink: &mut LegacySink, command: LegacyCommand) -> Result<(), PeerError> {
    send(
        sink,
        LegacyMessage::ErrorResponse {
            command,
            return_code: LEVIN_ERROR_HANDLER_NOT_DEFINED,
        },
    )
    .await
}

/// A C++-style back-ping (`NodeServer::try_ping`): connect to `addr`, send `COMMAND_PING`,
/// expect `status == "OK"`. Returns the remote peer id (compare it with the handshake's).
pub async fn ping(addr: SocketAddr, cfg: &LegacyConfig) -> Result<u64, PeerError> {
    timeout(cfg.ping_timeout, ping_inner(addr, cfg))
        .await
        .map_err(|_| PeerError::ConnectTimeout)?
}

async fn ping_inner(addr: SocketAddr, cfg: &LegacyConfig) -> Result<u64, PeerError> {
    let stream = TcpStream::connect(addr).await?;
    let mut framed = Framed::new(
        stream,
        LevinMessageCodec::<LegacyMessage>::new(cfg.protocol()),
    );
    framed
        .send(LevinMessage::Body(LegacyMessage::Request(
            AdminRequest::Ping,
        )))
        .await?;
    match framed.next().await {
        Some(Ok(LegacyMessage::Response(AdminResponse::Ping(r))))
            if r.status == PING_OK_RESPONSE_STATUS_TEXT =>
        {
            Ok(r.peer_id)
        }
        Some(Err(e)) => Err(e.into()),
        None => Err(PeerError::Closed),
        Some(Ok(_)) => Err(PeerError::BadPing),
    }
}

#[cfg(test)]
mod tests {
    use tokio::net::TcpListener;
    use xkr_types::Hash;

    use super::*;
    use crate::network::TESTNET_NETWORK_ID;

    fn cfg(peer_id: u64) -> LegacyConfig {
        LegacyConfig {
            network_id: TESTNET_NETWORK_ID,
            peer_id,
            my_port: 0,
            connect_timeout: Duration::from_secs(5),
            handshake_timeout: Duration::from_secs(5),
            ping_timeout: Duration::from_secs(5),
            invoke_timeout: Duration::from_secs(5),
            idle_timeout: Duration::from_secs(60),
            timed_sync_interval: Duration::from_millis(200),
            max_packet_size: LEVIN_DEFAULT_MAX_PACKET_SIZE,
            max_packet_size_before_handshake: 256 * 1024,
            peers_in_handshake: 250,
            white_list_limit: 1000,
            gray_list_limit: 5000,
            allow_local_ip: true,
            outbound_queue: 16,
            outbound_queue_bytes: P2P_CONNECTION_MAX_WRITE_BUFFER_SIZE,
            inbound_handshake_timeout: Duration::from_secs(5),
            unsolicited_max_packet_size: UNSOLICITED_MAX_PACKET_SIZE,
            solicited_max_packet_size: SOLICITED_MAX_PACKET_SIZE,
            large_readers: Arc::new(Semaphore::new(MAX_LARGE_READERS)),
        }
    }

    fn local(
        height: u32,
    ) -> (
        LocalState,
        watch::Sender<CoreSyncData>,
        watch::Sender<Arc<[PeerlistEntry]>>,
    ) {
        local_with_seed(height, 0)
    }

    fn local_with_seed(
        height: u32,
        pruning_seed: u32,
    ) -> (
        LocalState,
        watch::Sender<CoreSyncData>,
        watch::Sender<Arc<[PeerlistEntry]>>,
    ) {
        let (sync_tx, sync_rx) = watch::channel(CoreSyncData {
            current_height: height,
            top_id: Hash([height as u8; 32]),
            pruning_seed,
        });
        let (pl_tx, pl_rx) = watch::channel::<Arc<[PeerlistEntry]>>(Arc::from(Vec::new()));
        (
            LocalState {
                core_sync: sync_rx,
                peerlist_head: pl_rx,
            },
            sync_tx,
            pl_tx,
        )
    }

    #[tokio::test]
    async fn handshake_timed_sync_and_ping_between_two_rust_peers() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (server_local, _s1, _s2) = local(10);
        // The client is a stripe-pruned node (4 stripes, stripe 3): its seed travels in the
        // handshake and in every timed sync.
        let (client_local, _c1, _c2) = local_with_seed(7, 0x0403);

        let server = tokio::spawn(async move {
            let (stream, remote) = listener.accept().await.unwrap();
            let Accepted::Peer(hs) = LegacyPeer::accept(stream, remote, &cfg(1), &server_local)
                .await
                .unwrap()
            else {
                panic!("expected a handshake");
            };
            assert_eq!(hs.peer.info.core_sync.current_height, 7);
            assert_eq!(hs.peer.info.core_sync.pruning_seed, 0x0403);
            let (events_tx, mut events_rx) = mpsc::channel(16);
            let (_handle, _task) = hs.peer.spawn(cfg(1), server_local, 1, events_tx);
            // The client's timed sync arrives as a CoreSync event.
            loop {
                let ev = events_rx.recv().await.unwrap();
                if let PeerEventKind::CoreSync(cs) = ev.kind {
                    assert_eq!(cs.current_height, 7);
                    assert_eq!(cs.pruning_seed, 0x0403);
                    break;
                }
            }
        });

        let hs = LegacyPeer::connect(addr, &cfg(2), &client_local)
            .await
            .unwrap();
        assert_eq!(hs.peer.info.peer_id, 1);
        assert_eq!(hs.peer.info.core_sync.current_height, 10);
        assert_eq!(
            hs.peer.info.core_sync.pruning_seed, 0,
            "full node: no seed sent"
        );
        let (events_tx, _events_rx) = mpsc::channel(16);
        let (handle, _task) = hs.peer.spawn(cfg(2), client_local, 2, events_tx);

        match handle.request(AdminRequest::Ping).await.unwrap() {
            AdminResponse::Ping(p) => {
                assert_eq!(p.status, "OK");
                assert_eq!(p.peer_id, 1);
            }
            other => panic!("unexpected {other:?}"),
        }
        server.await.unwrap();
    }

    #[tokio::test]
    async fn wrong_network_is_rejected() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (server_local, _s1, _s2) = local(1);
        let (client_local, _c1, _c2) = local(1);
        let server = tokio::spawn(async move {
            let (stream, remote) = listener.accept().await.unwrap();
            let mut c = cfg(1);
            c.network_id = crate::network::MAINNET_NETWORK_ID;
            LegacyPeer::accept(stream, remote, &c, &server_local).await
        });
        let client = LegacyPeer::connect(addr, &cfg(2), &client_local).await;
        assert!(matches!(
            server.await.unwrap(),
            Err(PeerError::WrongNetwork(_))
        ));
        assert!(client.is_err());
    }

    /// The read limit is large only while one of our requests is outstanding and a
    /// node-wide permit is free; answers consume the request, unsolicited messages do not.
    #[test]
    fn read_gate_follows_our_requests() {
        use crate::wire::{NewTransactions, RequestGetObjects, RequestTxPool, ResponseGetObjects};
        let mut c = cfg(1);
        c.large_readers = Arc::new(Semaphore::new(1));
        let now = Instant::now();
        let first_limit = Arc::new(AtomicU64::new(0));
        let mut a = ReadGate::new(&c, first_limit.clone());
        assert_eq!(
            first_limit.load(Ordering::Relaxed),
            UNSOLICITED_MAX_PACKET_SIZE
        );
        let unsolicited = ProtocolMessage::NewTransactions(NewTransactions::default());
        assert!(!a.on_receive(&unsolicited));
        a.on_send(
            &ProtocolMessage::RequestGetObjects(RequestGetObjects::default()),
            now,
        );
        assert_eq!(
            first_limit.load(Ordering::Relaxed),
            SOLICITED_MAX_PACKET_SIZE
        );
        // The only permit is taken: a second connection stays at the small limit.
        let second_limit = Arc::new(AtomicU64::new(0));
        let mut b = ReadGate::new(&c, second_limit.clone());
        b.on_send(
            &ProtocolMessage::RequestTxPool(RequestTxPool::default()),
            now,
        );
        assert_eq!(
            second_limit.load(Ordering::Relaxed),
            UNSOLICITED_MAX_PACKET_SIZE
        );
        assert!(
            b.on_receive(&unsolicited),
            "still the answer to its pool request"
        );
        // A's answer releases the permit.
        assert!(!a.on_receive(&unsolicited));
        assert!(a.on_receive(&ProtocolMessage::ResponseGetObjects(
            ResponseGetObjects::default()
        )));
        assert_eq!(
            first_limit.load(Ordering::Relaxed),
            UNSOLICITED_MAX_PACKET_SIZE
        );
        b.on_send(
            &ProtocolMessage::RequestTxPool(RequestTxPool::default()),
            now,
        );
        assert_eq!(
            second_limit.load(Ordering::Relaxed),
            SOLICITED_MAX_PACKET_SIZE
        );
        // An unanswered request is forgotten after the invoke timeout.
        b.expire(now + c.invoke_timeout);
        assert_eq!(
            second_limit.load(Ordering::Relaxed),
            UNSOLICITED_MAX_PACKET_SIZE
        );
        assert!(!b.on_receive(&unsolicited));
    }

    /// Exceeding the read limit closes without a ban; exceeding the rate limits is a violation.
    #[test]
    fn read_limit_is_not_a_violation_but_rate_excess_is() {
        assert!(!PeerError::Levin(BucketError::BucketExceededReadLimit).is_protocol_violation());
        assert!(PeerError::Levin(BucketError::BucketExceededMaxSize).is_protocol_violation());
        assert!(PeerError::RateLimited.is_protocol_violation());
    }

    /// An inbound connection that says nothing is closed after the inbound timeout.
    #[tokio::test]
    async fn silent_inbound_connection_times_out() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (server_local, _s1, _s2) = local(1);
        let server = tokio::spawn(async move {
            let (stream, remote) = listener.accept().await.unwrap();
            let mut c = cfg(1);
            c.inbound_handshake_timeout = Duration::from_millis(200);
            let started = Instant::now();
            let res = LegacyPeer::accept(stream, remote, &c, &server_local).await;
            (res, started.elapsed())
        });
        let _silent = TcpStream::connect(addr).await.unwrap();
        let (res, took) = server.await.unwrap();
        assert!(matches!(res, Err(PeerError::HandshakeTimeout)), "{res:?}");
        assert!(took < Duration::from_secs(3), "{took:?}");
    }

    /// The outbound queue is bounded by bytes, like the C++ write queue.
    #[tokio::test]
    async fn outbound_queue_is_bounded_by_bytes() {
        use crate::wire::NewTransactions;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (server_local, _s1, _s2) = local(10);
        let (client_local, _c1, _c2) = local(7);
        let server = tokio::spawn(async move {
            let (stream, remote) = listener.accept().await.unwrap();
            let accepted = LegacyPeer::accept(stream, remote, &cfg(1), &server_local).await;
            tokio::time::sleep(Duration::from_millis(500)).await;
            drop(accepted);
        });
        let mut c = cfg(2);
        c.outbound_queue_bytes = 4096;
        let hs = LegacyPeer::connect(addr, &c, &client_local).await.unwrap();
        let (events_tx, _events_rx) = mpsc::channel(16);
        let (handle, _task) = hs.peer.spawn(c, client_local, 2, events_tx);
        let big = ProtocolMessage::NewTransactions(NewTransactions {
            txs: vec![vec![0u8; 8192]],
        });
        assert!(matches!(
            handle.notify(big),
            Err(PeerError::WriteBufferFull)
        ));
        assert_eq!(handle.queued_bytes(), 0);
        let small = ProtocolMessage::NewTransactions(NewTransactions {
            txs: vec![vec![0u8; 100]],
        });
        handle.notify(small).unwrap();
        server.await.unwrap();
    }
}
