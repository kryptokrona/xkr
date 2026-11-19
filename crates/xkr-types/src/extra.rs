//! Transaction `extra` parsing.
//!
//! [`parse_extra`] / [`parse_extra_partial`] mirror C++ `parseTransactionExtra`
//! (`src/cryptonote_core/transaction_extra.cpp`) **including its quirks**, because the node uses
//! that parser for consensus-relevant lookups (the merge-mining tag of the parent block, the
//! transaction public key):
//!
//! * unknown tag bytes are skipped one byte at a time (there is no `default:` case);
//! * a second padding / public-key / nonce field stops parsing successfully, keeping the fields
//!   parsed so far;
//! * a second merge-mining tag byte is skipped as a single byte (`break` out of the `switch`);
//! * padding must run to the end of `extra`, be all zeros and be at most 255 bytes in total
//!   (tag included);
//! * the nonce length is a single raw byte (not a varint);
//! * on failure C++ returns `false` but leaves the fields parsed so far in the output vector,
//!   and callers like `getTransactionPublicKeyFromExtra` ignore the return value. Use
//!   [`parse_extra_partial`] when you need that behaviour.
//!
//! [`parse_utils_extra`] is a separate, non-consensus decoder for the layout written by
//! `kryptokrona-utils` (wallet-backend-js, Hugin): varint field lengths and nonce sub-tags
//! `0x00` (payment id) and `0x7f` (arbitrary data, where Hugin stores its messages).

use crate::keys::{Hash, PublicKey};

/// `TX_EXTRA_TAG_PADDING`.
pub const TX_EXTRA_TAG_PADDING: u8 = 0x00;
/// `TX_EXTRA_TAG_PUBKEY`.
pub const TX_EXTRA_TAG_PUBKEY: u8 = 0x01;
/// `TX_EXTRA_NONCE`.
pub const TX_EXTRA_NONCE: u8 = 0x02;
/// `TX_EXTRA_MERGE_MINING_TAG`.
pub const TX_EXTRA_MERGE_MINING_TAG: u8 = 0x03;
/// kryptokrona-utils `RECIPIENT_PUBLIC_VIEW_KEY` (not parsed by the C++ node).
pub const TX_EXTRA_RECIPIENT_PUBLIC_VIEW_KEY: u8 = 0x04;
/// kryptokrona-utils `RECIPIENT_PUBLIC_SPEND_KEY` (not parsed by the C++ node).
pub const TX_EXTRA_RECIPIENT_PUBLIC_SPEND_KEY: u8 = 0x05;
/// kryptokrona-utils `TRANSACTION_PRIVATE_KEY` (not parsed by the C++ node).
pub const TX_EXTRA_TRANSACTION_PRIVATE_KEY: u8 = 0x06;
/// kryptokrona-utils `POOL_NONCE` (not parsed by the C++ node).
pub const TX_EXTRA_POOL_NONCE: u8 = 0x07;
/// `TX_EXTRA_NONCE_PAYMENT_ID`: nonce sub-tag for a 32-byte payment id.
pub const TX_EXTRA_NONCE_PAYMENT_ID: u8 = 0x00;
/// kryptokrona-utils `NonceTagType.EXTRA_DATA`: nonce sub-tag for arbitrary data (Hugin).
pub const TX_EXTRA_NONCE_DATA: u8 = 0x7f;
/// `TX_EXTRA_PADDING_MAX_COUNT`.
pub const TX_EXTRA_PADDING_MAX_COUNT: usize = 255;
/// `TX_EXTRA_NONCE_MAX_COUNT`.
pub const TX_EXTRA_NONCE_MAX_COUNT: usize = 255;
/// `parameters::MAX_EXTRA_SIZE`: the binary input serializer discards an `mm_tag` string longer
/// than this (`binary_input_stream_serializer.cpp`).
pub const MAX_MM_TAG_STRING: u64 = 2200;

/// C++ `TransactionExtraMergeMiningTag`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MergeMiningTag {
    /// Depth of the aux-chain merkle branch (C++ `size_t`).
    pub depth: u64,
    /// Aux-chain merkle root.
    pub merkle_root: Hash,
}

/// C++ `TransactionExtraField` (`boost::variant<Padding, PublicKey, Nonce, MergeMiningTag>`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExtraField {
    /// Zero padding; `size` counts the tag byte, like C++.
    Padding {
        /// Total padding length including the tag byte.
        size: usize,
    },
    /// Transaction public key `R = r*G`.
    PublicKey(PublicKey),
    /// Extra nonce (payment id lives here as sub-tag `0x00`).
    Nonce(Vec<u8>),
    /// Merge-mining tag.
    MergeMiningTag(MergeMiningTag),
}

