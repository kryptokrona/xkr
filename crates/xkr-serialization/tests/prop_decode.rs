//! Property tests for the CryptoNote decoders.
//!
//! * `decode(arbitrary bytes)` never panics and never allocates more than a bound linear in the
//!   input length ([`ALLOC_FACTOR`] x len + [`ALLOC_SLACK`]);
//! * `decode(encode(x)) == x` for generated transactions (versions 0/1 with coinbase and key
//!   inputs and legacy ring signatures, version 2 with tagged outputs and CLSAG signatures, any
//!   extra) and blocks (major 1 and merge-mined major >= 2 with a parent block);
//! * every truncation and one trailing byte is an error, not a panic;
//! * varints round-trip at every width boundary and fail exactly like C++ `readVarint`.
//!
//! The generator is a small seeded PRNG instead of `proptest` so this crate needs no new
//! dev-dependency. Case count: `PROPTEST_CASES` (default 256); seed: `XKR_PROP_SEED`.
#![allow(unsafe_code)] // the counting global allocator below

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use xkr_serialization::varint::{VarintError, read_varint, varint_bytes, write_varint};
use xkr_serialization::{
    DecodeError, MAX_BLOCK_MAJOR_VERSION, decode_block, decode_raw_block, decode_transaction,
    decode_transaction_prefix, encode_block, encode_raw_block, encode_transaction,
    encode_transaction_prefix,
};
use xkr_types::{
    Block, BlockHeader, ClsagSignature, EcScalar, Hash, KeyImage, ParentBlock, PublicKey, RawBlock,
    Signature, TX_VERSION_2, Transaction, TransactionPrefix, TxInput, TxOutput, TxOutputTarget,
};

// ------------------------------------------------------------------ allocation accounting

/// Peak heap bytes a decoder may hold per input byte. Before the coinbase shape rule the largest
/// in-memory/wire ratio was a coinbase input (2 wire bytes for a 64-byte `TxInput` plus a reserved
/// signature slot); a coinbase input now only decodes as the single input, so the remaining large
/// ratios are an empty `Vec<u8>` per 1-byte length in `decode_raw_block` (24x) and the per-input
/// signature slots. The general bound is kept loose; the coinbase input has its own 8x test.
const ALLOC_FACTOR: usize = 96;
/// Constant allowance (small fixed buffers, error values).
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
// const-initialised thread-locals without destructors (no allocation, no re-entrancy).
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

/// Runs `f` and returns its result with the peak number of heap bytes it held (this thread).
fn peak_alloc<R>(f: impl FnOnce() -> R) -> (R, usize) {
    let _ = CURRENT.try_with(|c| c.set(0));
    let _ = PEAK.try_with(|p| p.set(0));
    let _ = ACTIVE.try_with(|a| a.set(true));
    let r = f();
    let _ = ACTIVE.try_with(|a| a.set(false));
    (r, PEAK.try_with(Cell::get).unwrap_or(0))
}

fn assert_bounded(what: &str, input_len: usize, peak: usize) {
    let bound = ALLOC_FACTOR * input_len + ALLOC_SLACK;
    assert!(
        peak <= bound,
        "{what}: {peak} heap bytes for {input_len} input bytes (bound {bound})"
    );
}

// ------------------------------------------------------------------ deterministic generator

/// splitmix64.
struct Rng(u64);

impl Rng {
    fn new(salt: u64) -> Self {
        let seed = std::env::var("XKR_PROP_SEED")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0x5eed_0fc0_ffee_u64);
        Self(seed ^ salt.wrapping_mul(0x9e37_79b9_7f4a_7c15))
    }

    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    /// Uniform in `0..n` (`n > 0`).
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }

    fn usize_below(&mut self, n: usize) -> usize {
        self.below(n as u64) as usize
    }

    fn bool(&mut self) -> bool {
        self.next() & 1 == 1
    }

    fn byte(&mut self) -> u8 {
        self.next() as u8
    }

    /// Values that stress varints: small, around 7-bit group boundaries, or anything.
    fn interesting_u64(&mut self) -> u64 {
        match self.below(4) {
            0 => self.below(300),
            1 => {
                let k = self.below(10) as u32;
                let base = 1u64.checked_shl(7 * k).unwrap_or(u64::MAX);
                base.wrapping_add(self.below(3)).wrapping_sub(1)
            }
            2 => u64::MAX - self.below(3),
            _ => self.next(),
        }
    }

    fn bytes(&mut self, max_len: usize) -> Vec<u8> {
        let n = self.usize_below(max_len + 1);
        (0..n).map(|_| self.byte()).collect()
    }

    fn arr32(&mut self) -> [u8; 32] {
        std::array::from_fn(|_| self.byte())
    }

    fn arr64(&mut self) -> [u8; 64] {
        std::array::from_fn(|_| self.byte())
    }
}

