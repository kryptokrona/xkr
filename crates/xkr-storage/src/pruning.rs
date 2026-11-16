//! Monero-style stripe pruning (`docs/storage.md` "Stripe pruning").
//!
//! The chain is cut into spans of [`STRIPE_SPAN`] blocks; span `n` belongs to stripe
//! `n % stripes + 1`. A pruning node keeps the ring signatures of one stripe (its
//! [`PruningSeed`]) below its prune height and drops the others; everything else (records,
//! outputs, key images, indexes) is kept, so wallets, RPC and the verification of new blocks
//! are unaffected. Nodes with the same stripe count but different stripes together hold every
//! signature, and full nodes ([`PruningSeed::FULL`]) hold them all.
//!
//! A span is exactly 64 sealed chunks of [`CHUNK_BLOCKS`] blocks, so a chunk never straddles two
//! stripes and pruning deletes whole signature parts.

use crate::schema::CHUNK_BLOCKS;

/// Blocks per stripe span (64 chunks of [`CHUNK_BLOCKS`] blocks).
pub const STRIPE_SPAN: u64 = 4_096;

/// Stripe count of a seed picked by `xkrd --prune` (a node then keeps about a quarter of the
/// old signatures).
pub const DEFAULT_STRIPES: u8 = 4;

/// Smallest stripe count a seed may have.
pub const MIN_STRIPES: u8 = 2;

/// Largest stripe count a seed may have.
pub const MAX_STRIPES: u8 = 8;

/// Stripe counts a node short of a stripe looks for (`docs/hyper-transport.md`, "Stripe
/// topics"): the default and 6, the two counts the network is meant to use. A height has one
/// holding stripe per count ([`PruningSeed::holding`]), and they differ between counts.
pub const LOOKUP_STRIPES: [u8; 2] = [DEFAULT_STRIPES, 6];

const _: () = assert!(STRIPE_SPAN.is_multiple_of(CHUNK_BLOCKS));

/// Raw value of [`PruningSeed::RECENT_ONLY`]: stripe count 1, stripe 0 (never a valid stripe
/// seed: older xkrd nodes fail to decode it and treat it like any invalid seed, C++ nodes
/// ignore the key altogether).
const RECENT_ONLY_RAW: u32 = 1 << 8;

/// Which stripe a node keeps, encoded as `stripes << 8 | stripe` with `stripe` in
/// `1..=stripes` and `stripes` in [`MIN_STRIPES`]`..=`[`MAX_STRIPES`]. Raw 0
/// ([`PruningSeed::FULL`], the default) is a full node that keeps every signature; raw `0x0100`
/// ([`PruningSeed::RECENT_ONLY`]) is a node pruned completely (`xkrd db prune --all`) that keeps
/// only its recent blocks whole.
///
/// The stripe count travels in the seed, so a network can mix stripe counts (4 and 6, say)
/// without a protocol change: every peer computes the stripe of a height from the peer's own
/// seed.
///
/// The raw wire value is private: seeds come from [`Self::new`], [`Self::decode`] and the
/// constants, which validate, so a seed is always one of the three kinds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct PruningSeed(u32);

impl PruningSeed {
    /// A full node: keeps every stripe.
    pub const FULL: Self = Self(0);

    /// A node that keeps no old signatures at all, only its newest blocks whole (a database
    /// pruned with `xkrd db prune --all`, and the receiving side's reading of an invalid remote
    /// seed). It holds no stripe: [`Self::keeps`] is false for every height and
    /// [`Self::stripes`] / [`Self::stripe`] are 0.
    pub const RECENT_ONLY: Self = Self(RECENT_ONLY_RAW);

    /// The seed keeping `stripe` (`1..=stripes`) of `stripes` (`2..=8`); `None` otherwise.
    #[must_use]
    pub fn new(stripes: u8, stripe: u8) -> Option<Self> {
        if !(MIN_STRIPES..=MAX_STRIPES).contains(&stripes) || stripe == 0 || stripe > stripes {
            return None;
        }
        Some(Self((u32::from(stripes) << 8) | u32::from(stripe)))
    }

    /// Decodes a seed from its wire/`META` value: 0 is [`Self::FULL`], `0x0100` is
    /// [`Self::RECENT_ONLY`]; anything else must be a valid `stripes << 8 | stripe` (no other
    /// bits set), else `None`.
    #[must_use]
    pub fn decode(value: u32) -> Option<Self> {
        if value == 0 {
            return Some(Self::FULL);
        }
        if value == RECENT_ONLY_RAW {
            return Some(Self::RECENT_ONLY);
        }
        if value > 0xffff {
            return None;
        }
        Self::new((value >> 8) as u8, (value & 0xff) as u8)
    }

