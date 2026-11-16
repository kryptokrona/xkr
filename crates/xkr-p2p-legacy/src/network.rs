//! Network identifiers (`CRYPTONOTE_NETWORK` in `cryptonote_config.h`).

use xkr_consensus::params::Network;

use crate::constants::{MAINNET_P2P_PORT, TESTNET_P2P_PORT};

/// Mainnet `CRYPTONOTE_NETWORK` = `f14bb8c8-b256-452e-eef0-b499ab716ccc`.
pub const MAINNET_NETWORK_ID: [u8; 16] = [
    0xf1, 0x4b, 0xb8, 0xc8, 0xb2, 0x56, 0x45, 0x2e, 0xee, 0xf0, 0xb4, 0x99, 0xab, 0x71, 0x6c, 0xcc,
];

/// `CRYPTONOTE_NETWORK` of a `-DTEST_NET=ON` (`USE_TESTNET`) build =
/// `f14cb8c8-b356-452e-eef0-b499ab716ccc`. Note that two bytes differ from mainnet
/// (index 1: `0x4b -> 0x4c`, index 4: `0xb2 -> 0xb3`).
///
/// The C++ `NetNodeConfig::getTestnet()` path (`m_network_id.data[0] += 1`) is dead code in
/// kryptokronad: the daemon has no `--testnet` flag, so only the compile-time id matters.
pub const TESTNET_NETWORK_ID: [u8; 16] = [
    0xf1, 0x4c, 0xb8, 0xc8, 0xb3, 0x56, 0x45, 0x2e, 0xee, 0xf0, 0xb4, 0x99, 0xab, 0x71, 0x6c, 0xcc,
];

/// Network id sent in `basic_node_data.network_id`.
#[must_use]
pub const fn network_id(network: Network) -> [u8; 16] {
    match network {
        Network::Mainnet => MAINNET_NETWORK_ID,
        Network::Testnet => TESTNET_NETWORK_ID,
    }
}

/// Default P2P listen port (`P2P_DEFAULT_PORT`).
#[must_use]
pub const fn default_p2p_port(network: Network) -> u16 {
    match network {
        Network::Mainnet => MAINNET_P2P_PORT,
        Network::Testnet => TESTNET_P2P_PORT,
    }
}
