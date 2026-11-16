//! [`HyperConn`]: one HyperDHT connection as a tokio byte stream.

use std::collections::VecDeque;
use std::fmt;
use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use bytes::{Buf, Bytes};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::sync::oneshot;

use crate::bridge::{BridgeError, Inner};
use crate::frame::{Body, Close, Frame, HANDSHAKE_HASH_LEN, HEADER_LEN, PeerInfo, close_reason};

/// Largest `Data` payload a [`HyperConn`] writes in one frame (keeps the shared socket fair).
pub const MAX_WRITE_CHUNK: usize = 64 * 1024;

const PEER_INFO_TIMEOUT: Duration = Duration::from_secs(10);

/// Why a connection was closed (`Close.reason`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseReason {
    Normal,
    Error,
    Timeout,
    /// Firewall rejection, e.g. a missing or invalid Hugin `mid`.
    Firewall,
    Protocol,
    /// The bridge is shutting down.
    Shutdown,
    Other(u8),
}

impl CloseReason {
    #[must_use]
    pub fn from_wire(v: u8) -> Self {
        match v {
            close_reason::NORMAL => Self::Normal,
            close_reason::ERROR => Self::Error,
            close_reason::TIMEOUT => Self::Timeout,
            close_reason::FIREWALL => Self::Firewall,
            close_reason::PROTOCOL => Self::Protocol,
            close_reason::SHUTDOWN => Self::Shutdown,
            other => Self::Other(other),
        }
    }

    #[must_use]
    pub fn to_wire(self) -> u8 {
        match self {
            Self::Normal => close_reason::NORMAL,
            Self::Error => close_reason::ERROR,
            Self::Timeout => close_reason::TIMEOUT,
            Self::Firewall => close_reason::FIREWALL,
            Self::Protocol => close_reason::PROTOCOL,
            Self::Shutdown => close_reason::SHUTDOWN,
            Self::Other(v) => v,
        }
    }
}

/// A received or sent `Close`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CloseInfo {
    pub reason: CloseReason,
    pub code: u32,
    pub message: String,
}

impl CloseInfo {
    pub(crate) fn from_frame(c: &Close) -> Self {
        Self {
            reason: CloseReason::from_wire(c.reason),
            code: c.code,
            message: c.message.clone(),
        }
    }
}

impl fmt::Display for CloseInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?} (code {})", self.reason, self.code)?;
        if !self.message.is_empty() {
            write!(f, ": {}", self.message)?;
        }
        Ok(())
    }
}

/// What the bridge reported about a connection in `Opened`.
#[derive(Clone, PartialEq, Eq)]
pub struct ConnInfo {
    pub conn_id: u64,
    /// The remote peer's DHT public key (noise static key).
    pub remote_public_key: [u8; 32],
    /// Noise handshake hash (64 bytes, BLAKE2b); identical on both ends.
    pub handshake_hash: [u8; HANDSHAKE_HASH_LEN],
    /// Topic the connection belongs to, if known (always known for Hugin identities and
    /// explicit opens; unknown for inbound connections of the plain swarm).
    pub topic: Option<[u8; 32]>,
    /// True if we dialed.
    pub initiator: bool,
    /// Remote UDP address as seen by the bridge (may be a relay).
    pub addr: Option<SocketAddr>,
    /// The local DHT public key used (node key or the Hugin identity of the topic).
    pub local_public_key: Option<[u8; 32]>,
}

impl fmt::Debug for ConnInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ConnInfo")
            .field("conn_id", &format_args!("{:#x}", self.conn_id))
            .field("remote_public_key", &hex::encode(self.remote_public_key))
            .field("topic", &self.topic.map(hex::encode))
            .field("initiator", &self.initiator)
            .field("addr", &self.addr)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Default)]
pub(crate) struct ConnState {
    rx: VecDeque<Bytes>,
    /// Bytes received from the bridge and not yet credited back.
    rx_outstanding: u64,
    /// Bytes handed to the reader and not yet credited back.
    consumed_uncredited: u64,
    read_waker: Option<Waker>,
    /// Our send window (may only go below zero by the bridge's oversized-message rule, never
    /// by our own writes).
    send_window: i64,
    write_waker: Option<Waker>,
    remote_close: Option<CloseInfo>,
    local_close_sent: bool,
    local_error: Option<CloseInfo>,
    dead: Option<BridgeError>,
    peer_info: Option<PeerInfo>,
    peer_info_waiters: Vec<oneshot::Sender<PeerInfo>>,
}

impl ConnState {
    fn wake(&mut self) {
        if let Some(w) = self.read_waker.take() {
            w.wake();
        }
        if let Some(w) = self.write_waker.take() {
            w.wake();
        }
    }