    /// The seed of `stripes` stripes (`2..=8`, else `None`) that keeps `height`: stripe
    /// `(height / STRIPE_SPAN) % stripes + 1`. Seeds of different counts keep different
    /// heights, so a node looking for the holders of `height` asks for one seed per count.
    #[must_use]
    pub fn holding(stripes: u8, height: u64) -> Option<Self> {
        if !(MIN_STRIPES..=MAX_STRIPES).contains(&stripes) {
            return None;
        }
        let stripe = ((height / STRIPE_SPAN) % u64::from(stripes)) as u8 + 1;
        Self::new(stripes, stripe)
    }

    /// The encoded value (`stripes << 8 | stripe`, 0 for a full node, `0x0100` for
    /// [`Self::RECENT_ONLY`]).
    #[must_use]
    pub const fn raw(self) -> u32 {
        self.0
    }

    /// Number of stripes (0 for a full node and for [`Self::RECENT_ONLY`]).
    #[must_use]
    pub const fn stripes(self) -> u8 {
        if self.is_stripe() {
            (self.0 >> 8) as u8
        } else {
            0
        }
    }

    /// The kept stripe, `1..=stripes` (0 for a full node and for [`Self::RECENT_ONLY`]).
    #[must_use]
    pub const fn stripe(self) -> u8 {
        if self.is_stripe() {
            (self.0 & 0xff) as u8
        } else {
            0
        }
    }

    /// Whether this is a full node's seed (keeps everything).
    #[must_use]
    pub const fn is_full(self) -> bool {
        self.0 == 0
    }

    /// Whether this is [`Self::RECENT_ONLY`] (keeps no old signatures, no stripe).
    #[must_use]
    pub const fn is_recent_only(self) -> bool {
        self.0 == RECENT_ONLY_RAW
    }

    /// Whether this seed keeps one stripe (neither full nor [`Self::RECENT_ONLY`]).
    #[must_use]
    pub const fn is_stripe(self) -> bool {
        !self.is_full() && !self.is_recent_only()
    }

    /// The stripe (`1..=stripes`) that `height` belongs to under this seed's stripe count:
    /// `(height / STRIPE_SPAN) % stripes + 1`. 0 for a full node and for
    /// [`Self::RECENT_ONLY`] (no stripes).
    #[must_use]
    pub fn stripe_of_height(self, height: u64) -> u8 {
        let stripes = self.stripes();
        if stripes == 0 {
            return 0;
        }
        ((height / STRIPE_SPAN) % u64::from(stripes)) as u8 + 1
    }

    /// Whether a node with this seed keeps the signatures of the block at `height` (once
    /// pruned; heights above the node's prune height are always kept). Never for
    /// [`Self::RECENT_ONLY`].
    #[must_use]
    pub fn keeps(self, height: u64) -> bool {
        self.is_full() || (self.is_stripe() && self.stripe_of_height(height) == self.stripe())
    }

    /// Whether this seed keeps sealed chunk `chunk_index` (blocks
    /// `chunk_index * CHUNK_BLOCKS ..` + [`CHUNK_BLOCKS`], always inside one stripe span).
    #[must_use]
    pub fn keeps_chunk(self, chunk_index: u64) -> bool {
        self.keeps(chunk_index.saturating_mul(CHUNK_BLOCKS))
    }

    /// Whether this seed keeps every block of the half-open range `start..end` (true for an
    /// empty range and for a full node). With two or more stripes this needs the whole range
    /// inside one span of the kept stripe; [`Self::RECENT_ONLY`] holds no non-empty range.
    ///
    /// This is the seed alone: a pruning peer also holds every block at or above its own
    /// prune height (the newest `2 * STRIPE_SPAN` or more blocks), which the caller has to
    /// add from the peer's height.
    #[must_use]
    pub fn holds_range(self, start: u64, end: u64) -> bool {
        if start >= end || self.is_full() {
            return true;
        }
        start / STRIPE_SPAN == (end - 1) / STRIPE_SPAN && self.keeps(start)
    }
}

impl std::fmt::Display for PruningSeed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.is_full() {
            f.write_str("full")
        } else if self.is_recent_only() {
            write!(f, "recent blocks only ({:#06x})", self.0)
        } else {
            write!(
                f,
                "stripe {}/{} ({:#06x})",
                self.stripe(),
                self.stripes(),
                self.0
            )
        }
    }
}

