//! Per-peer block-chain synchronisation as plain data plus a [`step`] function.
//!
//! A port of the state handling in `src/cryptonote_protocol/cryptonote_protocol_handler.cpp`
//! (`process_payload_sync_data`, `start_sync`, `handle_*`, `request_missing_objects`,
//! `doPushLiteBlock`). It performs **no** consensus validation: blocks and transactions
//! are handed to the node as [`SyncEvent`]s, the node verifies/stores them (batched, on
//! rayon) and feeds the outcome back with [`SyncInput::BlocksProcessed`] or
//! [`SyncInput::BlockProcessed`].
//!
//! Read access to the local chain goes through [`ChainView`], which the node implements
//! over a storage read transaction and its mempool. Two of its methods
//! ([`ChainView::parse_block_template`], [`ChainView::tx_hash`]) are hashing helpers so
//! this crate does not depend on the serialization/crypto crates.

use std::collections::{HashMap, HashSet, VecDeque};

use xkr_types::{Hash, RawBlock};

use crate::{
    constants::{
        BLOCKS_IDS_SYNCHRONIZING_DEFAULT_COUNT, BLOCKS_SYNCHRONIZING_DEFAULT_COUNT,
        MAX_MISSING_TXS, MAX_REQUEST_CHAIN_IDS, MAX_RESPONSE_BYTES, MAX_TX_POOL_RESPONSE_BYTES,
        P2P_LITE_BLOCKS_PROPAGATION_VERSION,
    },
    wire::{
        CoreSyncData, MissingTxs, NewBlock, NewLiteBlock, NewTransactions, ProtocolMessage,
        RawBlockLegacy, RequestChain, RequestGetObjects, RequestTxPool, ResponseChainEntry,
        ResponseGetObjects,
    },
};

/// What the sync machine needs from the local chain and pool.
pub trait ChainView {
    /// `(height, top hash)` where `height = top index + 1` (`get_payload_sync_data`).
    fn top(&self) -> (u32, Hash);
    /// Main-chain index of a block, `None` if unknown or only on an alternative chain.
    fn main_chain_index(&self, hash: &Hash) -> Option<u32>;
    /// `ICore::hasBlock`: main or alternative chain.
    fn has_block(&self, hash: &Hash) -> bool;
    /// Main-chain hash at `index`.
    fn block_hash_at(&self, index: u32) -> Option<Hash>;
    /// Up to `max` consecutive main-chain hashes starting at `start`.
    fn block_hashes(&self, start: u32, max: usize) -> Vec<Hash>;
    /// Raw blocks (block blob + tx blobs) for `hashes`; unknown ones go to `missed`.
    fn raw_blocks(&self, hashes: &[Hash], found: &mut Vec<RawBlock>, missed: &mut Vec<Hash>);
    /// A transaction blob from the pool or the chain (`ICore::getTransaction`).
    fn transaction_blob(&self, hash: &Hash) -> Option<Vec<u8>>;
    /// Hashes of all pool transactions (`getPoolTransactionHashes`).
    fn pool_hashes(&self) -> Vec<Hash>;
    /// Pool transactions whose hash is not in `known` (`getPoolChanges` "added" part).
    fn pool_txs_not_in(&self, known: &[Hash]) -> Vec<Vec<u8>>;
    /// [`Self::pool_txs_not_in`] with at most `max_bytes` of blobs (the rest is left out).
    /// Implementations should avoid copying more than that.
    #[must_use]
    fn pool_txs_not_in_capped(&self, known: &[Hash], max_bytes: usize) -> Vec<Vec<u8>> {
        let mut txs = self.pool_txs_not_in(known);
        let mut total = 0usize;
        txs.retain(|t| {
            total = total.saturating_add(t.len());
            total <= max_bytes
        });
        txs
    }
    /// Parses a block blob into its id and transaction hashes; `None` if it does not parse.
    fn parse_block_template(&self, blob: &[u8]) -> Option<TemplateInfo>;
    /// `getBinaryArrayHash(tx_blob)` (`cn_fast_hash`), the transaction id.
    fn tx_hash(&self, tx_blob: &[u8]) -> Hash;
}

/// Result of [`ChainView::parse_block_template`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TemplateInfo {
    /// Block id (hash of the hashing blob).
    pub hash: Hash,
    /// `transactionHashes` of the block (excluding the miner tx).
    pub tx_hashes: Vec<Hash>,
}

