//! One peer connection as a state machine without I/O (`P2PServer::P2PClient`): bytes in
//! through [`PeerSession::feed`], bytes out through [`PeerSession::take_outbox`], events for the
//! owner (the sidechain and the server) and commands from it. [`super::conn`] runs it over any
//! `AsyncRead + AsyncWrite` stream: a TCP socket to a C++ node or a Hyperswarm connection.
//!
//! Share bytes stay opaque here; the owner parses them, and reports a wrong share with
//! [`PeerSession::reject_block`] (counts towards a ban) or ends the connection with a ban.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use super::handshake::{self, HandshakeError, PeerIds};
use super::message::{self, CHALLENGE_SIZE, HASH_SIZE, Message, MessageId, PeerEntry, WireError};
use super::peers::{
    self, BAN_FAULT_THRESHOLD, PEER_REQUEST_DELAY, PROTOCOL_1_0, PROTOCOL_1_2, ReceivedPeers,
    VersionRecord,
};

/// `MAX_PENDING_BLOCK_REQUESTS`.
pub const MAX_PENDING_BLOCK_REQUESTS: usize = 25;
/// Most `BLOCK_REQUEST`s accepted in one read (`MAX_PENDING_BLOCK_REQUESTS * 4`).
pub const MAX_BLOCK_REQUESTS_PER_READ: u32 = 100;
/// Most `PEER_LIST_REQUEST`s accepted in one read.
pub const MAX_PEER_LIST_REQUESTS_PER_READ: u32 = 10;
/// Idle limit before the handshake completes.
pub const IDLE_BEFORE_HANDSHAKE: Duration = Duration::from_secs(10);
/// Idle limit after the handshake.
pub const IDLE_AFTER_HANDSHAKE: Duration = Duration::from_mins(5);
/// How many recently broadcast ids are remembered per peer (`m_broadcastedHashes`).
pub const REMEMBERED_BROADCASTS: usize = 8;

/// Who opened the connection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    /// We connected; we must put work into our handshake solution.
    Outgoing,
    /// The peer connected; its solution must carry the work.
    Incoming,
}

/// Which transport the connection runs on; picks the peer id we send.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Domain {
    /// TCP (C++ nodes and xkrd).
    Tcp,
    /// Hyperswarm (xkrd only).
    Hyper,
}

/// Fixed settings of one connection.
#[derive(Clone, Debug)]
pub struct SessionConfig {
    /// The sidechain's consensus id (never sent).
    pub consensus_id: [u8; 32],
    /// Our peer ids.
    pub peer_ids: PeerIds,
    /// Transport.
    pub domain: Domain,
    /// Who opened the connection.
    pub direction: Direction,
    /// The TCP port we announce in `LISTEN_PORT` (1..65535, required by C++ peers even when we
    /// do not listen).
    pub listen_port: u16,
    /// The version record of our first peer list answer.
    pub version: VersionRecord,
}

/// What the owner learns from the peer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    /// The peer's challenge arrived with this peer id. A second connection with the same id in
    /// the same domain should be closed (without a ban).
    PeerIdentified {
        /// The peer's id.
        peer_id: u64,
    },
    /// Both handshake solutions are checked; `LISTEN_PORT` and a request for the peer's chain
    /// tip were sent.
    HandshakeComplete,
    /// The peer's listen port; the peer is now good (it gets broadcasts).
    ListenPort(u16),
    /// The peer asks for a share (`None` = our chain tip). Answer with
    /// [`PeerSession::respond_block`].
    BlockRequest(Option<[u8; HASH_SIZE]>),
    /// The answer to our oldest request (`requested = None`: the tip request). An empty `blob`
    /// means the peer has no such share. A non-empty answer to a request by id must have that
    /// id, else the owner ends the connection with a ban.
    BlockResponse {
        /// What we asked for.
        requested: Option<[u8; HASH_SIZE]>,
        /// The full share.
        blob: Vec<u8>,
    },
    /// A share pushed by the peer (never empty).
    BlockBroadcast {
        /// The share.
        blob: Vec<u8>,
        /// `BLOCK_BROADCAST_COMPACT`.
        compact: bool,
    },
    /// The peer has this share; request it with [`PeerSession::request_block`] if unknown.
    BlockNotify([u8; HASH_SIZE]),
    /// The peer asks for peers. Answer with [`PeerSession::respond_peer_list`].
    PeerListRequest,
    /// The peer's list after filtering; its version record is already applied.
    PeerList(ReceivedPeers),
    /// A message xkrd reads and drops (`AUX_JOB_DONATION`, `MONERO_BLOCK_BROADCAST`).
    Dropped(MessageId),
}

