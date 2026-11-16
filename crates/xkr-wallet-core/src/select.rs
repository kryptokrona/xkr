//! Choosing which owned outputs a transaction spends.
//!
//! * [`inputs_for_amount`]: C++ wallet-api (`src/sub_wallets/sub_wallets.cpp`
//!   `SubWallets::getTransactionInputsForAmount`): shuffle the unlocked outputs and take them in
//!   that order until the amount is covered.
//! * [`fusion_inputs`] and [`fusion_outputs`]: C++ wallet-api fusion
//!   (`SubWallets::getFusionTransactionInputs` and the loop of
//!   `send_transaction::sendFusionTransactionAdvanced`).
//! * [`fusion_ready_count`] and [`fusion_inputs_by_threshold`]: walletd fusion with a threshold
//!   (`src/wallet/wallet_green.cpp` `WalletGreen::estimate`, `WalletGreen::pickRandomFusionInputs`).
//!
//! Shuffles take a caller-supplied random number generator; wallets pass
//! `xkr_crypto::os_rng()`.

use std::collections::BTreeMap;

use rand_core::CryptoRng;
use xkr_consensus::params::{
    FUSION_TX_MAX_SIZE, FUSION_TX_MIN_IN_OUT_COUNT_RATIO, FUSION_TX_MIN_INPUT_COUNT,
};
use xkr_consensus::reward::default_fusion_dust_threshold;

use crate::rules::{approximate_max_input_count, is_unlocked};

/// An owned, unspent output as the selection sees it.
pub trait SpendableInput {
    /// Amount in atomic units.
    fn amount(&self) -> u64;
    /// `unlock_time` of the transaction that created it.
    fn unlock_time(&self) -> u64;
}

/// Why no inputs were chosen. [`SelectError::code`] is the C++ `WalletErrorCode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SelectError {
    /// `NOT_ENOUGH_BALANCE` (11): the unlocked outputs do not cover the amount.
    #[error(
        "Not enough unlocked funds were found to cover this transaction in the subwallets \
         specified (or all wallets, if not specified. (Sum of amounts + fee + node fee)"
    )]
    NotEnoughFunds {
        /// Sum of the unlocked outputs.
        available: u64,
        /// Amount asked for.
        need: u64,
    },
    /// `FUSION_MIXIN_TOO_LARGE` (37): with this mixin fewer than `FUSION_TX_MIN_INPUT_COUNT`
    /// inputs fit in `FUSION_TX_MAX_SIZE`.
    #[error(
        "Cannot send fusion transacton - mixin is too large to meet input/output ratio \
         requirements whilst remaining in size constraints."
    )]
    FusionMixinTooLarge,
    /// `FULLY_OPTIMIZED` (36): not enough inputs left for a fusion transaction.
    #[error("Cannot send fusion transaction - wallet is already fully optimized.")]
    FullyOptimized,
}

impl SelectError {
    /// The C++ error code (`src/errors/errors.h` `WalletErrorCode`).
    #[must_use]
    pub const fn code(self) -> u16 {
        match self {
            Self::NotEnoughFunds { .. } => 11,
            Self::FusionMixinTooLarge => 37,
            Self::FullyOptimized => 36,
        }
    }
}

/// Inputs for a transaction and their sum.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selection<'a, T> {
    /// The chosen outputs, in the order they were taken.
    pub inputs: Vec<&'a T>,
    /// Sum of their amounts.
    pub sum: u64,
}

/// `SubWallet::getSpendableInputs`: the outputs that are unlocked at `height` / `now`
/// ([`crate::rules::is_unlocked`]).
#[must_use]
pub fn spendable<T: SpendableInput>(inputs: &[T], height: u64, now: u64) -> Vec<&T> {
    inputs
        .iter()
        .filter(|i| is_unlocked(i.unlock_time(), height, now))
        .collect()
}

