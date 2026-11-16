//! Read views of the chain for other components: [`NodeView`] implements the legacy sync
//! machine's [`ChainView`] (chain + pool), [`ChainSnapshot`] the pool's [`PoolChainState`].

use xkr_consensus::reward::is_spend_time_unlocked;
use xkr_consensus::{ForkSchedule, MAINNET, RingLookupError};
use xkr_p2p_legacy::sync::{ChainView, TemplateInfo};
use xkr_serialization::{block_hash, decode_block, transaction_hash_from_blob};
use xkr_storage::Reader;
use xkr_txpool::PoolChainState;
use xkr_types::{Hash, KeyImage, PublicKey, RawBlock};

use crate::chain::Chain;

/// The pool's view of the main chain: one storage snapshot plus the top index, block-size
/// median and wall clock taken when it was created.
pub struct ChainSnapshot {
    reader: Reader,
    top_index: u32,
    median_size: u64,
    now: u64,
    forks: ForkSchedule,
}

impl std::fmt::Debug for ChainSnapshot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChainSnapshot")
            .field("top_index", &self.top_index)
            .finish_non_exhaustive()
    }
}

impl ChainSnapshot {
    /// A snapshot over `reader`.
    #[must_use]
    pub fn new(reader: Reader, top_index: u32, median_size: u64, now: u64) -> Self {
        Self {
            reader,
            top_index,
            median_size,
            now,
            forks: MAINNET.forks(),
        }
    }

    /// This snapshot with the chain's fork table (fork-14 transaction-version rules for the
    /// pool; the default from [`Self::new`] is the mainnet table).
    #[must_use]
    pub fn with_forks(self, forks: ForkSchedule) -> Self {
        Self { forks, ..self }
    }

    /// The storage snapshot.
    #[must_use]
    pub fn reader(&self) -> &Reader {
        &self.reader
    }
}

impl PoolChainState for ChainSnapshot {
    fn top_index(&self) -> u32 {
        self.top_index
    }

    fn block_median_size(&self) -> u64 {
        self.median_size
    }

    fn fork_schedule(&self) -> ForkSchedule {
        self.forks
    }

    fn key_image_spent(&self, key_image: &KeyImage) -> bool {
        // Fail closed on storage errors.
        self.reader.is_key_image_spent(key_image).unwrap_or(true)
    }

    fn ring_members(
        &self,
        amount: u64,
        global_indexes: &[u32],
    ) -> Result<Vec<PublicKey>, RingLookupError> {
        // `ring_keys` applies the C++ ring order itself (ascending, each index once, missing
        // outputs left out), so any caller gets the consensus ring.
        let indexes: Vec<u64> = global_indexes.iter().map(|i| u64::from(*i)).collect();
        let members = self
            .reader
            .ring_keys(amount, &indexes)
            .map_err(|_| RingLookupError::InvalidGlobalIndex)?;
        members
            .into_iter()
            .map(|(_, key, unlock, _)| {
                if is_spend_time_unlocked(unlock, self.top_index, self.now) {
                    Ok(key)
                } else {
                    Err(RingLookupError::OutputLocked)
                }
            })
            .collect()
    }
}

/// [`ChainView`] over one storage snapshot of `chain` and its pool, for
/// `xkr_p2p_legacy::sync::step`. Create one per step (cheap: one read transaction).
pub struct NodeView<'a> {
    chain: &'a Chain,
    reader: Option<Reader>,
}

impl std::fmt::Debug for NodeView<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NodeView").finish_non_exhaustive()
    }
}

impl<'a> NodeView<'a> {
    /// A view of `chain` (if the snapshot cannot be opened every lookup answers "unknown").
    #[must_use]
    pub fn new(chain: &'a Chain) -> Self {
        let reader = match chain.reader() {
            Ok(r) => Some(r),
            Err(e) => {
                tracing::error!(error = %e, "cannot open a chain snapshot");
                None
            }
        };
        Self { chain, reader }
    }

