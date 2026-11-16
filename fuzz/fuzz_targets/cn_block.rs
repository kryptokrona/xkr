//! CryptoNote block and raw-block decoding (`xkr-serialization`): never panics, and whatever
//! decodes re-encodes to something that decodes to the same value.
#![no_main]

use libfuzzer_sys::fuzz_target;
use xkr_serialization::{
    block_hash, block_hashing_blob, decode_block, decode_raw_block, decode_transaction,
    encode_block, encode_raw_block,
};

fuzz_target!(|data: &[u8]| {
    if let Ok(block) = decode_block(data) {
        let blob = encode_block(&block).expect("a decoded block re-encodes");
        assert_eq!(decode_block(&blob).as_ref(), Ok(&block));
        let _ = block_hash(&block);
        let _ = block_hashing_blob(&block);
        let _ = xkr_types::merge_mining_tag(&block.miner_tx.prefix.extra);
    }
    if let Ok(raw) = decode_raw_block(data) {
        assert_eq!(decode_raw_block(&encode_raw_block(&raw)).as_ref(), Ok(&raw));
        let _ = decode_block(&raw.block);
        for tx in &raw.transactions {
            let _ = decode_transaction(tx);
        }
    }
});