/// `CryptoNoteConnectionContext::m_state`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SyncStatus {
    #[default]
    BeforeHandshake,
    Synchronizing,
    Idle,
    Normal,
    Shutdown,
}

/// A lite block waiting for `NOTIFY_NEW_TRANSACTIONS` after we sent `NOTIFY_MISSING_TXS`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingLiteBlock {
    pub request: NewLiteBlock,
    pub missing: Vec<Hash>,
}

/// Per-connection sync state (`CryptoNoteConnectionContext` minus the socket).
#[derive(Debug, Clone, Default)]
pub struct PeerSync {
    pub status: SyncStatus,
    /// Peer's P2P version from the handshake (lite blocks from 4).
    pub version: u8,
    /// `m_remote_blockchain_height`
    pub remote_height: u32,
    /// `m_last_response_height`
    pub last_response_height: u32,
    /// `m_needed_objects`: block ids from chain entries not yet requested.
    pub needed: VecDeque<Hash>,
    /// `m_requested_objects`: ids of the batch in flight (at most 100).
    pub requested: Vec<Hash>,
    /// A received batch is being verified by the node.
    pub awaiting_batch: bool,
    /// `m_pending_lite_block`
    pub pending_lite: Option<PendingLiteBlock>,
    /// Node-managed block download (multi-peer downloader in the node).
    ///
    /// When `false` (the default) this machine downloads by itself, exactly like the C++
    /// per-connection logic: `NOTIFY_REQUEST_CHAIN` -> `NOTIFY_RESPONSE_CHAIN_ENTRY` ->
    /// batches of `NOTIFY_REQUEST_GET_OBJECTS`, one batch in flight per peer.
    ///
    /// When `true` the machine never requests chain entries or objects itself. Instead it
    /// emits [`SyncEvent::SyncRequired`] when the peer has blocks we lack, forwards
    /// `NOTIFY_RESPONSE_CHAIN_ENTRY` / `NOTIFY_RESPONSE_GET_OBJECTS` as
    /// [`SyncEvent::ChainEntry`] / [`SyncEvent::Objects`] to the node's downloader (which
    /// spreads batches over several peers), and waits for [`SyncInput::DownloadFinished`]
    /// before it goes to the normal (relay) state. Everything else (serving requests, relay,
    /// lite blocks, pool sync) is unchanged.
    pub managed_download: bool,
}

/// Outcome of verifying/storing a batch from [`SyncEvent::BlocksReceived`]
/// (`processObjects` + `ICore::addBlock` result conditions).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BatchOutcome {
    /// All blocks were added (main or alternative chain).
    Added,
    /// A block already existed: the C++ node switches the peer to idle.
    AlreadyExists,
    /// A block was orphaned (`BLOCK_REJECTED`): drop the peer.
    Orphaned,
    /// Block/transaction validation or deserialization failed: drop the peer.
    Invalid,
}

/// Outcome of a single relayed block ([`SyncEvent::NewBlock`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockOutcome {
    AddedToMain,
    AddedAndSwitched,
    AddedToAlternative,
    AlreadyExists,
    /// Orphan (parent unknown): resync with this peer.
    Rejected,
    /// Invalid: drop the peer.
    Invalid,
}

/// Inputs to [`step`].
#[derive(Debug, Clone)]
pub enum SyncInput {
    /// Handshake finished (either direction); `is_initial = true` path.
    Handshake {
        core_sync: CoreSyncData,
        version: u8,
    },
    /// `CORE_SYNC_DATA` from a timed sync request or response.
    TimedSync(CoreSyncData),
    /// A protocol notification from the peer.
    Message(ProtocolMessage),
    /// Result for the last [`SyncEvent::BlocksReceived`].
    BlocksProcessed(BatchOutcome),
    /// Result for a [`SyncEvent::NewBlock`].
    BlockProcessed(BlockOutcome),
    /// Managed download only: the node's downloader has every block this peer announced;
    /// go to the normal state (requests the peer's pool, emits [`SyncEvent::Synchronized`]).
    DownloadFinished,
}

