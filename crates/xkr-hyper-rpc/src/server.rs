//! One connection on the server side: [`serve`].

use std::fmt;
use std::io;
use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt as _;
use futures::future::BoxFuture;
use futures::stream::{BoxStream, FuturesUnordered};
use serde_json::Value;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt as _};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::Instant;

use crate::frame::{self, FrameError, MAX_FRAME, read_frame, write_frame};
use crate::message::{
    Hello, HelloReply, Push, Refusal, RefuseReason, Request, Response, RpcError, VERSION, code,
};

/// Answers the calls of one accepted connection.
pub trait Handler: Send + Sync + 'static {
    /// One call. Calls of a connection run concurrently (up to
    /// [`ServerLimits::max_in_flight`]); answers go out in the order they finish.
    fn call(&self, method: String, params: Value) -> BoxFuture<'static, Result<Value, RpcError>>;
}

/// What the server makes of an accepted hello.
pub struct Accepted {
    /// Sent to the client as the answer to its hello.
    pub reply: HelloReply,
    /// Answers the calls.
    pub handler: Arc<dyn Handler>,
    /// Pushes to send (e.g. the tip); a push that finds the send queue full is dropped.
    pub pushes: Option<BoxStream<'static, Push>>,
}

impl fmt::Debug for Accepted {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Accepted")
            .field("reply", &self.reply)
            .field("pushes", &self.pushes.is_some())
            .finish_non_exhaustive()
    }
}

/// A refused hello: the reason goes to the client, then the connection closes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refused {
    pub reason: RefuseReason,
    pub message: String,
}

impl Refused {
    #[must_use]
    pub fn new(reason: RefuseReason, message: impl Into<String>) -> Self {
        Self {
            reason,
            message: message.into(),
        }
    }
}

/// Limits of one connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ServerLimits {
    /// Largest frame either way ([`MAX_FRAME`]); a larger answer becomes a
    /// [`code::TOO_LARGE`] error.
    pub max_frame: usize,
    /// Calls being answered at once; further calls get [`code::BUSY`].
    pub max_in_flight: usize,
    /// Sustained calls per second (0: no rate limit); over it calls get
    /// [`code::RATE_LIMITED`].
    pub rate_per_sec: u32,
    /// Calls allowed at once above the sustained rate.
    pub burst: u32,
    /// Refused calls in a row after which the connection is closed ([`ServeEnd::TooManyRefused`]).
    pub max_refused_in_a_row: u32,
    /// Time the client has for its hello.
    pub hello_timeout: Duration,
    /// Frames queued for sending; while it is full no more calls are read.
    pub out_queue: usize,
    /// Time to send what is queued when the connection ends.
    pub drain_timeout: Duration,
}

impl Default for ServerLimits {
    fn default() -> Self {
        Self {
            max_frame: MAX_FRAME,
            max_in_flight: 8,
            rate_per_sec: 20,
            burst: 40,
            max_refused_in_a_row: 64,
            hello_timeout: Duration::from_secs(10),
            out_queue: 16,
            drain_timeout: Duration::from_secs(5),
        }
    }
}

/// How a connection ended.
#[derive(Debug)]
pub enum ServeEnd {
    /// The client closed it.
    Closed,
    /// The hello was refused.
    Refused(RefuseReason),
    /// No hello in time.
    HelloTimeout,
    /// A frame that is too large, empty or not a message of the protocol.
    Protocol(String),
    /// Too many refused calls in a row.
    TooManyRefused,
    /// The stream failed.
    Io(io::Error),
}

impl ServeEnd {
    /// The client broke the protocol or its limits (worth a ban).
    #[must_use]
    pub fn is_misbehaviour(&self) -> bool {
        matches!(self, ServeEnd::Protocol(_) | ServeEnd::TooManyRefused)
    }
}

impl fmt::Display for ServeEnd {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ServeEnd::Closed => f.write_str("closed by the client"),
            ServeEnd::Refused(r) => write!(f, "hello refused ({})", r.as_str()),
            ServeEnd::HelloTimeout => f.write_str("no hello in time"),
            ServeEnd::Protocol(e) => write!(f, "protocol error: {e}"),
            ServeEnd::TooManyRefused => f.write_str("too many refused calls"),
            ServeEnd::Io(e) => write!(f, "I/O error: {e}"),
        }
    }
}

