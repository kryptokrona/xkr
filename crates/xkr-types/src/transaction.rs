//! Transaction object model, field-for-field the C++ `cryptonote.h` structs
//! (`TransactionPrefix`, `Transaction`, `BaseInput`, `KeyInput`, `KeyOutput`), plus the
//! transaction version 2 additions of fork 14 (`docs/tx-v2.md`): tagged outputs
//! ([`TxOutputTarget::TaggedKey`]) and CLSAG signatures ([`ClsagSignature`]).

use crate::keys::{EcScalar, KeyImage, PublicKey, Signature};

/// Transaction version 2 (fork 14): tagged outputs and CLSAG signatures (`docs/tx-v2.md`).
/// Versions 0 and 1 are the CryptoNote layout.
pub const TX_VERSION_2: u8 = 2;

/// A transaction input. C++ `cryptonote::TransactionInput` (`boost::variant<BaseInput, KeyInput>`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TxInput {
    /// Coinbase input (wire tag `0xff`). C++ `BaseInput { uint32_t blockIndex; }`.
    Coinbase {
        /// Height (block index) of the block this coinbase belongs to.
        height: u32,
    },
    /// Spend of a key output (wire tag `0x02`). C++ `KeyInput`.
    Key {
        /// Amount of every ring member (CryptoNote v1 rings are per-amount).
        amount: u64,
        /// Ring member global output indexes **as on the wire**: the first entry is absolute,
        /// every following entry is a delta to the previous one. Use
        /// [`TxInput::absolute_output_indexes`] to get the absolute indexes.
        output_indexes: Vec<u32>,
        /// Key image of the real output.
        key_image: KeyImage,
    },
}

impl TxInput {
    /// Number of ring signature elements this input carries
    /// (C++ `getSignaturesCount` in `cryptonote_serialization.cpp`): 0 for coinbase,
    /// ring size for key inputs.
    #[must_use]
    pub fn signature_count(&self) -> usize {
        match self {
            Self::Coinbase { .. } => 0,
            Self::Key { output_indexes, .. } => output_indexes.len(),
        }
    }

    /// Amount carried by the input (0 for coinbase).
    #[must_use]
    pub fn amount(&self) -> u64 {
        match self {
            Self::Coinbase { .. } => 0,
            Self::Key { amount, .. } => *amount,
        }
    }

    /// Converts the packed (relative) indexes to absolute ones, with wrapping `u32` addition
    /// exactly like C++ `relativeOutputOffsetsToAbsolute` / `Core::validateTransaction`.
    /// Empty for coinbase inputs.
    #[must_use]
    pub fn absolute_output_indexes(&self) -> Vec<u32> {
        match self {
            Self::Coinbase { .. } => Vec::new(),
            Self::Key { output_indexes, .. } => relative_to_absolute(output_indexes),
        }
    }
}

/// C++ `relativeOutputOffsetsToAbsolute` (`cryptonote_format_utils.cpp`), with `u32` wrapping.
#[must_use]
pub fn relative_to_absolute(relative: &[u32]) -> Vec<u32> {
    let mut out = Vec::with_capacity(relative.len());
    let mut acc = 0u32;
    for (i, d) in relative.iter().enumerate() {
        acc = if i == 0 { *d } else { acc.wrapping_add(*d) };
        out.push(acc);
    }
    out
}

/// C++ `absolute_output_offsets_to_relative` (`cryptonote_format_utils.cpp`).
#[must_use]
pub fn absolute_to_relative(absolute: &[u32]) -> Vec<u32> {
    let mut out = Vec::with_capacity(absolute.len());
    for (i, a) in absolute.iter().enumerate() {
        out.push(if i == 0 {
            *a
        } else {
            a.wrapping_sub(absolute[i - 1])
        });
    }
    out
}

/// Output target. C++ `TransactionOutputTarget` (`boost::variant<KeyOutput>`), wire tag `0x02`,
/// plus the tagged key of transaction version 2 (wire tag `0x03`, `docs/tx-v2.md`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TxOutputTarget {
    /// One-time output public key (transaction versions 0 and 1).
    Key(PublicKey),
    /// One-time output public key with its view tag (transaction version 2, fork 14):
    /// `view_tag = H("view_tag" || 8rA || varint(output_index))[0]`, so a wallet only does the
    /// full key derivation for the ~1/256 of outputs whose tag matches.
    TaggedKey {
        /// One-time output public key.
        key: PublicKey,
        /// First byte of the view-tag hash.
        view_tag: u8,
    },
}

