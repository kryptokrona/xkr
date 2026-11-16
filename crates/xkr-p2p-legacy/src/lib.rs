//! xkr-p2p-legacy — the Kryptokrona v1.1.x P2P protocol (Levin + epee portable storage).
//!
//! Layers, each usable on its own:
//!
//! - [`wire`]: every P2P (1001–1006) and block-chain (2001–2010) message with its exact epee
//!   keys, plus [`wire::LegacyMessage`], the Levin body for the vendored `xkr-levin` codec.
//! - [`peer`]: tokio connections — handshake, timed sync, ping, request/response
//!   correlation, timeouts — reporting to the node through [`peer::PeerEvent`]s.
//! - [`peerlist`]: white/gray peer list bookkeeping as plain data.
//! - [`ratelimit`]: per-connection token buckets for incoming messages.
//! - [`sync`]: the per-peer sync state machine as data plus [`sync::step`]; it emits
//!   [`sync::SyncEvent`]s (blocks, transactions) for the node to verify and store. This
//!   crate performs no consensus validation.
//!
//! Reference: the C++ node, github.com/kryptokrona/kryptokrona, `src/p2p/*` and
//! `src/cryptonote_protocol/*`.

pub mod blob;
pub mod constants;
pub mod network;
pub mod peer;
pub mod peerlist;
pub mod ratelimit;
pub mod sync;
pub mod wire;

pub use peer::{
    Accepted, BoxedPeerStream, Handshaked, LegacyConfig, LegacyPeer, LocalState, PeerError,
    PeerEvent, PeerEventKind, PeerHandle, PeerInfo, PeerStream,
};
pub use peerlist::PeerList;
pub use sync::{ChainView, PeerSync, SyncAction, SyncEvent, SyncInput, step};
pub use wire::{LegacyCommand, LegacyMessage, ProtocolMessage};
