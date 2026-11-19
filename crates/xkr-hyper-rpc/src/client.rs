//! The client side: [`Client`].

use std::collections::HashMap;
use std::io;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::Value;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt as _};
use tokio::sync::{broadcast, mpsc, oneshot, watch};
use tokio::task::AbortHandle;

use crate::frame::{self, FrameError, MAX_FRAME, read_frame, write_frame};
use crate::message::{Hello, HelloReply, Push, Refusal, Request, Response, RpcError, Tip, VERSION};

/// Settings of a [`Client`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClientOptions {
    /// Largest frame either way.
    pub max_frame: usize,
    /// Time the server has to answer the hello.
    pub hello_timeout: Duration,
    /// Time a call waits for its answer.
    pub call_timeout: Duration,
    /// Calls queued for sending.
    pub out_queue: usize,
    /// Pushes kept for a slow [`Client::pushes`] receiver.
    pub push_buffer: usize,
}

impl Default for ClientOptions {
    fn default() -> Self {
        Self {
            max_frame: MAX_FRAME,
            hello_timeout: Duration::from_secs(10),
            call_timeout: Duration::from_secs(30),
            out_queue: 32,
            push_buffer: 64,
        }
    }
}

/// Why a connection or a call failed.
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    /// The server refused the hello (`reason` is [`crate::RefuseReason::as_str`]).
    #[error("the node refused the hello: {reason} ({message})")]
    Refused { reason: String, message: String },
    /// The server broke the protocol.
    #[error("protocol error: {0}")]
    Protocol(String),
    /// The connection is closed.
    #[error("the connection is closed")]
    Closed,
    /// No answer in time.
    #[error("no answer in time")]
    Timeout,
    /// The server answered with an error.
    #[error("{0}")]
    Rpc(RpcError),
    #[error(transparent)]
    Io(#[from] io::Error),
}

impl From<FrameError> for ClientError {
    fn from(e: FrameError) -> Self {
        match e {
            FrameError::Io(e) => ClientError::Io(e),
            FrameError::Truncated => ClientError::Closed,
            other => ClientError::Protocol(other.to_string()),
        }
    }
}

type Pending = HashMap<u64, oneshot::Sender<Result<Value, RpcError>>>;

/// Calls waiting for their answer; `None` once the connection is closed.
type PendingMap = Arc<Mutex<Option<Pending>>>;

struct Inner {
    out: mpsc::Sender<Vec<u8>>,
    pending: PendingMap,
    next_id: AtomicU64,
    reply: HelloReply,
    tips: watch::Receiver<Tip>,
    pushes: broadcast::Sender<Push>,
    closed: watch::Receiver<bool>,
    options: ClientOptions,
    tasks: [AbortHandle; 2],
}

impl Drop for Inner {
    fn drop(&mut self) {
        for t in &self.tasks {
            t.abort();
        }
    }
}

/// A connection to a node's service: the hello is done, calls can run concurrently (answers
/// are matched by id), and the tip pushes land in [`Client::tips`]. Cheap to clone; the
/// connection closes when the last clone is dropped.
#[derive(Clone)]
pub struct Client {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Client")
            .field("reply", &self.inner.reply)
            .field("closed", &self.is_closed())
            .finish_non_exhaustive()
    }
}

/// Fails every waiting call and refuses new ones.
fn close_all(pending: &PendingMap, closed: &watch::Sender<bool>) {
    if let Ok(mut p) = pending.lock() {
        p.take();
    }
    closed.send_replace(true);
}

impl Client {
    /// Sends `hello` on `stream` and waits for the answer, with [`ClientOptions::default`].
    pub async fn connect<S>(stream: S, hello: Hello) -> Result<Self, ClientError>
    where
        S: AsyncRead + AsyncWrite + Send + 'static,
    {
        Self::connect_with(stream, hello, ClientOptions::default()).await
    }

