//! xkr-p2pool: the Kryptokrona p2pool sidechain, embedded in xkrd and compatible byte for byte
//! with the C++ p2pool (`kryptokrona/p2pool`, with its fork 13 and fork 14 patches).
//!
//! | Module | Content (C++ mirrored) |
//! |---|---|
//! | [`difficulty`] | [`Difficulty`]: the 128-bit `difficulty_type` with its wrapping rules, `check_pow`, stratum target (`src/common.h`, `src/util.cpp`) |
//! | [`order`] | the hash order of `hash::operator<`, `xorshift64star`, `umul128` high part |
//! | [`varint`] | `readVarint` / `writeVarint` (`src/util.h`) |
//! | [`config`] | [`SidechainConfig`]: default, mini, nano and JSON sidechains, checks, consensus id (`src/side_chain.cpp`) |
//! | [`forks`] | block major version per main-chain height, coinbase version per major version |
//! | [`keys`] | deterministic coinbase keys, one-time output keys and view tags (`src/crypto.cpp`) |
//! | [`wallet`] | [`MinerWallet`]: payout keys, order, `SEKR…` text (`src/wallet.cpp`) |
//! | [`block`] | [`PoolBlock`]: a share (`src/pool_block.h`) |
//! | [`parse`] | reading a share exactly as C++ does (`src/pool_block_parser.inl`) |
//! | [`serialize`] | writing a share, aux hash, proof-of-work blob, tx key seed, share id (`src/pool_block.cpp`) |
//! | [`mainchain`] | what the sidechain asks of the main chain, the header cache (`ChainMain`, `MinerData`, `src/p2pool.cpp`) |
//! | [`pplns`] | PPLNS window, reward split, coinbase outputs (`SideChain::get_shares`, `split_reward`, `decompose_outputs`) |
//! | [`sidechain`] | [`SideChain`]: share store, verification, difficulty, depths, fork choice, pruning (`src/side_chain.cpp`) |
//! | [`seeds`] | the built-in seed peers |
#![forbid(unsafe_code)]

pub mod block;
pub mod config;
pub mod difficulty;
pub mod forks;
pub mod keys;
pub mod mainchain;
pub mod order;
pub mod p2p;
pub mod parse;
pub mod pplns;
pub mod seeds;
pub mod serialize;
pub mod sidechain;
pub mod stratum;
pub mod template;
pub mod varint;
pub mod wallet;

pub use block::{FullId, MAX_BLOCK_SIZE, OffchainState, PoolBlock, TxOutput};
pub use config::{ConfigError, SidechainConfig, short_id};
pub use difficulty::Difficulty;
pub use forks::ForkHeights;
pub use mainchain::{ChainMain, MainChainCache, MainChainTip, MainChainView, seed_height};
pub use order::{HashOrd, hash_cmp, hash_less};
pub use parse::{
    ParseError, SidechainView, StaticView, parse_compact_pool_block, parse_pool_block,
};
pub use pplns::{MinerShare, PplnsError, PplnsWindow, split_reward};
pub use serialize::{IdRegions, MainchainData, PowBlob};
pub use sidechain::{
    AddError, BlockRejected, ExternalOutcome, SideChain, SidechainEvent, SidechainOptions,
};
pub use template::{BlockTemplate, PoolNode, PoolTemplates, Template, TemplateMinerData};
pub use wallet::MinerWallet;

/// Software id xkrd writes into its shares (`sidechainExtraBuf[0]`) and into the version record
/// of its first peer list: the ASCII bytes `XKRD` as a little-endian `u32`. The C++ p2pool
/// writes 0.
pub const SOFTWARE_ID: u32 = u32::from_le_bytes(*b"XKRD");

/// Software version next to [`SOFTWARE_ID`] (`sidechainExtraBuf[1]`): xkrd's version as
/// `major << 16 | minor << 8 | patch`, the layout of `P2POOL_VERSION`.
pub const SOFTWARE_VERSION: u32 = (parse_u32(env!("CARGO_PKG_VERSION_MAJOR")) << 16)
    | (parse_u32(env!("CARGO_PKG_VERSION_MINOR")) << 8)
    | parse_u32(env!("CARGO_PKG_VERSION_PATCH"));

/// Software id of the C++ p2pool (`SoftwareID::P2Pool`).
pub const SOFTWARE_ID_P2POOL: u32 = 0;

/// P2P protocol version announced in the peer list version record (1.4).
pub const PROTOCOL_VERSION: u32 = 0x0001_0004;

/// Decimal digits to a number at compile time (package version parts).
const fn parse_u32(s: &str) -> u32 {
    let b = s.as_bytes();
    let mut i = 0;
    let mut v = 0u32;
    while i < b.len() {
        v = v * 10 + (b[i] - b'0') as u32;
        i += 1;
    }
    v
}

/// What a software id stands for, for logs and the console.
#[must_use]
pub fn software_name(id: u32) -> &'static str {
    match id {
        SOFTWARE_ID => "xkrd",
        SOFTWARE_ID_P2POOL => "p2pool",
        0x624F_6F47 => "GoObserver",
        _ => "unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn software_id_and_version() {
        assert_eq!(SOFTWARE_ID, 0x4452_4B58);
        assert_eq!(SOFTWARE_ID.to_le_bytes(), *b"XKRD");
        assert_eq!(
            SOFTWARE_VERSION >> 16,
            parse_u32(env!("CARGO_PKG_VERSION_MAJOR"))
        );
        assert_eq!(parse_u32("0"), 0);
        assert_eq!(parse_u32("17"), 17);
        assert_eq!(software_name(0), "p2pool");
        assert_eq!(software_name(SOFTWARE_ID), "xkrd");
    }
}