fn cases() -> usize {
    std::env::var("PROPTEST_CASES")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(256)
}

// ------------------------------------------------------------------ generators

fn gen_key_input(r: &mut Rng) -> TxInput {
    let ring = r.usize_below(6);
    TxInput::Key {
        amount: r.interesting_u64(),
        output_indexes: (0..ring).map(|_| r.interesting_u64() as u32).collect(),
        key_image: KeyImage(r.arr32()),
    }
}

fn gen_input(r: &mut Rng) -> TxInput {
    if r.below(4) == 0 {
        TxInput::Coinbase {
            height: r.interesting_u64() as u32,
        }
    } else {
        gen_key_input(r)
    }
}

/// An `extra` that is sometimes well-formed (public key, nonce, merge-mining tag, padding) and
/// sometimes plain garbage: the transaction codec must carry it verbatim either way.
fn gen_extra(r: &mut Rng) -> Vec<u8> {
    let mut e = Vec::new();
    if r.bool() {
        return r.bytes(80);
    }
    if r.bool() {
        e.push(0x01);
        e.extend_from_slice(&r.arr32());
    }
    if r.bool() {
        let nonce = r.bytes(40);
        e.push(0x02);
        e.push(nonce.len() as u8);
        e.extend_from_slice(&nonce);
    }
    if r.bool() {
        let pad = r.usize_below(8);
        e.extend(std::iter::repeat_n(0u8, pad + 1));
    }
    e
}

#[allow(clippy::field_reassign_with_default)] // robust against new prefix fields
fn gen_prefix(r: &mut Rng, coinbase_only: bool) -> TransactionPrefix {
    let mut p = TransactionPrefix::default();
    p.version = u8::from(r.bool()); // 0 or 1: what every deserializer version accepts
    p.unlock_time = r.interesting_u64();
    p.inputs = if coinbase_only {
        vec![TxInput::Coinbase {
            height: r.interesting_u64() as u32,
        }]
    } else {
        // A coinbase input only ever stands alone (the decoder refuses other shapes).
        let n = r.usize_below(5);
        if n == 1 {
            vec![gen_input(r)]
        } else {
            (0..n).map(|_| gen_key_input(r)).collect()
        }
    };
    let n_out = r.usize_below(5);
    p.outputs = (0..n_out)
        .map(|_| TxOutput {
            amount: r.interesting_u64(),
            target: TxOutputTarget::Key(PublicKey(r.arr32())),
        })
        .collect();
    p.extra = gen_extra(r);
    p
}

/// A version-2 transaction: key inputs only (or none / one coinbase), tagged outputs, one CLSAG
/// signature (`s` per ring member + `c1`) per key input.
#[allow(clippy::field_reassign_with_default)]
fn gen_tx_v2(r: &mut Rng) -> Transaction {
    let mut p = gen_prefix(r, false);
    p.version = TX_VERSION_2;
    match r.below(4) {
        0 => p.inputs.clear(),
        1 => {
            p.inputs = vec![TxInput::Coinbase {
                height: r.interesting_u64() as u32,
            }];
        }
        _ => p.inputs.retain(|i| matches!(i, TxInput::Key { .. })),
    }
    for o in &mut p.outputs {
        o.target = TxOutputTarget::TaggedKey {
            key: *o.target.key(),
            view_tag: r.byte(),
        };
    }
    let clsag = if matches!(p.inputs.as_slice(), [] | [TxInput::Coinbase { .. }]) {
        Vec::new()
    } else {
        p.inputs
            .iter()
            .map(|i| ClsagSignature {
                s: (0..i.signature_count())
                    .map(|_| EcScalar(r.arr32()))
                    .collect(),
                c1: EcScalar(r.arr32()),
            })
            .collect()
    };
    let mut tx = Transaction::default();
    tx.prefix = p;
    tx.clsag_signatures = clsag;
    tx
}

