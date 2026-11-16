//! Fixed-size byte newtypes: hashes, curve points, scalars, key images, signatures.
//!
//! These mirror the C++ POD types in `include/crypto_types.h` (`crypto::Hash`,
//! `crypto::PublicKey`, ...): plain little-endian byte arrays with no invariants. Validation
//! (is this a valid curve point? a canonical scalar?) lives in `xkr-crypto`.

// `bytemuck::Pod`/`Zeroable` derives expand to `unsafe impl`s; they are sound here because every
// type is `#[repr(transparent)]` over a byte array (no padding, every bit pattern valid).
#![allow(unsafe_code)]

use core::fmt;
use core::str::FromStr;

/// Error returned by the hex `FromStr` impls.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum HexError {
    /// The string has the wrong number of characters.
    #[error("expected {expected} hex characters, got {got}")]
    WrongLength {
        /// Required number of hex characters.
        expected: usize,
        /// Number of characters in the input.
        got: usize,
    },
    /// The string contains a non-hex character.
    #[error("invalid hex character")]
    InvalidChar,
}

fn write_hex(f: &mut fmt::Formatter<'_>, bytes: &[u8]) -> fmt::Result {
    for b in bytes {
        write!(f, "{b:02x}")?;
    }
    Ok(())
}

macro_rules! byte_newtype {
    ($(#[$meta:meta])* $name:ident, $len:expr, redact_debug = $redact:expr) => {
        $(#[$meta])*
        #[derive(
            Clone,
            Copy,
            PartialEq,
            Eq,
            core::hash::Hash,
            PartialOrd,
            Ord,
            bytemuck::Pod,
            bytemuck::Zeroable,
        )]
        #[repr(transparent)]
        pub struct $name(pub [u8; $len]);

        impl $name {
            /// Size in bytes.
            pub const LEN: usize = $len;
            /// All-zero value (the C++ `boost::value_initialized<T>()`).
            pub const ZERO: Self = Self([0u8; $len]);

            /// Wraps raw bytes.
            #[must_use]
            pub const fn from_bytes(bytes: [u8; $len]) -> Self {
                Self(bytes)
            }

            /// Borrows the raw bytes.
            #[must_use]
            pub const fn as_bytes(&self) -> &[u8; $len] {
                &self.0
            }

            /// Copies out the raw bytes.
            #[must_use]
            pub const fn to_bytes(self) -> [u8; $len] {
                self.0
            }

            /// Wraps a slice of exactly `LEN` bytes, `None` otherwise.
            #[must_use]
            pub fn from_slice(bytes: &[u8]) -> Option<Self> {
                <[u8; $len]>::try_from(bytes).ok().map(Self)
            }

            /// True if every byte is zero.
            #[must_use]
            pub fn is_zero(&self) -> bool {
                self.0.iter().all(|b| *b == 0)
            }

            /// Lower-case hex encoding.
            #[must_use]
            pub fn to_hex(&self) -> String {
                hex::encode(self.0)
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::ZERO
            }
        }

        impl AsRef<[u8]> for $name {
            fn as_ref(&self) -> &[u8] {
                &self.0
            }
        }

        impl From<[u8; $len]> for $name {
            fn from(bytes: [u8; $len]) -> Self {
                Self(bytes)
            }
        }

        impl From<$name> for [u8; $len] {
            fn from(value: $name) -> Self {
                value.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write_hex(f, &self.0)
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                if $redact {
                    write!(f, "{}(<redacted>)", stringify!($name))
                } else {
                    write!(f, "{}(", stringify!($name))?;
                    write_hex(f, &self.0)?;
                    f.write_str(")")
                }
            }
        }

        impl FromStr for $name {
            type Err = HexError;

            fn from_str(s: &str) -> Result<Self, HexError> {
                if s.len() != 2 * $len {
                    return Err(HexError::WrongLength { expected: 2 * $len, got: s.len() });
                }
                let mut out = [0u8; $len];
                hex::decode_to_slice(s, &mut out).map_err(|_| HexError::InvalidChar)?;
                Ok(Self(out))
            }
        }
    };
}

byte_newtype!(
    /// 32-byte Keccak (`cn_fast_hash`) or CryptoNight digest. C++ `crypto::Hash`.
    Hash,
    32,
    redact_debug = false
);
byte_newtype!(
    /// Compressed Ed25519 point. C++ `crypto::PublicKey`.
    PublicKey,
    32,
    redact_debug = false
);
byte_newtype!(
    /// Ed25519 scalar (little-endian). C++ `crypto::SecretKey`. `Debug` is redacted;
    /// `Display` still prints hex because wallets and tests need it.
    SecretKey,
    32,
    redact_debug = true
);
byte_newtype!(
    /// Key image `x * Hp(P)`. C++ `crypto::KeyImage`.
    KeyImage,
    32,
    redact_debug = false
);
byte_newtype!(
    /// ECDH derivation `8 * r * A`. C++ `crypto::KeyDerivation`.
    KeyDerivation,
    32,
    redact_debug = false
);
byte_newtype!(
    /// One ring-signature element `(c, r)`, 2 x 32-byte scalars. C++ `crypto::Signature`.
    Signature,
    64,
    redact_debug = false
);
byte_newtype!(
    /// A public 32-byte little-endian scalar (a CLSAG `s_i` or `c1`, `docs/tx-v2.md`). Not
    /// secret, so `Debug` prints it. Canonical form (`< l`) is checked by the verifier.
    EcScalar,
    32,
    redact_debug = false
);

impl Signature {
    /// The challenge scalar `c` (first 32 bytes).
    #[must_use]
    pub fn c(&self) -> [u8; 32] {
        let mut out = [0u8; 32];
        out.copy_from_slice(&self.0[..32]);
        out
    }

    /// The response scalar `r` (last 32 bytes).
    #[must_use]
    pub fn r(&self) -> [u8; 32] {
        let mut out = [0u8; 32];
        out.copy_from_slice(&self.0[32..]);
        out
    }

    /// Builds a signature from `c || r`.
    #[must_use]
    pub fn from_parts(c: &[u8; 32], r: &[u8; 32]) -> Self {
        let mut out = [0u8; 64];
        out[..32].copy_from_slice(c);
        out[32..].copy_from_slice(r);
        Self(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_roundtrip() {
        let s = "7fb97df81221dd1366051b2d0bc7f49c66c22ac4431d879c895b06d66ef66f4c";
        let h: Hash = s.parse().unwrap();
        assert_eq!(h.to_string(), s);
        assert_eq!(h.as_bytes()[0], 0x7f);
        assert_eq!(format!("{h:?}"), format!("Hash({s})"));
    }

    #[test]
    fn hex_errors() {
        assert_eq!(
            "00".parse::<Hash>(),
            Err(HexError::WrongLength {
                expected: 64,
                got: 2
            })
        );
        let bad = "zz".repeat(32);
        assert_eq!(bad.parse::<Hash>(), Err(HexError::InvalidChar));
    }

    #[test]
    fn secret_key_debug_redacted() {
        let k = SecretKey([7u8; 32]);
        assert_eq!(format!("{k:?}"), "SecretKey(<redacted>)");
    }

    #[test]
    fn signature_parts() {
        let c = [1u8; 32];
        let r = [2u8; 32];
        let s = Signature::from_parts(&c, &r);
        assert_eq!(s.c(), c);
        assert_eq!(s.r(), r);
        assert_eq!(bytemuck::bytes_of(&s).len(), 64);
    }
}
