//! Property tests for the storage byte codecs (`docs/storage.md`): block records, chunk bodies,
//! varints, output-key stripping and the zstd framing round-trip; corrupt bytes (a damaged database
//! file) are errors, never panics, and decoding allocates linearly in the input. Cases:
//! `PROPTEST_CASES` (default 128).
#![allow(unsafe_code)] // the counting global allocator below

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use proptest::collection::vec;
use proptest::prelude::*;
use xkr_storage::Compressor;
use xkr_storage::codec::{
    BlockRecord, TAG_KEY_OUTPUT, TxRecord, decode_chunk, encode_chunk, put_varint,
    strip_output_keys, stripped_amounts, unstrip_output_keys, varint_at, varint_len,
};
use xkr_storage::compress::{DEFAULT_LEVEL, framed_dictionary_id, is_raw_frame};

// ------------------------------------------------------------------ allocation accounting

const ALLOC_FACTOR: usize = 128;
const ALLOC_SLACK: usize = 64 * 1024;

struct Counting;

thread_local! {
    static ACTIVE: Cell<bool> = const { Cell::new(false) };
    static CURRENT: Cell<usize> = const { Cell::new(0) };
    static PEAK: Cell<usize> = const { Cell::new(0) };
}

fn on_alloc(n: usize) {
    if ACTIVE.try_with(Cell::get).unwrap_or(false) {
        let cur = CURRENT.try_with(Cell::get).unwrap_or(0).saturating_add(n);
        let _ = CURRENT.try_with(|c| c.set(cur));
        let _ = PEAK.try_with(|p| p.set(p.get().max(cur)));
    }
}

fn on_dealloc(n: usize) {
    if ACTIVE.try_with(Cell::get).unwrap_or(false) {
        let _ = CURRENT.try_with(|c| c.set(c.get().saturating_sub(n)));
    }
}

// SAFETY: forwards every call to `System` unchanged; the bookkeeping only touches
// const-initialised thread-locals without destructors.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        on_alloc(layout.size());
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        on_alloc(layout.size());
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        on_dealloc(layout.size());
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        on_alloc(new_size);
        on_dealloc(layout.size());
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

fn peak_alloc<R>(f: impl FnOnce() -> R) -> (R, usize) {
    let _ = CURRENT.try_with(|c| c.set(0));
    let _ = PEAK.try_with(|p| p.set(0));
    let _ = ACTIVE.try_with(|a| a.set(true));
    let r = f();
    let _ = ACTIVE.try_with(|a| a.set(false));
    (r, PEAK.try_with(Cell::get).unwrap_or(0))
}

fn cases(default: u32) -> u32 {
    std::env::var("PROPTEST_CASES")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(default)
}

// ------------------------------------------------------------------ strategies

fn interesting_u64() -> impl Strategy<Value = u64> {
    prop_oneof![
        0u64..300,
        (0u32..10, 0u64..3).prop_map(|(k, d)| (1u64 << (7 * k)).wrapping_add(d).wrapping_sub(1)),
        Just(u64::MAX),
        any::<u64>(),
    ]
}

fn tx_record() -> impl Strategy<Value = TxRecord> {
    (
        vec(any::<u8>(), 0..120),
        any::<u32>(),
        vec(any::<u32>(), 0..6),
        any::<u32>(),
    )
        .prop_map(|(prefix, outputs_at, global_indexes, sig_len)| TxRecord {
            prefix,
            outputs_at,
            global_indexes,
            sig_len,
        })
}

