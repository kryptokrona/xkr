//! Per-connection token buckets for incoming messages.
//!
//! The C++ node answers every timed sync, chain request, pool request and missing-transaction
//! request and processes every relay, however often a peer sends them. Each connection task
//! keeps one [`RateLimiter`]: a message beyond its bucket ([`crate::constants::rate_limits`])
//! is dropped and costs a strike; a peer out of strikes is a protocol violation (the node bans
//! it). Answers to requests we sent are exempt. Plain data; time is passed in.

use std::time::{Duration, Instant};

use crate::constants::rate_limits::{
    self, NEW_TRANSACTION_BYTES_BURST, NEW_TRANSACTION_BYTES_PER_SECOND, Rate,
};
use crate::wire::{LegacyCommand, ProtocolMessage};

/// A token bucket kept as time credit: full = `burst x every`, one token costs `every`.
#[derive(Debug, Clone)]
struct Bucket {
    every: Duration,
    capacity: Duration,
    credit: Duration,
    last: Instant,
}

impl Bucket {
    fn new(rate: Rate, now: Instant) -> Self {
        let capacity = rate.every.saturating_mul(rate.burst);
        Self {
            every: rate.every,
            capacity,
            credit: capacity,
            last: now,
        }
    }

    /// A bucket of bytes: `burst` bytes, `per_second` bytes per second.
    fn bytes(burst: u32, per_second: u32, now: Instant) -> Self {
        let every = Duration::from_nanos(1_000_000_000 / u64::from(per_second.max(1)));
        Self::new(Rate { burst, every }, now)
    }

    /// Takes `units` tokens if there are enough.
    fn take(&mut self, now: Instant, units: u32) -> bool {
        let elapsed = now.saturating_duration_since(self.last);
        if now > self.last {
            self.last = now;
        }
        self.credit = self.credit.saturating_add(elapsed).min(self.capacity);
        let cost = self.every.saturating_mul(units);
        if self.credit >= cost {
            self.credit -= cost;
            true
        } else {
            false
        }
    }
}

/// What to do with one incoming message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Process it.
    Allow,
    /// Ignore it (it cost a strike).
    Drop,
    /// The peer is out of strikes: close and ban.
    Violation,
}

/// The buckets of one connection.
#[derive(Debug, Clone)]
pub struct RateLimiter {
    timed_sync: Bucket,
    ping: Bucket,
    blocks: Bucket,
    txs: Bucket,
    tx_bytes: Bucket,
    request_chain: Bucket,
    get_objects: Bucket,
    tx_pool: Bucket,
    missing_txs: Bucket,
    other: Bucket,
    strikes: Bucket,
}

impl RateLimiter {
    /// Full buckets.
    #[must_use]
    pub fn new(now: Instant) -> Self {
        Self {
            timed_sync: Bucket::new(rate_limits::TIMED_SYNC, now),
            ping: Bucket::new(rate_limits::PING, now),
            blocks: Bucket::new(rate_limits::NEW_BLOCK, now),
            txs: Bucket::new(rate_limits::NEW_TRANSACTIONS, now),
            tx_bytes: Bucket::bytes(
                NEW_TRANSACTION_BYTES_BURST,
                NEW_TRANSACTION_BYTES_PER_SECOND,
                now,
            ),
            request_chain: Bucket::new(rate_limits::REQUEST_CHAIN, now),
            get_objects: Bucket::new(rate_limits::REQUEST_GET_OBJECTS, now),
            tx_pool: Bucket::new(rate_limits::REQUEST_TX_POOL, now),
            missing_txs: Bucket::new(rate_limits::MISSING_TXS, now),
            other: Bucket::new(rate_limits::OTHER, now),
            strikes: Bucket::new(rate_limits::STRIKES, now),
        }
    }

    /// An admin request from the peer (`COMMAND_TIMED_SYNC`, `COMMAND_PING`, debug commands).
    pub fn request(&mut self, command: LegacyCommand, now: Instant) -> Verdict {
        let ok = match command {
            LegacyCommand::TimedSync => self.timed_sync.take(now, 1),
            LegacyCommand::Ping => self.ping.take(now, 1),
            _ => self.other.take(now, 1),
        };
        self.verdict(ok, now)
    }

