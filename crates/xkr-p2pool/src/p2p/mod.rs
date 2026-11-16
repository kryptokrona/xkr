//! The sidechain P2P protocol of the C++ p2pool (`src/p2p_server.h`, `src/p2p_server.cpp`),
//! independent of the transport.
//!
//! | Module | Content |
//! |---|---|
//! | [`message`] | the twelve messages and their bytes |
//! | [`handshake`] | challenge, solution with its small proof of work, peer ids |
//! | [`peers`] | peer lists, the version record, bans, the broadcast throttle |
//! | [`session`] | one connection as a state machine without I/O |
//! | [`conn`] | that state machine over any `AsyncRead + AsyncWrite` stream |
//! | [`server`] | [`P2pServer`]: connections, peer list, bans, syncing and broadcasting |
//! | [`dialer`] | seed and peer entries to addresses, outgoing TCP |
//! | [`peerfile`] | `p2pool_peers.txt` |
//!
//! The same bytes run over TCP (to C++ nodes and xkrd) and over Hyperswarm connections on the
//! sidechain's topic ([`hyperswarm_topic_key`], derived from the consensus id). By
//! default xkrd only connects out; it listens for TCP peers only when given an address to
//! listen on ([`P2pConfig::listen`]).

pub mod conn;
pub mod dialer;
pub mod handshake;
pub mod message;
pub mod peerfile;
pub mod peers;
pub mod server;
pub mod session;

use std::net::SocketAddr;

use xkr_types::Hash;

pub use conn::{Command, ConnEnd, ConnEvent, run_connection};
pub use handshake::{HandshakeError, PeerIds};
pub use message::{Message, MessageId, PeerEntry, WireError};
pub use peers::{BanKey, BanList, BroadcastThrottle, PeerBook, VersionRecord};
pub use server::{
    Clock, P2pEvent, P2pServer, P2pServerConfig, P2pStats, PeerStatus, PeerStream, SharedMainChain,
    SharedSideChain, Transport,
};
pub use session::{Direction, Domain, Event, PeerError, PeerSession, SessionConfig};

/// `DEFAULT_P2P_PORT`.
pub const DEFAULT_P2P_PORT: u16 = 37889;
/// `DEFAULT_P2P_PORT_MINI`.
pub const DEFAULT_P2P_PORT_MINI: u16 = 37888;
/// `DEFAULT_P2P_PORT_NANO`.
pub const DEFAULT_P2P_PORT_NANO: u16 = 37890;

/// Connection limits and the listen address (`Params`: `--p2p`, `--out-peers`, `--in-peers`,
/// `--p2p-allow-same-ip`; `src/params.h:109-111`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct P2pConfig {
    /// Address to accept TCP peers on; `None` (the default) connects out only.
    pub listen: Option<SocketAddr>,
    /// Port announced in `LISTEN_PORT`: the listen port, or the default port when not
    /// listening (C++ peers require a value in 1..65535).
    pub external_port: u16,
    /// Outgoing TCP connections to keep.
    pub out_peers: usize,
    /// Incoming TCP connections accepted.
    pub in_peers: usize,
    /// Incoming connections accepted from localhost.
    pub in_peers_localhost: usize,
    /// Allow more than one connection per IP address (IPv6: per /64).
    pub allow_same_ip: bool,
}

impl Default for P2pConfig {
    fn default() -> Self {
        Self {
            listen: None,
            external_port: DEFAULT_P2P_PORT,
            out_peers: 10,
            in_peers: 450,
            in_peers_localhost: 10,
            allow_same_ip: false,
        }
    }
}

impl P2pConfig {
    /// Whether incoming TCP connections are accepted.
    #[must_use]
    pub fn accepts_incoming(&self) -> bool {
        self.listen.is_some()
    }
}

/// A random `u64` from the OS random source.
#[must_use]
pub fn random_u64() -> u64 {
    use rand_core::RngCore as _;
    xkr_crypto::os_rng().next_u64()
}

