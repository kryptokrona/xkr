//! The transactions of a template (`BlockTemplate::select_mempool_transactions`, the pick in
//! `BlockTemplate::update`, `BlockTemplate::shuffle_tx_order`, `src/block_template.cpp`;
//! `TxMempoolData`, `src/common.h`).
//!
//! * [`eligible_transactions`]: transactions in the backlog for more than [`TX_MIN_AGE`]
//!   seconds, or with a fee of at least [`HIGH_FEE_VALUE`];
//! * [`cap_transactions`]: at most as many as fit in a share (the best by [`fee_order`]);
//! * [`pick_transactions`]: all of them when the block stays within the median size;
//!   otherwise, in [`fee_order`], each transaction is added when that raises the reward, or
//!   replaces one of the last 100 picked when that raises it more; then zero-fee (fusion)
//!   transactions fill what is left below the median;
//! * [`shuffle`]: the final order is random.
//!
//! The reward is the Kryptokrona block reward ([`block_reward`]): emission
//! `(MONEY_SUPPLY - generated) >> 21` plus fees, both reduced by the block-size penalty above the
//! median, the rule the daemon checks the coinbase against.

use core::cmp::Ordering;
use std::collections::HashSet;

use rand_core::RngCore;
use xkr_types::Hash;

use super::TemplateMinerData;
use crate::order::{hash_cmp, mul_hi};

/// `HIGH_FEE_VALUE` (`src/mempool.h`): a transaction with at least this fee is taken at once.
pub const HIGH_FEE_VALUE: u64 = 6_000_000_000;

/// A transaction must have been in the backlog for more than this many seconds (unless its fee
/// is high).
pub const TX_MIN_AGE: u64 = 5;

/// How far back the pick looks for a transaction to replace.
const REPLACE_DEPTH: usize = 100;

/// One backlog transaction (`TxMempoolData`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MempoolTx {
    /// Transaction hash.
    pub id: Hash,
    /// Blob size in bytes (`weight`).
    pub weight: u64,
    /// Fee.
    pub fee: u64,
    /// When it entered the node's pool (seconds since the epoch).
    pub time_received: u64,
}

/// `TxMempoolData::operator<`: higher fee per byte first, then the smaller transaction, then
/// the id in hash order. [`Ordering::Less`] means `a` is preferred.
#[must_use]
pub fn fee_order(a: &MempoolTx, b: &MempoolTx) -> Ordering {
    let ours = u128::from(a.fee) * u128::from(b.weight);
    let theirs = u128::from(b.fee) * u128::from(a.weight);
    theirs
        .cmp(&ours)
        .then(a.weight.cmp(&b.weight))
        .then_with(|| hash_cmp(&a.id, &b.id))
}

/// The block reward for `fees` and a cumulative block size `weight` at the template's height
/// (`xkr_consensus::get_block_reward` with the major version, median size and generated coins
/// of `data`); 0 when the block is too large.
#[must_use]
pub fn block_reward(data: &TemplateMinerData, fees: u64, weight: u64) -> u64 {
    xkr_consensus::reward::get_block_reward(
        data.major_version,
        data.median_weight,
        weight,
        data.already_generated_coins,
        fees,
    )
    .map_or(0, |(reward, _)| reward)
}

/// The first filter of `select_mempool_transactions`: received more than [`TX_MIN_AGE`] seconds
/// before `now`, or a fee of at least [`HIGH_FEE_VALUE`].
#[must_use]
pub fn eligible_transactions(backlog: &[MempoolTx], now: u64) -> Vec<MempoolTx> {
    backlog
        .iter()
        .filter(|t| now > t.time_received.saturating_add(TX_MIN_AGE) || t.fee >= HIGH_FEE_VALUE)
        .copied()
        .collect()
}

/// The block-size safeguard of `select_mempool_transactions`: at most `max_transactions`, the
/// best by [`fee_order`].
#[must_use]
pub fn cap_transactions(mut txs: Vec<MempoolTx>, max_transactions: usize) -> Vec<MempoolTx> {
    if txs.len() > max_transactions {
        txs.sort_by(fee_order);
        txs.truncate(max_transactions);
    }
    txs
}

