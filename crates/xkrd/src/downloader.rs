//! Multi-peer block downloader (Cuprate's `block_downloader`, over the legacy protocol).
//!
//! * **Chain tracker**: block ids from `NOTIFY_RESPONSE_CHAIN_ENTRY` (up to 10 000 per entry),
//!   starting at the first id we do not have. When it runs low, the next entry is requested
//!   with the tracker's last id in front of our sparse chain, so the peer continues where the
//!   tracker ends even though those blocks are not verified yet.
//! * **Requests**: batches of up to `BLOCKS_SYNCHRONIZING_DEFAULT_COUNT` (100) ids go to every
//!   idle peer whose height covers them, one `NOTIFY_REQUEST_GET_OBJECTS` in flight per peer.
//!   Failed or timed-out batches are queued again and go to another peer first.
//! * **Stripes** (`docs/node-architecture.md`, "Pruned peers"): a batch never straddles a
//!   [`STRIPE_SPAN`] boundary and goes only to a peer that holds all of it in full
//!   ([`PeerSlot::holds`]: full peers, a pruned peer's own stripe, and every pruned peer's
//!   recent [`PRUNED_TIP_BLOCKS`]). A pruned peer that cannot serve the next batch leaves it
//!   queued for others and takes a later one of its stripe inside the download window. When
//!   no connected peer holds the next range (or every one that should has answered it as
//!   missed), [`Downloader::unserved`] names it (the P2P manager looks for its holders:
//!   Hyperswarm stripe topics of every [`stripe_holders_for`] seed, peer list,
//!   [`Downloader::rotation_victim`] within a per-span [`rotation_backoff`] budget),
//!   [`Downloader::missing_stripe`] reports its default-count stripe and a rate-limited log
//!   says so.
//! * **Peer records** (survive resets, dropped with the connection): verified blocks each peer
//!   delivered (`fill` asks the most productive peers first), how often it answered a range
//!   its seed claims (full, or its own stripe) as missed, and how often its chain entry added
//!   nothing while it announced more ([`CHAIN_STRIKE_LIMIT`]). A miss of a claimed range is
//!   recorded at once for the peer whose chain entry the tracker follows; for any other peer it
//!   is recorded once that range verified (until then it is pending). It never counts towards
//!   the three-failure reset, and the peer is dropped (no ban) after [`CLAIMED_MISS_LIMIT`]
//!   recorded misses. A miss in a range we only assumed a pruned peer holds (its recent blocks)
//!   is not counted either. When every peer that holds the next range has missed it for
//!   [`STALL_RESET`], the downloader starts over; if that range was not confirmed, the tracker's
//!   source gets a chain strike.
//! * **Chain sources**: chain entries are requested from the peer with the fewest chain
//!   strikes, then the tracker's source, then the most verified blocks delivered, then the
//!   highest announced height counted up to [`network_height_estimate`]. Tracker ids count as
//!   confirmed below the height where a batch verified or where a chain entry of another peer
//!   (a continuation, or a second chain request sent when the next range has no holder) has
//!   the same ids; [`Downloader::rotation_range`] only names confirmed ranges.
//! * **Reordering and backpressure**: responses land in a height-ordered buffer. Requests are
//!   only issued while the buffer is below a byte budget and not too far ahead of the next
//!   block to verify (except the very next batch, so a slow peer cannot stall everything).
//! * **Verification queue**: consecutive buffered batches are merged into jobs of up to
//!   `max_job_blocks` and handed to the chain thread, at most `max_queued_jobs` at a time.
//! * **Failure**: an invalid block bans the peer that sent it and resets the downloader (new
//!   generation; stale responses and results are ignored). A batch that timed out or lost its
//!   peer three times also resets it.
//!
//! The downloader is plain data driven by the P2P manager; it never touches the network
//! itself and returns [`DlAction`]s.

use std::cmp::Reverse;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::time::{Duration, Instant};

use xkr_chain::{BatchErrorKind, BatchReport};
use xkr_p2p_legacy::constants::{
    BLOCKS_IDS_SYNCHRONIZING_DEFAULT_COUNT, BLOCKS_SYNCHRONIZING_DEFAULT_COUNT,
};
use xkr_p2p_legacy::wire::{
    ProtocolMessage, RequestChain, RequestGetObjects, ResponseChainEntry, ResponseGetObjects,
};
use xkr_storage::pruning::{DEFAULT_STRIPES, LOOKUP_STRIPES, PruningSeed, STRIPE_SPAN};
use xkr_types::{Hash, RawBlock};

use crate::chain_worker::ChainJob;

/// Blocks below its top that every stripe-pruned peer is assumed to keep in full, whatever its
/// stripe. A pruning node never prunes above `tip - 2 * STRIPE_SPAN` (`docs/storage.md`,
/// "Bound": the newest 8 192 to 12 287 blocks stay whole); this keeps 512 blocks of slack for a
/// peer whose top moved on since its last timed sync. A wrong guess costs one re-queued batch,
/// never a ban.
pub const PRUNED_TIP_BLOCKS: u64 = 2 * STRIPE_SPAN - 512;

/// How often "no peer holds this stripe" is logged while it lasts.
const STRIPE_LOG_INTERVAL: Duration = Duration::from_mins(1);

/// Misses of ranges its seed claims after which a peer is dropped (no ban).
pub const CLAIMED_MISS_LIMIT: u32 = 2;

/// How long every connected peer that holds the next range may have answered it as missed
/// before the downloader starts over (the tracked branch is likely gone).
pub const STALL_RESET: Duration = Duration::from_secs(30);

/// Chain entries that added nothing although the peer announced more blocks, after which the
/// peer is dropped (no ban).
pub const CHAIN_STRIKE_LIMIT: u32 = 2;

/// Least time between two second chain requests for the same tracker.
const SECOND_OPINION_INTERVAL: Duration = Duration::from_secs(10);

/// After a chain entry with nothing new, wait this long before the next chain request.
const NOTHING_NEW_QUIET: Duration = Duration::from_secs(5);

/// Most pending misses kept at once (the oldest are forgotten first).
const MAX_PENDING_MISSES: usize = 1024;

/// Outbound peers per transport that delivered the most verified blocks and are never rotated
/// out for a missing stripe.
pub const ANCHOR_PEERS: usize = 2;

/// First wait between two stripe rotations for the same span; it doubles per rotation.
const ROTATION_BASE: Duration = Duration::from_secs(30);

/// Longest wait between two stripe rotations for the same span.
const ROTATION_MAX: Duration = Duration::from_mins(10);

/// A peer's seed from the raw `CORE_SYNC_DATA.pruning_seed` (0 for C++ nodes and full xkrd
/// nodes). An invalid value is read as [`PruningSeed::RECENT_ONLY`] (never as full): such a
/// peer is only asked for its recent blocks.
#[must_use]
pub fn peer_seed(raw: u32) -> PruningSeed {
    PruningSeed::decode(raw).unwrap_or(PruningSeed::RECENT_ONLY)
}

/// End (exclusive) of the [`STRIPE_SPAN`] that contains `height`: no batch crosses it
/// (saturates at `u64::MAX`).
#[must_use]
pub fn stripe_span_end(height: u64) -> u64 {
    (height / STRIPE_SPAN)
        .saturating_add(1)
        .saturating_mul(STRIPE_SPAN)
}

/// Least time between two stripe rotations for one span after `rotations` of them: 30 s,
/// 60 s, 120 s, ... up to 10 minutes.
#[must_use]
pub fn rotation_backoff(rotations: u32) -> Duration {
    ROTATION_BASE
        .saturating_mul(1_u32 << rotations.min(8))
        .min(ROTATION_MAX)
}

/// The highest height that at least two of `heights` reach (the second-highest value), the
/// only value for one, 0 for none: the network height as far as more than one peer says.
#[must_use]
pub fn network_height_estimate(heights: impl IntoIterator<Item = u64>) -> u64 {
    let (mut first, mut second) = (None, None);
    for h in heights {
        if first.is_none_or(|f| h > f) {
            second = first;
            first = Some(h);
        } else if second.is_none_or(|s| h > s) {
            second = Some(h);
        }
    }
    second.or(first).unwrap_or(0)
}

/// The seed of the stripe (for [`DEFAULT_STRIPES`]) that holds `height` (`/status`
/// `missing_stripe`, logs).
#[must_use]
pub fn default_stripe_for(height: u64) -> Option<PruningSeed> {
    PruningSeed::holding(DEFAULT_STRIPES, height)
}

/// The seeds that hold `height`, one per stripe count in [`LOOKUP_STRIPES`] (4 and 6): what a
/// node lacking that height looks for (`hyper::Session::find_stripe_peers`). Each count puts
/// `height` in its own stripe, so these are different stripes, not one stripe number.
#[must_use]
pub fn stripe_holders_for(height: u64) -> Vec<PruningSeed> {
    LOOKUP_STRIPES
        .iter()
        .filter_map(|&n| PruningSeed::holding(n, height))
        .collect()
}

/// Limits.
#[derive(Debug, Clone)]
pub struct DownloaderConfig {
    /// Blocks per `NOTIFY_REQUEST_GET_OBJECTS`.
    pub batch_blocks: usize,
    /// Do not request blocks more than this far above the next block to verify.
    pub max_ahead_blocks: u64,
    /// Byte budget of downloaded, not yet verified blocks.
    pub max_ready_bytes: usize,
    /// Jobs queued at the chain thread at once.
    pub max_queued_jobs: usize,
    /// Blocks per verification job.
    pub max_job_blocks: usize,
    /// Request another chain entry when fewer unrequested ids remain.
    pub low_water_ids: usize,
    /// A `NOTIFY_REQUEST_GET_OBJECTS` not answered within this is re-queued.
    pub request_timeout: Duration,
    /// A `NOTIFY_REQUEST_CHAIN` not answered within this is given up.
    pub chain_timeout: Duration,
    /// Most `NOTIFY_REQUEST_GET_OBJECTS` in flight at once. Below
    /// `xkr_p2p_legacy::constants::MAX_LARGE_READERS` (24), so every answer can be read with
    /// the solicited read limit.
    pub max_in_flight: usize,
}

impl Default for DownloaderConfig {
    fn default() -> Self {
        Self {
            batch_blocks: BLOCKS_SYNCHRONIZING_DEFAULT_COUNT,
            max_ahead_blocks: 20_000,
            max_ready_bytes: 96 * 1024 * 1024,
            max_queued_jobs: 2,
            max_job_blocks: 1_000,
            low_water_ids: 4_000,
            request_timeout: Duration::from_secs(60),
            chain_timeout: Duration::from_secs(30),
            max_in_flight: 16,
        }
    }
}

/// What the manager must do.
#[derive(Debug)]
pub enum DlAction {
    /// Send to a peer.
    Send(u64, ProtocolMessage),
    /// Drop a peer (and ban it if `true`).
    Drop(u64, &'static str, bool),
    /// Queue a verification job.
    Submit(ChainJob),
}

/// What the downloader needs to know about a peer.
#[derive(Debug, Clone, Copy)]
pub struct PeerSlot {
    /// Connection id.
    pub conn: u64,
    /// Peer's height (top index + 1).
    pub remote_height: u64,
    /// The peer told us it has blocks we lack (synchronizing state).
    pub syncing: bool,
    /// The peer may be asked for blocks (handshaken, not shutting down).
    pub usable: bool,
    /// The peer's pruning seed from its `CORE_SYNC_DATA` (full for C++ nodes and full xkrd
    /// nodes, [`PruningSeed::RECENT_ONLY`] for completely pruned and invalid seeds).
    pub pruning_seed: PruningSeed,
    /// We dialed the peer (it did not choose to connect to us).
    pub outbound: bool,
}

impl PeerSlot {
    /// Whether the peer holds every block of `[start, end)` in full: a full node always; a
    /// pruned one if the range is in its stripe or within its last [`PRUNED_TIP_BLOCKS`].
    /// (Whether its chain reaches `end` at all is the caller's `reach` check.)
    #[must_use]
    pub fn holds(&self, start: u64, end: u64) -> bool {
        self.claims(start, end) || start >= self.remote_height.saturating_sub(PRUNED_TIP_BLOCKS)
    }

    /// Whether the peer's seed itself says it keeps `[start, end)`: full, or the range is in
    /// its stripe. A miss of such a range is the peer's fault (`Downloader` peer records); the
    /// recent blocks of a pruned peer are only our assumption from its height.
    #[must_use]
    pub fn claims(&self, start: u64, end: u64) -> bool {
        self.pruning_seed.holds_range(start, end)
    }
}

/// What the downloader remembers about a connection across resets and rounds.
#[derive(Debug, Clone, Copy, Default)]
struct PeerRecord {
    /// Blocks from this peer the chain thread verified (and stored).
    delivered: u64,
    /// Answers that missed a range the peer's seed claims.
    claimed_misses: u32,
    /// Lowest first height of such a missed range.
    missed_at: Option<u64>,
    /// Chain entries that added nothing while the peer announced more ([`CHAIN_STRIKE_LIMIT`]).
    chain_strikes: u32,
}

/// Stripe rotations for one [`STRIPE_SPAN`]: at most one per [`ROTATION_BASE`] overall and,
/// for the same span, [`rotation_backoff`] after the previous one, until a peer delivers
/// blocks of that span.
#[derive(Debug, Clone, Copy, Default)]
struct RotationBudget {
    /// Span index (`height / STRIPE_SPAN`) the count belongs to.
    span: Option<u64>,
    /// Rotations for that span so far.
    rotations: u32,
    /// Last rotation (any span).
    last: Option<Instant>,
}

impl RotationBudget {
    fn allows(&self, range: Unserved, now: Instant) -> bool {
        let Some(last) = self.last else {
            return true;
        };
        let since = now.saturating_duration_since(last);
        if self.span == Some(range.start / STRIPE_SPAN) && self.rotations > 0 {
            since >= rotation_backoff(self.rotations - 1)
        } else {
            since >= ROTATION_BASE
        }
    }