/// The signature layout the decoder produces: none for "no inputs" or "one coinbase input",
/// otherwise one vector per input with `signature_count()` entries. A third of the
/// non-coinbase transactions are version 2.
#[allow(clippy::field_reassign_with_default)]
fn gen_tx(r: &mut Rng, coinbase_only: bool) -> Transaction {
    if !coinbase_only && r.below(3) == 0 {
        return gen_tx_v2(r);
    }
    let prefix = gen_prefix(r, coinbase_only);
    let none = matches!(prefix.inputs.as_slice(), [] | [TxInput::Coinbase { .. }]);
    let signatures = if none {
        Vec::new()
    } else {
        prefix
            .inputs
            .iter()
            .map(|i| {
                (0..i.signature_count())
                    .map(|_| Signature(r.arr64()))
                    .collect()
            })
            .collect()
    };
    let mut tx = Transaction::default();
    tx.prefix = prefix;
    tx.signatures = signatures;
    tx
}

/// `0x03 | varint len | varint depth | root[32]` (C++ `TransactionExtraMergeMiningTag`).
fn mm_tag_extra(depth: u64, root: [u8; 32]) -> Vec<u8> {
    let mut body = varint_bytes(depth);
    body.extend_from_slice(&root);
    let mut e = vec![0x03];
    write_varint(&mut e, body.len() as u64);
    e.extend_from_slice(&body);
    e
}

#[allow(clippy::field_reassign_with_default)]
fn gen_block(r: &mut Rng) -> Block {
    let major = 1 + r.below(u64::from(MAX_BLOCK_MAJOR_VERSION)) as u8;
    let mut header = BlockHeader::default();
    header.major_version = major;
    header.minor_version = r.byte();
    header.timestamp = r.interesting_u64();
    header.prev_hash = Hash(r.arr32());
    header.nonce = r.next() as u32;

    let parent_block = (major >= 2).then(|| {
        let tx_count = 1 + r.below(16) as u16;
        let depth = r.below(4);
        let mut base = gen_tx(r, true);
        let mut extra = mm_tag_extra(depth, r.arr32());
        if r.bool() {
            extra.push(0x01);
            extra.extend_from_slice(&r.arr32());
        }
        base.prefix.extra = extra;
        base.prefix.version = r.byte() % 3; // the parent miner tx has no version check
        base.signatures = Vec::new();
        let mut pb = ParentBlock::default();
        pb.major_version = r.byte();
        pb.minor_version = r.byte();
        pb.prev_hash = Hash(r.arr32());
        pb.transaction_count = tx_count;
        pb.base_transaction_branch = (0..xkr_crypto::tree_depth(usize::from(tx_count)))
            .map(|_| Hash(r.arr32()))
            .collect();
        pb.base_transaction = base;
        pb.blockchain_branch = (0..depth).map(|_| Hash(r.arr32())).collect();
        pb
    });

    let mut block = Block::default();
    block.header = header;
    block.parent_block = parent_block;
    block.miner_tx = gen_tx(r, true);
    let n = r.usize_below(6);
    block.tx_hashes = (0..n).map(|_| Hash(r.arr32())).collect();
    block
}

/// Flips, inserts or deletes a few bytes of `blob` (structured garbage near valid input).
fn mutate(r: &mut Rng, blob: &[u8]) -> Vec<u8> {
    let mut b = blob.to_vec();
    for _ in 0..=r.below(4) {
        match r.below(4) {
            0 if !b.is_empty() => {
                let i = r.usize_below(b.len());
                b[i] ^= 1 << r.below(8);
            }
            1 => {
                let i = r.usize_below(b.len() + 1);
                // a maximal varint: huge counts / lengths
                for (k, byte) in [0xffu8, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x01]
                    .into_iter()
                    .enumerate()
                {
                    b.insert(i + k, byte);
                }
            }
            2 if !b.is_empty() => {
                let i = r.usize_below(b.len());
                b.remove(i);
            }
            _ => {
                let i = r.usize_below(b.len() + 1);
                b.insert(i, r.byte());
            }
        }
    }
    b
}

