//! Hashrate and share counters of the stratum server (`StratumServer::update_hashrate_data`,
//! `print_stratum_status`, `api_update_local_stats`, `reset_share_counters`).

use std::collections::VecDeque;
use std::net::SocketAddr;

use serde_json::{Value, json};

use crate::difficulty::Difficulty;

/// The longest hashrate window kept: 24 hours.
const DAY: u64 = 24 * 60 * 60;

/// Accepted hashes over time and the sidechain shares found (`m_hashrateData` and the counters
/// next to it).
#[derive(Clone, Debug)]
pub struct HashrateTracker {
    /// `(seconds since the epoch, cumulative hashes)`, one entry per second with shares, 24
    /// hours back.
    data: VecDeque<(u64, u64)>,
    cumulative: u64,
    at_last_share: u64,
    found_shares_diff: f64,
    shares_found: u32,
    shares_failed: u32,
    last_share_found_time: u64,
    stratum_shares: u64,
}

/// What [`HashrateTracker::share_found`] changed, to take back when the share could not be
/// added.
#[derive(Clone, Copy, Debug)]
pub struct FoundShare {
    /// The share's effort in percent.
    pub effort: f64,
    prev_at_last_share: u64,
    diff: f64,
    prev_time: u64,
}

impl HashrateTracker {
    /// Empty, starting at `now`.
    #[must_use]
    pub fn new(now: u64) -> Self {
        Self {
            data: VecDeque::from([(now, 0)]),
            cumulative: 0,
            at_last_share: 0,
            found_shares_diff: 0.0,
            shares_found: 0,
            shares_failed: 0,
            last_share_found_time: 0,
            stratum_shares: 0,
        }
    }

    /// `update_hashrate_data`: an accepted share of `hashes` at `timestamp` (0 hashes only moves
    /// the clock).
    pub fn update(&mut self, hashes: u64, timestamp: u64) {
        if hashes != 0 {
            self.cumulative = self.cumulative.wrapping_add(hashes);
            self.stratum_shares += 1;
        }
        match self.data.back_mut() {
            Some(head) if head.0 == timestamp => head.1 = self.cumulative,
            _ => self.data.push_back((timestamp, self.cumulative)),
        }
        while self.data.len() > 1 && self.data.front().is_some_and(|f| f.0 + DAY < timestamp) {
            self.data.pop_front();
        }
    }

    /// Average hashes per second over the last `window` seconds of data.
    #[must_use]
    pub fn hashrate(&self, window: u64) -> u64 {
        let Some(&(head_t, head_h)) = self.data.back() else {
            return 0;
        };
        let i = self.data.partition_point(|e| e.0 + window < head_t);
        let Some(&(tail_t, tail_h)) = self.data.get(i) else {
            return 0;
        };
        let dt = head_t.saturating_sub(tail_t);
        head_h.wrapping_sub(tail_h).checked_div(dt).unwrap_or(0)
    }

    /// A sidechain share of `hashes` with sidechain difficulty `diff` found at `now`
    /// (`on_share_found`).
    #[allow(clippy::cast_precision_loss)]
    pub fn share_found(&mut self, hashes: u64, diff: f64, now: u64) -> FoundShare {
        let n = self.cumulative.wrapping_add(hashes);
        let effort = n.wrapping_sub(self.at_last_share) as f64 * 100.0 / diff;
        let found = FoundShare {
            effort,
            prev_at_last_share: self.at_last_share,
            diff,
            prev_time: self.last_share_found_time,
        };
        self.at_last_share = n;
        self.found_shares_diff += diff;
        self.shares_found += 1;
        self.last_share_found_time = now;
        found
    }

    /// The share of [`Self::share_found`] could not be added: count it as failed.
    pub fn share_failed(&mut self, found: FoundShare) {
        self.at_last_share = found.prev_at_last_share;
        self.found_shares_diff -= found.diff;
        if self.shares_found > 0 {
            self.shares_found -= 1;
            self.shares_failed += 1;
            self.last_share_found_time = found.prev_time;
        }
    }

    /// `on_reset_share_counters`.
    pub fn reset_share_counters(&mut self) {
        self.at_last_share = self.cumulative;
        self.shares_found = 0;
        self.shares_failed = 0;
        self.last_share_found_time = 0;
    }

    /// The server-wide part of [`StratumStats`]; `sidechain_difficulty` for the current effort.
    #[must_use]
    #[allow(clippy::cast_precision_loss)]
    pub fn snapshot(&self, sidechain_difficulty: Difficulty) -> StratumStats {
        let since_last = self.cumulative.saturating_sub(self.at_last_share);
        let average_effort = if self.found_shares_diff > 0.0 {
            self.at_last_share as f64 * 100.0 / self.found_shares_diff
        } else {
            0.0
        };
        let side = sidechain_difficulty.to_f64();
        let current_effort = if side > 0.0 {
            since_last as f64 * 100.0 / side
        } else {
            0.0
        };
        StratumStats {
            hashrate_15m: self.hashrate(15 * 60),
            hashrate_1h: self.hashrate(60 * 60),
            hashrate_24h: self.hashrate(DAY),
            total_hashes: self.cumulative,
            total_stratum_shares: self.stratum_shares,
            last_share_found_time: self.last_share_found_time,
            shares_found: self.shares_found,
            shares_failed: self.shares_failed,
            average_effort,
            current_effort,
            connections: 0,
            workers: Vec::new(),
        }
    }
}

