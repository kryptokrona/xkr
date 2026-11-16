//! Transaction (de)serialization: C++ `serialize(TransactionPrefix&)`, `serialize(Transaction&)`,
//! `serialize(BaseTransaction&)`, `serialize(TransactionInput&)`, `serialize(KeyInput&)`,
//! `serialize(TransactionOutput&)` in `src/cryptonote_core/cryptonote_serialization.cpp`.
//!
//! Plus transaction version 2 (fork 14, `docs/tx-v2.md`): outputs are tagged keys (tag `0x03`,
//! key, one view-tag byte) and each key input is followed by a CLSAG signature
//! (`s_0 .. s_{n-1} || c1`, 32 bytes each). Versions 0 and 1 are byte-for-byte the C++ format;
//! a version-1 output with tag `0x03` is still an unknown tag.

use xkr_types::{
    ClsagSignature, EcScalar, KeyImage, PublicKey, Signature, TX_VERSION_2, Transaction,
    TransactionPrefix, TxInput, TxOutput, TxOutputTarget,
};

use crate::error::{DecodeError, EncodeError};
use crate::reader::Reader;
use crate::varint::write_varint;

/// C++ `CURRENT_TRANSACTION_VERSION`: the version this crate's callers build by default
/// (templates, wallets). The deserializer accepts up to [`MAX_TRANSACTION_VERSION`].
pub const CURRENT_TRANSACTION_VERSION: u8 = 1;
/// Transaction version 2 (fork 14): tagged outputs and CLSAG signatures.
pub const TRANSACTION_VERSION_2: u8 = TX_VERSION_2;
/// Highest transaction version the deserializer accepts (C++ v1.2.2: 1). Whether a version is
/// valid at a height is consensus (`xkr-consensus` fork table), not decoding.
pub const MAX_TRANSACTION_VERSION: u8 = TRANSACTION_VERSION_2;
/// Variant tag of `BaseInput` (coinbase).
pub const TAG_BASE_INPUT: u8 = 0xff;
/// Variant tag of `KeyInput` and of `KeyOutput`.
pub const TAG_KEY: u8 = 0x02;
/// Variant tag of the tagged key output of transaction version 2 (Monero's
/// `txout_to_tagged_key` uses the same value).
pub const TAG_TAGGED_KEY: u8 = 0x03;

/// Smallest encoding of a key input: tag, one-byte amount, one-byte offset count, no offsets,
/// key image. Used to bound `vin` once a transaction has more than one input (then every input
/// must be a key input, see [`read_prefix`]).
const MIN_KEY_INPUT_WIRE_SIZE: usize = 1 + 1 + 1 + 32;
/// Smallest encoding of an output: one-byte amount, tag, key (the version-2 tagged key is one
/// byte longer). Any `vout` count above `remaining / 34` fails at end of input anyway; checking it
/// first keeps `Vec::with_capacity` proportional to the bytes that can really follow.
const MIN_OUTPUT_WIRE_SIZE: usize = 1 + 1 + 32;
/// Consensus limit on the wire size of a merge-mining parent block (C++ `validateBlock`:
/// `getObjectBinarySize(parentBlock) > 2048` is `PARENT_BLOCK_SIZE_TOO_BIG`; `xkr-consensus`
/// `BlockError::ParentBlockSizeTooBig`). Every input of the parent's miner transaction encodes
/// to at least 2 bytes, so a parent miner transaction with more than `2048 / 2` inputs belongs
/// to a block that no node accepts.
pub const MAX_PARENT_BLOCK_SIZE: usize = 2048;

pub(crate) fn write_prefix(out: &mut Vec<u8>, p: &TransactionPrefix) {
    write_varint(out, u64::from(p.version));
    write_varint(out, p.unlock_time);
    write_varint(out, p.inputs.len() as u64);
    for input in &p.inputs {
        match input {
            TxInput::Coinbase { height } => {
                out.push(TAG_BASE_INPUT);
                write_varint(out, u64::from(*height));
            }
            TxInput::Key {
                amount,
                output_indexes,
                key_image,
            } => {
                out.push(TAG_KEY);
                write_varint(out, *amount);
                write_varint(out, output_indexes.len() as u64);
                for idx in output_indexes {
                    write_varint(out, u64::from(*idx));
                }
                out.extend_from_slice(&key_image.0);
            }
        }
    }
    write_varint(out, p.outputs.len() as u64);
    for o in &p.outputs {
        write_varint(out, o.amount);
        match &o.target {
            TxOutputTarget::Key(k) => {
                out.push(TAG_KEY);
                out.extend_from_slice(&k.0);
            }
            TxOutputTarget::TaggedKey { key, view_tag } => {
                out.push(TAG_TAGGED_KEY);
                out.extend_from_slice(&key.0);
                out.push(*view_tag);
            }
        }
    }
    write_varint(out, p.extra.len() as u64);
    out.extend_from_slice(&p.extra);
}

