//! Kryptokrona addresses.
//!
//! A standard address is `encode_addr(prefix, spend_public || view_public)` (99 characters,
//! `SEKR…` for [`ADDRESS_PREFIX`]). The C++ node accepts the configured prefix and
//! [`ADDRESS_PREFIX_ALT`] (`Xkr…`), which encode the same keys
//! (`Currency::parseAccountAddressString`). Integrated addresses (wallet-backend-js) put the
//! payment id as 64 ASCII hex characters in front of the keys (187 characters).

use std::fmt;
use std::str::FromStr;

use xkr_consensus::params::{
    CRYPTONOTE_PUBLIC_ADDRESS_BASE58_PREFIX, CRYPTONOTE_PUBLIC_ADDRESS_BASE58_PREFIX_ALT,
};
use xkr_crypto::check_key;
use xkr_types::{Hash, PublicKey};

use crate::base58;

/// `CRYPTONOTE_PUBLIC_ADDRESS_BASE58_PREFIX` (`SEKR…`). The testnet build uses it too.
pub const ADDRESS_PREFIX: u64 = CRYPTONOTE_PUBLIC_ADDRESS_BASE58_PREFIX;
/// `CRYPTONOTE_PUBLIC_ADDRESS_BASE58_PREFIX_ALT` (`Xkr…`).
pub const ADDRESS_PREFIX_ALT: u64 = CRYPTONOTE_PUBLIC_ADDRESS_BASE58_PREFIX_ALT;

/// Why an address does not parse.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum AddressError {
    /// Not base58, or the checksum does not match.
    #[error("address is not valid base58 or its checksum does not match")]
    Encoding,
    /// Neither the `SEKR` nor the `Xkr` prefix.
    #[error("wrong address prefix {0}")]
    WrongPrefix(u64),
    /// The payload is neither 64 (standard) nor 128 (integrated) bytes.
    #[error("address payload of {0} bytes")]
    WrongLength(usize),
    /// The payment id of an integrated address is not hex.
    #[error("integrated address payment id is not hex")]
    PaymentId,
    /// A key is not a valid curve point (`check_key`).
    #[error("address key is not a valid public key")]
    InvalidKey,
}

/// A parsed address.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Address {
    /// Public spend key `B`.
    pub spend: PublicKey,
    /// Public view key `A`.
    pub view: PublicKey,
    /// Payment id of an integrated address.
    pub payment_id: Option<Hash>,
    /// Prefix it was written with (re-encoding keeps it).
    pub prefix: u64,
}

impl Address {
    /// A standard `SEKR…` address for the two public keys.
    #[must_use]
    pub const fn new(spend: PublicKey, view: PublicKey) -> Self {
        Self {
            spend,
            view,
            payment_id: None,
            prefix: ADDRESS_PREFIX,
        }
    }

    /// The same keys with a payment id (an integrated address).
    #[must_use]
    pub const fn integrated(self, payment_id: Hash) -> Self {
        Self {
            payment_id: Some(payment_id),
            ..self
        }
    }

    /// The same keys without a payment id.
    #[must_use]
    pub const fn standard(self) -> Self {
        Self {
            payment_id: None,
            ..self
        }
    }

    /// Parses a standard or integrated address with either accepted prefix.
    pub fn parse(text: &str) -> Result<Self, AddressError> {
        let (prefix, data) = base58::decode_addr(text.trim()).ok_or(AddressError::Encoding)?;
        if prefix != ADDRESS_PREFIX && prefix != ADDRESS_PREFIX_ALT {
            return Err(AddressError::WrongPrefix(prefix));
        }
        let (payment_id, keys) = match data.len() {
            64 => (None, &data[..]),
            128 => {
                let text = std::str::from_utf8(&data[..64]).map_err(|_| AddressError::PaymentId)?;
                let bytes = hex::decode(text).map_err(|_| AddressError::PaymentId)?;
                (
                    Some(Hash::from_slice(&bytes).ok_or(AddressError::PaymentId)?),
                    &data[64..],
                )
            }
            n => return Err(AddressError::WrongLength(n)),
        };
        let spend = PublicKey::from_slice(&keys[..32]).ok_or(AddressError::InvalidKey)?;
        let view = PublicKey::from_slice(&keys[32..64]).ok_or(AddressError::InvalidKey)?;
        if !check_key(&spend) || !check_key(&view) {
            return Err(AddressError::InvalidKey);
        }
        Ok(Self {
            spend,
            view,
            payment_id,
            prefix,
        })
    }

    /// The base58 text.
    #[must_use]
    pub fn encode(&self) -> String {
        let mut data = Vec::with_capacity(128);
        if let Some(pid) = &self.payment_id {
            data.extend_from_slice(hex::encode(pid.0).as_bytes());
        }
        data.extend_from_slice(&self.spend.0);
        data.extend_from_slice(&self.view.0);
        base58::encode_addr(self.prefix, &data)
    }
}

impl fmt::Display for Address {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.encode())
    }
}

impl FromStr for Address {
    type Err = AddressError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// From kryptokrona-wallet-backend-js `tests/tests.ts`.
    const ADDR: &str = "SEKReVqaRaqWfBg4Fypb6dVNLx27FXw8icGgA3EUKKhVUdB7EVJR1iia3fVTxqw5RTiFJN4DU98KuBXvCoG6f9qx5XRtqnc883h";
    const PID: &str = "7bedddf2b150cf247a42dde6a999e73b7f297d67d2ddfadbc331ea54a526ba1b";
    const INTEGRATED: &str = "SEKReU5xRJsHnvwLSGzSLfHdJDPhNiQ8hHnvvr1FbyG4HvvGRUeRtorAGNFpKrhzJwJ8Dg26FMRJQHxWbWdCpCysHTQYZsk6JdoWfBg4Fypb6dVNLx27FXw8icGgA3EUKKhVUdB7EVJR1iia3fVTxqw5RTiFJN4DU98KuBXvCoG6f9qx5XRtqppRa7E";

    #[test]
    fn standard_round_trip() {
        let a = Address::parse(ADDR).unwrap();
        assert_eq!(a.prefix, ADDRESS_PREFIX);
        assert!(a.payment_id.is_none());
        assert_eq!(a.encode(), ADDR);
        // The testnet compose miner address.
        let m = "SEKReYwbPp2NBcX5TxYoxrDhPdzPa7kXaXFVW6WqoNTaEkk8CAqM4HCRzkyKtMR2czK1Bma5w5HBM2xHtCb1zwfp9TSS2UitA67";
        assert_eq!(Address::parse(m).unwrap().to_string(), m);
    }

    #[test]
    fn integrated_matches_wallet_backend() {
        let a = Address::parse(ADDR).unwrap();
        let pid: Hash = PID.parse().unwrap();
        assert_eq!(a.integrated(pid).encode(), INTEGRATED);
        let back = Address::parse(INTEGRATED).unwrap();
        assert_eq!(back.payment_id, Some(pid));
        assert_eq!(back.standard(), a);
    }

    #[test]
    fn alt_prefix_has_same_keys() {
        let a = Address::parse(ADDR).unwrap();
        let alt = Address {
            prefix: ADDRESS_PREFIX_ALT,
            ..a
        };
        let text = alt.encode();
        assert!(text.starts_with("Xkr"), "{text}");
        let back = Address::parse(&text).unwrap();
        assert_eq!((back.spend, back.view), (a.spend, a.view));
        assert!(matches!(
            Address::parse(&base58::encode_addr(1, &[0u8; 64])),
            Err(AddressError::WrongPrefix(1))
        ));
    }
}