/// Why `parseTransactionExtra` returned `false`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ExtraError {
    /// Padding contains a non-zero byte.
    #[error("non-zero byte in extra padding at offset {offset}")]
    NonZeroPadding {
        /// Offset of the offending byte.
        offset: usize,
    },
    /// Padding longer than `TX_EXTRA_PADDING_MAX_COUNT`.
    #[error("extra padding longer than 255 bytes")]
    PaddingTooLong,
    /// A field runs past the end of `extra`.
    #[error("truncated extra field {field} at offset {offset}")]
    Truncated {
        /// Field being parsed.
        field: &'static str,
        /// Offset where the field started.
        offset: usize,
    },
    /// A varint inside the merge-mining tag is malformed.
    #[error("malformed varint in merge-mining tag at offset {offset}")]
    BadVarint {
        /// Offset where the varint started.
        offset: usize,
    },
    /// The merge-mining tag string is longer than `MAX_EXTRA_SIZE` and is discarded by C++.
    #[error("merge-mining tag of {len} bytes exceeds 2200")]
    MergeMiningTagTooLong {
        /// Declared length.
        len: u64,
    },
}

/// Reads a C++ `readVarint<uint64_t>` varint (`src/common/stream_tools.cpp`) at `*pos`.
/// Returns `None` on EOF, overflow or non-canonical encoding.
fn read_varint_u64(data: &[u8], pos: &mut usize) -> Option<u64> {
    let mut value: u64 = 0;
    let mut shift: u32 = 0;
    loop {
        let piece = *data.get(*pos)?;
        *pos += 1;
        if shift >= 57 && u32::from(piece) >= (1u32 << (64 - shift)) {
            return None;
        }
        value |= u64::from(piece & 0x7f) << shift;
        if piece & 0x80 == 0 {
            if piece == 0 && shift != 0 {
                return None;
            }
            return Some(value);
        }
        shift += 7;
    }
}

fn take<'a>(data: &'a [u8], pos: &mut usize, n: usize) -> Option<&'a [u8]> {
    let end = pos.checked_add(n)?;
    let s = data.get(*pos..end)?;
    *pos = end;
    Some(s)
}

/// Parses the merge-mining tag body exactly like C++ `serialize(TransactionExtraMergeMiningTag&)`
/// on a `BinaryInputStreamSerializer`: a length-prefixed string named `"mm_tag"` that is itself
/// `varint depth || 32-byte merkle root` (trailing bytes ignored).
fn parse_mm_tag(data: &[u8], pos: &mut usize) -> Result<MergeMiningTag, ExtraError> {
    let start = *pos;
    let len = read_varint_u64(data, pos).ok_or(ExtraError::BadVarint { offset: start })?;
    if len > MAX_MM_TAG_STRING {
        // C++ reads and discards `len` bytes, then fails parsing the now-empty string.
        return Err(ExtraError::MergeMiningTagTooLong { len });
    }
    let len = usize::try_from(len).map_err(|_| ExtraError::Truncated {
        field: "mm_tag",
        offset: start,
    })?;
    let body = take(data, pos, len).ok_or(ExtraError::Truncated {
        field: "mm_tag",
        offset: start,
    })?;
    let mut bpos = 0usize;
    let depth = read_varint_u64(body, &mut bpos).ok_or(ExtraError::BadVarint { offset: start })?;
    let root = take(body, &mut bpos, 32).ok_or(ExtraError::Truncated {
        field: "mm_tag",
        offset: start,
    })?;
    let mut merkle_root = Hash::ZERO;
    merkle_root.0.copy_from_slice(root);
    Ok(MergeMiningTag { depth, merkle_root })
}