fn decode_all_bounded(input: &[u8]) {
    let (_, p) = peak_alloc(|| decode_block(input).is_ok());
    assert_bounded("decode_block", input.len(), p);
    let (_, p) = peak_alloc(|| decode_transaction(input).is_ok());
    assert_bounded("decode_transaction", input.len(), p);
    let (_, p) = peak_alloc(|| decode_transaction_prefix(input).is_ok());
    assert_bounded("decode_transaction_prefix", input.len(), p);
    let (_, p) = peak_alloc(|| decode_raw_block(input).is_ok());
    // RawBlock copies every slice once: <= 1x plus a 24-byte Vec per (>= 1 byte) transaction.
    assert_bounded("decode_raw_block", input.len(), p);
}

// ------------------------------------------------------------------ properties

#[test]
fn random_bytes_never_panic_and_allocate_linearly() {
    let mut r = Rng::new(1);
    for _ in 0..cases() {
        let input = r.bytes(1024);
        decode_all_bounded(&input);
        // garbage that starts like a transaction / block header gets further into the decoder
        let mut tx_like = vec![1, r.byte(), r.byte()];
        tx_like.extend_from_slice(&input);
        decode_all_bounded(&tx_like);
    }
}

#[test]
fn mutated_valid_encodings_never_panic_and_allocate_linearly() {
    let mut r = Rng::new(2);
    for _ in 0..cases() {
        let tx = encode_transaction(&gen_tx(&mut r, false)).expect("generated tx encodes");
        decode_all_bounded(&mutate(&mut r, &tx));
        let block = encode_block(&gen_block(&mut r)).expect("generated block encodes");
        decode_all_bounded(&mutate(&mut r, &block));
    }
}

#[test]
fn transactions_round_trip() {
    let mut r = Rng::new(3);
    for case in 0..cases() {
        let coinbase_only = r.below(8) == 0;
        let tx = gen_tx(&mut r, coinbase_only);
        let blob = encode_transaction(&tx).expect("generated tx encodes");
        assert_eq!(decode_transaction(&blob).as_ref(), Ok(&tx), "case {case}");
        let prefix = encode_transaction_prefix(&tx.prefix);
        assert_eq!(
            decode_transaction_prefix(&prefix).as_ref(),
            Ok(&tx.prefix),
            "case {case}"
        );
        assert!(blob.starts_with(&prefix), "case {case}: prefix is a prefix");
    }
}

#[test]
fn blocks_round_trip() {
    let mut r = Rng::new(4);
    for case in 0..cases() {
        let block = gen_block(&mut r);
        let blob = encode_block(&block).expect("generated block encodes");
        assert_eq!(decode_block(&blob).as_ref(), Ok(&block), "case {case}");
    }
}

#[test]
fn raw_blocks_round_trip() {
    let mut r = Rng::new(5);
    for case in 0..cases() {
        let n = r.usize_below(5);
        let raw = RawBlock {
            block: r.bytes(200),
            transactions: (0..n).map(|_| r.bytes(100)).collect(),
        };
        let blob = encode_raw_block(&raw);
        assert_eq!(decode_raw_block(&blob).as_ref(), Ok(&raw), "case {case}");
    }
}

#[test]
fn every_truncation_and_trailing_byte_is_an_error() {
    let mut r = Rng::new(6);
    for case in 0..cases().min(64) {
        let tx = encode_transaction(&gen_tx(&mut r, false)).expect("encodes");
        for cut in 0..tx.len() {
            assert!(
                decode_transaction(&tx[..cut]).is_err(),
                "case {case} cut {cut}"
            );
        }
        let mut longer = tx.clone();
        longer.push(r.byte());
        assert!(matches!(
            decode_transaction(&longer),
            Err(DecodeError::TrailingBytes { trailing: 1, .. })
        ));

        let block = encode_block(&gen_block(&mut r)).expect("encodes");
        for cut in 0..block.len() {
            assert!(
                decode_block(&block[..cut]).is_err(),
                "case {case} cut {cut}"
            );
        }
        let mut longer = block.clone();
        longer.push(0);
        assert!(decode_block(&longer).is_err(), "case {case}");
    }
}

