//! PPLNS payouts (`src/side_chain.cpp`: `SideChain::get_shares`, `SideChain::split_reward`,
//! `SideChain::decompose_outputs`; the one-time keys of `BlockTemplate::create_miner_tx` and
//! `SideChain::verify`).
//!
//! * [`get_shares`]: the PPLNS window below a share: walk the share and its parents (and their
//!   uncles) for at most `pplns_window` shares or until the total weight passes the cap (twice
//!   the main-chain difficulty at the share's seed height), sum the weight per miner wallet,
//!   sort the wallets and shuffle them with the share's tx key seed. The result order is the
//!   coinbase output order.
//! * [`split_reward`]: the reward per wallet, proportional to the weights, with exact 128-bit
//!   integer arithmetic and nothing left over.
//! * [`decompose_outputs`]: one output per non-zero decimal digit of each reward, smallest
//!   first.
//! * [`expected_outputs`]: amounts, one-time keys and view tags of the whole coinbase.

use std::collections::BTreeMap;

use xkr_crypto::cn_fast_hash;
use xkr_types::{Hash, PublicKey, SecretKey};

use crate::block::{MAX_OUTPUT_VALUE, PoolBlock};
use crate::difficulty::Difficulty;
use crate::keys::eph_public_key;
use crate::mainchain::seed_height;
use crate::order::{mul_hi, xorshift64star};
use crate::wallet::MinerWallet;

/// One wallet's part of a PPLNS window (C++ `MinerShare`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MinerShare {
    /// Summed weight of the wallet's shares (and uncle shares) in the window.
    pub weight: Difficulty,
    /// The payout wallet.
    pub wallet: MinerWallet,
}

/// The rules of a PPLNS window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PplnsParams {
    /// `m_chainWindowSize`: the most shares walked.
    pub window: u64,
    /// `m_unclePenalty` in percent.
    pub uncle_penalty: u64,
    /// The weight cap: twice the main-chain difficulty, or [`Difficulty::MAX`] on a test
    /// sidechain without a cap.
    pub max_weight: Difficulty,
}

/// A PPLNS window (the result of C++ `get_shares` with its bottom height).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PplnsWindow {
    /// Wallets with their weights, in coinbase output order.
    pub shares: Vec<MinerShare>,
    /// Sidechain height of the lowest share walked (`bottom_height`).
    pub bottom_height: u64,
    /// Total weight counted (`pplns_weight`).
    pub weight: Difficulty,
    /// Shares walked (not counting uncles).
    pub blocks: u64,
    /// Uncles counted.
    pub uncles: u64,
    /// Some share walked has another seed height (main-chain difficulty epoch) than the tip.
    /// Informational: the cap always uses the tip's seed height.
    pub epoch_mixed: bool,
}

/// Why a PPLNS window or a reward split cannot be computed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum PplnsError {
    /// An uncle of a share in the window is not known.
    #[error("uncle {id} of the share at height {height} is not known")]
    MissingUncle {
        /// Height of the share naming it.
        height: u64,
        /// The uncle id.
        id: Hash,
    },
    /// The parent of a share in the window is not known.
    #[error("parent {id} at height {height} is not known")]
    MissingParent {
        /// Height of the missing parent.
        height: u64,
        /// The parent id.
        id: Hash,
    },
    /// The main-chain difficulty at the seed height is not known.
    #[error("main-chain difficulty at height {0} is not known")]
    MainChainDifficulty(u64),
    /// The weights sum to zero.
    #[error("total PPLNS weight is zero")]
    ZeroWeight,
    /// One reward does not fit in an output amount.
    #[error("reward too large for the split")]
    RewardTooLarge,
    /// The rewards do not add up to the total.
    #[error("rewards do not add up to the total")]
    RewardMismatch,
}

/// The uncle penalty: `difficulty * penalty / 100` (the includer's part of an uncle).
#[must_use]
pub fn uncle_penalty(difficulty: Difficulty, penalty_percent: u64) -> Difficulty {
    difficulty
        .wrapping_mul_u64(penalty_percent)
        .checked_div_u64(100)
        .unwrap_or_default()
}

