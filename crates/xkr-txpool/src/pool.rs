//! The pool itself.

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;

use parking_lot::RwLock;
use xkr_consensus::params::{
    CRYPTONOTE_MEMPOOL_MAX_FUSION_TRANSACTIONS, CRYPTONOTE_MEMPOOL_RECENTLY_SEEN_TX_LIVETIME,
    CRYPTONOTE_MEMPOOL_TX_LIVETIME, CRYPTONOTE_NUMBER_OF_PERIODS_TO_FORGET_TX_DELETED_FROM_POOL,
};
use xkr_consensus::reward::{
    is_fusion_transaction, max_transaction_allowed_size, mixin_limits, validate_mixin,
};
use xkr_consensus::validation::{PoolPolicyError, check_pool_policy, is_valid_for_block_template};
use xkr_consensus::{
    BlockTransaction, ForkSchedule, MAINNET, ResolvedInput, RingLookupError,
    check_transaction_version, validate_semantic, validate_transaction,
};
use xkr_serialization::{decode_transaction, transaction_hash_from_blob, transaction_prefix_hash};
use xkr_types::{Hash, KeyImage, PublicKey, Transaction, TxInput};

use crate::error::PoolError;

/// What the pool needs to know about the main chain (implemented by `xkr-chain` over a
/// storage snapshot).
pub trait PoolChainState {
    /// Index of the top block (`getTopBlockIndex`); pool transactions are validated as if
    /// they were in the next block.
    fn top_index(&self) -> u32;
    /// C++ `Core::blockMedianSize`: `max(median of the last 100 block sizes, full reward zone)`.
    fn block_median_size(&self) -> u64;
    /// Whether a key image is spent in the main chain.
    fn key_image_spent(&self, key_image: &KeyImage) -> bool;
    /// Ring member keys for `global_indexes` of `amount` as the C++ main chain
    /// (`DatabaseBlockchainCache::extractKeyOutputKeys(amount, top_index, ...)`) resolves
    /// them, including the unlock check. The pool passes the indexes in ring order
    /// ([`ring_global_indexes`]: ascending, no duplicates); the keys come back in that order,
    /// and an index without an output is left out (the shorter ring then fails the signature
    /// count or the signature, as in C++).
    fn ring_members(
        &self,
        amount: u64,
        global_indexes: &[u32],
    ) -> Result<Vec<PublicKey>, RingLookupError>;
    /// The fork table of the chain, for the transaction-version rules of fork 14
    /// (`docs/tx-v2.md`: version 2 only after the fork, version 1 only until the end of the
    /// grace period). The default is the mainnet table (fork 14 at
    /// [`xkr_consensus::forks::FORK_14_HEIGHT_MAINNET`]).
    fn fork_schedule(&self) -> ForkSchedule {
        MAINNET.forks()
    }
}

/// The global indexes a key input's ring is made of, in the order its signature covers them.
///
/// The packed offsets are summed with wrapping `u32` addition (C++ `Core::validateTransaction`,
/// [`TxInput::absolute_output_indexes`]); the result is then sorted ascending and duplicates
/// are removed. That is what C++ verifies against: `DatabaseBlockchainCache::extractKeyOutputs`
/// (`database_blockchain_cache.cpp`) reads the requested outputs in one batch (a repeated index
/// is read once) and walks them through a `std::map` keyed by `(amount, global index)`, so the
/// keys reach `checkRingSignature` sorted by global index and without duplicates. Offsets whose
/// running sum wraps past `u32::MAX` therefore still give an ascending ring, and a duplicate
/// shortens the ring, which fails the signature count
/// (`TRANSACTION_SIGNATURE_COUNT_VALIDATION_HEIGHT`, and always for version 2).
///
/// Version 2 (CLSAG, fork 14) resolves its rings by the same rule (`docs/tx-v2.md`). Empty for
/// a coinbase input.
#[must_use]
pub fn ring_global_indexes(input: &TxInput) -> Vec<u32> {
    canonical_ring_indexes(input.absolute_output_indexes())
}