/// C++ `parseTransactionExtra`, returning both the fields parsed so far and the success flag.
///
/// The returned vector is exactly what C++ leaves in `transactionExtraFields`, also when the
/// result is an error.
pub fn parse_extra_partial(extra: &[u8]) -> (Vec<ExtraField>, Result<(), ExtraError>) {
    let mut fields = Vec::new();
    let mut seen_padding = false;
    let mut seen_pubkey = false;
    let mut seen_nonce = false;
    let mut seen_mm = false;
    let mut pos = 0usize;

    while pos < extra.len() {
        let tag_offset = pos;
        let tag = extra[pos];
        pos += 1;
        match tag {
            TX_EXTRA_TAG_PADDING => {
                if seen_padding {
                    return (fields, Ok(()));
                }
                seen_padding = true;
                let mut size = 1usize;
                while pos < extra.len() && size <= TX_EXTRA_PADDING_MAX_COUNT {
                    if extra[pos] != 0 {
                        return (fields, Err(ExtraError::NonZeroPadding { offset: pos }));
                    }
                    pos += 1;
                    size += 1;
                }
                if size > TX_EXTRA_PADDING_MAX_COUNT {
                    return (fields, Err(ExtraError::PaddingTooLong));
                }
                fields.push(ExtraField::Padding { size });
            }
            TX_EXTRA_TAG_PUBKEY => {
                if seen_pubkey {
                    return (fields, Ok(()));
                }
                seen_pubkey = true;
                let Some(bytes) = take(extra, &mut pos, 32) else {
                    return (
                        fields,
                        Err(ExtraError::Truncated {
                            field: "public_key",
                            offset: tag_offset,
                        }),
                    );
                };
                let mut key = PublicKey::ZERO;
                key.0.copy_from_slice(bytes);
                fields.push(ExtraField::PublicKey(key));
            }
            TX_EXTRA_NONCE => {
                if seen_nonce {
                    return (fields, Ok(()));
                }
                seen_nonce = true;
                let Some(&size) = extra.get(pos) else {
                    return (
                        fields,
                        Err(ExtraError::Truncated {
                            field: "nonce",
                            offset: tag_offset,
                        }),
                    );
                };
                pos += 1;
                let Some(bytes) = take(extra, &mut pos, usize::from(size)) else {
                    return (
                        fields,
                        Err(ExtraError::Truncated {
                            field: "nonce",
                            offset: tag_offset,
                        }),
                    );
                };
                fields.push(ExtraField::Nonce(bytes.to_vec()));
            }
            TX_EXTRA_MERGE_MINING_TAG => {
                if seen_mm {
                    // C++ `break`s out of the switch: the tag byte is consumed, nothing else.
                    continue;
                }
                seen_mm = true;
                match parse_mm_tag(extra, &mut pos) {
                    Ok(tag) => fields.push(ExtraField::MergeMiningTag(tag)),
                    Err(e) => return (fields, Err(e)),
                }
            }
            _ => {}
        }
    }
    (fields, Ok(()))
}

/// C++ `parseTransactionExtra`: `Ok(fields)` if it returns `true`.
pub fn parse_extra(extra: &[u8]) -> Result<Vec<ExtraField>, ExtraError> {
    let (fields, result) = parse_extra_partial(extra);
    result.map(|()| fields)
}

/// C++ `getTransactionPublicKeyFromExtra`: first public-key field of the (possibly partial)
/// parse. C++ returns an all-zero key when absent; this returns `None` instead.
#[must_use]
pub fn tx_public_key(extra: &[u8]) -> Option<PublicKey> {
    parse_extra_partial(extra)
        .0
        .into_iter()
        .find_map(|f| match f {
            ExtraField::PublicKey(k) => Some(k),
            _ => None,
        })
}

/// The transaction public key as wallet sync reports it (C++ `Core::getPubKeyFromExtra`,
/// `/getwalletsyncdata` `txPublicKey`): [`tx_public_key`] when the fields parse up to a key,
/// otherwise the 32 bytes after the first `0x01` byte, and all zeros if there is none or it is
/// truncated.
#[must_use]
pub fn wallet_tx_public_key(extra: &[u8]) -> [u8; 32] {
    if let Some(key) = tx_public_key(extra) {
        return key.0;
    }
    let mut key = [0u8; 32];
    if let Some(i) = extra.iter().position(|b| *b == 0x01)
        && let Some(bytes) = extra.get(i + 1..i + 33)
    {
        key.copy_from_slice(bytes);
    }
    key
}

/// C++ `getMergeMiningTagFromExtra`: first merge-mining tag of the (possibly partial) parse.
#[must_use]
pub fn merge_mining_tag(extra: &[u8]) -> Option<MergeMiningTag> {
    parse_extra_partial(extra)
        .0
        .into_iter()
        .find_map(|f| match f {
            ExtraField::MergeMiningTag(t) => Some(t),
            _ => None,
        })
}