/// C++ `SideChain::get_shares(tip, shares, &bottom_height)`: the PPLNS window below `tip`.
/// `lookup` finds a known share by id (parents and uncles); `tip` itself need not be known
/// (a template).
///
/// For each share from `tip` down: its uncles first (skipped when `pplns_window` or more
/// below `tip`, or when they would push the total above the cap; otherwise the uncle's wallet
/// gets `difficulty - penalty` and the including share the penalty), then the share's own
/// weight (always, even above the cap). The walk ends when the total passes the cap, after
/// `pplns_window` shares, or at height 0.
pub fn get_shares<'a, F>(
    tip: &'a PoolBlock,
    lookup: F,
    params: &PplnsParams,
) -> Result<PplnsWindow, PplnsError>
where
    F: Fn(&Hash) -> Option<&'a PoolBlock>,
{
    let tip_epoch = seed_height(tip.txin_gen_height);
    let mut weights: BTreeMap<MinerWallet, Difficulty> = BTreeMap::new();
    let mut add = |wallet: MinerWallet, weight: Difficulty| {
        let w = weights.entry(wallet).or_default();
        *w = w.wrapping_add(weight);
    };
    let mut pplns_weight = Difficulty::ZERO;
    let mut block_count = 0u64;
    let mut uncle_count = 0u64;
    let mut epoch_mixed = false;
    let mut cur = tip;
    loop {
        if seed_height(cur.txin_gen_height) != tip_epoch {
            epoch_mixed = true;
        }
        let mut cur_weight = cur.difficulty;
        for uncle_id in &cur.uncles {
            let uncle = lookup(uncle_id).ok_or(PplnsError::MissingUncle {
                height: cur.sidechain_height,
                id: *uncle_id,
            })?;
            if tip.sidechain_height.wrapping_sub(uncle.sidechain_height) >= params.window {
                continue;
            }
            let penalty = uncle_penalty(uncle.difficulty, params.uncle_penalty);
            let uncle_weight = uncle.difficulty.wrapping_sub(penalty);
            let new_weight = pplns_weight.wrapping_add(uncle_weight);
            if new_weight > params.max_weight {
                continue;
            }
            cur_weight = cur_weight.wrapping_add(penalty);
            add(uncle.miner_wallet, uncle_weight);
            pplns_weight = new_weight;
            uncle_count += 1;
        }
        add(cur.miner_wallet, cur_weight);
        pplns_weight = pplns_weight.wrapping_add(cur_weight);
        block_count += 1;
        if pplns_weight > params.max_weight
            || block_count >= params.window
            || cur.sidechain_height == 0
        {
            break;
        }
        cur = lookup(&cur.parent).ok_or(PplnsError::MissingParent {
            height: cur.sidechain_height.wrapping_sub(1),
            id: cur.parent,
        })?;
    }
    let mut shares: Vec<MinerShare> = weights
        .into_iter()
        .map(|(wallet, weight)| MinerShare { weight, wallet })
        .collect();
    shuffle(&mut shares, &tip.txkey_sec_seed);
    Ok(PplnsWindow {
        shares,
        bottom_height: cur.sidechain_height,
        weight: pplns_weight,
        blocks: block_count,
        uncles: uncle_count,
        epoch_mixed,
    })
}

/// The shuffle of C++ `get_shares`: `seed` = the first 8 bytes (little-endian) of
/// `keccak(txkey_sec_seed)`, 1 if zero; for `i = 0 .. n-2`: `seed = xorshift64star(seed)`,
/// `k = floor(seed * (n - i) / 2^64)`, swap items `i` and `i + k`.
pub fn shuffle<T>(items: &mut [T], txkey_sec_seed: &Hash) {
    let n = items.len();
    if n <= 1 {
        return;
    }
    let h = cn_fast_hash(&txkey_sec_seed.0);
    let mut seed = u64::from_le_bytes(core::array::from_fn(|i| h.0[i]));
    if seed == 0 {
        seed = 1;
    }
    for i in 0..n - 1 {
        seed = xorshift64star(seed);
        let k = mul_hi(seed, (n - i) as u64) as usize;
        items.swap(i, i + k);
    }
}