impl TxOutputTarget {
    /// The one-time output key.
    #[must_use]
    pub fn key(&self) -> &PublicKey {
        match self {
            Self::Key(k) | Self::TaggedKey { key: k, .. } => k,
        }
    }

    /// The one-time output key, mutable.
    pub fn key_mut(&mut self) -> &mut PublicKey {
        match self {
            Self::Key(k) | Self::TaggedKey { key: k, .. } => k,
        }
    }

    /// The view tag, `None` for an untagged (version 0/1) output.
    #[must_use]
    pub fn view_tag(&self) -> Option<u8> {
        match self {
            Self::Key(_) => None,
            Self::TaggedKey { view_tag, .. } => Some(*view_tag),
        }
    }
}

/// A transaction output. C++ `TransactionOutput`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TxOutput {
    /// Amount in atomic units (5 decimals).
    pub amount: u64,
    /// Destination.
    pub target: TxOutputTarget,
}

impl TxOutput {
    /// The one-time output key.
    #[must_use]
    pub fn key(&self) -> &PublicKey {
        self.target.key()
    }

    /// The one-time output key, mutable.
    pub fn key_mut(&mut self) -> &mut PublicKey {
        self.target.key_mut()
    }

    /// The view tag, `None` for an untagged (version 0/1) output.
    #[must_use]
    pub fn view_tag(&self) -> Option<u8> {
        self.target.view_tag()
    }
}

/// The CLSAG signature of one input of a version-2 transaction (single-key CLSAG without
/// `RingCT`, `docs/tx-v2.md`): one response `s_i` per ring member and the challenge `c1` of ring
/// member 0. The key image is the input's `key_image`. Wire size `32 * (ring size + 1)`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ClsagSignature {
    /// Responses `s_0 .. s_{n-1}`, in ring order.
    pub s: Vec<EcScalar>,
    /// Challenge `c_0` (Monero calls it `c1`).
    pub c1: EcScalar,
}

impl ClsagSignature {
    /// Size on the wire: `32 * (s.len() + 1)`.
    #[must_use]
    pub fn wire_size(&self) -> usize {
        32 * (self.s.len() + 1)
    }
}

/// C++ `TransactionPrefix`: everything that is hashed for the ring signature message.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TransactionPrefix {
    /// Transaction version (only 0 and 1 deserialize on mainnet; see `xkr-serialization`).
    pub version: u8,
    /// Unlock time: block index if `< CRYPTONOTE_MAX_BLOCK_NUMBER`, else unix timestamp.
    pub unlock_time: u64,
    /// Inputs.
    pub inputs: Vec<TxInput>,
    /// Outputs.
    pub outputs: Vec<TxOutput>,
    /// Raw `extra` bytes (see [`crate::extra`]).
    pub extra: Vec<u8>,
}

/// C++ `Transaction`: prefix plus one signature vector per input.
///
/// `signatures` mirrors the C++ deserializer exactly: it is **empty** for a transaction whose
/// only input is a single coinbase input (and for a transaction without inputs); otherwise it
/// has one entry per input, each holding `input.signature_count()` signatures (an empty entry
/// for a coinbase input mixed with key inputs).
///
/// Version 2 (fork 14) carries CLSAG signatures in `clsag_signatures` instead, with the same
/// shape rule (empty for no inputs or a single coinbase input, otherwise one per input) and
/// `signatures` empty. For versions 0 and 1 `clsag_signatures` is always empty.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Transaction {
    /// The signed prefix.
    pub prefix: TransactionPrefix,
    /// Legacy CryptoNote ring signatures (versions 0 and 1), one `Vec` per input.
    pub signatures: Vec<Vec<Signature>>,
    /// CLSAG signatures (version 2), one per input.
    pub clsag_signatures: Vec<ClsagSignature>,
}