/// Things the node must do on behalf of this peer.
#[derive(Debug, Clone)]
pub enum SyncAction {
    /// Send a notification to this peer.
    Send(ProtocolMessage),
    /// Hand data to the node.
    Event(SyncEvent),
    /// Close the connection (C++ `state_shutdown`).
    Disconnect(&'static str),
}

/// Data for the node (verify/store/relay). The crate does no validation.
#[derive(Debug, Clone)]
pub enum SyncEvent {
    /// A requested batch, in chain order, with the ids the peer sent (already checked to be
    /// exactly the requested set with matching tx counts). Reply with
    /// [`SyncInput::BlocksProcessed`].
    BlocksReceived {
        blocks: Vec<RawBlock>,
        hashes: Vec<Hash>,
    },
    /// A relayed block (`NOTIFY_NEW_BLOCK`, or a completed lite block). Reply with
    /// [`SyncInput::BlockProcessed`]; on `AddedToMain`/`AddedAndSwitched` relay it with
    /// `hop + 1` using [`relay_block_messages`] to all other peers.
    NewBlock {
        block: RawBlock,
        hop: u32,
        current_height: u32,
    },
    /// Relayed transactions: validate, add to the pool, relay the accepted ones to all
    /// other peers (`NOTIFY_NEW_TRANSACTIONS`).
    NewTx(Vec<Vec<u8>>),
    /// The peer's chain height changed (`updateObservedHeight`).
    PeerHeight(u32),
    /// This peer finished initial sync (`on_connection_synchronized`).
    Synchronized,
    /// Managed download only: the peer has blocks we do not know (its top id is unknown);
    /// the node's downloader should fetch them. The peer stays in the synchronizing state
    /// (ignoring relayed blocks, like C++) until [`SyncInput::DownloadFinished`].
    SyncRequired {
        /// The peer's `current_height` (top index + 1).
        remote_height: u32,
    },
    /// Managed download only: a `NOTIFY_RESPONSE_CHAIN_ENTRY` for the node's downloader.
    ChainEntry(ResponseChainEntry),
    /// Managed download only: a `NOTIFY_RESPONSE_GET_OBJECTS` for the node's downloader.
    Objects(ResponseGetObjects),
}

/// C++ `Core::doBuildSparseChain` indices for a chain whose top index is `top_index`:
/// `top, top-1, top-2, top-4, top-8, ...` (while the offset is `< top_index`) and finally
/// the genesis index 0 unless the top is genesis.
///
/// Note: the comment in `cryptonote_protocol_definitions.h` claims "first 10 sequential,
/// then powers of two" (Monero's scheme); the TurtleCoin/XKR code does powers of two
/// right away. The responder only needs the list to end with genesis, so either works; we
/// mirror what C++ actually sends.
#[must_use]
pub fn sparse_chain_indices(top_index: u32) -> Vec<u32> {
    let mut out = vec![top_index];
    let mut i: u32 = 1;
    while i < top_index {
        out.push(top_index - i);
        i = match i.checked_mul(2) {
            Some(n) => n,
            None => break,
        };
    }
    if top_index != 0 {
        out.push(0);
    }
    out
}

/// Builds the sparse chain hash list (`Core::buildSparseChain`) from a [`ChainView`].
pub fn build_sparse_chain(chain: &impl ChainView) -> Vec<Hash> {
    let (height, top) = chain.top();
    let top_index = height.saturating_sub(1);
    let mut out = Vec::new();
    for idx in sparse_chain_indices(top_index) {
        if idx == top_index {
            out.push(top);
        } else if let Some(h) = chain.block_hash_at(idx) {
            out.push(h);
        }
    }
    out
}

/// `Core::findBlockchainSupplement`: index of the first hash in `remote` that is on our
/// main chain.
pub fn find_supplement_start(chain: &impl ChainView, remote: &[Hash]) -> Option<u32> {
    remote.iter().find_map(|h| chain.main_chain_index(h))
}

/// The two relay encodings of a block (`CryptoNoteProtocolHandler::relayBlock`): peers with
/// `version >= P2P_LITE_BLOCKS_PROPAGATION_VERSION` get the lite one.
#[must_use]
pub fn relay_block_messages(
    block: &RawBlock,
    current_height: u32,
    hop: u32,
) -> (ProtocolMessage, ProtocolMessage) {
    let lite = ProtocolMessage::NewLiteBlock(NewLiteBlock {
        current_blockchain_height: current_height,
        hop,
        block_template: block.block.clone(),
    });
    let full = ProtocolMessage::NewBlock(NewBlock {
        block: RawBlockLegacy::from(block.clone()),
        current_blockchain_height: current_height,
        hop,
    });
    (lite, full)
}

/// Answers `NOTIFY_REQUEST_GET_OBJECTS` (C++ `handle_request_get_objects`) within limits the C++
/// node does not have: at most [`BLOCKS_SYNCHRONIZING_DEFAULT_COUNT`] (100) ids are looked up,
/// which is what every C++ and xkrd peer asks for; blocks stop once they would exceed
/// [`MAX_RESPONSE_BYTES`] (a C++ node cannot send more either). Ids beyond the 100 are neither
/// served nor reported missing.
#[must_use]
pub fn serve_get_objects(chain: &impl ChainView, req: &RequestGetObjects) -> ResponseGetObjects {
    serve_get_objects_held(chain, req, |_| true)
}

/// [`serve_get_objects`] for a node that does not hold every block in full (stripe pruning,
/// `docs/node-architecture.md`, "Pruned peers"): ids for which `held` is false are not read
/// and go to `missed_ids`, like unknown ones. The answer never contains a block we cannot
/// serve in full; the requester decides what to do with the rest (xkrd asks another peer; a
/// C++ node ignores `missed_ids` and drops the connection when not every requested block came
/// back).
#[must_use]
pub fn serve_get_objects_held(
    chain: &impl ChainView,
    req: &RequestGetObjects,
    held: impl Fn(&Hash) -> bool,
) -> ResponseGetObjects {
    let mut ids: Vec<Hash> = Vec::new();
    let mut missed = Vec::new();
    for id in req.blocks.iter().take(BLOCKS_SYNCHRONIZING_DEFAULT_COUNT) {
        if held(id) {
            ids.push(*id);
        } else {
            missed.push(*id);
        }
    }
    let mut found = Vec::new();
    chain.raw_blocks(&ids, &mut found, &mut missed);
    let mut bytes = 0usize;
    let mut blocks = Vec::with_capacity(found.len());
    for b in found {
        let size = b.block.len() + b.transactions.iter().map(Vec::len).sum::<usize>();
        if !blocks.is_empty() && bytes.saturating_add(size) > MAX_RESPONSE_BYTES {
            break;
        }
        bytes = bytes.saturating_add(size);
        blocks.push(RawBlockLegacy::from(b));
    }
    ResponseGetObjects {
        txs: Vec::new(),
        blocks,
        missed_ids: missed,
        current_blockchain_height: chain.top().0,
    }
}

/// Answers `NOTIFY_MISSING_TXS` (C++ `handle_notify_missing_txs`): every requested transaction
/// from the pool or the chain, or an error (C++ then shuts the connection down). Within limits
/// C++ does not have: at most [`MAX_MISSING_TXS`] hashes, duplicates answered once (a
/// block never lists a transaction twice), at most [`MAX_RESPONSE_BYTES`] in the answer.
pub fn serve_missing_txs(
    chain: &impl ChainView,
    req: &MissingTxs,
) -> Result<NewTransactions, &'static str> {
    if req.missing_txs.len() > MAX_MISSING_TXS {
        return Err("NOTIFY_MISSING_TXS: too many transactions requested");
    }
    let mut seen = HashSet::with_capacity(req.missing_txs.len());
    let mut txs = Vec::with_capacity(req.missing_txs.len());
    let mut bytes = 0usize;
    for h in &req.missing_txs {
        if !seen.insert(*h) {
            continue;
        }
        let Some(tx) = chain.transaction_blob(h) else {
            return Err("NOTIFY_MISSING_TXS: unable to provide requested txs");
        };
        bytes = bytes.saturating_add(tx.len());
        if bytes > MAX_RESPONSE_BYTES {
            return Err("NOTIFY_MISSING_TXS: the answer would be too large");
        }
        txs.push(tx);
    }
    Ok(NewTransactions { txs })
}

