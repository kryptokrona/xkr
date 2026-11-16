//! Block major version per main-chain height as the sidechain checks it
//! (`SideChain::network_major_version`, `src/side_chain.cpp`, with the fork 13 and fork 14
//! steps of the Kryptokrona p2pool patches), and the coinbase version per major version
//! (`coin::coinbase_tx_version`, `src/coin_config.h`).
//!
//! The base table is always the mainnet one (`external/src/hardforks/hardforks.cpp`): Kryptokrona
//! addresses carry no network, so the C++ sidechain is mainnet-typed on every network. The fork
//! heights come from xkrd's own network parameters, so the sidechain and the node never
//! disagree.

use xkr_consensus::NetworkParams;

/// Block major version from fork 13 on.
pub const FORK13_MAJOR_VERSION: u8 = 6;
/// Block major version from fork 14 on (version-2 coinbase with view tags).
pub const FORK14_MAJOR_VERSION: u8 = 7;
/// Coinbase transaction version below fork 14 (`coin::COINBASE_TX_VERSION`).
pub const COINBASE_TX_VERSION_1: u8 = 1;
/// Coinbase transaction version from fork 14 on (`coin::COINBASE_TX_VERSION_2`).
pub const COINBASE_TX_VERSION_2: u8 = 2;

/// `(version, first height)` of the mainnet table (`mainnet_hard_forks`).
const MAINNET_TABLE: [(u8, u64); 5] = [(1, 0), (2, 1), (3, 2), (4, 3), (5, 4)];

/// Fork heights of the sidechain: a fork at height `F` governs blocks `F + 1` and later; 0 means
/// not scheduled (C++ `s_fork13Height` / `s_fork14Height`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ForkHeights {
    /// Fork 13 height (major version 6), 0 if not scheduled.
    pub fork13: u64,
    /// Fork 14 height (major version 7, version-2 coinbase), 0 if not scheduled.
    pub fork14: u64,
}

impl ForkHeights {
    /// The heights of a network as xkrd runs it (including `--fork13-height` /
    /// `--fork14-height` on the testnet).
    #[must_use]
    pub fn from_params(params: &NetworkParams) -> Self {
        Self {
            fork13: params.fork13_height.map_or(0, u64::from),
            fork14: params.fork14_height.map_or(0, u64::from),
        }
    }

    /// C++ `SideChain::network_major_version(height)`.
    #[must_use]
    pub fn network_major_version(self, height: u64) -> u8 {
        let mut result = MAINNET_TABLE[0].0;
        for (version, from) in &MAINNET_TABLE[1..] {
            if height >= *from {
                result = *version;
            } else {
                break;
            }
        }
        if self.fork13 != 0 && height > self.fork13 && result < FORK13_MAJOR_VERSION {
            result = FORK13_MAJOR_VERSION;
        }
        if self.fork14 != 0 && height > self.fork14 && result < FORK14_MAJOR_VERSION {
            result = FORK14_MAJOR_VERSION;
        }
        result
    }
}

/// C++ `coin::coinbase_tx_version(major_version)`.
#[must_use]
pub const fn coinbase_tx_version(major_version: u8) -> u8 {
    if major_version >= FORK14_MAJOR_VERSION {
        COINBASE_TX_VERSION_2
    } else {
        COINBASE_TX_VERSION_1
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use xkr_consensus::Network;

    #[test]
    fn mainnet_table_and_fork_steps() {
        let none = ForkHeights::default();
        let majors: Vec<u8> = (0..7).map(|h| none.network_major_version(h)).collect();
        assert_eq!(majors, [1, 2, 3, 4, 5, 5, 5]);
        assert_eq!(none.network_major_version(3_000_001), 5);

        let mainnet = ForkHeights::from_params(Network::Mainnet.params());
        assert_eq!(
            mainnet,
            ForkHeights {
                fork13: 0,
                fork14: 3_000_000
            }
        );
        assert_eq!(mainnet.network_major_version(2_100_000), 5);
        assert_eq!(mainnet.network_major_version(3_000_000), 5);
        assert_eq!(mainnet.network_major_version(3_000_001), 7);

        let both = ForkHeights {
            fork13: 100,
            fork14: 200,
        };
        assert_eq!(both.network_major_version(100), 5);
        assert_eq!(both.network_major_version(101), 6);
        assert_eq!(both.network_major_version(200), 6);
        assert_eq!(both.network_major_version(201), 7);
        // A fork 13 height above fork 14 never lowers the version.
        let odd = ForkHeights {
            fork13: 300,
            fork14: 200,
        };
        assert_eq!(odd.network_major_version(301), 7);
    }

    #[test]
    fn coinbase_version_per_major() {
        assert_eq!(coinbase_tx_version(5), 1);
        assert_eq!(coinbase_tx_version(6), 1);
        assert_eq!(coinbase_tx_version(7), 2);
    }
}