/// Sorts absolute global indexes ascending and drops duplicates (the ring order of
/// [`ring_global_indexes`]). Idempotent.
#[must_use]
pub fn canonical_ring_indexes(mut absolute: Vec<u32>) -> Vec<u32> {
    absolute.sort_unstable();
    absolute.dedup();
    absolute
}

/// Where a transaction came from (filters differ).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TxSource {
    /// Relayed by a peer: subject to the "recently seen" filter.
    Peer,
    /// Submitted locally (`/sendrawtransaction`).
    Local,
    /// Re-added from a block popped by a reorg: skips the recently seen/deleted filters.
    Reorg,
}

/// Pool limits. Defaults are the C++ constants plus a byte budget.
#[derive(Debug, Clone)]
pub struct PoolConfig {
    /// `CRYPTONOTE_MEMPOOL_TX_LIVETIME` (24 h).
    pub tx_livetime: u64,
    /// `CRYPTONOTE_MEMPOOL_RECENTLY_SEEN_TX_LIVETIME` (10 min).
    pub recently_seen_livetime: u64,
    /// How long evicted hashes are refused
    /// (`tx_livetime * CRYPTONOTE_NUMBER_OF_PERIODS_TO_FORGET_TX_DELETED_FROM_POOL`).
    pub recently_deleted_livetime: u64,
    /// `CRYPTONOTE_MEMPOOL_MAX_FUSION_TRANSACTIONS`: fusion transactions are refused once the
    /// pool holds this many transactions (C++ compares with the total count).
    pub max_fusion_transactions: usize,
    /// Byte budget for all transaction blobs (not in C++; keeps a desktop node's memory bounded).
    pub max_bytes: usize,
    /// Most hashes the "recently seen" filter remembers within its 10 minutes (not in C++).
    /// Every unique relayed blob, valid or not, enters it; at the cap the oldest entries are
    /// forgotten first.
    pub max_seen_entries: usize,
}

impl Default for PoolConfig {
    fn default() -> Self {
        Self {
            tx_livetime: CRYPTONOTE_MEMPOOL_TX_LIVETIME,
            recently_seen_livetime: CRYPTONOTE_MEMPOOL_RECENTLY_SEEN_TX_LIVETIME,
            recently_deleted_livetime: CRYPTONOTE_MEMPOOL_TX_LIVETIME
                * CRYPTONOTE_NUMBER_OF_PERIODS_TO_FORGET_TX_DELETED_FROM_POOL,
            max_fusion_transactions: CRYPTONOTE_MEMPOOL_MAX_FUSION_TRANSACTIONS,
            max_bytes: 128 * 1024 * 1024,
            max_seen_entries: MAX_SEEN_ENTRIES,
        }
    }
}

/// Default of [`PoolConfig::max_seen_entries`]: about 10 MB of filter. At C++'s 10-minute
/// livetime that is 330 unique transactions per second, far above the network's rate.
pub const MAX_SEEN_ENTRIES: usize = 200_000;

/// Hash -> unix time, bounded in entries: an insertion-ordered history next to the map lets the
/// oldest entries go first when the cap is reached, in amortised O(1).
#[derive(Debug)]
struct RecentHashes<V> {
    map: HashMap<Hash, (u64, V)>,
    order: VecDeque<(u64, Hash)>,
}

impl<V> Default for RecentHashes<V> {
    fn default() -> Self {
        Self {
            map: HashMap::new(),
            order: VecDeque::new(),
        }
    }
}

impl<V> RecentHashes<V> {
    /// The value recorded for `hash` within the last `ttl` seconds.
    fn get_fresh(&self, hash: &Hash, now: u64, ttl: u64) -> Option<&V> {
        self.map
            .get(hash)
            .filter(|(t, _)| now.saturating_sub(*t) < ttl)
            .map(|(_, v)| v)
    }

    /// Records `hash` at `now`; forgets the oldest records beyond `cap`.
    fn insert(&mut self, hash: Hash, now: u64, value: V, cap: usize) {
        self.map.insert(hash, (now, value));
        self.order.push_back((now, hash));
        // `order` may hold stale duplicates of re-inserted hashes; it is bounded too.
        while self.order.len() > cap.max(1) {
            self.pop_oldest();
        }
    }