/// C++ `SideChain::split_reward(reward, shares, rewards)`: with `W_i` the running sum of the
/// weights and `T` the total, wallet `i` gets
/// `lo(W_i * reward / T) - lo(W_{i-1} * reward / T)` (product modulo `2^128`, floor
/// division). Fails on a zero total, on a reward above the output limit, or when the rewards
/// do not add up to `reward`.
pub fn split_reward(reward: u64, shares: &[MinerShare]) -> Result<Vec<u64>, PplnsError> {
    let total = shares
        .iter()
        .fold(Difficulty::ZERO, |a, s| a.wrapping_add(s.weight));
    if total.is_zero() {
        return Err(PplnsError::ZeroWeight);
    }
    let mut amounts = Vec::with_capacity(shares.len());
    let mut w = Difficulty::ZERO;
    let mut given = 0u64;
    for s in shares {
        w = w.wrapping_add(s.weight);
        let next = w
            .wrapping_mul_u64(reward)
            .checked_div(total)
            .ok_or(PplnsError::ZeroWeight)?;
        let r = next.lo().wrapping_sub(given);
        given = next.lo();
        if r > MAX_OUTPUT_VALUE {
            return Err(PplnsError::RewardTooLarge);
        }
        amounts.push(r);
    }
    if amounts.iter().fold(0u64, |a, r| a.wrapping_add(*r)) != reward {
        return Err(PplnsError::RewardMismatch);
    }
    Ok(amounts)
}

/// C++ `SideChain::decompose_outputs`: for each wallet in order, one output per non-zero
/// decimal digit of its reward (`digit * 10^k`), smallest denomination first; a zero reward
/// gives no output.
#[must_use]
pub fn decompose_outputs(shares: &[MinerShare], rewards: &[u64]) -> Vec<(u64, MinerWallet)> {
    let mut outputs = Vec::with_capacity(shares.len().min(rewards.len()) * 4);
    for (share, reward) in shares.iter().zip(rewards) {
        let mut amount = *reward;
        let mut order = 1u64;
        while amount != 0 {
            let chunk = (amount % 10).wrapping_mul(order);
            amount /= 10;
            order = order.wrapping_mul(10);
            if chunk != 0 {
                outputs.push((chunk, share.wallet));
            }
        }
    }
    outputs
}

/// One coinbase output as the PPLNS rules require it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExpectedOutput {
    /// Amount.
    pub amount: u64,
    /// One-time key.
    pub key: PublicKey,
    /// View tag (written only in version-2 coinbases).
    pub view_tag: u8,
    /// The payee.
    pub wallet: MinerWallet,
}

