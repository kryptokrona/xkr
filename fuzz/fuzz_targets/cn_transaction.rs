//! CryptoNote transaction decoding and every `extra` parser: never panics; decoded
//! transactions and prefixes round-trip.
#![no_main]

use libfuzzer_sys::fuzz_target;
use xkr_serialization::{
    decode_transaction, decode_transaction_prefix, encode_transaction, encode_transaction_prefix,
    transaction_hash, transaction_prefix_hash,
};
use xkr_types::extra::{hugin_data, parse_utils_extra};
use xkr_types::{merge_mining_tag, parse_extra_partial, payment_id, tx_public_key};

fn extras(extra: &[u8]) {
    let (_fields, _ok) = parse_extra_partial(extra);
    let _ = tx_public_key(extra);
    let _ = merge_mining_tag(extra);
    let _ = payment_id(extra);
    let _ = parse_utils_extra(extra);
    let _ = hugin_data(extra);
}

fuzz_target!(|data: &[u8]| {
    if let Ok(tx) = decode_transaction(data) {
        let blob = encode_transaction(&tx).expect("a decoded transaction re-encodes");
        assert_eq!(decode_transaction(&blob).as_ref(), Ok(&tx));
        let _ = transaction_hash(&tx);
        extras(&tx.prefix.extra);
    }
    if let Ok(prefix) = decode_transaction_prefix(data) {
        let blob = encode_transaction_prefix(&prefix);
        assert_eq!(decode_transaction_prefix(&blob).as_ref(), Ok(&prefix));
        let _ = transaction_prefix_hash(&prefix);
    }
    extras(data);
});