    fn pop_oldest(&mut self) {
        if let Some((t, h)) = self.order.pop_front()
            && self.map.get(&h).is_some_and(|(mt, _)| *mt == t)
        {
            self.map.remove(&h);
        }
    }

    /// Forgets records older than `ttl`.
    fn expire(&mut self, now: u64, ttl: u64) {
        self.map.retain(|_, (t, _)| now.saturating_sub(*t) < ttl);
        while self
            .order
            .front()
            .is_some_and(|(t, _)| now.saturating_sub(*t) >= ttl)
        {
            self.order.pop_front();
        }
    }
}

/// One pool transaction.
#[derive(Debug, Clone)]
pub struct PoolEntry {
    /// Transaction hash (`cn_fast_hash` of the blob).
    pub hash: Hash,
    /// The serialized transaction.
    pub blob: Arc<[u8]>,
    /// Decoded transaction.
    pub tx: Transaction,
    /// Fee (inputs minus outputs).
    pub fee: u64,
    /// Unix time it entered the pool.
    pub received: u64,
    /// Zero-fee fusion transaction.
    pub fusion: bool,
}

impl PoolEntry {
    /// Blob size in bytes.
    #[must_use]
    pub fn size(&self) -> usize {
        self.blob.len()
    }

    /// Fee per kilobyte, used for eviction and template ordering.
    #[must_use]
    pub fn fee_per_kb(&self) -> u64 {
        self.fee.saturating_mul(1024) / (self.blob.len().max(1) as u64)
    }

    fn key_images(&self) -> impl Iterator<Item = &KeyImage> {
        self.tx.prefix.inputs.iter().filter_map(|i| match i {
            TxInput::Key { key_image, .. } => Some(key_image),
            TxInput::Coinbase { .. } => None,
        })
    }
}

/// Counters for `/getinfo` and `/status`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PoolStats {
    /// Number of transactions.
    pub count: usize,
    /// Sum of blob sizes.
    pub bytes: usize,
    /// Number of fusion transactions.
    pub fusion: usize,
}

/// Hash, size, fee and arrival time of one pool transaction, without its body
/// (`get_miner_data` `tx_backlog`, `/get_pool`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PoolTxSummary {
    /// Transaction hash.
    pub hash: Hash,
    /// Blob size in bytes.
    pub size: usize,
    /// Fee (inputs minus outputs).
    pub fee: u64,
    /// Unix time it entered the pool.
    pub received: u64,
}

#[derive(Debug, Default)]
struct Inner {
    txs: HashMap<Hash, PoolEntry>,
    key_images: HashMap<KeyImage, Hash>,
    /// Hash -> unix time it was last processed (accepted or rejected), bounded
    /// ([`PoolConfig::max_seen_entries`]).
    seen: RecentHashes<()>,
    /// Hash -> why it failed decoding or `validate_semantic`, for the "recently seen" livetime.
    /// Both depend on the blob alone, so a local resubmission (which skips `seen`, like C++)
    /// gets the same answer without being decoded and checked again.
    rejected: RecentHashes<PoolError>,
    /// Hash -> unix time it was evicted.
    deleted: HashMap<Hash, u64>,
    bytes: usize,
}

impl Inner {
    fn remove(&mut self, hash: &Hash) -> Option<PoolEntry> {
        let entry = self.txs.remove(hash)?;
        for ki in entry.key_images() {
            if self.key_images.get(ki) == Some(hash) {
                self.key_images.remove(ki);
            }
        }
        self.bytes = self.bytes.saturating_sub(entry.size());
        Some(entry)
    }

    fn insert(&mut self, entry: PoolEntry) {
        for ki in entry.key_images() {
            self.key_images.insert(*ki, entry.hash);
        }
        self.bytes += entry.size();
        self.txs.insert(entry.hash, entry);
    }
}

