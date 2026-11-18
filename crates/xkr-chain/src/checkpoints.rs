//! Hard-coded block-hash checkpoints (`docs/node-architecture.md`, "The sync pipeline").
//!
//! The C++ `src/config/cryptonote_checkpoints.h` is empty (`CHECKPOINTS = {}`). These hashes are
//! real mainnet blocks: the genesis block plus the blocks in
//! `crates/xkr-serialization/tests/fixtures/blocks.json`, fetched from a public node and
//! cross-checked by the block fixtures (block hash recomputed from the blob).
//!
//! Semantics (stricter than "skip everything" C++ checkpoints, see `docs/node-architecture.md`):
//! * a block at a checkpoint height whose hash differs is rejected, always;
//! * an alternative block is rejected when the main chain already holds a checkpoint at or
//!   above its height (Monero's `is_alternative_block_allowed`), so no reorg goes below a
//!   checkpoint the node has reached; below the next checkpoint ahead of the tip the heavier
//!   branch wins as usual ([`Checkpoints::reached_at_or_above`]);
//! * with `--fast-sync` only, proof of work is not verified for blocks up to the last
//!   checkpoint that arrive in a requested sync batch; single relayed blocks always have their
//!   proof of work checked. Everything else (signatures, amounts, double spends) is always
//!   verified.

use std::collections::BTreeMap;

use xkr_consensus::Network;
use xkr_storage::STRIPE_SPAN;
use xkr_types::Hash;

/// Mainnet checkpoints `(height, block hash)`.
pub const MAINNET_CHECKPOINTS: &[(u64, &str)] = &[
    (
        0,
        "7fb97df81221dd1366051b2d0bc7f49c66c22ac4431d879c895b06d66ef66f4c",
    ),
    (
        1_000,
        "0532eccb68ca3e2fd22d8d0ef86644326bb0ba05acc603cea70d150e5ca2d764",
    ),
    (
        5_052,
        "c915cfe4ead817bf55cc4d2049fa113f301bac8def38263bd69e7fd4b2bf9a20",
    ),
    (
        187_000,
        "14b514f2c7b7a56bfb7530bfc6372960c139f750b03a0361e27822857b085a47",
    ),
    (
        440_000,
        "eee4f79cf2d80b47927e23f4429e004c78c26f824f7d777009b9cfcd1acf168a",
    ),
    (
        620_000,
        "3111c996fa11a42139e61dd0b82827fa41223761179db222ba86ff59977d63be",
    ),
    (
        700_001,
        "c260e8de90abe25c3dd6b0b7cbdb83e2da571c9bb163cc19b2db8ae74faf64ae",
    ),
    (
        800_001,
        "d12896c7cecd8258f146ecc41e49aed7cc2c2d67ec861519cb78572fea28a4c4",
    ),
    (
        1_250_000,
        "049c2d5cf7c82fdcd52c6b3dc0c954e7f5282c3b1b7380a8119805ddc225dad1",
    ),
    (
        1_300_000,
        "1a245602e68ba888de302eabd3220fa9a43d9faf0182522c26587088c151e1ba",
    ),
    (
        1_750_000,
        "ce74788f86bf0d9971833f8cce2719be0bcec3eef79079bda3d95cff6d1f4637",
    ),
    (
        2_200_000,
        "21be4f1626bd273630126e4c2176b30dbf1d1c28131136a733cdaac8eabc4647",
    ),
    (
        2_617_000,
        "e27c0bd232b65c3213a62be08d5ff3af7ce019dcad157076d03730c8bbe27ae6",
    ),
    (
        2_617_400,
        "283f449be60fecccb9485bc010c830a3f818c43b08b96e6b3e7caf9c5dd1b966",
    ),
];

/// Testnet checkpoints: only the genesis block (the testnet genesis equals mainnet's, see
/// `xkr-consensus` `genesis.rs`).
pub const TESTNET_CHECKPOINTS: &[(u64, &str)] = &[(
    0,
    "7fb97df81221dd1366051b2d0bc7f49c66c22ac4431d879c895b06d66ef66f4c",
)];

/// A set of checkpoints.
#[derive(Debug, Clone, Default)]
pub struct Checkpoints {
    map: BTreeMap<u64, Hash>,
}

impl Checkpoints {
    /// The compiled-in checkpoints of `network`.
    ///
    /// # Panics
    /// Never for the constants above (they are valid hex).
    #[must_use]
    pub fn for_network(network: Network) -> Self {
        let list = match network {
            Network::Mainnet => MAINNET_CHECKPOINTS,
            Network::Testnet => TESTNET_CHECKPOINTS,
        };
        Self::from_list(
            list.iter()
                .map(|(h, s)| (*h, s.parse().expect("checkpoint hash is valid hex"))),
        )
    }

