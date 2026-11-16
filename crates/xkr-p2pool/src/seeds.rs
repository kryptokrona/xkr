//! Built-in sidechain seed peers (`seed_nodes`, `src/p2p_server.cpp`) in one place.
//!
//! The default sidechain gets the C++ seeds (a host name is resolved, DNS TXT first, like C++)
//! and long-running peers of the live sidechain. The mini and nano sidechains have none, like
//! C++. Edit the lists here; nothing else names a peer.

use crate::config::SidechainConfig;

/// Seeds of the default sidechain, `host:port`.
pub const DEFAULT_SEEDS: &[&str] = &[
    // The C++ p2pool's built-in seeds.
    "deploy.cloud.cbh.kth.se:20211",
    "209.38.232.36:37889",
    "92.34.6.2:37889",
    // Peers of the live sidechain.
    "149.28.87.122:37889",
    "130.254.47.155:37889",
    "198.135.48.179:37889",
    "107.172.250.250:37889",
    "112.224.196.23:37889",
    "223.81.200.134:37889",
    "130.237.83.246:20211",
    "130.237.83.250:20211",
];

/// Seeds of the mini sidechain (none).
pub const MINI_SEEDS: &[&str] = &[];

/// Seeds of the nano sidechain (none).
pub const NANO_SEEDS: &[&str] = &[];

/// The seeds of a sidechain (C++ `is_default()` / `is_mini()` / `is_nano()`; any other
/// sidechain has none).
#[must_use]
pub fn seeds_for(config: &SidechainConfig) -> &'static [&'static str] {
    if config.is_default() {
        DEFAULT_SEEDS
    } else if config.is_mini() {
        MINI_SEEDS
    } else if config.is_nano() {
        NANO_SEEDS
    } else {
        &[]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seed_lists() {
        assert_eq!(seeds_for(&SidechainConfig::main()).len(), 11);
        assert!(seeds_for(&SidechainConfig::mini()).is_empty());
        let private = SidechainConfig {
            password: "secret".into(),
            ..SidechainConfig::main()
        };
        assert!(seeds_for(&private).is_empty());
        for s in DEFAULT_SEEDS {
            let (host, port) = s.rsplit_once(':').unwrap();
            assert!(!host.is_empty());
            assert!(port.parse::<u16>().is_ok_and(|p| p > 0), "{s}");
        }
    }
}