/// Thread-safe transaction pool.
#[derive(Debug, Default)]
pub struct TxPool {
    cfg: PoolConfig,
    inner: RwLock<Inner>,
}

impl TxPool {
    /// An empty pool.
    #[must_use]
    pub fn new(cfg: PoolConfig) -> Self {
        Self {
            cfg,
            inner: RwLock::new(Inner::default()),
        }
    }

    /// The configuration.
    #[must_use]
    pub fn config(&self) -> &PoolConfig {
        &self.cfg
    }

    /// C++ `Core::addTransactionToPool`: validates `blob` against `chain` and adds it.
    /// Returns the transaction hash.
    ///
    /// Callers that also add blocks must hold the chain's pool gate (see `xkr-chain`) so a
    /// block cannot be committed between validation and insertion.
    pub fn add(
        &self,
        blob: Vec<u8>,
        chain: &impl PoolChainState,
        source: TxSource,
        now: u64,
    ) -> Result<Hash, PoolError> {
        let hash = transaction_hash_from_blob(&blob);
        {
            let inner = self.inner.read();
            if inner.txs.contains_key(&hash) {
                return Err(PoolError::AlreadyInPool);
            }
            if source != TxSource::Reorg {
                let ttl = self.cfg.recently_seen_livetime;
                if source == TxSource::Peer && inner.seen.get_fresh(&hash, now, ttl).is_some() {
                    return Err(PoolError::RecentlySeen);
                }
                if let Some(e) = inner.rejected.get_fresh(&hash, now, ttl) {
                    return Err(e.clone());
                }
                if inner
                    .deleted
                    .get(&hash)
                    .is_some_and(|t| now.saturating_sub(*t) < self.cfg.recently_deleted_livetime)
                {
                    return Err(PoolError::RecentlyDeleted);
                }
            }
        }

        // The size limit only needs the blob length, so it runs before the blob is decoded
        // (decoding is the memory-hungry step). Same rule and error as `check_pool_policy`,
        // which C++ applies after decoding; only the order of checks changes.
        let max = max_transaction_allowed_size(chain.block_median_size());
        if blob.len() as u64 > max {
            return Err(PoolError::Policy(PoolPolicyError::TooBig {
                size: blob.len() as u64,
                max,
            }));
        }

        // Context-free part (decode, `validate_semantic`), then the expensive part (ring
        // signatures, storage lookups), both without the lock.
        let mut permanent = None;
        let checked = match Self::decode_checked(&blob) {
            Ok((tx, fee)) => {
                Self::validate(hash, &blob, &tx, fee, chain).map(|fusion| (tx, fee, fusion))
            }
            Err(e) => {
                permanent = Some(e.clone());
                Err(e)
            }
        };

        let mut inner = self.inner.write();
        let cap = self.cfg.max_seen_entries;
        inner.seen.insert(hash, now, (), cap);
        if let Some(e) = permanent {
            inner.rejected.insert(hash, now, e, cap);
        }
        let (tx, fee, fusion) = checked?;
        if inner.txs.contains_key(&hash) {
            return Err(PoolError::AlreadyInPool);
        }
        let entry = PoolEntry {
            hash,
            blob: Arc::from(blob),
            tx,
            fee,
            received: now,
            fusion,
        };
        if let Some(other) = entry.key_images().find_map(|ki| inner.key_images.get(ki)) {
            return Err(PoolError::KeyImageConflict(*other));
        }
        if fusion && inner.txs.len() >= self.cfg.max_fusion_transactions {
            return Err(PoolError::FusionLimit);
        }
        if inner.bytes + entry.size() > self.cfg.max_bytes {
            self.make_room(&mut inner, &entry, now)?;
        }
        tracing::debug!(%hash, fee, size = entry.size(), "transaction added to pool");
        inner.insert(entry);
        Ok(hash)
    }

