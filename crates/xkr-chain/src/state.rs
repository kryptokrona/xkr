//! State hashes per stripe span (`docs/storage.md` "State hashes").
//!
//! The chain is cut into spans of [`STRIPE_SPAN`] blocks (the pruning spans). For span `k`
//! (heights `4096k .. 4096k + 4095`) the node computes
//!
//! ```text
//! D_k = cn_fast_hash(entries of the span's blocks, see `docs/storage.md`)
//! S_k = cn_fast_hash("xkr state v1" || S_{k-1} || D_k),   S_{-1} = 32 zero bytes
//! ```
//!
//! from exactly the data a pruned node keeps and `/getwalletsyncdata` plus
//! `/get_global_indexes_for_range` return, so a wallet can recompute `D_k` from RPC answers.
//!
//! A span is hashed once it is entirely at or below `tip - max_reorg_depth`
//! ([`Chain::stable_spans`]); the hashes are stored in `META_BLOBS["state_hashes"]` together
//! with the hash of each span's last block, and dropped from the first span whose last block
//! is no longer on the main chain. A computed hash that differs from the compiled-in list
//! ([`MAINNET_STATE_HASHES`]) is logged as an error, shown in `/status.state.mismatch`, and
//! the chain refuses further blocks ([`ChainError::StateMismatch`]).

use std::collections::BTreeMap;
use std::ops::Range;

use xkr_consensus::Network;
use xkr_crypto::{FastHasher, cn_fast_hash};
use xkr_serialization::{decode_block, decode_transaction_prefix, transaction_hash};
use xkr_storage::{Compressor, Reader, STRIPE_SPAN};
use xkr_types::{Hash, TransactionPrefix, TxInput, wallet_tx_public_key};

use crate::{Chain, ChainError};

/// Domain prefix of [`chain_state`].
pub const STATE_DOMAIN: &[u8; 12] = b"xkr state v1";

/// Mainnet state hashes `(span, S_span)` checked by every node, generated with
/// `xkrd db state-hashes` from a fully verified node.
pub const MAINNET_STATE_HASHES: &[(u32, &str)] = &[];

/// Testnet state hashes (none: testnet chains are rebuilt).
pub const TESTNET_STATE_HASHES: &[(u32, &str)] = &[];

/// Spans hashed per [`Chain::update_state_hashes`] call by the node's state thread before it
/// looks at the tip again.
pub const STATE_SPANS_PER_STEP: usize = 8;

/// A compiled-in (or injected) list of state hashes.
#[derive(Debug, Clone, Default)]
pub struct StateHashes {
    map: BTreeMap<u64, Hash>,
}

impl StateHashes {
    /// The compiled-in list of `network`.
    ///
    /// # Panics
    /// Never for the constants above (they are valid hex).
    #[must_use]
    pub fn for_network(network: Network) -> Self {
        let list = match network {
            Network::Mainnet => MAINNET_STATE_HASHES,
            Network::Testnet => TESTNET_STATE_HASHES,
        };
        Self::from_list(list.iter().map(|(span, s)| {
            (
                u64::from(*span),
                s.parse().expect("state hash is valid hex"),
            )
        }))
    }

    /// A list from `(span, state hash)` pairs.
    pub fn from_list(list: impl IntoIterator<Item = (u64, Hash)>) -> Self {
        Self {
            map: list.into_iter().collect(),
        }
    }

    /// The listed hash of `span`, if any.
    #[must_use]
    pub fn get(&self, span: u64) -> Option<Hash> {
        self.map.get(&span).copied()
    }

    /// `Some(true)` if `span` is listed with `hash`, `Some(false)` if it is listed with another
    /// hash, `None` if it is not listed.
    #[must_use]
    pub fn matches(&self, span: u64, hash: &Hash) -> Option<bool> {
        self.get(span).map(|h| h == *hash)
    }

    /// Every `(span, hash)`, ascending.
    pub fn iter(&self) -> impl Iterator<Item = (u64, Hash)> + '_ {
        self.map.iter().map(|(s, h)| (*s, *h))
    }

    /// Whether the list is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
}

/// A computed state hash that differs from the compiled-in one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StateMismatch {
    /// The span.
    pub span: u64,
    /// The compiled-in hash.
    pub expected: Hash,
    /// The hash computed from this database.
    pub found: Hash,
}

/// The node's state hashes as [`Chain::state_view`] shows them.
#[derive(Debug, Clone, Default)]
pub struct StateView {
    /// `(S_k, hash of the last block of span k)` for spans `0..len`.
    pub hashes: Vec<(Hash, Hash)>,
    /// The first span whose hash differs from the compiled-in list.
    pub mismatch: Option<StateMismatch>,
}

