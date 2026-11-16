//! The block template: the share this node mines next and the main-chain block inside it
//! (`BlockTemplate`, `src/block_template.cpp` / `src/block_template.h`, with the fork 13 and
//! fork 14 steps of the Kryptokrona p2pool patches).
//!
//! [`BlockTemplate::update`] follows C++ `BlockTemplate::update`: the main-chain fields come from
//! the node's miner data ([`TemplateMinerData`]), the sidechain part from
//! [`SideChain::fill_sidechain_data`] (parent, uncles, difficulty, tx key seed, PPLNS window),
//! the transactions from the node's backlog ([`txs`]: age and fee filter, block-size cap,
//! fee-per-byte pick with replacement, fusion fill, shuffle), the reward from the Kryptokrona
//! emission and block-size penalty, the coinbase from the PPLNS split ([`builder`]). The share
//! id is the merkle root in the coinbase. Each update keeps the [`OLD_TEMPLATES`] templates
//! before it, so a share found on a slightly older template still counts.
//!
//! [`Template`] answers the stratum side: one hashing blob per extra nonce
//! ([`Template::hashing_blob`]), and from `(nonce, extra nonce)` the finished share
//! ([`Template::share`]) and the main-chain block ([`Template::mainchain_blob`]).
//! [`PoolTemplates`] puts the template next to the node ([`PoolNode`]) and implements the
//! stratum server's [`crate::stratum::TemplateSource`] and [`crate::stratum::ShareSink`].
//!
//! | file | C++ |
//! |---|---|
//! | `mod.rs` | `update`, `m_oldTemplates`, `get_hashing_blob`, `get_difficulties`, `get_block_template_blob`, `submit_sidechain_block` |
//! | `builder.rs` | `create_miner_tx`, `calc_sidechain_hash`, the sidechain extra buffer |
//! | `txs.rs` | `select_mempool_transactions`, the transaction pick of `update`, `shuffle_tx_order`, `get_block_reward` |
//! | `service.rs` | `p2pool::submit_block_async`, `p2pool::submit_sidechain_block`, the stratum hooks |

pub mod builder;
mod service;
pub mod txs;

use std::collections::{HashSet, VecDeque};
use std::sync::Arc;

use rand_core::RngCore;
use xkr_types::Hash;

use crate::block::{MAX_BLOCK_SIZE, OffchainState, PoolBlock};
use crate::difficulty::Difficulty;
use crate::forks::{COINBASE_TX_VERSION_2, coinbase_tx_version};
use crate::mainchain::MainChainView;
use crate::pplns::{MinerShare, PplnsError, decompose_outputs, split_reward};
use crate::sidechain::SideChain;
use crate::stratum::{HashingBlob, TemplateDifficulties};
use crate::varint::varint_len;
use crate::wallet::MinerWallet;
use crate::{SOFTWARE_ID, SOFTWARE_VERSION};

pub use builder::{ShareSpec, build_share, coinbase_size, finish_share, mined_share};
pub use service::{FoundBlock, PoolNode, PoolTemplates, SubmitError};
pub use txs::{HIGH_FEE_VALUE, MempoolTx, TX_MIN_AGE, fee_order};

/// Templates kept after a newer one replaced them (`m_oldTemplates[4]`).
pub const OLD_TEMPLATES: usize = 4;

/// Rounds of "reward from block size, coinbase size from reward" before giving up on a fixed
/// point (only ever more than one round inside the block-size penalty zone).
const COINBASE_SIZE_ROUNDS: usize = 8;

/// Room left for the coinbase to change size when the template falls back to the
/// penalty-free zone.
const COINBASE_SIZE_SLACK: u64 = 256;

/// What the template needs from the node for the next main-chain block (C++ `MinerData`; in
/// xkrd `Chain::miner_data()`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TemplateMinerData {
    /// Block major version of the next block.
    pub major_version: u8,
    /// Block minor version the node writes into its templates (the fork index it supports,
    /// `ForkSchedule::template_minor_version`).
    pub minor_version: u8,
    /// Height of the next block (top + 1).
    pub height: u64,
    /// Id of the top block.
    pub prev_id: Hash,
    /// Difficulty of the next block.
    pub difficulty: Difficulty,
    /// `max(median of the last block sizes, full reward zone)`.
    pub median_weight: u64,
    /// Coins generated up to and including the top block.
    pub already_generated_coins: u64,
    /// Median timestamp of the timestamp-check window: the block must be later.
    pub median_timestamp: u64,
    /// The transactions the next block may carry.
    pub tx_backlog: Vec<MempoolTx>,
}

