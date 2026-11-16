//! Errors: [`ChainError`] for failures of the node itself (storage, corrupt data),
//! [`RejectReason`] for blocks that consensus (or policy) refuses.

use xkr_consensus::{BlockError, ConsensusError, GenesisError};
use xkr_serialization::EncodeError;
use xkr_types::Hash;

/// The node could not complete an operation (not the block's fault).
#[derive(Debug, thiserror::Error)]
pub enum ChainError {
    /// Database error.
    #[error("storage: {0}")]
    Storage(#[from] xkr_storage::Error),
    /// The compiled-in genesis block is unusable.
    #[error("genesis: {0}")]
    Genesis(#[from] GenesisError),
    /// A stored object could not be re-encoded.
    #[error("encode: {0}")]
    Encode(#[from] EncodeError),
    /// The database belongs to another network.
    #[error("database genesis {found} does not match the network genesis {expected}")]
    WrongGenesis {
        /// Genesis hash in the database.
        found: Hash,
        /// Genesis hash of the configured network.
        expected: Hash,
    },
    /// Stored data does not decode or is inconsistent.
    #[error("corrupt chain data: {0}")]
    Corrupt(String),
    /// A main-chain block is needed whole (a reorg popping it, serving it) but this node pruned
    /// its ring signatures (`docs/storage.md` "Stripe pruning").
    #[error("block {0} is pruned on this node (its ring signatures were dropped)")]
    Pruned(u64),
}

impl ChainError {
    /// Whether the database was closed ([`crate::Chain::close`]): the node is stopping, so a
    /// caller that still runs stops quietly instead of reporting a failure.
    #[must_use]
    pub fn is_closed(&self) -> bool {
        matches!(self, Self::Storage(e) if e.is_closed())
    }

    /// Whether this is a pruned block, directly or as the storage error.
    #[must_use]
    pub fn is_pruned(&self) -> bool {
        matches!(
            self,
            Self::Pruned(_) | Self::Storage(xkr_storage::Error::Pruned(_))
        )
    }
}

impl From<redb::CommitError> for ChainError {
    fn from(e: redb::CommitError) -> Self {
        Self::Storage(e.into())
    }
}

impl From<redb::SetDurabilityError> for ChainError {
    fn from(e: redb::SetDurabilityError) -> Self {
        Self::Storage(e.into())
    }
}

/// Why a block was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RejectReason {
    /// A consensus rule (`Core::addBlock` / `validateTransaction`) failed.
    #[error(transparent)]
    Consensus(#[from] ConsensusError),
    /// The peer sent a different block than the id we asked for.
    #[error("block {got} is not the requested {expected}")]
    UnexpectedBlock {
        /// Requested id.
        expected: Hash,
        /// Id of the block received.
        got: Hash,
    },
    /// The raw block carries a different number of transactions than its template lists.
    #[error("block lists {hashes} transaction hashes but carries {txs} transactions")]
    TxCountMismatch {
        /// `tx_hashes.len()`.
        hashes: usize,
        /// Transactions supplied.
        txs: usize,
    },
    /// The block hash disagrees with a hard-coded checkpoint.
    #[error("block at height {0} does not match the checkpoint")]
    Checkpoint(u64),
    /// Alternative blocks at or below a checkpoint the main chain has reached are refused
    /// (`Checkpoints::reached_at_or_above`).
    #[error("alternative block at {height} is not above the reached checkpoint {checkpoint}")]
    BelowCheckpoint {
        /// Height of the alternative block.
        height: u64,
        /// Height of the highest checkpoint at or below the main-chain tip.
        checkpoint: u64,
    },
    /// Inside a batch, a block does not build on the previous one.
    #[error("block does not extend the previous block of the batch")]
    NotConnected,
}

impl RejectReason {
    /// Deserialization failure (C++ `DESERIALIZATION_FAILED`).
    pub(crate) fn decode(what: impl std::fmt::Display) -> Self {
        Self::Consensus(ConsensusError::DeserializationFailed(what.to_string()))
    }

    /// `Some((expected, got))` when the block carries an **older** major version than its
    /// height requires: the sender still follows the rules before a block-version fork
    /// ("peer is on an older fork", `docs/node-architecture.md`).
    #[must_use]
    pub fn older_block_version(&self) -> Option<(u8, u8)> {
        match self {
            Self::Consensus(ConsensusError::Block(BlockError::WrongVersion { expected, got }))
                if got < expected =>
            {
                Some((*expected, *got))
            }
            _ => None,
        }
    }
}