/// Whether a peer of `version` should receive the lite relay message.
#[must_use]
pub const fn wants_lite_blocks(version: u8) -> bool {
    version >= P2P_LITE_BLOCKS_PROPAGATION_VERSION
}

/// Advances one peer's sync state. Appends what the node must do to `out`.
pub fn step(
    state: &mut PeerSync,
    input: SyncInput,
    chain: &impl ChainView,
    out: &mut Vec<SyncAction>,
) {
    if state.status == SyncStatus::Shutdown {
        return;
    }
    match input {
        SyncInput::Handshake { core_sync, version } => {
            state.version = version;
            process_sync_data(state, &core_sync, true, chain, out);
        }
        SyncInput::TimedSync(core_sync) => process_sync_data(state, &core_sync, false, chain, out),
        SyncInput::Message(msg) => on_message(state, msg, chain, out),
        SyncInput::BlocksProcessed(outcome) => on_batch_processed(state, outcome, chain, out),
        SyncInput::BlockProcessed(outcome) => on_block_processed(state, outcome, chain, out),
        SyncInput::DownloadFinished => {
            if matches!(state.status, SyncStatus::Synchronizing | SyncStatus::Idle) {
                state.status = SyncStatus::Normal;
                state.needed.clear();
                state.requested.clear();
                send_request_tx_pool(chain, out);
                out.push(SyncAction::Event(SyncEvent::Synchronized));
            }
        }
    }
}

