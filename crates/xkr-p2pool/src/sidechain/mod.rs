//! The sidechain store and its rules (`SideChain`, `src/side_chain.cpp` / `src/side_chain.h`).
//!
//! [`SideChain`] holds every known share by id, by height and by coinbase merkle root, and keeps
//! the off-chain state of each ([`crate::block::OffchainState`]): depth, verified, invalid,
//! broadcast flags, local time, cached next difficulty. Shares come in through
//! [`SideChain::add_external_block`] (from peers: difficulty, main-chain height and proof of
//! work first) or [`SideChain::add_block`] (own shares and the block cache), are verified as
//! soon as their parent and uncles are, and the heaviest valid chain becomes the tip.
//!
//! | file | C++ |
//! |---|---|
//! | `mod.rs` | store, `add_external_block`, `add_block`, `get_missing_blocks`, `prune_old_blocks`, seen sets |
//! | `retarget.rs` | `get_difficulty` |
//! | `verify.rs` | `verify`, `verify_loop` |
//! | `depth.rs` | `update_depths` |
//! | `tip.rs` | `update_chain_tip`, `is_longer_chain` |
//! | `uncles.rs` | `fill_sidechain_data` (uncle choice for a new share) |
//!
//! Side effects that C++ performs directly (broadcast to peers, write to the block cache, ask
//! for a new block template) are queued as [`SidechainEvent`]s; the owner drains them with
//! [`SideChain::take_events`]. Time is passed in (`now`, seconds since the epoch).

mod depth;
mod retarget;
mod tip;
mod uncles;
mod verify;

use std::collections::{BTreeMap, HashMap, HashSet};

use xkr_types::{Hash, PublicKey};

use crate::block::{FullId, PoolBlock};
use crate::config::{MAIN_CHAIN_BLOCK_TIME, SidechainConfig, UNCLE_BLOCK_DEPTH};
use crate::difficulty::Difficulty;
use crate::forks::ForkHeights;
use crate::mainchain::{ChainMain, MainChainView, seed_height};
use crate::parse::SidechainView;
use crate::pplns::{PplnsError, PplnsParams, PplnsWindow, get_shares};

pub use retarget::{DifficultyData, next_difficulty};
pub use tip::ChainComparison;

/// Seen wallets are forgotten after this many seconds (72 h).
const SEEN_WALLET_TIMEOUT: u64 = 72 * 3600;
/// Seen wallets are pruned at most this often (5 min).
const SEEN_PRUNE_INTERVAL: u64 = 5 * 60;
/// "Already received" entries are forgotten after this many seconds (10 min).
const INCOMING_TIMEOUT: u64 = 10 * 60;

/// How a sidechain is run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SidechainOptions {
    /// The sidechain parameters (and with them the consensus id).
    pub config: SidechainConfig,
    /// Fork heights of the main chain.
    pub forks: ForkHeights,
    /// Coinbase unlock window of the main chain (20 on mainnet, 1 on the testnet).
    pub unlock_window: u64,
    /// No PPLNS weight cap (test sidechains only; every node of the sidechain must agree).
    pub uncapped_pplns: bool,
}

impl SidechainOptions {
    /// A capped sidechain.
    #[must_use]
    pub const fn new(config: SidechainConfig, forks: ForkHeights, unlock_window: u64) -> Self {
        Self {
            config,
            forks,
            unlock_window,
            uncapped_pplns: false,
        }
    }
}

/// Something the owner of the sidechain should act on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SidechainEvent {
    /// A new chain tip (C++ "new chain tip"): build a new template.
    NewTip {
        /// The tip's id.
        id: Hash,
        /// Its sidechain height.
        height: u64,
        /// Difficulty of the next share.
        next_difficulty: Difficulty,
        /// Its main-chain height (`txinGenHeight`).
        main_height: u64,
        /// The tip moved to another chain (reset share counters).
        alternative: bool,
    },
    /// Send this share to the peers.
    Broadcast(Hash),
    /// Write this valid share to the block cache.
    StoreInCache(Hash),
    /// A valid share close to the tip that is not the tip: a possible uncle, refresh the
    /// template.
    PossibleUncle(Hash),
    /// A share passed verification.
    Verified {
        /// Its id.
        id: Hash,
        /// Its height.
        height: u64,
    },
    /// A share failed verification (permanently).
    Invalid {
        /// Its id.
        id: Hash,
        /// Its height.
        height: u64,
    },
    /// Old shares were removed.
    Pruned {
        /// How many.
        blocks: usize,
        /// They were below this height.
        below_height: u64,
    },
}