/// The whole coinbase output list for a window and its rewards: [`decompose_outputs`] with
/// the one-time key and view tag of each output for the coinbase secret key `tx_secret`
/// (output index = position in the flattened list). `None` if a wallet key is not usable.
#[must_use]
pub fn expected_outputs(
    shares: &[MinerShare],
    rewards: &[u64],
    tx_secret: &SecretKey,
) -> Option<Vec<ExpectedOutput>> {
    decompose_outputs(shares, rewards)
        .into_iter()
        .enumerate()
        .map(|(i, (amount, wallet))| {
            eph_public_key(&wallet, tx_secret, i as u64).map(|(key, view_tag)| ExpectedOutput {
                amount,
                key,
                view_tag,
                wallet,
            })
        })
        .collect()
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

    fn share(weight: u128, n: u8) -> MinerShare {
        MinerShare {
            weight: Difficulty(weight),
            wallet: wallet(n),
        }
    }

    /// The same split with plain u128 arithmetic where it cannot wrap.
    fn split_reference(reward: u64, weights: &[u128]) -> Vec<u64> {
        let total: u128 = weights.iter().sum();
        let mut acc = 0u128;
        let mut given = 0u128;
        weights
            .iter()
            .map(|w| {
                acc += w;
                let next = acc * u128::from(reward) / total;
                let r = next - given;
                given = next;
                r as u64
            })
            .collect()
    }

    #[test]
    fn split_is_exact_and_proportional() {
        let cases: [(&[u128], u64); 5] = [
            (&[1], 12_345),
            (&[1, 1, 1], 10),
            (&[1_662_962_854, 1_359_986_539, 1_057_981_908], 13_668_012),
            (&[3, 0, 7], 1_000_001),
            (&[u128::from(u64::MAX), 5, 1 << 70], 999_999_999_999),
        ];
        for (weights, reward) in cases {
            let shares: Vec<MinerShare> = weights
                .iter()
                .enumerate()
                .map(|(i, w)| share(*w, i as u8 + 1))
                .collect();
            let split = split_reward(reward, &shares).unwrap();
            assert_eq!(split, split_reference(reward, weights));
            assert_eq!(split.iter().sum::<u64>(), reward);
        }
        // The fixture share at height 25 088.
        let shares = [
            share(1_662_962_854, 1),
            share(1_359_986_539, 2),
            share(1_057_981_908, 3),
        ];
        assert_eq!(
            split_reward(13_668_012, &shares).unwrap(),
            [5_569_659, 4_554_919, 3_543_434]
        );
        assert_eq!(split_reward(5, &[]), Err(PplnsError::ZeroWeight));
        assert_eq!(split_reward(5, &[share(0, 1)]), Err(PplnsError::ZeroWeight));
        // One wallet cannot take more than the output limit.
        assert_eq!(
            split_reward(MAX_OUTPUT_VALUE + 1, &[share(1, 1)]),
            Err(PplnsError::RewardTooLarge)
        );
        assert_eq!(
            split_reward(MAX_OUTPUT_VALUE, &[share(1, 1)]).unwrap(),
            [MAX_OUTPUT_VALUE]
        );
    }

    #[test]
    fn split_wraps_like_the_128_bit_product() {
        // W * reward above 2^128: the product wraps, as in C++, and the split no longer adds up.
        let shares = [share(1 << 100, 1), share(1 << 100, 2)];
        assert_eq!(
            split_reward(1 << 40, &shares),
            Err(PplnsError::RewardMismatch)
        );
    }

    #[test]
    fn decomposition_smallest_first() {
        let shares = [share(1, 1), share(1, 2), share(1, 3)];
        let out = decompose_outputs(&shares, &[5_569_659, 0, 1_000_300]);
        let amounts: Vec<u64> = out.iter().map(|o| o.0).collect();
        assert_eq!(
            amounts,
            [
                9, 50, 600, 9_000, 60_000, 500_000, 5_000_000, 300, 1_000_000
            ]
        );
        assert!(out[..7].iter().all(|o| o.1 == wallet(1)));
        assert!(out[7..].iter().all(|o| o.1 == wallet(3)));
        let big = decompose_outputs(&shares[..1], &[MAX_OUTPUT_VALUE]);
        assert_eq!(big.iter().map(|o| o.0).sum::<u64>(), MAX_OUTPUT_VALUE);
        let digits = MAX_OUTPUT_VALUE.to_string();
        assert_eq!(big.len(), digits.bytes().filter(|d| *d != b'0').count());
    }

    #[test]
    fn shuffle_is_the_cpp_permutation() {
        // Reference: the loop written out with u128.
        let seed_hash = Hash([0x42; 32]);
        for n in 0..12usize {
            let mut items: Vec<usize> = (0..n).collect();
            shuffle(&mut items, &seed_hash);
            let mut reference: Vec<usize> = (0..n).collect();
            if n > 1 {
                let h = cn_fast_hash(&seed_hash.0);
                let mut s = u64::from_le_bytes(h.0[..8].try_into().unwrap());
                if s == 0 {
                    s = 1;
                }
                for i in 0..n - 1 {
                    s = xorshift64star(s);
                    let k = ((u128::from(s) * (n - i) as u128) >> 64) as usize;
                    reference.swap(i, i + k);
                }
            }
            assert_eq!(items, reference);
            let mut sorted = items.clone();
            sorted.sort_unstable();
            assert_eq!(sorted, (0..n).collect::<Vec<_>>());
        }
    }

    #[test]
    fn expected_outputs_derive_keys_per_flat_index() {
        let shares = [share(2, 1), share(1, 2)];
        let (_, r) = get_tx_keys(&Hash([9; 32]), &Hash([8; 32]));
        let rewards = split_reward(3_000_021, &shares).unwrap();
        let outs = expected_outputs(&shares, &rewards, &r).unwrap();
        assert_eq!(outs.len(), decompose_outputs(&shares, &rewards).len());
        for (i, o) in outs.iter().enumerate() {
            let (key, tag) = eph_public_key(&o.wallet, &r, i as u64).unwrap();
            assert_eq!((o.key, o.view_tag), (key, tag));
        }
    }

    #[test]
    fn penalty() {
        assert_eq!(
            uncle_penalty(Difficulty(150_000_000), 20),
            Difficulty(30_000_000)
        );
        assert_eq!(uncle_penalty(Difficulty(199), 20), Difficulty(39));
    }
}