/// C++ `getPaymentIdFromTxExtra`: requires a successful parse and a nonce of exactly 33 bytes
/// starting with sub-tag `0x00`.
#[must_use]
pub fn payment_id(extra: &[u8]) -> Option<Hash> {
    let fields = parse_extra(extra).ok()?;
    let nonce = fields.into_iter().find_map(|f| match f {
        ExtraField::Nonce(n) => Some(n),
        _ => None,
    })?;
    payment_id_from_nonce(&nonce)
}

/// C++ `getPaymentIdFromTransactionExtraNonce`.
#[must_use]
pub fn payment_id_from_nonce(nonce: &[u8]) -> Option<Hash> {
    if nonce.len() != 33 || nonce[0] != TX_EXTRA_NONCE_PAYMENT_ID {
        return None;
    }
    Hash::from_slice(&nonce[1..])
}

/// A field as written by `kryptokrona-utils` (wallet-backend-js, Hugin). Not consensus.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UtilsExtraField {
    /// Padding (rest of extra).
    Padding(usize),
    /// Tag `0x01`.
    PublicKey(PublicKey),
    /// Tag `0x02` with varint length; sub-fields decoded from its content.
    Nonce(Vec<NonceField>),
    /// Tag `0x03` with varint length (raw body).
    MergeMining(Vec<u8>),
    /// Tag `0x04`.
    RecipientPublicViewKey(PublicKey),
    /// Tag `0x05`.
    RecipientPublicSpendKey(PublicKey),
    /// Tag `0x06`.
    TransactionPrivateKey([u8; 32]),
    /// Tag `0x07` with varint length.
    PoolNonce(Vec<u8>),
}

/// A sub-field of a kryptokrona-utils extra nonce.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NonceField {
    /// Sub-tag `0x00`: 32-byte payment id.
    PaymentId(Hash),
    /// Sub-tag `0x7f`: varint length + arbitrary data (Hugin messages).
    Data(Vec<u8>),
}

fn parse_nonce_fields(nonce: &[u8]) -> Vec<NonceField> {
    let mut out = Vec::new();
    let mut pos = 0usize;
    while pos < nonce.len() {
        let save = pos;
        let Some(tag) = read_varint_u64(nonce, &mut pos) else {
            pos = save + 1;
            continue;
        };
        match tag {
            0x00 => match take(nonce, &mut pos, 32) {
                Some(b) => out.push(NonceField::PaymentId(
                    Hash::from_slice(b).unwrap_or_default(),
                )),
                None => pos = save + 1,
            },
            0x7f => {
                let len = read_varint_u64(nonce, &mut pos).and_then(|l| usize::try_from(l).ok());
                match len.and_then(|l| take(nonce, &mut pos, l)) {
                    Some(b) => out.push(NonceField::Data(b.to_vec())),
                    None => pos = save + 1,
                }
            }
            _ => pos = save + 1,
        }
    }
    out
}

/// Lenient decoder for the kryptokrona-utils extra layout (varint tags and lengths). Malformed
/// or unknown fields are skipped one byte at a time, like `readExtra` in kryptokrona-utils.
/// **Not** used for consensus; the C++ node only understands [`parse_extra`].
#[must_use]
pub fn parse_utils_extra(extra: &[u8]) -> Vec<UtilsExtraField> {
    let mut out = Vec::new();
    let mut pos = 0usize;
    while pos < extra.len() {
        let save = pos;
        let Some(tag) = read_varint_u64(extra, &mut pos) else {
            pos = save + 1;
            continue;
        };
        let key32 = |pos: &mut usize| {
            take(extra, pos, 32).map(|b| {
                let mut k = [0u8; 32];
                k.copy_from_slice(b);
                k
            })
        };
        let len_prefixed = |pos: &mut usize| {
            read_varint_u64(extra, pos)
                .and_then(|l| usize::try_from(l).ok())
                .and_then(|l| take(extra, pos, l))
                .map(<[u8]>::to_vec)
        };
        let field = match tag {
            0x00 => {
                let rest = extra.len() - save;
                pos = extra.len();
                Some(UtilsExtraField::Padding(rest))
            }
            0x01 => key32(&mut pos).map(|k| UtilsExtraField::PublicKey(PublicKey(k))),
            0x02 => len_prefixed(&mut pos).map(|n| UtilsExtraField::Nonce(parse_nonce_fields(&n))),
            0x03 => len_prefixed(&mut pos).map(UtilsExtraField::MergeMining),
            0x04 => key32(&mut pos).map(|k| UtilsExtraField::RecipientPublicViewKey(PublicKey(k))),
            0x05 => key32(&mut pos).map(|k| UtilsExtraField::RecipientPublicSpendKey(PublicKey(k))),
            0x06 => key32(&mut pos).map(UtilsExtraField::TransactionPrivateKey),
            0x07 => len_prefixed(&mut pos).map(UtilsExtraField::PoolNonce),
            _ => None,
        };
        match field {
            Some(f) => out.push(f),
            None => pos = save + 1,
        }
    }
    out
}