/// What picking one transaction does to the picked list.
#[derive(Debug, Clone, Copy)]
enum Pick {
    Add,
    Replace(usize),
}

/// The transaction pick of `update` for a coinbase of `miner_tx_weight` bytes;
/// `block_reward(fees, weight)` is the reward of a block of that size with those fees. The
/// result is not shuffled yet.
#[must_use]
pub fn pick_transactions<F>(
    txs: &[MempoolTx],
    miner_tx_weight: u64,
    median_weight: u64,
    block_reward: F,
) -> Vec<MempoolTx>
where
    F: Fn(u64, u64) -> u64,
{
    let total = txs.iter().fold(0u64, |a, t| a.wrapping_add(t.weight));
    // Within the penalty-free zone: everything.
    if total.wrapping_add(miner_tx_weight) <= median_weight {
        return txs.to_vec();
    }

    let mut order: Vec<usize> = (0..txs.len()).collect();
    order.sort_by(|a, b| fee_order(&txs[*a], &txs[*b]));

    let mut final_fees = 0u64;
    let mut final_weight = miner_tx_weight;
    let mut final_reward = block_reward(final_fees, final_weight);
    let mut picked: Vec<usize> = Vec::with_capacity(order.len());
    for &i in &order {
        let tx = &txs[i];
        let mut pick = None;
        let reward = block_reward(
            final_fees.wrapping_add(tx.fee),
            final_weight.wrapping_add(tx.weight),
        );
        if reward > final_reward {
            final_reward = reward;
            pick = Some(Pick::Add);
        }
        if final_weight.wrapping_add(tx.weight) > median_weight {
            let n = picked.len();
            for j in (n.saturating_sub(REPLACE_DEPTH)..n).rev() {
                let prev = &txs[picked[j]];
                let replaced = block_reward(
                    final_fees.wrapping_add(tx.fee).wrapping_sub(prev.fee),
                    final_weight
                        .wrapping_add(tx.weight)
                        .wrapping_sub(prev.weight),
                );
                if replaced > final_reward {
                    final_reward = replaced;
                    pick = Some(Pick::Replace(j));
                }
            }
        }
        match pick {
            Some(Pick::Add) => {
                picked.push(i);
                final_fees = final_fees.wrapping_add(tx.fee);
                final_weight = final_weight.wrapping_add(tx.weight);
            }
            Some(Pick::Replace(j)) => {
                let prev = &txs[picked[j]];
                final_fees = final_fees.wrapping_add(tx.fee).wrapping_sub(prev.fee);
                final_weight = final_weight
                    .wrapping_add(tx.weight)
                    .wrapping_sub(prev.weight);
                picked[j] = i;
            }
            None => {}
        }
    }

    // Zero-fee (fusion) transactions fill the room left below the median, where they cost
    // no reward.
    let mut taken: HashSet<usize> = picked.iter().copied().collect();
    for &i in &order {
        let tx = &txs[i];
        if tx.fee != 0 || taken.contains(&i) {
            continue;
        }
        if final_weight.wrapping_add(tx.weight) > median_weight {
            continue;
        }
        picked.push(i);
        taken.insert(i);
        final_weight = final_weight.wrapping_add(tx.weight);
    }

    picked.into_iter().map(|i| txs[i]).collect()
}

