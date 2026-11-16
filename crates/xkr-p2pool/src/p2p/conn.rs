//! Runs a [`PeerSession`] over a byte stream: a TCP socket to a C++ node or an xkrd
//! Hyperswarm connection (`xkr_p2p_hyper::HyperConn`), anything `AsyncRead + AsyncWrite`.
//!
//! The task reads, writes, applies the owner's [`Command`]s and forwards [`Event`]s, until the
//! peer closes, a rule is broken, the peer is idle too long or the owner lets go. The owner
//! (the server) bans the peer's address or key when the result is [`ConnEnd::Ban`].

use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _};
use tokio::sync::mpsc;

use super::message::{HASH_SIZE, PeerEntry};
use super::session::{Event, PeerError, PeerSession};

/// More than this many bytes waiting for the peer ends the connection (4 MiB).
pub const MAX_WRITE_QUEUE: usize = 4 * 1024 * 1024;
/// How often idle limits and peer list requests are checked.
pub const TICK: Duration = Duration::from_secs(2);
/// Bytes read per call.
const READ_CHUNK: usize = 64 * 1024;

/// What the owner tells a connection.
#[derive(Clone, Debug)]
pub enum Command {
    /// Request a share by id ([`PeerSession::request_block`]; ignored over the limit).
    RequestBlock([u8; HASH_SIZE]),
    /// Request the peer's peers now.
    RequestPeerList,
    /// Answer a block request ([`PeerSession::respond_block`]).
    RespondBlock {
        /// The share, empty when we have none.
        blob: Arc<[u8]>,
        /// Ids to announce first.
        notify: Vec<[u8; HASH_SIZE]>,
    },
    /// Answer a peer list request ([`PeerSession::respond_peer_list`]).
    RespondPeerList {
        /// Good TCP peers other than this one.
        candidates: Vec<PeerEntry>,
        /// Open connections.
        num_connections: usize,
    },
    /// Send a share ([`PeerSession::broadcast_block`]).
    Broadcast {
        /// Share id.
        id: [u8; HASH_SIZE],
        /// Full share.
        blob: Arc<[u8]>,
    },
    /// The peer has this share ([`PeerSession::note_broadcasted`]).
    NoteBroadcasted([u8; HASH_SIZE]),
    /// The sidechain rejected a share from this peer ([`PeerSession::reject_block`]).
    RejectBlock,
    /// End the connection with a ban.
    Ban(String),
    /// End the connection without a ban (a duplicate peer id, shutdown).
    Close,
}

/// An event from connection `conn`.
#[derive(Clone, Debug)]
pub struct ConnEvent {
    /// The id the owner gave the connection.
    pub conn: u64,
    /// What happened.
    pub event: Event,
}

/// Why a connection ended.
#[derive(Debug)]
pub enum ConnEnd {
    /// A rule was broken: ban the peer.
    Ban(PeerError),
    /// The peer closed the stream.
    Closed,
    /// Read or write failed.
    Io(std::io::Error),
    /// The peer was silent too long.
    Idle,
    /// The peer does not read what we send.
    WriteQueueFull,
    /// [`Command::Close`], or the owner dropped its channel ends.
    ByOwner,
}

impl ConnEnd {
    /// Whether the peer is to be banned.
    #[must_use]
    pub fn bans(&self) -> bool {
        matches!(self, Self::Ban(_))
    }
}

/// Applies one command; `Some` ends the connection.
fn apply(session: &mut PeerSession, cmd: Command, now: Instant) -> Option<ConnEnd> {
    match cmd {
        Command::RequestBlock(id) => {
            session.request_block(id);
        }
        Command::RequestPeerList => session.request_peer_list(now),
        Command::RespondBlock { blob, notify } => session.respond_block(&blob, &notify),
        Command::RespondPeerList {
            candidates,
            num_connections,
        } => {
            session.respond_peer_list(&candidates, num_connections, now);
        }
        Command::Broadcast { id, blob } => {
            session.broadcast_block(id, &blob);
        }
        Command::NoteBroadcasted(id) => session.note_broadcasted(id),
        Command::RejectBlock => {
            if let Err(e) = session.reject_block() {
                return Some(ConnEnd::Ban(e));
            }
        }
        Command::Ban(reason) => return Some(ConnEnd::Ban(PeerError::Rejected(reason))),
        Command::Close => return Some(ConnEnd::ByOwner),
    }
    None
}

/// Runs the connection until it ends. The session's challenge goes out first. Events are sent
/// to `events` tagged with `conn`; a full `events` channel slows reading from this peer.
pub async fn run_connection<S>(
    stream: S,
    mut session: PeerSession,
    conn: u64,
    mut commands: mpsc::Receiver<Command>,
    events: mpsc::Sender<ConnEvent>,
) -> ConnEnd
where
    S: AsyncRead + AsyncWrite,
{
    let (mut reader, mut writer) = tokio::io::split(stream);
    let mut rbuf = vec![0u8; READ_CHUNK];
    let mut out = session.take_outbox();
    let mut tick = tokio::time::interval(TICK);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut commands_open = true;

    loop {
        out.extend_from_slice(&session.take_outbox());
        if out.len() > MAX_WRITE_QUEUE {
            return ConnEnd::WriteQueueFull;
        }
        tokio::select! {
            r = reader.read(&mut rbuf) => match r {
                Ok(0) => return ConnEnd::Closed,
                Ok(n) => match session.feed(&rbuf[..n], Instant::now()) {
                    Ok(list) => {
                        for event in list {
                            if events.send(ConnEvent { conn, event }).await.is_err() {
                                return ConnEnd::ByOwner;
                            }
                        }
                    }
                    Err(e) => return ConnEnd::Ban(e),
                },
                Err(e) => return ConnEnd::Io(e),
            },
            w = writer.write(&out), if !out.is_empty() => match w {
                Ok(0) => return ConnEnd::Closed,
                Ok(n) => {
                    out.drain(..n);
                    if out.is_empty() && let Err(e) = writer.flush().await {
                        return ConnEnd::Io(e);
                    }
                }
                Err(e) => return ConnEnd::Io(e),
            },
            cmd = commands.recv(), if commands_open => match cmd {
                Some(cmd) => {
                    if let Some(end) = apply(&mut session, cmd, Instant::now()) {
                        return end;
                    }
                }
                None => commands_open = false,
            },
            _ = tick.tick() => {
                let now = Instant::now();
                if session.idle(now) {
                    return ConnEnd::Idle;
                }
                if session.is_good() && session.peer_list_due(now) {
                    session.request_peer_list(now);
                }
                if !commands_open && events.is_closed() {
                    return ConnEnd::ByOwner;
                }
            }
        }
    }
}