    fn record(&mut self, range: Unserved, now: Instant) {
        let span = range.start / STRIPE_SPAN;
        if self.span != Some(span) {
            self.span = Some(span);
            self.rotations = 0;
        }
        self.rotations = self.rotations.saturating_add(1);
        self.last = Some(now);
    }

    /// Blocks `start..end` arrived: a budget for their span starts over.
    fn served(&mut self, start: u64, end: u64) {
        let first = start / STRIPE_SPAN;
        let last = end.saturating_sub(1) / STRIPE_SPAN;
        if self.span.is_some_and(|span| (first..=last).contains(&span)) {
            self.span = None;
            self.rotations = 0;
        }
    }
}

/// The lowest range still to be requested when no connected peer that reaches it holds it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Unserved {
    /// First height.
    pub start: u64,
    /// End (exclusive).
    pub end: u64,
}

#[derive(Debug)]
struct Tracker {
    /// Height of `ids[0]`.
    base: u64,
    ids: Vec<Hash>,
    /// First index not requested yet.
    next: usize,
    /// The last entry reached the peer's top.
    complete: bool,
    /// Peer that sent the entries.
    source: u64,
    /// Heights below this are confirmed: ours, verified, or the same in another peer's entry.
    confirmed_to: u64,
    /// A batch of this tracker verified.
    verified: bool,
    /// Peers asked for a second chain entry for this tracker.
    asked: HashSet<u64>,
    /// Last second chain request.
    last_opinion: Option<Instant>,
}

impl Tracker {
    fn remaining(&self) -> usize {
        self.ids.len() - self.next
    }
}

#[derive(Debug)]
struct InFlight {
    start: u64,
    ids: Vec<Hash>,
    sent: Instant,
    /// The peer's seed claims the range ([`PeerSlot::claims`]).
    claimed: bool,
}

/// A `NOTIFY_REQUEST_CHAIN` in flight.
#[derive(Debug, Clone, Copy)]
struct ChainRequest {
    conn: u64,
    sent: Instant,
    /// The height the peer announced when asked.
    claimed: u64,
    /// Asked to compare with the tracker, not to extend it.
    second_opinion: bool,
}

/// A miss of a claimed range by a peer other than the tracker's source, recorded against the
/// peer once the range verified.
#[derive(Debug, Clone, Copy)]
struct PendingMiss {
    conn: u64,
    start: u64,
    end: u64,
}

#[derive(Debug)]
struct Ready {
    blocks: Vec<RawBlock>,
    ids: Vec<Hash>,
    conn: u64,
    bytes: usize,
}

/// Counters for progress logs and `/status`.
#[derive(Debug, Clone, Copy, Default)]
pub struct DlStats {
    /// Blocks received.
    pub blocks_downloaded: u64,
    /// Bytes received (block + transaction blobs).
    pub bytes_downloaded: u64,
    /// Blocks added by the chain thread.
    pub blocks_added: u64,
}

/// The downloader state.
#[derive(Debug)]
pub struct Downloader {
    cfg: DownloaderConfig,
    generation: u64,
    /// A chain entry is needed (a peer announced blocks we lack).
    wanted: bool,
    /// A round (want -> chain entries -> blocks -> verified) is in progress.
    round_open: bool,
    /// After a peer answered "nothing new", do not ask again before this.
    quiet_until: Option<Instant>,
    tracker: Option<Tracker>,
    chain_request: Option<ChainRequest>,
    in_flight: HashMap<u64, InFlight>,
    requeue: BTreeMap<u64, (Vec<Hash>, u32)>,
    /// Peer -> lowest height it failed to serve (it will not be asked for blocks at or above).
    /// Cleared with each round and reset.
    lacking: HashMap<u64, u64>,
    /// Per-connection records that survive resets (removed when the peer closes).
    records: HashMap<u64, PeerRecord>,
    /// Misses waiting for their range to verify (cleared with the tracker).
    pending_misses: Vec<PendingMiss>,
    /// The next range nobody connected can serve (stripe pruning), see [`Self::missing_stripe`].
    unserved: Option<Unserved>,
    /// Since when every connected peer that holds the next range (first height) has answered
    /// it as missed ([`STALL_RESET`]).
    stalled: Option<(u64, Instant)>,
    /// Stripe rotations of the P2P manager ([`Self::may_rotate`]).
    rotation: RotationBudget,
    /// Last "no peer holds this stripe" log.
    last_stripe_log: Option<Instant>,
    ready: BTreeMap<u64, Ready>,
    ready_bytes: usize,
    next_submit: u64,
    queued: usize,
    /// Height announced in the accepted chain entry of the current tracker (0 without one).
    pub target_height: u64,
    /// Counters.
    pub stats: DlStats,
}

impl Downloader {
    /// An idle downloader.
    #[must_use]
    pub fn new(cfg: DownloaderConfig) -> Self {
        Self {
            cfg,
            generation: 0,
            wanted: false,
            round_open: false,
            quiet_until: None,
            tracker: None,
            chain_request: None,
            in_flight: HashMap::new(),
            requeue: BTreeMap::new(),
            lacking: HashMap::new(),
            records: HashMap::new(),
            pending_misses: Vec::new(),
            unserved: None,
            stalled: None,
            rotation: RotationBudget::default(),
            last_stripe_log: None,
            ready: BTreeMap::new(),
            ready_bytes: 0,
            next_submit: 0,
            queued: 0,
            target_height: 0,
            stats: DlStats::default(),
        }
    }

    /// A peer reported blocks we do not have.
    pub fn want_sync(&mut self) {
        self.wanted = true;
        self.round_open = true;
    }

    /// Whether the downloader is working (also while it only waits for a chain entry).
    #[must_use]
    pub fn active(&self) -> bool {
        self.wanted || !self.idle()
    }

    /// Whether blocks are being downloaded or verified: batches are buffered or at the chain
    /// thread, or requests are out for a tracker of which a batch already verified. A chain
    /// request alone, or requests for ids no block has verified yet, do not count.
    #[must_use]
    pub fn downloading(&self) -> bool {
        self.queued > 0
            || !self.ready.is_empty()
            || (self.tracker.as_ref().is_some_and(|t| t.verified) && !self.idle())
    }

    /// Nothing requested, buffered or being verified.
    #[must_use]
    pub fn idle(&self) -> bool {
        self.tracker.as_ref().is_none_or(|t| t.remaining() == 0)
            && self.chain_request.is_none()
            && self.in_flight.is_empty()
            && self.requeue.is_empty()
            && self.ready.is_empty()
            && self.queued == 0
    }

    /// The current round is over: every tracked block is verified and the last chain entry
    /// reached the peer's top (or there is no tracker). Clears the round.
    pub fn take_finished_round(&mut self) -> bool {
        if !self.round_open || !self.idle() || self.tracker.as_ref().is_some_and(|t| !t.complete) {
            return false;
        }
        if self.tracker.is_none() && self.wanted {
            return false;
        }
        self.tracker = None;
        self.pending_misses.clear();
        self.target_height = 0;
        self.lacking.clear();
        self.unserved = None;
        self.stalled = None;
        self.round_open = false;
        true
    }

    fn no_outstanding_work(&self) -> bool {
        self.in_flight.is_empty()
            && self.requeue.is_empty()
            && self.ready.is_empty()
            && self.queued == 0
    }

    /// Forgets everything but the peer records and the rotation budget (after an invalid
    /// block, a chain switch or a stalled branch).
    pub fn reset(&mut self) {
        self.generation += 1;
        self.tracker = None;
        self.pending_misses.clear();
        self.target_height = 0;
        self.chain_request = None;
        self.in_flight.clear();
        self.requeue.clear();
        self.lacking.clear();
        self.unserved = None;
        self.stalled = None;
        self.ready.clear();
        self.ready_bytes = 0;
        self.queued = 0;
        self.wanted = true;
        self.round_open = true;
    }

    /// Whether a `NOTIFY_REQUEST_CHAIN` should be sent now: for the tracker, or for a second
    /// chain entry to compare with it ([`Self::second_opinion_due`]).
    #[must_use]
    pub fn needs_chain(&self) -> bool {
        if self.chain_request.is_some() {
            return false;
        }
        self.tracker_needs_chain() || self.second_opinion_due(Instant::now())
    }

    fn tracker_needs_chain(&self) -> bool {
        match &self.tracker {
            None => self.wanted && self.quiet_until.is_none_or(|t| Instant::now() >= t),
            Some(t) => !t.complete && t.remaining() < self.cfg.low_water_ids,
        }
    }

    /// A second chain entry is wanted: the next range has no holder that can be asked
    /// ([`Self::unserved`]), it is not confirmed yet, and the last second request for this
    /// tracker is at least [`SECOND_OPINION_INTERVAL`] old.
    fn second_opinion_due(&self, now: Instant) -> bool {
        let (Some(t), Some(range)) = (&self.tracker, self.unserved) else {
            return false;
        };
        range.end > t.confirmed_to
            && t.last_opinion
                .is_none_or(|at| now.saturating_duration_since(at) >= SECOND_OPINION_INTERVAL)
    }

    fn chain_strikes(&self, conn: u64) -> u32 {
        self.records.get(&conn).map_or(0, |r| r.chain_strikes)
    }

    /// Picks a peer and builds the `NOTIFY_REQUEST_CHAIN` (`sparse` is our sparse chain).
    ///
    /// For the tracker: among usable peers that are syncing, ahead of `our_height` or the
    /// tracker's source, the one with the fewest chain strikes, then the tracker's source, then
    /// the most verified blocks delivered, then the highest announced height counted up to
    /// [`network_height_estimate`] of all usable peers, then the oldest connection. For a second
    /// entry ([`Self::second_opinion_due`]): the same order among usable peers other than the
    /// source not asked yet for this tracker, with our sparse chain only.
    pub fn request_chain(
        &mut self,
        peers: &[PeerSlot],
        our_height: u64,
        sparse: Vec<Hash>,
        out: &mut Vec<DlAction>,
    ) {
        if self.chain_request.is_some() {
            return;
        }
        let now = Instant::now();
        let second_opinion = !self.tracker_needs_chain() && self.second_opinion_due(now);
        let usable_heights = peers.iter().filter(|p| p.usable).map(|p| p.remote_height);
        let cap = network_height_estimate(usable_heights).max(our_height + 1);
        let source = self.tracker.as_ref().map(|t| t.source);
        let asked = self.tracker.as_ref().map(|t| &t.asked);
        let rank = |p: &&PeerSlot| {
            (
                Reverse(self.chain_strikes(p.conn)),
                !second_opinion && Some(p.conn) == source,
                self.delivered(p.conn),
                p.remote_height.min(cap),
                Reverse(p.conn),
            )
        };
        let pick = peers
            .iter()
            .filter(|p| p.usable)
            .filter(|p| {
                if second_opinion {
                    Some(p.conn) != source && asked.is_none_or(|a| !a.contains(&p.conn))
                } else {
                    Some(p.conn) == source || p.syncing || p.remote_height > our_height
                }
            })
            .max_by_key(rank)
            .copied();
        if second_opinion && let Some(t) = &mut self.tracker {
            t.last_opinion = Some(now);
        }
        let Some(peer) = pick else {
            return;
        };
        let mut block_ids = Vec::with_capacity(sparse.len() + 1);
        if second_opinion {
            if let Some(t) = &mut self.tracker {
                t.asked.insert(peer.conn);
            }
        } else if let Some(t) = &self.tracker
            && let Some(last) = t.ids.last()
        {
            block_ids.push(*last);
        }
        block_ids.extend(sparse);
        self.chain_request = Some(ChainRequest {
            conn: peer.conn,
            sent: now,
            claimed: peer.remote_height,
            second_opinion,
        });
        out.push(DlAction::Send(
            peer.conn,
            ProtocolMessage::RequestChain(RequestChain { block_ids }),
        ));
    }

    /// One chain strike for `conn`; at [`CHAIN_STRIKE_LIMIT`] the peer is dropped (no ban).
    fn chain_strike(&mut self, conn: u64, out: &mut Vec<DlAction>) {
        let rec = self.records.entry(conn).or_default();
        rec.chain_strikes = rec.chain_strikes.saturating_add(1);
        tracing::debug!(
            conn,
            strikes = rec.chain_strikes,
            "chain entry added no blocks the peer announced"
        );
        if rec.chain_strikes >= CHAIN_STRIKE_LIMIT {
            out.push(DlAction::Drop(
                conn,
                "chain entries add no blocks the peer announced",
                false,
            ));
        }
    }

