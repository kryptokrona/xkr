//! xkr-p2p-hyper: the Hyperswarm/HyperDHT transport of xkrd.
//!
//! HyperDHT, holepunching, UDX and secret-stream only exist in JavaScript today, so they run in
//! JavaScript (`bridge/`) on N Bare worker threads inside this process (`bridge/src/worker.js`,
//! started through bare-kit's C API), and this crate talks to each worker with the binary frame
//! protocol of `docs/hyper-bridge-protocol.md` over an in-memory channel.
//!
//! Peer connections come out as [`HyperConn`]s, ordinary `AsyncRead + AsyncWrite` byte streams,
//! so the node runs Levin (or anything else) over them unchanged.
//!
//! | Module | Content |
//! |---|---|
//! | [`frame`] | frame types, TLVs, the `tokio_util` codec (golden vectors shared with JS) |
//! | [`bridge`] | [`Bridge`]: Hello, keepalive, requests, credits, worker routing |
//! | [`transport`] | in-memory byte channel between a link and a worker thread |
//! | [`bare`] | [`BareWorkers`]: Bare worker threads through bare-kit's C API (only unsafe code) |
//! | [`conn`] | [`HyperConn`] and its close/peer-info types |
//! | [`topic`] | XKR topics (`cn_fast_hash` of a name) and Hugin topics (hugin-node's derivation) |
//!
//! ```no_run
//! # async fn demo() -> Result<(), xkr_p2p_hyper::BridgeError> {
//! use futures::StreamExt;
//! use xkr_p2p_hyper::{AnnounceMode, BareConfig, Bridge, BridgeConfig, OpenFlags, topic};
//!
//! let (bridge, _workers) =
//!     Bridge::start_in_process(BridgeConfig::default(), &BareConfig::default()).await?;
//! let t = topic::xkr_blocks_topic(topic::XkrNetwork::Mainnet);
//! bridge.announce(t, AnnounceMode::Both, None).await?;
//! let mut incoming = bridge.incoming().expect("first call");
//! for peer in bridge.lookup(t, 16).await? {
//!     let _conn = bridge.open(peer.public_key, Some(t), OpenFlags::NONE).await?;
//! }
//! while let Some(conn) = incoming.next().await {
//!     let conn = conn?; // Err = bridge died; reconnecting is up to the caller
//!     println!("peer {:?}", conn.info());
//! }
//! # Ok(()) }
//! ```
// Only `bare::ffi` allows unsafe code.
#![deny(unsafe_code)]

pub mod bare;
pub mod bridge;
pub mod conn;
pub mod frame;
pub mod topic;
pub mod transport;

pub use bare::{BareConfig, BareWorkers};
pub use bridge::{
    AnnounceMode, Bridge, BridgeConfig, BridgeError, HuginMid, Incoming, MAX_WORKERS, OpenFlags,
    PeerAddr, SessionParams,
};
pub use conn::{CloseHandle, CloseInfo, CloseReason, ConnInfo, HyperConn};
pub use frame::{Ack, DhtStatus, PeerInfo};
pub use topic::{HuginTopic, XkrNetwork};