/// Why no template was built (C++ falls back to the previous template in these cases; here the
/// previous one simply stays current).
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum TemplateError {
    /// The node's major version is not the one the sidechain expects at this height: every
    /// peer would refuse the share.
    #[error("major version {daemon} at height {height}, the sidechain expects {sidechain}")]
    MajorVersion {
        /// Main-chain height.
        height: u64,
        /// Major version from the node.
        daemon: u8,
        /// Major version of the sidechain fork table.
        sidechain: u8,
    },
    /// PPLNS window or reward split failed.
    #[error(transparent)]
    Pplns(#[from] PplnsError),
    /// The block reward is zero (block too large).
    #[error("block reward is zero")]
    ZeroReward,
    /// A wallet key of the PPLNS window is not a usable point.
    #[error("an output key cannot be derived")]
    OutputKey,
    /// Reward and coinbase size did not settle on one value.
    #[error("coinbase size does not settle")]
    CoinbaseSize,
    /// The share would be larger than a peer accepts.
    #[error("share of {0} bytes is too large")]
    BlockTooBig(usize),
    /// The node did not hand out its sidechain.
    #[error("sidechain not available")]
    Unavailable,
}

/// One built template (the C++ `BlockTemplate` state of one template id).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Template {
    /// The template id (generation counter, never 0).
    pub id: u32,
    /// The share with nonce 0 and extra nonce 0; its id and merkle root are final.
    pub block: PoolBlock,
    /// Difficulty of the main-chain block (`m_difficulty`).
    pub mainchain_difficulty: Difficulty,
    /// Coinbase unlock window of the main chain.
    pub unlock_window: u64,
    /// PPLNS window in coinbase output order (`m_shares`).
    pub shares: Vec<MinerShare>,
    /// The coinbase total (`m_finalReward`).
    pub reward: u64,
    /// Fees of the included transactions.
    pub fees: u64,
    /// Cumulative block size: coinbase and transactions.
    pub block_weight: u64,
    /// Transactions offered by the node.
    pub backlog_size: usize,
    /// When it was built (seconds since the epoch, `m_lastUpdated`).
    pub created: u64,
}

impl Template {
    /// Main-chain height of the block.
    #[must_use]
    pub const fn height(&self) -> u64 {
        self.block.txin_gen_height
    }

    /// Sidechain height of the share.
    #[must_use]
    pub const fn sidechain_height(&self) -> u64 {
        self.block.sidechain_height
    }

    /// The share id (also the coinbase merkle root).
    #[must_use]
    pub const fn sidechain_id(&self) -> &Hash {
        &self.block.sidechain_id
    }

    /// `get_difficulties`.
    #[must_use]
    pub const fn difficulties(&self) -> TemplateDifficulties {
        TemplateDifficulties {
            height: self.block.txin_gen_height,
            sidechain_height: self.block.sidechain_height,
            mainchain_difficulty: self.mainchain_difficulty,
            sidechain_difficulty: self.block.difficulty,
        }
    }

    /// `get_hashing_blob_nolock(extra_nonce)`: the proof-of-work blob with this extra nonce in
    /// the coinbase and a zero nonce. The extra nonce changes the coinbase hash, so the aux
    /// hash and the parent coinbase hash in the blob; the nonce offset only depends on the
    /// timestamp.
    #[must_use]
    pub fn hashing_blob(&self, extra_nonce: u32) -> HashingBlob {
        let pow = self.block.pow_blob(self.unlock_window, extra_nonce, 0);
        HashingBlob {
            blob: pow.blob,
            nonce_offset: pow.nonce_offset,
            template_id: self.id,
            height: self.block.txin_gen_height,
            sidechain_height: self.block.sidechain_height,
            mainchain_difficulty: self.mainchain_difficulty,
            sidechain_difficulty: self.block.difficulty,
            seed_hash: Hash::ZERO,
        }
    }

    /// The share a miner found with `nonce` and `extra_nonce` (`submit_sidechain_block`):
    /// nonce, extra nonce and `sidechainExtraBuf[3]` set, id and merkle root unchanged, fresh
    /// local state.
    #[must_use]
    pub fn share(&self, nonce: u32, extra_nonce: u32) -> PoolBlock {
        mined_share(&self.block, nonce, extra_nonce)
    }

