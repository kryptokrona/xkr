//! Genesis block: C++ `Currency::generateGenesisBlock` (`currency.cpp`).

use xkr_serialization::{DecodeError, EncodeError, block_hash, decode_transaction};
use xkr_types::{Block, BlockHeader, Hash};

use crate::params::{
    BLOCK_MAJOR_VERSION_1, BLOCK_MINOR_VERSION_0, GENESIS_COINBASE_TX_HEX, NetworkParams,
};

/// Mainnet genesis block hash (checked against a public node in the tests).
pub const MAINNET_GENESIS_HASH: &str =
    "7fb97df81221dd1366051b2d0bc7f49c66c22ac4431d879c895b06d66ef66f4c";

/// The compiled-in genesis coinbase could not be used (C++ logs "failed to parse coinbase tx
/// from hard coded blob" and refuses to start).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum GenesisError {
    /// `GENESIS_COINBASE_TX_HEX` is not hex.
    #[error("GENESIS_COINBASE_TX_HEX is not valid hex")]
    Hex,
    /// The coinbase does not decode.
    #[error(transparent)]
    Decode(#[from] DecodeError),
    /// The block does not encode.
    #[error(transparent)]
    Encode(#[from] EncodeError),
}

/// C++ `Currency::generateGenesisBlock`: the coinbase from `GENESIS_COINBASE_TX_HEX`, major 1,
/// minor 0, timestamp 0, previous hash 0, nonce [`NetworkParams::genesis_nonce`] (70).
pub fn genesis_block(params: &NetworkParams) -> Result<Block, GenesisError> {
    let blob = hex::decode(GENESIS_COINBASE_TX_HEX).map_err(|_| GenesisError::Hex)?;
    let miner_tx = decode_transaction(&blob)?;
    Ok(Block {
        header: BlockHeader {
            major_version: BLOCK_MAJOR_VERSION_1,
            minor_version: BLOCK_MINOR_VERSION_0,
            timestamp: 0,
            prev_hash: Hash::ZERO,
            nonce: params.genesis_nonce,
        },
        parent_block: None,
        miner_tx,
        tx_hashes: Vec::new(),
    })
}

/// Hash of [`genesis_block`] (C++ `Currency::genesisBlockHash`).
pub fn genesis_hash(params: &NetworkParams) -> Result<Hash, GenesisError> {
    Ok(block_hash(&genesis_block(params)?)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::params::{MAINNET, TESTNET};

    #[test]
    fn mainnet_genesis_hash() {
        assert_eq!(
            genesis_hash(&MAINNET).unwrap().to_string(),
            MAINNET_GENESIS_HASH
        );
    }

    #[test]
    fn testnet_genesis_is_the_mainnet_genesis() {
        // m_testnet is never set in the C++ code, so the testnet keeps nonce 70.
        assert_eq!(
            genesis_hash(&TESTNET).unwrap().to_string(),
            MAINNET_GENESIS_HASH
        );
    }
}