    /// Evicts transactions paying less per byte than `entry` until it fits.
    fn make_room(&self, inner: &mut Inner, entry: &PoolEntry, now: u64) -> Result<(), PoolError> {
        let mut candidates: Vec<(u64, Hash, usize)> = inner
            .txs
            .values()
            .map(|e| (e.fee_per_kb(), e.hash, e.size()))
            .collect();
        candidates.sort_unstable_by_key(|c| c.0);
        let rate = entry.fee_per_kb();
        let mut freed = 0usize;
        let needed = (inner.bytes + entry.size()).saturating_sub(self.cfg.max_bytes);
        let mut victims = Vec::new();
        for (r, h, size) in candidates {
            if freed >= needed {
                break;
            }
            if r >= rate {
                return Err(PoolError::PoolFull);
            }
            freed += size;
            victims.push(h);
        }
        if freed < needed {
            return Err(PoolError::PoolFull);
        }
        for h in victims {
            inner.remove(&h);
            inner.deleted.insert(h, now);
        }
        Ok(())
    }

    /// The parts of admission that depend on the blob alone: decoding and C++
    /// `validateSemantic`. Returns the transaction and its fee.
    fn decode_checked(blob: &[u8]) -> Result<(Transaction, u64), PoolError> {
        let tx = decode_transaction(blob).map_err(|e| PoolError::Decode(e.to_string()))?;
        let fee = validate_semantic(&tx)?;
        Ok((tx, fee))
    }

    /// C++ `Core::isTransactionValidForPool` (without the pool-state parts) for a transaction
    /// that passed [`Self::decode_checked`]. Returns whether it is a fusion transaction.
    fn validate(
        hash: Hash,
        blob: &[u8],
        tx: &Transaction,
        fee: u64,
        chain: &impl PoolChainState,
    ) -> Result<bool, PoolError> {
        let top = chain.top_index();
        check_transaction_version(&chain.fork_schedule(), u64::from(top) + 1, tx)?;
        check_pool_policy(tx, blob.len(), fee, top, chain.block_median_size())?;

        let mut rings: Vec<(bool, Result<Vec<PublicKey>, RingLookupError>)> =
            Vec::with_capacity(tx.prefix.inputs.len());
        for input in &tx.prefix.inputs {
            if let TxInput::Key {
                amount, key_image, ..
            } = input
            {
                let spent = chain.key_image_spent(key_image);
                let ring = chain.ring_members(*amount, &ring_global_indexes(input));
                rings.push((spent, ring));
            }
        }
        let resolved: Vec<ResolvedInput<'_>> = rings
            .iter()
            .map(|(spent, ring)| ResolvedInput {
                key_image_spent: *spent,
                ring: ring.as_deref().map_err(|e| *e),
            })
            .collect();
        let entry = BlockTransaction {
            tx,
            hash,
            prefix_hash: transaction_prefix_hash(&tx.prefix),
            blob_size: blob.len(),
            inputs: &resolved,
        };
        let mut seen = HashSet::new();
        validate_transaction(&entry, top, false, &mut seen)?;
        Ok(fee == 0 && is_fusion_transaction(tx, blob.len(), top))
    }

    /// A block was added to the main chain: drop its transactions and every pool transaction
    /// that spends one of its key images (C++ `removeTransaction` + `actualizePoolTransactionsLite`).
    /// Returns the hashes removed because of a key-image conflict.
    pub fn remove_for_block(&self, tx_hashes: &[Hash], spent_key_images: &[KeyImage]) -> Vec<Hash> {
        let mut inner = self.inner.write();
        for h in tx_hashes {
            inner.remove(h);
        }
        let conflicting: Vec<Hash> = spent_key_images
            .iter()
            .filter_map(|ki| inner.key_images.get(ki).copied())
            .collect();
        for h in &conflicting {
            inner.remove(h);
        }
        conflicting
    }