/// `SubWallets::getTransactionInputsForAmount`: the unlocked outputs of `available` (the
/// subwallets to take from) in random order, taken until their sum is at least `amount`
/// (destinations + fee + node fee). The sum is usually larger; the rest is change.
pub fn inputs_for_amount<'a, T: SpendableInput, R: CryptoRng + ?Sized>(
    available: &'a [T],
    amount: u64,
    height: u64,
    now: u64,
    rng: &mut R,
) -> Result<Selection<'a, T>, SelectError> {
    let mut candidates = spendable(available, height, now);
    shuffle(&mut candidates, rng);
    let mut inputs = Vec::new();
    let mut sum = 0u64;
    for input in candidates {
        inputs.push(input);
        sum = sum.saturating_add(input.amount());
        if sum >= amount {
            return Ok(Selection { inputs, sum });
        }
    }
    Err(SelectError::NotEnoughFunds {
        available: sum,
        need: amount,
    })
}

// ---------------------------------------------------------------------------------------------
// fusion, wallet-api

/// Inputs for a fusion transaction (`SubWallets::getFusionTransactionInputs`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FusionSelection<'a, T> {
    /// The chosen outputs.
    pub inputs: Vec<&'a T>,
    /// About how many inputs with the mixin fit in `FUSION_TX_MAX_SIZE`
    /// ([`fusion_max_inputs`]).
    pub max_inputs: usize,
    /// Sum of the chosen amounts.
    pub sum: u64,
}

impl<'a, T: SpendableInput> FusionSelection<'a, T> {
    /// Drops the last input (C++ does this when the ratio or the size does not fit).
    pub fn drop_last(&mut self) -> Option<&'a T> {
        let last = self.inputs.pop()?;
        self.sum = self.sum.saturating_sub(last.amount());
        Some(last)
    }
}

/// `Currency::getApproximateMaximumInputCount(FUSION_TX_MAX_SIZE,
/// FUSION_TX_MIN_IN_OUT_COUNT_RATIO, mixin)`: about how many inputs a fusion transaction with
/// `mixin` may have (the output count assumed is 4, the ratio).
#[must_use]
pub fn fusion_max_inputs(mixin: u64) -> usize {
    let count = approximate_max_input_count(
        FUSION_TX_MAX_SIZE as u64,
        FUSION_TX_MIN_IN_OUT_COUNT_RATIO as u64,
        mixin,
    );
    usize::try_from(count).unwrap_or(usize::MAX)
}

/// `SubWallets::getFusionTransactionInputs`: the unlocked outputs, shuffled and grouped by
/// their number of decimal digits; one random group with at least `FUSION_TX_MIN_INPUT_COUNT`
/// outputs if there is one, otherwise every group; taken until [`fusion_max_inputs`] is
/// reached. Follow with [`fusion_outputs`].
pub fn fusion_inputs<'a, T: SpendableInput, R: CryptoRng + ?Sized>(
    available: &'a [T],
    mixin: u64,
    height: u64,
    now: u64,
    rng: &mut R,
) -> FusionSelection<'a, T> {
    let mut candidates = spendable(available, height, now);
    let max_inputs = fusion_max_inputs(mixin);
    shuffle(&mut candidates, rng);
    // `int numberOfDigits = log10(amount)`; amount 0 gets a group of its own.
    let mut buckets: BTreeMap<u32, Vec<&'a T>> = BTreeMap::new();
    for input in candidates {
        let digits = input.amount().checked_ilog10().unwrap_or(u32::MAX);
        buckets.entry(digits).or_default().push(input);
    }
    let mut full: Vec<&Vec<&'a T>> = buckets
        .values()
        .filter(|b| b.len() >= FUSION_TX_MIN_INPUT_COUNT)
        .collect();
    shuffle(&mut full, rng);
    let take_from: Vec<&Vec<&'a T>> = match full.first() {
        Some(bucket) => vec![*bucket],
        None => buckets.values().collect(),
    };
    let mut inputs = Vec::new();
    let mut sum = 0u64;
    'buckets: for bucket in take_from {
        for &input in bucket {
            inputs.push(input);
            sum = sum.saturating_add(input.amount());
            if inputs.len() >= max_inputs {
                break 'buckets;
            }
        }
    }
    FusionSelection {
        inputs,
        max_inputs,
        sum,
    }
}

