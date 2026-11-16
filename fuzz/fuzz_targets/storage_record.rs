//! Storage codecs (`xkr-storage`): block records, chunk bodies, varints, output-key stripping
//! and the zstd framing, on bytes as a damaged database file could hold them.
//! Never panics; decoded records and chunks round-trip.
#![no_main]

use std::cell::RefCell;

use libfuzzer_sys::fuzz_target;
use xkr_storage::Compressor;
use xkr_storage::codec::{
    BlockRecord, decode_chunk, encode_chunk, stripped_amounts, unstrip_output_keys, varint_at,
};
use xkr_storage::compress::DEFAULT_LEVEL;

/// Frames declaring more than this are skipped to keep runs fast: `decompress` pre-allocates the
/// declared length once it passes the payload bound (128 KiB per 3 payload bytes), so a few KiB of
/// input could still reserve hundreds of MiB.
const MAX_DECLARED: usize = 16 << 20;

thread_local! {
    static COMPRESSOR: RefCell<Compressor> =
        RefCell::new(Compressor::new(DEFAULT_LEVEL).expect("zstd context"));
}

fuzz_target!(|data: &[u8]| {
    if let Ok(rec) = BlockRecord::decode(data) {
        assert_eq!(BlockRecord::decode(&rec.encode()).expect("re-encoded record"), rec);
        let _ = rec.sig_bytes();
        for i in 0..=rec.txs.len() + 1 {
            let _ = rec.tx_hash(u32::try_from(i).unwrap_or(u32::MAX));
        }
    }
    if let Ok(parts) = decode_chunk(data) {
        let owned: Vec<Vec<u8>> = parts.iter().map(|p| p.to_vec()).collect();
        let body = encode_chunk(&owned);
        assert_eq!(decode_chunk(&body).expect("re-encoded chunk"), parts);
    }
    for pos in [0, 1, data.len() / 2] {
        let _ = varint_at(data, pos);
    }
    if let Some((&n, rest)) = data.split_first() {
        let off = usize::from(n) % (rest.len() + 2);
        let _ = stripped_amounts(rest, off, usize::from(n));
        let _ = unstrip_output_keys(rest, off, usize::from(n), |_, _| Ok([0u8; 32]));
    }
    if data.len() >= 5 {
        let declared = u32::from_le_bytes([data[1], data[2], data[3], data[4]]) as usize;
        if declared <= MAX_DECLARED {
            COMPRESSOR.with(|c| {
                let _ = c.borrow_mut().decompress(data);
            });
        }
    }
    // compress -> decompress is the identity
    COMPRESSOR.with(|c| {
        let mut c = c.borrow_mut();
        let framed = c.compress(data).expect("compress");
        assert_eq!(c.decompress(&framed).expect("decompress"), data);
    });
});