/// Reads the prefix fields. `base_transaction = true` is C++ `serialize(BaseTransaction&)`: no
/// version check and an extra ignored varint for version >= 2 (and always the version-1 output
/// layout: the merge-mining parent's miner transaction is not an XKR transaction).
pub(crate) fn read_prefix(
    r: &mut Reader<'_>,
    base_transaction: bool,
) -> Result<TransactionPrefix, DecodeError> {
    let version = r.varint_u8("transaction version")?;
    if !base_transaction && version > MAX_TRANSACTION_VERSION {
        return Err(DecodeError::WrongTransactionVersion(version));
    }
    let tagged_outputs = !base_transaction && version >= TRANSACTION_VERSION_2;
    let unlock_time = r.varint_u64("unlock_time")?;

    let vin_offset = r.pos();
    let n_inputs = r.count(2, "vin")?;
    if base_transaction {
        // The merge-mining parent's miner transaction belongs to another chain; it is
        // bounded by the 2048-byte parent block limit.
        if n_inputs > MAX_PARENT_BLOCK_SIZE / 2 {
            return Err(DecodeError::LengthTooLarge {
                offset: vin_offset,
                what: "parent miner transaction vin",
                len: n_inputs as u64,
                remaining: MAX_PARENT_BLOCK_SIZE,
            });
        }
    } else if n_inputs > 1 {
        // A coinbase input is legal only as the single input of a miner transaction (C++
        // `validateBlock`: exactly one `BaseInput`; `validateTransactionInputs` and the pool
        // refuse a `BaseInput` in any other transaction). So with two or more inputs every
        // input is a key input of at least `MIN_KEY_INPUT_WIRE_SIZE` bytes; without this a
        // 2-byte coinbase input decoded to ~90 bytes of heap (about 44x the wire size).
        let fits = n_inputs
            .checked_mul(MIN_KEY_INPUT_WIRE_SIZE)
            .is_some_and(|need| need <= r.remaining());
        if !fits {
            return Err(DecodeError::LengthTooLarge {
                offset: vin_offset,
                what: "vin",
                len: n_inputs as u64,
                remaining: r.remaining(),
            });
        }
    }
    let mut inputs = Vec::with_capacity(n_inputs);
    for i in 0..n_inputs {
        let offset = r.pos();
        let tag = r.u8_raw("input tag")?;
        match tag {
            TAG_BASE_INPUT => {
                if n_inputs > 1 && !base_transaction {
                    return Err(DecodeError::CoinbaseInputInSignedTransaction(i));
                }
                let height = r.varint_u32("base input height")?;
                inputs.push(TxInput::Coinbase { height });
            }
            TAG_KEY => {
                let amount = r.varint_u64("key input amount")?;
                let n = r.count(1, "key_offsets")?;
                let mut output_indexes = Vec::with_capacity(n);
                for _ in 0..n {
                    output_indexes.push(r.varint_u32("key offset")?);
                }
                let key_image = KeyImage(r.array32("key image")?);
                inputs.push(TxInput::Key {
                    amount,
                    output_indexes,
                    key_image,
                });
            }
            _ => return Err(DecodeError::UnknownInputTag { tag, offset }),
        }
    }

    let n_outputs = r.count(MIN_OUTPUT_WIRE_SIZE, "vout")?;
    let mut outputs = Vec::with_capacity(n_outputs);
    for _ in 0..n_outputs {
        let amount = r.varint_u64("output amount")?;
        let offset = r.pos();
        let tag = r.u8_raw("output tag")?;
        let target = match (tag, tagged_outputs) {
            (TAG_KEY, false) => TxOutputTarget::Key(PublicKey(r.array32("output key")?)),
            (TAG_TAGGED_KEY, true) => {
                let key = PublicKey(r.array32("output key")?);
                let view_tag = r.u8_raw("view tag")?;
                TxOutputTarget::TaggedKey { key, view_tag }
            }
            // A version-1 output under a version-2 version byte: not a version-2 transaction.
            (TAG_KEY, true) => return Err(DecodeError::WrongTransactionVersion(version)),
            _ => return Err(DecodeError::UnknownOutputTag { tag, offset }),
        };
        outputs.push(TxOutput { amount, target });
    }

    let extra_len = r.count(1, "extra")?;
    let extra = r.bytes(extra_len, "extra")?.to_vec();

    if base_transaction && version >= 2 {
        let _ignored = r.varint_u64("base transaction ignored field")?;
    }

    Ok(TransactionPrefix {
        version,
        unlock_time,
        inputs,
        outputs,
        extra,
    })
}