impl StateView {
    /// The newest stored span and its state hash.
    #[must_use]
    pub fn latest(&self) -> Option<(u64, Hash)> {
        let span = self.hashes.len().checked_sub(1)?;
        Some((span as u64, self.hashes[span].0))
    }

    /// The stored state hash of `span`.
    #[must_use]
    pub fn get(&self, span: u64) -> Option<Hash> {
        let i = usize::try_from(span).ok()?;
        self.hashes.get(i).map(|e| e.0)
    }
}

/// What one [`Chain::update_state_hashes`] call did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StateUpdate {
    /// Spans hashed in this call.
    pub computed: u64,
    /// Stored spans dropped first because their last block left the main chain.
    pub dropped: u64,
    /// Stable spans still to hash.
    pub pending: u64,
}

/// Heights of `span`.
#[must_use]
pub const fn span_heights(span: u64) -> Range<u64> {
    span * STRIPE_SPAN..(span + 1) * STRIPE_SPAN
}

/// `S_k = cn_fast_hash("xkr state v1" || S_{k-1} || D_k)`.
#[must_use]
pub fn chain_state(prev: &Hash, digest: &Hash) -> Hash {
    let mut buf = [0u8; 12 + 64];
    buf[..12].copy_from_slice(STATE_DOMAIN);
    buf[12..44].copy_from_slice(&prev.0);
    buf[44..].copy_from_slice(&digest.0);
    cn_fast_hash(&buf)
}

fn put_u64(h: &mut FastHasher, v: u64) {
    h.update(&v.to_le_bytes());
}

fn corrupt(height: u64, what: &dyn std::fmt::Display) -> ChainError {
    ChainError::Corrupt(format!("block {height}: {what}"))
}

/// One transaction's entry of `D_k`.
fn absorb_tx(
    h: &mut FastHasher,
    height: u64,
    hash: &Hash,
    prefix: &TransactionPrefix,
    global_indexes: &[u32],
) -> Result<(), ChainError> {
    if global_indexes.len() != prefix.outputs.len() {
        return Err(corrupt(
            height,
            &format!(
                "transaction {hash} has {} global indexes for {} outputs",
                global_indexes.len(),
                prefix.outputs.len()
            ),
        ));
    }
    h.update(&hash.0);
    h.update(&wallet_tx_public_key(&prefix.extra));
    put_u64(h, prefix.unlock_time);
    put_u64(h, prefix.outputs.len() as u64);
    for (o, gi) in prefix.outputs.iter().zip(global_indexes) {
        h.update(&o.key().0);
        put_u64(h, o.amount);
        match o.view_tag() {
            Some(tag) => h.update(&[1, tag]),
            None => h.update(&[0, 0]),
        }
        put_u64(h, u64::from(*gi));
    }
    put_u64(h, prefix.inputs.len() as u64);
    for input in &prefix.inputs {
        match input {
            TxInput::Key { key_image, .. } => h.update(&key_image.0),
            TxInput::Coinbase { height } => put_u64(h, u64::from(*height)),
        }
    }
    Ok(())
}

/// `D_span` from `reader`. Every block of the span must be stored (the span is below the tip);
/// signatures are not read, so a pruned database gives the same digest as a full one.
pub fn span_digest(reader: &Reader, comp: &mut Compressor, span: u64) -> Result<Hash, ChainError> {
    let heights = span_heights(span);
    let infos = reader.block_infos(heights.start, heights.end)?;
    if infos.len() as u64 != STRIPE_SPAN {
        return Err(ChainError::Corrupt(format!(
            "span {span} is not complete ({} of {STRIPE_SPAN} blocks stored)",
            infos.len()
        )));
    }
    let mut h = FastHasher::new();
    for (height, info) in heights.zip(&infos) {
        let stored = reader
            .block_prefixes(height, comp)?
            .ok_or_else(|| corrupt(height, &"no block record"))?;
        let block = decode_block(&stored.block).map_err(|e| corrupt(height, &e))?;
        if stored.txs.len() != block.tx_hashes.len() {
            return Err(corrupt(
                height,
                &format!(
                    "{} transaction records for {} hashes",
                    stored.txs.len(),
                    block.tx_hashes.len()
                ),
            ));
        }
        put_u64(&mut h, height);
        h.update(&info.hash);
        put_u64(&mut h, block.header.timestamp);
        put_u64(&mut h, 1 + block.tx_hashes.len() as u64);
        let miner_hash = transaction_hash(&block.miner_tx)?;
        absorb_tx(
            &mut h,
            height,
            &miner_hash,
            &block.miner_tx.prefix,
            &stored.miner_global_indexes,
        )?;
        for ((blob, global_indexes), hash) in stored.txs.iter().zip(&block.tx_hashes) {
            let prefix = decode_transaction_prefix(blob).map_err(|e| corrupt(height, &e))?;
            absorb_tx(&mut h, height, hash, &prefix, global_indexes)?;
        }
    }
    Ok(h.finalize())
}

