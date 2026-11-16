//! Building transaction `extra`.
//!
//! * `0x01 || R`: the transaction public key (`addTransactionPublicKeyToExtra`).
//! * `0x02 || 33 || 0x00 || payment_id`: a C++ payment id nonce
//!   (`setPaymentIdToTransactionExtraNonce`), byte-identical to what the C++ parser reads.
//! * Hugin data: the kryptokrona-utils nonce layout, `0x02 || varint(len) || [0x00 ||
//!   payment_id] || 0x7f || varint(data_len) || data` (read back by
//!   `xkr_types::extra::hugin_data`). With short data every varint is one byte, so the C++
//!   parser sees an ordinary nonce.
//!
//! The pool refuses `extra` of `MAX_EXTRA_SIZE_POOL` (2200) bytes or more; blocks templates
//! skip transactions with 128 bytes or more, so Hugin messages live in the pool only.

use xkr_consensus::params::MAX_EXTRA_SIZE_POOL;
use xkr_serialization::varint::write_varint;
use xkr_types::extra::{
    TX_EXTRA_NONCE, TX_EXTRA_NONCE_DATA, TX_EXTRA_NONCE_PAYMENT_ID, TX_EXTRA_TAG_PUBKEY,
};
use xkr_types::{Hash, PublicKey};

/// Largest `extra` the pool accepts (`extra.len() < MAX_EXTRA_SIZE_POOL`).
pub const MAX_EXTRA_LEN: usize = MAX_EXTRA_SIZE_POOL as usize - 1;

/// Builds `extra` for a transaction with public key `tx_public_key`, an optional payment id
/// and optional arbitrary data (Hugin).
#[must_use]
pub fn build_extra(
    tx_public_key: &PublicKey,
    payment_id: Option<&Hash>,
    data: Option<&[u8]>,
) -> Vec<u8> {
    let mut extra = Vec::with_capacity(33 + 35 + data.map_or(0, |d| d.len() + 6));
    extra.push(TX_EXTRA_TAG_PUBKEY);
    extra.extend_from_slice(&tx_public_key.0);
    match (payment_id, data) {
        (None, None) => {}
        (Some(pid), None) => {
            extra.push(TX_EXTRA_NONCE);
            extra.push(33);
            extra.push(TX_EXTRA_NONCE_PAYMENT_ID);
            extra.extend_from_slice(&pid.0);
        }
        (pid, Some(d)) => {
            let mut nonce = Vec::with_capacity(d.len() + 40);
            if let Some(pid) = pid {
                nonce.push(TX_EXTRA_NONCE_PAYMENT_ID);
                nonce.extend_from_slice(&pid.0);
            }
            nonce.push(TX_EXTRA_NONCE_DATA);
            write_varint(&mut nonce, d.len() as u64);
            nonce.extend_from_slice(d);
            extra.push(TX_EXTRA_NONCE);
            write_varint(&mut extra, nonce.len() as u64);
            extra.extend_from_slice(&nonce);
        }
    }
    extra
}

#[cfg(test)]
mod tests {
    use xkr_types::extra::hugin_data;
    use xkr_types::{payment_id, tx_public_key};

    use super::*;

    #[test]
    fn fields_read_back() {
        let r = PublicKey([9u8; 32]);
        let pid = Hash([4u8; 32]);
        let e = build_extra(&r, Some(&pid), None);
        assert_eq!(tx_public_key(&e), Some(r));
        assert_eq!(payment_id(&e), Some(pid));
        let msg = vec![0x42u8; 1500];
        let e = build_extra(&r, None, Some(&msg));
        assert_eq!(tx_public_key(&e), Some(r));
        assert_eq!(hugin_data(&e), Some(msg));
        assert!(e.len() <= MAX_EXTRA_LEN);
        let e = build_extra(&r, Some(&pid), Some(b"hi"));
        assert_eq!(hugin_data(&e).as_deref(), Some(&b"hi"[..]));
    }
}