    /// C++ `Core::actualizePoolTransactions`, run after the main chain switched to another
    /// branch: every pool transaction is validated again against `chain` (the new main chain)
    /// and those that no longer pass are removed (after a reorg a ring index can point at
    /// another output, an output can be locked or gone, a key image spent). Removed hashes are
    /// not put on the "recently deleted" list (C++ only removes them), so a later relay is
    /// validated again. Returns the removed hashes.
    ///
    /// The caller holds the chain's pool gate for writing, so nothing is added meanwhile.
    pub fn revalidate(&self, chain: &impl PoolChainState) -> Vec<Hash> {
        let stale: Vec<Hash> = {
            let inner = self.inner.read();
            inner
                .txs
                .values()
                .filter(|e| Self::validate(e.hash, &e.blob, &e.tx, e.fee, chain).is_err())
                .map(|e| e.hash)
                .collect()
        };
        if !stale.is_empty() {
            let mut inner = self.inner.write();
            for h in &stale {
                inner.remove(h);
            }
        }
        stale
    }

    /// Transactions of blocks popped by a reorg go back to the pool (validated against the new
    /// chain; failures are dropped). Returns how many were re-added.
    pub fn readd(&self, blobs: Vec<Vec<u8>>, chain: &impl PoolChainState, now: u64) -> usize {
        blobs
            .into_iter()
            .flat_map(|b| self.add(b, chain, TxSource::Reorg, now))
            .count()
    }

    /// C++ `TransactionPoolCleanWrapper::clean`: evicts expired transactions and those whose
    /// mixin is no longer allowed at `top_index`, and forgets old seen/deleted hashes.
    /// Returns the evicted hashes.
    pub fn clean(&self, now: u64, top_index: u32) -> Vec<Hash> {
        let mut inner = self.inner.write();
        let limits = mixin_limits(u64::from(top_index));
        let expired: Vec<Hash> = inner
            .txs
            .values()
            .filter(|e| {
                now.saturating_sub(e.received) >= self.cfg.tx_livetime
                    || validate_mixin(&e.tx, limits).is_err()
            })
            .map(|e| e.hash)
            .collect();
        for h in &expired {
            inner.remove(h);
            inner.deleted.insert(*h, now);
        }
        let seen_ttl = self.cfg.recently_seen_livetime;
        let deleted_ttl = self.cfg.recently_deleted_livetime;
        inner.seen.expire(now, seen_ttl);
        inner.rejected.expire(now, seen_ttl);
        inner
            .deleted
            .retain(|_, t| now.saturating_sub(*t) < deleted_ttl);
        expired
    }

    /// Evicts every transaction that `allowed` rejects (fork 14: version-1 transactions once
    /// the grace period is over) and remembers them as deleted. Returns their hashes.
    pub fn remove_disallowed(&self, now: u64, allowed: impl Fn(&Transaction) -> bool) -> Vec<Hash> {
        let mut inner = self.inner.write();
        let victims: Vec<Hash> = inner
            .txs
            .values()
            .filter(|e| !allowed(&e.tx))
            .map(|e| e.hash)
            .collect();
        for h in &victims {
            inner.remove(h);
            inner.deleted.insert(*h, now);
        }
        victims
    }

    /// Whether `hash` is in the pool.
    #[must_use]
    pub fn contains(&self, hash: &Hash) -> bool {
        self.inner.read().txs.contains_key(hash)
    }

    /// Number of transactions.
    #[must_use]
    pub fn len(&self) -> usize {
        self.inner.read().txs.len()
    }

