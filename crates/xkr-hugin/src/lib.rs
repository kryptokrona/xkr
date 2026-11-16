//! xkr-hugin: a hugin-node relay in Rust (`docs/hugin-relay.md`).
//!
//! Hugin apps (hugin-desktop, hugin-native) send end-to-end encrypted direct messages through
//! hugin-nodes: store-and-forward relays on the Hyperswarm DHT that keep messages for 24 hours,
//! gossip them to each other and are paid with proof-of-work shares of an XKR mining job. This
//! crate ports hugin-node 2.0.0 so every xkrd can be such a relay; its jobs come from p2pool,
//! which receives the shares under the node operator's fee address.
//!
//! | Module | Content |
//! |---|---|
//! | [`protocol`] | bare-rpc frame codec and the JSON packets |
//! | [`pow`] | share verification (CN-Turtle-Lite v2), targets, message checks, `pow.auth` signatures |
//! | [`store`] | the bounded 24 h message pool with an append-only file |
//! | [`pool`] | job source trait; the stratum pool client (hugin-node's `PoolConnector`, plain TCP or TLS) from `xkr-stratum` |
//! | [`p2pool`] | the relay's job source on p2pool: local node first, public nodes as fall-back, fee address login, jobs only while the node is synchronized |
//! | [`relay`] | the relay: clients, node mesh, gossip, sync, bans, stats |
//! | [`identity`] | the node's XKR identity (`identity.json`, hugin-node `config.json` compatible) |
//!
//! The relay is transport-agnostic: [`relay::Relay::serve`] takes any
//! `AsyncRead + AsyncWrite` stream; xkrd feeds it `HyperConn`s from the Hugin topics.
#![forbid(unsafe_code)]

pub mod identity;
pub mod p2pool;
pub mod pool;
pub mod pow;
pub mod protocol;
pub mod relay;
pub mod store;

pub use identity::NodeIdentity;
pub use p2pool::{P2poolConfig, P2poolSource};
pub use pool::{
    ChainTip, JobSource, ManualJobSource, NoJobSource, PoolAddr, PoolJob, PoolStatus,
    StratumConfig, StratumPool,
};
pub use relay::{
    FeeInfo, HuginStats, HuginStatsSnapshot, P2poolInfo, PeerMeta, Relay, RelayConfig, Role,
};