    fn released(&self) -> bool {
        self.remote_close.is_some() || self.local_close_sent || self.dead.is_some()
    }
}

/// Per-connection state shared between the bridge reader task and the [`HyperConn`].
#[derive(Debug)]
pub(crate) struct ConnShared {
    pub(crate) info: ConnInfo,
    initial_credit: u32,
    state: Mutex<ConnState>,
}

/// Result of feeding a `Data` frame into a connection.
pub(crate) enum DataOutcome {
    Ok,
    CreditExceeded,
    Ignored,
}

impl ConnShared {
    pub(crate) fn new(info: ConnInfo, initial_credit: u32) -> Self {
        let state = ConnState {
            send_window: i64::from(initial_credit),
            ..ConnState::default()
        };
        Self {
            info,
            initial_credit,
            state: Mutex::new(state),
        }
    }

    fn lock(&self) -> MutexGuard<'_, ConnState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub(crate) fn on_data(&self, payload: Bytes) -> DataOutcome {
        let mut st = self.lock();
        if st.released() {
            return DataOutcome::Ignored;
        }
        let len = payload.len() as u64;
        if st.rx_outstanding > 0 && st.rx_outstanding + len > u64::from(self.initial_credit) {
            return DataOutcome::CreditExceeded;
        }
        st.rx_outstanding += len;
        if !payload.is_empty() {
            st.rx.push_back(payload);
        }
        if let Some(w) = st.read_waker.take() {
            w.wake();
        }
        DataOutcome::Ok
    }

    pub(crate) fn on_credit(&self, bytes: u32) {
        let mut st = self.lock();
        st.send_window = (st.send_window + i64::from(bytes)).min(i64::from(self.initial_credit));
        if let Some(w) = st.write_waker.take() {
            w.wake();
        }
    }

    /// Records the peer's `Close`. Returns true if we had already sent our own `Close`.
    pub(crate) fn on_remote_close(&self, info: CloseInfo) -> bool {
        let mut st = self.lock();
        let had_sent = st.local_close_sent;
        if st.remote_close.is_none() {
            st.remote_close = Some(info);
        }
        st.peer_info_waiters.clear();
        st.wake();
        had_sent
    }

    /// Marks a local protocol failure; returns false if the connection was already released.
    pub(crate) fn fail_local(&self, info: CloseInfo) -> bool {
        let mut st = self.lock();
        if st.released() {
            return false;
        }
        st.local_close_sent = true;
        st.local_error = Some(info);
        st.wake();
        true
    }

    pub(crate) fn on_bridge_dead(&self, err: &BridgeError) {
        let mut st = self.lock();
        if st.dead.is_none() {
            st.dead = Some(err.clone());
        }
        st.peer_info_waiters.clear();
        st.wake();
    }

    pub(crate) fn on_peer_info(&self, info: PeerInfo) {
        let mut st = self.lock();
        st.peer_info = Some(info);
        for w in st.peer_info_waiters.drain(..) {
            let _ = w.send(info);
        }
    }
}

/// One HyperDHT (secret-stream) connection carried over the bridge.
///
/// Implements [`AsyncRead`] + [`AsyncWrite`]: bytes written go out as `Data` frames (at most
/// [`MAX_WRITE_CHUNK`] each, never more than the credit window), bytes read come from `Data`
/// frames in order. Message boundaries are not preserved, so stream protocols (Levin, bare-rpc)
/// work unchanged. Credits are returned as the reader consumes data.
///
/// `poll_shutdown` and `Drop` close the whole connection (the protocol has no half-close).
pub struct HyperConn {
    shared: Arc<ConnShared>,
    bridge: Arc<Inner>,
}

impl fmt::Debug for HyperConn {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HyperConn")
            .field("info", &self.shared.info)
            .finish_non_exhaustive()
    }
}

impl HyperConn {
    pub(crate) fn new(shared: Arc<ConnShared>, bridge: Arc<Inner>) -> Self {
        Self { shared, bridge }
    }

    /// Remote key, topic, initiator flag, address.
    #[must_use]
    pub fn info(&self) -> &ConnInfo {
        &self.shared.info
    }

    #[must_use]
    pub fn conn_id(&self) -> u64 {
        self.shared.info.conn_id
    }

    /// Closes the connection (idempotent). Already received data stays readable.
    pub fn close(&self, reason: CloseReason) {
        close_conn(&self.shared, &self.bridge, reason);
    }

