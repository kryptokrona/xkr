//! Miner statistics: hashrate, accepted and rejected solutions, the job being mined (C++
//! `MinerManager::printHashRate`, xkrd `/status.mining`).

use std::time::Instant;

/// What the miner has done so far.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Stats {
    /// Hashing threads.
    pub threads: usize,
    /// Hashes per second over the last report interval.
    pub hashrate: f64,
    /// Hashes since start.
    pub total_hashes: u64,
    /// Solutions accepted: blocks added to the chain (solo) or shares the pool took.
    pub accepted: u64,
    /// Solutions refused by the node or the pool.
    pub rejected: u64,
    /// Solutions of a job that was no longer current (not submitted, or the pool called the
    /// job stale).
    pub stale: u64,
    /// Solutions that could not be submitted (no answer from the node or the pool).
    pub errors: u64,
    /// Whether the threads have a job.
    pub mining: bool,
    /// Height of the job (0 when a pool does not say).
    pub height: u64,
    /// Difficulty of the job (block difficulty, or the share difficulty of a pool target).
    pub difficulty: u64,
    /// Id of the job.
    pub job_id: Option<String>,
    /// Hash of the last accepted block.
    pub last_block_hash: Option<String>,
    /// The last refusal or submit error.
    pub last_error: Option<String>,
}

/// Hashes per second between two readings of a hash counter.
#[derive(Debug, Clone, Copy)]
pub struct RateMeter {
    last_total: u64,
    last_at: Instant,
}

impl RateMeter {
    /// A meter starting at `total` hashes now.
    #[must_use]
    pub fn new(total: u64) -> Self {
        Self {
            last_total: total,
            last_at: Instant::now(),
        }
    }

    /// The rate since the previous reading; `total` becomes the new starting point.
    pub fn update(&mut self, total: u64) -> f64 {
        let now = Instant::now();
        let secs = now.duration_since(self.last_at).as_secs_f64();
        #[expect(clippy::cast_precision_loss, reason = "a hashrate is approximate")]
        let rate = if secs > 0.0 {
            total.saturating_sub(self.last_total) as f64 / secs
        } else {
            0.0
        };
        self.last_total = total;
        self.last_at = now;
        rate
    }
}

/// C++ `utilities::get_mining_speed`: two decimals and H/s, KH/s, MH/s or GH/s.
#[must_use]
pub fn format_speed(hashrate: f64) -> String {
    if hashrate > 1e9 {
        format!("{:.2} GH/s", hashrate / 1e9)
    } else if hashrate > 1e6 {
        format!("{:.2} MH/s", hashrate / 1e6)
    } else if hashrate > 1e3 {
        format!("{:.2} KH/s", hashrate / 1e3)
    } else {
        format!("{hashrate:.2} H/s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn speeds() {
        assert_eq!(format_speed(12.345), "12.35 H/s");
        assert_eq!(format_speed(1500.0), "1.50 KH/s");
        assert_eq!(format_speed(2_500_000.0), "2.50 MH/s");
        assert_eq!(format_speed(3e9 + 1.0), "3.00 GH/s");
        let mut m = RateMeter::new(0);
        assert!(m.update(0) >= 0.0);
    }
}
