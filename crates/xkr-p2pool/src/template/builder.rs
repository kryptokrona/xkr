//! Building one share from its parts (`BlockTemplate::update`, `create_miner_tx`,
//! `calc_sidechain_hash`, `submit_sidechain_block`, `src/block_template.cpp`).
//!
//! A share is built in three steps:
//!
//! 1. [`ShareSpec::new_block`]: the main-chain fields of the node (versions, previous block,
//!    height, timestamp), the payout wallet and the first three words of the sidechain extra
//!    buffer (software id, software version, a random word);
//! 2. [`SideChain::fill_sidechain_data`]: parent, height, uncles, difficulty, cumulative
//!    difficulty, tx key seed and coinbase keys, and the PPLNS window;
//! 3. [`finish_share`]: the reward split over the window, one output per decimal digit with its
//!    one-time key (and view tag from fork 14 on), the transaction list, then the share id
//!    over the serialization with the mining regions zeroed, which is also the coinbase merkle
//!    root.
//!
//! [`mined_share`] then sets what the miner found: nonce, extra nonce and the last word of
//! the sidechain extra buffer. None of them changes the share id.

use xkr_types::{Hash, PublicKey, SecretKey};

use super::TemplateError;
use crate::block::{COINBASE_EXTRA_SIZE, MAX_BLOCK_SIZE, OffchainState, PoolBlock, TxOutput};
use crate::difficulty::Difficulty;
use crate::forks::{COINBASE_TX_VERSION_2, coinbase_tx_version};
use crate::mainchain::MainChainView;
use crate::pplns::{MinerShare, PplnsWindow, expected_outputs, split_reward};
use crate::sidechain::SideChain;
use crate::varint::varint_len;
use crate::wallet::MinerWallet;

/// The fields of a share that come from the node and the miner, not from the sidechain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShareSpec {
    /// Block major version (the node's).
    pub major_version: u8,
    /// Block minor version.
    pub minor_version: u8,
    /// The main-chain block it builds on.
    pub prev_id: Hash,
    /// Main-chain height of the block (coinbase input height).
    pub height: u64,
    /// Block timestamp.
    pub timestamp: u64,
    /// Payout wallet.
    pub wallet: MinerWallet,
    /// `sidechainExtraBuf[0]`: software id.
    pub software_id: u32,
    /// `sidechainExtraBuf[1]`: software version.
    pub software_version: u32,
    /// `sidechainExtraBuf[2]`: a random word.
    pub random: u32,
}

impl ShareSpec {
    /// The share before [`SideChain::fill_sidechain_data`]: main-chain fields, wallet,
    /// sidechain extra buffer, no outputs, no transactions, nonce and extra nonce 0.
    #[must_use]
    pub fn new_block(&self, unlock_window: u64) -> PoolBlock {
        PoolBlock {
            major_version: self.major_version,
            minor_version: self.minor_version,
            prev_id: self.prev_id,
            timestamp: self.timestamp,
            nonce: 0,
            unlock_time: self.height.wrapping_add(unlock_window),
            txin_gen_height: self.height,
            outputs: Vec::new(),
            eph_public_keys: Vec::new(),
            txkey_pub: PublicKey([0; 32]),
            extra_nonce: 0,
            merkle_root: Hash::ZERO,
            transactions: vec![Hash::ZERO],
            miner_wallet: self.wallet,
            txkey_sec_seed: Hash::ZERO,
            parent: Hash::ZERO,
            uncles: Vec::new(),
            sidechain_height: 0,
            difficulty: Difficulty::ZERO,
            cumulative_difficulty: Difficulty::ZERO,
            merkle_proof: Vec::new(),
            merge_mining_extra: Vec::new(),
            sidechain_extra: [self.software_id, self.software_version, self.random, 0],
            txkey_sec: SecretKey([0; 32]),
            sidechain_id: Hash::ZERO,
            offchain: OffchainState::default(),
        }
    }
}

