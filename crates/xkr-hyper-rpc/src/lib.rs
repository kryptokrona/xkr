//! xkr-hyper-rpc: the wire protocol of xkrd's service topics on Hyperswarm.
//!
//! A node with the chain serves other programs over a Hyperswarm connection: chainless xkrd
//! modes (`--p2pool`, `--hugin` without `--node`), later wallets. The connection is a plain byte
//! stream (`xkr_p2p_hyper::HyperConn`, or anything `AsyncRead + AsyncWrite`); this crate is the
//! protocol on it, shared by the server in xkrd and every client. The contract, with examples,
//! is `docs/hyper-transport.md`, "Clients topic".
//!
//! # Frames
//!
//! Every message is one frame: a `u32` little-endian length `n`, then `n` bytes of UTF-8 JSON
//! (one object). `1 <= n <= MAX_FRAME` (1 MiB); a longer or empty frame, or a frame that is not
//! a message of the protocol, ends the connection ([`frame`]).
//!
//! # Session
//!
//! 1. The client sends a [`Hello`]: `{"v":1,"network":"mainnet","service":"node","client":"xkrd/1.0.0"}`.
//! 2. The server answers a [`HelloReply`] (`{"v":1,"height":…,"hash":…,"synced":…,"services":[…],
//!    "state_span":…,"state_hash":…,"pruning_seed":…}`) or a [`Refusal`] (`{"v":1,"error":"service",
//!    "message":…}`) and closes.
//! 3. Then requests `{"id":1,"method":"getinfo","params":{}}` and responses `{"id":1,"result":…}`
//!    or `{"id":1,"error":{"code":…,"message":…}}`, in any order (match by `id`), and server
//!    pushes `{"method":"tip","params":{"height":…,"hash":…,"synced":…}}` (no `id`).
//!
//! | Module | Content |
//! |---|---|
//! | [`frame`] | length-prefixed frames, JSON encoding |
//! | [`message`] | hello, reply, refusal, request, response, push, error codes |
//! | [`server`] | [`serve`]: one connection on the server side (hello, limits, calls, pushes) |
//! | [`client`] | [`Client`]: hello, concurrent calls, the tip channel |

pub mod client;
pub mod frame;
pub mod message;
pub mod server;

pub use client::{Client, ClientError, ClientOptions};
pub use frame::{FrameError, MAX_FRAME, read_frame, write_frame};
pub use message::{
    Hello, HelloReply, Push, Refusal, RefuseReason, Request, Response, RpcError, Service, Tip,
    VERSION, code,
};
pub use server::{Accepted, Handler, Refused, ServeEnd, ServerLimits, serve};