    /// `NOTIFY_RESPONSE_CHAIN_ENTRY`. `known` answers whether we have a block.
    /// `Err` = protocol violation; peers to drop without a ban go to `out`.
    ///
    /// An entry with nothing we lack gets the peer a chain strike when the peer announced a
    /// higher chain than the entry shows, or when the entry is incomplete (also a continuation
    /// without new ids). `target_height` follows the accepted entries of the tracker.
    pub fn on_chain_entry(
        &mut self,
        conn: u64,
        entry: ResponseChainEntry,
        known: impl Fn(&Hash) -> bool,
        out: &mut Vec<DlAction>,
    ) -> Result<(), &'static str> {
        let Some(req) = self.chain_request.filter(|r| r.conn == conn) else {
            // Unsolicited (or answered after a timeout): harmless, ignore.
            return Ok(());
        };
        self.chain_request = None;
        let len = entry.block_ids.len() as u64;
        let Some(first) = entry.block_ids.first().copied() else {
            return Err("NOTIFY_RESPONSE_CHAIN_ENTRY: empty m_block_ids");
        };
        // Nodes answer with at most BLOCKS_IDS_SYNCHRONIZING_DEFAULT_COUNT ids (C++
        // `findBlockchainSupplement`, and this node).
        if entry.block_ids.len() > BLOCKS_IDS_SYNCHRONIZING_DEFAULT_COUNT {
            return Err("NOTIFY_RESPONSE_CHAIN_ENTRY: more block ids than a chain entry holds");
        }
        let start = u64::from(entry.start_height);
        let total = u64::from(entry.total_height);
        if start + len > total {
            return Err("NOTIFY_RESPONSE_CHAIN_ENTRY: start + count > total");
        }
        let complete = start + len >= total;

        if req.second_opinion {
            if !known(&first) {
                return Err("NOTIFY_RESPONSE_CHAIN_ENTRY: starts from an unknown block");
            }
            self.compare_entry(start, &entry.block_ids);
            return Ok(());
        }