/// The coinbase part of `create_miner_tx` and the id of `calc_sidechain_hash`, for a share
/// whose sidechain fields are filled (parent, height, uncles, difficulties, tx key seed and
/// keys): splits `reward` over `shares`, writes one output per decimal digit with its one-time
/// key (and view tag in a version-2 coinbase), takes `transactions` (without the coinbase) in
/// this order, clears the merge-mining fields, sets nonce, extra nonce and the last sidechain
/// extra word to 0, then sets the share id and the coinbase merkle root to the hash of the
/// serialization with the mining regions zeroed.
pub fn finish_share(
    block: &mut PoolBlock,
    shares: &[MinerShare],
    reward: u64,
    transactions: &[Hash],
    unlock_window: u64,
    consensus_id: &Hash,
) -> Result<(), TemplateError> {
    if reward == 0 {
        return Err(TemplateError::ZeroReward);
    }
    let rewards = split_reward(reward, shares)?;
    let outputs =
        expected_outputs(shares, &rewards, &block.txkey_sec).ok_or(TemplateError::OutputKey)?;
    let tagged = coinbase_tx_version(block.major_version) >= COINBASE_TX_VERSION_2;
    block.outputs = outputs
        .iter()
        .map(|o| TxOutput {
            reward: o.amount,
            view_tag: if tagged { o.view_tag } else { 0 },
        })
        .collect();
    block.eph_public_keys = outputs.iter().map(|o| o.key).collect();
    block.transactions.clear();
    block.transactions.reserve(transactions.len() + 1);
    block.transactions.push(Hash::ZERO);
    block.transactions.extend_from_slice(transactions);
    block.unlock_time = block.txin_gen_height.wrapping_add(unlock_window);
    block.merkle_proof.clear();
    block.merge_mining_extra.clear();
    block.nonce = 0;
    block.extra_nonce = 0;
    block.sidechain_extra[3] = 0;
    block.merkle_root = Hash::ZERO;
    let id = block.calculate_sidechain_id(unlock_window, consensus_id);
    block.sidechain_id = id;
    block.merkle_root = id;
    let size = block.serialize(unlock_window).len();
    if size > MAX_BLOCK_SIZE {
        return Err(TemplateError::BlockTooBig(size));
    }
    Ok(())
}

/// The steps of a template with a given transaction list and reward: [`ShareSpec::new_block`],
/// [`SideChain::fill_sidechain_data`] on the sidechain tip, [`finish_share`]. Returns the share
/// (nonce and extra nonce 0) and its PPLNS window.
pub fn build_share(
    spec: &ShareSpec,
    sidechain: &SideChain,
    main: &dyn MainChainView,
    transactions: &[Hash],
    reward: u64,
) -> Result<(PoolBlock, PplnsWindow), TemplateError> {
    let unlock_window = sidechain.options().unlock_window;
    let mut block = spec.new_block(unlock_window);
    let window = sidechain.fill_sidechain_data(&mut block, main)?;
    finish_share(
        &mut block,
        &window.shares,
        reward,
        transactions,
        unlock_window,
        sidechain.consensus_id(),
    )?;
    Ok((block, window))
}

/// What `submit_sidechain_block` changes on the template block: the nonce, the coinbase extra
/// nonce and `sidechainExtraBuf[3]` (the same extra nonce). The id and merkle root stay; the
/// local state starts fresh.
#[must_use]
pub fn mined_share(block: &PoolBlock, nonce: u32, extra_nonce: u32) -> PoolBlock {
    let mut b = block.clone();
    b.nonce = nonce;
    b.extra_nonce = extra_nonce;
    b.sidechain_extra[3] = extra_nonce;
    b.offchain = OffchainState::default();
    b
}