/// C++ `start_sync`: request the peer's chain, or (managed download) hand over to the node.
fn start_sync(
    state: &mut PeerSync,
    remote_height: u32,
    chain: &impl ChainView,
    out: &mut Vec<SyncAction>,
) {
    state.status = SyncStatus::Synchronizing;
    state.needed.clear();
    state.requested.clear();
    if state.managed_download {
        out.push(SyncAction::Event(SyncEvent::SyncRequired { remote_height }));
    } else {
        send_request_chain(chain, out);
    }
}

fn shutdown(state: &mut PeerSync, reason: &'static str, out: &mut Vec<SyncAction>) {
    state.status = SyncStatus::Shutdown;
    state.needed.clear();
    state.requested.clear();
    state.pending_lite = None;
    out.push(SyncAction::Disconnect(reason));
}

fn set_remote_height(state: &mut PeerSync, height: u32, out: &mut Vec<SyncAction>) {
    if height != state.remote_height {
        out.push(SyncAction::Event(SyncEvent::PeerHeight(height)));
    }
    state.remote_height = height;
}

fn send_request_chain(chain: &impl ChainView, out: &mut Vec<SyncAction>) {
    out.push(SyncAction::Send(ProtocolMessage::RequestChain(
        RequestChain {
            block_ids: build_sparse_chain(chain),
        },
    )));
}

fn send_request_tx_pool(chain: &impl ChainView, out: &mut Vec<SyncAction>) {
    out.push(SyncAction::Send(ProtocolMessage::RequestTxPool(
        RequestTxPool {
            txs: chain.pool_hashes(),
        },
    )));
}

/// `process_payload_sync_data` (+ the `state_sync_required` / `state_pool_sync_required`
/// transitions that C++ performs at the top of its connection loop).
fn process_sync_data(
    state: &mut PeerSync,
    sync: &CoreSyncData,
    is_initial: bool,
    chain: &impl ChainView,
    out: &mut Vec<SyncAction>,
) {
    if state.status == SyncStatus::BeforeHandshake && !is_initial {
        return;
    }
    if state.status == SyncStatus::Synchronizing {
        // keep going, only refresh the height below
    } else if chain.has_block(&sync.top_id) {
        if is_initial {
            // state_pool_sync_required -> requestMissingPoolTransactions -> normal
            state.status = SyncStatus::Normal;
            send_request_tx_pool(chain, out);
            out.push(SyncAction::Event(SyncEvent::Synchronized));
        } else {
            state.status = SyncStatus::Normal;
        }
    } else {
        // state_sync_required -> start_sync
        start_sync(state, sync.current_height, chain, out);
    }
    set_remote_height(state, sync.current_height, out);
}