/// Why [`SideChain::add_block`] did not take a share.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum AddError {
    /// A share with this id is already stored.
    #[error("share {0} is already stored")]
    Duplicate(Hash),
}

/// A share from a peer that is not taken and counts against the peer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum BlockRejected {
    /// Difficulty below the sidechain minimum.
    #[error("difficulty below the sidechain minimum")]
    DifficultyBelowMinimum,
    /// The coinbase height is not the height after the main-chain block it builds on.
    #[error("main-chain height {got}, expected {expected}")]
    WrongMainChainHeight {
        /// The share's coinbase height.
        got: u64,
        /// The height after its main-chain block.
        expected: u64,
    },
    /// The proof of work is below the share difficulty.
    #[error("proof of work below the share difficulty")]
    NotEnoughPow {
        /// It is a candidate for the next main-chain block and meets the main-chain difficulty:
        /// submit it to the daemon.
        submit_to_daemon: bool,
    },
    /// The share is already stored.
    #[error("share already stored")]
    Duplicate,
}

/// Why a share from a peer is ignored without counting against the peer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IgnoreReason {
    /// Difficulty well below the current one and below half of every share of the window.
    LowDifficulty,
    /// The main-chain block at its seed height is not known yet.
    SeedUnknown,
}

/// What [`SideChain::add_external_block`] did with a share.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExternalOutcome {
    /// Already stored.
    AlreadyKnown,
    /// Not stored, no fault of the peer.
    Ignored(IgnoreReason),
    /// Stored.
    Added {
        /// Parent and uncles not known yet: ask the peer for them.
        missing: Vec<Hash>,
        /// A candidate for the next main-chain block that meets the main-chain difficulty:
        /// submit it to the daemon.
        submit_to_daemon: bool,
        /// It is the main-chain block being watched for (a found block).
        found: Option<ChainMain>,
    },
}

/// The sidechain (C++ `SideChain`).
#[derive(Debug)]
pub struct SideChain {
    options: SidechainOptions,
    consensus_id: Hash,
    blocks: HashMap<Hash, PoolBlock>,
    by_height: BTreeMap<u64, Vec<Hash>>,
    by_merkle_root: HashMap<Hash, Hash>,
    tip: Option<Hash>,
    difficulty: Difficulty,
    incoming: HashMap<FullId, u64>,
    seen_wallets: HashMap<PublicKey, u64>,
    seen_wallets_pruned: u64,
    watch: Option<(ChainMain, Hash)>,
    events: Vec<SidechainEvent>,
    now: u64,
}

impl SideChain {
    /// An empty sidechain; the current difficulty starts at the minimum.
    #[must_use]
    pub fn new(options: SidechainOptions) -> Self {
        let consensus_id = options.config.consensus_id();
        let difficulty = options.config.min_difficulty();
        Self {
            options,
            consensus_id,
            blocks: HashMap::new(),
            by_height: BTreeMap::new(),
            by_merkle_root: HashMap::new(),
            tip: None,
            difficulty,
            incoming: HashMap::new(),
            seen_wallets: HashMap::new(),
            seen_wallets_pruned: 0,
            watch: None,
            events: Vec::new(),
            now: 0,
        }
    }

    /// The options it runs with.
    #[must_use]
    pub const fn options(&self) -> &SidechainOptions {
        &self.options
    }

    /// The sidechain parameters.
    #[must_use]
    pub const fn config(&self) -> &SidechainConfig {
        &self.options.config
    }

    /// The consensus id (also the tx key seed of the genesis share).
    #[must_use]
    pub const fn consensus_id(&self) -> &Hash {
        &self.consensus_id
    }