/// One connected miner (an entry of the API's `workers`, a line of `workers`).
#[derive(Clone, Debug, PartialEq)]
pub struct WorkerStats {
    /// The miner's address.
    pub addr: SocketAddr,
    /// Seconds connected.
    pub uptime: u64,
    /// Difficulty of its last job (the sidechain difficulty before the first job).
    pub difficulty: Difficulty,
    /// Hashrate estimate from autodiff (0 without autodiff data).
    pub hashrate: u64,
    /// User name from the login, `None` before the login.
    pub name: Option<String>,
    /// Shares accepted from it.
    pub stratum_shares: u32,
    /// Sidechain shares found by it.
    pub sidechain_shares: u32,
}

/// The stratum server's status for xkrd's console and `/status` (C++ `local/stratum` API file).
#[derive(Clone, Debug, PartialEq)]
pub struct StratumStats {
    /// Hashrate over 15 minutes (hashes per second).
    pub hashrate_15m: u64,
    /// Hashrate over one hour.
    pub hashrate_1h: u64,
    /// Hashrate over 24 hours.
    pub hashrate_24h: u64,
    /// All accepted hashes.
    pub total_hashes: u64,
    /// All accepted shares.
    pub total_stratum_shares: u64,
    /// When the last sidechain share was found (seconds since the epoch, 0 for none).
    pub last_share_found_time: u64,
    /// Sidechain shares found.
    pub shares_found: u32,
    /// Sidechain shares that could not be added.
    pub shares_failed: u32,
    /// Average effort of the shares found, in percent.
    pub average_effort: f64,
    /// Effort since the last share found, in percent.
    pub current_effort: f64,
    /// Open connections (all incoming).
    pub connections: u32,
    /// The miners.
    pub workers: Vec<WorkerStats>,
}

impl StratumStats {
    /// The C++ `local/stratum` object without the wallet fields (the node adds those):
    /// `workers` are `"addr,uptime,difficulty,hashrate,name"` texts.
    #[must_use]
    pub fn to_json(&self) -> Value {
        let workers: Vec<String> = self
            .workers
            .iter()
            .map(|w| {
                format!(
                    "{},{},{},{},{}",
                    w.addr,
                    w.uptime,
                    w.difficulty,
                    w.hashrate,
                    w.name.as_deref().unwrap_or("not logged in")
                )
            })
            .collect();
        json!({
            "hashrate_15m": self.hashrate_15m,
            "hashrate_1h": self.hashrate_1h,
            "hashrate_24h": self.hashrate_24h,
            "total_hashes": self.total_hashes,
            "total_stratum_shares": self.total_stratum_shares,
            "last_share_found_time": self.last_share_found_time,
            "shares_found": self.shares_found,
            "shares_failed": self.shares_failed,
            "average_effort": self.average_effort,
            "current_effort": self.current_effort,
            "connections": self.connections,
            "incoming_connections": self.connections,
            "workers": workers,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hashrate_windows() {
        let mut h = HashrateTracker::new(1000);
        h.update(3000, 1010);
        h.update(3000, 1010);
        h.update(6000, 1020);
        assert_eq!(h.hashrate(15 * 60), 12_000 / 20);
        // two hours later, only the 24 h window still sees the old shares
        h.update(0, 1020 + 7200);
        assert_eq!(h.hashrate(15 * 60), 0);
        assert_eq!(h.hashrate(DAY), 12_000 / 7220);
        let s = h.snapshot(Difficulty::from_u64(24_000));
        assert_eq!((s.total_hashes, s.total_stratum_shares), (12_000, 3));
        assert!((s.current_effort - 50.0).abs() < 1e-9);
    }

    #[test]
    fn found_and_failed_shares() {
        let mut h = HashrateTracker::new(0);
        h.update(500, 1);
        let f = h.share_found(500, 1000.0, 2);
        assert!((f.effort - 100.0).abs() < 1e-9);
        h.update(500, 2);
        let s = h.snapshot(Difficulty::from_u64(1000));
        assert_eq!((s.shares_found, s.last_share_found_time), (1, 2));
        assert!((s.average_effort - 100.0).abs() < 1e-9);
        let g = h.share_found(1000, 1000.0, 3);
        h.share_failed(g);
        let s = h.snapshot(Difficulty::from_u64(1000));
        assert_eq!(
            (s.shares_found, s.shares_failed, s.last_share_found_time),
            (1, 1, 2)
        );
        h.reset_share_counters();
        let s = h.snapshot(Difficulty::from_u64(1000));
        assert_eq!((s.shares_found, s.shares_failed), (0, 0));
        assert!(s.current_effort.abs() < 1e-9);
    }
}