    /// Whether the pool is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.inner.read().txs.is_empty()
    }

    /// Counters.
    #[must_use]
    pub fn stats(&self) -> PoolStats {
        let inner = self.inner.read();
        PoolStats {
            count: inner.txs.len(),
            bytes: inner.bytes,
            fusion: inner.txs.values().filter(|e| e.fusion).count(),
        }
    }

    /// All transaction hashes (`getPoolTransactionHashes`).
    #[must_use]
    pub fn hashes(&self) -> Vec<Hash> {
        self.inner.read().txs.keys().copied().collect()
    }

    /// Blobs of the pool transactions whose hash is not in `known` (`NOTIFY_REQUEST_TX_POOL`).
    #[must_use]
    pub fn txs_not_in(&self, known: &[Hash]) -> Vec<Vec<u8>> {
        let known: HashSet<&Hash> = known.iter().collect();
        self.inner
            .read()
            .txs
            .values()
            .filter(|e| !known.contains(&e.hash))
            .map(|e| e.blob.to_vec())
            .collect()
    }

    /// [`Self::txs_not_in`] with at most `max_bytes` of blobs (the answer to
    /// `NOTIFY_REQUEST_TX_POOL` is capped). Copies no more than it returns.
    #[must_use]
    pub fn txs_not_in_capped(&self, known: &[Hash], max_bytes: usize) -> Vec<Vec<u8>> {
        let known: HashSet<&Hash> = known.iter().collect();
        let inner = self.inner.read();
        let mut total = 0usize;
        let mut out = Vec::new();
        for e in inner.txs.values().filter(|e| !known.contains(&e.hash)) {
            total = total.saturating_add(e.size());
            if total > max_bytes {
                break;
            }
            out.push(e.blob.to_vec());
        }
        out
    }

    /// [`PoolTxSummary`] of every transaction, in pool (hash map) order. Clones no transaction.
    #[must_use]
    pub fn summaries(&self) -> Vec<PoolTxSummary> {
        self.inner
            .read()
            .txs
            .values()
            .map(|e| PoolTxSummary {
                hash: e.hash,
                size: e.size(),
                fee: e.fee,
                received: e.received,
            })
            .collect()
    }

    /// Hashes of the pool transactions whose extra carries `payment_id`
    /// (`TransactionPool::getTransactionHashesByPaymentId`). Clones no transaction.
    #[must_use]
    pub fn hashes_by_payment_id(&self, payment_id: &Hash) -> Vec<Hash> {
        self.inner
            .read()
            .txs
            .values()
            .filter(|e| xkr_types::payment_id(&e.tx.prefix.extra).as_ref() == Some(payment_id))
            .map(|e| e.hash)
            .collect()
    }

    /// One transaction blob.
    #[must_use]
    pub fn blob(&self, hash: &Hash) -> Option<Vec<u8>> {
        self.inner.read().txs.get(hash).map(|e| e.blob.to_vec())
    }

    /// Hash and blob of every transaction.
    #[must_use]
    pub fn blobs(&self) -> Vec<(Hash, Arc<[u8]>)> {
        self.inner
            .read()
            .txs
            .values()
            .map(|e| (e.hash, Arc::clone(&e.blob)))
            .collect()
    }

    /// One entry.
    #[must_use]
    pub fn entry(&self, hash: &Hash) -> Option<PoolEntry> {
        self.inner.read().txs.get(hash).cloned()
    }

    /// Snapshot of every entry (RPC).
    #[must_use]
    pub fn entries(&self) -> Vec<PoolEntry> {
        self.inner.read().txs.values().cloned().collect()
    }

    /// Transactions for a block template at `height`, best fee per byte first, skipping those
    /// C++ keeps out of templates (`validateBlockTemplateTransaction`: extra >= 128 bytes,
    /// mixin outside the limits), until `max_size` bytes.
    #[must_use]
    pub fn select_for_template(&self, height: u64, max_size: usize) -> Vec<PoolEntry> {
        let inner = self.inner.read();
        let mut v: Vec<&PoolEntry> = inner
            .txs
            .values()
            .filter(|e| is_valid_for_block_template(&e.tx, height))
            .collect();
        v.sort_unstable_by(|a, b| {
            b.fee_per_kb()
                .cmp(&a.fee_per_kb())
                .then(a.received.cmp(&b.received))
        });
        let mut total = 0usize;
        let mut out = Vec::new();
        for e in v {
            if total + e.size() > max_size {
                continue;
            }
            total += e.size();
            out.push(e.clone());
        }
        out
    }

    /// C++ `Core::fillBlockTemplate`: picks pool transactions for a block at `height`.
    ///
    /// Transactions are walked in C++ priority order ([`priority_cmp`]: fee per byte, then
    /// smaller, then older). First the zero-fee (fusion) transactions from the cheap end while
    /// their total stays within `fusion_max_size` (`FUSION_TX_MAX_SIZE`); then every
    /// transaction in order, zero-fee ones within `median_size` and paying ones within
    /// `max_total_size` (`min(125 % of the median, max cumulative size) - coinbase reserve`).
    /// Transactions C++ keeps out of templates (`validateBlockTemplateTransaction`: extra of
    /// 128 bytes or more, mixin outside the limits at `height`) and transactions whose key
    /// images are already used by a selected one (`TransactionSpentInputsChecker`) are skipped.
    #[must_use]
    pub fn fill_block_template(
        &self,
        height: u64,
        median_size: usize,
        max_total_size: usize,
        fusion_max_size: usize,
    ) -> TemplateSelection {
        self.fill_block_template_with(height, median_size, max_total_size, fusion_max_size, |_| {
            true
        })
    }

    /// [`Self::fill_block_template`] that also skips the transactions `allowed` rejects (the
    /// chain passes the fork-14 version rule of `height`).
    #[must_use]
    pub fn fill_block_template_with(
        &self,
        height: u64,
        median_size: usize,
        max_total_size: usize,
        fusion_max_size: usize,
        allowed: impl Fn(&Transaction) -> bool,
    ) -> TemplateSelection {
        let inner = self.inner.read();
        let mut ordered: Vec<&PoolEntry> = inner.txs.values().collect();
        ordered.sort_by(|a, b| priority_cmp(a, b));

        let mut sel = TemplateSelection::default();
        let mut spent: HashSet<KeyImage> = HashSet::new();
        let mut chosen: HashSet<Hash> = HashSet::new();

        // Pass 1: fusion transactions, cheapest end first, while the fee is zero.
        for e in ordered.iter().rev().take_while(|e| e.fee == 0) {
            if fusion_max_size < sel.size + e.size() {
                continue;
            }
            if !is_valid_for_block_template(&e.tx, height) || !allowed(&e.tx) {
                continue;
            }
            if !have_spent_inputs(e, &mut spent) {
                sel.size += e.size();
                chosen.insert(e.hash);
                sel.txs.push((*e).clone());
            }
        }

        // Pass 2: everything in priority order.
        for e in &ordered {
            let limit = if e.fee == 0 {
                median_size
            } else {
                max_total_size
            };
            if limit < sel.size + e.size() {
                continue;
            }
            if !is_valid_for_block_template(&e.tx, height) || !allowed(&e.tx) {
                continue;
            }
            // A fusion transaction taken in pass 1 fails here on its own key images, like C++.
            if chosen.contains(&e.hash) || have_spent_inputs(e, &mut spent) {
                continue;
            }
            sel.size += e.size();
            sel.fee = sel.fee.wrapping_add(e.fee);
            chosen.insert(e.hash);
            sel.txs.push((*e).clone());
        }
        sel
    }
}

