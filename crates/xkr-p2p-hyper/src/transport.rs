//! In-memory byte channel between the tokio side of a [`crate::Bridge`] and a plain thread (the
//! I/O pump of a Bare worker, or a fake worker in tests).
//!
//! [`mem_channel`] returns a [`MemStream`] (`AsyncRead + AsyncWrite`, handed to
//! [`crate::Bridge::connect_streams`]) and a [`MemPeer`] with non-blocking `read`/`write` calls
//! and a blocking [`MemPeer::wait`]. Frames travel back to back as over any byte stream, so the
//! frame layer does not know what carries it. Each direction buffers at
//! most `capacity` bytes; a full direction makes the writer wait (the stream side returns
//! `Pending`, the peer side gets `Some(0)`).
//!
//! Closing:
//! * the stream side stops writing on `shutdown` or drop; the peer reads what is left, then `None`;
//! * the peer side closes with [`MemPeer::close`] or drop; the stream reads what is left, then
//!   EOF, and its writes fail with `BrokenPipe`.

use std::fmt;
use std::io;
use std::pin::Pin;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use bytes::{Buf, BytesMut};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

/// Default buffer size per direction.
pub const DEFAULT_CHANNEL_CAPACITY: usize = 1024 * 1024;

struct State {
    /// Written by the stream, waiting for the peer.
    to_peer: BytesMut,
    /// Written by the peer, waiting for the stream.
    to_stream: BytesMut,
    capacity: usize,
    /// The stream will write no more (shutdown or dropped).
    stream_write_closed: bool,
    /// The stream was dropped: nobody reads `to_stream` any more.
    stream_dropped: bool,
    /// The peer closed the channel (both directions).
    peer_closed: bool,
    read_waker: Option<Waker>,
    write_waker: Option<Waker>,
    /// Bumped on every change a waiting peer may care about.
    generation: u64,
}

struct Shared {
    state: Mutex<State>,
    changed: Condvar,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Wakes a peer blocked in [`MemPeer::wait`]. Called with the lock held.
    fn changed(&self, st: &mut State) {
        st.generation = st.generation.wrapping_add(1);
        self.changed.notify_all();
    }
}

/// A new channel with `capacity` bytes of buffer per direction (at least 1).
#[must_use]
pub fn mem_channel(capacity: usize) -> (MemStream, MemPeer) {
    let shared = Arc::new(Shared {
        state: Mutex::new(State {
            to_peer: BytesMut::new(),
            to_stream: BytesMut::new(),
            capacity: capacity.max(1),
            stream_write_closed: false,
            stream_dropped: false,
            peer_closed: false,
            read_waker: None,
            write_waker: None,
            generation: 0,
        }),
        changed: Condvar::new(),
    });
    (
        MemStream {
            shared: shared.clone(),
        },
        MemPeer { shared },
    )
}

fn store_waker(slot: &mut Option<Waker>, cx: &Context<'_>) {
    if !slot.as_ref().is_some_and(|w| w.will_wake(cx.waker())) {
        *slot = Some(cx.waker().clone());
    }
}

/// The tokio side of a [`mem_channel`].
pub struct MemStream {
    shared: Arc<Shared>,
}

impl fmt::Debug for MemStream {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MemStream").finish_non_exhaustive()
    }
}

impl AsyncRead for MemStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let shared = &self.shared;
        let mut st = shared.lock();
        if !st.to_stream.is_empty() {
            let n = st.to_stream.len().min(buf.remaining());
            buf.put_slice(&st.to_stream[..n]);
            st.to_stream.advance(n);
            shared.changed(&mut st);
            return Poll::Ready(Ok(()));
        }
        if st.peer_closed {
            // EOF
            return Poll::Ready(Ok(()));
        }
        store_waker(&mut st.read_waker, cx);
        Poll::Pending
    }
}

impl AsyncWrite for MemStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        data: &[u8],
    ) -> Poll<io::Result<usize>> {
        let shared = &self.shared;
        let mut st = shared.lock();
        if st.peer_closed || st.stream_write_closed {
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "in-memory channel closed",
            )));
        }
        if data.is_empty() {
            return Poll::Ready(Ok(0));
        }
        let room = st.capacity.saturating_sub(st.to_peer.len());
        if room == 0 {
            store_waker(&mut st.write_waker, cx);
            return Poll::Pending;
        }
        let n = room.min(data.len());
        st.to_peer.extend_from_slice(&data[..n]);
        shared.changed(&mut st);
        Poll::Ready(Ok(n))
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let shared = &self.shared;
        let mut st = shared.lock();
        st.stream_write_closed = true;
        shared.changed(&mut st);
        Poll::Ready(Ok(()))
    }
}

impl Drop for MemStream {
    fn drop(&mut self) {
        let mut st = self.shared.lock();
        st.stream_write_closed = true;
        st.stream_dropped = true;
        self.shared.changed(&mut st);
    }
}

/// The thread side of a [`mem_channel`]. Every call is non-blocking except [`MemPeer::wait`].
pub struct MemPeer {
    shared: Arc<Shared>,
}

impl fmt::Debug for MemPeer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MemPeer").finish_non_exhaustive()
    }
}

impl MemPeer {
    /// Takes up to `buf.len()` bytes the stream wrote. `Some(0)`: nothing there now; `None`: the
    /// stream will write no more and everything was taken.
    pub fn read(&self, buf: &mut [u8]) -> Option<usize> {
        let (n, waker) = {
            let mut st = self.shared.lock();
            if st.to_peer.is_empty() {
                return if st.stream_write_closed {
                    None
                } else {
                    Some(0)
                };
            }
            let n = st.to_peer.len().min(buf.len());
            if n == 0 {
                return Some(0);
            }
            buf[..n].copy_from_slice(&st.to_peer[..n]);
            st.to_peer.advance(n);
            (n, st.write_waker.take())
        };
        if let Some(w) = waker {
            w.wake();
        }
        Some(n)
    }