/// Counts and lengths that cannot fit the remaining input fail before any allocation.
#[test]
fn huge_counts_fail_before_allocating() {
    let max = [0xffu8, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x01]; // u64::MAX
    let big = [0xffu8, 0xff, 0xff, 0xff, 0x0f]; // 2^32 - 1
    let cases: Vec<(&str, Vec<u8>)> = vec![
        ("vin count", [&[1u8, 0][..], &max[..]].concat()),
        ("vout count", [&[1u8, 0, 0][..], &big[..]].concat()),
        (
            "key offsets",
            [&[1u8, 0, 1, 2, 5][..], &big[..], &[0u8; 8][..]].concat(),
        ),
        ("extra length", [&[1u8, 0, 0, 0][..], &max[..]].concat()),
        ("raw block size", max.to_vec()),
        ("raw tx count", [&[0u8][..], &big[..]].concat()),
    ];
    for (what, input) in cases {
        let (tx, p1) = peak_alloc(|| decode_transaction(&input));
        let (raw, p2) = peak_alloc(|| decode_raw_block(&input));
        assert!(tx.is_err() && raw.is_err(), "{what}");
        assert!(p1 < 4096 && p2 < 4096, "{what}: {p1}/{p2} bytes allocated");
        if what != "raw block size" && what != "raw tx count" {
            assert!(
                matches!(
                    tx,
                    Err(DecodeError::LengthTooLarge { .. } | DecodeError::VarintOverflow { .. })
                ),
                "{what}: {tx:?}"
            );
        }
    }

    // block with u32::MAX transaction hashes after a valid header + miner tx
    let mut r = Rng::new(7);
    let mut block = gen_block(&mut r);
    block.tx_hashes.clear();
    let mut blob = encode_block(&block).expect("encodes");
    blob.pop(); // the zero tx_hashes count
    blob.extend_from_slice(&big);
    let (res, p) = peak_alloc(|| decode_block(&blob));
    assert!(
        matches!(res, Err(DecodeError::LengthTooLarge { .. })),
        "{res:?}"
    );
    assert!(p < 4096, "{p} bytes allocated");
}

#[test]
fn merge_mining_depth_limit() {
    let mut r = Rng::new(8);
    let mut block = gen_block(&mut r);
    while block.header.major_version < 2 {
        block = gen_block(&mut r);
    }
    let blob = encode_block(&block).expect("encodes");
    let pb = block.parent_block.as_ref().expect("major >= 2");
    let old_tag = mm_tag_extra(pb.blockchain_branch.len() as u64, [0; 32]);
    // Rewrite the mm tag depth to 257 (> 256): `Wrong merge mining tag depth`.
    let extra = &pb.base_transaction.prefix.extra;
    let pos = blob
        .windows(extra.len())
        .position(|w| w == extra.as_slice())
        .expect("extra is in the blob");
    let mut bad = blob[..pos - 1].to_vec(); // drop the 1-byte extra length too
    let mut new_extra = mm_tag_extra(257, [0; 32]);
    new_extra.extend_from_slice(&extra[old_tag.len()..]);
    write_varint(&mut bad, new_extra.len() as u64);
    bad.extend_from_slice(&new_extra);
    bad.extend_from_slice(&blob[pos + extra.len()..]);
    assert!(extra.len() < 0x80, "1-byte extra length assumed");
    assert_eq!(
        decode_block(&bad),
        Err(DecodeError::MergeMiningDepthTooLarge(257))
    );
}

#[test]
fn varints_at_every_width_boundary() {
    for bits in [8u32, 16, 32, 64] {
        let max = if bits == 64 {
            u64::MAX
        } else {
            (1u64 << bits) - 1
        };
        let mut probes = vec![0, 1, max, max - 1, max / 2, max / 2 + 1];
        for k in 1..=9u32 {
            let b = 1u64.checked_shl(7 * k).unwrap_or(u64::MAX);
            probes.extend([b.wrapping_sub(1), b, b.wrapping_add(1)]);
        }
        for v in probes {
            let enc = varint_bytes(v);
            let mut pos = 0;
            let got = read_varint(&enc, &mut pos, bits);
            if v <= max {
                assert_eq!(got, Ok(v), "bits {bits} value {v}");
                assert_eq!(pos, enc.len());
                for cut in 0..enc.len() {
                    let mut p = 0;
                    assert_eq!(
                        read_varint(&enc[..cut], &mut p, bits),
                        Err(VarintError::Eof)
                    );
                }
            } else {
                assert_eq!(got, Err(VarintError::Overflow), "bits {bits} value {v}");
            }
            // non-canonical: an extra zero group
            if v <= max >> 7 {
                let mut nc = enc.clone();
                *nc.last_mut().expect("non-empty") |= 0x80;
                nc.push(0);
                let mut p = 0;
                assert_eq!(
                    read_varint(&nc, &mut p, bits),
                    Err(VarintError::NonCanonical),
                    "bits {bits} value {v}"
                );
            }
        }
    }
    // never more than 10 bytes are read for a u64, whatever follows
    let mut r = Rng::new(9);
    for _ in 0..cases() {
        let input = r.bytes(16);
        let mut pos = 0;
        let _ = read_varint(&input, &mut pos, 64);
        assert!(pos <= 10, "read {pos} bytes");
    }
}

