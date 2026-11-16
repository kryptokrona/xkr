//! Fusion transactions: many small outputs into a few larger ones, without fee.
//!
//! * wallet-api (`src/wallet_backend/transfer.cpp` `sendFusionTransactionBasic`,
//!   `sendFusionTransactionAdvanced`, `SubWallets::getFusionTransactionInputs`): inputs from one
//!   random power-of-ten group, as many as fit; inputs are dropped from the end until the
//!   input/output ratio and the size fit.
//! * walletd (`src/wallet/wallet_green.cpp` `WalletGreen::createFusionTransaction`,
//!   `pickRandomFusionInputs`, `estimate`): inputs below a threshold, from one random
//!   power-of-ten group with at least `FUSION_TX_MIN_INPUT_COUNT` members; the largest are
//!   dropped until the size fits.

use std::sync::Arc;

use xkr_consensus::params::{
    FUSION_TX_MAX_SIZE, FUSION_TX_MIN_IN_OUT_COUNT_RATIO, FUSION_TX_MIN_INPUT_COUNT,
};
use xkr_consensus::reward::{
    decompose_amount, default_fusion_dust_threshold, is_fusion_transaction_amounts,
};
use xkr_crypto::os_rng;
use xkr_daemon_client::Transport;
use xkr_wallet_core::select::{self, FusionEstimate, split_amount_into_denominations};
use xkr_wallet_core::{Address, TransferError, build_fusion_transaction, rules};

use super::{
    Prepared, context, input_sources, our_key, our_keys, own_outputs, relay, transfer_error,
    verify_amounts, verify_fee,
};
use crate::error::{ErrorCode, WalletError};
use crate::state::{Candidate, TransferDestination, TransferKind, WalletState};
use crate::wallet::Shared;

/// A fusion transaction (wallet-api `/transactions/send/fusion/advanced`, walletd
/// `sendFusionTransaction`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FusionRequest {
    /// Ring size - 1; `None` for the network default.
    pub mixin: Option<u64>,
    /// Addresses to take inputs from; empty for all.
    pub source_addresses: Vec<String>,
    /// Own address receiving the result; `None` for the primary address.
    pub destination: Option<String>,
    /// walletd: only outputs below this amount (`None`: the wallet-api selection).
    pub threshold: Option<u64>,
}

