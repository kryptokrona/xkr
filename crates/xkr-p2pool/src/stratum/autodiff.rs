//! The per-miner difficulty from its share times (`StratumServer::update_auto_diff`, the
//! autodiff part of `StratumServer::on_blobs_ready`, `hash_compress` / `hash_uncompress`).
//!
//! A window of the last [`AUTO_DIFF_SIZE`] shares keeps a 16-bit time stamp and the share's
//! hash count, compressed to 16 bits. The difficulty is the window's hashes per second times
//! [`AUTO_DIFF_TARGET_TIME`], at least [`MIN_DIFF`]. The first share fixes it to that share's
//! hash count until there are ten.

use super::job::{AUTO_DIFF_TARGET_TIME, MIN_DIFF};

/// `StratumClient::AUTO_DIFF_SIZE`: shares in the window.
pub const AUTO_DIFF_SIZE: usize = 64;

/// Bits of the mantissa of a compressed hash count (`HashValue::bits`).
const HASH_BITS: u32 = 11;
/// `HashValue::mask`.
const HASH_MASK: u64 = (1 << HASH_BITS) - 1;

/// `hash_uncompress`: mantissa (low 11 bits) shifted left by the exponent (high 5 bits).
#[must_use]
pub const fn hash_uncompress(h: u64) -> u64 {
    (h & HASH_MASK) << (h >> HASH_BITS)
}

/// `HashMaxValue::value`: the largest count a compressed value holds.
const HASH_MAX_VALUE: u64 = hash_uncompress(0xFFFF);

/// `hash_compress`: a hash count as 16 bits (5 bits exponent, 11 bits mantissa), rounded down.
#[must_use]
pub fn hash_compress(h: u64) -> u16 {
    if h <= HASH_MASK {
        return h as u16;
    }
    if h >= HASH_MAX_VALUE {
        return u16::MAX;
    }
    let bsr = 63 - u64::from(h.leading_zeros());
    let shift = bsr - u64::from(HASH_BITS - 1);
    ((shift << HASH_BITS) | (h >> shift)) as u16
}

/// One share in the window (`AutoDiffData`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Entry {
    /// Seconds since the epoch, low 16 bits.
    timestamp: u16,
    /// Compressed hash count.
    hashes: u16,
}

/// A miner's autodiff state (`m_autoDiffData`, `m_autoDiffWindowHashes`, `m_autoDiffIndex`,
/// `m_autoDiff`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AutoDiff {
    data: [Entry; AUTO_DIFF_SIZE],
    window_hashes: u64,
    index: u32,
    diff: u64,
}

impl Default for AutoDiff {
    fn default() -> Self {
        Self::new()
    }
}

impl AutoDiff {
    /// No shares yet, difficulty 0 (unset).
    #[must_use]
    pub const fn new() -> Self {
        Self {
            data: [Entry {
                timestamp: 0,
                hashes: 0,
            }; AUTO_DIFF_SIZE],
            window_hashes: 0,
            index: 0,
            diff: 0,
        }
    }

    /// The current autodiff, 0 before the first share.
    #[must_use]
    pub const fn difficulty(&self) -> u64 {
        self.diff
    }

    /// Shares counted so far.
    #[must_use]
    pub const fn shares(&self) -> u32 {
        self.index
    }

    /// The hashrate estimate the difficulty stands for (`m_autoDiff / AUTO_DIFF_TARGET_TIME`).
    #[must_use]
    pub const fn hashrate(&self) -> u64 {
        self.diff / AUTO_DIFF_TARGET_TIME
    }

    /// `update_auto_diff`: one share of `hashes` at `timestamp` (seconds since the epoch).
    pub fn update(&mut self, timestamp: u64, hashes: u64) {
        let compressed = hash_compress(hashes);
        self.window_hashes = self
            .window_hashes
            .wrapping_add(hash_uncompress(u64::from(compressed)));

        let k = self.index;
        self.index = self.index.wrapping_add(1);
        let n = AUTO_DIFF_SIZE as u32;
        let slot = (k % n) as usize;

        if k >= n {
            self.window_hashes = self
                .window_hashes
                .wrapping_sub(hash_uncompress(u64::from(self.data[slot].hashes)));
        }

        let t1 = self.data[slot].timestamp;
        let t2 = timestamp as u16;
        self.data[slot] = Entry {
            timestamp: t2,
            hashes: compressed,
        };

        if k >= n {
            // full window
            let dt = u64::from(t2.wrapping_sub(t1));
            self.diff =
                (self.window_hashes.wrapping_mul(AUTO_DIFF_TARGET_TIME) / dt.max(1)).max(MIN_DIFF);
        } else if k >= 10 {
            // partial window: the first share's hashes were found before its time stamp
            let h0 = hash_uncompress(u64::from(self.data[0].hashes));
            let dt = u64::from(
                self.data[slot]
                    .timestamp
                    .wrapping_sub(self.data[0].timestamp),
            );
            self.diff = (self
                .window_hashes
                .wrapping_sub(h0)
                .wrapping_mul(AUTO_DIFF_TARGET_TIME)
                / dt.max(1))
            .max(MIN_DIFF);
        } else if k == 0 {
            // first share: the difficulty stays at this share's until there are ten
            self.diff = hashes;
        }
    }

    /// Before a new job (`on_blobs_ready`): more than `5 * AUTO_DIFF_TARGET_TIME` seconds since
    /// the last share cuts the difficulty by an eighth (at least [`MIN_DIFF`]), each job until
    /// a share comes. Nothing happens before the first share.
    pub fn decay(&mut self, now: u64) {
        if self.diff == 0 {
            return;
        }
        let last = self.data[(self.index.wrapping_sub(1) % AUTO_DIFF_SIZE as u32) as usize];
        let elapsed = u64::from((now as u16).wrapping_sub(last.timestamp));
        if elapsed > AUTO_DIFF_TARGET_TIME * 5 {
            self.diff = (self.diff - self.diff / 8).max(MIN_DIFF);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compression() {
        for h in [0u64, 1, 2047] {
            assert_eq!(u64::from(hash_compress(h)), h);
        }
        assert_eq!(hash_uncompress(u64::from(hash_compress(2048))), 2048);
        assert_eq!(hash_uncompress(u64::from(hash_compress(4097))), 4096);
        assert_eq!(hash_compress(u64::MAX), u16::MAX);
        assert_eq!(HASH_MAX_VALUE, 2047 << 31);
        for h in [5000u64, 1_000_000, 123_456_789, 1 << 40] {
            let back = hash_uncompress(u64::from(hash_compress(h)));
            assert!(back <= h && back >= h - h / 1024, "{h} {back}");
        }
    }
}