/// A coinbase input is 2 bytes on the wire (`0xff` + height) but decodes to a 64-byte `TxInput`
/// plus a reserved 24-byte signature vector. It is legal only as the single input of a miner
/// transaction (C++ `validateBlock`, `validateTransactionInputs`), so the decoder now refuses it
/// next to other inputs and counts `vin` with the 35-byte key-input minimum once there are two or
/// more inputs. Without that rule a 100 MB `NOTIFY_NEW_TRANSACTIONS` blob of them would
/// cost ~4.4 GB.
#[test]
fn coinbase_inputs_do_not_amplify_memory() {
    let n = 100_000u64;
    let mut blob = vec![1u8, 0]; // version 1, unlock_time 0
    write_varint(&mut blob, n);
    for _ in 0..n {
        blob.extend_from_slice(&[0xff, 0x00]); // coinbase input, height 0
    }
    blob.extend_from_slice(&[0, 0]); // no outputs, empty extra
    let (res, peak) = peak_alloc(|| decode_transaction(&blob));
    assert!(res.is_err(), "{res:?}");
    let bound = 8 * blob.len() + ALLOC_SLACK;
    assert!(
        peak <= bound,
        "{peak} heap bytes for a {}-byte blob (bound {bound})",
        blob.len()
    );
    // The same bound for the prefix decoder and for a block whose miner transaction has them.
    let (res, peak) = peak_alloc(|| decode_transaction_prefix(&blob));
    assert!(res.is_err(), "{res:?}");
    assert!(peak <= bound, "prefix: {peak} heap bytes (bound {bound})");
}

/// Coinbase shape rule: a coinbase input next to any other input fails with
/// `CoinbaseInputInSignedTransaction(index)` wherever it sits (or, when the bytes after `vin`
/// cannot even hold that many key inputs, with `LengthTooLarge` before anything is allocated);
/// a lone coinbase input (miner transaction) and transactions of key inputs only still decode.
#[test]
fn coinbase_input_only_as_single_input() {
    let key = |b: u8| TxInput::Key {
        amount: 7,
        output_indexes: vec![1, 2],
        key_image: KeyImage([b; 32]),
    };
    let coinbase = TxInput::Coinbase { height: 3 };
    for (inputs, bad) in [
        (vec![coinbase.clone(), key(1)], Some(0)),
        (vec![key(1), coinbase.clone()], Some(1)),
        (vec![coinbase.clone(), coinbase.clone()], Some(0)),
        (vec![key(1), key(2), coinbase.clone()], Some(2)),
        (vec![coinbase.clone()], None),
        (vec![key(1), key(2)], None),
    ] {
        let p = TransactionPrefix {
            version: 1,
            inputs: inputs.clone(),
            ..TransactionPrefix::default()
        };
        let mut blob = encode_transaction_prefix(&p);
        let res = decode_transaction_prefix(&blob);
        match bad {
            Some(i) => {
                assert!(
                    matches!(
                        res,
                        Err(DecodeError::CoinbaseInputInSignedTransaction(_)
                            | DecodeError::LengthTooLarge { .. })
                    ),
                    "{inputs:?}: {res:?}"
                );
                // With room for ring signatures behind the prefix only the shape decides.
                blob.extend_from_slice(&[0u8; 8 * 64]);
                assert_eq!(
                    decode_transaction(&blob),
                    Err(DecodeError::CoinbaseInputInSignedTransaction(i)),
                    "{inputs:?}"
                );
            }
            None => assert_eq!(res.as_ref(), Ok(&p), "{inputs:?}"),
        }
    }
}
