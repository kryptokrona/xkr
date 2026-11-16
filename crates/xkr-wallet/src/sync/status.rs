//! Sync progress: the last block hashes and the sparse checkpoints the wallet sends with
//! `/getwalletsyncdata` (`src/wallet_backend/synchronization_status.cpp`
//! `SynchronizationStatus`).

use std::collections::VecDeque;

use xkr_types::Hash;
use xkr_wallet_files::backend::{SynchronizationStatusJson, UnknownFields};

/// `Constants::LAST_KNOWN_BLOCK_HASHES_SIZE`.
pub const LAST_KNOWN_BLOCK_HASHES_SIZE: usize = 100;
/// `Constants::BLOCK_HASH_CHECKPOINTS_INTERVAL`.
pub const BLOCK_HASH_CHECKPOINTS_INTERVAL: u64 = 5000;

/// `SynchronizationStatus`, with the height of every stored hash.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SyncStatus {
    /// One hash every [`BLOCK_HASH_CHECKPOINTS_INTERVAL`] blocks, newest first.
    checkpoints: VecDeque<(u64, Hash)>,
    /// The last [`LAST_KNOWN_BLOCK_HASHES_SIZE`] hashes, newest first.
    last_known: VecDeque<(u64, Hash)>,
    /// Index of the last processed block.
    height: u64,
    /// Fields of the wallet file not modelled here.
    pub(crate) other: UnknownFields,
}

impl SyncStatus {
    /// `getHeight`: index of the last processed block (0 before the first).
    #[must_use]
    pub fn height(&self) -> u64 {
        self.height
    }

    /// Whether no block was processed yet.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.last_known.is_empty() && self.checkpoints.is_empty()
    }

    /// Hash of the last processed block.
    #[must_use]
    pub fn last_hash(&self) -> Option<Hash> {
        self.last_known.front().map(|(_, h)| *h)
    }

    /// The stored hash of the block at `height`, if it is among the last hashes or the
    /// checkpoints.
    #[must_use]
    pub fn hash_at(&self, height: u64) -> Option<Hash> {
        self.last_known
            .iter()
            .chain(self.checkpoints.iter())
            .find(|(h, _)| *h == height)
            .map(|(_, hash)| *hash)
    }

    /// `storeBlockHash`: records a processed block.
    pub(crate) fn store_block_hash(&mut self, hash: Hash, height: u64) {
        self.height = height;
        if height.is_multiple_of(BLOCK_HASH_CHECKPOINTS_INTERVAL) {
            self.checkpoints.push_front((height, hash));
        }
        self.last_known.push_front((height, hash));
        self.last_known.truncate(LAST_KNOWN_BLOCK_HASHES_SIZE);
    }

    /// `getBlockHashCheckpoints`: the last hashes, then the checkpoints, newest first.
    #[must_use]
    pub fn checkpoints(&self) -> Vec<Hash> {
        self.last_known
            .iter()
            .chain(self.checkpoints.iter())
            .map(|(_, h)| *h)
            .collect()
    }

    /// Forgets the blocks at or above `fork_height` (they are on a chain the node left).
    pub(crate) fn rewind(&mut self, fork_height: u64) {
        while self
            .last_known
            .front()
            .is_some_and(|(h, _)| *h >= fork_height)
        {
            self.last_known.pop_front();
        }
        while self
            .checkpoints
            .front()
            .is_some_and(|(h, _)| *h >= fork_height)
        {
            self.checkpoints.pop_front();
        }
        self.height = fork_height.saturating_sub(1).min(self.height);
        if self.is_empty() {
            self.height = 0;
        }
    }

    /// `SynchronizationStatus::fromJSON`. The file stores hashes without heights; the last
    /// hashes are taken as consecutive blocks below `lastKnownBlockHeight` and the checkpoints
    /// as the multiples of the interval below it, which is how they were stored.
    #[must_use]
    pub fn from_json(json: &SynchronizationStatusJson) -> Self {
        let height = json.last_known_block_height;
        let last_known = json
            .last_known_block_hashes
            .iter()
            .enumerate()
            .map(|(i, h)| (height.saturating_sub(i as u64), *h))
            .collect();
        let top = height - height % BLOCK_HASH_CHECKPOINTS_INTERVAL;
        let checkpoints = json
            .block_hash_checkpoints
            .iter()
            .enumerate()
            .map(|(i, h)| {
                (
                    top.saturating_sub(i as u64 * BLOCK_HASH_CHECKPOINTS_INTERVAL),
                    *h,
                )
            })
            .collect();
        Self {
            checkpoints,
            last_known,
            height,
            other: json.other.clone(),
        }
    }

    /// `SynchronizationStatus::toJSON`.
    #[must_use]
    pub fn to_json(&self) -> SynchronizationStatusJson {
        SynchronizationStatusJson {
            block_hash_checkpoints: self.checkpoints.iter().map(|(_, h)| *h).collect(),
            last_known_block_hashes: self.last_known.iter().map(|(_, h)| *h).collect(),
            last_known_block_height: self.height,
            other: self.other.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(n: u64) -> Hash {
        let mut b = [0u8; 32];
        b[..8].copy_from_slice(&n.to_le_bytes());
        Hash(b)
    }

    #[test]
    fn stores_rewinds_and_round_trips() {
        let mut s = SyncStatus::default();
        assert!(s.is_empty());
        for height in 0..=10_250 {
            s.store_block_hash(h(height), height);
        }
        assert_eq!(s.height(), 10_250);
        let cps = s.checkpoints();
        assert_eq!(cps.len(), 100 + 3);
        assert_eq!(cps[0], h(10_250));
        assert_eq!(cps[99], h(10_151));
        assert_eq!(&cps[100..], &[h(10_000), h(5_000), h(0)]);
        assert_eq!(s.hash_at(10_200), Some(h(10_200)));
        assert_eq!(s.hash_at(5_000), Some(h(5_000)));
        assert_eq!(s.hash_at(7_000), None);

        let back = SyncStatus::from_json(&s.to_json());
        assert_eq!(back, s);

        s.rewind(10_200);
        assert_eq!(s.height(), 10_199);
        assert_eq!(s.last_hash(), Some(h(10_199)));
        s.rewind(9_000);
        assert_eq!(s.height(), 8_999);
        assert_eq!(s.checkpoints(), vec![h(5_000), h(0)]);
        s.rewind(0);
        assert!(s.is_empty());
        assert_eq!(s.height(), 0);
    }
}