/// A protocol violation. The connection ends and the peer is banned for
/// [`peers::BAN_TIME`].
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum PeerError {
    /// A message that cannot be read.
    #[error(transparent)]
    Wire(#[from] WireError),
    /// A failed handshake.
    #[error(transparent)]
    Handshake(#[from] HandshakeError),
    /// A message before the handshake completed, or a handshake message after it.
    #[error("{} out of order", .0.name())]
    OutOfOrder(MessageId),
    /// A second `LISTEN_PORT`.
    #[error("repeated LISTEN_PORT")]
    RepeatedListenPort,
    /// A port outside 1..65535.
    #[error("invalid listen port {0}")]
    BadListenPort(i32),
    /// More than 100 `BLOCK_REQUEST`s in one read.
    #[error("too many block requests at once")]
    TooManyBlockRequests,
    /// More than 10 `PEER_LIST_REQUEST`s in one read.
    #[error("too many peer list requests at once")]
    TooManyPeerListRequests,
    /// A `BLOCK_RESPONSE` without a pending request.
    #[error("unexpected BLOCK_RESPONSE")]
    UnexpectedBlockResponse,
    /// A `PEER_LIST_RESPONSE` without a pending request.
    #[error("unexpected PEER_LIST_RESPONSE")]
    UnexpectedPeerList,
    /// A broadcast without a share.
    #[error("empty block broadcast")]
    EmptyBroadcast,
    /// The [`BAN_FAULT_THRESHOLD`]-th rejected share on this connection.
    #[error("too many rejected blocks")]
    RejectedBlocks,
    /// A violation found by the owner (share parse error, wrong share id, stale chain tip,
    /// broadcast throttle).
    #[error("{0}")]
    Rejected(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stage {
    AwaitChallenge,
    AwaitSolution,
    Done,
}

/// One connection's protocol state.
#[derive(Debug)]
pub struct PeerSession {
    cfg: SessionConfig,
    our_challenge: [u8; CHALLENGE_SIZE],
    first_salt: u64,
    stage: Stage,
    peer_id: Option<u64>,
    peer_listen_port: Option<u16>,
    protocol: u32,
    software: Option<VersionRecord>,
    pending_blocks: VecDeque<Option<[u8; HASH_SIZE]>>,
    peer_list_pending: bool,
    next_peer_list_request: Option<Instant>,
    last_peers_sent: Option<Instant>,
    broadcasted: [[u8; HASH_SIZE]; REMEMBERED_BROADCASTS],
    broadcasted_next: usize,
    ban_score: u32,
    last_alive: Instant,
    inbuf: Vec<u8>,
    outbox: Vec<u8>,
}

impl PeerSession {
    /// A new connection with random challenge and salt; the challenge is queued at once.
    #[must_use]
    pub fn new(cfg: SessionConfig, now: Instant) -> Self {
        Self::with_values(cfg, super::random_u64(), super::random_u64(), now)
    }

    /// A new connection with a given challenge and first salt (tests).
    #[must_use]
    pub fn with_values(cfg: SessionConfig, challenge: u64, first_salt: u64, now: Instant) -> Self {
        let mut s = Self {
            cfg,
            our_challenge: challenge.to_le_bytes(),
            first_salt,
            stage: Stage::AwaitChallenge,
            peer_id: None,
            peer_listen_port: None,
            protocol: PROTOCOL_1_0,
            software: None,
            pending_blocks: VecDeque::new(),
            peer_list_pending: false,
            next_peer_list_request: None,
            last_peers_sent: None,
            broadcasted: [[0; HASH_SIZE]; REMEMBERED_BROADCASTS],
            broadcasted_next: 0,
            ban_score: 0,
            last_alive: now,
            inbuf: Vec::new(),
            outbox: Vec::new(),
        };
        let peer_id = match s.cfg.domain {
            Domain::Tcp => s.cfg.peer_ids.clearnet(),
            Domain::Hyper => s.cfg.peer_ids.hyper(),
        };
        s.send(&Message::HandshakeChallenge {
            challenge: s.our_challenge,
            peer_id,
        });
        s
    }

    fn send(&mut self, m: &Message) {
        m.encode(&mut self.outbox);
    }

    /// Takes the bytes to write to the peer.
    pub fn take_outbox(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.outbox)
    }

    /// Bytes waiting to be written.
    #[must_use]
    pub fn outbox_len(&self) -> usize {
        self.outbox.len()
    }

    /// Feeds bytes read from the peer and returns the events of every complete message. The
    /// per-read limits (`BLOCK_REQUEST`, `PEER_LIST_REQUEST`) count within one call, as C++
    /// counts within one read. An error ends the connection with a ban.
    pub fn feed(&mut self, data: &[u8], now: Instant) -> Result<Vec<Event>, PeerError> {
        self.inbuf.extend_from_slice(data);
        let mut events = Vec::new();
        let mut block_requests = 0u32;
        let mut peer_list_requests = 0u32;
        let mut used = 0;
        while let Some((msg, n)) = message::decode(&self.inbuf[used..])? {
            used += n;
            self.last_alive = now;
            match msg.id() {
                MessageId::BlockRequest => {
                    block_requests += 1;
                    if block_requests > MAX_BLOCK_REQUESTS_PER_READ {
                        return Err(PeerError::TooManyBlockRequests);
                    }
                }
                MessageId::PeerListRequest => {
                    peer_list_requests += 1;
                    if peer_list_requests > MAX_PEER_LIST_REQUESTS_PER_READ {
                        return Err(PeerError::TooManyPeerListRequests);
                    }
                }
                _ => {}
            }
            self.handle(msg, now, &mut events)?;
        }
        self.inbuf.drain(..used);
        Ok(events)
    }

    fn handle(
        &mut self,
        msg: Message,
        now: Instant,
        events: &mut Vec<Event>,
    ) -> Result<(), PeerError> {
        let id = msg.id();
        let expected = match self.stage {
            Stage::AwaitChallenge => Some(MessageId::HandshakeChallenge),
            Stage::AwaitSolution => Some(MessageId::HandshakeSolution),
            Stage::Done => None,
        };
        match expected {
            Some(e) if e != id => return Err(PeerError::OutOfOrder(id)),
            None if matches!(
                id,
                MessageId::HandshakeChallenge | MessageId::HandshakeSolution
            ) =>
            {
                return Err(PeerError::OutOfOrder(id));
            }
            _ => {}
        }
        match msg {
            Message::HandshakeChallenge { challenge, peer_id } => {
                handshake::check_peer_id(peer_id, self.cfg.peer_ids.all())?;
                self.peer_id = Some(peer_id);
                self.stage = Stage::AwaitSolution;
                let outgoing = self.cfg.direction == Direction::Outgoing;
                let (solution, salt) =
                    handshake::solve(challenge, &self.cfg.consensus_id, self.first_salt, outgoing);
                self.send(&Message::HandshakeSolution { solution, salt });
                events.push(Event::PeerIdentified { peer_id });
            }
            Message::HandshakeSolution { solution, salt } => {
                handshake::check_solution(
                    self.our_challenge,
                    &self.cfg.consensus_id,
                    &solution,
                    salt,
                    self.cfg.direction == Direction::Incoming,
                )?;
                self.stage = Stage::Done;
                // on_after_handshake: our listen port and a request for the peer's tip
                self.send(&Message::ListenPort(i32::from(self.cfg.listen_port)));
                self.send(&Message::BlockRequest([0; HASH_SIZE]));
                self.pending_blocks.push_back(None);
                events.push(Event::HandshakeComplete);
            }
            Message::ListenPort(port) => {
                if self.peer_listen_port.is_some() {
                    return Err(PeerError::RepeatedListenPort);
                }
                let p = u16::try_from(port)
                    .ok()
                    .filter(|p| *p != 0)
                    .ok_or(PeerError::BadListenPort(port))?;
                self.peer_listen_port = Some(p);
                events.push(Event::ListenPort(p));
            }
            Message::BlockRequest(h) => {
                events.push(Event::BlockRequest((h != [0; HASH_SIZE]).then_some(h)));
            }
            Message::BlockResponse(blob) => {
                let requested = self
                    .pending_blocks
                    .pop_front()
                    .ok_or(PeerError::UnexpectedBlockResponse)?;
                if requested.is_none() && self.peer_list_due(now) {
                    self.request_peer_list(now);
                }
                events.push(Event::BlockResponse { requested, blob });
            }
            Message::BlockBroadcast(blob) | Message::BlockBroadcastCompact(blob) => {
                if blob.is_empty() {
                    return Err(PeerError::EmptyBroadcast);
                }
                let compact = id == MessageId::BlockBroadcastCompact;
                events.push(Event::BlockBroadcast { blob, compact });
            }
            Message::BlockNotify(h) => {
                self.note_broadcasted(h);
                events.push(Event::BlockNotify(h));
            }
            Message::PeerListRequest => events.push(Event::PeerListRequest),
            Message::PeerListResponse(entries) => {
                if !self.peer_list_pending {
                    return Err(PeerError::UnexpectedPeerList);
                }
                self.peer_list_pending = false;
                let received = peers::filter_received(&entries);
                if let Some(v) = received.version {
                    self.protocol = peers::negotiate_protocol(self.protocol, v.protocol);
                    self.software = Some(v);
                }
                events.push(Event::PeerList(received));
            }
            Message::AuxJobDonation(_) | Message::MoneroBlockBroadcast(_) => {
                events.push(Event::Dropped(id));
            }
        }
        Ok(())
    }

    /// Asks the peer for a share by id. `false` (nothing sent) before the handshake or with
    /// [`MAX_PENDING_BLOCK_REQUESTS`] requests open.
    pub fn request_block(&mut self, id: [u8; HASH_SIZE]) -> bool {
        if self.stage != Stage::Done || self.pending_blocks.len() >= MAX_PENDING_BLOCK_REQUESTS {
            return false;
        }
        self.send(&Message::BlockRequest(id));
        self.pending_blocks.push_back(Some(id));
        true
    }

    /// Whether a peer list request is due (`send_peer_list_request` every 60 to 120 s).
    #[must_use]
    pub fn peer_list_due(&self, now: Instant) -> bool {
        self.next_peer_list_request.is_none_or(|t| now >= t)
    }

    /// Asks the peer for its peers unless a request is open; the next one is due 60 to 120 s
    /// later.
    pub fn request_peer_list(&mut self, now: Instant) {
        if self.peer_list_pending || self.stage != Stage::Done {
            return;
        }
        let delay = PEER_REQUEST_DELAY.as_secs();
        let jitter = super::random_u64() % (delay + 1);
        self.next_peer_list_request = Some(now + Duration::from_secs(delay + jitter));
        self.send(&Message::PeerListRequest);
        self.peer_list_pending = true;
    }

    /// Answers a `BLOCK_REQUEST` (`on_block_request`): `BLOCK_NOTIFY` for each of `notify`
    /// (parent, uncles and grandparent we have; only to peers speaking 1.2 or later, and only
    /// with a share), then `BLOCK_RESPONSE` with `blob` (empty: we have none).
    pub fn respond_block(&mut self, blob: &[u8], notify: &[[u8; HASH_SIZE]]) {
        if !blob.is_empty() && self.protocol >= PROTOCOL_1_2 {
            for h in notify {
                self.send(&Message::BlockNotify(*h));
            }
        }
        self.send(&Message::BlockResponse(blob.to_vec()));
    }

    /// Answers a `PEER_LIST_REQUEST` (`on_peer_list_request`). `candidates` are the good TCP
    /// peers other than this one (never Hyperswarm peers); `num_connections` is the number of
    /// open connections. A request within 60 s of the last answer gets an empty list; the
    /// first answer carries our version record in slot 0.
    pub fn respond_peer_list(
        &mut self,
        candidates: &[PeerEntry],
        num_connections: usize,
        now: Instant,
    ) {
        let first = self.last_peers_sent.is_none();
        let mut picked = Vec::new();
        if first
            || self
                .last_peers_sent
                .is_some_and(|t| now >= t + PEER_REQUEST_DELAY)
        {
            self.last_peers_sent = Some(now);
            let target = peers::peers_to_send(num_connections);
            picked = peers::select_peers(candidates, target, super::random_u64);
        }
        let entries = peers::response_entries(picked, first, self.cfg.version);
        self.send(&Message::PeerListResponse(entries));
    }

    /// Sends a share to the peer (`P2PServer::on_broadcast`): `BLOCK_NOTIFY` when the peer
    /// speaks 1.2 or later and recently sent us this id, else `BLOCK_BROADCAST` with the full
    /// share. `false` (nothing sent) when the peer is not good yet.
    pub fn broadcast_block(&mut self, id: [u8; HASH_SIZE], blob: &[u8]) -> bool {
        if !self.is_good() {
            return false;
        }
        if self.protocol >= PROTOCOL_1_2 && self.broadcasted.contains(&id) {
            self.send(&Message::BlockNotify(id));
        } else {
            self.send(&Message::BlockBroadcast(blob.to_vec()));
        }
        true
    }

    /// Remembers that the peer has the share `id` (its broadcast or tip answer, once parsed).
    pub fn note_broadcasted(&mut self, id: [u8; HASH_SIZE]) {
        self.broadcasted[self.broadcasted_next % REMEMBERED_BROADCASTS] = id;
        self.broadcasted_next = self.broadcasted_next.wrapping_add(1);
    }

    /// Counts a share the sidechain rejected (`post_handle_incoming_block`); the
    /// [`BAN_FAULT_THRESHOLD`]-th ends the connection with a ban.
    pub fn reject_block(&mut self) -> Result<(), PeerError> {
        self.ban_score += 1;
        if self.ban_score >= BAN_FAULT_THRESHOLD {
            return Err(PeerError::RejectedBlocks);
        }
        Ok(())
    }

    /// Rejected shares so far.
    #[must_use]
    pub fn ban_score(&self) -> u32 {
        self.ban_score
    }

    /// Both handshake solutions checked.
    #[must_use]
    pub fn handshake_complete(&self) -> bool {
        self.stage == Stage::Done
    }

    /// Handshake complete and listen port known (`is_good`).
    #[must_use]
    pub fn is_good(&self) -> bool {
        self.stage == Stage::Done && self.peer_listen_port.is_some()
    }

    /// The peer's id, once its challenge arrived.
    #[must_use]
    pub fn peer_id(&self) -> Option<u64> {
        self.peer_id
    }

    /// The peer's listen port.
    #[must_use]
    pub fn peer_listen_port(&self) -> Option<u16> {
        self.peer_listen_port
    }

    /// The protocol version spoken with the peer (1.0 until its version record arrives).
    #[must_use]
    pub fn protocol(&self) -> u32 {
        self.protocol
    }

    /// The peer's version record.
    #[must_use]
    pub fn software(&self) -> Option<VersionRecord> {
        self.software
    }

    /// Open block requests.
    #[must_use]
    pub fn pending_block_requests(&self) -> usize {
        self.pending_blocks.len()
    }

    /// The connection's settings.
    #[must_use]
    pub fn config(&self) -> &SessionConfig {
        &self.cfg
    }

    /// Whether the peer has been silent too long at `now`: 10 s before the handshake, 300 s
    /// after (`P2PServer::update_peer_connections`).
    #[must_use]
    pub fn idle(&self, now: Instant) -> bool {
        let limit = if self.stage == Stage::Done {
            IDLE_AFTER_HANDSHAKE
        } else {
            IDLE_BEFORE_HANDSHAKE
        };
        now.saturating_duration_since(self.last_alive) >= limit
    }
}