/// Length of the real coinbase (`xkr_serialize_real_coinbase`) of a share at main-chain
/// `height` paying `amounts`: version, unlock time, the coinbase input, the outputs (tag and
/// key, plus the view tag in a version-2 coinbase) and the 71-byte extra. The size the daemon
/// counts for the block-size penalty.
#[must_use]
pub fn coinbase_size(major_version: u8, height: u64, unlock_window: u64, amounts: &[u64]) -> usize {
    let tagged = coinbase_tx_version(major_version) >= COINBASE_TX_VERSION_2;
    let per_output = 1 + 32 + usize::from(tagged);
    let outputs: usize = amounts.iter().map(|a| varint_len(*a) + per_output).sum();
    1 + varint_len(height.wrapping_add(unlock_window))
        + 2
        + varint_len(height)
        + varint_len(amounts.len() as u64)
        + outputs
        + varint_len(COINBASE_EXTRA_SIZE)
        + COINBASE_EXTRA_SIZE as usize
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keys::get_tx_keys;

    fn wallet(n: u8) -> MinerWallet {
        let (spend, _) = get_tx_keys(&Hash([n; 32]), &Hash([1; 32]));
        let (view, _) = get_tx_keys(&Hash([n; 32]), &Hash([2; 32]));
        MinerWallet { spend, view }
    }

    fn spec(major_version: u8) -> ShareSpec {
        ShareSpec {
            major_version,
            minor_version: 14,
            prev_id: Hash([3; 32]),
            height: 3_000_123,
            timestamp: 1_800_000_000,
            wallet: wallet(4),
            software_id: crate::SOFTWARE_ID,
            software_version: crate::SOFTWARE_VERSION,
            random: 0x1234_5678,
        }
    }

    #[test]
    fn coinbase_size_matches_the_serialization() {
        for major in [5u8, 7] {
            let mut b = spec(major).new_block(20);
            (b.txkey_pub, b.txkey_sec) = get_tx_keys(&Hash([5; 32]), &b.prev_id);
            let shares = [
                MinerShare {
                    weight: Difficulty::from_u64(3),
                    wallet: wallet(6),
                },
                MinerShare {
                    weight: Difficulty::from_u64(5),
                    wallet: wallet(7),
                },
            ];
            let txs = [Hash([8; 32])];
            for reward in [1u64, 9, 10, 1_234_567, 47_683_715] {
                finish_share(&mut b, &shares, reward, &txs, 20, &Hash([9; 32])).unwrap();
                let amounts: Vec<u64> = b.outputs.iter().map(|o| o.reward).collect();
                assert_eq!(amounts.iter().sum::<u64>(), reward);
                assert_eq!(
                    coinbase_size(major, b.txin_gen_height, 20, &amounts),
                    b.real_coinbase(20, 0).len(),
                    "major {major} reward {reward}"
                );
            }
        }
    }

    #[test]
    fn mining_fields_do_not_change_the_id() {
        let mut b = spec(5).new_block(20);
        (b.txkey_pub, b.txkey_sec) = get_tx_keys(&Hash([5; 32]), &b.prev_id);
        let shares = [MinerShare {
            weight: Difficulty::from_u64(1),
            wallet: wallet(6),
        }];
        let consensus_id = Hash([9; 32]);
        finish_share(&mut b, &shares, 1000, &[], 20, &consensus_id).unwrap();
        assert_eq!(b.merkle_root, b.sidechain_id);
        assert_eq!(
            b.sidechain_extra[..3],
            [crate::SOFTWARE_ID, crate::SOFTWARE_VERSION, 0x1234_5678]
        );
        let m = mined_share(&b, 77, 88);
        assert_eq!((m.nonce, m.extra_nonce, m.sidechain_extra[3]), (77, 88, 88));
        assert_eq!(m.calculate_sidechain_id(20, &consensus_id), b.sidechain_id);
        assert_ne!(m.serialize(20), b.serialize(20));
        assert!(matches!(
            finish_share(&mut b, &shares, 0, &[], 20, &consensus_id),
            Err(TemplateError::ZeroReward)
        ));
    }
}