/// `send_transaction::splitAmountIntoDenominations`: one output per non-zero decimal digit,
/// smallest first (`1010` gives `[10, 1000]`).
#[must_use]
pub fn split_amount_into_denominations(amount: u64) -> Vec<u64> {
    let mut out = Vec::new();
    let mut amount = amount;
    let mut multiplier = 1u64;
    while amount > 0 {
        let denomination = multiplier.wrapping_mul(amount % 10);
        if denomination != 0 {
            out.push(denomination);
        }
        amount /= 10;
        multiplier = multiplier.wrapping_mul(10);
    }
    out
}

/// The input loop of `send_transaction::sendFusionTransactionAdvanced` before the transaction
/// is built: [`SelectError::FusionMixinTooLarge`] if fewer than `FUSION_TX_MIN_INPUT_COUNT`
/// inputs fit; otherwise drop the last input until there are at least
/// `FUSION_TX_MIN_IN_OUT_COUNT_RATIO` inputs per output, and return the output amounts
/// ([`split_amount_into_denominations`] of the sum). [`SelectError::FullyOptimized`] once fewer
/// than `FUSION_TX_MIN_INPUT_COUNT` inputs are left.
///
/// The caller then builds the transaction (`crate::transfer::build_fusion_transaction`); if it
/// is too large, it calls [`FusionSelection::drop_last`] and this function again.
pub fn fusion_outputs<T: SpendableInput>(
    selection: &mut FusionSelection<'_, T>,
) -> Result<Vec<u64>, SelectError> {
    if selection.max_inputs < FUSION_TX_MIN_INPUT_COUNT {
        return Err(SelectError::FusionMixinTooLarge);
    }
    loop {
        if selection.inputs.len() < FUSION_TX_MIN_INPUT_COUNT {
            return Err(SelectError::FullyOptimized);
        }
        let outputs = split_amount_into_denominations(selection.sum);
        if outputs.is_empty()
            || selection.inputs.len() / outputs.len() < FUSION_TX_MIN_IN_OUT_COUNT_RATIO
        {
            selection.drop_last();
            continue;
        }
        return Ok(outputs);
    }
}

// ---------------------------------------------------------------------------------------------
// fusion, walletd (threshold)

/// `Currency::isAmountApplicableInFusionTransactionInput`: whether an output of `amount` may go
/// into a walletd fusion transaction with `threshold` at `height` (below the threshold, not
/// below the fusion dust threshold, a single non-zero digit), and if so its power of ten.
#[must_use]
pub fn fusion_input_power(amount: u64, threshold: u64, height: u32) -> Option<u8> {
    if amount >= threshold || amount < default_fusion_dust_threshold(height) {
        return None;
    }
    pretty_amount_power(amount)
}

/// The power of ten of an amount in `Currency::PRETTY_AMOUNTS` (`d * 10^k`, `d` in `1..=9`),
/// `None` for any other amount.
#[must_use]
pub fn pretty_amount_power(amount: u64) -> Option<u8> {
    if amount == 0 {
        return None;
    }
    let mut rest = amount;
    let mut power = 0u8;
    while rest.is_multiple_of(10) {
        rest /= 10;
        power += 1;
    }
    (rest <= 9).then_some(power)
}

/// What walletd's `estimateFusion` reports (`IFusionManager::EstimateResult`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FusionEstimate {
    /// Outputs in power-of-ten groups of at least `FUSION_TX_MIN_INPUT_COUNT` fusion-ready
    /// outputs.
    pub fusion_ready_count: usize,
    /// All outputs looked at.
    pub total_output_count: usize,
}

/// `WalletGreen::estimate(threshold, sourceAddresses)` over the amounts of the unlocked outputs
/// of the chosen addresses.
#[must_use]
pub fn fusion_ready_count<I: IntoIterator<Item = u64>>(
    amounts: I,
    threshold: u64,
    height: u32,
) -> FusionEstimate {
    let mut buckets = [0usize; 20];
    let mut total = 0usize;
    for amount in amounts {
        total += 1;
        if let Some(power) = fusion_input_power(amount, threshold, height) {
            buckets[usize::from(power)] += 1;
        }
    }
    FusionEstimate {
        fusion_ready_count: buckets
            .iter()
            .filter(|&&n| n >= FUSION_TX_MIN_INPUT_COUNT)
            .sum(),
        total_output_count: total,
    }
}