    /// A handle that can close this connection after the `HyperConn` itself was moved away
    /// (e.g. boxed into a peer task). It holds weak references only, so it never keeps the
    /// connection or the bridge alive.
    #[must_use]
    pub fn close_handle(&self) -> CloseHandle {
        CloseHandle {
            conn_id: self.conn_id(),
            shared: Arc::downgrade(&self.shared),
            bridge: Arc::downgrade(&self.bridge),
        }
    }

    /// The peer's `Close`, if it closed the connection.
    #[must_use]
    pub fn close_info(&self) -> Option<CloseInfo> {
        let st = self.shared.lock();
        st.remote_close.clone().or_else(|| st.local_error.clone())
    }

    /// True once the connection is closed in either direction or the bridge died.
    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.shared.lock().released()
    }

    /// Current send window in bytes (diagnostics and tests).
    #[must_use]
    pub fn send_window(&self) -> i64 {
        self.shared.lock().send_window
    }

    /// The latest `PeerInfo` (the bridge sends one every 30 s).
    #[must_use]
    pub fn last_peer_info(&self) -> Option<PeerInfo> {
        self.shared.lock().peer_info
    }

    /// Asks the bridge for fresh statistics.
    pub async fn peer_info(&self) -> Result<PeerInfo, BridgeError> {
        let (tx, rx) = oneshot::channel();
        {
            let mut st = self.shared.lock();
            if let Some(e) = &st.dead {
                return Err(e.clone());
            }
            if let Some(c) = &st.remote_close {
                return Err(BridgeError::ConnClosed(c.clone()));
            }
            st.peer_info_waiters.push(tx);
        }
        self.bridge
            .send(Frame::conn(self.conn_id(), Body::PeerInfoRequest))?;
        match tokio::time::timeout(PEER_INFO_TIMEOUT, rx).await {
            Ok(Ok(info)) => Ok(info),
            Ok(Err(_)) => Err(BridgeError::Closed("connection closed".into())),
            Err(_) => Err(BridgeError::Timeout),
        }
    }

    fn max_chunk(&self) -> usize {
        let frame_limit = (self.bridge.params.max_frame as usize).saturating_sub(HEADER_LEN);
        MAX_WRITE_CHUNK.min(frame_limit).max(1)
    }
}

/// Sends our `Close` once; later calls (and calls after the remote closed or the bridge died)
/// are no-ops.
fn close_conn(shared: &ConnShared, bridge: &Inner, reason: CloseReason) {
    {
        let mut st = shared.lock();
        if st.released() {
            return;
        }
        st.local_close_sent = true;
        st.wake();
    }
    let _ = bridge.send(Frame::conn(
        shared.info.conn_id,
        Body::Close(Close {
            reason: reason.to_wire(),
            code: 0,
            message: String::new(),
        }),
    ));
}

/// Closes a [`HyperConn`] from elsewhere; see [`HyperConn::close_handle`]. Cheap to clone.
///
/// Closing makes the owner's next read return EOF and its next write fail, exactly like
/// [`HyperConn::close`].
#[derive(Debug, Clone)]
pub struct CloseHandle {
    conn_id: u64,
    shared: Weak<ConnShared>,
    bridge: Weak<Inner>,
}

impl CloseHandle {
    /// The connection id.
    #[must_use]
    pub fn conn_id(&self) -> u64 {
        self.conn_id
    }

    /// Closes the connection (idempotent; a no-op once it is closed or the bridge is gone).
    pub fn close(&self, reason: CloseReason) {
        if let (Some(shared), Some(bridge)) = (self.shared.upgrade(), self.bridge.upgrade()) {
            close_conn(&shared, &bridge, reason);
        }
    }

    /// True once the connection is closed in either direction, the bridge died, or everything
    /// referring to it was dropped.
    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.shared.upgrade().is_none_or(|s| s.lock().released())
    }
}

fn closed_read_result(info: &CloseInfo) -> io::Result<()> {
    match info.reason {
        CloseReason::Normal | CloseReason::Shutdown => Ok(()),
        CloseReason::Timeout => Err(io::Error::new(io::ErrorKind::TimedOut, info.to_string())),
        CloseReason::Firewall => Err(io::Error::new(
            io::ErrorKind::ConnectionRefused,
            info.to_string(),
        )),
        _ => Err(io::Error::new(
            io::ErrorKind::ConnectionAborted,
            info.to_string(),
        )),
    }
}