    /// Checkpoints from `(height, hash)` pairs.
    pub fn from_list(list: impl IntoIterator<Item = (u64, Hash)>) -> Self {
        Self {
            map: list.into_iter().collect(),
        }
    }

    /// Checkpoint hash at `height`, if any.
    #[must_use]
    pub fn get(&self, height: u64) -> Option<Hash> {
        self.map.get(&height).copied()
    }

    /// `Checkpoints::checkBlock`: false only if there is a checkpoint at `height` with another hash.
    #[must_use]
    pub fn check(&self, height: u64, hash: &Hash) -> bool {
        self.map.get(&height).is_none_or(|h| h == hash)
    }

    /// Every `(height, hash)`, ascending by height.
    pub fn iter(&self) -> impl Iterator<Item = (u64, Hash)> + '_ {
        self.map.iter().map(|(h, hash)| (*h, *hash))
    }

    /// Height of the last checkpoint (`None` if there are none).
    #[must_use]
    pub fn last_height(&self) -> Option<u64> {
        self.map.keys().next_back().copied()
    }

    /// The highest checkpoint at or below `tip` (the main chain's top height), if it is at or
    /// above `height`. An alternative block at `height` (or a reorg whose first new block is
    /// at `height`) is refused exactly when this is `Some`: the main chain already passed that
    /// checkpoint, and a branch from below it would have to replace the checkpointed block.
    #[must_use]
    pub fn reached_at_or_above(&self, tip: u64, height: u64) -> Option<u64> {
        self.map
            .range(..=tip)
            .next_back()
            .map(|(h, _)| *h)
            .filter(|c| *c >= height)
    }

    /// `Checkpoints::isInCheckpointZone`: `height <= last checkpoint`.
    #[must_use]
    pub fn in_zone(&self, height: u64) -> bool {
        self.last_height().is_some_and(|last| height <= last)
    }

    /// Stripe-pruning safety bound (`docs/storage.md` "Stripe pruning"): a node never drops
    /// signatures at or above `min(last checkpoint, tip - 2 * STRIPE_SPAN)` rounded down to a
    /// multiple of [`STRIPE_SPAN`]. Everything above stays whole, so reorgs (at most
    /// `max_reorg_depth` deep, never below a checkpoint the tip has passed) and peers syncing
    /// the newest blocks always find full blocks. 0 (prune nothing) without checkpoints or on a
    /// short chain; pass it as `below_height` to `xkr_storage::ops::prune_stripe`.
    #[must_use]
    pub fn prune_bound(&self, tip: u64) -> u64 {
        let Some(last) = self.last_height() else {
            return 0;
        };
        last.min(tip.saturating_sub(2 * STRIPE_SPAN)) / STRIPE_SPAN * STRIPE_SPAN
    }

    /// Number of checkpoints.
    #[must_use]
    pub fn len(&self) -> usize {
        self.map.len()
    }

    /// Whether there are none.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compiled_in_lists_parse() {
        let main = Checkpoints::for_network(Network::Mainnet);
        assert_eq!(main.len(), MAINNET_CHECKPOINTS.len());
        assert_eq!(main.last_height(), Some(2_617_400));
        assert!(main.in_zone(2_617_400));
        assert!(!main.in_zone(2_617_401));
        let genesis: Hash = xkr_consensus::genesis::MAINNET_GENESIS_HASH
            .parse()
            .unwrap();
        assert!(main.check(0, &genesis));
        assert!(!main.check(0, &Hash::ZERO));
        assert!(main.check(1, &Hash::ZERO));
        let test = Checkpoints::for_network(Network::Testnet);
        assert_eq!(test.last_height(), Some(0));
        assert!(test.check(0, &genesis));
    }

    /// The mainnet state hashes cover spans 0, 1, 2, … without gaps, each a 32-byte hex hash,
    /// and the last span ends at or below the last checkpoint.
    #[test]
    fn state_hash_list_is_contiguous_and_below_the_last_checkpoint() {
        use crate::state::MAINNET_STATE_HASHES;
        for (i, (span, hash)) in MAINNET_STATE_HASHES.iter().enumerate() {
            assert_eq!(*span as usize, i);
            assert!(hash.parse::<Hash>().is_ok(), "span {span}");
        }
        let last = u64::from(MAINNET_STATE_HASHES.last().unwrap().0);
        let last_checkpoint = Checkpoints::for_network(Network::Mainnet).last_height().unwrap();
        assert!((last + 1) * STRIPE_SPAN - 1 <= last_checkpoint);
    }

    #[test]
    fn alternative_blocks_are_refused_only_below_a_reached_checkpoint() {
        let cps = Checkpoints::from_list([(0, Hash::ZERO), (10, Hash::ZERO), (20, Hash::ZERO)]);
        // Tip 5: only genesis is reached, everything above it may be replaced.
        assert_eq!(cps.reached_at_or_above(5, 1), None);
        assert_eq!(cps.reached_at_or_above(5, 0), Some(0));
        // Tip 15: checkpoint 10 is reached.
        assert_eq!(cps.reached_at_or_above(15, 3), Some(10));
        assert_eq!(cps.reached_at_or_above(15, 10), Some(10));
        assert_eq!(cps.reached_at_or_above(15, 11), None);
        // Tip exactly on a checkpoint.
        assert_eq!(cps.reached_at_or_above(20, 20), Some(20));
        assert_eq!(cps.reached_at_or_above(20, 21), None);
        assert_eq!(Checkpoints::default().reached_at_or_above(5, 1), None);
    }

    #[test]
    fn prune_bound_stays_below_checkpoint_and_tip() {
        let main = Checkpoints::for_network(Network::Mainnet);
        // The tip side: tip - 8192 rounded down to 4096.
        assert_eq!(main.prune_bound(0), 0);
        assert_eq!(main.prune_bound(2 * STRIPE_SPAN + 4_095), 0);
        assert_eq!(main.prune_bound(3 * STRIPE_SPAN), STRIPE_SPAN);
        assert_eq!(
            main.prune_bound(1_000_000),
            991_808 / STRIPE_SPAN * STRIPE_SPAN
        );
        // The checkpoint side: 2 617 400 rounded down, however far the tip is.
        let at_checkpoint = 2_617_400 / STRIPE_SPAN * STRIPE_SPAN;
        assert_eq!(main.prune_bound(2_700_000), at_checkpoint);
        assert_eq!(main.prune_bound(u64::MAX), at_checkpoint);
        for tip in [10_000, 500_000, 2_620_000, 2_625_000] {
            let b = main.prune_bound(tip);
            assert!(b.is_multiple_of(STRIPE_SPAN));
            assert!(b <= 2_617_400 && b + 2 * STRIPE_SPAN <= tip.max(2 * STRIPE_SPAN));
        }
        // Testnet's only checkpoint is genesis: it never prunes; no checkpoints, neither.
        let test = Checkpoints::for_network(Network::Testnet);
        assert_eq!(test.prune_bound(1_000_000), 0);
        assert_eq!(Checkpoints::default().prune_bound(1_000_000), 0);
    }

    /// `prune_bound` = `min(last checkpoint, tip - 2 * STRIPE_SPAN)` rounded down to a span, at
    /// the edges of both sides: one block either side of a span boundary, the checkpoint on and
    /// just below a boundary, only the last checkpoint counting, and the extremes.
    #[test]
    fn prune_bound_edges() {
        let s = STRIPE_SPAN;
        let at = |last: u64| Checkpoints::from_list([(0, Hash::ZERO), (last, Hash([1; 32]))]);
        // The tip side with a checkpoint far above it.
        let far = at(1_000 * s);
        for (tip, bound) in [
            (2 * s, 0),
            (3 * s - 1, 0),
            (3 * s, s),
            (3 * s + 1, s),
            (4 * s - 1, s),
            (4 * s, 2 * s),
        ] {
            assert_eq!(far.prune_bound(tip), bound, "tip {tip}");
        }
        // The checkpoint side: on a span boundary it is the bound itself, one below it is not.
        for (last, tip, bound) in [
            (3 * s, 5 * s, 3 * s),
            (3 * s, 5 * s - 1, 2 * s),
            (3 * s, u64::MAX, 3 * s),
            (3 * s - 1, u64::MAX, 2 * s),
            (3 * s + 1, u64::MAX, 3 * s),
            (s - 1, u64::MAX, 0),
            (0, u64::MAX, 0),
        ] {
            assert_eq!(
                at(last).prune_bound(tip),
                bound,
                "checkpoint {last}, tip {tip}"
            );
        }
        // Only the last checkpoint counts, whatever order the list came in.
        let several = Checkpoints::from_list([
            (7 * s + 5, Hash([2; 32])),
            (0, Hash::ZERO),
            (2 * s + 9, Hash([3; 32])),
        ]);
        assert_eq!(several.last_height(), Some(7 * s + 5));
        assert_eq!(several.prune_bound(u64::MAX), 7 * s);
        assert_eq!(several.prune_bound(6 * s), 4 * s);
        // Both sides at their maximum: no overflow, still a whole span.
        let top = at(u64::MAX).prune_bound(u64::MAX);
        assert_eq!(top, (u64::MAX - 2 * s) / s * s);
        assert!(top.is_multiple_of(s));
        // The bound never decreases as the tip grows.
        let mut previous = 0;
        for tip in (0..12 * s).step_by(511) {
            let bound = at(8 * s + 3).prune_bound(tip);
            assert!(bound >= previous && bound <= 8 * s, "tip {tip}");
            assert!(bound.is_multiple_of(s));
            previous = bound;
        }
        assert_eq!(previous, 8 * s);
    }
}
