//! Decoy selection for wallets: C++ `Core::getRandomOutputs` /
//! `DatabaseBlockchainCache::getRandomOutsByAmount` (`/getrandom_outs`).
//!
//! Global indexes of `amount` are drawn uniformly without replacement (C++ `ShuffleGenerator`)
//! until `count` outputs are found that are unlocked at the top block and at least
//! `CRYPTONOTE_MINED_MONEY_UNLOCK_WINDOW` blocks deep, or the indexes run out. The result is
//! sorted by global index.
//!
//! Unlike C++, the draws per request are bounded ([`MAX_PROBES`]): an amount whose outputs are
//! almost all locked (for example by many outputs with far-future unlock times) would otherwise
//! make every request read every output of the amount. With the bound the answer differs from C++
//! only when fewer than one draw in [`MAX_PROBES`] finds a spendable output; it then holds the
//! outputs found so far.

use std::collections::HashMap;

use rand_core::RngCore;
use xkr_consensus::reward::is_spend_time_unlocked;
use xkr_crypto::os_rng;
use xkr_types::PublicKey;

use crate::chain::{Chain, unix_now};
use crate::error::ChainError;

/// Least number of outputs drawn per amount and request before giving up (at least
/// `100 x count`).
pub const MAX_PROBES: u64 = 100_000;

/// Draws distinct values from `0..n` in random order (a lazy Fisher-Yates shuffle over a
/// sparse swap map, like C++ `ShuffleGenerator`).
#[derive(Debug)]
struct Shuffle {
    n: u64,
    drawn: u64,
    swapped: HashMap<u64, u64>,
}

impl Shuffle {
    fn new(n: u64) -> Self {
        Self {
            n,
            drawn: 0,
            swapped: HashMap::new(),
        }
    }

    fn next(&mut self, rng: &mut impl RngCore) -> Option<u64> {
        if self.drawn >= self.n {
            return None;
        }
        let remaining = self.n - self.drawn;
        let pick = self.drawn + rng.next_u64() % remaining;
        let value = *self.swapped.get(&pick).unwrap_or(&pick);
        let first = *self.swapped.get(&self.drawn).unwrap_or(&self.drawn);
        self.swapped.insert(pick, first);
        self.drawn += 1;
        Some(value)
    }
}

impl Chain {
    /// Up to `count` random spendable outputs of `amount` as `(global index, key)`, sorted by
    /// index. `None` where C++ fails the request: the chain is shorter than two unlock
    /// windows, or no output of the amount qualifies.
    pub fn random_outputs(
        &self,
        amount: u64,
        count: usize,
    ) -> Result<Option<Vec<(u32, PublicKey)>>, ChainError> {
        if count == 0 {
            return Ok(Some(Vec::new()));
        }
        let reader = self.reader()?;
        let top = reader.top_height()?.unwrap_or(0);
        let window = u64::from(self.params().mined_money_unlock_window);
        // `upperBlockLimit = top - window` is a u32 in C++ and wraps below the window.
        if top >= window && top - window < window {
            return Ok(None);
        }
        let upper = top.saturating_sub(window);
        let top32 = u32::try_from(top).unwrap_or(u32::MAX);
        let now = unix_now();
        let total = reader.output_count(amount)?;

        let mut rng = os_rng();
        let mut shuffle = Shuffle::new(total);
        let budget = (count as u64).saturating_mul(100).max(MAX_PROBES);
        let mut probed = 0u64;
        let mut picked: Vec<(u32, PublicKey)> = Vec::with_capacity(count);
        while picked.len() < count {
            let want = count - picked.len();
            let mut batch = Vec::with_capacity(want);
            while batch.len() < want && probed < budget {
                match shuffle.next(&mut rng) {
                    Some(i) => batch.push((amount, i)),
                    None => break,
                }
                probed += 1;
            }
            if batch.is_empty() {
                break;
            }
            for (key, row) in batch.iter().zip(reader.outputs(&batch)?) {
                let Some(o) = row else {
                    return Err(ChainError::Corrupt(format!(
                        "output {} of amount {amount} missing",
                        key.1
                    )));
                };
                if !is_spend_time_unlocked(o.unlock_time, top32, now) || o.height > upper {
                    continue;
                }
                let index = u32::try_from(key.1)
                    .map_err(|_| ChainError::Corrupt("global index above u32".into()))?;
                picked.push((index, PublicKey(o.key)));
            }
        }
        if picked.is_empty() {
            return Ok(None);
        }
        picked.sort_unstable_by_key(|p| p.0);
        Ok(Some(picked))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shuffle_draws_each_value_once() {
        let mut rng = os_rng();
        let mut s = Shuffle::new(50);
        let mut seen: Vec<u64> = std::iter::from_fn(|| s.next(&mut rng)).collect();
        assert_eq!(seen.len(), 50);
        seen.sort_unstable();
        assert_eq!(seen, (0..50).collect::<Vec<_>>());
        assert!(Shuffle::new(0).next(&mut rng).is_none());
    }
}
