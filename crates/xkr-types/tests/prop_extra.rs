//! Property tests for the transaction `extra` parsers: garbage never panics and allocates linearly,
//! the partial and strict parsers agree, well-formed extras (any field order, padding last) parse
//! back to their fields, duplicate and oversized fields behave like C++ `parseTransactionExtra`,
//! and the kryptokrona-utils (Hugin) layout round-trips.
//!
//! Seeded PRNG instead of `proptest` (no new dev-dependency). Cases: `PROPTEST_CASES`
//! (default 256); seed: `XKR_PROP_SEED`.
#![allow(unsafe_code)] // the counting global allocator below

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use xkr_types::extra::{
    TX_EXTRA_MERGE_MINING_TAG, TX_EXTRA_NONCE, TX_EXTRA_NONCE_DATA, TX_EXTRA_TAG_PUBKEY,
    UtilsExtraField, hugin_data, parse_utils_extra,
};
use xkr_types::{
    ExtraError, ExtraField, Hash, MergeMiningTag, PublicKey, merge_mining_tag, parse_extra,
    parse_extra_partial, payment_id, tx_public_key,
};

// ------------------------------------------------------------------ allocation accounting

const ALLOC_FACTOR: usize = 64;
const ALLOC_SLACK: usize = 16 * 1024;

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

fn assert_bounded(what: &str, input_len: usize, peak: usize) {
    let bound = ALLOC_FACTOR * input_len + ALLOC_SLACK;
    assert!(
        peak <= bound,
        "{what}: {peak} heap bytes for {input_len} input bytes (bound {bound})"
    );
}

// ------------------------------------------------------------------ generator

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

    fn bytes(&mut self, max_len: usize) -> Vec<u8> {
        let n = self.usize_below(max_len + 1);
        (0..n).map(|_| self.byte()).collect()
    }

    /// Bytes biased towards the extra tags (0..=7, 0x7f) and varint continuation bytes.
    fn extra_like(&mut self, max_len: usize) -> Vec<u8> {
        let n = self.usize_below(max_len + 1);
        (0..n)
            .map(|_| match self.below(4) {
                0 => self.below(8) as u8,
                1 => [0x7f, 0x80, 0xff, 0x21][self.usize_below(4)],
                _ => self.byte(),
            })
            .collect()
    }

    fn arr32(&mut self) -> [u8; 32] {
        std::array::from_fn(|_| self.byte())
    }
}

fn cases() -> usize {
    std::env::var("PROPTEST_CASES")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(256)
}