impl Transaction {
    /// Whether the transaction uses the version-2 layout (tagged outputs, CLSAG signatures).
    #[must_use]
    pub fn is_v2(&self) -> bool {
        self.prefix.version >= TX_VERSION_2
    }

    /// Bytes of the signature section at the end of the blob (everything after the prefix):
    /// `64` per legacy signature, `32 * (ring size + 1)` per CLSAG signature.
    #[must_use]
    pub fn signature_section_len(&self) -> usize {
        let legacy: usize = self.signatures.iter().map(|s| s.len() * 64).sum();
        let clsag: usize = self
            .clsag_signatures
            .iter()
            .map(ClsagSignature::wire_size)
            .sum();
        legacy + clsag
    }

    /// True if this is a miner transaction: exactly one input and it is a coinbase input.
    #[must_use]
    pub fn is_coinbase(&self) -> bool {
        matches!(self.prefix.inputs.as_slice(), [TxInput::Coinbase { .. }])
    }

    /// Sum of output amounts with overflow detection (`None` on overflow).
    #[must_use]
    pub fn output_amount_checked(&self) -> Option<u64> {
        self.prefix
            .outputs
            .iter()
            .try_fold(0u64, |acc, o| acc.checked_add(o.amount))
    }

    /// Sum of key-input amounts with overflow detection (`None` on overflow).
    #[must_use]
    pub fn input_amount_checked(&self) -> Option<u64> {
        self.prefix
            .inputs
            .iter()
            .try_fold(0u64, |acc, i| acc.checked_add(i.amount()))
    }

    /// Ring size of the largest ring, i.e. `mixin + 1` as computed by C++ `Mixins::validate`
    /// (starts at 1, so a transaction without key inputs has ring size 1).
    #[must_use]
    pub fn max_ring_size(&self) -> usize {
        self.prefix
            .inputs
            .iter()
            .filter_map(|i| match i {
                TxInput::Key { output_indexes, .. } => Some(output_indexes.len()),
                TxInput::Coinbase { .. } => None,
            })
            .fold(1, usize::max)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_absolute_roundtrip() {
        let rel = [384_905u32, 1, 1, 119];
        let abs = relative_to_absolute(&rel);
        assert_eq!(abs, vec![384_905, 384_906, 384_907, 385_026]);
        assert_eq!(absolute_to_relative(&abs), rel.to_vec());
        assert_eq!(relative_to_absolute(&[]), Vec::<u32>::new());
    }

    #[test]
    fn tagged_targets() {
        let key = PublicKey([3; 32]);
        let mut o = TxOutput {
            amount: 5,
            target: TxOutputTarget::TaggedKey {
                key,
                view_tag: 0x7f,
            },
        };
        assert_eq!(o.key(), &key);
        assert_eq!(o.view_tag(), Some(0x7f));
        o.key_mut().0[0] = 9;
        assert_eq!(o.key().0[0], 9);
        let plain = TxOutput {
            amount: 5,
            target: TxOutputTarget::Key(key),
        };
        assert_eq!(plain.view_tag(), None);
    }

    #[test]
    fn signature_section_sizes() {
        let legacy = Transaction {
            signatures: vec![vec![Signature::ZERO; 3], vec![Signature::ZERO]],
            ..Transaction::default()
        };
        assert_eq!(legacy.signature_section_len(), 4 * 64);
        assert!(!legacy.is_v2());
        let v2 = Transaction {
            prefix: TransactionPrefix {
                version: TX_VERSION_2,
                ..TransactionPrefix::default()
            },
            signatures: Vec::new(),
            clsag_signatures: vec![ClsagSignature {
                s: vec![EcScalar::ZERO; 4],
                c1: EcScalar::ZERO,
            }],
        };
        assert_eq!(v2.signature_section_len(), 5 * 32);
        assert!(v2.is_v2());
    }

    #[test]
    fn coinbase_detection() {
        let mut tx = Transaction::default();
        assert!(!tx.is_coinbase());
        tx.prefix.inputs.push(TxInput::Coinbase { height: 5 });
        assert!(tx.is_coinbase());
        assert_eq!(tx.max_ring_size(), 1);
    }
}