    /// [`Client::connect`] with other settings.
    pub async fn connect_with<S>(
        stream: S,
        hello: Hello,
        options: ClientOptions,
    ) -> Result<Self, ClientError>
    where
        S: AsyncRead + AsyncWrite + Send + 'static,
    {
        let (mut rd, mut wr) = tokio::io::split(stream);
        let reply = tokio::time::timeout(options.hello_timeout, async {
            write_frame(&mut wr, &frame::encode(&hello)?, options.max_frame).await?;
            let body = read_frame(&mut rd, options.max_frame)
                .await?
                .ok_or(ClientError::Closed)?;
            let v: Value = frame::decode(&body)?;
            if v.get("error").is_some() {
                let r: Refusal =
                    serde_json::from_value(v).map_err(|e| ClientError::Protocol(e.to_string()))?;
                return Err(ClientError::Refused {
                    reason: r.error,
                    message: r.message,
                });
            }
            let reply: HelloReply =
                serde_json::from_value(v).map_err(|e| ClientError::Protocol(e.to_string()))?;
            if reply.v != VERSION {
                return Err(ClientError::Protocol(format!(
                    "the node speaks version {}",
                    reply.v
                )));
            }
            Ok(reply)
        })
        .await
        .map_err(|_| ClientError::Timeout)??;

        let pending: PendingMap = Arc::new(Mutex::new(Some(HashMap::new())));
        let (closed_tx, closed) = watch::channel(false);
        let closed_tx = Arc::new(closed_tx);
        let (tips_tx, tips) = watch::channel(reply.tip());
        let (pushes, _) = broadcast::channel(options.push_buffer.max(1));
        let (out, mut out_rx) = mpsc::channel::<Vec<u8>>(options.out_queue.max(1));

        let reader = {
            let pending = pending.clone();
            let closed_tx = closed_tx.clone();
            let pushes = pushes.clone();
            let max_frame = options.max_frame;
            tokio::spawn(async move {
                loop {
                    let Ok(Some(body)) = read_frame(&mut rd, max_frame).await else {
                        break;
                    };
                    let Ok(v) = frame::decode::<Value>(&body) else {
                        break;
                    };
                    if v.get("id").is_some() {
                        let Ok(r) = serde_json::from_value::<Response>(v) else {
                            break;
                        };
                        let waiter = pending
                            .lock()
                            .ok()
                            .and_then(|mut p| p.as_mut().and_then(|p| p.remove(&r.id)));
                        if let Some(w) = waiter {
                            let _ = w.send(r.into_outcome());
                        }
                    } else {
                        let Ok(push) = serde_json::from_value::<Push>(v) else {
                            break;
                        };
                        if let Some(tip) = push.as_tip() {
                            tips_tx.send_replace(tip);
                        }
                        let _ = pushes.send(push);
                    }
                }
                close_all(&pending, &closed_tx);
                // `tips_tx` drops here: tip receivers see the end.
            })
        };
        let writer = {
            let pending = pending.clone();
            let closed_tx = closed_tx.clone();
            tokio::spawn(async move {
                while let Some(body) = out_rx.recv().await {
                    if write_frame(&mut wr, &body, usize::MAX).await.is_err() {
                        close_all(&pending, &closed_tx);
                        return;
                    }
                }
                let _ = wr.shutdown().await;
            })
        };
        Ok(Self {
            inner: Arc::new(Inner {
                out,
                pending,
                next_id: AtomicU64::new(1),
                reply,
                tips,
                pushes,
                closed,
                options,
                tasks: [reader.abort_handle(), writer.abort_handle()],
            }),
        })
    }

    /// The node's answer to the hello.
    #[must_use]
    pub fn hello(&self) -> &HelloReply {
        &self.inner.reply
    }

    /// Calls `method` with `params` and waits for the answer (at most
    /// [`ClientOptions::call_timeout`]).
    pub async fn call(&self, method: &str, params: Value) -> Result<Value, ClientError> {
        let inner = &self.inner;
        let id = inner.next_id.fetch_add(1, Ordering::Relaxed);
        let body = frame::encode(&Request {
            id,
            method: method.to_owned(),
            params,
        })?;
        if body.len() > inner.options.max_frame {
            return Err(FrameError::TooLarge(body.len(), inner.options.max_frame).into());
        }
        let (tx, rx) = oneshot::channel();
        {
            let mut guard = inner.pending.lock().map_err(|_| ClientError::Closed)?;
            let Some(p) = guard.as_mut() else {
                return Err(ClientError::Closed);
            };
            p.insert(id, tx);
        }
        let forget = || {
            if let Ok(mut p) = inner.pending.lock()
                && let Some(p) = p.as_mut()
            {
                p.remove(&id);
            }
        };
        if inner.out.send(body).await.is_err() {
            forget();
            return Err(ClientError::Closed);
        }
        match tokio::time::timeout(inner.options.call_timeout, rx).await {
            Ok(Ok(outcome)) => outcome.map_err(ClientError::Rpc),
            Ok(Err(_)) => Err(ClientError::Closed),
            Err(_) => {
                forget();
                Err(ClientError::Timeout)
            }
        }
    }

    /// The node's tip: the one of the hello, then every `tip` push. `changed()` fails once the
    /// connection is closed.
    #[must_use]
    pub fn tips(&self) -> watch::Receiver<Tip> {
        self.inner.tips.clone()
    }

    /// Every push from now on (the tip included).
    #[must_use]
    pub fn pushes(&self) -> broadcast::Receiver<Push> {
        self.inner.pushes.subscribe()
    }

    /// Whether the connection is closed.
    #[must_use]
    pub fn is_closed(&self) -> bool {
        *self.inner.closed.borrow()
    }

    /// Resolves once the connection is closed.
    pub async fn closed(&self) {
        let mut rx = self.inner.closed.clone();
        let _ = rx.wait_for(|c| *c).await;
    }
}