fn varint(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push((v as u8 & 0x7f) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

fn mm_field(depth: u64, root: [u8; 32]) -> Vec<u8> {
    let mut body = Vec::new();
    varint(&mut body, depth);
    body.extend_from_slice(&root);
    let mut e = vec![TX_EXTRA_MERGE_MINING_TAG];
    varint(&mut e, body.len() as u64);
    e.extend_from_slice(&body);
    e
}

fn check_all(input: &[u8]) {
    let ((fields, res), p) = peak_alloc(|| parse_extra_partial(input));
    assert_bounded("parse_extra_partial", input.len(), p);
    match parse_extra(input) {
        Ok(f) => {
            assert!(res.is_ok());
            assert_eq!(f, fields);
        }
        Err(e) => assert_eq!(res, Err(e)),
    }
    let first_pk = fields.iter().find_map(|f| match f {
        ExtraField::PublicKey(k) => Some(*k),
        _ => None,
    });
    assert_eq!(tx_public_key(input), first_pk);
    let _ = merge_mining_tag(input);
    let _ = payment_id(input);
    let (_, p) = peak_alloc(|| parse_utils_extra(input).len());
    assert_bounded("parse_utils_extra", input.len(), p);
    let (_, p) = peak_alloc(|| hugin_data(input).map(|d| d.len()));
    assert_bounded("hugin_data", input.len(), p);
}

// ------------------------------------------------------------------ properties

#[test]
fn garbage_never_panics_and_allocates_linearly() {
    let mut r = Rng::new(11);
    for _ in 0..cases() {
        check_all(&r.bytes(600));
        check_all(&r.extra_like(600));
    }
    // long repeating patterns terminate (every loop advances at least one byte)
    for pattern in [
        &[0x02u8, 0xff, 0xff, 0x03][..],
        &[0x03, 0xff],
        &[0x02, 0x7f, 0x80],
        &[0x01],
        &[0x07, 0x81, 0x80, 0x80],
    ] {
        let input: Vec<u8> = pattern.iter().copied().cycle().take(64 * 1024).collect();
        check_all(&input);
    }
}

#[test]
fn well_formed_extras_parse_back() {
    let mut r = Rng::new(12);
    for case in 0..cases() {
        let mut parts: Vec<(Vec<u8>, ExtraField)> = Vec::new();
        if r.bool() {
            let k = r.arr32();
            let mut b = vec![TX_EXTRA_TAG_PUBKEY];
            b.extend_from_slice(&k);
            parts.push((b, ExtraField::PublicKey(PublicKey(k))));
        }
        if r.bool() {
            let nonce = r.bytes(255);
            let mut b = vec![TX_EXTRA_NONCE, nonce.len() as u8];
            b.extend_from_slice(&nonce);
            parts.push((b, ExtraField::Nonce(nonce)));
        }
        if r.bool() {
            let depth = r.next() >> r.below(64);
            let root = r.arr32();
            parts.push((
                mm_field(depth, root),
                ExtraField::MergeMiningTag(MergeMiningTag {
                    depth,
                    merkle_root: Hash(root),
                }),
            ));
        }
        // random order
        for i in (1..parts.len()).rev() {
            let j = r.usize_below(i + 1);
            parts.swap(i, j);
        }
        let mut extra: Vec<u8> = parts.iter().flat_map(|(b, _)| b.clone()).collect();
        let mut expected: Vec<ExtraField> = parts.into_iter().map(|(_, f)| f).collect();
        if r.bool() {
            let size = 1 + r.usize_below(255);
            extra.extend(std::iter::repeat_n(0u8, size));
            expected.push(ExtraField::Padding { size });
        }
        assert_eq!(
            parse_extra(&extra),
            Ok(expected),
            "case {case}: {extra:02x?}"
        );
    }
}

#[test]
fn duplicate_fields_follow_cpp() {
    let mut r = Rng::new(13);
    for _ in 0..cases() {
        let (a, b) = (r.arr32(), r.arr32());
        // a second public key ends parsing successfully; whatever follows is never looked at
        let mut e = vec![TX_EXTRA_TAG_PUBKEY];
        e.extend_from_slice(&a);
        e.push(TX_EXTRA_TAG_PUBKEY);
        e.extend_from_slice(&b);
        e.extend(r.bytes(64));
        assert_eq!(
            parse_extra(&e),
            Ok(vec![ExtraField::PublicKey(PublicKey(a))])
        );

        // a second nonce likewise
        let mut e = vec![TX_EXTRA_NONCE, 1, 7, TX_EXTRA_NONCE, 200];
        e.extend(r.bytes(64));
        assert_eq!(parse_extra(&e), Ok(vec![ExtraField::Nonce(vec![7])]));

        // a second merge-mining tag byte is skipped alone, then parsing goes on
        let mut e = mm_field(1, a);
        e.push(TX_EXTRA_MERGE_MINING_TAG);
        e.push(TX_EXTRA_TAG_PUBKEY);
        e.extend_from_slice(&b);
        assert_eq!(
            parse_extra(&e),
            Ok(vec![
                ExtraField::MergeMiningTag(MergeMiningTag {
                    depth: 1,
                    merkle_root: Hash(a),
                }),
                ExtraField::PublicKey(PublicKey(b)),
            ])
        );
    }
}

#[test]
fn oversized_and_malformed_fields_are_errors() {
    assert_eq!(parse_extra(&[0u8; 256]), Err(ExtraError::PaddingTooLong));
    assert_eq!(
        parse_extra(&[0u8; 255]),
        Ok(vec![ExtraField::Padding { size: 255 }])
    );
    assert!(matches!(
        parse_extra(&[0, 0, 0, 1]),
        Err(ExtraError::NonZeroPadding { offset: 3 })
    ));
    let mut nonce = vec![TX_EXTRA_NONCE, 200];
    nonce.extend_from_slice(&[1; 10]);
    assert!(matches!(
        parse_extra(&nonce),
        Err(ExtraError::Truncated { field: "nonce", .. })
    ));
    let mut mm = vec![TX_EXTRA_MERGE_MINING_TAG];
    varint(&mut mm, 2201);
    mm.extend_from_slice(&[0; 40]);
    assert_eq!(
        parse_extra(&mm),
        Err(ExtraError::MergeMiningTagTooLong { len: 2201 })
    );
    let mut mm = vec![TX_EXTRA_MERGE_MINING_TAG];
    varint(&mut mm, u64::MAX);
    assert_eq!(
        parse_extra(&mm),
        Err(ExtraError::MergeMiningTagTooLong { len: u64::MAX })
    );
    // an 11-byte varint is malformed, not a panic
    let mut mm = vec![TX_EXTRA_MERGE_MINING_TAG];
    mm.extend_from_slice(&[0xff; 11]);
    assert!(matches!(
        parse_extra(&mm),
        Err(ExtraError::BadVarint { .. })
    ));
    // a truncated public key keeps nothing and fails
    assert!(matches!(
        parse_extra_partial(&[TX_EXTRA_TAG_PUBKEY, 1, 2]),
        (f, Err(ExtraError::Truncated { field: "public_key", .. })) if f.is_empty()
    ));
}

#[test]
fn hugin_layout_round_trips() {
    let mut r = Rng::new(14);
    for case in 0..cases() {
        let data = r.bytes(3000);
        let pk = r.arr32();
        let mut inner = vec![TX_EXTRA_NONCE_DATA];
        varint(&mut inner, data.len() as u64);
        inner.extend_from_slice(&data);
        let mut extra = vec![TX_EXTRA_TAG_PUBKEY];
        extra.extend_from_slice(&pk);
        extra.push(TX_EXTRA_NONCE);
        varint(&mut extra, inner.len() as u64);
        extra.extend_from_slice(&inner);
        assert_eq!(hugin_data(&extra), Some(data), "case {case}");
        assert_eq!(
            parse_utils_extra(&extra).first(),
            Some(&UtilsExtraField::PublicKey(PublicKey(pk)))
        );
        // the consensus parser never panics on the same bytes (it reads a raw-length nonce)
        check_all(&extra);
    }
}