/// `shuffle_tx_order`: for `i = 0 .. n-2`, `k = floor(rng() * (n - i) / 2^64)`, swap items `i`
/// and `i + k`.
pub fn shuffle<T, R: RngCore + ?Sized>(items: &mut [T], rng: &mut R) {
    let n = items.len();
    if n <= 1 {
        return;
    }
    for i in 0..n - 1 {
        let k = mul_hi(rng.next_u64(), (n - i) as u64) as usize;
        items.swap(i, i + k);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tx(n: u8, weight: u64, fee: u64) -> MempoolTx {
        MempoolTx {
            id: Hash([n; 32]),
            weight,
            fee,
            time_received: 100,
        }
    }

    /// A penalty like the daemon's: full reward up to the median, nothing above twice it.
    fn reward(median: u64) -> impl Fn(u64, u64) -> u64 {
        move |fees, weight| {
            let base = 1_000_000u64;
            let total = base + fees;
            if weight <= median {
                total
            } else if weight > 2 * median {
                0
            } else {
                let w = u128::from(weight);
                let m = u128::from(median);
                (u128::from(total) * w * (2 * m - w) / m / m) as u64
            }
        }
    }

    #[test]
    fn order_by_fee_per_byte_then_size_then_id() {
        let a = tx(1, 100, 1000);
        let b = tx(2, 200, 1000);
        let c = tx(3, 200, 2000);
        assert_eq!(fee_order(&a, &b), Ordering::Less);
        // same fee per byte: the smaller first
        assert_eq!(fee_order(&a, &c), Ordering::Less);
        assert_eq!(fee_order(&c, &b), Ordering::Less);
        let d = tx(4, 100, 1000);
        assert_eq!(fee_order(&a, &d), Ordering::Less);
        assert_eq!(fee_order(&a, &a), Ordering::Equal);
    }

    #[test]
    fn age_and_fee_filter() {
        let mut high = tx(2, 100, HIGH_FEE_VALUE);
        high.time_received = 104;
        let backlog = [tx(1, 100, 10), high, tx(3, 100, 10)];
        assert_eq!(eligible_transactions(&backlog, 105).len(), 1);
        assert_eq!(eligible_transactions(&backlog, 106).len(), 3);
    }

    #[test]
    fn cap_keeps_the_best() {
        let txs = vec![tx(1, 100, 10), tx(2, 100, 30), tx(3, 100, 20)];
        let kept = cap_transactions(txs.clone(), 2);
        assert_eq!(kept, [txs[1], txs[2]]);
        assert_eq!(cap_transactions(txs.clone(), 3), txs);
    }

    #[test]
    fn free_zone_takes_everything() {
        let txs = [tx(1, 100, 0), tx(2, 100, 5)];
        assert_eq!(pick_transactions(&txs, 100, 300, reward(300)), txs);
    }

    #[test]
    fn penalty_zone_pick_and_fusion_fill() {
        let median = 10_000;
        // Pays well: taken although above the median.
        let rich = tx(1, 600, 600_000);
        // Pays nothing worth its size above the median.
        let poor = tx(2, 3_000, 1);
        // Fusion: no fee, fits below the median after the others.
        let fusion = tx(3, 500, 0);
        let filler = tx(4, 8_000, 80_000);
        let txs = [rich, poor, fusion, filler];
        let picked = pick_transactions(&txs, 400, median, reward(median));
        assert!(picked.contains(&rich));
        assert!(picked.contains(&filler));
        assert!(!picked.contains(&poor));
        assert!(picked.contains(&fusion));
        let weight: u64 = 400 + picked.iter().map(|t| t.weight).sum::<u64>();
        assert!(weight <= median, "{weight}");
    }

    struct Counter(u64);

    impl RngCore for Counter {
        fn next_u32(&mut self) -> u32 {
            self.next_u64() as u32
        }
        fn next_u64(&mut self) -> u64 {
            self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
            self.0
        }
        fn fill_bytes(&mut self, dst: &mut [u8]) {
            for b in dst {
                *b = self.next_u64() as u8;
            }
        }
    }

    #[test]
    fn shuffle_is_a_permutation() {
        let mut v: Vec<u32> = (0..50).collect();
        shuffle(&mut v, &mut Counter(1));
        assert_ne!(v, (0..50).collect::<Vec<_>>());
        v.sort_unstable();
        assert_eq!(v, (0..50).collect::<Vec<_>>());
        let mut one = [7];
        shuffle(&mut one, &mut Counter(1));
        assert_eq!(one, [7]);
    }
}