/// `sendFusionTransactionAdvanced` (or walletd's threshold variant) up to the relay.
pub(crate) async fn prepare_fusion<T: Transport + 'static>(
    shared: &Arc<Shared<T>>,
    request: &FusionRequest,
) -> Result<Prepared, WalletError> {
    if shared.state.lock().is_view_wallet {
        return Err(ErrorCode::IllegalViewWalletOperation.into());
    }
    let ctx = context(shared).await?;
    let range = rules::mixin_range(ctx.height, ctx.version, ctx.ring_rule);
    let mixin = request.mixin.unwrap_or(range.default);
    let height = u32::try_from(ctx.height).unwrap_or(u32::MAX);

    let (chosen, destination, view_secret, keys) = {
        let s = shared.state.lock();
        // validateFusionTransaction
        range.check(mixin)?;
        let sources = our_keys(&s, &request.source_addresses)?;
        let destination_text = request
            .destination
            .clone()
            .unwrap_or_else(|| s.try_primary().map_or_default(|p| p.address.clone()));
        let destination = Address::new(our_key(&s, &destination_text)?, s.view_public);
        let candidates = s.spendable(sources.as_deref(), ctx.height, ctx.now);
        let mut rng = os_rng();
        let chosen: Vec<Candidate> = match request.threshold {
            None => {
                let mut selection =
                    select::fusion_inputs(&candidates, mixin, ctx.height, ctx.now, &mut rng);
                select::fusion_outputs(&mut selection)?;
                selection.inputs.into_iter().cloned().collect()
            }
            Some(threshold) => {
                let max = select::fusion_max_inputs(mixin);
                if max < FUSION_TX_MIN_INPUT_COUNT {
                    return Err(ErrorCode::FusionMixinTooLarge.into());
                }
                let picked = select::fusion_inputs_by_threshold(
                    &candidates,
                    threshold,
                    FUSION_TX_MIN_INPUT_COUNT,
                    max,
                    height,
                    &mut rng,
                );
                if picked.len() < FUSION_TX_MIN_INPUT_COUNT {
                    return Err(ErrorCode::FullyOptimized.into());
                }
                picked.into_iter().cloned().collect()
            }
        };
        (
            chosen,
            destination,
            s.view_secret,
            Arc::clone(&s.spend_keys),
        )
    };

    let node = shared.node();
    let rings = relay::ring_members(&node.client, mixin, &chosen).await?;
    let mut chosen = chosen;
    let mut sources = input_sources(&chosen, &rings, &view_secret)?;
    // As C++, inputs are dropped from the end until the transaction is a fusion transaction
    // that fits. The rules other than the size are checked on the amounts before signing, and
    // a transaction that is too large drops at once the inputs its size shows to be too many,
    // so signing happens only a few times however many inputs go.
    let built = loop {
        if sources.len() < FUSION_TX_MIN_INPUT_COUNT {
            return Err(ErrorCode::FullyOptimized.into());
        }
        let amounts: Vec<u64> = sources.iter().map(|s| s.amount).collect();
        let sum = amounts.iter().fold(0u64, |a, b| a.saturating_add(*b));
        if request.threshold.is_none() {
            let outputs = split_amount_into_denominations(sum).len();
            if outputs == 0 || sources.len() / outputs < FUSION_TX_MIN_IN_OUT_COUNT_RATIO {
                sources.pop();
                chosen.pop();
                continue;
            }
        }
        let mut outputs = decompose_amount(sum, default_fusion_dust_threshold(height));
        outputs.sort_unstable();
        if !is_fusion_transaction_amounts(&amounts, &outputs, 0, height) {
            sources.pop();
            chosen.pop();
            continue;
        }
        match build_fusion_transaction(&sources, &destination, ctx.version, height, &mut os_rng()) {
            Ok(built) => break built,
            Err(TransferError::FusionTooLarge(size)) => {
                let keep = sources.len() - too_many_inputs(size, sources.len());
                sources.truncate(keep);
                chosen.truncate(keep);
            }
            Err(TransferError::NotFusion) => {
                sources.pop();
                chosen.pop();
            }
            Err(e) => return Err(transfer_error(&e)),
        }
    };
    verify_amounts(&built)?;
    verify_fee(&built, 0)?;
    let unconfirmed = own_outputs(&view_secret, &keys, &built);
    // what went to the destination address (walletd lists it as that address's output)
    let total: u64 = built.tx.prefix.outputs.iter().map(|o| o.amount).sum();
    Ok(Prepared {
        built,
        inputs: chosen.into_iter().map(|c| (c.owner, c.input)).collect(),
        fee: 0,
        payment_id: String::new(),
        change: None,
        destinations: vec![TransferDestination {
            address: destination.to_string(),
            amount: total as i64,
            kind: TransferKind::Usual,
        }],
        unconfirmed,
    })
}

/// How many inputs a fusion transaction of `size` bytes with `inputs` inputs has too many: at
/// least one, and no more than the excess over `FUSION_TX_MAX_SIZE` divided by the size per
/// input rounded up (which is larger than an input's share, so no more are dropped than
/// needed; the rest go one at a time).
fn too_many_inputs(size: usize, inputs: usize) -> usize {
    let per_input = size.div_ceil(inputs.max(1)).max(1);
    (size.saturating_sub(FUSION_TX_MAX_SIZE) / per_input).clamp(1, inputs.max(1))
}

/// walletd `estimateFusion` (`WalletGreen::estimate`): how many of the unlocked outputs of
/// `addresses` (all for none) could go into fusion transactions with `threshold`.
pub(crate) fn estimate(
    state: &WalletState,
    threshold: u64,
    addresses: &[String],
    height: u64,
    now: u64,
) -> Result<FusionEstimate, WalletError> {
    let sources = our_keys(state, addresses)?;
    let amounts = state
        .spendable(sources.as_deref(), height, now)
        .into_iter()
        .map(|c| c.input.amount);
    Ok(select::fusion_ready_count(
        amounts,
        threshold,
        u32::try_from(height).unwrap_or(u32::MAX),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inputs_dropped_for_the_size_are_never_too_many() {
        // 100 inputs of 400 bytes plus 1 000 bytes: each input is 400 bytes, the estimate
        // takes 410 per input
        let size = 100 * 400 + 1_000;
        let excess = size - FUSION_TX_MAX_SIZE;
        let dropped = too_many_inputs(size, 100);
        assert!(dropped >= 1);
        assert!(dropped * 400 <= excess + 400);
        // just over: one
        assert_eq!(too_many_inputs(FUSION_TX_MAX_SIZE + 1, 50), 1);
        // never more than there are
        assert!((1..=3).contains(&too_many_inputs(usize::MAX / 2, 3)));
        assert_eq!(too_many_inputs(usize::MAX / 2, 1), 1);
    }
}