fn block_record() -> impl Strategy<Value = BlockRecord> {
    (
        vec(any::<u8>(), 0..300),
        any::<u32>(),
        vec(any::<u32>(), 0..6),
        vec(tx_record(), 0..5),
    )
        .prop_map(
            |(blob, miner_outputs_at, miner_global_indexes, txs)| BlockRecord {
                blob,
                miner_outputs_at,
                miner_global_indexes,
                txs,
            },
        )
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(cases(128)))]

    #[test]
    fn block_records_round_trip(rec in block_record()) {
        let enc = rec.encode();
        prop_assert_eq!(BlockRecord::decode(&enc).unwrap(), rec);
        for cut in 0..enc.len() {
            prop_assert!(BlockRecord::decode(&enc[..cut]).is_err(), "cut {}", cut);
        }
        let mut longer = enc.clone();
        longer.push(0);
        prop_assert!(BlockRecord::decode(&longer).is_err());
    }

    #[test]
    fn chunks_round_trip(records in vec(vec(any::<u8>(), 0..80), 0..12)) {
        let body = encode_chunk(&records);
        let parts = decode_chunk(&body).unwrap();
        prop_assert_eq!(parts.len(), records.len());
        for (a, b) in parts.iter().zip(&records) {
            prop_assert_eq!(*a, &b[..]);
        }
        for cut in 0..body.len() {
            prop_assert!(decode_chunk(&body[..cut]).is_err(), "cut {}", cut);
        }
    }

    #[test]
    fn garbage_records_and_chunks_never_panic(garbage in vec(any::<u8>(), 0..1024), lead in any::<bool>()) {
        let mut input = garbage;
        if lead {
            input.insert(0, 1); // the record / chunk version byte gets the decoder further
        }
        let (_, p) = peak_alloc(|| BlockRecord::decode(&input).is_ok());
        prop_assert!(p <= ALLOC_FACTOR * input.len() + ALLOC_SLACK, "record: {} bytes", p);
        let (_, p) = peak_alloc(|| decode_chunk(&input).map(|c| c.len()).ok());
        prop_assert!(p <= ALLOC_FACTOR * input.len() + ALLOC_SLACK, "chunk: {} bytes", p);
        for off in [0usize, 1, input.len() / 2, input.len(), input.len() + 3] {
            let ((), p) = peak_alloc(|| {
                let _ = stripped_amounts(&input, off, 1 << 40);
                let _ = unstrip_output_keys(&input, off, 1 << 40, |_, _| Ok([7u8; 32]));
            });
            prop_assert!(p <= ALLOC_FACTOR * input.len() + ALLOC_SLACK, "unstrip: {} bytes", p);
        }
    }

    #[test]
    fn varints_round_trip(v in interesting_u64(), garbage in vec(any::<u8>(), 0..16)) {
        let mut b = vec![0xaa];
        put_varint(&mut b, v);
        prop_assert_eq!(b.len() - 1, varint_len(v));
        prop_assert_eq!(varint_at(&b, 1), Some((v, varint_len(v))));
        if let Some((_, n)) = varint_at(&garbage, 0) {
            prop_assert!(n <= 10 && n <= garbage.len());
        }
        prop_assert_eq!(varint_at(&garbage, garbage.len() + 5), None);
    }

    #[test]
    fn output_keys_strip_and_unstrip(
        head in vec(any::<u8>(), 0..40),
        tail in vec(any::<u8>(), 0..40),
        outputs in vec((interesting_u64(), any::<[u8; 32]>()), 1..6),
    ) {
        let mut blob = head;
        put_varint(&mut blob, outputs.len() as u64);
        for (a, k) in &outputs {
            put_varint(&mut blob, *a);
            blob.push(TAG_KEY_OUTPUT);
            blob.extend_from_slice(k);
        }
        blob.extend_from_slice(&tail);
        let (off, stripped) = strip_output_keys(&blob, &outputs).expect("strippable");
        prop_assert_eq!(stripped.len(), blob.len() - 32 * outputs.len());
        let amounts: Vec<u64> = outputs.iter().map(|o| o.0).collect();
        prop_assert_eq!(stripped_amounts(&stripped, off, outputs.len()).unwrap(), amounts);
        let back = unstrip_output_keys(&stripped, off, outputs.len(), |i, _| Ok(outputs[i].1)).unwrap();
        prop_assert_eq!(back, blob);
    }

    #[test]
    fn compression_round_trips(data in vec(any::<u8>(), 0..4096), repeat in 1usize..8) {
        let data: Vec<u8> = data.iter().copied().cycle().take(data.len() * repeat).collect();
        let mut c = Compressor::new(DEFAULT_LEVEL).unwrap();
        let framed = c.compress(&data).unwrap();
        prop_assert_eq!(c.decompress(&framed).unwrap(), data.clone());
        let chunk = c.compress_chunk(&data).unwrap();
        prop_assert_eq!(c.decompress(&chunk).unwrap(), data);
        prop_assert_eq!(framed_dictionary_id(&framed), None);
    }

    #[test]
    fn corrupt_frames_are_errors(
        tag in prop_oneof![Just(0u8), Just(1), Just(2), any::<u8>()],
        len in 0u32..(1 << 20),
        payload in vec(any::<u8>(), 0..512),
    ) {
        let mut framed = vec![tag];
        framed.extend_from_slice(&len.to_le_bytes());
        framed.extend_from_slice(&payload);
        let mut c = Compressor::new(DEFAULT_LEVEL).unwrap();
        // declared length <= 1 MiB here; see `declared_length_is_bounded_by_the_payload`
        let _ = c.decompress(&framed);
        let _ = framed_dictionary_id(&framed);
        let _ = is_raw_frame(&framed);
        for cut in 0..framed.len().min(9) {
            let _ = c.decompress(&framed[..cut]);
        }
    }
}

/// Every 5-byte frame header declares the uncompressed length and zstd pre-allocates it: a 9-byte
/// corrupt blob used to make `decompress` reserve up to 4 GiB. The declared length is now checked
/// against what the payload can hold (at most 128 KiB per 3-byte zstd block header) before anything
/// is allocated.
#[test]
fn declared_length_is_bounded_by_the_payload() {
    let mut c = Compressor::new(DEFAULT_LEVEL).unwrap();
    for tag in [1u8, 2] {
        let mut framed = vec![tag];
        framed.extend_from_slice(&(512u32 << 20).to_le_bytes()); // claims 512 MiB
        if tag == 2 {
            framed.extend_from_slice(&0u32.to_le_bytes()); // dictionary id
        }
        framed.extend_from_slice(&[0x28, 0xb5, 0x2f, 0xfd]); // zstd magic, then nothing
        let (r, peak) = peak_alloc(|| c.decompress(&framed).is_ok());
        assert!(!r);
        assert!(
            peak < 1 << 20,
            "{peak} bytes allocated for a {}-byte blob",
            framed.len()
        );
    }
    // u32::MAX declared over a few hundred bytes of garbage: refused, nothing reserved
    let mut framed = vec![1u8];
    framed.extend_from_slice(&u32::MAX.to_le_bytes());
    framed.extend(std::iter::repeat_n(0xa5u8, 300));
    let (r, peak) = peak_alloc(|| c.decompress(&framed).is_ok());
    assert!(!r);
    assert!(peak < 1 << 20, "{peak} bytes allocated");
    // a real, highly compressible frame still decodes (the bound is never too tight)
    let zeros = vec![0u8; 8 << 20];
    let framed = c.compress(&zeros).unwrap();
    assert!(framed.len() < 4096, "{} bytes", framed.len());
    assert_eq!(c.decompress(&framed).unwrap(), zeros);
}