    /// Hands bytes to the stream and returns how many fit (`Some(0)` when the buffer is full).
    /// `None`: the stream is gone, or this side was closed.
    #[must_use = "bytes that did not fit must be offered again"]
    pub fn write(&self, data: &[u8]) -> Option<usize> {
        let (n, waker) = {
            let mut st = self.shared.lock();
            if st.stream_dropped || st.peer_closed {
                return None;
            }
            let room = st.capacity.saturating_sub(st.to_stream.len());
            let n = room.min(data.len());
            if n == 0 {
                return Some(0);
            }
            st.to_stream.extend_from_slice(&data[..n]);
            (n, st.read_waker.take())
        };
        if let Some(w) = waker {
            w.wake();
        }
        Some(n)
    }

    /// Whether the stream was dropped (nothing written by this side will be read).
    #[must_use]
    pub fn stream_dropped(&self) -> bool {
        self.shared.lock().stream_dropped
    }

    /// The change counter; pass it to [`MemPeer::wait`] to sleep until something changes.
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.shared.lock().generation
    }

    /// Blocks until the stream side read or wrote, closed, or a [`PeerWaker`] fired since
    /// `seen` was taken with [`MemPeer::generation`], or until `timeout`.
    pub fn wait(&self, seen: u64, timeout: Duration) {
        let st = self.shared.lock();
        if st.generation != seen {
            return;
        }
        let (guard, _timed_out) = self
            .shared
            .changed
            .wait_timeout_while(st, timeout, |s| s.generation == seen)
            .unwrap_or_else(PoisonError::into_inner);
        drop(guard);
    }

    /// A handle that wakes [`MemPeer::wait`] from any thread (an FFI callback, a stop request).
    #[must_use]
    pub fn waker(&self) -> PeerWaker {
        PeerWaker {
            shared: self.shared.clone(),
        }
    }

    /// Closes both directions: the stream reads what is buffered, then EOF.
    pub fn close(&self) {
        let (r, w) = {
            let mut st = self.shared.lock();
            if st.peer_closed {
                return;
            }
            st.peer_closed = true;
            self.shared.changed(&mut st);
            (st.read_waker.take(), st.write_waker.take())
        };
        for waker in [r, w].into_iter().flatten() {
            waker.wake();
        }
    }
}

impl Drop for MemPeer {
    fn drop(&mut self) {
        self.close();
    }
}

/// Wakes the [`MemPeer`] of a channel from another thread. Cheap to clone.
#[derive(Clone)]
pub struct PeerWaker {
    shared: Arc<Shared>,
}

impl fmt::Debug for PeerWaker {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PeerWaker").finish_non_exhaustive()
    }
}

impl PeerWaker {
    /// Ends the current or next [`MemPeer::wait`] early.
    pub fn wake(&self) {
        let mut st = self.shared.lock();
        self.shared.changed(&mut st);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[tokio::test]
    async fn bytes_flow_both_ways_with_backpressure() {
        let (mut stream, peer) = mem_channel(4);
        // The stream side fills the buffer, then waits for the peer.
        let writer = tokio::spawn(async move {
            stream.write_all(b"0123456789").await.expect("write");
            stream.shutdown().await.expect("shutdown");
            stream
        });
        let mut got = Vec::new();
        let mut buf = [0u8; 3];
        loop {
            let seen = peer.generation();
            match peer.read(&mut buf) {
                None => break,
                Some(0) => {
                    tokio::task::yield_now().await;
                    peer.wait(seen, Duration::from_millis(5));
                }
                Some(n) => got.extend_from_slice(&buf[..n]),
            }
        }
        assert_eq!(got, b"0123456789");
        let mut stream = writer.await.expect("join");

        // Peer to stream: at most `capacity` bytes are taken at once.
        assert_eq!(peer.write(b"abcdef"), Some(4));
        assert_eq!(peer.write(b"ef"), Some(0));
        let mut out = [0u8; 8];
        let n = stream.read(&mut out).await.expect("read");
        assert_eq!(&out[..n], b"abcd");
        assert_eq!(peer.write(b"ef"), Some(2));
        peer.close();
        let mut rest = Vec::new();
        stream.read_to_end(&mut rest).await.expect("read to end");
        assert_eq!(rest, b"ef", "buffered bytes are read before EOF");
        let err = stream.write_all(b"x").await.expect_err("write after close");
        assert_eq!(err.kind(), io::ErrorKind::BrokenPipe);
        assert_eq!(peer.write(b"x"), None, "a closed peer cannot write");
    }

    #[test]
    fn dropping_the_stream_ends_the_peer() {
        let (stream, peer) = mem_channel(16);
        assert!(!peer.stream_dropped());
        drop(stream);
        assert!(peer.stream_dropped());
        assert_eq!(peer.read(&mut [0u8; 4]), None);
        assert_eq!(peer.write(b"x"), None);
    }

    #[test]
    fn a_waker_ends_a_wait_early() {
        let (_stream, peer) = mem_channel(16);
        let waker = peer.waker();
        let seen = peer.generation();
        let t = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            waker.wake();
        });
        let t0 = std::time::Instant::now();
        peer.wait(seen, Duration::from_secs(10));
        assert!(t0.elapsed() < Duration::from_secs(5));
        t.join().expect("join");
    }
}