/// True if the deserializer leaves `signatures` empty: no inputs, or exactly one coinbase input.
fn signatures_not_expected_on_input(inputs: &[TxInput]) -> bool {
    inputs.is_empty() || matches!(inputs, [TxInput::Coinbase { .. }])
}

fn read_legacy_signatures(
    r: &mut Reader<'_>,
    inputs: &[TxInput],
) -> Result<Vec<Vec<Signature>>, DecodeError> {
    let not_expected = signatures_not_expected_on_input(inputs);
    let mut signatures: Vec<Vec<Signature>> = if not_expected {
        Vec::new()
    } else {
        vec![Vec::new(); inputs.len()]
    };
    for (i, input) in inputs.iter().enumerate() {
        let n = input.signature_count();
        if not_expected {
            if n == 0 {
                continue;
            }
            return Err(DecodeError::SignaturesNotExpected(i));
        }
        let mut sigs = Vec::with_capacity(n.min(r.remaining() / 64));
        for _ in 0..n {
            sigs.push(Signature(r.array64("ring signature")?));
        }
        signatures[i] = sigs;
    }
    Ok(signatures)
}

/// Version 2: nothing for no inputs or a single coinbase input; otherwise every input must be a
/// key input and carries `s_0 .. s_{n-1} || c1`.
fn read_clsag_signatures(
    r: &mut Reader<'_>,
    inputs: &[TxInput],
) -> Result<Vec<ClsagSignature>, DecodeError> {
    if signatures_not_expected_on_input(inputs) {
        return Ok(Vec::new());
    }
    let mut out = Vec::with_capacity(inputs.len());
    for (i, input) in inputs.iter().enumerate() {
        let TxInput::Key { output_indexes, .. } = input else {
            return Err(DecodeError::CoinbaseInputInSignedTransaction(i));
        };
        let n = output_indexes.len();
        let mut s = Vec::with_capacity(n.min(r.remaining() / 32));
        for _ in 0..n {
            s.push(EcScalar(r.array32("clsag s")?));
        }
        let c1 = EcScalar(r.array32("clsag c1")?);
        out.push(ClsagSignature { s, c1 });
    }
    Ok(out)
}

pub(crate) fn read_transaction(r: &mut Reader<'_>) -> Result<Transaction, DecodeError> {
    let prefix = read_prefix(r, false)?;
    if prefix.version >= TRANSACTION_VERSION_2 {
        let clsag_signatures = read_clsag_signatures(r, &prefix.inputs)?;
        return Ok(Transaction {
            prefix,
            signatures: Vec::new(),
            clsag_signatures,
        });
    }
    let signatures = read_legacy_signatures(r, &prefix.inputs)?;
    Ok(Transaction {
        prefix,
        signatures,
        clsag_signatures: Vec::new(),
    })
}

fn check_legacy_signatures(tx: &Transaction) -> Result<(), EncodeError> {
    let inputs = &tx.prefix.inputs;
    if !tx.clsag_signatures.is_empty() {
        return Err(EncodeError::SignatureFormat {
            version: tx.prefix.version,
        });
    }
    let not_expected = tx.signatures.is_empty();
    if !not_expected && inputs.len() != tx.signatures.len() {
        return Err(EncodeError::SignatureVectorCount {
            inputs: inputs.len(),
            signatures: tx.signatures.len(),
        });
    }
    for (i, input) in inputs.iter().enumerate() {
        let n = input.signature_count();
        if not_expected {
            if n != 0 {
                return Err(EncodeError::SignaturesNotExpected(i));
            }
        } else if tx.signatures[i].len() != n {
            return Err(EncodeError::SignatureCount {
                input: i,
                expected: n,
                got: tx.signatures[i].len(),
            });
        }
    }
    Ok(())
}

