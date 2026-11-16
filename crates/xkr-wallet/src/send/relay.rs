//! Talking to the daemon while sending (`src/wallet_backend/transfer.cpp`
//! `getRingParticipants`, `prepareRingParticipants`, `relayTransaction`).

use std::collections::HashSet;

use xkr_daemon_client::{DaemonClient, DaemonError, SendOutcome, Transport};
use xkr_types::{Hash, PublicKey};
use xkr_wallet_core::ATOMIC_UNITS_PER_COIN;

use crate::error::{ErrorCode, WalletError};
use crate::state::Candidate;

/// Where the C++ messages about missing ring members point.
const FAKE_OUTPUTS_HELP: &str =
    "https://gist.github.com/zpalmtree/80b3e80463225bcfb8f8432043cb594c";

/// `utilities::formatAmount`: `1,234.56789 XKR`.
#[must_use]
pub fn format_amount(amount: u64) -> String {
    let whole = (amount / ATOMIC_UNITS_PER_COIN).to_string();
    let mut grouped = String::with_capacity(whole.len() + whole.len() / 3);
    for (i, c) in whole.chars().enumerate() {
        if i > 0 && (whole.len() - i).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(c);
    }
    format!("{grouped}.{:05} XKR", amount % ATOMIC_UNITS_PER_COIN)
}

/// `DAEMON_OFFLINE` with the client's error as detail.
pub(crate) fn daemon_offline(e: &DaemonError) -> WalletError {
    WalletError::new(ErrorCode::DaemonOffline).with_detail(e.to_string())
}

fn not_enough(amount: u64, requested: u64, found: usize) -> WalletError {
    WalletError::with_message(
        ErrorCode::NotEnoughFakeOutputs,
        format!(
            "Failed to get enough matching outputs for amount {amount} ({}). Requested outputs: \
             {requested}, found outputs: {found}. Further explanation here: {FAKE_OUTPUTS_HELP}",
            format_amount(amount)
        ),
    )
}

/// `getRingParticipants` + `prepareRingParticipants`: `mixin` decoys per input from
/// `/getrandom_outs` (asked for `mixin + 1` outputs per amount, the amounts in ascending order).
/// The input's own output and repeated global indexes are dropped; the decoys come back in
/// global index order, one list per input in the order of `inputs`.
pub(crate) async fn ring_members<T: Transport>(
    client: &DaemonClient<T>,
    mixin: u64,
    inputs: &[Candidate],
) -> Result<Vec<Vec<(u32, PublicKey)>>, WalletError> {
    if mixin == 0 {
        return Ok(vec![Vec::new(); inputs.len()]);
    }
    let requested = mixin.saturating_add(1);
    let outs_count = u16::try_from(requested).map_err(|_| {
        WalletError::new(ErrorCode::MixinTooBig).with_detail("more decoys than a request takes")
    })?;
    let mut order: Vec<usize> = (0..inputs.len()).collect();
    order.sort_by_key(|&i| inputs[i].input.amount);
    let amounts: Vec<u64> = order.iter().map(|&i| inputs[i].input.amount).collect();
    let answer = client
        .random_outs(&amounts, outs_count)
        .await
        .map_err(|e| {
            tracing::warn!("asking the node for ring members: {e}");
            daemon_offline(&e)
        })?;

    for &amount in &amounts {
        let Some(entry) = answer.iter().find(|o| o.amount == amount) else {
            return Err(WalletError::with_message(
                ErrorCode::NotEnoughFakeOutputs,
                format!(
                    "Failed to get any matching outputs for amount {amount} ({}). Further \
                     explanation here: {FAKE_OUTPUTS_HELP}",
                    format_amount(amount)
                ),
            ));
        };
        if (entry.outs.len() as u64) < mixin {
            return Err(not_enough(amount, requested, entry.outs.len()));
        }
    }
    if answer.len() != amounts.len() {
        return Err(ErrorCode::NotEnoughFakeOutputs.into());
    }
    if let Some(short) = answer.iter().find(|e| (e.outs.len() as u64) < mixin) {
        return Err(not_enough(short.amount, requested, short.outs.len()));
    }

    let mut used = vec![false; answer.len()];
    let mut rings = vec![Vec::new(); inputs.len()];
    for &i in &order {
        let input = &inputs[i].input;
        let Some(pos) = (0..answer.len()).find(|&p| !used[p] && answer[p].amount == input.amount)
        else {
            return Err(ErrorCode::NotEnoughFakeOutputs.into());
        };
        used[pos] = true;
        let mut outs = answer[pos].outs.clone();
        outs.sort_by_key(|o| o.global_amount_index);
        let mut seen = HashSet::new();
        let mut ring: Vec<(u32, PublicKey)> = Vec::new();
        for out in outs {
            if Some(out.global_amount_index) == input.global_output_index
                || out.out_key == input.key
                || !seen.insert(out.global_amount_index)
            {
                continue;
            }
            let Ok(index) = u32::try_from(out.global_amount_index) else {
                continue;
            };
            ring.push((index, out.out_key));
            if ring.len() as u64 >= mixin {
                break;
            }
        }
        if (ring.len() as u64) < mixin {
            return Err(not_enough(input.amount, mixin, ring.len()));
        }
        rings[i] = ring;
    }
    Ok(rings)
}

/// `relayTransaction`: hands the blob to the node. No answer is `DAEMON_OFFLINE`, a refusal
/// `DAEMON_ERROR` (with the node's reason as detail). The hash is the wallet's own
/// (`getTransactionHash`).
pub(crate) async fn relay<T: Transport>(
    client: &DaemonClient<T>,
    blob: &[u8],
    hash: Hash,
) -> Result<Hash, WalletError> {
    match client.send_raw_transaction(blob).await {
        Ok(SendOutcome::Ok(_)) => Ok(hash),
        Ok(SendOutcome::Failed(reason)) => {
            tracing::warn!("the node refused transaction {hash}: {reason}");
            Err(WalletError::new(ErrorCode::DaemonError).with_detail(reason))
        }
        Err(e @ (DaemonError::Transport(_) | DaemonError::Http { .. })) => {
            tracing::warn!("relaying transaction {hash}: {e}");
            Err(daemon_offline(&e))
        }
        Err(e) => {
            tracing::warn!("relaying transaction {hash}: {e}");
            Err(WalletError::new(ErrorCode::DaemonError).with_detail(e.to_string()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn amounts_format_like_cpp() {
        assert_eq!(format_amount(0), "0.00000 XKR");
        assert_eq!(format_amount(123_456_789), "1,234.56789 XKR");
        assert_eq!(format_amount(100_000_000_000), "1,000,000.00000 XKR");
        assert_eq!(format_amount(99_900_000), "999.00000 XKR");
    }
}
