//! xkr-stratum: the CryptoNote stratum protocol for Kryptokrona miners and pools.
//!
//! | Module | Content |
//! |---|---|
//! | [`client`] | [`StratumPool`]: a pool client with login, pushed jobs, submits, keepalive, TLS (`stratum+ssl://`), fail-over with an optional return to the first pool, a login that can change while it runs (hugin-node `PoolConnector`, `pools.js`) |
//! | [`messages`] | the JSON messages (`login`, `job`, `submit`, `keepalived`, results and errors) as types, and the line framing, for pool servers |
//! | [`blob`] | job blob fields: nonce offset, `prev_id`, major version (hugin-utils) |
//! | [`target`] | compact share targets and the share check (hugin-utils `parseTarget` / `meetsTarget`) |
//!
//! Used by `xkr-miner` (pool mining) and `xkr-hugin` (a relay's jobs from a pool).
#![forbid(unsafe_code)]

pub mod blob;
pub mod client;
pub mod messages;
pub mod target;

pub use client::{
    DEFAULT_POOL_PORT, Failover, LoginExtras, PoolAddr, PoolJob, PoolStatus, RECONNECT_DELAY,
    ShareSubmit, StratumConfig, StratumPool, SubmitOutcome, VALID_JOBS, pool_addr, tls_connector,
};
pub use messages::{
    JobParams, KeepalivedParams, LoginParams, LoginResult, Message, MessageError, RpcError,
    StatusResult, SubmitParams,
};