    /// Difficulty of the next share on the tip (`difficulty()`).
    #[must_use]
    pub const fn difficulty(&self) -> Difficulty {
        self.difficulty
    }

    /// The chain tip (`chainTip()`).
    #[must_use]
    pub fn chain_tip(&self) -> Option<&PoolBlock> {
        self.tip.and_then(|id| self.blocks.get(&id))
    }

    /// Cumulative difficulty of the tip (`total_hashes()`).
    #[must_use]
    pub fn total_hashes(&self) -> Difficulty {
        self.chain_tip()
            .map_or(Difficulty::ZERO, |t| t.cumulative_difficulty)
    }

    /// Local time the tip was added (`last_updated()`).
    #[must_use]
    pub fn last_updated(&self) -> u64 {
        self.chain_tip().map_or(0, |t| t.offchain.local_timestamp)
    }

    /// Number of stored shares.
    #[must_use]
    pub fn len(&self) -> usize {
        self.blocks.len()
    }

    /// No shares stored.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.blocks.is_empty()
    }

    /// A stored share (`find_block`).
    #[must_use]
    pub fn find_block(&self, id: &Hash) -> Option<&PoolBlock> {
        self.blocks.get(id)
    }

    /// A stored share by its coinbase merkle root (`find_block_by_merkle_root`): maps a
    /// main-chain block back to the share it came from.
    #[must_use]
    pub fn find_block_by_merkle_root(&self, root: &Hash) -> Option<&PoolBlock> {
        self.by_merkle_root
            .get(root)
            .and_then(|id| self.blocks.get(id))
    }

    /// The stored shares at a height, in the order they were added.
    pub fn blocks_at_height(&self, height: u64) -> impl Iterator<Item = &PoolBlock> {
        self.by_height
            .get(&height)
            .into_iter()
            .flatten()
            .filter_map(|id| self.blocks.get(id))
    }

    /// All stored shares, in no particular order.
    pub fn blocks(&self) -> impl Iterator<Item = &PoolBlock> {
        self.blocks.values()
    }

    /// The parent of a share, if stored (`get_parent`).
    #[must_use]
    pub fn parent_of(&self, block: &PoolBlock) -> Option<&PoolBlock> {
        self.blocks.get(&block.parent)
    }

    /// `get_cached_next_difficulty(id)`.
    #[must_use]
    pub fn cached_next_difficulty(&self, id: &Hash) -> Option<Difficulty> {
        self.blocks
            .get(id)
            .and_then(|b| b.offchain.cached_next_difficulty)
    }

    /// Wallets seen in the last 72 hours (`miner_count()`).
    #[must_use]
    pub fn miner_count(&self) -> usize {
        self.seen_wallets.len()
    }

    /// The queued events, oldest first.
    pub fn take_events(&mut self) -> Vec<SidechainEvent> {
        core::mem::take(&mut self.events)
    }

    /// `watch_mainchain_block(data, merkle_root)`: the next share from a peer with this
    /// coinbase merkle root is reported as the found main-chain block `data`.
    pub fn watch_mainchain_block(&mut self, data: ChainMain, merkle_root: Hash) {
        self.watch = Some((data, merkle_root));
    }

    /// The PPLNS rules for a share: window, penalty and the weight cap (twice the main-chain
    /// difficulty at the share's seed height; 0 for a share without a parent; none on an
    /// uncapped test sidechain).
    pub fn pplns_params(
        &self,
        tip: &PoolBlock,
        main: &dyn MainChainView,
    ) -> Result<PplnsParams, PplnsError> {
        let mainchain_diff = if tip.parent.is_zero() {
            Difficulty::ZERO
        } else {
            let h = seed_height(tip.txin_gen_height);
            main.difficulty_at_height(h)
                .ok_or(PplnsError::MainChainDifficulty(h))?
        };
        let max_weight = if self.options.uncapped_pplns {
            Difficulty::MAX
        } else {
            mainchain_diff.wrapping_mul_u64(2)
        };
        Ok(PplnsParams {
            window: self.options.config.pplns_window,
            uncle_penalty: self.options.config.uncle_penalty,
            max_weight,
        })
    }

    /// C++ `get_shares(tip)`: the PPLNS window below `tip` ([`crate::pplns::get_shares`])
    /// with this sidechain's rules. `tip` need not be stored (a template).
    pub fn get_shares(
        &self,
        tip: &PoolBlock,
        main: &dyn MainChainView,
    ) -> Result<PplnsWindow, PplnsError> {
        let params = self.pplns_params(tip, main)?;
        get_shares(tip, |id| self.blocks.get(id), &params)
    }

    /// C++ `get_bottom_height(tip)`: the lowest height of the PPLNS window, 0 if unknown.
    #[must_use]
    pub fn bottom_height(&self, tip: &PoolBlock, main: &dyn MainChainView) -> u64 {
        self.get_shares(tip, main).map_or(0, |w| w.bottom_height)
    }

    /// C++ `incoming_block_seen`: true for a share already received (same id, nonce and
    /// extra nonce) or one more than `2 * pplns_window` below the tip with less cumulative
    /// difficulty; otherwise it is remembered and false is returned.
    pub fn incoming_block_seen(&mut self, block: &PoolBlock, now: u64) -> bool {
        self.now = now;
        if let Some(tip) = self.chain_tip()
            && tip.sidechain_height
                > block
                    .sidechain_height
                    .wrapping_add(self.options.config.pplns_window * 2)
            && block.cumulative_difficulty < tip.cumulative_difficulty
        {
            return true;
        }
        match self.incoming.entry(block.full_id()) {
            std::collections::hash_map::Entry::Occupied(_) => true,
            std::collections::hash_map::Entry::Vacant(v) => {
                v.insert(now);
                false
            }
        }
    }

    /// C++ `forget_incoming_block`.
    pub fn forget_incoming_block(&mut self, block: &PoolBlock) {
        self.incoming.remove(&block.full_id());
    }

    /// C++ `cleanup_incoming_blocks`: forgets entries older than 10 minutes.
    pub fn cleanup_incoming_blocks(&mut self, now: u64) {
        self.now = now;
        self.incoming.retain(|_, t| now < *t + INCOMING_TIMEOUT);
    }

    /// C++ `add_external_block`: a parsed share from a peer.
    ///
    /// In order: difficulty at least the minimum (else rejected); already stored (accepted);
    /// difficulty below the current one and less than half of every share difficulty of the
    /// last `pplns_window` shares of the chain (ignored); coinbase height = height after its
    /// main-chain block when that block is known (else rejected); main-chain block at the seed
    /// height known (else ignored and forgotten); proof of work (computed here unless
    /// `offchain.pow_hash` is set) checked against the main-chain difficulty for a share at the
    /// next main-chain height (submit, [`submits_as_main_block`]) and the share difficulty
    /// (else rejected); then stored with [`Self::add_block`].
    pub fn add_external_block(
        &mut self,
        mut block: PoolBlock,
        main: &dyn MainChainView,
        now: u64,
    ) -> Result<ExternalOutcome, BlockRejected> {
        self.now = now;
        let cfg = &self.options.config;
        if block.difficulty < cfg.min_difficulty() {
            return Err(BlockRejected::DifficultyBelowMinimum);
        }
        if self.blocks.contains_key(&block.sidechain_id) {
            return Ok(ExternalOutcome::AlreadyKnown);
        }
        if block.difficulty < self.difficulty {
            let diff2 = block.difficulty.wrapping_add(block.difficulty);
            let mut too_low = true;
            if let Some(tip) = self.chain_tip() {
                let mut walk = Some(tip);
                while let Some(t) = walk {
                    if t.sidechain_height + cfg.pplns_window <= tip.sidechain_height {
                        break;
                    }
                    if diff2 >= t.difficulty {
                        too_low = false;
                        break;
                    }
                    walk = self.parent_of(t);
                }
            }
            if too_low {
                return Ok(ExternalOutcome::Ignored(IgnoreReason::LowDifficulty));
            }
        }
        if let Some(data) = main.block_by_hash(&block.prev_id) {
            let expected = data.height.wrapping_add(1);
            if expected != block.txin_gen_height {
                return Err(BlockRejected::WrongMainChainHeight {
                    got: block.txin_gen_height,
                    expected,
                });
            }
        }
        if main.seed(block.txin_gen_height).is_none() {
            self.forget_incoming_block(&block);
            return Ok(ExternalOutcome::Ignored(IgnoreReason::SeedUnknown));
        }
        let pow = block
            .offchain
            .pow_hash
            .unwrap_or_else(|| block.pow_hash(self.options.unlock_window));
        block.offchain.pow_hash = Some(pow);
        let submit_to_daemon = submits_as_main_block(&block, &pow, main);
        if !block.difficulty.check_pow(&pow) {
            return Err(BlockRejected::NotEnoughPow { submit_to_daemon });
        }

        let mut missing = Vec::new();
        if !block.parent.is_zero() && !self.blocks.contains_key(&block.parent) {
            missing.push(block.parent);
        }
        for h in &block.uncles {
            if !h.is_zero() && !self.blocks.contains_key(h) {
                missing.push(*h);
            }
        }
        let found = if let Some((data, root)) = self.watch
            && root == block.merkle_root
        {
            self.watch = None;
            Some(data)
        } else {
            None
        };
        self.add_block(block, main, now)
            .map_err(|_| BlockRejected::Duplicate)?;
        Ok(ExternalOutcome::Added {
            missing,
            submit_to_daemon,
            found,
        })
    }

    /// C++ `add_block`: stores a share (its local time set to `now`), updates the depths
    /// around it, then either takes it as a tip candidate (a share marked verified, i.e. an
    /// own share) or verifies it and every share waiting on it.
    pub fn add_block(
        &mut self,
        mut block: PoolBlock,
        main: &dyn MainChainView,
        now: u64,
    ) -> Result<(), AddError> {
        self.now = now;
        block.offchain.local_timestamp = now;
        block.offchain.cached_next_difficulty = None;
        self.seen_wallets.insert(block.miner_wallet.spend, now);
        self.prune_seen_data();

        let id = block.sidechain_id;
        if self.blocks.contains_key(&id) {
            return Err(AddError::Duplicate(id));
        }
        self.by_height
            .entry(block.sidechain_height)
            .or_default()
            .push(id);
        self.by_merkle_root.entry(block.merkle_root).or_insert(id);
        self.blocks.insert(id, block);

        self.update_depths(id, main);

        let Some(b) = self.blocks.get(&id) else {
            return Ok(());
        };
        if b.offchain.verified {
            if !b.offchain.invalid {
                self.update_chain_tip(id, main);
                self.events.push(SidechainEvent::StoreInCache(id));
            }
        } else {
            self.verify_loop(id, main);
        }
        Ok(())
    }

    /// C++ `get_missing_blocks`: for every unverified share, its parent if not stored and at
    /// most the first two uncles that are not stored.
    #[must_use]
    pub fn get_missing_blocks(&self) -> HashSet<Hash> {
        let mut missing = HashSet::new();
        for b in self.blocks.values() {
            if b.offchain.verified {
                continue;
            }
            if !b.parent.is_zero() && !self.blocks.contains_key(&b.parent) {
                missing.insert(b.parent);
            }
            let mut uncles = 0;
            for h in &b.uncles {
                if !h.is_zero() && !self.blocks.contains_key(h) {
                    missing.insert(*h);
                    uncles += 1;
                    if uncles >= 2 {
                        break;
                    }
                }
            }
        }
        missing
    }

    /// Distance below the tip from which shares are pruned:
    /// `(W - 1) * 2 + 2U + 90 / T + 1` (4 334 on the default sidechain).
    #[must_use]
    pub const fn prune_distance(&self) -> u64 {
        let cfg = &self.options.config;
        (cfg.pplns_window - 1) * 2
            + UNCLE_BLOCK_DEPTH * 2
            + MAIN_CHAIN_BLOCK_TIME / cfg.block_time
            + 1
    }

    /// C++ `prune_old_blocks`: once the tip is at least [`Self::prune_distance`] high, removes
    /// every share below `tip - distance` that is deeper than the distance or was added more
    /// than `W * 4 * T` seconds ago. Returns how many were removed.
    pub fn prune_old_blocks(&mut self, now: u64) -> usize {
        self.now = now;
        let distance = self.prune_distance();
        let cfg = &self.options.config;
        let delay = cfg.pplns_window * 4 * cfg.block_time;
        let Some(tip_height) = self.chain_tip().map(|t| t.sidechain_height) else {
            return 0;
        };
        if tip_height < distance {
            return 0;
        }
        let h = tip_height - distance;
        let old_heights: Vec<u64> = self.by_height.range(..h).map(|(k, _)| *k).collect();
        let mut pruned = 0;
        for height in old_heights {
            let Some(ids) = self.by_height.remove(&height) else {
                continue;
            };
            let mut kept = Vec::with_capacity(ids.len());
            for id in ids {
                let remove = self.blocks.get(&id).is_none_or(|b| {
                    b.offchain.depth > distance
                        || now >= b.offchain.local_timestamp.saturating_add(delay)
                });
                if remove {
                    if let Some(b) = self.blocks.remove(&id) {
                        self.by_merkle_root.remove(&b.merkle_root);
                    }
                    pruned += 1;
                } else {
                    kept.push(id);
                }
            }
            if !kept.is_empty() {
                self.by_height.insert(height, kept);
            }
        }
        if pruned > 0 {
            self.events.push(SidechainEvent::Pruned {
                blocks: pruned,
                below_height: h,
            });
        }
        pruned
    }

    /// C++ `prune_seen_data`: every 5 minutes, forgets wallets not seen for 72 hours.
    fn prune_seen_data(&mut self) {
        let now = self.now;
        if self.seen_wallets_pruned + SEEN_PRUNE_INTERVAL <= now {
            self.seen_wallets
                .retain(|_, t| t.saturating_add(SEEN_WALLET_TIMEOUT) >= now);
            self.seen_wallets_pruned = now;
        }
    }

    /// The ids stored at a height (empty if none).
    fn ids_at(&self, height: u64) -> Vec<Hash> {
        self.by_height.get(&height).cloned().unwrap_or_default()
    }
}