    /// A block-chain notification. `solicited`: it answers a request we sent (always allowed).
    pub fn notification(
        &mut self,
        msg: &ProtocolMessage,
        solicited: bool,
        now: Instant,
    ) -> Verdict {
        if solicited {
            return Verdict::Allow;
        }
        let ok = match msg {
            ProtocolMessage::NewBlock(_) | ProtocolMessage::NewLiteBlock(_) => {
                self.blocks.take(now, 1)
            }
            ProtocolMessage::NewTransactions(m) => {
                let bytes = m.txs.iter().map(Vec::len).sum::<usize>();
                let count_ok = self.txs.take(now, 1);
                let bytes_ok = self
                    .tx_bytes
                    .take(now, u32::try_from(bytes).unwrap_or(u32::MAX));
                count_ok && bytes_ok
            }
            ProtocolMessage::RequestChain(_) => self.request_chain.take(now, 1),
            ProtocolMessage::RequestGetObjects(_) => self.get_objects.take(now, 1),
            ProtocolMessage::RequestTxPool(_) => self.tx_pool.take(now, 1),
            ProtocolMessage::MissingTxs(_) => self.missing_txs.take(now, 1),
            ProtocolMessage::ResponseGetObjects(_) | ProtocolMessage::ResponseChainEntry(_) => {
                self.other.take(now, 1)
            }
        };
        self.verdict(ok, now)
    }

    /// A message with an unknown command id.
    pub fn unknown(&mut self, now: Instant) -> Verdict {
        let ok = self.other.take(now, 1);
        self.verdict(ok, now)
    }

    fn verdict(&mut self, ok: bool, now: Instant) -> Verdict {
        if ok {
            Verdict::Allow
        } else if self.strikes.take(now, 1) {
            Verdict::Drop
        } else {
            Verdict::Violation
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::{NewTransactions, RequestTxPool};

    fn tx_pool() -> ProtocolMessage {
        ProtocolMessage::RequestTxPool(RequestTxPool::default())
    }

    #[test]
    fn burst_then_drop_then_refill() {
        let t0 = Instant::now();
        let mut rl = RateLimiter::new(t0);
        for _ in 0..rate_limits::TIMED_SYNC.burst {
            assert_eq!(rl.request(LegacyCommand::TimedSync, t0), Verdict::Allow);
        }
        assert_eq!(rl.request(LegacyCommand::TimedSync, t0), Verdict::Drop);
        // One token back after `every`.
        let t1 = t0 + rate_limits::TIMED_SYNC.every;
        assert_eq!(rl.request(LegacyCommand::TimedSync, t1), Verdict::Allow);
        assert_eq!(rl.request(LegacyCommand::TimedSync, t1), Verdict::Drop);
        // Buckets are independent.
        assert_eq!(rl.request(LegacyCommand::Ping, t1), Verdict::Allow);
    }

    /// One pool request per 30 s; a burst runs out of strikes and becomes a violation.
    #[test]
    fn pool_request_burst_is_a_violation() {
        let t0 = Instant::now();
        let mut rl = RateLimiter::new(t0);
        assert_eq!(rl.notification(&tx_pool(), false, t0), Verdict::Allow);
        for _ in 0..rate_limits::STRIKES.burst {
            assert_eq!(rl.notification(&tx_pool(), false, t0), Verdict::Drop);
        }
        assert_eq!(rl.notification(&tx_pool(), false, t0), Verdict::Violation);
        // Strikes come back with time; so does the pool request.
        let later = t0 + rate_limits::REQUEST_TX_POOL.every;
        assert_eq!(rl.notification(&tx_pool(), false, later), Verdict::Allow);
    }

    /// Answers to our own requests are never counted.
    #[test]
    fn solicited_messages_are_exempt() {
        let t0 = Instant::now();
        let mut rl = RateLimiter::new(t0);
        let big = ProtocolMessage::NewTransactions(NewTransactions {
            txs: vec![vec![0u8; 1024 * 1024]; 64],
        });
        for _ in 0..1000 {
            assert_eq!(rl.notification(&big, true, t0), Verdict::Allow);
        }
        // Unsolicited, 64 MiB at once is over the 32 MiB byte burst.
        assert_eq!(rl.notification(&big, false, t0), Verdict::Drop);
        let small = ProtocolMessage::NewTransactions(NewTransactions {
            txs: vec![vec![0u8; 2000]],
        });
        assert_eq!(rl.notification(&small, false, t0), Verdict::Allow);
    }
}