/// The Hugin payload of a transaction: the first nonce sub-field `0x7f` in the
/// kryptokrona-utils layout. Not consensus.
#[must_use]
pub fn hugin_data(extra: &[u8]) -> Option<Vec<u8>> {
    parse_utils_extra(extra).into_iter().find_map(|f| match f {
        UtilsExtraField::Nonce(fields) => fields.into_iter().find_map(|n| match n {
            NonceField::Data(d) => Some(d),
            NonceField::PaymentId(_) => None,
        }),
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pk_extra() -> Vec<u8> {
        let mut e = vec![TX_EXTRA_TAG_PUBKEY];
        e.extend_from_slice(&[0xab; 32]);
        e
    }

    #[test]
    fn pubkey_and_nonce() {
        let mut e = pk_extra();
        e.push(TX_EXTRA_NONCE);
        e.push(33);
        e.push(TX_EXTRA_NONCE_PAYMENT_ID);
        e.extend_from_slice(&[0x11; 32]);
        let f = parse_extra(&e).unwrap();
        assert_eq!(f.len(), 2);
        assert_eq!(tx_public_key(&e), Some(PublicKey([0xab; 32])));
        assert_eq!(payment_id(&e), Some(Hash([0x11; 32])));
    }

    #[test]
    fn duplicate_pubkey_stops_successfully() {
        let mut e = pk_extra();
        e.extend_from_slice(&pk_extra());
        e.push(0x00);
        e.push(0x01); // would be bad padding, never reached
        assert_eq!(parse_extra(&e).unwrap().len(), 1);
    }

    #[test]
    fn unknown_tags_are_skipped_bytewise() {
        let mut e = vec![0x55, 0x66];
        e.extend_from_slice(&pk_extra());
        assert_eq!(
            parse_extra(&e).unwrap(),
            vec![ExtraField::PublicKey(PublicKey([0xab; 32]))]
        );
    }

    #[test]
    fn padding_rules() {
        assert_eq!(
            parse_extra(&[0, 0, 0]).unwrap(),
            vec![ExtraField::Padding { size: 3 }]
        );
        assert!(matches!(
            parse_extra(&[0, 0, 1]),
            Err(ExtraError::NonZeroPadding { offset: 2 })
        ));
        assert!(parse_extra(&[0u8; 255]).is_ok());
        assert_eq!(parse_extra(&[0u8; 256]), Err(ExtraError::PaddingTooLong));
    }

    #[test]
    fn partial_parse_keeps_pubkey() {
        let mut e = pk_extra();
        e.push(TX_EXTRA_NONCE);
        e.push(10); // truncated nonce
        let (fields, res) = parse_extra_partial(&e);
        assert!(res.is_err());
        assert_eq!(fields.len(), 1);
        assert_eq!(tx_public_key(&e), Some(PublicKey([0xab; 32])));
    }

    #[test]
    fn merge_mining_tag_from_parent_block() {
        // Parent block miner tx extra seen on mainnet block 2617400.
        let e =
            hex::decode("03210075879d1a16edcae83e59d45351cdf0610fcb28bde0211d17c36524ea23b01c7d")
                .unwrap();
        let tag = merge_mining_tag(&e).unwrap();
        assert_eq!(tag.depth, 0);
        assert_eq!(
            tag.merkle_root.to_string(),
            "75879d1a16edcae83e59d45351cdf0610fcb28bde0211d17c36524ea23b01c7d"
        );
    }

    #[test]
    fn hugin_layout() {
        // 01 <pk> 02 <varint len> 7f <varint len> <data>
        let data = b"7b2274223a317d".to_vec();
        let mut inner = vec![0x7f, u8::try_from(data.len()).unwrap()];
        inner.extend_from_slice(&data);
        let mut e = pk_extra();
        e.push(0x02);
        e.push(u8::try_from(inner.len()).unwrap());
        e.extend_from_slice(&inner);
        assert_eq!(hugin_data(&e), Some(data));
        // The C++ parser reads the same bytes as a raw-length nonce.
        assert!(parse_extra(&e).is_ok());
    }
}