/// Number of complete spans whose blocks are all stored at or below `top`.
#[must_use]
pub const fn complete_spans(top: u64) -> u64 {
    (top + 1) / STRIPE_SPAN
}

impl Chain {
    /// Spans entirely at or below `tip - max_reorg_depth` (`0..n`): the ones that get a state
    /// hash.
    #[must_use]
    pub fn stable_spans(&self) -> u64 {
        let tip = self.tip().height;
        match tip.checked_sub(self.config().max_reorg_depth) {
            Some(stable_top) => complete_spans(stable_top),
            None => 0,
        }
    }

    /// The stored state hashes and the mismatch, if any.
    #[must_use]
    pub fn state_view(&self) -> StateView {
        self.state.read().clone()
    }

    /// The first mismatch against the compiled-in list, if any.
    #[must_use]
    pub fn state_mismatch(&self) -> Option<StateMismatch> {
        self.state.read().mismatch
    }

    /// Hashes up to `max_spans` stable spans that have no state hash yet. First drops the
    /// stored hashes from the first span whose last block is no longer on the main chain.
    /// Stops at the first hash that differs from the compiled-in list: that span is not
    /// stored, [`Self::state_mismatch`] names it and the chain takes no more blocks. Does
    /// nothing once a mismatch is set.
    pub fn update_state_hashes(&self, max_spans: usize) -> Result<StateUpdate, ChainError> {
        let _guard = self.state_update.lock();
        let mut update = StateUpdate::default();
        if self.state_mismatch().is_some() {
            return Ok(update);
        }
        let reader = self.reader()?;
        let mut hashes = self.state.read().hashes.clone();
        let stored = hashes.len();
        while let Some((_, last)) = hashes.last() {
            let span = hashes.len() as u64 - 1;
            if reader.block_hash(span_heights(span).end - 1)? == Some(*last) {
                break;
            }
            hashes.pop();
        }
        update.dropped = (stored - hashes.len()) as u64;
        if update.dropped > 0 {
            tracing::warn!(
                from_span = hashes.len(),
                spans = update.dropped,
                "state hashes dropped: their blocks are no longer on the main chain"
            );
        }
        let target = self
            .stable_spans()
            .min(complete_spans(reader.top_height()?.unwrap_or_default()));
        let builtin = &self.config().state_hashes;
        let mut mismatch = None;
        while (hashes.len() as u64) < target && update.computed < max_spans as u64 {
            let span = hashes.len() as u64;
            let digest = self.with_compressor(|c| span_digest(&reader, c, span))??;
            let prev = hashes.last().map_or(Hash::ZERO, |e| e.0);
            let state = chain_state(&prev, &digest);
            if let Some(expected) = builtin.get(span)
                && expected != state
            {
                mismatch = Some(StateMismatch {
                    span,
                    expected,
                    found: state,
                });
                break;
            }
            let last = reader
                .block_hash(span_heights(span).end - 1)?
                .ok_or_else(|| ChainError::Corrupt(format!("span {span} has no last block")))?;
            hashes.push((state, last));
            update.computed += 1;
        }
        update.pending = target.saturating_sub(hashes.len() as u64);
        if update.dropped > 0 || update.computed > 0 {
            let raw: Vec<([u8; 32], [u8; 32])> = hashes.iter().map(|(s, l)| (s.0, l.0)).collect();
            self.store().set_state_hashes(&raw)?;
        }
        let mut state = self.state.write();
        state.hashes = hashes;
        if let Some(m) = mismatch {
            log_mismatch(&m);
            state.mismatch = Some(m);
            update.pending = 0;
        }
        Ok(update)
    }
}

/// The error line of a mismatch.
pub(crate) fn log_mismatch(m: &StateMismatch) {
    tracing::error!(
        span = m.span,
        found = %m.found,
        expected = %m.expected,
        "the state hash of this database differs from the built-in one; the chain takes no \
         more blocks (check the database with `xkrd db verify-state`)"
    );
}

/// The state view of a freshly opened store: the stored hashes and the first of them that
/// differs from `builtin`.
pub(crate) fn load_view(
    store: &xkr_storage::Store,
    builtin: &StateHashes,
) -> Result<StateView, ChainError> {
    let hashes: Vec<(Hash, Hash)> = store
        .state_hashes()?
        .into_iter()
        .map(|(s, l)| (Hash(s), Hash(l)))
        .collect();
    let mismatch = hashes.iter().enumerate().find_map(|(span, (found, _))| {
        let span = span as u64;
        builtin
            .get(span)
            .filter(|expected| expected != found)
            .map(|expected| StateMismatch {
                span,
                expected,
                found: *found,
            })
    });
    if let Some(m) = &mismatch {
        log_mismatch(m);
    }
    Ok(StateView { hashes, mismatch })
}