fn check_clsag_signatures(tx: &Transaction) -> Result<(), EncodeError> {
    let inputs = &tx.prefix.inputs;
    if !tx.signatures.is_empty() {
        return Err(EncodeError::SignatureFormat {
            version: tx.prefix.version,
        });
    }
    let sigs = &tx.clsag_signatures;
    if sigs.is_empty() {
        // Like C++ for version 1: no signatures is only valid without key inputs, and a
        // version-2 transaction never mixes a coinbase input with others.
        return match inputs.as_slice() {
            [] | [TxInput::Coinbase { .. }] => Ok(()),
            _ => Err(
                match inputs.iter().position(|i| matches!(i, TxInput::Key { .. })) {
                    Some(i) => EncodeError::SignaturesNotExpected(i),
                    None => EncodeError::CoinbaseInputInSignedTransaction(0),
                },
            ),
        };
    }
    if signatures_not_expected_on_input(inputs) || inputs.len() != sigs.len() {
        return Err(EncodeError::SignatureVectorCount {
            inputs: inputs.len(),
            signatures: sigs.len(),
        });
    }
    for (i, (input, sig)) in inputs.iter().zip(sigs).enumerate() {
        let TxInput::Key { output_indexes, .. } = input else {
            return Err(EncodeError::CoinbaseInputInSignedTransaction(i));
        };
        if sig.s.len() != output_indexes.len() {
            return Err(EncodeError::SignatureCount {
                input: i,
                expected: output_indexes.len(),
                got: sig.s.len(),
            });
        }
    }
    Ok(())
}

pub(crate) fn write_transaction(out: &mut Vec<u8>, tx: &Transaction) -> Result<(), EncodeError> {
    // Validate fully before writing so a failed encode leaves no partial output semantics.
    if tx.prefix.version >= TRANSACTION_VERSION_2 {
        check_clsag_signatures(tx)?;
    } else {
        check_legacy_signatures(tx)?;
    }
    write_prefix(out, &tx.prefix);
    for sigs in &tx.signatures {
        for s in sigs {
            out.extend_from_slice(&s.0);
        }
    }
    for sig in &tx.clsag_signatures {
        for s in &sig.s {
            out.extend_from_slice(&s.0);
        }
        out.extend_from_slice(&sig.c1.0);
    }
    Ok(())
}

/// C++ `fromBinaryArray<Transaction>`: decodes a full transaction blob (all bytes consumed).
pub fn decode_transaction(blob: &[u8]) -> Result<Transaction, DecodeError> {
    let mut r = Reader::new(blob);
    let tx = read_transaction(&mut r)?;
    r.finish("transaction")?;
    Ok(tx)
}

/// C++ `fromBinaryArray<TransactionPrefix>`.
pub fn decode_transaction_prefix(blob: &[u8]) -> Result<TransactionPrefix, DecodeError> {
    let mut r = Reader::new(blob);
    let p = read_prefix(&mut r, false)?;
    r.finish("transaction prefix")?;
    Ok(p)
}

/// C++ `toBinaryArray(Transaction)`.
pub fn encode_transaction(tx: &Transaction) -> Result<Vec<u8>, EncodeError> {
    let mut out = Vec::with_capacity(256);
    write_transaction(&mut out, tx)?;
    Ok(out)
}

/// C++ `toBinaryArray(TransactionPrefix)` (never fails; no version check on output).
#[must_use]
pub fn encode_transaction_prefix(prefix: &TransactionPrefix) -> Vec<u8> {
    let mut out = Vec::with_capacity(128);
    write_prefix(&mut out, prefix);
    out
}

