//! xkr-chain — chain state, block verification and storage glue for the Kryptokrona node.
//!
//! [`Chain`] owns the [`xkr_storage::Store`], an in-memory window of the last few thousand
//! block headers (difficulty, timestamp and size windows), the main-chain tip and the
//! alternative blocks. It is shared as `Arc<Chain>`: one writer at a time (internally
//! serialised), any number of readers on redb MVCC snapshots.
//!
//! Sync batches go through a four-stage pipeline ([`Chain::add_blocks`]):
//!
//! 1. **prepare** (parallel, rayon): decode, block/transaction hashes, proof-of-work slow hash,
//!    `validateSemantic` ([`prepare`]);
//! 2. **context** (sequential): header, coinbase, timestamps, difficulty, size, reward,
//!    checkpoint, proof-of-work target, spent key images and ring members against storage plus
//!    the blocks accepted earlier in the batch ([`verify`]);
//! 3. **signatures** (parallel, rayon): every ring signature of the batch;
//! 4. **store**: one redb write transaction for the accepted prefix.
//!
//! Single relayed blocks go through [`Chain::add_block`], which also handles alternative
//! blocks and switches the main chain when an alternative branch has more cumulative
//! difficulty. See `docs/node-architecture.md`.
//!
//! Mining support ([`template`]): [`Chain::block_template`] (C++ `getBlockTemplate`),
//! [`Chain::assemble_mined_block`] (`submitBlock`), [`Chain::miner_data`] (`get_miner_data`, also
//! read by the embedded p2pool together with [`Chain::subscribe_tip`]), and wallet decoys
//! [`Chain::random_outputs`] (`getRandomOutputs`). See `docs/mining.md`.
//!
//! Forks ([`forks`]): [`Chain::fork_status`] (active/next fork, readiness signalled through
//! block minor versions) and [`Chain::block_rate`] (blocks of the last hour against the target).
//! See `docs/forks.md`.
//!
//! State hashes ([`state`]): one hash per 4 096-block span over the data every node keeps
//! (also a pruned one), checked against a compiled-in list ([`Chain::update_state_hashes`]).
//! See `docs/storage.md` "State hashes".

mod chain;
pub mod checkpoints;
mod error;
pub mod forks;
pub mod headers;
pub mod miner_data;
mod outputs;
pub mod prepare;
pub mod state;
pub mod template;
pub mod verify;
mod view;

pub use chain::{
    AddBlockOutcome, AddBlockResult, BatchError, BatchErrorKind, BatchReport, Chain, ChainConfig,
    StageTimes, TipInfo, load_verified, unix_now,
};
pub use checkpoints::Checkpoints;
pub use error::{ChainError, RejectReason};
pub use forks::{BlockRate, BlockRateStatus, ForkStatus};
pub use miner_data::{MainChainHeader, MinerData, MinerDataError};
pub use state::{StateHashes, StateMismatch, StateUpdate, StateView};
pub use template::{
    BlockTemplate, MinerAddress, SubmitError, TemplateError, reserved_offset, set_merge_mining_tag,
};
pub use view::{ChainSnapshot, NodeView, parse_block_template};

#[cfg(test)]
mod tests;