/// `WalletGreen::pickRandomFusionInputs`: of the unlocked outputs `available`, the fusion-ready
/// ones ([`fusion_input_power`]) of one random power-of-ten group with at least
/// `min_input_count` members; at most `max_input_count` of them, chosen at random, sorted by
/// amount. Empty if no group is large enough.
pub fn fusion_inputs_by_threshold<'a, T: SpendableInput, R: CryptoRng + ?Sized>(
    available: &'a [T],
    threshold: u64,
    min_input_count: usize,
    max_input_count: usize,
    height: u32,
    rng: &mut R,
) -> Vec<&'a T> {
    let mut groups: [Vec<&'a T>; 20] = std::array::from_fn(|_| Vec::new());
    for input in available {
        if let Some(power) = fusion_input_power(input.amount(), threshold, height) {
            groups[usize::from(power)].push(input);
        }
    }
    let mut order: Vec<usize> = (0..groups.len()).collect();
    shuffle(&mut order, rng);
    let Some(chosen) = order
        .into_iter()
        .find(|&g| groups[g].len() >= min_input_count)
    else {
        return Vec::new();
    };
    let mut selected = std::mem::take(&mut groups[chosen]);
    if selected.len() > max_input_count {
        shuffle(&mut selected, rng);
        selected.truncate(max_input_count);
    }
    selected.sort_by_key(|i| i.amount());
    selected
}

// ---------------------------------------------------------------------------------------------
// randomness

/// A uniform index below `n` (`n > 0`), by rejection so every index is equally likely.
fn random_index<R: CryptoRng + ?Sized>(rng: &mut R, n: usize) -> usize {
    let n = n as u64;
    // 2^64 mod n values at the top are rejected.
    let reject = (u64::MAX % n + 1) % n;
    loop {
        let x = rng.next_u64();
        if x <= u64::MAX - reject {
            return (x % n) as usize;
        }
    }
}

/// Fisher-Yates shuffle (`std::shuffle`).
fn shuffle<T, R: CryptoRng + ?Sized>(items: &mut [T], rng: &mut R) {
    for i in (1..items.len()).rev() {
        let j = random_index(rng, i + 1);
        items.swap(i, j);
    }
}

#[cfg(test)]
mod tests {
    use xkr_crypto::os_rng;

    use super::*;
    use crate::rules::estimate_tx_size;
    use crate::transfer::TxVersion;

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    struct Out {
        amount: u64,
        unlock_time: u64,
    }

    impl SpendableInput for Out {
        fn amount(&self) -> u64 {
            self.amount
        }
        fn unlock_time(&self) -> u64 {
            self.unlock_time
        }
    }

    fn outs(amounts: &[u64]) -> Vec<Out> {
        amounts
            .iter()
            .map(|&amount| Out {
                amount,
                unlock_time: 0,
            })
            .collect()
    }

