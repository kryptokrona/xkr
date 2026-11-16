//! The miner wallet of a share (`Wallet`, `src/wallet.cpp` / `src/wallet.h`): the public spend
//! and view keys, ordered the C++ way and shown as a `SEKR…` address.

use core::cmp::Ordering;
use core::fmt;

use xkr_crypto::check_key;
use xkr_types::PublicKey;
use xkr_wallet_core::Address;

use crate::order::hash_cmp;

/// A payout wallet: public spend key `B` and public view key `A`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct MinerWallet {
    /// Public spend key.
    pub spend: PublicKey,
    /// Public view key.
    pub view: PublicKey,
}

impl MinerWallet {
    /// C++ `Wallet::assign(spend, view, …)`: both keys must decode as curve points
    /// (`ge_frombytes_vartime`).
    #[must_use]
    pub fn assign(spend: PublicKey, view: PublicKey) -> Option<Self> {
        (check_key(&spend) && check_key(&view)).then_some(Self { spend, view })
    }

    /// Parses a standard `SEKR…` / `Xkr…` address (C++ `Wallet::decode`; integrated addresses
    /// are refused, a payout has no payment id).
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        let a = Address::parse(text).ok()?;
        a.payment_id.is_none().then_some(Self {
            spend: a.spend,
            view: a.view,
        })
    }

    /// The `SEKR…` address (C++ `Wallet::encode` with the canonical prefix).
    #[must_use]
    pub fn address(&self) -> String {
        Address::new(self.spend, self.view).encode()
    }
}

/// C++ `Wallet::operator<` (`src/wallet.h`): spend keys first, then view keys, each in the hash
/// order of [`hash_cmp`].
impl Ord for MinerWallet {
    fn cmp(&self, other: &Self) -> Ordering {
        hash_cmp(
            &xkr_types::Hash(self.spend.0),
            &xkr_types::Hash(other.spend.0),
        )
        .then_with(|| {
            hash_cmp(
                &xkr_types::Hash(self.view.0),
                &xkr_types::Hash(other.view.0),
            )
        })
    }
}

impl PartialOrd for MinerWallet {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl fmt::Display for MinerWallet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.address())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The donation address of `xkrd` (`crates/xkrd/src/fee.rs`) round-trips.
    #[test]
    fn address_round_trip_and_order() {
        let text = "SEKReSw2cP5bv43mgSmUq2FqLRopwDqrseW6CHxDJuAfRnxaJNvzd2dY6QExxP6JER6RBHcLMYi8ESrzV9M8xPKkFNjSkcBJZGc";
        {
            let w = MinerWallet::parse(text).unwrap();
            assert_eq!(w.address(), text);
            assert_eq!(MinerWallet::assign(w.spend, w.view), Some(w));
            let swapped = MinerWallet {
                spend: w.view,
                view: w.spend,
            };
            assert_eq!(
                w.cmp(&swapped),
                hash_cmp(&xkr_types::Hash(w.spend.0), &xkr_types::Hash(w.view.0))
            );
        }
        // Not a point: y = 2^255 - 1 is above the field prime.
        let mut bad = [0xff; 32];
        bad[31] = 0x7f;
        let g = PublicKey(
            xkr_crypto::EdwardsPoint::mul_base(&xkr_crypto::Scalar::ONE)
                .compress()
                .to_bytes(),
        );
        assert_eq!(MinerWallet::assign(PublicKey(bad), g), None);
        assert!(MinerWallet::assign(g, g).is_some());
    }
}