        // Continuation of the tracker?
        if let Some(t) = &mut self.tracker
            && t.ids.last() == Some(&first)
            && t.base + t.ids.len() as u64 - 1 == start
        {
            if t.source != conn {
                // Another peer knows the tracker's last id.
                t.confirmed_to = t.confirmed_to.max(start + 1);
            }
            t.ids.extend_from_slice(&entry.block_ids[1..]);
            t.complete = complete;
            t.source = conn;
            if len == 1 && !complete {
                self.chain_strike(conn, out);
            } else {
                self.target_height = total;
            }
            return Ok(());
        }
        if !known(&first) {
            return Err("NOTIFY_RESPONSE_CHAIN_ENTRY: starts from an unknown block");
        }
        if self.tracker.is_some() && !self.no_outstanding_work() {
            // A different branch while batches are outstanding: finish those first.
            return Ok(());
        }
        let k = entry
            .block_ids
            .iter()
            .position(|h| !known(h))
            .unwrap_or(entry.block_ids.len());
        if k == entry.block_ids.len() {
            // The peer has nothing we lack.
            if !complete || req.claimed > total {
                self.chain_strike(conn, out);
            }
            self.tracker = None;
            self.pending_misses.clear();
            self.target_height = 0;
            self.wanted = false;
            self.quiet_until = Some(Instant::now() + NOTHING_NEW_QUIET);
            return Ok(());
        }
        let next = k.max(1).min(entry.block_ids.len());
        self.tracker = Some(Tracker {
            base: start,
            ids: entry.block_ids,
            next,
            complete,
            source: conn,
            confirmed_to: start + k as u64,
            verified: false,
            asked: HashSet::new(),
            last_opinion: None,
        });
        self.pending_misses.clear();
        self.target_height = total;
        self.wanted = false;
        self.round_open = true;
        if self.no_outstanding_work() {
            self.next_submit = start + next as u64;
        }
        Ok(())
    }

    /// A second chain entry (`ids` from height `start`): where it has the same ids as the
    /// tracker from the first height both cover on, without a gap above the confirmed heights,
    /// those heights count as confirmed.
    fn compare_entry(&mut self, start: u64, ids: &[Hash]) {
        let Some(t) = &mut self.tracker else {
            return;
        };
        let lo = start.max(t.base);
        let hi = (start + ids.len() as u64).min(t.base + t.ids.len() as u64);
        if lo >= hi || lo > t.confirmed_to {
            return;
        }
        let same = (lo..hi)
            .take_while(|h| ids[(h - start) as usize] == t.ids[(h - t.base) as usize])
            .count() as u64;
        t.confirmed_to = t.confirmed_to.max(lo + same);
    }

    /// The range [`Self::unserved`] names, if the tracker's ids for it are confirmed: the range
    /// a stripe rotation may make room for.
    #[must_use]
    pub fn rotation_range(&self) -> Option<Unserved> {
        let t = self.tracker.as_ref()?;
        self.unserved.filter(|r| r.end <= t.confirmed_to)
    }

    /// Records a miss of a claimed range against `conn`; at [`CLAIMED_MISS_LIMIT`] the peer is
    /// dropped (no ban).
    fn record_claimed_miss(&mut self, conn: u64, start: u64, out: &mut Vec<DlAction>) {
        let Some(rec) = self.records.get_mut(&conn) else {
            return;
        };
        rec.claimed_misses = rec.claimed_misses.saturating_add(1);
        rec.missed_at = Some(rec.missed_at.map_or(start, |m| m.min(start)));
        tracing::debug!(
            conn,
            start,
            misses = rec.claimed_misses,
            "peer missed blocks its pruning seed claims"
        );
        if rec.claimed_misses >= CLAIMED_MISS_LIMIT {
            out.push(DlAction::Drop(
                conn,
                "answered blocks its pruning seed claims as missed",
                false,
            ));
        }
    }

    /// `NOTIFY_RESPONSE_GET_OBJECTS`. Never a protocol violation: an incomplete answer
    /// requeues the range for another peer (no branch failure). If the peer's seed claims the
    /// range, the miss is recorded against the peer: at once if the tracker follows this peer's
    /// chain entry, otherwise once the range verified. After [`CLAIMED_MISS_LIMIT`] of them it
    /// is dropped without a ban (`out`).
    pub fn on_objects(&mut self, conn: u64, resp: ResponseGetObjects, out: &mut Vec<DlAction>) {
        let Some(f) = self.in_flight.remove(&conn) else {
            // Stale (reset) or answered after a timeout.
            return;
        };
        if resp.blocks.len() != f.ids.len() || !resp.missed_ids.is_empty() {
            // The peer does not have them (another branch, behind or pruned): ask someone else.
            self.lacking.insert(conn, f.start);
            if f.claimed {
                let end = f.start + f.ids.len() as u64;
                if self.tracker.as_ref().is_some_and(|t| t.source == conn) {
                    self.record_claimed_miss(conn, f.start, out);
                } else {
                    if self.pending_misses.len() >= MAX_PENDING_MISSES {
                        self.pending_misses.remove(0);
                    }
                    self.pending_misses.push(PendingMiss {
                        conn,
                        start: f.start,
                        end,
                    });
                }
            }
            self.requeue_unfailed(f.start, f.ids);
            return;
        }
        let blocks: Vec<RawBlock> = resp.blocks.into_iter().map(RawBlock::from).collect();
        let bytes: usize = blocks
            .iter()
            .map(|b| b.block.len() + b.transactions.iter().map(Vec::len).sum::<usize>())
            .sum();
        self.stats.blocks_downloaded += blocks.len() as u64;
        self.stats.bytes_downloaded += bytes as u64;
        if f.start < self.next_submit {
            // Already submitted from a duplicate request.
            return;
        }
        self.ready_bytes += bytes;
        self.ready.insert(
            f.start,
            Ready {
                blocks,
                ids: f.ids,
                conn,
                bytes,
            },
        );
    }

    /// Re-queues a batch whose request timed out or lost its peer: one branch failure.
    fn push_requeue(&mut self, start: u64, ids: Vec<Hash>) {
        let failures = self.requeue.get(&start).map_or(0, |r| r.1) + 1;
        self.requeue.insert(start, (ids, failures));
    }

    /// Re-queues a batch a peer answered as missed: no branch failure (a peer cannot push the
    /// downloader into a reset by claiming blocks it does not send).
    fn requeue_unfailed(&mut self, start: u64, ids: Vec<Hash>) {
        let failures = self.requeue.get(&start).map_or(0, |r| r.1);
        self.requeue.insert(start, (ids, failures));
    }

    /// Verified blocks `conn` delivered (this connection).
    fn delivered(&self, conn: u64) -> u64 {
        self.records.get(&conn).map_or(0, |r| r.delivered)
    }

    /// Whether `peer` answered a range at or below `range` as missed: this round (`lacking`)
    /// or, for a range its seed claimed, any time on this connection (recorded misses).
    fn missed(&self, peer: &PeerSlot, range: Unserved) -> bool {
        self.lacking.get(&peer.conn).is_some_and(|l| *l < range.end)
            || self.recorded_miss(peer, range)
    }

    /// Whether a recorded miss of `peer` ([`PeerRecord::missed_at`]) lies below `range.end`.
    fn recorded_miss(&self, peer: &PeerSlot, range: Unserved) -> bool {
        self.records
            .get(&peer.conn)
            .and_then(|r| r.missed_at)
            .is_some_and(|m| m < range.end)
    }

    /// Heights `< reach` that `peer` may be asked for.
    fn reach(&self, peer: &PeerSlot) -> u64 {
        self.lacking
            .get(&peer.conn)
            .map_or(peer.remote_height, |l| (*l).min(peer.remote_height))
    }

    /// Issues `NOTIFY_REQUEST_GET_OBJECTS` to idle peers. Peers that delivered the most
    /// verified blocks go first, then full peers (they can take any batch), then by height;
    /// pruned peers only get batches they hold in full. What a peer claims buys no priority
    /// before it delivered.
    pub fn fill(&mut self, peers: &[PeerSlot], out: &mut Vec<DlAction>) {
        let mut idle: Vec<PeerSlot> = peers
            .iter()
            .copied()
            .filter(|p| p.usable && !self.in_flight.contains_key(&p.conn))
            .collect();
        idle.sort_by_key(|p| {
            (
                Reverse(self.delivered(p.conn)),
                !p.pruning_seed.is_full(),
                Reverse(p.remote_height),
            )
        });
        for peer in idle {
            if self.in_flight.len() >= self.cfg.max_in_flight {
                break;
            }
            let reach = self.reach(&peer);
            let Some((start, ids)) = self.next_batch(&peer, reach) else {
                continue;
            };
            out.push(DlAction::Send(
                peer.conn,
                ProtocolMessage::RequestGetObjects(RequestGetObjects {
                    txs: Vec::new(),
                    blocks: ids.clone(),
                }),
            ));
            let claimed = peer.claims(start, start + ids.len() as u64);
            self.records.entry(peer.conn).or_default();
            self.in_flight.insert(
                peer.conn,
                InFlight {
                    start,
                    ids,
                    sent: Instant::now(),
                    claimed,
                },
            );
        }
        self.check_unserved(peers, Instant::now());
    }

    /// The next batch `peer` (heights `< reach`) can serve in full.
    ///
    /// Re-queued batches come first, oldest height first. Then the tracker: a batch ends at the
    /// next [`STRIPE_SPAN`] boundary, so it belongs to one stripe. If `peer` is pruned and does
    /// not hold it, the batch is set aside in the re-queue (no failure counted) for a peer that
    /// does, and the next one is tried, as long as the download window allows; so a pruned
    /// peer can work on a later range of its stripe while full peers fill the gap.
    fn next_batch(&mut self, peer: &PeerSlot, reach: u64) -> Option<(u64, Vec<Hash>)> {
        let requeued = self
            .requeue
            .iter()
            .find(|(s, (ids, _))| {
                let end = **s + ids.len() as u64;
                end <= reach && peer.holds(**s, end) && self.in_window(**s)
            })
            .map(|(s, _)| *s);
        if let Some(start) = requeued {
            let (ids, _) = self.requeue.remove(&start)?;
            return Some((start, ids));
        }
        loop {
            let (start, ids) = self.take_tracker_batch(reach)?;
            let end = start + ids.len() as u64;
            if peer.holds(start, end) {
                return Some((start, ids));
            }
            self.requeue.insert(start, (ids, 0));
        }
    }

    /// Takes the next tracker batch for a peer with `reach`: at most `batch_blocks`, never
    /// across a [`STRIPE_SPAN`] boundary, inside the download window (the batch at the head of
    /// the queue is always allowed).
    fn take_tracker_batch(&mut self, reach: u64) -> Option<(u64, Vec<Hash>)> {
        let start = {
            let t = self.tracker.as_ref()?;
            if t.remaining() == 0 {
                return None;
            }
            t.base + t.next as u64
        };
        if !self.in_window(start) {
            return None;
        }
        let t = self.tracker.as_mut()?;
        let limit = reach.min(stripe_span_end(start)).saturating_sub(start);
        let n = (self.cfg.batch_blocks.min(t.remaining()) as u64).min(limit) as usize;
        if n == 0 {
            return None;
        }
        let ids = t.ids[t.next..t.next + n].to_vec();
        t.next += n;
        // Requested ids live on in their batches; keep the tracker small during a long sync
        // (the last id stays for continuation requests).
        if t.next > 20_000 {
            let d = t.next - 1;
            t.ids.drain(..d);
            t.base += d as u64;
            t.next -= d;
        }
        Some((start, ids))
    }

    /// The download window, for re-queued and tracker batches alike: a batch from `start` may
    /// be requested while it starts less than `max_ahead_blocks` above the next block to
    /// submit and the downloaded blocks stay below `max_ready_bytes`. The batch at the head of
    /// the queue (or below it) always may, so the download never waits on itself.
    fn in_window(&self, start: u64) -> bool {
        start <= self.next_submit
            || (start - self.next_submit < self.cfg.max_ahead_blocks
                && self.ready_bytes < self.cfg.max_ready_bytes)
    }

    /// The lowest range not requested yet: the first re-queued batch, else the tracker head.
    fn next_needed(&self) -> Option<Unserved> {
        if let Some((start, (ids, _))) = self.requeue.iter().next() {
            return Some(Unserved {
                start: *start,
                end: *start + ids.len() as u64,
            });
        }
        let t = self.tracker.as_ref().filter(|t| t.remaining() > 0)?;
        let start = t.base + t.next as u64;
        let n = self.cfg.batch_blocks.min(t.remaining()) as u64;
        Some(Unserved {
            start,
            end: (start + n).min(stripe_span_end(start)),
        })
    }

    /// Records (and logs, at most every [`STRIPE_LOG_INTERVAL`]) when connected peers reach the
    /// next needed range but none of them that holds it in full can be asked for it: only
    /// pruned peers without that stripe are there, or every holder answered it as missed this
    /// round (then the range is also looked for, and after [`STALL_RESET`] the download starts
    /// over). Peers that do not reach it at all are the normal "nobody has it yet" case, not a
    /// stripe problem.
    fn check_unserved(&mut self, peers: &[PeerSlot], now: Instant) {
        let next = self.next_needed();
        let mut stalled = false;
        let unserved = next.filter(|r| {
            let reaching: Vec<&PeerSlot> = peers
                .iter()
                .filter(|p| p.usable && p.remote_height >= r.end)
                .collect();
            let holders: Vec<&&PeerSlot> = reaching
                .iter()
                .filter(|p| p.holds(r.start, r.end))
                .collect();
            if holders.iter().any(|p| self.reach(p) >= r.end) {
                return false;
            }
            stalled = !holders.is_empty();
            !reaching.is_empty()
        });
        self.stalled = match (unserved, stalled) {
            (Some(r), true) => Some(
                self.stalled
                    .filter(|(start, _)| *start == r.start)
                    .unwrap_or((r.start, now)),
            ),
            _ => None,
        };
        if let Some(r) = unserved
            && self
                .last_stripe_log
                .is_none_or(|t| now.duration_since(t) >= STRIPE_LOG_INTERVAL)
        {
            self.last_stripe_log = Some(now);
            let stripe = default_stripe_for(r.start).map(PruningSeed::stripe);
            tracing::warn!(
                start = r.start,
                end = r.end,
                stripe = ?stripe,
                stripes = DEFAULT_STRIPES,
                "no connected peer holds these blocks in full (only pruned peers of other \
                 stripes, or peers that answered them as missed); looking for a full node or a \
                 peer of this stripe"
            );
        }
        self.unserved = unserved;
    }

    /// The next range no connected peer can serve because of stripe pruning, if any.
    #[must_use]
    pub fn unserved(&self) -> Option<Unserved> {
        self.unserved
    }

    /// The stripe (for [`DEFAULT_STRIPES`]) the downloader waits for, if any (`/status`). The
    /// P2P manager works from [`Self::unserved`]: it prefers peers holding that range when it
    /// dials, rotates peers without it out, and the Hyperswarm transport looks up the topics of
    /// every [`stripe_holders_for`] seed.
    #[must_use]
    pub fn missing_stripe(&self) -> Option<PruningSeed> {
        self.unserved.and_then(|r| default_stripe_for(r.start))
    }

    /// Hands consecutive downloaded batches to the chain thread.
    pub fn submit(&mut self, out: &mut Vec<DlAction>) {
        while self.queued < self.cfg.max_queued_jobs {
            let Some(first) = self.ready.remove(&self.next_submit) else {
                break;
            };
            let start = self.next_submit;
            let mut end = start + first.blocks.len() as u64;
            let mut sources = vec![(start, first.conn)];
            let mut blocks = first.blocks;
            let mut expected = first.ids;
            let mut bytes = first.bytes;
            while blocks.len() < self.cfg.max_job_blocks {
                let Some(r) = self.ready.remove(&end) else {
                    break;
                };
                sources.push((end, r.conn));
                end += r.blocks.len() as u64;
                bytes += r.bytes;
                blocks.extend(r.blocks);
                expected.extend(r.ids);
            }
            self.ready_bytes = self.ready_bytes.saturating_sub(bytes);
            self.next_submit = end;
            self.queued += 1;
            out.push(DlAction::Submit(ChainJob::Batch {
                generation: self.generation,
                start,
                blocks,
                expected,
                sources,
            }));
        }
    }

    /// A job finished. Returns the peer to drop/ban if a block was bad. Credits every source
    /// with the blocks of its batches that were verified.
    pub fn on_batch_done(
        &mut self,
        generation: u64,
        start: u64,
        sources: &[(u64, u64)],
        report: &BatchReport,
        out: &mut Vec<DlAction>,
    ) {
        if generation != self.generation {
            return;
        }
        let verified = report.error.as_ref().map_or(report.submitted, |e| e.index);
        let verified_end = start + verified as u64;
        self.credit(sources, verified_end);
        if verified > 0 {
            // A span counts as served once blocks of it verified.
            self.rotation.served(start, verified_end);
            if let Some(t) = &mut self.tracker {
                t.verified = true;
                t.confirmed_to = t.confirmed_to.max(verified_end);
            }
            self.confirm_pending_misses(verified_end, out);
        }
        self.queued = self.queued.saturating_sub(1);
        self.stats.blocks_added += (report.added_main + report.added_alt) as u64;
        if let Some(err) = &report.error {
            let height = start + err.index as u64;
            let culprit = sources
                .iter()
                .rev()
                .find(|(h, _)| *h <= height)
                .map(|(_, c)| *c);
            tracing::warn!(height, reason = %err.reason, kind = ?err.kind, peer = ?culprit, "downloaded block rejected");
            if let Some(conn) = culprit {
                let ban = err.kind == BatchErrorKind::Invalid;
                out.push(DlAction::Drop(conn, "sent an invalid block", ban));
            }
            self.reset();
        } else if report.switched {
            // The main chain changed under the tracker: start over from the new tip.
            self.reset();
        }
    }

    /// Adds the verified blocks below `verified_end` to the records of their sources (`(first
    /// height, conn)` in height order; each source's blocks end where the next one starts).
    fn credit(&mut self, sources: &[(u64, u64)], verified_end: u64) {
        for (i, (first, conn)) in sources.iter().enumerate() {
            let end = sources
                .get(i + 1)
                .map_or(verified_end, |next| next.0.min(verified_end));
            let blocks = end.saturating_sub(*first);
            if blocks > 0
                && let Some(rec) = self.records.get_mut(conn)
            {
                rec.delivered = rec.delivered.saturating_add(blocks);
            }
        }
    }

    /// Records the pending misses whose range lies below `verified_end` (those blocks verified).
    fn confirm_pending_misses(&mut self, verified_end: u64, out: &mut Vec<DlAction>) {
        let (due, keep): (Vec<PendingMiss>, Vec<PendingMiss>) =
            std::mem::take(&mut self.pending_misses)
                .into_iter()
                .partition(|m| m.end <= verified_end);
        self.pending_misses = keep;
        for m in due {
            self.record_claimed_miss(m.conn, m.start, out);
        }
    }

    /// A job failed for a node-side reason (storage error): start over.
    pub fn on_batch_failed(&mut self, generation: u64) {
        if generation == self.generation {
            self.reset();
        }
    }

    /// A peer disconnected.
    pub fn on_peer_closed(&mut self, conn: u64) {
        if let Some(f) = self.in_flight.remove(&conn) {
            self.push_requeue(f.start, f.ids);
        }
        if self.chain_request.is_some_and(|c| c.conn == conn) {
            self.chain_request = None;
        }
        self.lacking.remove(&conn);
        self.records.remove(&conn);
        self.pending_misses.retain(|m| m.conn != conn);
    }

    /// Re-queues timed-out requests; returns the peers to drop without a ban and why: those
    /// that timed out, and a tracker source that reached [`CHAIN_STRIKE_LIMIT`] when the
    /// download started over at an unconfirmed range.
    pub fn check_timeouts(&mut self, now: Instant) -> Vec<(u64, &'static str)> {
        let mut slow = Vec::new();
        let expired: Vec<u64> = self
            .in_flight
            .iter()
            .filter(|(_, f)| now.duration_since(f.sent) > self.cfg.request_timeout)
            .map(|(c, _)| *c)
            .collect();
        for conn in expired {
            if let Some(f) = self.in_flight.remove(&conn) {
                self.push_requeue(f.start, f.ids);
            }
            slow.push((conn, "request timed out"));
        }
        if let Some(req) = self.chain_request
            && now.duration_since(req.sent) > self.cfg.chain_timeout
        {
            self.chain_request = None;
            slow.push((req.conn, "request timed out"));
        }
        // A batch that timed out or lost its peer three times, or a range every holder
        // answered as missed for a while: the tracker is on a branch our peers do not have;
        // start over. (Misses alone never count as failures: claiming blocks and not sending
        // them must not push the downloader into a reset loop.)
        let stalled = self
            .stalled
            .filter(|(_, since)| now.saturating_duration_since(*since) >= STALL_RESET);
        if stalled.is_some() || self.requeue.values().any(|(_, failures)| *failures >= 3) {
            tracing::info!("no peer serves the tracked branch, restarting the download");
            // The tracker's source gets a chain strike when the range was not confirmed.
            let unconfirmed_source = stalled.and_then(|(start, _)| {
                self.tracker
                    .as_ref()
                    .filter(|t| start >= t.confirmed_to)
                    .map(|t| t.source)
            });
            self.reset();
            if let Some(source) = unconfirmed_source {
                let mut out = Vec::new();
                self.chain_strike(source, &mut out);
                for a in out {
                    if let DlAction::Drop(conn, reason, _) = a {
                        slow.push((conn, reason));
                    }
                }
            }
        }
        slow
    }

    /// Whether a stripe rotation for `range` is allowed now: at most one per 30 s overall and,
    /// for the same span, [`rotation_backoff`] after the previous one (30 s, 60 s, ... 10 min)
    /// until a peer delivers blocks of that span.
    #[must_use]
    pub fn may_rotate(&self, range: Unserved, now: Instant) -> bool {
        self.rotation.allows(range, now)
    }

    /// A peer was dropped to make room for a holder of `range`.
    pub fn rotated(&mut self, range: Unserved, now: Instant) {
        self.rotation.record(range, now);
    }

    /// Stripe rotation: the connected peer to drop (no ban) so that a holder of `range` can take
    /// its slot, when no connected peer serves it ([`Self::unserved`]) and the slots are full.
    /// Only peers `may_drop` allows (the caller's rule: outbound, not explicit, of the
    /// transport whose slots are full) are candidates.
    ///
    /// 1. A peer that holds `range` by its seed or its recent blocks and has a recorded miss
    ///    of it (or a lower range) goes first, whatever its seed: the fewest delivered blocks,
    ///    then the newest connection. Misses still pending (range not verified) do not count
    ///    here.
    /// 2. Otherwise only a stripe-pruned (or recent-only) peer that does not hold `range` and
    ///    could be asked for it, and only while another usable peer that has not missed it
    ///    reaches `range.end` (the last one is kept: it is where the chain entries come from).
    ///    The [`ANCHOR_PEERS`] candidates that delivered the most verified blocks are never
    ///    dropped. Least useful first: a seed another *outbound* peer has too (inbound peers
    ///    choose their own seed, so theirs do not count here), then the fewest delivered
    ///    blocks, the lowest chain and the newest connection.
    #[must_use]
    pub fn rotation_victim(
        &self,
        peers: &[PeerSlot],
        range: Unserved,
        may_drop: impl Fn(&PeerSlot) -> bool,
    ) -> Option<u64> {
        let candidates: Vec<&PeerSlot> = peers.iter().filter(|p| may_drop(p)).collect();
        let missed_holder = candidates
            .iter()
            .filter(|p| p.holds(range.start, range.end) && self.recorded_miss(p, range))
            .min_by_key(|p| (self.delivered(p.conn), Reverse(p.conn)));
        if let Some(p) = missed_holder {
            return Some(p.conn);
        }
        let reaches =
            |p: &PeerSlot| p.usable && self.reach(p) >= range.end && !self.missed(p, range);
        if peers.iter().filter(|p| reaches(p)).count() < 2 {
            return None;
        }
        let mut productive: Vec<&&PeerSlot> = candidates
            .iter()
            .filter(|p| self.delivered(p.conn) > 0 && !self.missed(p, range))
            .collect();
        productive.sort_by_key(|p| (Reverse(self.delivered(p.conn)), p.conn));
        let anchors: HashSet<u64> = productive
            .iter()
            .take(ANCHOR_PEERS)
            .map(|p| p.conn)
            .collect();
        candidates
            .into_iter()
            .filter(|p| {
                !anchors.contains(&p.conn)
                    && !p.pruning_seed.is_full()
                    && !p.holds(range.start, range.end)
                    && reaches(p)
            })
            .min_by_key(|p| {
                let shared = peers
                    .iter()
                    .any(|q| q.outbound && q.conn != p.conn && q.pruning_seed == p.pruning_seed);
                (
                    !shared,
                    self.delivered(p.conn),
                    p.remote_height,
                    Reverse(p.conn),
                )
            })
            .map(|p| p.conn)
    }

    /// Requests in flight.
    #[must_use]
    pub fn in_flight(&self) -> usize {
        self.in_flight.len()
    }

    /// Bytes buffered for verification.
    #[must_use]
    pub fn ready_bytes(&self) -> usize {
        self.ready_bytes
    }
}