    #[test]
    fn inputs_cover_the_amount_and_skip_locked_outputs() {
        let mut rng = os_rng();
        let mut available = outs(&[10, 20, 30, 40, 50]);
        // locked until height 1 000 (the wallet is at 500) and a unix time in the future
        available.push(Out {
            amount: 1_000_000,
            unlock_time: 1_000,
        });
        available.push(Out {
            amount: 2_000_000,
            unlock_time: 2_000_000_000,
        });
        for _ in 0..50 {
            let s = inputs_for_amount(&available, 75, 500, 1_700_000_000, &mut rng).unwrap();
            assert!(s.sum >= 75);
            assert_eq!(s.sum, s.inputs.iter().map(|i| i.amount).sum::<u64>());
            assert!(s.inputs.iter().all(|i| i.unlock_time == 0));
            // taken until covered: without the last one the amount is not covered
            let last = s.inputs.last().unwrap().amount;
            assert!(s.sum - last < 75);
        }
        let e = inputs_for_amount(&available, 151, 500, 1_700_000_000, &mut rng).unwrap_err();
        assert_eq!(
            e,
            SelectError::NotEnoughFunds {
                available: 150,
                need: 151
            }
        );
        assert_eq!(e.code(), 11);
        // later the locked ones count (all seven are needed)
        let s = inputs_for_amount(&available, 3_000_150, 999, 2_000_000_000, &mut rng).unwrap();
        assert_eq!(s.inputs.len(), 7);
        // every order shows up
        let firsts: std::collections::HashSet<u64> = (0..200)
            .map(|_| {
                inputs_for_amount(&available, 1, 500, 0, &mut rng)
                    .unwrap()
                    .inputs[0]
                    .amount
            })
            .collect();
        assert_eq!(firsts.len(), 5);
    }

    #[test]
    fn fusion_inputs_fit_the_size_and_use_one_full_group() {
        let mut rng = os_rng();
        // 30 outputs of three digits, 5 of one digit
        let mut amounts: Vec<u64> = (0..30).map(|i| 100 * (1 + i % 9)).collect();
        amounts.extend([1, 2, 3, 4, 5]);
        let available = outs(&amounts);
        let mut s = fusion_inputs(&available, 3, 2_000_000, 0, &mut rng);
        assert_eq!(s.max_inputs, 94);
        assert_eq!(s.inputs.len(), 30);
        assert!(s.inputs.iter().all(|i| (100..1000).contains(&i.amount)));
        assert_eq!(s.sum, s.inputs.iter().map(|i| i.amount).sum::<u64>());
        let outputs = fusion_outputs(&mut s).unwrap();
        assert!(s.inputs.len() >= FUSION_TX_MIN_INPUT_COUNT);
        assert!(s.inputs.len() / outputs.len() >= FUSION_TX_MIN_IN_OUT_COUNT_RATIO);
        assert_eq!(outputs.iter().sum::<u64>(), s.sum);

        // no full group: every group is used, still fewer than 12 here
        let few = outs(&[1, 2, 30, 40, 500]);
        let mut small = fusion_inputs(&few, 3, 2_000_000, 0, &mut rng);
        assert_eq!(small.inputs.len(), 5);
        assert_eq!(fusion_outputs(&mut small), Err(SelectError::FullyOptimized));

        // the maximum caps the count: 300 outputs of 1 with mixin 3 -> 94
        let many = outs(&[1; 300]);
        let s = fusion_inputs(&many, 3, 2_000_000, 0, &mut rng);
        assert_eq!(s.inputs.len(), 94);

        // a mixin too large for 12 inputs
        let mut s = fusion_inputs(&available, 100, 2_000_000, 0, &mut rng);
        assert_eq!(s.max_inputs, 4);
        let e = fusion_outputs(&mut s).unwrap_err();
        assert_eq!((e, e.code()), (SelectError::FusionMixinTooLarge, 37));
    }

    #[test]
    fn fusion_size_estimate_stays_below_the_limit() {
        // FUSION_TX_MAX_SIZE with the maximum number of inputs and four outputs
        for mixin in 0..=7 {
            let inputs = fusion_max_inputs(mixin) as u64;
            assert!(inputs >= FUSION_TX_MIN_INPUT_COUNT as u64, "mixin {mixin}");
            let size = estimate_tx_size(mixin, inputs, 4, false, 0, TxVersion::V1);
            assert!(size <= FUSION_TX_MAX_SIZE as u64, "mixin {mixin}: {size}");
        }
    }