fn on_message(
    state: &mut PeerSync,
    msg: ProtocolMessage,
    chain: &impl ChainView,
    out: &mut Vec<SyncAction>,
) {
    match msg {
        ProtocolMessage::NewBlock(arg) => {
            set_remote_height(state, arg.current_blockchain_height, out);
            if state.status != SyncStatus::Normal {
                return;
            }
            // A block we already have costs a hash lookup only (C++ would run addBlock
            // and get "already exists").
            if chain
                .parse_block_template(&arg.block.block)
                .is_some_and(|info| chain.has_block(&info.hash))
            {
                return;
            }
            out.push(SyncAction::Event(SyncEvent::NewBlock {
                block: arg.block.into(),
                hop: arg.hop,
                current_height: arg.current_blockchain_height,
            }));
        }
        ProtocolMessage::NewTransactions(arg) => {
            if state.status != SyncStatus::Normal {
                return;
            }
            if let Some(pending) = state.pending_lite.take() {
                push_lite_block(
                    state,
                    pending.request,
                    Some(pending.missing.as_slice()),
                    arg.txs,
                    chain,
                    out,
                );
            } else if !arg.txs.is_empty() {
                out.push(SyncAction::Event(SyncEvent::NewTx(arg.txs)));
            }
        }
        ProtocolMessage::RequestGetObjects(arg) => {
            out.push(SyncAction::Send(ProtocolMessage::ResponseGetObjects(
                serve_get_objects(chain, &arg),
            )));
        }
        ProtocolMessage::ResponseGetObjects(arg) if state.managed_download => {
            set_remote_height(state, arg.current_blockchain_height, out);
            out.push(SyncAction::Event(SyncEvent::Objects(arg)));
        }
        ProtocolMessage::ResponseGetObjects(arg) => on_response_get_objects(state, arg, chain, out),
        ProtocolMessage::RequestChain(arg) => {
            let Some(last) = arg.block_ids.last() else {
                return shutdown(state, "NOTIFY_REQUEST_CHAIN: block_ids is empty", out);
            };
            // A sparse chain is ~log2(height) ids; each one is a database lookup.
            if arg.block_ids.len() > MAX_REQUEST_CHAIN_IDS {
                return shutdown(state, "NOTIFY_REQUEST_CHAIN: too many block ids", out);
            }
            if chain.block_hash_at(0).as_ref() != Some(last) {
                return shutdown(
                    state,
                    "NOTIFY_REQUEST_CHAIN: block_ids doesn't end with genesis",
                    out,
                );
            }
            let Some(start) = find_supplement_start(chain, &arg.block_ids) else {
                // C++ throws "Genesis block hash was not found." -> connection closed
                return shutdown(state, "NOTIFY_REQUEST_CHAIN: no common block", out);
            };
            out.push(SyncAction::Send(ProtocolMessage::ResponseChainEntry(
                ResponseChainEntry {
                    start_height: start,
                    total_height: chain.top().0,
                    block_ids: chain.block_hashes(start, BLOCKS_IDS_SYNCHRONIZING_DEFAULT_COUNT),
                },
            )));
        }
        ProtocolMessage::ResponseChainEntry(arg) if state.managed_download => {
            out.push(SyncAction::Event(SyncEvent::ChainEntry(arg)));
        }
        ProtocolMessage::ResponseChainEntry(arg) => on_response_chain_entry(state, arg, chain, out),
        ProtocolMessage::RequestTxPool(arg) => {
            // The protocol answers with full blobs only; the answer is capped (the rest
            // reaches the peer through relays). The connection task rate-limits the requests.
            let txs = chain.pool_txs_not_in_capped(&arg.txs, MAX_TX_POOL_RESPONSE_BYTES);
            if !txs.is_empty() {
                out.push(SyncAction::Send(ProtocolMessage::NewTransactions(
                    NewTransactions { txs },
                )));
            }
        }
        ProtocolMessage::NewLiteBlock(arg) => {
            set_remote_height(state, arg.current_blockchain_height, out);
            if state.status != SyncStatus::Normal {
                return;
            }
            push_lite_block(state, arg, None, Vec::new(), chain, out);
        }
        ProtocolMessage::MissingTxs(arg) => match serve_missing_txs(chain, &arg) {
            Ok(answer) => out.push(SyncAction::Send(ProtocolMessage::NewTransactions(answer))),
            Err(reason) => shutdown(state, reason, out),
        },
    }
}