#[cfg(test)]
mod tests {
    use xkr_p2p_legacy::wire::RawBlockLegacy;

    use super::*;

    fn hid(i: u64) -> Hash {
        let mut b = [0u8; 32];
        b[..8].copy_from_slice(&i.to_le_bytes());
        b[31] = 1;
        Hash(b)
    }

    fn peer(conn: u64, height: u64) -> PeerSlot {
        PeerSlot {
            conn,
            remote_height: height,
            syncing: true,
            usable: true,
            pruning_seed: PruningSeed::FULL,
            outbound: true,
        }
    }

    fn striped(conn: u64, height: u64, pruning_seed: PruningSeed) -> PeerSlot {
        PeerSlot {
            pruning_seed,
            ..peer(conn, height)
        }
    }

    /// Height encoded by [`hid`].
    fn height_of(h: &Hash) -> u64 {
        u64::from_le_bytes(h.0[..8].try_into().unwrap())
    }

    /// The default-stripe seed holding `height`, and one that does not.
    fn seed_for(height: u64) -> PruningSeed {
        default_stripe_for(height).unwrap()
    }

    fn other_seed(height: u64) -> PruningSeed {
        let s = seed_for(height);
        PruningSeed::new(s.stripes(), s.stripe() % s.stripes() + 1).unwrap()
    }

    /// `(conn, first height, block count)` of every `NOTIFY_REQUEST_GET_OBJECTS` in `out`.
    fn get_objects(out: &[DlAction]) -> Vec<(u64, u64, u64)> {
        out.iter()
            .filter_map(|a| match a {
                DlAction::Send(c, ProtocolMessage::RequestGetObjects(r)) => {
                    let start = height_of(&r.blocks[0]);
                    for (i, h) in r.blocks.iter().enumerate() {
                        assert_eq!(height_of(h), start + i as u64, "a batch is consecutive");
                    }
                    Some((*c, start, r.blocks.len() as u64))
                }
                _ => None,
            })
            .collect()
    }

    /// A downloader tracking `first..end` (we have every block below `first`) announced by a
    /// peer at `total`.
    fn tracking(first: u64, end: u64, total: u64) -> Downloader {
        let mut d = Downloader::new(DownloaderConfig::default());
        d.want_sync();
        let mut out = Vec::new();
        d.request_chain(&[peer(1, total)], first, vec![hid(first - 1)], &mut out);
        let entry = ResponseChainEntry {
            start_height: u32::try_from(first - 1).unwrap(),
            total_height: u32::try_from(total).unwrap(),
            block_ids: (first - 1..end).map(hid).collect(),
        };
        d.on_chain_entry(1, entry, |h| height_of(h) < first, &mut Vec::new())
            .unwrap();
        d
    }

    /// Batches end at stripe boundaries, so each one belongs to exactly one stripe.
    #[test]
    fn batches_never_cross_a_stripe_span() {
        let s = STRIPE_SPAN;
        let mut d = tracking(s - 150, s + 150, s + 150);
        let peers: Vec<PeerSlot> = (1..=6).map(|c| peer(c, s + 150)).collect();
        let mut out = Vec::new();
        d.fill(&peers, &mut out);
        let mut reqs = get_objects(&out);
        reqs.sort_by_key(|r| r.1);
        let ranges: Vec<(u64, u64)> = reqs.iter().map(|r| (r.1, r.2)).collect();
        assert_eq!(
            ranges,
            vec![(s - 150, 100), (s - 50, 50), (s, 100), (s + 100, 50)]
        );
        for (start, n) in ranges {
            assert_eq!(
                start / s,
                (start + n - 1) / s,
                "batch {start}+{n} crosses a span"
            );
        }
    }

    /// A pruned peer is never asked for a range outside its stripe: it takes a later range of
    /// its own stripe, the skipped ones wait for a peer that holds them, and the downloader
    /// reports the missing stripe until one connects.
    #[test]
    fn pruned_peers_only_get_their_stripe() {
        let s = STRIPE_SPAN;
        // Far below the peers' tops: no recent-block window applies.
        let top = 10 * s + 2 * PRUNED_TIP_BLOCKS;
        let mut d = tracking(s, 3 * s, top);
        let a = striped(7, top, seed_for(2 * s));
        let mut out = Vec::new();
        d.fill(&[a], &mut out);
        assert_eq!(get_objects(&out), vec![(7, 2 * s, 100)]);
        assert_eq!(
            d.missing_stripe().map(PruningSeed::raw),
            Some(seed_for(s).raw()),
            "heights {s}.. are in a stripe nobody connected holds"
        );
        assert_eq!(d.unserved().map(|u| u.start), Some(s));

        // Another pruned peer of the wrong stripe changes nothing for the gap.
        let b = striped(8, top, other_seed(s));
        out.clear();
        d.fill(&[a, b], &mut out);
        for (conn, start, n) in get_objects(&out) {
            assert_eq!(conn, 8);
            assert!(b.pruning_seed.holds_range(start, start + n));
            assert!(!(s..2 * s).contains(&start));
        }

        // A full peer takes the oldest set-aside batch; the stripe is no longer missing.
        let f = peer(9, top);
        out.clear();
        d.fill(&[a, b, f], &mut out);
        assert_eq!(get_objects(&out), vec![(9, s, 100)]);
        assert!(d.missing_stripe().is_none());

        // A peer of the right stripe takes the next one.
        let c = striped(10, top, seed_for(s));
        out.clear();
        d.fill(&[a, b, f, c], &mut out);
        assert_eq!(get_objects(&out), vec![(10, s + 100, 100)]);
    }

    /// Every pruned peer keeps its recent blocks in full, whatever its stripe.
    #[test]
    fn pruned_peers_serve_their_recent_blocks() {
        let s = STRIPE_SPAN;
        // The range starts within the peer's last `PRUNED_TIP_BLOCKS`.
        let near = s + PRUNED_TIP_BLOCKS / 2;
        let mut d = tracking(s, s + 300, near);
        let mut out = Vec::new();
        d.fill(&[striped(3, near, other_seed(s))], &mut out);
        assert_eq!(get_objects(&out), vec![(3, s, 100)]);

        // The same peer far ahead of the range does not have it.
        let far = s + 3 * PRUNED_TIP_BLOCKS;
        let mut d = tracking(s, s + 300, far);
        out.clear();
        d.fill(&[striped(3, far, other_seed(s))], &mut out);
        assert_eq!(get_objects(&out).len(), 0);
        assert!(d.missing_stripe().is_some());
    }

    /// A batch a full peer could not serve goes back to the queue and is only handed to a peer
    /// that holds it.
    #[test]
    fn requeued_batches_respect_stripes() {
        let s = STRIPE_SPAN;
        let top = 10 * s + 2 * PRUNED_TIP_BLOCKS;
        let mut d = tracking(s, s + 100, top);
        let mut out = Vec::new();
        d.fill(&[peer(1, top)], &mut out);
        let ids: Vec<Hash> = (s..s + 100).map(hid).collect();
        assert_eq!(get_objects(&out), vec![(1, s, 100)]);
        d.on_objects(
            1,
            ResponseGetObjects {
                txs: vec![],
                blocks: vec![],
                missed_ids: ids,
                current_blockchain_height: u32::try_from(top).unwrap(),
            },
            &mut out,
        );
        out.clear();
        d.fill(&[peer(1, top), striped(2, top, other_seed(s))], &mut out);
        assert!(
            get_objects(&out).is_empty(),
            "peer 1 lacks it, peer 2 is of another stripe"
        );
        out.clear();
        d.fill(
            &[
                peer(1, top),
                striped(2, top, other_seed(s)),
                striped(3, top, seed_for(s)),
            ],
            &mut out,
        );
        assert_eq!(get_objects(&out), vec![(3, s, 100)]);
    }

    /// One holder per lookup count, each in its own stripe.
    #[test]
    fn holders_are_looked_up_per_stripe_count() {
        let s = STRIPE_SPAN;
        let seed = |n, k| PruningSeed::new(n, k).unwrap();
        assert_eq!(stripe_holders_for(5 * s + 1), vec![seed(4, 2), seed(6, 6)]);
        assert_eq!(stripe_holders_for(0), vec![seed(4, 1), seed(6, 1)]);
        assert_eq!(stripe_holders_for(4 * s), vec![seed(4, 1), seed(6, 5)]);
        for h in [0, s - 1, s, 7 * s + 9, 2_618_387] {
            let seeds = stripe_holders_for(h);
            assert_eq!(seeds.len(), LOOKUP_STRIPES.len());
            assert!(seeds.iter().all(|x| x.keeps(h)));
            assert_eq!(seeds[0], seed_for(h));
        }
    }

    /// A 6-stripe peer serves its own spans; stripe 1 of 6 is not stripe 1 of 4.
    #[test]
    fn mixed_stripe_counts_in_the_downloader() {
        let s = STRIPE_SPAN;
        let top = 20 * s + 2 * PRUNED_TIP_BLOCKS;
        // Span 4 is stripe 1 of 4 and stripe 5 of 6.
        let mut d = tracking(4 * s, 4 * s + 200, top);
        let wrong = striped(1, top, PruningSeed::new(6, 1).unwrap());
        let mut out = Vec::new();
        d.fill(&[wrong], &mut out);
        assert_eq!(get_objects(&out).len(), 0);
        assert_eq!(d.unserved().map(|u| u.start), Some(4 * s));
        assert_eq!(d.missing_stripe(), PruningSeed::new(4, 1));
        let six = striped(2, top, PruningSeed::new(6, 5).unwrap());
        out.clear();
        d.fill(&[wrong, six], &mut out);
        assert_eq!(get_objects(&out), vec![(2, 4 * s, 100)]);
        assert_eq!(d.missing_stripe(), None);
    }

    /// Stripe rotation drops a reaching pruned peer without the range, the least useful first,
    /// never a holder, a full peer, a peer the caller protects or the last reaching peer.
    #[test]
    fn rotation_picks_the_least_useful_peer_without_the_stripe() {
        let s = STRIPE_SPAN;
        let top = 20 * s + 2 * PRUNED_TIP_BLOCKS;
        // Held by stripe 1 of 4 and stripe 5 of 6.
        let range = Unserved {
            start: 4 * s,
            end: 4 * s + 100,
        };
        let seed = |n, k| PruningSeed::new(n, k).unwrap();
        let any = |_: &PeerSlot| true;
        let d = Downloader::new(DownloaderConfig::default());
        let full = peer(1, top);
        let holder4 = striped(2, top, seed(4, 1));
        let holder6 = striped(3, top, seed(6, 5));
        let a = striped(4, top, seed(4, 2));
        let b = striped(5, top, seed(4, 3));
        let b2 = striped(6, top, seed(4, 3));
        let peers = [full, holder4, holder6, a, b, b2];
        // Two peers share stripe 3 of 4: the newer one goes first.
        assert_eq!(d.rotation_victim(&peers, range, any), Some(6));
        assert_eq!(d.rotation_victim(&peers, range, |p| p.conn != 6), Some(5));
        assert_eq!(d.rotation_victim(&peers, range, |p| p.conn == 4), Some(4));
        assert_eq!(d.rotation_victim(&peers, range, |p| p.conn < 4), None);
        // Without a shared seed the lower chain goes first.
        let c = striped(7, top - 100, seed(4, 4));
        assert_eq!(d.rotation_victim(&[full, a, c], range, any), Some(7));
        // Holders and full peers stay, also a pruned peer whose recent blocks cover the range.
        let recent = striped(8, range.end + 100, seed(4, 2));
        assert_eq!(
            d.rotation_victim(&[full, holder4, holder6, recent], range, any),
            None
        );
        // Stripe 1 of 6 does not hold span 4 (stripe 1 of 4 does).
        let one_of_6 = striped(9, top, seed(6, 1));
        assert_eq!(d.rotation_victim(&[holder4, one_of_6], range, any), Some(9));
        // The last peer that reaches the range stays; a peer behind it does not count.
        let behind = peer(10, 2 * s);
        assert_eq!(d.rotation_victim(&[a], range, any), None);
        assert_eq!(d.rotation_victim(&[a, behind], range, any), None);
        assert_eq!(d.rotation_victim(&[a, c, behind], range, any), Some(7));
    }

    /// A whole answer to `ids` (each block's bytes are its id).
    fn delivered_answer(ids: &[Hash]) -> ResponseGetObjects {
        ResponseGetObjects {
            txs: vec![],
            blocks: ids
                .iter()
                .map(|h| RawBlockLegacy {
                    block: h.0.to_vec(),
                    txs: vec![],
                })
                .collect(),
            missed_ids: vec![],
            current_blockchain_height: 0,
        }
    }

    /// An answer that has none of `ids`.
    fn missed_answer(ids: &[Hash]) -> ResponseGetObjects {
        ResponseGetObjects {
            txs: vec![],
            blocks: vec![],
            missed_ids: ids.to_vec(),
            current_blockchain_height: 0,
        }
    }