    /// `get_block_template_blob`: the main-chain block with the final nonce and extra nonce,
    /// ready for the node's block submission.
    #[must_use]
    pub fn mainchain_blob(&self, nonce: u32, extra_nonce: u32) -> Vec<u8> {
        self.block
            .serialize_mainchain_data(self.unlock_window, nonce, extra_nonce)
            .bytes
    }

    /// The found main-chain block for [`PoolNode::submit_mainchain_block`].
    #[must_use]
    pub fn found_block(&self, nonce: u32, extra_nonce: u32) -> FoundBlock {
        FoundBlock {
            template_id: self.id,
            height: self.block.txin_gen_height,
            prev_id: self.block.prev_id,
            difficulty: self.mainchain_difficulty,
            sidechain_id: self.block.sidechain_id,
            nonce,
            extra_nonce,
            blob: self.mainchain_blob(nonce, extra_nonce),
        }
    }
}

/// The current template and the [`OLD_TEMPLATES`] before it (C++ `BlockTemplate` with
/// `m_oldTemplates`).
#[derive(Debug, Default)]
pub struct BlockTemplate {
    current: Option<Arc<Template>>,
    old: VecDeque<Arc<Template>>,
    last_id: u32,
}

impl BlockTemplate {
    /// No template yet.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The current template.
    #[must_use]
    pub const fn current(&self) -> Option<&Arc<Template>> {
        self.current.as_ref()
    }

    /// The current or a kept older template by id.
    #[must_use]
    pub fn get(&self, id: u32) -> Option<&Arc<Template>> {
        self.current
            .iter()
            .chain(self.old.iter())
            .find(|t| t.id == id)
    }

    /// When the current template was built (`last_updated()`), 0 without one.
    #[must_use]
    pub fn last_updated(&self) -> u64 {
        self.current.as_ref().map_or(0, |t| t.created)
    }

    /// C++ `BlockTemplate::update(data, mempool, params)` with the node's random source: builds
    /// a template for `wallet` on the sidechain tip and makes it current. On failure the
    /// current template stays.
    pub fn update(
        &mut self,
        data: &TemplateMinerData,
        sidechain: &SideChain,
        main: &dyn MainChainView,
        wallet: &MinerWallet,
        now: u64,
    ) -> Result<Arc<Template>, TemplateError> {
        self.update_with_rng(
            data,
            sidechain,
            main,
            wallet,
            now,
            &mut xkr_crypto::os_rng(),
        )
    }

    /// [`Self::update`] with a given random source (transaction order and the random word of
    /// the sidechain extra buffer).
    pub fn update_with_rng<R: RngCore + ?Sized>(
        &mut self,
        data: &TemplateMinerData,
        sidechain: &SideChain,
        main: &dyn MainChainView,
        wallet: &MinerWallet,
        now: u64,
        rng: &mut R,
    ) -> Result<Arc<Template>, TemplateError> {
        let template = make_template(data, sidechain, main, wallet, now, rng)?;
        Ok(self.insert(template))
    }

    /// Makes `template` current under the next template id; the current one moves to the kept
    /// ones, the oldest kept one is dropped.
    pub fn insert(&mut self, mut template: Template) -> Arc<Template> {
        self.last_id = self.last_id.wrapping_add(1);
        if self.last_id == 0 {
            self.last_id = 1;
        }
        template.id = self.last_id;
        let template = Arc::new(template);
        if let Some(previous) = self.current.replace(template.clone()) {
            self.old.push_front(previous);
            self.old.truncate(OLD_TEMPLATES);
        }
        template
    }
}

