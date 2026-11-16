//! xkr-consensus: the consensus rules of the Kryptokrona C++ node as pure functions.
//!
//! | Module | C++ source |
//! |---|---|
//! | [`params`] | `src/config/cryptonote_config.h` (mainnet + testnet as [`params::Network`]) |
//! | [`forks`] | `FORK_HEIGHTS`, `UPGRADE_HEIGHT_V*`, `*_HEIGHT` rule constants: the fork table ([`ForkSchedule`]), fork 14 / tx v2 (`docs/tx-v2.md`) |
//! | [`version`] | `UpgradeManager::getBlockMajorVersion`, `CachedBlock::getBlockLongHash` |
//! | [`difficulty`] | `Currency::getNextDifficulty`/`nextDifficulty`, `difficulty.cpp`, `getDifficultyForNextBlock` |
//! | [`reward`] | `Currency::getBlockReward`, `getPenalizedAmount`, `maxBlockCumulativeSize`, `Mixins`, fusion |
//! | [`validation`] | `Core::addBlock`, `validateBlock`, `validateTransaction`, `validateSemantic`, `checkProofOfWork` |
//! | [`genesis`] | `Currency::generateGenesisBlock` |
//!
//! Nothing here touches storage: chain history and ring members are passed in as slices.
#![forbid(unsafe_code)]

pub mod difficulty;
pub mod forks;
pub mod genesis;
pub mod params;
pub mod reward;
pub mod validation;
pub mod version;

pub use difficulty::{difficulty_for_next_block, difficulty_window, next_difficulty};
pub use forks::{
    Fork, Fork14Params, ForkSchedule, RingSizeRule, Rules, SUPPORTED_FORK_INDEX, TxVersions,
};
pub use genesis::{GenesisError, genesis_block, genesis_hash};
pub use params::{MAINNET, Network, NetworkParams, TESTNET};
pub use reward::get_block_reward;
pub use validation::{
    AcceptedBlock, BlockError, BlockTransaction, ChainContext, ConsensusError, ResolvedInput,
    RingLookupError, TxError, check_proof_of_work, check_proof_of_work_with_hash,
    check_transaction_version, validate_block, validate_semantic, validate_transaction,
};
pub use version::{PowVariant, block_major_version_for_height, pow_variant_for};