    /// The underlying snapshot.
    #[must_use]
    pub fn reader(&self) -> Option<&Reader> {
        self.reader.as_ref()
    }
}

/// Parses a block blob into its id and transaction hashes.
#[must_use]
pub fn parse_block_template(blob: &[u8]) -> Option<TemplateInfo> {
    let block = decode_block(blob).ok()?;
    let hash = block_hash(&block).ok()?;
    Some(TemplateInfo {
        hash,
        tx_hashes: block.tx_hashes,
    })
}

impl ChainView for NodeView<'_> {
    fn top(&self) -> (u32, Hash) {
        let from_reader = self.reader.as_ref().and_then(|r| {
            let top = r.top_height().ok()??;
            let hash = r.block_hash(top).ok()??;
            Some((u32::try_from(top + 1).unwrap_or(u32::MAX), hash))
        });
        from_reader.unwrap_or_else(|| {
            let tip = self.chain.tip();
            (u32::try_from(tip.height + 1).unwrap_or(u32::MAX), tip.hash)
        })
    }

    fn main_chain_index(&self, hash: &Hash) -> Option<u32> {
        let h = self.reader.as_ref()?.height_of(hash).ok()??;
        u32::try_from(h).ok()
    }

    fn has_block(&self, hash: &Hash) -> bool {
        self.main_chain_index(hash).is_some() || self.chain.is_alt_block(hash)
    }

    fn block_hash_at(&self, index: u32) -> Option<Hash> {
        self.reader.as_ref()?.block_hash(u64::from(index)).ok()?
    }

    fn block_hashes(&self, start: u32, max: usize) -> Vec<Hash> {
        self.reader
            .as_ref()
            .and_then(|r| r.block_hashes(u64::from(start), max as u64).ok())
            .unwrap_or_default()
    }

    /// Blocks this node pruned (stripe pruning) are answered as missed, like unknown ones: the
    /// peer asks a node of the right stripe or a full node instead.
    fn raw_blocks(&self, hashes: &[Hash], found: &mut Vec<RawBlock>, missed: &mut Vec<Hash>) {
        let Some(reader) = self.reader.as_ref() else {
            missed.extend_from_slice(hashes);
            return;
        };
        let res = self.chain.with_compressor(|comp| {
            for h in hashes {
                let raw = match reader.height_of(h) {
                    Ok(Some(height)) => match reader.raw_block(height, comp) {
                        Ok(raw) => raw,
                        Err(xkr_storage::Error::Pruned(_)) => None,
                        Err(e) => {
                            tracing::warn!(error = %e, height, "cannot read a block to serve");
                            None
                        }
                    },
                    _ => None,
                };
                match raw {
                    Some(raw) => found.push(raw),
                    None => missed.push(*h),
                }
            }
        });
        if res.is_err() {
            // No decompression context: nothing was read.
            missed.extend_from_slice(hashes);
        }
    }

    /// A transaction of a pruned block is not served (`None`, like an unknown one).
    fn transaction_blob(&self, hash: &Hash) -> Option<Vec<u8>> {
        if let Some(b) = self.chain.pool().blob(hash) {
            return Some(b);
        }
        let reader = self.reader.as_ref()?;
        self.chain
            .with_compressor(|comp| reader.transaction_blob(hash, comp).ok().flatten())
            .ok()
            .flatten()
    }

    fn pool_hashes(&self) -> Vec<Hash> {
        self.chain.pool().hashes()
    }

    fn pool_txs_not_in(&self, known: &[Hash]) -> Vec<Vec<u8>> {
        self.chain.pool().txs_not_in(known)
    }

    fn pool_txs_not_in_capped(&self, known: &[Hash], max_bytes: usize) -> Vec<Vec<u8>> {
        self.chain.pool().txs_not_in_capped(known, max_bytes)
    }

    fn parse_block_template(&self, blob: &[u8]) -> Option<TemplateInfo> {
        parse_block_template(blob)
    }

    fn tx_hash(&self, tx_blob: &[u8]) -> Hash {
        transaction_hash_from_blob(tx_blob)
    }
}