/// Serves one connection: reads the hello, asks `accept` what to do with it (version checked
/// already), answers it, then answers calls through the [`Handler`] and sends the pushes until
/// the client closes, breaks the protocol or the stream fails.
pub async fn serve<S, F>(stream: S, limits: ServerLimits, accept: F) -> ServeEnd
where
    S: AsyncRead + AsyncWrite + Send + 'static,
    F: FnOnce(&Hello) -> Result<Accepted, Refused>,
{
    let (mut rd, mut wr) = tokio::io::split(stream);
    let first =
        match tokio::time::timeout(limits.hello_timeout, read_frame(&mut rd, limits.max_frame))
            .await
        {
            Err(_) => return ServeEnd::HelloTimeout,
            Ok(Ok(Some(body))) => body,
            Ok(Ok(None)) => return ServeEnd::Closed,
            Ok(Err(e)) => return frame_end(e),
        };
    let hello = match frame::decode::<Hello>(&first) {
        Ok(h) => h,
        Err(e) => {
            let refused = Refused::new(RefuseReason::Hello, "the first frame must be a hello");
            refuse(&mut wr, &limits, &refused).await;
            return ServeEnd::Protocol(e.to_string());
        }
    };
    let accepted = if hello.v == VERSION {
        accept(&hello)
    } else {
        Err(Refused::new(
            RefuseReason::Version,
            format!("protocol version {VERSION} only"),
        ))
    };
    let accepted = match accepted {
        Ok(a) => a,
        Err(refused) => {
            refuse(&mut wr, &limits, &refused).await;
            return ServeEnd::Refused(refused.reason);
        }
    };
    let reply = match frame::encode(&accepted.reply) {
        Ok(body) => body,
        Err(e) => return ServeEnd::Io(io::Error::other(e)),
    };
    if let Err(e) = write_frame(&mut wr, &reply, limits.max_frame).await {
        return frame_end(e);
    }
    run(rd, wr, limits, accepted).await
}

/// Sends the refusal (within the drain timeout) and closes the write side.
async fn refuse<W: AsyncWrite + Unpin>(wr: &mut W, limits: &ServerLimits, refused: &Refused) {
    let Ok(body) = frame::encode(&Refusal::new(refused.reason, refused.message.clone())) else {
        return;
    };
    let _ = tokio::time::timeout(limits.drain_timeout, async {
        if write_frame(wr, &body, limits.max_frame).await.is_ok() {
            let _ = wr.shutdown().await;
        }
    })
    .await;
}

fn frame_end(e: FrameError) -> ServeEnd {
    match e {
        FrameError::Io(e) => ServeEnd::Io(e),
        FrameError::Truncated => ServeEnd::Closed,
        other => ServeEnd::Protocol(other.to_string()),
    }
}

fn gone() -> io::Error {
    io::Error::new(io::ErrorKind::BrokenPipe, "the connection is gone")
}

/// A task that is aborted when this is dropped.
struct AbortOnDrop<T>(JoinHandle<T>);

impl<T> Drop for AbortOnDrop<T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// The call budget of a connection: `burst` calls, refilled at `rate` per second.
struct Bucket {
    tokens: f64,
    /// `None`: no limit.
    rate: Option<f64>,
    burst: f64,
    last: Instant,
}

impl Bucket {
    fn new(limits: &ServerLimits, now: Instant) -> Self {
        let burst = f64::from(limits.burst.max(1));
        Self {
            tokens: burst,
            rate: (limits.rate_per_sec > 0).then(|| f64::from(limits.rate_per_sec)),
            burst,
            last: now,
        }
    }