/// The steps of C++ `BlockTemplate::update` up to a finished template (id 0).
fn make_template<R: RngCore + ?Sized>(
    data: &TemplateMinerData,
    sidechain: &SideChain,
    main: &dyn MainChainView,
    wallet: &MinerWallet,
    now: u64,
    rng: &mut R,
) -> Result<Template, TemplateError> {
    let options = sidechain.options();
    let expected = options.forks.network_major_version(data.height);
    if data.major_version != expected {
        return Err(TemplateError::MajorVersion {
            height: data.height,
            daemon: data.major_version,
            sidechain: expected,
        });
    }
    let unlock_window = options.unlock_window;
    let timestamp = if now <= data.median_timestamp {
        data.median_timestamp + 1
    } else {
        now
    };
    let spec = ShareSpec {
        major_version: data.major_version,
        minor_version: data.minor_version,
        prev_id: data.prev_id,
        height: data.height,
        timestamp,
        wallet: *wallet,
        software_id: SOFTWARE_ID,
        software_version: SOFTWARE_VERSION,
        random: (rng.next_u64() >> 32) as u32,
    };
    let mut block = spec.new_block(unlock_window);
    let window = sidechain.fill_sidechain_data(&mut block, main)?;
    let shares = window.shares;

    let reward_at = |fees: u64, weight: u64| txs::block_reward(data, fees, weight);

    // The largest coinbase: every fee, no penalty.
    let eligible = txs::eligible_transactions(&data.tx_backlog, now);
    let all_fees = eligible.iter().fold(0u64, |a, t| a.saturating_add(t.fee));
    let max_reward = reward_at(0, 0).saturating_add(all_fees);
    let split_at_max = split_reward(max_reward, &shares)?;
    let max_amounts: Vec<u64> = decompose_outputs(&shares, &split_at_max)
        .into_iter()
        .map(|(a, _)| a)
        .collect();
    let max_coinbase = coinbase_size(data.major_version, data.height, unlock_window, &max_amounts);

    // Block-size cap on the number of transactions.
    let max_transactions = {
        let tagged = coinbase_tx_version(data.major_version) >= COINBASE_TX_VERSION_2;
        let (count, bytes) = outputs_bound(&split_at_max, tagged);
        let empty = block
            .serialize_mainchain_data(unlock_window, 0, 0)
            .bytes
            .len()
            + block.serialize_sidechain_data().len();
        // the empty block has one-byte output and transaction counts
        let fixed = empty - 2 + varint_len(count) + bytes + varint_len(eligible.len() as u64);
        MAX_BLOCK_SIZE.saturating_sub(fixed) / 32
    };
    let candidates = txs::cap_transactions(eligible, max_transactions);

    let mut picked = txs::pick_transactions(
        &candidates,
        max_coinbase as u64,
        data.median_weight,
        reward_at,
    );
    txs::shuffle(&mut picked, rng);
    let mut seen = HashSet::with_capacity(picked.len());
    picked.retain(|t| seen.insert(t.id));

    let settled = match settle_reward(data, &shares, &picked, unlock_window, max_coinbase) {
        Err(TemplateError::CoinbaseSize | TemplateError::ZeroReward) => {
            // Inside the penalty zone reward and coinbase size can alternate: keep only what
            // fits below the median, where the reward does not depend on the size.
            let limit = data
                .median_weight
                .saturating_sub(max_coinbase as u64 + COINBASE_SIZE_SLACK);
            let mut total = 0u64;
            picked.retain(|t| {
                let fits = total.saturating_add(t.weight) <= limit;
                if fits {
                    total += t.weight;
                }
                fits
            });
            settle_reward(data, &shares, &picked, unlock_window, max_coinbase)?
        }
        other => other?,
    };

    let ids: Vec<Hash> = picked.iter().map(|t| t.id).collect();
    finish_share(
        &mut block,
        &shares,
        settled.reward,
        &ids,
        unlock_window,
        sidechain.consensus_id(),
    )?;
    let coinbase_len = block.real_coinbase(unlock_window, 0).len() as u64;
    if coinbase_len + settled.txs_weight != settled.weight {
        return Err(TemplateError::CoinbaseSize);
    }
    Ok(Template {
        id: 0,
        block,
        mainchain_difficulty: data.difficulty,
        unlock_window,
        shares,
        reward: settled.reward,
        fees: settled.fees,
        block_weight: settled.weight,
        backlog_size: data.tx_backlog.len(),
        created: now,
    })
}

/// Reward, fees and sizes of a template whose coinbase size and reward agree.
#[derive(Debug, Clone, Copy)]
struct Settled {
    reward: u64,
    fees: u64,
    txs_weight: u64,
    weight: u64,
}

