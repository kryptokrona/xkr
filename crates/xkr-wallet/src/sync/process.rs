//! Turning the blocks of `/getwalletsyncdata` into wallet state
//! (`src/wallet_backend/wallet_synchronizer.cpp` `WalletSynchronizer::processBlock`,
//! `processBlockOutputs`, `processTransactionOutputs`, `processCoinbaseTransaction`,
//! `processTransaction`).
//!
//! [`scan_block`] only reads the keys, so the sync task runs it without holding the wallet
//! lock; [`apply_block`] then changes the state in one step.

use std::collections::HashMap;

use xkr_daemon_client::{KeyOutput, WalletBlock};
use xkr_types::{Hash, KeyImage, PublicKey, SecretKey};
use xkr_wallet_core::{SpendKeys, scan_for_subwallets};
use xkr_wallet_files::backend::UnknownFields;

use crate::state::{OwnedInput, PRUNE_SPENT_INPUTS_INTERVAL, Transaction, WalletState};

/// `Constants::GLOBAL_INDEXES_OBSCURITY`: global indexes are asked for whole ranges of this
/// many blocks, not for single transactions.
pub const GLOBAL_INDEXES_OBSCURITY: u64 = 10;

/// An output of a block that belongs to a subwallet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Found {
    /// The subwallet.
    pub owner: PublicKey,
    /// The output (global index not known yet).
    pub input: OwnedInput,
}

fn tagged(outputs: &[KeyOutput]) -> Vec<(u64, PublicKey, Option<u8>)> {
    outputs
        .iter()
        .map(|o| (o.amount, o.key, o.view_tag))
        .collect()
}

#[allow(clippy::too_many_arguments)]
fn scan_transaction(
    view_secret: &SecretKey,
    keys: &SpendKeys,
    height: u64,
    hash: Hash,
    tx_public_key: &PublicKey,
    unlock_time: u64,
    outputs: &[KeyOutput],
    found: &mut Vec<Found>,
) {
    for o in scan_for_subwallets(view_secret, keys, tx_public_key, &tagged(outputs)) {
        found.push(Found {
            owner: o.owner,
            input: OwnedInput {
                key_image: o.key_image.unwrap_or(KeyImage::ZERO),
                amount: o.amount,
                block_height: height,
                transaction_public_key: *tx_public_key,
                transaction_index: o.index as u64,
                global_output_index: None,
                key: o.key,
                spend_height: 0,
                unlock_time,
                parent_transaction_hash: hash,
                other: UnknownFields::new(),
            },
        });
    }
}

/// `processBlockOutputs`: the outputs of the coinbase and of every transaction of `block` that
/// belong to one of `keys` (view tags first, `underive_public_key` for the rest).
#[must_use]
pub(crate) fn scan_block(
    view_secret: &SecretKey,
    keys: &SpendKeys,
    block: &WalletBlock,
) -> Vec<Found> {
    let mut found = Vec::new();
    // wallet_config::processCoinbaseTransactions
    scan_transaction(
        view_secret,
        keys,
        block.height,
        block.coinbase.hash,
        &block.coinbase.tx_public_key,
        block.coinbase.unlock_time,
        &block.coinbase.outputs,
        &mut found,
    );
    for tx in &block.transactions {
        scan_transaction(
            view_secret,
            keys,
            block.height,
            tx.hash,
            &tx.tx_public_key,
            tx.unlock_time,
            &tx.outputs,
            &mut found,
        );
    }
    found
}

/// `utilities::getLowerBound` / `getUpperBound`: the block range whose global indexes are
/// fetched for a block at `height` (end exclusive).
#[must_use]
pub const fn global_index_range(height: u64) -> (u64, u64) {
    let start = height - height % GLOBAL_INDEXES_OBSCURITY;
    (start, start + GLOBAL_INDEXES_OBSCURITY)
}

/// Fills in the global indexes of `found` from a `/get_global_indexes_for_range` answer. False
/// if a transaction or an index is missing (the node forked or is faulty; C++ prints "Failed
/// to get correct global indexes from daemon" and leaves the block for the next round).
pub(crate) fn assign_global_indexes(
    found: &mut [Found],
    indexes: &HashMap<Hash, Vec<u64>>,
) -> bool {
    for f in found.iter_mut() {
        let index = usize::try_from(f.input.transaction_index)
            .ok()
            .and_then(|i| {
                indexes
                    .get(&f.input.parent_transaction_hash)?
                    .get(i)
                    .copied()
            });
        match index {
            Some(index) => f.input.global_output_index = Some(index),
            None => return false,
        }
    }
    true
}