/// Transactions chosen by [`TxPool::fill_block_template`], in block order.
#[derive(Debug, Clone, Default)]
pub struct TemplateSelection {
    /// The transactions.
    pub txs: Vec<PoolEntry>,
    /// Sum of their blob sizes (`transactionsSize`).
    pub size: usize,
    /// Sum of their fees.
    pub fee: u64,
}

/// C++ `TransactionPool::TransactionPriorityComparator` as an ordering (best first): higher
/// `fee / size` (compared as `fee_a * size_b` against `fee_b * size_a` in 128 bits), then the
/// smaller blob, then the older transaction.
#[must_use]
pub fn priority_cmp(a: &PoolEntry, b: &PoolEntry) -> Ordering {
    let left = u128::from(a.fee) * entry_size(b);
    let right = u128::from(b.fee) * entry_size(a);
    right
        .cmp(&left)
        .then(a.size().cmp(&b.size()))
        .then(a.received.cmp(&b.received))
}

fn entry_size(e: &PoolEntry) -> u128 {
    e.size() as u128
}

/// C++ `TransactionSpentInputsChecker::haveSpentInputs`: inserts the key images one by one
/// and reports the first one already present (earlier ones stay inserted, like C++).
fn have_spent_inputs(e: &PoolEntry, spent: &mut HashSet<KeyImage>) -> bool {
    e.key_images().any(|ki| !spent.insert(*ki))
}