    /// `(conn, ids)` of every `NOTIFY_REQUEST_GET_OBJECTS` in `out`.
    fn requests(out: &[DlAction]) -> Vec<(u64, Vec<Hash>)> {
        out.iter()
            .filter_map(|a| match a {
                DlAction::Send(c, ProtocolMessage::RequestGetObjects(r)) => {
                    Some((*c, r.blocks.clone()))
                }
                _ => None,
            })
            .collect()
    }

    /// Peers `out` drops; every drop here is without a ban.
    fn drops(out: &[DlAction]) -> Vec<u64> {
        out.iter()
            .filter_map(|a| match a {
                DlAction::Drop(c, _, ban) => {
                    assert!(!ban, "a miss never bans");
                    Some(*c)
                }
                _ => None,
            })
            .collect()
    }

    /// A downloader with 10-block batches tracking `1..total` from `conn` (we have block 0).
    fn tracking_small(conn: u64, total: u64) -> Downloader {
        let mut d = Downloader::new(DownloaderConfig {
            batch_blocks: 10,
            ..DownloaderConfig::default()
        });
        d.want_sync();
        let mut out = Vec::new();
        d.request_chain(&[peer(conn, total)], 1, vec![hid(0)], &mut out);
        d.on_chain_entry(conn, small_entry(total), |h| *h == hid(0), &mut Vec::new())
            .unwrap();
        d
    }

    fn small_entry(total: u64) -> ResponseChainEntry {
        ResponseChainEntry {
            start_height: 0,
            total_height: u32::try_from(total).unwrap(),
            block_ids: (0..total).map(hid).collect(),
        }
    }

    /// A peer that claims the range (full, or the holding stripe) and answered it as missed is the
    /// first rotation victim, whatever its seed; it no longer counts as a peer reaching the range,
    /// so the last peer that has not missed it is never dropped in its favour. Such a range is
    /// still reported as unserved (stripe lookups go on).
    #[test]
    fn rotation_drops_a_peer_that_missed_what_it_claims_first() {
        let s = STRIPE_SPAN;
        let top = 20 * s + 2 * PRUNED_TIP_BLOCKS;
        let seed = |n, k| PruningSeed::new(n, k).unwrap();
        let any = |_: &PeerSlot| true;
        let a = striped(2, top, seed(4, 2));
        let b = striped(3, top, seed(4, 3));
        // A peer (`miss`) that claims to be full and one of the holding stripe (stripe 1 of 4).
        for miss in [peer(1, top), striped(1, top, seed(4, 1))] {
            let mut d = tracking(4 * s, 4 * s + 100, top);
            let mut out = Vec::new();
            d.fill(&[miss], &mut out);
            let reqs = requests(&out);
            assert_eq!(reqs.len(), 1);
            let mut follow = Vec::new();
            d.on_objects(1, missed_answer(&reqs[0].1), &mut follow);
            assert_eq!(drops(&follow).len(), 0, "one miss is no reason to drop");
            out.clear();
            d.fill(&[miss, a, b], &mut out);
            assert_eq!(requests(&out).len(), 0);
            let range = d
                .unserved()
                .expect("every holder missed it: still looked for");
            assert_eq!(range.start, 4 * s);
            assert_eq!(d.missing_stripe(), Some(seed(4, 1)));
            assert_eq!(d.rotation_victim(&[miss, a, b], range, any), Some(1));
            // The caller protects that peer (inbound, say): it does not count as reaching, so
            // the last peer that has not missed the range stays.
            assert_eq!(d.rotation_victim(&[miss, a], range, |p| p.conn != 1), None);
            assert_eq!(
                d.rotation_victim(&[miss, a, b], range, |p| p.conn != 1),
                Some(3)
            );
        }
    }

    /// The peers that delivered the most verified blocks are anchors and never rotated; only
    /// outbound peers make a seed "shared", so an inbound peer cannot pick the victim.
    #[test]
    fn anchors_and_outbound_seeds_steer_the_rotation() {
        let s = STRIPE_SPAN;
        let top = 20 * s + 2 * PRUNED_TIP_BLOCKS;
        let range = Unserved {
            start: 4 * s,
            end: 4 * s + 100,
        };
        let seed = |n, k| PruningSeed::new(n, k).unwrap();
        let any = |_: &PeerSlot| true;
        let full = peer(1, top);
        let a = striped(2, top, seed(4, 2));
        let b = striped(3, top, seed(4, 3));
        let c = striped(4, top, seed(4, 4));
        let mut d = Downloader::new(DownloaderConfig::default());
        d.records.entry(2).or_default().delivered = 500;
        d.records.entry(3).or_default().delivered = 100;
        assert_eq!(d.rotation_victim(&[full, a, b, c], range, any), Some(4));
        assert_eq!(d.rotation_victim(&[full, a, b], range, any), None);
        // A third productive peer is no anchor.
        d.records.entry(4).or_default().delivered = 50;
        assert_eq!(d.rotation_victim(&[full, a, b, c], range, any), Some(4));

        let d = Downloader::new(DownloaderConfig::default());
        let outbound = |p: &PeerSlot| p.outbound;
        let x = striped(5, top, seed(4, 2));
        let y = striped(6, top - 100, seed(4, 3));
        // An inbound peer with x's seed: x stays, the lower chain goes.
        let echo = PeerSlot {
            outbound: false,
            ..striped(7, top, seed(4, 2))
        };
        assert_eq!(
            d.rotation_victim(&[full, x, y, echo], range, outbound),
            Some(6)
        );
        // An outbound twin of x: one of the two goes (the newer).
        let twin = striped(8, top, seed(4, 2));
        assert_eq!(
            d.rotation_victim(&[full, x, y, twin], range, outbound),
            Some(8)
        );
    }

    /// Rotations for one span back off (30 s, 60 s, 120 s, ... 10 min) until a peer delivers
    /// blocks of that span; other spans only keep the 30 s spacing.
    #[test]
    fn rotations_back_off_per_span_until_it_is_served() {
        let s = STRIPE_SPAN;
        let range = Unserved {
            start: 4 * s,
            end: 4 * s + 100,
        };
        let other = Unserved {
            start: 9 * s,
            end: 9 * s + 100,
        };
        let mut d = Downloader::new(DownloaderConfig::default());
        let t0 = Instant::now();
        let at = |secs| t0 + Duration::from_secs(secs);
        assert!(d.may_rotate(range, t0));
        d.rotated(range, t0);
        assert!(!d.may_rotate(range, at(29)));
        assert!(d.may_rotate(range, at(30)));
        d.rotated(range, at(30));
        assert!(
            !d.may_rotate(range, at(89)),
            "60 s after the second rotation"
        );
        assert!(d.may_rotate(range, at(90)));
        d.rotated(range, at(90));
        assert!(!d.may_rotate(range, at(209)));
        assert!(d.may_rotate(range, at(210)));
        assert!(!d.may_rotate(other, at(100)));
        assert!(d.may_rotate(other, at(120)));
        // Blocks of the span arrived: its budget starts over.
        d.rotation.served(4 * s + 100, 4 * s + 200);
        assert!(d.may_rotate(range, at(120)));
        assert_eq!(rotation_backoff(0), Duration::from_secs(30));
        assert_eq!(rotation_backoff(1), Duration::from_mins(1));
        assert_eq!(rotation_backoff(2), Duration::from_mins(2));
        assert_eq!(rotation_backoff(4), Duration::from_mins(8));
        assert_eq!(rotation_backoff(5), Duration::from_mins(10));
        assert_eq!(rotation_backoff(u32::MAX), Duration::from_mins(10));
    }

    /// Three peers that claim to be full nodes (with the highest chains) and answer nothing,
    /// plus one full peer that delivers: the download finishes without a reset, that peer
    /// delivers everything and goes first once it has delivered, the other three are dropped
    /// (no ban) after their second miss.
    #[test]
    fn peers_claiming_full_without_blocks_do_not_reset_the_download() {
        let total = 200;
        let mut d = tracking_small(4, total);
        let claimers = [1, 2, 3];
        let mut peers = vec![
            peer(1, total + 50),
            peer(2, total + 50),
            peer(3, total + 50),
            peer(4, total),
        ];
        let mut submitted = 0;
        let mut dropped = Vec::new();
        for _ in 0..500 {
            let mut actions = Vec::new();
            d.fill(&peers, &mut actions);
            d.submit(&mut actions);
            let mut follow = Vec::new();
            for a in actions {
                match a {
                    DlAction::Send(c, ProtocolMessage::RequestGetObjects(r)) => {
                        let answer = if claimers.contains(&c) {
                            missed_answer(&r.blocks)
                        } else {
                            delivered_answer(&r.blocks)
                        };
                        d.on_objects(c, answer, &mut follow);
                    }
                    DlAction::Submit(ChainJob::Batch {
                        generation,
                        start,
                        blocks,
                        sources,
                        ..
                    }) => {
                        submitted += blocks.len();
                        let report = BatchReport {
                            submitted: blocks.len(),
                            ..BatchReport::default()
                        };
                        d.on_batch_done(generation, start, &sources, &report, &mut follow);
                    }
                    other => panic!("{other:?}"),
                }
            }
            for c in drops(&follow) {
                dropped.push(c);
                peers.retain(|p| p.conn != c);
                d.on_peer_closed(c);
            }
            assert_eq!(d.check_timeouts(Instant::now()).len(), 0);
            if d.idle() {
                break;
            }
        }
        assert_eq!(d.generation, 0, "no reset");
        assert_eq!(submitted, 199);
        assert_eq!(d.delivered(4), 199);
        assert!(dropped.iter().all(|c| claimers.contains(c)), "{dropped:?}");
        for conn in claimers {
            let misses = d
                .records
                .get(&conn)
                .map_or(CLAIMED_MISS_LIMIT, |r| r.claimed_misses);
            assert!(
                (1..=CLAIMED_MISS_LIMIT).contains(&misses),
                "{conn}: {misses}"
            );
            assert_eq!(d.delivered(conn), 0);
        }
        // The productive peer is asked first from now on, ahead of higher "full" chains.
        let mut d = tracking_small(4, total);
        d.records.entry(4).or_default().delivered = 10;
        let mut out = Vec::new();
        d.fill(&[peer(1, total + 50), peer(4, total)], &mut out);
        assert_eq!(requests(&out)[0].1[0], hid(1));
        assert_eq!(requests(&out)[0].0, 4);
    }

    /// A peer that claims the blocks and keeps missing them: the range is still looked for,
    /// the download starts over after `STALL_RESET` (not before), the peer keeps its record
    /// across the reset and is dropped (no ban) at its second miss.
    #[test]
    fn claimed_misses_drop_the_peer_and_a_stall_restarts() {
        let total = 30;
        let mut d = tracking_small(1, total);
        let peers = [peer(1, total)];
        let mut out = Vec::new();
        d.fill(&peers, &mut out);
        let reqs = requests(&out);
        assert_eq!(reqs.len(), 1);
        let mut follow = Vec::new();
        d.on_objects(1, missed_answer(&reqs[0].1), &mut follow);
        assert_eq!(drops(&follow).len(), 0);
        out.clear();
        d.fill(&peers, &mut out);
        assert_eq!(requests(&out).len(), 0);
        assert_eq!(d.unserved().map(|u| u.start), Some(1));
        let now = Instant::now();
        d.check_timeouts(now);
        assert_eq!(d.generation, 0);
        d.check_timeouts(now + STALL_RESET + Duration::from_secs(1));
        assert_eq!(d.generation, 1, "the stalled branch is given up");
        assert_eq!(
            d.records[&1].claimed_misses, 1,
            "the record survives the reset"
        );

        out.clear();
        assert!(d.needs_chain());
        d.request_chain(&peers, 1, vec![hid(0)], &mut out);
        d.on_chain_entry(1, small_entry(total), |h| *h == hid(0), &mut Vec::new())
            .unwrap();
        out.clear();
        d.fill(&peers, &mut out);
        let reqs = requests(&out);
        assert_eq!(reqs.len(), 1);
        let mut follow = Vec::new();
        d.on_objects(1, missed_answer(&reqs[0].1), &mut follow);
        assert_eq!(drops(&follow), vec![1]);
    }

    /// A pruned peer missing blocks we only assumed it keeps (its recent blocks) is not at
    /// fault: no claimed miss, no drop; it is just not asked for them again this round.
    #[test]
    fn misses_of_assumed_recent_blocks_do_not_count() {
        let s = STRIPE_SPAN;
        let near = s + PRUNED_TIP_BLOCKS / 2;
        let mut d = tracking(s, s + 300, near);
        let p = striped(3, near, other_seed(s));
        let mut out = Vec::new();
        d.fill(&[p], &mut out);
        let reqs = requests(&out);
        assert_eq!(reqs.len(), 1);
        let mut follow = Vec::new();
        d.on_objects(3, missed_answer(&reqs[0].1), &mut follow);
        assert_eq!(drops(&follow).len(), 0);
        assert_eq!(d.records[&3].claimed_misses, 0);
        out.clear();
        d.fill(&[p], &mut out);
        assert_eq!(requests(&out).len(), 0);
    }