    fn take(&mut self, now: Instant) -> bool {
        let Some(rate) = self.rate else {
            return true;
        };
        let elapsed = now.saturating_duration_since(self.last).as_secs_f64();
        self.last = now;
        self.tokens = (self.tokens + elapsed * rate).min(self.burst);
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

async fn next_push(pushes: &mut Option<BoxStream<'static, Push>>) -> Option<Push> {
    match pushes {
        Some(s) => s.next().await,
        None => std::future::pending().await,
    }
}

/// Queues the answer to a call (a [`code::TOO_LARGE`] error if it does not fit a frame);
/// `false` once the writer is gone.
async fn send(out: &mpsc::Sender<Vec<u8>>, response: Response, max_frame: usize) -> bool {
    let id = response.id;
    let body = match frame::encode(&response) {
        Ok(body) if body.len() <= max_frame => body,
        _ => {
            let error = RpcError::new(code::TOO_LARGE, "the answer is larger than a frame");
            match frame::encode(&Response::of(id, Err(error))) {
                Ok(body) => body,
                Err(_) => return false,
            }
        }
    };
    out.send(body).await.is_ok()
}

/// The session after the hello: a reader task, a writer task and this loop, which takes calls,
/// runs them and queues their answers and the pushes.
async fn run<R, W>(mut rd: R, mut wr: W, limits: ServerLimits, accepted: Accepted) -> ServeEnd
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let Accepted {
        handler,
        mut pushes,
        ..
    } = accepted;
    let max_frame = limits.max_frame;
    let (in_tx, mut in_rx) = mpsc::channel(1);
    let _reader = AbortOnDrop(tokio::spawn(async move {
        loop {
            let r = read_frame(&mut rd, max_frame).await;
            let more = matches!(r, Ok(Some(_)));
            if in_tx.send(r).await.is_err() || !more {
                return;
            }
        }
    }));
    let (out_tx, mut out_rx) = mpsc::channel::<Vec<u8>>(limits.out_queue.max(1));
    let mut writer = AbortOnDrop(tokio::spawn(async move {
        while let Some(body) = out_rx.recv().await {
            // `body` fits a frame (checked when queued).
            if write_frame(&mut wr, &body, usize::MAX).await.is_err() {
                return;
            }
        }
        let _ = wr.shutdown().await;
    }));
    let mut calls = FuturesUnordered::new();
    let mut bucket = Bucket::new(&limits, Instant::now());
    let mut refused = 0u32;
    let end = loop {
        tokio::select! {
            frame = in_rx.recv() => {
                let body = match frame {
                    None | Some(Ok(None)) => break ServeEnd::Closed,
                    Some(Err(e)) => break frame_end(e),
                    Some(Ok(Some(body))) => body,
                };
                let request: Request = match frame::decode(&body) {
                    Ok(r) => r,
                    Err(e) => break ServeEnd::Protocol(e.to_string()),
                };
                let refusal = if calls.len() >= limits.max_in_flight {
                    Some(RpcError::new(code::BUSY, "too many calls in flight"))
                } else if !bucket.take(Instant::now()) {
                    Some(RpcError::new(code::RATE_LIMITED, "too many calls per second"))
                } else {
                    None
                };
                if let Some(error) = refusal {
                    refused += 1;
                    if refused > limits.max_refused_in_a_row {
                        break ServeEnd::TooManyRefused;
                    }
                    if !send(&out_tx, Response::of(request.id, Err(error)), max_frame).await {
                        break ServeEnd::Io(gone());
                    }
                } else {
                    refused = 0;
                    let id = request.id;
                    let call = handler.call(request.method, request.params);
                    calls.push(async move { (id, call.await) });
                }
            }
            Some((id, outcome)) = calls.next(), if !calls.is_empty() => {
                if !send(&out_tx, Response::of(id, outcome), max_frame).await {
                    break ServeEnd::Io(gone());
                }
            }
            push = next_push(&mut pushes) => match push {
                Some(p) => {
                    if let Ok(body) = frame::encode(&p)
                        && body.len() <= max_frame
                        && let Err(mpsc::error::TrySendError::Closed(_)) = out_tx.try_send(body)
                    {
                        break ServeEnd::Io(gone());
                    }
                    // A full queue drops the push: the client is behind, a later one follows.
                }
                None => pushes = None,
            },
        }
    };
    drop(calls);
    drop(out_tx);
    // Send what is queued (an error answer, the last answers), then the writer closes.
    let _ = tokio::time::timeout(limits.drain_timeout, &mut writer.0).await;
    end
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_bucket_refills_at_the_rate() {
        let limits = ServerLimits {
            rate_per_sec: 2,
            burst: 3,
            ..ServerLimits::default()
        };
        let t0 = Instant::now();
        let mut b = Bucket::new(&limits, t0);
        assert!(b.take(t0) && b.take(t0) && b.take(t0));
        assert!(!b.take(t0), "burst used up");
        assert!(!b.take(t0 + Duration::from_millis(400)));
        assert!(
            b.take(t0 + Duration::from_millis(500)),
            "one call per 0.5 s"
        );
        assert!(!b.take(t0 + Duration::from_millis(600)));
        // never above the burst
        let later = t0 + Duration::from_secs(60);
        assert!(b.take(later) && b.take(later) && b.take(later));
        assert!(!b.take(later));
        let unlimited = ServerLimits {
            rate_per_sec: 0,
            ..ServerLimits::default()
        };
        let mut b = Bucket::new(&unlimited, t0);
        assert!((0..10_000).all(|_| b.take(t0)));
    }

    #[test]
    fn misbehaviour() {
        assert!(ServeEnd::TooManyRefused.is_misbehaviour());
        assert!(ServeEnd::Protocol("x".into()).is_misbehaviour());
        assert!(!ServeEnd::Closed.is_misbehaviour());
        assert!(!ServeEnd::HelloTimeout.is_misbehaviour());
        assert!(!ServeEnd::Refused(RefuseReason::Service).is_misbehaviour());
    }
}