/// `handle_response_get_objects`: protocol checks only, then hand the batch to the node.
fn on_response_get_objects(
    state: &mut PeerSync,
    arg: ResponseGetObjects,
    chain: &impl ChainView,
    out: &mut Vec<SyncAction>,
) {
    if state.last_response_height > arg.current_blockchain_height {
        return shutdown(
            state,
            "NOTIFY_RESPONSE_GET_OBJECTS: height went backwards",
            out,
        );
    }
    set_remote_height(state, arg.current_blockchain_height, out);

    let mut hashes = Vec::with_capacity(arg.blocks.len());
    for (index, raw) in arg.blocks.iter().enumerate() {
        let Some(info) = chain.parse_block_template(&raw.block) else {
            return shutdown(
                state,
                "NOTIFY_RESPONSE_GET_OBJECTS: block does not parse",
                out,
            );
        };
        // C++ quirk kept as-is: if the *second* block is already known, go idle.
        if index == 1 && chain.has_block(&info.hash) {
            state.status = SyncStatus::Idle;
            state.needed.clear();
            state.requested.clear();
            return;
        }
        let Some(pos) = state.requested.iter().position(|h| *h == info.hash) else {
            return shutdown(
                state,
                "NOTIFY_RESPONSE_GET_OBJECTS: block wasn't requested",
                out,
            );
        };
        if info.tx_hashes.len() != raw.txs.len() {
            return shutdown(state, "NOTIFY_RESPONSE_GET_OBJECTS: tx count mismatch", out);
        }
        state.requested.swap_remove(pos);
        hashes.push(info.hash);
    }
    if !state.requested.is_empty() {
        return shutdown(
            state,
            "NOTIFY_RESPONSE_GET_OBJECTS: not all requested objects returned",
            out,
        );
    }

    state.awaiting_batch = true;
    out.push(SyncAction::Event(SyncEvent::BlocksReceived {
        blocks: arg.blocks.into_iter().map(RawBlock::from).collect(),
        hashes,
    }));
}

fn on_batch_processed(
    state: &mut PeerSync,
    outcome: BatchOutcome,
    chain: &impl ChainView,
    out: &mut Vec<SyncAction>,
) {
    state.awaiting_batch = false;
    match outcome {
        BatchOutcome::Added => {
            if state.status == SyncStatus::Synchronizing {
                request_missing_objects(state, true, chain, out);
            }
        }
        BatchOutcome::AlreadyExists => {
            state.status = SyncStatus::Idle;
            state.needed.clear();
            state.requested.clear();
        }
        BatchOutcome::Orphaned => shutdown(state, "block received at sync phase was orphaned", out),
        BatchOutcome::Invalid => shutdown(state, "block verification failed", out),
    }
}

fn on_block_processed(
    state: &mut PeerSync,
    outcome: BlockOutcome,
    chain: &impl ChainView,
    out: &mut Vec<SyncAction>,
) {
    match outcome {
        BlockOutcome::AddedAndSwitched => send_request_tx_pool(chain, out),
        BlockOutcome::AddedToMain
        | BlockOutcome::AddedToAlternative
        | BlockOutcome::AlreadyExists => {}
        BlockOutcome::Rejected => {
            let remote_height = state.remote_height;
            start_sync(state, remote_height, chain, out);
        }
        BlockOutcome::Invalid => shutdown(state, "block verification failed", out),
    }
}

/// `handle_response_chain_entry`.
fn on_response_chain_entry(
    state: &mut PeerSync,
    arg: ResponseChainEntry,
    chain: &impl ChainView,
    out: &mut Vec<SyncAction>,
) {
    let Some(first) = arg.block_ids.first() else {
        return shutdown(state, "NOTIFY_RESPONSE_CHAIN_ENTRY: empty m_block_ids", out);
    };
    if !chain.has_block(first) {
        return shutdown(
            state,
            "NOTIFY_RESPONSE_CHAIN_ENTRY: starts from unknown id",
            out,
        );
    }
    set_remote_height(state, arg.total_height, out);
    state.last_response_height = arg
        .start_height
        .saturating_add(arg.block_ids.len() as u32)
        .saturating_sub(1);
    if state.last_response_height > state.remote_height {
        return shutdown(
            state,
            "NOTIFY_RESPONSE_CHAIN_ENTRY: start + count > total",
            out,
        );
    }

    let mut all_known = true;
    for id in arg.block_ids {
        if all_known && chain.has_block(&id) {
            continue;
        }
        all_known = false;
        state.needed.push_back(id);
    }
    request_missing_objects(state, false, chain, out);
}