/// What a database has dropped: the signatures of every block below `pruned_below` that
/// `seed` does not keep. With [`PruningSeed::RECENT_ONLY`] nothing below it is kept
/// ([`crate::ops::prune_all_below`], the "drop everything" mode; databases pruned that way
/// before the seed existed store no seed and read back as `RECENT_ONLY` too); with the full
/// seed nothing was pruned (a full seed with `pruned_below > 0` is still read as "everything
/// below dropped").
///
/// `pruned_below` is a multiple of [`STRIPE_SPAN`] for stripe pruning and of
/// [`CHUNK_BLOCKS`] for the drop-everything mode, so a chunk is always dropped or kept whole.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PruneState {
    /// The stripe kept below `pruned_below` (`RECENT_ONLY`: none).
    pub seed: PruningSeed,
    /// Height below which signatures may have been dropped (`META["pruned_below"]`).
    pub pruned_below: u64,
}

impl PruneState {
    /// Whether the signatures of the block at `height` were dropped.
    #[must_use]
    pub fn dropped(&self, height: u64) -> bool {
        height < self.pruned_below && (self.seed.is_full() || !self.seed.keeps(height))
    }

    /// Whether the signatures of sealed chunk `chunk` were dropped.
    #[must_use]
    pub fn chunk_dropped(&self, chunk: u64) -> bool {
        self.dropped(chunk.saturating_mul(CHUNK_BLOCKS))
    }

    /// Whether any signature was dropped.
    #[must_use]
    pub const fn is_pruned(&self) -> bool {
        self.pruned_below > 0
    }
}

/// How [`crate::Store::prune`] prunes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PruneMode {
    /// Keep the signatures of this seed's stripe ([`crate::ops::prune_stripe`]).
    Stripe(PruningSeed),
    /// Keep no old signatures at all ([`crate::ops::prune_all_below`]).
    All,
}

/// What a prune call did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PruneReport {
    /// The database's prune state afterwards.
    pub state: PruneState,
    /// Sealed chunks whose signature parts were deleted.
    pub chunks: u64,
    /// Signature parts deleted (one 4 KB page each).
    pub parts: u64,
    /// Hot signature rows (blocks not sealed yet) deleted.
    pub hot_rows: u64,
    /// Transactions whose signatures were dropped.
    pub transactions: u64,
}

impl PruneReport {
    /// Adds the counters of `later` and takes its state (several steps of one prune).
    pub fn absorb(&mut self, later: &Self) {
        self.state = later.state;
        self.chunks += later.chunks;
        self.parts += later.parts;
        self.hot_rows += later.hot_rows;
        self.transactions += later.transactions;
    }
}

/// The seed stored as `META["pruning_seed"]` (absent = full).
pub(crate) fn seed_from_meta(value: Option<u64>) -> crate::Result<PruningSeed> {
    let Some(v) = value else {
        return Ok(PruningSeed::FULL);
    };
    u32::try_from(v)
        .ok()
        .and_then(PruningSeed::decode)
        .ok_or(crate::Error::Corrupt("invalid pruning seed in meta"))
}