    #[test]
    fn fusion_outputs_keep_the_ratio() {
        // 12 x 100 and 55: 1 255 = 5 + 50 + 200 + 1000, 13 / 4 < 4, so 55 is dropped and
        // 1 200 = 200 + 1000 leaves 12 / 2 = 6 inputs per output
        let mut amounts = vec![100u64; 12];
        amounts.push(55);
        let available = outs(&amounts);
        let mut s = FusionSelection {
            inputs: available.iter().collect(),
            max_inputs: 94,
            sum: 1_255,
        };
        assert_eq!(fusion_outputs(&mut s), Ok(vec![200, 1_000]));
        assert_eq!((s.inputs.len(), s.sum), (12, 1_200));
        // 12 inputs making 4 outputs: 12 / 4 = 3, drop one, 11 left
        let mut amounts = vec![1u64; 11];
        amounts.push(1_100);
        let available = outs(&amounts);
        let mut s = FusionSelection {
            inputs: available.iter().collect(),
            max_inputs: 94,
            sum: 1_111,
        };
        let e = fusion_outputs(&mut s).unwrap_err();
        assert_eq!((e, e.code()), (SelectError::FullyOptimized, 36));
        assert_eq!(split_amount_into_denominations(1010), vec![10, 1000]);
        assert_eq!(split_amount_into_denominations(0), Vec::<u64>::new());
        assert_eq!(
            split_amount_into_denominations(u64::MAX)
                .iter()
                .sum::<u64>(),
            u64::MAX
        );
    }

    #[test]
    fn walletd_threshold_rules() {
        let height = 2_000_000;
        assert_eq!(pretty_amount_power(7), Some(0));
        assert_eq!(pretty_amount_power(300), Some(2));
        assert_eq!(pretty_amount_power(10_000_000_000_000_000_000), Some(19));
        assert_eq!(pretty_amount_power(310), None);
        assert_eq!(pretty_amount_power(0), None);
        assert_eq!(fusion_input_power(500, 1_000, height), Some(2));
        assert_eq!(fusion_input_power(1_000, 1_000, height), None);
        assert_eq!(fusion_input_power(510, 1_000, height), None);
        // below the fusion dust threshold (10 before height 800 000)
        assert_eq!(fusion_input_power(5, 1_000, 700_000), None);
        assert_eq!(fusion_input_power(5, 1_000, 800_000), Some(0));

        // 12 hundreds, 11 tens, 3 not pretty, 2 above the threshold
        let mut amounts = vec![200u64; 12];
        amounts.extend([30u64; 11]);
        amounts.extend([123, 456, 789]);
        amounts.extend([5_000, 6_000]);
        let est = fusion_ready_count(amounts.iter().copied(), 1_000, height);
        assert_eq!(
            est,
            FusionEstimate {
                fusion_ready_count: 12,
                total_output_count: 28
            }
        );

        let mut rng = os_rng();
        let available = outs(&amounts);
        let picked = fusion_inputs_by_threshold(&available, 1_000, 12, 50, height, &mut rng);
        assert_eq!(picked.len(), 12);
        assert!(picked.iter().all(|i| i.amount == 200));
        // at most `max_input_count`, sorted
        let mut mixed: Vec<u64> = (1..=9).map(|d| d * 100).cycle().take(40).collect();
        mixed.push(7);
        let available = outs(&mixed);
        let picked = fusion_inputs_by_threshold(&available, 1_000, 12, 20, height, &mut rng);
        assert_eq!(picked.len(), 20);
        assert!(picked.windows(2).all(|w| w[0].amount <= w[1].amount));
        assert!(picked.iter().all(|i| (100..1000).contains(&i.amount)));
        // nothing to optimize
        let none = fusion_inputs_by_threshold(&available, 100, 12, 20, height, &mut rng);
        assert!(matches!(none.as_slice(), []));
    }

    #[test]
    fn shuffles_are_uniform_enough() {
        let mut rng = os_rng();
        let mut counts = [0u32; 4];
        for _ in 0..4_000 {
            counts[random_index(&mut rng, 4)] += 1;
        }
        assert!(
            counts.iter().all(|&c| (800..1200).contains(&c)),
            "{counts:?}"
        );
        let mut items: Vec<u32> = (0..10).collect();
        shuffle(&mut items, &mut rng);
        items.sort_unstable();
        assert_eq!(items, (0..10).collect::<Vec<_>>());
        let mut empty: Vec<u32> = Vec::new();
        shuffle(&mut empty, &mut rng);
    }
}