/// `request_missing_objects`.
fn request_missing_objects(
    state: &mut PeerSync,
    check_having_blocks: bool,
    chain: &impl ChainView,
    out: &mut Vec<SyncAction>,
) {
    if !state.needed.is_empty() {
        let mut blocks = Vec::with_capacity(BLOCKS_SYNCHRONIZING_DEFAULT_COUNT);
        while blocks.len() < BLOCKS_SYNCHRONIZING_DEFAULT_COUNT {
            let Some(id) = state.needed.pop_front() else {
                break;
            };
            if !(check_having_blocks && chain.has_block(&id)) {
                blocks.push(id);
            }
        }
        if !blocks.is_empty() {
            state.requested.extend_from_slice(&blocks);
            out.push(SyncAction::Send(ProtocolMessage::RequestGetObjects(
                RequestGetObjects {
                    txs: Vec::new(),
                    blocks,
                },
            )));
            return;
        }
        // Everything left was already known: fall through to the chain-entry checks
        // (C++ would send an empty request and loop once more).
    }

    if state.last_response_height < state.remote_height.saturating_sub(1) {
        send_request_chain(chain, out);
    } else if state.last_response_height == state.remote_height.saturating_sub(1)
        && state.needed.is_empty()
        && state.requested.is_empty()
    {
        send_request_tx_pool(chain, out);
        state.status = SyncStatus::Normal;
        out.push(SyncAction::Event(SyncEvent::Synchronized));
    }
    // else: C++ logs "request_missing_blocks final condition failed!" and stays put.
}

/// `doPushLiteBlock`. `expected` is the missing-tx list of a pending lite block when this is
/// the `NOTIFY_NEW_TRANSACTIONS` answer to our `NOTIFY_MISSING_TXS`.
fn push_lite_block(
    state: &mut PeerSync,
    arg: NewLiteBlock,
    expected: Option<&[Hash]>,
    provided: Vec<Vec<u8>>,
    chain: &impl ChainView,
    out: &mut Vec<SyncAction>,
) {
    let Some(info) = chain.parse_block_template(&arg.block_template) else {
        return shutdown(
            state,
            "lite block: deserialization of block template failed",
            out,
        );
    };
    // A block we already have costs a hash lookup only (and no NOTIFY_MISSING_TXS).
    if chain.has_block(&info.hash) {
        state.pending_lite = None;
        return;
    }
    // Every listed transaction is a pool or database lookup; no real block lists this many.
    if info.tx_hashes.len() > MAX_MISSING_TXS {
        return shutdown(state, "lite block: too many transactions", out);
    }

    // An answer to our NOTIFY_MISSING_TXS carries at most the block's transactions (C++
    // `handle_notify_missing_txs` sends only those asked for); each blob is hashed below.
    if provided.len() > info.tx_hashes.len() {
        return shutdown(
            state,
            "lite block: more transactions than the block lists",
            out,
        );
    }
    // A map, not a list: both the template and `provided` are peer-controlled, so matching
    // them stays linear.
    let provided: HashMap<Hash, Vec<u8>> = provided
        .into_iter()
        .map(|tx| (chain.tx_hash(&tx), tx))
        .collect();
    if let Some(expected) = expected
        && expected.iter().any(|h| !provided.contains_key(h))
    {
        return shutdown(
            state,
            "lite block: peer didn't provide a missing transaction",
            out,
        );
    }

    let mut have = Vec::with_capacity(info.tx_hashes.len());
    let mut need = Vec::new();
    for h in &info.tx_hashes {
        if let Some(tx) = provided.get(h) {
            have.push(tx.clone());
        } else if let Some(tx) = chain.transaction_blob(h) {
            have.push(tx);
        } else {
            need.push(*h);
        }
    }

    if need.is_empty() {
        state.pending_lite = None;
        out.push(SyncAction::Event(SyncEvent::NewBlock {
            block: RawBlock {
                block: arg.block_template,
                transactions: have,
            },
            hop: arg.hop,
            current_height: arg.current_blockchain_height,
        }));
    } else if expected.is_some() {
        shutdown(
            state,
            "lite block: peer didn't provide all necessary transactions",
            out,
        );
    } else {
        out.push(SyncAction::Send(ProtocolMessage::MissingTxs(MissingTxs {
            current_blockchain_height: arg.current_blockchain_height,
            block_hash: info.hash,
            missing_txs: need.clone(),
        })));
        state.pending_lite = Some(PendingLiteBlock {
            request: arg,
            missing: need,
        });
    }
}

#[cfg(test)]
mod tests;