/// The prune state from `META["pruning_seed"]` and `META["pruned_below"]`. A database pruned
/// completely before [`PruningSeed::RECENT_ONLY`] existed stores no seed: with a prune height it
/// reads as `RECENT_ONLY`, so such a node never presents itself as a full one.
pub(crate) fn state_from_meta(seed: Option<u64>, pruned_below: u64) -> crate::Result<PruneState> {
    let mut seed = seed_from_meta(seed)?;
    if seed.is_full() && pruned_below > 0 {
        seed = PruningSeed::RECENT_ONLY;
    }
    Ok(PruneState { seed, pruned_below })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spans_are_whole_chunks() {
        assert!(STRIPE_SPAN.is_multiple_of(CHUNK_BLOCKS));
        assert_eq!(STRIPE_SPAN / CHUNK_BLOCKS, 64);
        // Every chunk lies inside one span: its first and last block share a stripe.
        let seed = PruningSeed::new(4, 1).unwrap();
        for chunk in 0..(3 * STRIPE_SPAN / CHUNK_BLOCKS) {
            let first = chunk * CHUNK_BLOCKS;
            let last = first + CHUNK_BLOCKS - 1;
            assert_eq!(seed.stripe_of_height(first), seed.stripe_of_height(last));
            assert_eq!(seed.keeps_chunk(chunk), seed.keeps(first));
            assert_eq!(seed.keeps_chunk(chunk), seed.keeps(last));
        }
    }

    #[test]
    fn encoding_round_trips_and_rejects_invalid_seeds() {
        let s = PruningSeed::new(4, 3).unwrap();
        assert_eq!(s.raw(), 0x0403);
        assert_eq!((s.stripes(), s.stripe()), (4, 3));
        assert!(!s.is_full());
        assert_eq!(PruningSeed::decode(0x0403), Some(s));
        assert_eq!(PruningSeed::decode(0), Some(PruningSeed::FULL));
        assert_eq!(PruningSeed::default(), PruningSeed::FULL);
        assert!(PruningSeed::FULL.is_full());
        assert_eq!(
            (PruningSeed::FULL.stripes(), PruningSeed::FULL.stripe()),
            (0, 0)
        );
        for stripes in MIN_STRIPES..=MAX_STRIPES {
            for stripe in 1..=stripes {
                let seed = PruningSeed::new(stripes, stripe).unwrap();
                assert_eq!(PruningSeed::decode(seed.raw()), Some(seed));
            }
            assert_eq!(PruningSeed::new(stripes, 0), None);
            assert_eq!(PruningSeed::new(stripes, stripes + 1), None);
        }
        // Stripe counts outside 2..=8, stripe 0 or above the count, stray high bits.
        for bad in [
            0x0001_u32,
            0x0101,
            0x0102,
            0x0200,
            0x0203,
            0x0900,
            0x0901,
            0x0909,
            0x00ff,
            0x0408,
            0x1_0402,
            0xffff_ffff,
        ] {
            assert_eq!(PruningSeed::decode(bad), None, "{bad:#x}");
        }
        assert_eq!(PruningSeed::new(1, 1), None);
        assert_eq!(PruningSeed::new(9, 1), None);
        assert_eq!(s.to_string(), "stripe 3/4 (0x0403)");
        assert_eq!(PruningSeed::FULL.to_string(), "full");
    }

    /// The reserved "recent blocks only" seed (stripe count 1, stripe 0): no stripe, keeps no
    /// old height, holds no non-empty range, and is neither full nor a stripe seed.
    #[test]
    fn recent_only_seed_keeps_nothing_old() {
        let r = PruningSeed::RECENT_ONLY;
        assert_eq!(r.raw(), 0x0100);
        assert_eq!(PruningSeed::decode(0x0100), Some(r));
        assert!(r.is_recent_only() && !r.is_full() && !r.is_stripe());
        assert_eq!((r.stripes(), r.stripe()), (0, 0));
        for h in [
            0,
            1,
            STRIPE_SPAN - 1,
            STRIPE_SPAN,
            5 * STRIPE_SPAN + 17,
            u64::MAX,
        ] {
            assert!(!r.keeps(h), "keeps {h}");
            assert_eq!(r.stripe_of_height(h), 0);
        }
        assert!(!r.keeps_chunk(0) && !r.keeps_chunk(u64::MAX));
        assert!(!r.holds_range(0, 1));
        assert!(!r.holds_range(STRIPE_SPAN, STRIPE_SPAN + 10));
        assert!(r.holds_range(9, 9), "an empty range is held by everyone");
        assert_eq!(r.to_string(), "recent blocks only (0x0100)");
        // Only a stripe seed is a stripe seed.
        assert!(PruningSeed::new(4, 1).unwrap().is_stripe());
        assert!(!PruningSeed::FULL.is_stripe() && !PruningSeed::FULL.is_recent_only());
        // `PruningSeed::new` never builds it (count 1 is not a stripe count).
        assert_eq!(PruningSeed::new(1, 0), None);
    }

    /// A completely pruned database reads as `RECENT_ONLY`, also without a stored seed (pruned
    /// before the seed existed); a full seed without a prune height stays full.
    #[test]
    fn completely_pruned_state_reads_as_recent_only() {
        let recent = PruningSeed::RECENT_ONLY;
        assert_eq!(state_from_meta(None, 0).unwrap(), PruneState::default());
        assert_eq!(
            state_from_meta(None, 128).unwrap(),
            PruneState {
                seed: recent,
                pruned_below: 128
            }
        );
        assert_eq!(state_from_meta(Some(0x0100), 128).unwrap().seed, recent);
        assert_eq!(
            state_from_meta(Some(0x0402), 4096).unwrap().seed,
            PruningSeed::new(4, 2).unwrap()
        );
        assert!(state_from_meta(Some(0x0400), 0).is_err());
        let state = PruneState {
            seed: recent,
            pruned_below: 128,
        };
        assert!(state.dropped(0) && state.dropped(127) && !state.dropped(128));
        assert!(state.chunk_dropped(1) && !state.chunk_dropped(2));
    }

    #[test]
    fn stripe_of_height_cycles_through_the_spans() {
        let s = PruningSeed::new(4, 2).unwrap();
        assert_eq!(s.stripe_of_height(0), 1);
        assert_eq!(s.stripe_of_height(STRIPE_SPAN - 1), 1);
        assert_eq!(s.stripe_of_height(STRIPE_SPAN), 2);
        assert_eq!(s.stripe_of_height(2 * STRIPE_SPAN - 1), 2);
        assert_eq!(s.stripe_of_height(3 * STRIPE_SPAN), 4);
        assert_eq!(s.stripe_of_height(4 * STRIPE_SPAN), 1);
        assert_eq!(
            s.stripe_of_height(u64::MAX),
            ((u64::MAX / STRIPE_SPAN) % 4) as u8 + 1
        );
        assert!(!s.keeps(0));
        assert!(s.keeps(STRIPE_SPAN));
        assert!(s.keeps(5 * STRIPE_SPAN + 17));
        assert_eq!(PruningSeed::FULL.stripe_of_height(12_345), 0);
        assert!(PruningSeed::FULL.keeps(12_345));
        assert!(PruningSeed::FULL.keeps_chunk(u64::MAX));
    }

    /// The seeds `1..=n` of one stripe count keep every height exactly once.
    fn assert_partition(stripes: u8) {
        let seeds: Vec<_> = (1..=stripes)
            .map(|k| PruningSeed::new(stripes, k).unwrap())
            .collect();
        let spans = 3 * u64::from(stripes) + 1;
        for height in (0..spans * STRIPE_SPAN).step_by(97).chain([
            0,
            STRIPE_SPAN - 1,
            STRIPE_SPAN,
            spans * STRIPE_SPAN - 1,
            2_618_387,
        ]) {
            let holders = seeds.iter().filter(|s| s.keeps(height)).count();
            assert_eq!(holders, 1, "height {height} with {stripes} stripes");
        }
        for chunk in 0..spans * STRIPE_SPAN / CHUNK_BLOCKS {
            let holders = seeds.iter().filter(|s| s.keeps_chunk(chunk)).count();
            assert_eq!(holders, 1, "chunk {chunk} with {stripes} stripes");
        }
    }

    #[test]
    fn seeds_of_one_count_cover_every_height_once() {
        for stripes in MIN_STRIPES..=MAX_STRIPES {
            assert_partition(stripes);
        }
        // The two counts most likely in use, explicitly.
        assert_partition(4);
        assert_partition(6);
    }

    #[test]
    fn holds_range_needs_one_span_of_the_kept_stripe() {
        let s = PruningSeed::new(4, 2).unwrap();
        let span = STRIPE_SPAN;
        assert!(s.holds_range(span, 2 * span));
        assert!(s.holds_range(span + 10, span + 20));
        assert!(s.holds_range(5 * span, 6 * span));
        assert!(!s.holds_range(span - 1, span + 5));
        assert!(!s.holds_range(span, 2 * span + 1));
        assert!(!s.holds_range(0, 10));
        assert!(!s.holds_range(span, 6 * span));
        // Empty ranges are held by everyone; full nodes hold everything.
        assert!(s.holds_range(7, 7));
        assert!(s.holds_range(9, 3));
        assert!(PruningSeed::FULL.holds_range(0, u64::MAX));
        // Exactly one seed of a count holds a span-aligned range.
        for start in [0, span, 2 * span, 3 * span, 11 * span] {
            let holders = (1..=4)
                .filter(|&k| {
                    PruningSeed::new(4, k)
                        .unwrap()
                        .holds_range(start, start + span)
                })
                .count();
            assert_eq!(holders, 1);
        }
    }

    #[test]
    fn holding_names_the_seed_of_each_count() {
        let span = STRIPE_SPAN;
        for stripes in MIN_STRIPES..=MAX_STRIPES {
            for height in [0, 17, span - 1, span, 5 * span + 3, 2_618_387, u64::MAX] {
                let seed = PruningSeed::holding(stripes, height).unwrap();
                assert_eq!(seed.stripes(), stripes);
                assert!(seed.keeps(height), "{seed} keeps {height}");
                assert_eq!(seed.stripe(), seed.stripe_of_height(height));
            }
        }
        // Span 5: stripe 2 of 4, stripe 6 of 6.
        assert_eq!(PruningSeed::holding(4, 5 * span), PruningSeed::new(4, 2));
        assert_eq!(PruningSeed::holding(6, 5 * span), PruningSeed::new(6, 6));
        for bad in [0, 1, MAX_STRIPES + 1, u8::MAX] {
            assert_eq!(PruningSeed::holding(bad, 12_345), None);
        }
        assert_eq!(LOOKUP_STRIPES, [DEFAULT_STRIPES, 6]);
    }

    /// A network mixing stripe counts: every span is held by exactly one seed of each count,
    /// and the stripe number alone says nothing (stripe 1 of 4 and stripe 1 of 6 differ).
    #[test]
    fn mixed_stripe_counts_hold_their_own_spans() {
        let span = STRIPE_SPAN;
        let one_of_4 = PruningSeed::new(4, 1).unwrap();
        let one_of_6 = PruningSeed::new(6, 1).unwrap();
        // Span 4: stripe 1 of 4, stripe 5 of 6.
        assert!(one_of_4.holds_range(4 * span, 4 * span + 100));
        assert!(!one_of_6.holds_range(4 * span, 4 * span + 100));
        assert!(
            PruningSeed::new(6, 5)
                .unwrap()
                .holds_range(4 * span, 5 * span)
        );
        // Span 12 is stripe 1 under both counts.
        assert!(one_of_4.holds_range(12 * span, 13 * span));
        assert!(one_of_6.holds_range(12 * span, 13 * span));
        for n in 0..36 {
            let (start, end) = (n * span + 7, n * span + 107);
            for stripes in LOOKUP_STRIPES {
                let holders: Vec<u8> = (1..=stripes)
                    .filter(|&k| {
                        PruningSeed::new(stripes, k)
                            .unwrap()
                            .holds_range(start, end)
                    })
                    .collect();
                let expected = PruningSeed::holding(stripes, start).unwrap().stripe();
                assert_eq!(holders, vec![expected], "span {n}, {stripes} stripes");
            }
        }
    }

    #[test]
    fn prune_state_drops_the_other_stripes_below_the_height() {
        let seed = PruningSeed::new(2, 1).unwrap();
        let state = PruneState {
            seed,
            pruned_below: 3 * STRIPE_SPAN,
        };
        assert!(!state.dropped(0));
        assert!(state.dropped(STRIPE_SPAN));
        assert!(state.dropped(2 * STRIPE_SPAN - 1));
        assert!(!state.dropped(2 * STRIPE_SPAN));
        // At and above the prune height everything is kept.
        assert!(!state.dropped(3 * STRIPE_SPAN));
        assert!(!state.dropped(3 * STRIPE_SPAN + STRIPE_SPAN));
        assert!(state.chunk_dropped(STRIPE_SPAN / CHUNK_BLOCKS));
        assert!(!state.chunk_dropped(0));
        assert!(state.is_pruned());

        // The full seed with a prune height: everything below it is gone.
        let all = PruneState {
            seed: PruningSeed::FULL,
            pruned_below: 128,
        };
        assert!(all.dropped(0) && all.dropped(127) && !all.dropped(128));
        assert!(all.chunk_dropped(1) && !all.chunk_dropped(2));
        // Nothing pruned.
        let none = PruneState::default();
        assert!(!none.dropped(0) && !none.is_pruned());
        let seeded = PruneState {
            seed,
            pruned_below: 0,
        };
        assert!(!seeded.dropped(STRIPE_SPAN) && !seeded.is_pruned());
    }

    #[test]
    fn reports_accumulate() {
        let mut total = PruneReport::default();
        let step = PruneReport {
            state: PruneState {
                seed: PruningSeed::new(4, 1).unwrap(),
                pruned_below: STRIPE_SPAN,
            },
            chunks: 2,
            parts: 7,
            hot_rows: 1,
            transactions: 9,
        };
        total.absorb(&step);
        total.absorb(&step);
        assert_eq!(total.state, step.state);
        assert_eq!(
            (
                total.chunks,
                total.parts,
                total.hot_rows,
                total.transactions
            ),
            (4, 14, 2, 18)
        );
    }
}