impl AsyncRead for HyperConn {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if buf.remaining() == 0 {
            return Poll::Ready(Ok(()));
        }
        let mut credit = 0u64;
        {
            let mut st = this.shared.lock();
            let mut copied = 0usize;
            while buf.remaining() > 0 {
                let Some(front) = st.rx.front_mut() else {
                    break;
                };
                let n = front.len().min(buf.remaining());
                buf.put_slice(&front[..n]);
                front.advance(n);
                if front.is_empty() {
                    st.rx.pop_front();
                }
                copied += n;
            }
            if copied > 0 {
                st.consumed_uncredited += copied as u64;
                let threshold = u64::from(this.shared.initial_credit / 4).max(1);
                if !st.released() && (st.rx.is_empty() || st.consumed_uncredited >= threshold) {
                    credit = st.consumed_uncredited;
                    st.consumed_uncredited = 0;
                    st.rx_outstanding = st.rx_outstanding.saturating_sub(credit);
                }
            } else {
                if let Some(info) = &st.remote_close {
                    return Poll::Ready(closed_read_result(info));
                }
                if let Some(info) = &st.local_error {
                    return Poll::Ready(closed_read_result(info));
                }
                if st.local_close_sent {
                    return Poll::Ready(Ok(()));
                }
                if let Some(e) = &st.dead {
                    return Poll::Ready(Err(io::Error::other(e.clone())));
                }
                st.read_waker = Some(cx.waker().clone());
                return Poll::Pending;
            }
        }
        while credit > 0 {
            let n = credit.min(u64::from(u32::MAX));
            credit -= n;
            let _ = this
                .bridge
                .send(Frame::conn(this.conn_id(), Body::Credit(n as u32)));
        }
        Poll::Ready(Ok(()))
    }
}

impl AsyncWrite for HyperConn {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        if buf.is_empty() {
            return Poll::Ready(Ok(0));
        }
        let max_chunk = this.max_chunk();
        let n = {
            let mut st = this.shared.lock();
            if let Some(e) = &st.dead {
                return Poll::Ready(Err(io::Error::other(e.clone())));
            }
            if let Some(info) = &st.remote_close {
                return Poll::Ready(Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    info.to_string(),
                )));
            }
            if st.local_close_sent {
                return Poll::Ready(Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "connection closed",
                )));
            }
            if st.send_window <= 0 {
                st.write_waker = Some(cx.waker().clone());
                return Poll::Pending;
            }
            let window = usize::try_from(st.send_window).unwrap_or(usize::MAX);
            let n = buf.len().min(window).min(max_chunk);
            st.send_window -= n as i64;
            n
        };
        match this.bridge.send(Frame::conn(
            this.conn_id(),
            Body::Data(Bytes::copy_from_slice(&buf[..n])),
        )) {
            Ok(()) => Poll::Ready(Ok(n)),
            Err(e) => Poll::Ready(Err(io::Error::other(e))),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        // Frames are handed to the bridge writer task as soon as they are written.
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.close(CloseReason::Normal);
        Poll::Ready(Ok(()))
    }
}

impl Drop for HyperConn {
    fn drop(&mut self) {
        self.close(CloseReason::Normal);
    }
}

#[cfg(test)]
mod tests {
    use super::{CloseHandle, HyperConn};

    fn assert_peer_stream<
        T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + Sync + 'static,
    >() {
    }
    fn assert_send_sync<T: Send + Sync + 'static>() {}

    /// `xkr-p2p-legacy` boxes any `AsyncRead + AsyncWrite + Unpin + Send + 'static` stream as
    /// its `BoxedPeerStream`; xkrd hands `HyperConn`s to it.
    #[test]
    fn hyper_conn_is_a_peer_stream() {
        assert_peer_stream::<HyperConn>();
        assert_send_sync::<CloseHandle>();
    }

    /// Credit rule of `docs/hyper-bridge-protocol.md` section 4: one message larger than the window
    /// is accepted only while nothing is outstanding (it is bounded by `max_frame` in the codec);
    /// anything beyond the window after that is refused.
    #[test]
    fn credit_window_is_enforced_after_one_oversized_message() {
        use super::{ConnInfo, ConnShared, DataOutcome};
        let info = ConnInfo {
            conn_id: 1,
            remote_public_key: [0; 32],
            handshake_hash: [0; 64],
            topic: None,
            initiator: false,
            addr: None,
            local_public_key: None,
        };
        let shared = ConnShared::new(info, 1000);
        let chunk = |n: usize| bytes::Bytes::from(vec![0u8; n]);
        assert!(matches!(shared.on_data(chunk(1500)), DataOutcome::Ok));
        assert!(matches!(
            shared.on_data(chunk(1)),
            DataOutcome::CreditExceeded
        ));
        let normal = ConnShared::new(shared.info.clone(), 1000);
        assert!(matches!(normal.on_data(chunk(600)), DataOutcome::Ok));
        assert!(matches!(normal.on_data(chunk(400)), DataOutcome::Ok));
        assert!(matches!(
            normal.on_data(chunk(1)),
            DataOutcome::CreditExceeded
        ));
    }
}
