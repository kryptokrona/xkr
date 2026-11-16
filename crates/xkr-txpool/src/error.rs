//! Pool admission errors.

use xkr_consensus::TxError;
use xkr_consensus::validation::PoolPolicyError;
use xkr_types::Hash;

/// Why a transaction was not added to the pool.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PoolError {
    /// The transaction is already in the pool (C++ `checkIfTransactionPresent`).
    #[error("transaction already in the pool")]
    AlreadyInPool,
    /// The same transaction was processed (accepted or rejected) a short time ago
    /// (C++ `transactionRecentlySeen`).
    #[error("transaction was processed recently")]
    RecentlySeen,
    /// The transaction was evicted a short time ago (C++ `isTransactionRecentlyDeleted`).
    #[error("transaction was recently removed from the pool")]
    RecentlyDeleted,
    /// The blob is not a transaction.
    #[error("transaction does not decode: {0}")]
    Decode(String),
    /// Pool policy (mixin, extra size, size, fee).
    #[error(transparent)]
    Policy(#[from] PoolPolicyError),
    /// Consensus rule violated (`validateSemantic` / `validateTransaction`).
    #[error("invalid transaction: {0}")]
    Invalid(#[from] TxError),
    /// A key image is already used by another pool transaction.
    #[error("key image already used by pool transaction {0}")]
    KeyImageConflict(Hash),
    /// `CRYPTONOTE_MEMPOOL_MAX_FUSION_TRANSACTIONS` reached.
    #[error("the pool is at the fusion transaction limit")]
    FusionLimit,
    /// The byte budget is exhausted and the transaction pays too little to evict others.
    #[error("the pool is full")]
    PoolFull,
}