/// Domain of the inner hash of [`hyperswarm_topic_name`].
pub const HYPERSWARM_TOPIC_DOMAIN: &str = "p2pool-topic";

/// The name of a sidechain's Hyperswarm topic:
/// `xkr/<network>/p2pool/v1/<hex(cn_fast_hash("p2pool-topic" || consensus id))>`. The consensus
/// id is hashed first, so nodes of a private sidechain meet only each other and the topic does
/// not show the id.
#[must_use]
pub fn hyperswarm_topic_name(network: xkr_consensus::Network, consensus_id: &Hash) -> String {
    let mut data = Vec::with_capacity(HYPERSWARM_TOPIC_DOMAIN.len() + consensus_id.0.len());
    data.extend_from_slice(HYPERSWARM_TOPIC_DOMAIN.as_bytes());
    data.extend_from_slice(&consensus_id.0);
    let net = match network {
        xkr_consensus::Network::Mainnet => "mainnet",
        xkr_consensus::Network::Testnet => "testnet",
    };
    format!(
        "xkr/{net}/p2pool/v1/{}",
        hex::encode(xkr_crypto::cn_fast_hash(&data).0)
    )
}

/// The sidechain's Hyperswarm topic key: `cn_fast_hash` of [`hyperswarm_topic_name`] (the XKR
/// topic rule of `xkr-p2p-hyper`, `xkr_topic(name)`). xkrd joins it on its shared Hyperswarm
/// worker with its own DHT identity and hands every connection on it to
/// [`P2pServer::attach`].
#[must_use]
pub fn hyperswarm_topic_key(network: xkr_consensus::Network, consensus_id: &Hash) -> [u8; 32] {
    xkr_crypto::cn_fast_hash(hyperswarm_topic_name(network, consensus_id).as_bytes()).0
}

#[cfg(test)]
mod tests {
    use xkr_consensus::Network;

    use super::*;
    use crate::config::SidechainConfig;

    /// The topic of a sidechain: the name spelled out here, its key the Keccak of the name,
    /// different per network and per sidechain.
    #[test]
    fn hyperswarm_topic_derivation() {
        let id = SidechainConfig::main().consensus_id();
        let mut inner = b"p2pool-topic".to_vec();
        inner.extend_from_slice(&id.0);
        assert_eq!(inner.len(), 12 + 32);
        let inner_hex = hex::encode(xkr_crypto::cn_fast_hash(&inner).0);
        let name = hyperswarm_topic_name(Network::Mainnet, &id);
        assert_eq!(name, format!("xkr/mainnet/p2pool/v1/{inner_hex}"));
        assert_eq!(name.len(), "xkr/mainnet/p2pool/v1/".len() + 64);
        assert!(!name.contains(&id.to_hex()), "the topic name hides the id");
        // pinned: the default (mainnet) sidechain
        assert_eq!(
            name,
            "xkr/mainnet/p2pool/v1/673e93986c40520df95d74ffcc15e78a4b76890f7d2feda958f41c45876ca0b4"
        );
        assert_eq!(
            hex::encode(hyperswarm_topic_key(Network::Mainnet, &id)),
            "a85203fd4b5baf060109bd86691b8ba4f8d297ad0fdab591bb602ae71268535c"
        );
        assert_eq!(
            hyperswarm_topic_key(Network::Mainnet, &id),
            xkr_crypto::cn_fast_hash(name.as_bytes()).0
        );
        assert_eq!(
            hyperswarm_topic_name(Network::Testnet, &id),
            format!("xkr/testnet/p2pool/v1/{inner_hex}")
        );
        let keys = [
            hyperswarm_topic_key(Network::Mainnet, &id),
            hyperswarm_topic_key(Network::Testnet, &id),
            hyperswarm_topic_key(Network::Mainnet, &SidechainConfig::mini().consensus_id()),
            hyperswarm_topic_key(Network::Mainnet, &SidechainConfig::nano().consensus_id()),
        ];
        for (i, a) in keys.iter().enumerate() {
            for b in &keys[i + 1..] {
                assert_ne!(a, b);
            }
        }
    }
}