/// The parser's view of this sidechain: consensus id, fork heights, the parent's cached next
/// difficulty and transactions for compact blobs.
impl SidechainView for SideChain {
    fn consensus_id(&self) -> &Hash {
        &self.consensus_id
    }

    fn fork_heights(&self) -> ForkHeights {
        self.options.forks
    }

    fn cached_next_difficulty(&self, parent: &Hash) -> Option<Difficulty> {
        SideChain::cached_next_difficulty(self, parent)
    }

    fn transactions_of(&self, id: &Hash) -> Option<Vec<Hash>> {
        self.blocks.get(id).map(|b| b.transactions.clone())
    }
}

/// Whether a share from a peer goes to the daemon as a main-chain block (the submit step of
/// C++ `SideChain::add_external_block`, `src/side_chain.cpp`).
///
/// Only a candidate for the next main-chain block is sent: its coinbase height is the height
/// of the next block (`txin_gen_height == miner_data.height`, the top block's height + 1).
/// Then, as in C++, its proof of work must meet the difficulty of the next block when it
/// builds on our top block, or else the difficulty known for its own height. A share for a
/// height the main chain already holds (for example every old share received during the
/// initial sidechain sync) is not sent: that height is taken, so the chain cannot take it as
/// its next block, and its transactions have usually left the pool.
fn submits_as_main_block(block: &PoolBlock, pow: &Hash, main: &dyn MainChainView) -> bool {
    let main_tip = main.tip();
    if block.txin_gen_height != main_tip.height {
        return false;
    }
    (block.prev_id == main_tip.prev_id && main_tip.difficulty.check_pow(pow))
        || main
            .difficulty_at_height(block.txin_gen_height)
            .is_some_and(|d| d.check_pow(pow))
}

#[cfg(test)]
mod tests;