/// The reward of a block with these transactions: starting from the coinbase size of the
/// largest reward, the reward of the block size, then the coinbase size of that reward, until
/// both agree (the daemon requires the coinbase to pay exactly the reward of the block's own
/// size).
fn settle_reward(
    data: &TemplateMinerData,
    shares: &[MinerShare],
    picked: &[MempoolTx],
    unlock_window: u64,
    first_coinbase: usize,
) -> Result<Settled, TemplateError> {
    let fees = picked.iter().fold(0u64, |a, t| a.wrapping_add(t.fee));
    let txs_weight = picked.iter().fold(0u64, |a, t| a.wrapping_add(t.weight));
    let mut weight = txs_weight + first_coinbase as u64;
    for _ in 0..COINBASE_SIZE_ROUNDS {
        let reward = txs::block_reward(data, fees, weight);
        if reward == 0 {
            return Err(TemplateError::ZeroReward);
        }
        let rewards = split_reward(reward, shares)?;
        let amounts: Vec<u64> = decompose_outputs(shares, &rewards)
            .into_iter()
            .map(|(a, _)| a)
            .collect();
        let size = coinbase_size(data.major_version, data.height, unlock_window, &amounts);
        let next = txs_weight + size as u64;
        if next == weight {
            return Ok(Settled {
                reward,
                fees,
                txs_weight,
                weight,
            });
        }
        weight = next;
    }
    Err(TemplateError::CoinbaseSize)
}

/// An upper bound of the coinbase outputs for rewards up to `rewards` (and slightly above):
/// per wallet one output per decimal digit position, each as long as the largest amount.
/// Returns `(output count, output bytes)`.
fn outputs_bound(rewards: &[u64], tagged: bool) -> (u64, usize) {
    let per_output = 1 + 32 + usize::from(tagged);
    let mut count = 0u64;
    let mut bytes = 0usize;
    for r in rewards {
        // one more position for a reward that ends up a little larger
        let positions = decimal_digits(*r) + 1;
        count += positions;
        bytes += positions as usize * (varint_len(r.saturating_mul(10)) + per_output);
    }
    (count, bytes)
}

/// Number of decimal digits (0 for 0).
const fn decimal_digits(mut v: u64) -> u64 {
    let mut n = 0;
    while v != 0 {
        v /= 10;
        n += 1;
    }
    n
}

/// A finished share from a template block: [`mined_share`] with the off-chain state of an
/// own share (verified, to be broadcast).
#[must_use]
pub fn own_share(template: &Template, nonce: u32, extra_nonce: u32) -> PoolBlock {
    let mut b = template.share(nonce, extra_nonce);
    b.offchain = OffchainState {
        verified: true,
        want_broadcast: true,
        ..OffchainState::default()
    };
    b
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digits_and_bound() {
        assert_eq!(decimal_digits(0), 0);
        assert_eq!(decimal_digits(9), 1);
        assert_eq!(decimal_digits(10), 2);
        assert_eq!(decimal_digits(u64::MAX), 20);
        let (count, bytes) = outputs_bound(&[1000, 0], false);
        assert_eq!(count, 5 + 1);
        assert_eq!(bytes, 5 * (2 + 33) + (1 + 33));
    }

    #[test]
    fn kept_templates() {
        let mut bt = BlockTemplate::new();
        assert!(bt.current().is_none());
        assert_eq!(bt.last_updated(), 0);
        let blank = Template {
            id: 0,
            block: ShareSpec {
                major_version: 5,
                minor_version: 14,
                prev_id: Hash::ZERO,
                height: 1,
                timestamp: 7,
                wallet: MinerWallet {
                    spend: xkr_types::PublicKey([0; 32]),
                    view: xkr_types::PublicKey([0; 32]),
                },
                software_id: SOFTWARE_ID,
                software_version: SOFTWARE_VERSION,
                random: 0,
            }
            .new_block(20),
            mainchain_difficulty: Difficulty::ZERO,
            unlock_window: 20,
            shares: Vec::new(),
            reward: 0,
            fees: 0,
            block_weight: 0,
            backlog_size: 0,
            created: 7,
        };
        for _ in 0..7 {
            bt.insert(blank.clone());
        }
        assert_eq!(bt.current().map(|t| t.id), Some(7));
        assert_eq!(bt.last_updated(), 7);
        for id in 3..=7 {
            assert_eq!(bt.get(id).map(|t| t.id), Some(id));
        }
        assert!(bt.get(2).is_none());
        assert!(bt.get(8).is_none());
    }
}
