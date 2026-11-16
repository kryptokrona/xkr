//! xkr-txpool — the transaction memory pool.
//!
//! Mirrors the C++ `TransactionPool`, `TransactionPoolCleanWrapper` and the pool half of
//! `Core::addTransactionToPool` / `Core::isTransactionValidForPool`
//! (`src/cryptonote_core/{transaction_pool,transaction_pool_cleaner,core}.cpp`):
//!
//! * admission: decoding, `validateSemantic`, mixin limits, `MAX_EXTRA_SIZE_POOL` (2200 bytes,
//!   Hugin messages live only in the pool), maximum size from the block-size median, minimum
//!   fee unless it is a fusion transaction, the fusion-transaction cap, then the consensus
//!   input checks against the chain (spent key images, ring members, ring signatures);
//! * key-image conflicts between pool transactions;
//! * "recently seen" and "recently deleted" filters so relayed duplicates are not re-validated;
//! * eviction: time to live (`CRYPTONOTE_MEMPOOL_TX_LIVETIME`), mixin rule changes, and a byte
//!   budget (lowest fee per byte first);
//! * maintenance on chain changes: [`TxPool::remove_for_block`], [`TxPool::revalidate`] and
//!   [`TxPool::readd`];
//! * ring order: [`ring_global_indexes`] (ascending global indexes without duplicates, the
//!   order C++ verifies ring signatures in).
//!
//! The chain is reached only through the [`PoolChainState`] trait, so this crate does not
//! depend on storage. The pool is a thread-safe handle (`parking_lot::RwLock`, no async):
//! expensive validation runs without holding the lock.

mod error;
mod pool;

pub use error::PoolError;
pub use pool::{
    PoolChainState, PoolConfig, PoolEntry, PoolStats, PoolTxSummary, TemplateSelection, TxPool,
    TxSource, canonical_ring_indexes, priority_cmp, ring_global_indexes,
};

#[cfg(test)]
mod tests;