    /// Invalid remote seeds (and the reserved recent-only one) never hold more than the
    /// recent blocks, never count as full (`/status.pruned_peers` counts them as pruned).
    #[test]
    fn invalid_seeds_never_hold_more_than_recent_blocks() {
        let s = STRIPE_SPAN;
        let top = 20 * s;
        for raw in [0x0400_u32, 0x0405, 0x0901, 0x1_0402, u32::MAX, 0x0100] {
            let seed = peer_seed(raw);
            assert_eq!(seed, PruningSeed::RECENT_ONLY, "{raw:#x}");
            assert!(!seed.is_full(), "{raw:#x} counts as a pruned peer");
            let p = striped(1, top, seed);
            assert!(!p.holds(4 * s, 4 * s + 100));
            assert!(!p.claims(top - 100, top));
            assert!(
                p.holds(top - 100, top),
                "its recent blocks are assumed whole"
            );
        }
        assert_eq!(peer_seed(0), PruningSeed::FULL);
        assert_eq!(peer_seed(0x0402), PruningSeed::new(4, 2).unwrap());
        assert_eq!(stripe_span_end(0), s);
        assert_eq!(stripe_span_end(s), 2 * s);
        assert_eq!(stripe_span_end(u64::MAX), u64::MAX);
    }

    /// splitmix64: reproducible pseudo-random values for the sweep below.
    fn splitmix(state: &mut u64) -> u64 {
        *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = *state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// A raw remote seed: any `u32`, any 16-bit value, a valid stripe seed, 0 or `0x0100`.
    fn random_raw_seed(state: &mut u64) -> u32 {
        let x = splitmix(state);
        match x % 5 {
            0 => (x >> 32) as u32,
            1 => (x >> 32) as u32 & 0xffff,
            2 => {
                let stripes = (x >> 8) % 7 + 2;
                let stripe = (x >> 16) % stripes + 1;
                ((stripes << 8) | stripe) as u32
            }
            3 => 0,
            _ => 0x0100,
        }
    }

    /// Random raw seeds, heights and ranges through `peer_seed`, `PeerSlot::holds` / `claims`,
    /// `stripe_span_end` and `rotation_victim` (a fixed-seed sweep in place of a fuzz target):
    /// nothing panics; a seed that does not decode claims nothing and holds exactly what a
    /// recent-only peer of the same height holds; whatever a peer holds a full peer of the same
    /// height holds too; a stripe claim is one span of the kept stripe; and a victim is a
    /// peer the caller allows, never full, not holding the range, reaching it, while another
    /// peer reaches it too.
    #[test]
    fn random_seeds_never_hold_more_than_a_full_peer() {
        let s = STRIPE_SPAN;
        let mut state = 0x5eed_u64;
        let d = Downloader::new(DownloaderConfig::default());
        let mut victims = 0;
        for round in 0..3_000 {
            let n = splitmix(&mut state) % 8 + 1;
            let mut wire_seeds = Vec::new();
            let mut peers = Vec::new();
            for conn in 0..n {
                let x = splitmix(&mut state);
                let raw = random_raw_seed(&mut state);
                wire_seeds.push(raw);
                peers.push(PeerSlot {
                    conn,
                    remote_height: match x % 8 {
                        0 => u64::from(u32::MAX),
                        1 => 0,
                        _ => (x >> 8) % (40 * s),
                    },
                    syncing: true,
                    usable: (x >> 4) & 3 != 0,
                    pruning_seed: peer_seed(raw),
                    outbound: (x >> 6) & 1 == 0,
                });
            }
            let y = splitmix(&mut state);
            let start = y % (40 * s);
            let end = start + (y >> 32) % (2 * s) + 1;
            let range = Unserved { start, end };

            for (p, raw) in peers.iter().zip(&wire_seeds) {
                let seed = p.pruning_seed;
                let full = PeerSlot {
                    pruning_seed: PruningSeed::FULL,
                    ..*p
                };
                let recent = PeerSlot {
                    pruning_seed: PruningSeed::RECENT_ONLY,
                    ..*p
                };
                let holds = p.holds(start, end);
                assert!(!holds || full.holds(start, end), "round {round}: {raw:#x}");
                if PruningSeed::decode(*raw).is_none() {
                    assert_eq!(seed, PruningSeed::RECENT_ONLY, "{raw:#x}");
                    assert!(!p.claims(start, end), "round {round}: {raw:#x} claims");
                    assert_eq!(holds, recent.holds(start, end), "round {round}: {raw:#x}");
                    assert_eq!(
                        holds,
                        start >= p.remote_height.saturating_sub(PRUNED_TIP_BLOCKS),
                        "round {round}: {raw:#x} holds only its recent blocks"
                    );
                } else {
                    assert_eq!(seed.raw(), *raw);
                }
                if seed.is_stripe() && p.claims(start, end) {
                    assert_eq!(start / s, (end - 1) / s, "round {round}: one span");
                    assert!(seed.keeps(start), "round {round}: the kept stripe");
                }
            }

            let h = splitmix(&mut state);
            let span_end = stripe_span_end(h);
            assert!(
                span_end == u64::MAX
                    || (span_end > h && span_end.is_multiple_of(s) && span_end - h <= s),
                "stripe_span_end({h}) = {span_end}"
            );

            let allowed = splitmix(&mut state);
            let may_drop = |p: &PeerSlot| (allowed >> p.conn) & 1 == 1;
            if let Some(v) = d.rotation_victim(&peers, range, may_drop) {
                victims += 1;
                let p = peers
                    .iter()
                    .find(|p| p.conn == v)
                    .expect("the victim is a connected peer");
                assert!(may_drop(p), "round {round}: the caller protects {v}");
                assert!(!p.pruning_seed.is_full(), "round {round}: a full victim");
                assert!(!p.holds(start, end), "round {round}: a holder as victim");
                assert!(p.usable && p.remote_height >= end, "round {round}");
                let reaching = peers
                    .iter()
                    .filter(|q| q.usable && q.remote_height >= end)
                    .count();
                assert!(reaching >= 2, "round {round}: the last reaching peer");
            }
        }
        assert!(victims > 0, "the sweep reaches the rotation");
    }

    #[test]
    fn spreads_batches_and_submits_in_order() {
        let mut d = Downloader::new(DownloaderConfig {
            batch_blocks: 10,
            max_job_blocks: 25,
            ..DownloaderConfig::default()
        });
        d.want_sync();
        let peers = [peer(1, 50), peer(2, 50), peer(3, 50)];
        assert!(d.needs_chain());
        let mut out = Vec::new();
        d.request_chain(&peers, 5, vec![hid(4), hid(0)], &mut out);
        let conn = match &out[..] {
            [DlAction::Send(c, ProtocolMessage::RequestChain(r))] => {
                assert_eq!(r.block_ids, vec![hid(4), hid(0)]);
                *c
            }
            other => panic!("{other:?}"),
        };
        // We have 0..=4; the peer has 0..=49.
        let entry = ResponseChainEntry {
            start_height: 4,
            total_height: 50,
            block_ids: (4..50).map(hid).collect(),
        };
        d.on_chain_entry(
            conn,
            entry,
            |h| (0..5).any(|i| hid(i) == *h),
            &mut Vec::new(),
        )
        .unwrap();
        out.clear();
        d.fill(&peers, &mut out);
        assert_eq!(out.len(), 3);
        assert_eq!(d.in_flight(), 3);

        // Answer out of order: peer of the second batch first.
        let mut requests: Vec<(u64, Vec<Hash>)> = out
            .drain(..)
            .map(|a| match a {
                DlAction::Send(c, ProtocolMessage::RequestGetObjects(r)) => (c, r.blocks),
                other => panic!("{other:?}"),
            })
            .collect();
        requests.sort_by_key(|(_, ids)| ids[0].0[0]);
        assert_eq!(requests[0].1[0], hid(5));
        let answer = |ids: &[Hash]| ResponseGetObjects {
            txs: vec![],
            blocks: ids
                .iter()
                .map(|h| RawBlockLegacy {
                    block: h.0.to_vec(),
                    txs: vec![],
                })
                .collect(),
            missed_ids: vec![],
            current_blockchain_height: 50,
        };
        d.on_objects(requests[1].0, answer(&requests[1].1), &mut out);
        d.submit(&mut out);
        assert!(out.is_empty(), "nothing before the first batch is in");
        d.on_objects(requests[0].0, answer(&requests[0].1), &mut out);
        d.on_objects(requests[2].0, answer(&requests[2].1), &mut out);
        d.submit(&mut out);
        match &out[..] {
            [
                DlAction::Submit(ChainJob::Batch {
                    start,
                    blocks,
                    expected,
                    sources,
                    ..
                }),
            ] => {
                assert_eq!(*start, 5);
                assert_eq!(blocks.len(), 30);
                assert_eq!(expected[0], hid(5));
                assert_eq!(sources.len(), 3);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn missing_blocks_go_to_another_peer() {
        let mut d = Downloader::new(DownloaderConfig {
            batch_blocks: 10,
            ..DownloaderConfig::default()
        });
        d.want_sync();
        let mut out = Vec::new();
        d.request_chain(&[peer(1, 30)], 1, vec![hid(0)], &mut out);
        let entry = ResponseChainEntry {
            start_height: 0,
            total_height: 30,
            block_ids: (0..30).map(hid).collect(),
        };
        d.on_chain_entry(1, entry, |h| *h == hid(0), &mut Vec::new())
            .unwrap();
        out.clear();
        d.fill(&[peer(1, 30)], &mut out);
        let ids = match &out[..] {
            [DlAction::Send(1, ProtocolMessage::RequestGetObjects(r))] => r.blocks.clone(),
            other => panic!("{other:?}"),
        };
        d.on_objects(
            1,
            ResponseGetObjects {
                txs: vec![],
                blocks: vec![],
                missed_ids: ids.clone(),
                current_blockchain_height: 30,
            },
            &mut out,
        );
        out.clear();
        d.fill(&[peer(1, 30), peer(2, 30)], &mut out);
        let to_two = out.iter().find_map(|a| match a {
            DlAction::Send(2, ProtocolMessage::RequestGetObjects(r)) => Some(r.blocks.clone()),
            _ => None,
        });
        assert_eq!(to_two, Some(ids));
    }

    /// An id no peer's chain has (distinct from every [`hid`]).
    fn other_id(i: u64) -> Hash {
        let mut b = [0u8; 32];
        b[..8].copy_from_slice(&i.to_le_bytes());
        b[31] = 2;
        Hash(b)
    }

    /// `(conn, block ids)` of every `NOTIFY_REQUEST_CHAIN` in `out`.
    fn chain_requests(out: &[DlAction]) -> Vec<(u64, Vec<Hash>)> {
        out.iter()
            .filter_map(|a| match a {
                DlAction::Send(c, ProtocolMessage::RequestChain(r)) => {
                    Some((*c, r.block_ids.clone()))
                }
                _ => None,
            })
            .collect()
    }

    fn entry(start: u64, total: u64, block_ids: Vec<Hash>) -> ResponseChainEntry {
        ResponseChainEntry {
            start_height: u32::try_from(start).unwrap(),
            total_height: u32::try_from(total).unwrap(),
            block_ids,
        }
    }

    const MAX_HEIGHT: u64 = 0xffff_ffff;

    #[test]
    fn network_height_estimate_is_the_second_highest() {
        assert_eq!(network_height_estimate([MAX_HEIGHT, 1000, 900]), 1000);
        assert_eq!(network_height_estimate([900, MAX_HEIGHT, 1000]), 1000);
        assert_eq!(network_height_estimate([7, 7]), 7);
        assert_eq!(network_height_estimate([5]), 5);
        assert_eq!(network_height_estimate([]), 0);
    }

    /// Announced heights count up to the network height estimate when a chain source is
    /// picked; equal ones go to the older connection.
    #[test]
    fn chain_source_heights_count_up_to_the_estimate() {
        let mut d = Downloader::new(DownloaderConfig::default());
        d.want_sync();
        let peers = [peer(9, MAX_HEIGHT), peer(1, 1000), peer(5, 900)];
        let mut out = Vec::new();
        d.request_chain(&peers, 10, vec![hid(9)], &mut out);
        assert_eq!(chain_requests(&out)[0].0, 1);
    }

    /// A complete entry with nothing new from a peer that announced more gives it a chain
    /// strike; the next chain request goes to a peer without one.
    #[test]
    fn nothing_new_source_is_not_asked_again() {
        let known = |h: &Hash| height_of(h) < 10 && h.0[31] == 1;
        let high = peer(9, MAX_HEIGHT);
        let other = peer(1, 1000);
        let mut d = Downloader::new(DownloaderConfig::default());
        d.want_sync();
        let mut out = Vec::new();
        d.request_chain(&[high], 10, vec![hid(9)], &mut out);
        assert_eq!(chain_requests(&out)[0].0, 9);
        let mut follow = Vec::new();
        d.on_chain_entry(9, entry(9, 10, vec![hid(9)]), known, &mut follow)
            .unwrap();
        assert_eq!(drops(&follow).len(), 0);
        assert_eq!(d.records[&9].chain_strikes, 1);
        assert_eq!(d.target_height, 0);

        d.want_sync();
        out.clear();
        d.request_chain(&[high, other], 10, vec![hid(9)], &mut out);
        assert_eq!(chain_requests(&out)[0].0, 1);
        d.on_chain_entry(
            1,
            entry(9, 1000, (9..1000).map(hid).collect()),
            known,
            &mut follow,
        )
        .unwrap();
        assert_eq!(d.target_height, 1000);
        assert_eq!(drops(&follow).len(), 0);

        // A complete entry that matches what the peer announced is no strike.
        let mut d = Downloader::new(DownloaderConfig::default());
        d.want_sync();
        out.clear();
        d.request_chain(&[peer(3, 10)], 9, vec![hid(9)], &mut out);
        d.on_chain_entry(3, entry(9, 10, vec![hid(9)]), known, &mut follow)
            .unwrap();
        assert_eq!(d.chain_strikes(3), 0);
    }

    /// Incomplete entries that add nothing, also continuations, are chain strikes; the second
    /// drops the peer without a ban, and no tracker or target height is left behind.
    #[test]
    fn entries_without_new_ids_are_chain_strikes() {
        let known = |h: &Hash| height_of(h) < 10 && h.0[31] == 1;
        let high = peer(9, MAX_HEIGHT);
        let mut d = Downloader::new(DownloaderConfig::default());
        for round in 1..=2 {
            d.want_sync();
            let mut out = Vec::new();
            d.request_chain(&[high], 10, vec![hid(9)], &mut out);
            assert_eq!(chain_requests(&out)[0].0, 9);
            let mut follow = Vec::new();
            d.on_chain_entry(9, entry(9, MAX_HEIGHT, vec![hid(9)]), known, &mut follow)
                .unwrap();
            if round == 1 {
                assert_eq!(drops(&follow).len(), 0);
            } else {
                assert_eq!(drops(&follow), vec![9]);
            }
        }
        assert_eq!(d.target_height, 0);
        assert!(!d.downloading());

        // A continuation from the tracker's last id that adds nothing.
        let mut d = Downloader::new(DownloaderConfig::default());
        d.want_sync();
        let mut out = Vec::new();
        d.request_chain(&[high], 10, vec![hid(9)], &mut out);
        let mut follow = Vec::new();
        d.on_chain_entry(
            9,
            entry(9, MAX_HEIGHT, vec![hid(9), hid(10)]),
            known,
            &mut follow,
        )
        .unwrap();
        assert!(d.needs_chain());
        out.clear();
        d.request_chain(&[high], 10, vec![hid(9)], &mut out);
        assert_eq!(chain_requests(&out), vec![(9, vec![hid(10), hid(9)])]);
        d.on_chain_entry(9, entry(10, MAX_HEIGHT, vec![hid(10)]), known, &mut follow)
            .unwrap();
        assert_eq!(drops(&follow).len(), 0);
        assert_eq!(d.chain_strikes(9), 1);
    }

    /// `target_height` is the total of the accepted entry: a refused entry leaves it, a reset
    /// clears it.
    #[test]
    fn target_height_follows_accepted_entries() {
        let known = |h: &Hash| (0..5).any(|i| hid(i) == *h);
        let mut d = Downloader::new(DownloaderConfig::default());
        d.want_sync();
        let mut out = Vec::new();
        let mut follow = Vec::new();
        d.request_chain(&[peer(1, 50)], 5, vec![hid(4)], &mut out);
        assert!(
            d.on_chain_entry(
                1,
                entry(0, MAX_HEIGHT, vec![other_id(0)]),
                known,
                &mut follow
            )
            .is_err()
        );
        assert_eq!(d.target_height, 0);
        d.request_chain(&[peer(1, 50)], 5, vec![hid(4)], &mut out);
        d.on_chain_entry(
            1,
            entry(4, 50, (4..50).map(hid).collect()),
            known,
            &mut follow,
        )
        .unwrap();
        assert_eq!(d.target_height, 50);
        d.reset();
        assert_eq!(d.target_height, 0);
    }

    /// A chain entry whose ids full peers do not have: they answer them as missed, no miss is
    /// recorded against them and they are never rotation victims; the range is not confirmed
    /// (also after a second chain entry with other ids), and each restart at it gives the
    /// entry's source a chain strike, which drops it (no ban) at the second.
    #[test]
    fn unknown_entry_ids_do_not_drop_full_peers() {
        let known = |h: &Hash| *h == hid(0);
        let source = striped(9, MAX_HEIGHT, PruningSeed::RECENT_ONLY);
        let all = [source, peer(1, 100), peer(2, 100)];
        let any = |_: &PeerSlot| true;
        let mut d = Downloader::new(DownloaderConfig {
            batch_blocks: 10,
            ..DownloaderConfig::default()
        });
        d.want_sync();
        for round in 1..=2 {
            let mut out = Vec::new();
            let mut follow = Vec::new();
            d.request_chain(&[source], 1, vec![hid(0)], &mut out);
            assert_eq!(chain_requests(&out)[0].0, 9);
            let ids = std::iter::once(hid(0))
                .chain((1..500).map(other_id))
                .collect();
            d.on_chain_entry(9, entry(0, 500, ids), known, &mut follow)
                .unwrap();
            for _ in 0..10 {
                out.clear();
                d.fill(&all, &mut out);
                let reqs = requests(&out);
                if reqs.is_empty() {
                    break;
                }
                for (c, ids) in reqs {
                    assert!(c == 1 || c == 2, "{c}");
                    d.on_objects(c, missed_answer(&ids), &mut follow);
                }
            }
            assert_eq!(drops(&follow).len(), 0);
            let range = d.unserved().expect("every holder answered it as missed");
            assert_eq!(d.rotation_range(), None);
            let victim = d.rotation_victim(&all, range, any);
            assert!(victim != Some(1) && victim != Some(2), "{victim:?}");
            if round == 1 {
                // A second chain entry from a full peer has other ids from height 1 on.
                assert!(d.needs_chain());
                out.clear();
                d.request_chain(&all, 1, vec![hid(0)], &mut out);
                let (c, block_ids) = chain_requests(&out).remove(0);
                assert_eq!((c, block_ids), (1, vec![hid(0)]));
                d.on_chain_entry(
                    1,
                    entry(0, 100, (0..100).map(hid).collect()),
                    known,
                    &mut follow,
                )
                .unwrap();
                assert_eq!(d.rotation_range(), None);
            }
            let now = Instant::now();
            assert_eq!(d.check_timeouts(now).len(), 0);
            let dropped = d.check_timeouts(now + STALL_RESET + Duration::from_secs(1));
            let dropped: Vec<u64> = dropped.into_iter().map(|(c, _)| c).collect();
            if round == 1 {
                assert_eq!(dropped, Vec::<u64>::new());
            } else {
                assert_eq!(dropped, vec![9]);
            }
            for c in [1, 2] {
                assert_eq!(d.records[&c].claimed_misses, 0);
            }
        }
    }

    /// A miss by a peer whose chain entry the tracker does not follow is recorded once
    /// another peer's blocks of that range verified; `downloading` turns true with the first
    /// verified batch.
    #[test]
    fn misses_of_other_peers_count_once_the_range_verifies() {
        let mut d = tracking_small(4, 30);
        let full = peer(1, 30);
        let source = peer(4, 30);
        assert!(d.active());
        assert!(!d.downloading());
        let mut out = Vec::new();
        d.fill(&[full], &mut out);
        let reqs = requests(&out);
        assert_eq!(reqs.len(), 1);
        let mut follow = Vec::new();
        d.on_objects(1, missed_answer(&reqs[0].1), &mut follow);
        assert_eq!(d.records[&1].claimed_misses, 0, "pending");
        out.clear();
        d.fill(&[full, source], &mut out);
        let reqs = requests(&out);
        assert_eq!(reqs[0].0, 4);
        assert_eq!(reqs[0].1[0], hid(1));
        d.on_objects(4, delivered_answer(&reqs[0].1), &mut follow);
        out.clear();
        d.submit(&mut out);
        let Some(DlAction::Submit(ChainJob::Batch {
            generation,
            start,
            blocks,
            sources,
            ..
        })) = out.pop()
        else {
            panic!("a job");
        };
        let report = BatchReport {
            submitted: blocks.len(),
            ..BatchReport::default()
        };
        d.on_batch_done(generation, start, &sources, &report, &mut follow);
        assert_eq!(drops(&follow).len(), 0);
        assert_eq!(d.records[&1].claimed_misses, 1);
        assert!(d.downloading());
    }

    /// When the next range has no holder, a second chain entry is asked from another peer
    /// (our sparse chain only); the same ids confirm the range, other ids do not, and the next
    /// second request waits.
    #[test]
    fn second_chain_entry_confirms_the_range() {
        let s = STRIPE_SPAN;
        let top = 10 * s + 2 * PRUNED_TIP_BLOCKS;
        let known = |h: &Hash| height_of(h) < s && h.0[31] == 1;
        let a = striped(7, top, seed_for(2 * s));
        let b = striped(8, top, seed_for(2 * s));
        for same in [true, false] {
            let mut d = tracking(s, s + 9_999, top);
            let mut out = Vec::new();
            d.fill(&[a, b], &mut out);
            let range = d.unserved().expect("no connected peer holds it");
            assert_eq!(d.rotation_range(), None);
            assert!(d.needs_chain());
            out.clear();
            d.request_chain(&[a, b], s, vec![hid(s - 1)], &mut out);
            assert_eq!(chain_requests(&out), vec![(7, vec![hid(s - 1)])]);
            assert!(!d.needs_chain());
            let ids: Vec<Hash> = std::iter::once(hid(s - 1))
                .chain((s..s + 500).map(|h| if same { hid(h) } else { other_id(h) }))
                .collect();
            let mut follow = Vec::new();
            d.on_chain_entry(7, entry(s - 1, top, ids), known, &mut follow)
                .unwrap();
            if same {
                assert_eq!(d.rotation_range(), Some(range));
            } else {
                assert_eq!(d.rotation_range(), None);
                assert!(!d.needs_chain(), "within the interval");
            }
        }
    }

    /// Re-queued batches stay inside the download window like tracker batches: with the byte
    /// budget used up only the batch at the head of the queue is requested again.
    #[test]
    fn requeued_batches_wait_for_the_download_window() {
        let mut d = Downloader::new(DownloaderConfig {
            batch_blocks: 10,
            max_ready_bytes: 50,
            ..DownloaderConfig::default()
        });
        d.want_sync();
        let mut out = Vec::new();
        d.request_chain(&[peer(1, 200)], 1, vec![hid(0)], &mut out);
        d.on_chain_entry(1, small_entry(200), |h| *h == hid(0), &mut Vec::new())
            .unwrap();
        out.clear();
        d.fill(&[peer(1, 200), peer(2, 200), peer(3, 200)], &mut out);
        let reqs = requests(&out);
        let firsts: Vec<(u64, Hash)> = reqs.iter().map(|r| (r.0, r.1[0])).collect();
        assert_eq!(firsts, vec![(1, hid(1)), (2, hid(11)), (3, hid(21))]);
        let mut follow = Vec::new();
        // A later batch arrives and fills the byte budget; another one is answered as missed.
        d.on_objects(2, delivered_answer(&reqs[1].1), &mut follow);
        assert!(d.ready_bytes() >= 50);
        d.on_objects(3, missed_answer(&reqs[2].1), &mut follow);
        out.clear();
        d.fill(&[peer(4, 200)], &mut out);
        assert_eq!(requests(&out).len(), 0, "outside the window");
        // The head batch loses its peer: it is requested again at once.
        d.on_peer_closed(1);
        out.clear();
        d.fill(&[peer(4, 200)], &mut out);
        let again = requests(&out);
        assert_eq!(again.len(), 1);
        assert_eq!(again[0].0, 4);
        assert_eq!(again[0].1[0], hid(1));
    }

    /// The stripe rotation budget of a span starts over once blocks of the span verified, not
    /// when they arrive.
    #[test]
    fn rotation_budget_starts_over_when_blocks_verify() {
        let mut d = tracking_small(1, 30);
        let range = Unserved { start: 1, end: 11 };
        let t0 = Instant::now();
        let at = |secs| t0 + Duration::from_secs(secs);
        d.rotated(range, t0);
        d.rotated(range, at(30));
        assert!(!d.may_rotate(range, at(60)));
        let mut out = Vec::new();
        d.fill(&[peer(1, 30)], &mut out);
        let reqs = requests(&out);
        assert_eq!(reqs.len(), 1);
        let mut follow = Vec::new();
        d.on_objects(1, delivered_answer(&reqs[0].1), &mut follow);
        assert!(!d.may_rotate(range, at(60)), "downloaded, not verified");
        out.clear();
        d.submit(&mut out);
        let Some(DlAction::Submit(ChainJob::Batch {
            generation,
            start,
            blocks,
            sources,
            ..
        })) = out.pop()
        else {
            panic!("a job");
        };
        let report = BatchReport {
            submitted: blocks.len(),
            ..BatchReport::default()
        };
        d.on_batch_done(generation, start, &sources, &report, &mut follow);
        assert!(d.may_rotate(range, at(60)));
    }

    /// A chain entry carries at most `BLOCKS_IDS_SYNCHRONIZING_DEFAULT_COUNT` ids.
    #[test]
    fn chain_entries_hold_at_most_the_protocol_count() {
        let n = BLOCKS_IDS_SYNCHRONIZING_DEFAULT_COUNT as u64;
        for (len, ok) in [(n, true), (n + 1, false)] {
            let mut d = Downloader::new(DownloaderConfig::default());
            d.want_sync();
            let mut out = Vec::new();
            d.request_chain(&[peer(1, len + 10)], 1, vec![hid(0)], &mut out);
            let ids: Vec<Hash> = (0..len).map(hid).collect();
            let r = d.on_chain_entry(1, entry(0, len + 10, ids), |h| *h == hid(0), &mut out);
            assert_eq!(r.is_ok(), ok, "{len} ids");
        }
    }
}