/// C++ `toBinaryArray(BaseTransaction)`: the prefix, plus a zero varint for version >= 2.
#[must_use]
pub fn encode_base_transaction(prefix: &TransactionPrefix) -> Vec<u8> {
    let mut out = encode_transaction_prefix(prefix);
    if prefix.version >= 2 {
        write_varint(&mut out, 0);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `GENESIS_COINBASE_TX_HEX` from `cryptonote_config.h`.
    const GENESIS_TX: &str = "010a01ff000188f3b501029b2e4c0281c0b02e7c53291a94d1d0cbff8883f8024f5142ee494ffbbd088071210142694232c5b04151d9e4c27d31ec7a68ea568b19488cfcb422659a07a0e44dd5";

    #[test]
    fn genesis_coinbase_roundtrip() {
        let blob = hex::decode(GENESIS_TX).unwrap();
        let tx = decode_transaction(&blob).unwrap();
        assert_eq!(tx.prefix.version, 1);
        assert_eq!(tx.prefix.unlock_time, 10);
        assert_eq!(tx.prefix.inputs, vec![TxInput::Coinbase { height: 0 }]);
        assert_eq!(tx.prefix.outputs.len(), 1);
        assert_eq!(tx.prefix.outputs[0].amount, 2_980_232);
        assert_eq!(tx.signatures, Vec::<Vec<Signature>>::new());
        assert_eq!(encode_transaction(&tx).unwrap(), blob);
    }

    #[test]
    fn rejects_garbage_without_panicking() {
        let blob = hex::decode(GENESIS_TX).unwrap();
        for cut in 0..blob.len() {
            assert!(decode_transaction(&blob[..cut]).is_err(), "cut {cut}");
        }
        let mut v2 = blob.clone();
        v2[0] = 2;
        assert_eq!(
            decode_transaction(&v2),
            Err(DecodeError::WrongTransactionVersion(2))
        );
        let mut trailing = blob.clone();
        trailing.push(0);
        assert!(matches!(
            decode_transaction(&trailing),
            Err(DecodeError::TrailingBytes { .. })
        ));
        let mut bad_tag = blob;
        bad_tag[3] = 0x03; // input tag
        assert!(matches!(
            decode_transaction(&bad_tag),
            Err(DecodeError::UnknownInputTag { tag: 3, .. })
        ));
        // huge vin count
        assert!(matches!(
            decode_transaction(&[1, 0, 0xff, 0xff, 0xff, 0xff, 0x0f]),
            Err(DecodeError::LengthTooLarge { .. })
        ));
    }

    #[test]
    fn signature_count_checked_on_encode() {
        let mut tx = Transaction::default();
        tx.prefix.inputs.push(TxInput::Key {
            amount: 1,
            output_indexes: vec![1, 2],
            key_image: KeyImage::ZERO,
        });
        assert_eq!(
            encode_transaction(&tx),
            Err(EncodeError::SignaturesNotExpected(0))
        );
        tx.signatures = vec![vec![Signature::ZERO]];
        assert!(matches!(
            encode_transaction(&tx),
            Err(EncodeError::SignatureCount {
                input: 0,
                expected: 2,
                got: 1
            })
        ));
        tx.signatures = vec![vec![Signature::ZERO; 2]];
        let blob = encode_transaction(&tx).unwrap();
        assert_eq!(decode_transaction(&blob).unwrap(), tx);
    }

    fn v2_spend() -> Transaction {
        let key = |b: u8| PublicKey([b; 32]);
        Transaction {
            prefix: TransactionPrefix {
                version: TRANSACTION_VERSION_2,
                unlock_time: 0,
                inputs: vec![
                    TxInput::Key {
                        amount: 100,
                        output_indexes: vec![7, 1, 2],
                        key_image: KeyImage([0x11; 32]),
                    },
                    TxInput::Key {
                        amount: 1_000,
                        output_indexes: vec![300],
                        key_image: KeyImage([0x22; 32]),
                    },
                ],
                outputs: vec![
                    TxOutput {
                        amount: 90,
                        target: TxOutputTarget::TaggedKey {
                            key: key(0x33),
                            view_tag: 0xab,
                        },
                    },
                    TxOutput {
                        amount: 1_000,
                        target: TxOutputTarget::TaggedKey {
                            key: key(0x44),
                            view_tag: 0x00,
                        },
                    },
                ],
                extra: vec![0x01; 33],
            },
            signatures: Vec::new(),
            clsag_signatures: vec![
                ClsagSignature {
                    s: vec![EcScalar([1; 32]), EcScalar([2; 32]), EcScalar([3; 32])],
                    c1: EcScalar([4; 32]),
                },
                ClsagSignature {
                    s: vec![EcScalar([5; 32])],
                    c1: EcScalar([6; 32]),
                },
            ],
        }
    }

    #[test]
    fn v2_roundtrip_and_layout() {
        let tx = v2_spend();
        let blob = encode_transaction(&tx).unwrap();
        assert_eq!(decode_transaction(&blob).unwrap(), tx);
        // signature section: (3 + 1) + (1 + 1) scalars
        assert_eq!(tx.signature_section_len(), 6 * 32);
        let prefix = encode_transaction_prefix(&tx.prefix);
        assert_eq!(blob.len(), prefix.len() + 6 * 32);
        assert_eq!(&blob[..prefix.len()], prefix.as_slice());
        // s_0 .. s_{n-1} then c1, input by input
        let sigs = &blob[prefix.len()..];
        assert_eq!(sigs[0], 1);
        assert_eq!(sigs[64], 3);
        assert_eq!(sigs[96], 4);
        assert_eq!(sigs[128], 5);
        assert_eq!(sigs[160], 6);
        // output: amount 90, tag 0x03, key, view tag
        let out0 = prefix
            .windows(34)
            .position(|w| w[0] == TAG_TAGGED_KEY && w[1..33] == [0x33; 32])
            .unwrap();
        assert_eq!(prefix[out0 - 1], 90);
        assert_eq!(prefix[out0 + 33], 0xab);
        for cut in 0..blob.len() {
            assert!(decode_transaction(&blob[..cut]).is_err(), "cut {cut}");
        }
    }

    #[test]
    fn v2_format_rules() {
        // tag 0x03 is unknown in version 1
        let mut v1 = v2_spend();
        v1.prefix.version = 1;
        let prefix = encode_transaction_prefix(&v1.prefix);
        assert!(matches!(
            decode_transaction_prefix(&prefix),
            Err(DecodeError::UnknownOutputTag { tag: 0x03, .. })
        ));
        // version 3 is unknown
        let mut v3 = encode_transaction(&v2_spend()).unwrap();
        v3[0] = 3;
        assert_eq!(
            decode_transaction(&v3),
            Err(DecodeError::WrongTransactionVersion(3))
        );
        // legacy signatures on a v2 transaction / CLSAG on a v1 transaction
        let mut mixed = v2_spend();
        mixed.signatures = vec![vec![Signature::ZERO; 3], vec![Signature::ZERO]];
        assert_eq!(
            encode_transaction(&mixed),
            Err(EncodeError::SignatureFormat { version: 2 })
        );
        v1.prefix.outputs.clear();
        assert_eq!(
            encode_transaction(&v1),
            Err(EncodeError::SignatureFormat { version: 1 })
        );
        // wrong s count
        let mut short = v2_spend();
        short.clsag_signatures[0].s.pop();
        assert_eq!(
            encode_transaction(&short),
            Err(EncodeError::SignatureCount {
                input: 0,
                expected: 3,
                got: 2
            })
        );
        // missing signatures
        let mut none = v2_spend();
        none.clsag_signatures.clear();
        assert_eq!(
            encode_transaction(&none),
            Err(EncodeError::SignaturesNotExpected(0))
        );
        // a coinbase input next to a key input
        let mut mixed_inputs = v2_spend();
        mixed_inputs.prefix.inputs[1] = TxInput::Coinbase { height: 1 };
        assert_eq!(
            encode_transaction(&mixed_inputs),
            Err(EncodeError::CoinbaseInputInSignedTransaction(1))
        );
        let mut blob = encode_transaction_prefix(&mixed_inputs.prefix);
        blob.extend_from_slice(&[0u8; 4 * 32]);
        assert_eq!(
            decode_transaction(&blob),
            Err(DecodeError::CoinbaseInputInSignedTransaction(1))
        );
    }

    #[test]
    fn v2_coinbase_roundtrip() {
        let tx = Transaction {
            prefix: TransactionPrefix {
                version: TRANSACTION_VERSION_2,
                unlock_time: 30,
                inputs: vec![TxInput::Coinbase { height: 10 }],
                outputs: vec![TxOutput {
                    amount: 2_980_232,
                    target: TxOutputTarget::TaggedKey {
                        key: PublicKey([9; 32]),
                        view_tag: 0x5a,
                    },
                }],
                extra: vec![],
            },
            ..Transaction::default()
        };
        let blob = encode_transaction(&tx).unwrap();
        assert_eq!(blob.len(), encode_transaction_prefix(&tx.prefix).len());
        assert_eq!(decode_transaction(&blob).unwrap(), tx);
        assert_eq!(tx.signature_section_len(), 0);
    }
}