/// What [`apply_block`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Applied {
    /// The block was processed.
    Block {
        /// New transactions touching the wallet (`onTransaction`).
        transactions: Vec<Transaction>,
        /// If the block replaced blocks the wallet had (a reorg), the first height undone.
        rolled_back: Option<u64>,
    },
    /// The wallet already has this block (same height and hash); nothing changed.
    Duplicate,
    /// The block is not the next one; nothing changed (C++ "Blocks were missed in syncing
    /// process").
    Gap,
}

/// `WalletSynchronizer::processBlock` from the scanned outputs on: undoes a fork, drops old
/// spent outputs every [`PRUNE_SPENT_INPUTS_INTERVAL`] blocks (wallet-backend-js
/// `processBlocks`), records the transactions (coinbase first with fee 0, the others with
/// the inputs minus the outputs as fee), stores the new outputs, marks spent the outputs whose
/// key images appear, and stores the block hash.
///
/// An output received and spent in the same block is found too (the block's own key images
/// are checked as well as the stored ones).
pub(crate) fn apply_block(
    state: &mut WalletState,
    block: &WalletBlock,
    found: Vec<Found>,
) -> Applied {
    let mut rolled_back = None;
    if !state.sync.is_empty() {
        let top = state.sync.height();
        if block.height <= top {
            if state.sync.hash_at(block.height) == Some(block.hash) {
                return Applied::Duplicate;
            }
            state.remove_forked(block.height);
            rolled_back = Some(block.height);
        } else if block.height > top + 1 {
            return Applied::Gap;
        }
    }
    if block.height != 0 && block.height.is_multiple_of(PRUNE_SPENT_INPUTS_INTERVAL) {
        state.prune_spent_inputs(block.height - PRUNE_SPENT_INPUTS_INTERVAL);
    }
    // Timestamps are transient, heights are not (`downloadBlocks`).
    state.convert_sync_timestamp(block.height);

    let block_key_images: HashMap<KeyImage, PublicKey> = if state.is_view_wallet {
        HashMap::new()
    } else {
        found.iter().map(|f| (f.input.key_image, f.owner)).collect()
    };

    let mut transactions = Vec::new();
    let mut spent: Vec<(PublicKey, KeyImage)> = Vec::new();

    let mut coinbase = Transaction::new(block.coinbase.hash);
    for f in found
        .iter()
        .filter(|f| f.input.parent_transaction_hash == block.coinbase.hash)
    {
        coinbase.add_transfer(f.owner, f.input.amount as i64);
    }
    if !coinbase.transfers.is_empty() {
        coinbase.timestamp = block.timestamp;
        coinbase.block_height = block.height;
        coinbase.unlock_time = block.coinbase.unlock_time;
        coinbase.is_coinbase = true;
        transactions.push(coinbase);
    }

    for raw in &block.transactions {
        let mut tx = Transaction::new(raw.hash);
        for f in found
            .iter()
            .filter(|f| f.input.parent_transaction_hash == raw.hash)
        {
            tx.add_transfer(f.owner, f.input.amount as i64);
        }
        let mut tx_spent = Vec::new();
        for input in &raw.inputs {
            let owner = state
                .key_image_owner(&input.key_image)
                .or_else(|| block_key_images.get(&input.key_image).copied());
            if let Some(owner) = owner {
                tx.add_transfer(owner, -(input.amount as i64));
                tx_spent.push((owner, input.key_image));
            }
        }
        if tx.transfers.is_empty() {
            continue;
        }
        let inputs: u64 = raw
            .inputs
            .iter()
            .map(|i| i.amount)
            .fold(0, u64::wrapping_add);
        let outputs: u64 = raw
            .outputs
            .iter()
            .map(|o| o.amount)
            .fold(0, u64::wrapping_add);
        tx.fee = inputs.wrapping_sub(outputs);
        tx.timestamp = block.timestamp;
        tx.block_height = block.height;
        tx.payment_id = raw.payment_id.as_ref().map_or_default(Hash::to_hex);
        tx.unlock_time = raw.unlock_time;
        transactions.push(tx);
        spent.extend(tx_spent);
    }

    for tx in &transactions {
        state.add_transaction(tx.clone());
    }
    for f in found {
        state.store_input(&f.owner, f.input);
    }
    for (owner, key_image) in spent {
        state.mark_spent(&owner, &key_image, block.height);
    }
    state.sync.store_block_hash(block.hash, block.height);
    Applied::Block {
        transactions,
        rolled_back,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn global_index_ranges() {
        assert_eq!(global_index_range(0), (0, 10));
        assert_eq!(global_index_range(17), (10, 20));
        assert_eq!(global_index_range(20), (20, 30));
    }
}
