//! Property and limit tests for the Levin codec and the epee messages: garbage never panics and
//! allocates a bounded amount, every message and nested epee storage round-trips, truncated input
//! only ever asks for more bytes, and every size/depth limit fails fast. Cases: `PROPTEST_CASES`
//! (default 64).
#![allow(unsafe_code)] // the counting global allocator below

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::time::{Duration, Instant};

use bytes::BytesMut;
use proptest::collection::vec;
use proptest::prelude::*;
use tokio_util::codec::{Decoder, Encoder};
use xkr_epee::epee_object;
use xkr_levin::header::{BucketHead, Flags, HEADER_SIZE};
use xkr_levin::{BucketError, LevinBucketCodec, LevinMessage, LevinMessageCodec, Protocol};
use xkr_p2p_legacy::PeerList;
use xkr_p2p_legacy::constants::{MAX_BLOCKS_IN_RESPONSE, P2P_LOCAL_GRAY_PEERLIST_LIMIT};
use xkr_p2p_legacy::wire::{
    AdminRequest, AdminResponse, BasicNodeData, CoreSyncData, HandshakeRequest, LegacyCommand,
    LegacyMessage, MissingTxs, NewBlock, NewLiteBlock, NewTransactions, PeerlistEntry,
    ProtocolMessage, RawBlockLegacy, RequestChain, RequestGetObjects, RequestTxPool,
    ResponseChainEntry, ResponseGetObjects, TimedSyncResponse,
};
use xkr_types::Hash;

// ------------------------------------------------------------------ allocation accounting

/// Peak heap bytes per input byte for epee/Levin decoding.
const ALLOC_FACTOR: usize = 64;
/// epee pre-allocates up to 2 MB (`max_upfront_capacity`) per open sequence whatever the
/// input size; at most a few sequences are open at once (static type nesting).
const ALLOC_SLACK: usize = 8 * 1024 * 1024;

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

// ------------------------------------------------------------------ helpers

const EPEE_HEADER: [u8; 9] = [0x01, 0x11, 0x01, 0x01, 0x01, 0x01, 0x02, 0x01, 0x01];
const LEVIN_SIGNATURE: u64 = 0x0101_0101_0101_2101;

/// A raw 33-byte Levin header.
fn levin_header(size: u64, command: u32, have_to_return: bool, flags: u32) -> Vec<u8> {
    let mut h = Vec::with_capacity(HEADER_SIZE);
    h.extend_from_slice(&LEVIN_SIGNATURE.to_le_bytes());
    h.extend_from_slice(&size.to_le_bytes());
    h.push(u8::from(have_to_return));
    h.extend_from_slice(&command.to_le_bytes());
    h.extend_from_slice(&0i32.to_le_bytes());
    h.extend_from_slice(&flags.to_le_bytes());
    h.extend_from_slice(&1u32.to_le_bytes());
    h
}

fn push_name(out: &mut Vec<u8>, name: &str) {
    out.push(name.len() as u8);
    out.extend_from_slice(name.as_bytes());
}

fn hash() -> impl Strategy<Value = Hash> {
    any::<[u8; 32]>().prop_map(Hash)
}

/// Tx blobs take >= `MIN_TX_BLOB_WIRE_SIZE` (32) wire bytes, like every real transaction.
fn tx_blob() -> impl Strategy<Value = Vec<u8>> {
    vec(any::<u8>(), 32..120)
}

fn raw_block() -> impl Strategy<Value = RawBlockLegacy> {
    (vec(any::<u8>(), 0..160), vec(tx_blob(), 0..3))
        .prop_map(|(block, txs)| RawBlockLegacy { block, txs })
}

fn sync_data() -> impl Strategy<Value = CoreSyncData> {
    (any::<u32>(), hash(), any::<u32>()).prop_map(|(current_height, top_id, pruning_seed)| {
        CoreSyncData {
            current_height,
            top_id,
            pruning_seed,
        }
    })
}

fn peer_entry() -> impl Strategy<Value = PeerlistEntry> {
    (any::<u32>(), any::<u32>(), any::<u64>(), any::<u64>()).prop_map(
        |(ip, port, id, last_seen)| PeerlistEntry {
            ip,
            port,
            id,
            last_seen,
        },
    )
}

fn protocol_message() -> impl Strategy<Value = ProtocolMessage> {
    prop_oneof![
        vec(tx_blob(), 0..5)
            .prop_map(|txs| ProtocolMessage::NewTransactions(NewTransactions { txs })),
        (raw_block(), any::<u32>(), any::<u32>()).prop_map(|(block, h, hop)| {
            ProtocolMessage::NewBlock(NewBlock {
                block,
                current_blockchain_height: h,
                hop,
            })
        }),
        (vec(hash(), 0..4), vec(hash(), 0..4)).prop_map(|(txs, blocks)| {
            ProtocolMessage::RequestGetObjects(RequestGetObjects { txs, blocks })
        }),
        (
            vec(tx_blob(), 0..3),
            vec(raw_block(), 0..4),
            vec(hash(), 0..4),
            any::<u32>()
        )
            .prop_map(|(txs, blocks, missed_ids, h)| {
                ProtocolMessage::ResponseGetObjects(ResponseGetObjects {
                    txs,
                    blocks,
                    missed_ids,
                    current_blockchain_height: h,
                })
            }),
        vec(hash(), 0..8)
            .prop_map(|block_ids| ProtocolMessage::RequestChain(RequestChain { block_ids })),
        (any::<u32>(), any::<u32>(), vec(hash(), 0..8)).prop_map(|(s, t, block_ids)| {
            ProtocolMessage::ResponseChainEntry(ResponseChainEntry {
                start_height: s,
                total_height: t,
                block_ids,
            })
        }),
        vec(hash(), 0..8).prop_map(|txs| ProtocolMessage::RequestTxPool(RequestTxPool { txs })),
        (any::<u32>(), any::<u32>(), vec(any::<u8>(), 0..200)).prop_map(|(h, hop, t)| {
            ProtocolMessage::NewLiteBlock(NewLiteBlock {
                current_blockchain_height: h,
                hop,
                block_template: t,
            })
        }),
        (any::<u32>(), hash(), vec(hash(), 0..6)).prop_map(|(h, block_hash, missing_txs)| {
            ProtocolMessage::MissingTxs(MissingTxs {
                current_blockchain_height: h,
                block_hash,
                missing_txs,
            })
        }),
    ]
}

fn legacy_message() -> impl Strategy<Value = LegacyMessage> {
    let node_data = (
        any::<[u8; 16]>(),
        any::<u8>(),
        any::<u64>(),
        any::<u64>(),
        any::<u32>(),
    )
        .prop_map(
            |(network_id, version, peer_id, local_time, my_port)| BasicNodeData {
                network_id,
                version,
                peer_id,
                local_time,
                my_port,
            },
        );
    prop_oneof![
        protocol_message().prop_map(LegacyMessage::Notification),
        (node_data, sync_data()).prop_map(|(node_data, payload_data)| {
            LegacyMessage::Request(AdminRequest::Handshake(HandshakeRequest {
                node_data,
                payload_data,
            }))
        }),
        (any::<u64>(), sync_data(), vec(peer_entry(), 0..6)).prop_map(
            |(local_time, payload_data, local_peerlist)| {
                LegacyMessage::Response(AdminResponse::TimedSync(TimedSyncResponse {
                    local_time,
                    payload_data,
                    local_peerlist,
                }))
            }
        ),
    ]
}

// ------------------------------------------------------------------ nested epee storages

#[derive(Debug, Clone, PartialEq, Eq)]
struct Leaf {
    id: u64,
    small: u8,
    signed: i32,
    flag: bool,
    blob: Vec<u8>,
    name: String,
    nums: Vec<u32>,
}

epee_object!(
    Leaf,
    id: u64,
    small: u8,
    signed: i32,
    flag: bool,
    blob: Vec<u8>,
    name: String,
    nums: Vec<u32>,
);

#[derive(Debug, Clone, PartialEq, Eq)]
struct Branch {
    leaf: Leaf,
    leaves: Vec<Leaf>,
    tags: Vec<String>,
    blobs: Vec<Vec<u8>>,
}

epee_object!(
    Branch,
    leaf: Leaf,
    leaves: Vec<Leaf>,
    tags: Vec<String>,
    blobs: Vec<Vec<u8>>,
);

#[derive(Debug, Clone, PartialEq, Eq)]
struct Root {
    branch: Branch,
    branches: Vec<Branch>,
    total: u64,
    deltas: Vec<i64>,
}

epee_object!(
    Root,
    branch: Branch,
    branches: Vec<Branch>,
    total: u64,
    deltas: Vec<i64>,
);

fn leaf() -> impl Strategy<Value = Leaf> {
    (
        any::<u64>(),
        any::<u8>(),
        any::<i32>(),
        any::<bool>(),
        vec(any::<u8>(), 0..40),
        any::<String>(),
        vec(any::<u32>(), 0..8),
    )
        .prop_map(|(id, small, signed, flag, blob, name, nums)| Leaf {
            id,
            small,
            signed,
            flag,
            blob,
            name,
            nums,
        })
}

fn branch() -> impl Strategy<Value = Branch> {
    (
        leaf(),
        vec(leaf(), 0..4),
        vec(any::<String>(), 0..4),
        vec(vec(any::<u8>(), 0..20), 0..4),
    )
        .prop_map(|(leaf, leaves, tags, blobs)| Branch {
            leaf,
            leaves,
            tags,
            blobs,
        })
}

fn root() -> impl Strategy<Value = Root> {
    (
        branch(),
        vec(branch(), 0..3),
        any::<u64>(),
        vec(any::<i64>(), 0..8),
    )
        .prop_map(|(branch, branches, total, deltas)| Root {
            branch,
            branches,
            total,
            deltas,
        })
}

/// Decodes `body` as every message type; nothing may panic and the heap stays bounded.
fn decode_every_type(body: &[u8]) {
    macro_rules! each {
        ($($t:ty),* $(,)?) => {$(
            let (_, peak) = peak_alloc(|| xkr_epee::from_bytes::<$t, _>(&mut &body[..]).is_ok());
            let bound = ALLOC_FACTOR * body.len() + ALLOC_SLACK;
            assert!(peak <= bound, "{}: {peak} bytes for {} input bytes", stringify!($t), body.len());
        )*};
    }
    each!(
        NewTransactions,
        NewBlock,
        RequestGetObjects,
        ResponseGetObjects,
        RequestChain,
        ResponseChainEntry,
        RequestTxPool,
        NewLiteBlock,
        MissingTxs,
        HandshakeRequest,
        TimedSyncResponse,
        Root,
    );
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(cases(64)))]

    #[test]
    fn levin_header_round_trips(
        size in any::<u64>(),
        have in any::<bool>(),
        command in any::<u32>(),
        return_code in any::<i32>(),
        flags in any::<u32>(),
        version in any::<u32>(),
        signature in any::<u64>(),
    ) {
        let head = BucketHead {
            signature,
            size,
            have_to_return_data: have,
            command: LegacyCommand::from(command),
            return_code,
            flags: Flags::from(flags),
            protocol_version: version,
        };
        let mut buf = BytesMut::new();
        head.write_bytes_into(&mut buf);
        prop_assert_eq!(buf.len(), HEADER_SIZE);
        let back = BucketHead::<LegacyCommand>::from_bytes(&mut buf);
        prop_assert_eq!(back, head);
        prop_assert_eq!(u32::from(back.command), command);
    }

    #[test]
    fn messages_round_trip_and_truncations_only_ask_for_more(msg in legacy_message()) {
        let mut codec = LevinMessageCodec::<LegacyMessage>::default();
        let mut buf = BytesMut::new();
        codec.encode(LevinMessage::Body(msg.clone()), &mut buf).unwrap();
        let wire = buf.to_vec();
        let back = codec.decode(&mut buf).unwrap();
        prop_assert_eq!(back, Some(msg));
        prop_assert!(buf.is_empty());
        for cut in 0..wire.len() {
            let mut fresh = LevinMessageCodec::<LegacyMessage>::default();
            let mut part = BytesMut::from(&wire[..cut]);
            prop_assert!(matches!(fresh.decode(&mut part), Ok(None)), "cut {}", cut);
        }
    }

    #[test]
    fn nested_epee_storages_round_trip(v in root()) {
        let bytes = xkr_epee::to_bytes(v.clone()).unwrap();
        let back: Root = xkr_epee::from_bytes(&mut &bytes[..]).unwrap();
        prop_assert_eq!(back, v);
        // every truncation is an error, never a panic
        for cut in 0..bytes.len() {
            prop_assert!(xkr_epee::from_bytes::<Root, _>(&mut &bytes[..cut]).is_err());
        }
    }

    #[test]
    fn epee_garbage_never_panics(body in vec(any::<u8>(), 0..1024)) {
        decode_every_type(&body);
        let mut with_header = EPEE_HEADER.to_vec();
        with_header.extend_from_slice(&body);
        decode_every_type(&with_header);
    }

    #[test]
    fn mutated_messages_never_panic(msg in protocol_message(), flips in vec((any::<usize>(), any::<u8>()), 1..6)) {
        let mut codec = LevinMessageCodec::<LegacyMessage>::default();
        let mut buf = BytesMut::new();
        codec.encode(LevinMessage::Body(LegacyMessage::Notification(msg)), &mut buf).unwrap();
        let mut body = buf[HEADER_SIZE..].to_vec();
        for (i, x) in flips {
            let n = body.len();
            body[i % n] ^= x;
        }
        decode_every_type(&body);
    }

    #[test]
    fn levin_codec_garbage_never_panics(
        stream in vec(any::<u8>(), 0..2048),
        command in prop_oneof![Just(1001u32), Just(1002), Just(1003), Just(2001), Just(2002), Just(2004), any::<u32>()],
        have in any::<bool>(),
        flags in prop_oneof![Just(1u32), Just(2), Just(4), Just(8), Just(12), any::<u32>()],
        chunk in 1usize..64,
    ) {
        // a valid header in front gets the garbage into the body decoders
        let mut wire = levin_header(stream.len() as u64, command, have, flags);
        wire.extend_from_slice(&stream);
        for input in [&wire[..], &stream[..]] {
            let mut codec = LevinMessageCodec::<LegacyMessage>::default();
            let mut buf = BytesMut::new();
            let ((), peak) = peak_alloc(|| {
                for part in input.chunks(chunk) {
                    buf.extend_from_slice(part);
                    while let Ok(Some(_)) = codec.decode(&mut buf) {}
                }
            });
            let bound = ALLOC_FACTOR * input.len() + ALLOC_SLACK;
            prop_assert!(peak <= bound, "{} bytes for {} input bytes", peak, input.len());
        }
    }
}

// ------------------------------------------------------------------ limits

#[test]
fn oversized_levin_bodies_fail_on_the_header() {
    // per-command limit: a 65 KiB ping (limit 64 KiB)
    let mut codec = LevinBucketCodec::<LegacyCommand>::new(Protocol::default());
    let mut buf = BytesMut::from(&levin_header(65 * 1024, 1003, true, 1)[..]);
    assert!(matches!(
        codec.decode(&mut buf),
        Err(BucketError::BucketExceededMaxSize)
    ));
    // before the handshake: 256 KiB for any command
    let mut codec = LevinBucketCodec::<LegacyCommand>::new(Protocol::default());
    let mut buf = BytesMut::from(&levin_header(256 * 1024 + 1, 2001, false, 1)[..]);
    assert!(matches!(
        codec.decode(&mut buf),
        Err(BucketError::BucketExceededMaxSize)
    ));
    // the global maximum (100 MB) even after a handshake
    let mut codec = LevinBucketCodec::<LegacyCommand>::new(Protocol::default());
    let mut wire = levin_header(0, 1001, true, 1);
    wire.extend_from_slice(&levin_header(100_000_001, 2001, false, 1));
    let mut buf = BytesMut::from(&wire[..]);
    assert!(matches!(codec.decode(&mut buf), Ok(Some(_))));
    assert!(matches!(
        codec.decode(&mut buf),
        Err(BucketError::BucketExceededMaxSize)
    ));
    // wrong signature
    let mut bad = levin_header(0, 1003, true, 1);
    bad[0] ^= 1;
    let mut codec = LevinBucketCodec::<LegacyCommand>::new(Protocol::default());
    let mut buf = BytesMut::from(&bad[..]);
    assert!(matches!(
        codec.decode(&mut buf),
        Err(BucketError::InvalidHeaderSignature)
    ));
}

/// A 33-byte header claiming a 100 MB body must not make us reserve 100 MB
/// (`MAX_EAGER_RESERVE` in `vendor/levin/src/codec.rs`).
#[test]
fn declared_body_size_is_not_reserved_up_front() {
    let mut codec = LevinBucketCodec::<LegacyCommand>::new(Protocol::default());
    let mut wire = levin_header(0, 1001, true, 1); // handshake: lifts the 256 KiB limit
    wire.extend_from_slice(&levin_header(100_000_000, 2001, false, 1));
    let mut buf = BytesMut::from(&wire[..]);
    let (res, peak) = peak_alloc(|| {
        let first = codec.decode(&mut buf).map(|b| b.is_some());
        let second = codec.decode(&mut buf).map(|b| b.is_some());
        (first.ok(), second.ok())
    });
    assert_eq!(res, (Some(true), Some(false)));
    assert!(
        peak < 1024 * 1024,
        "{peak} bytes reserved for a body not sent"
    );
}

#[test]
fn deep_skipped_epee_objects_fail_fast() {
    fn nested(depth: usize) -> Vec<u8> {
        let mut e = EPEE_HEADER.to_vec();
        for _ in 0..depth {
            e.push(0x04); // 1 field
            push_name(&mut e, "x");
            e.push(0x0c); // OBJECT
        }
        e.push(0x00); // innermost object: 0 fields (objects have no end marker)
        e
    }
    // depth 1..=20 of unknown nested objects are skipped
    for depth in [1usize, 5, 20] {
        // the outermost level is the message itself, so `depth` levels are skipped objects
        let bytes = nested(depth);
        let r = xkr_epee::from_bytes::<NewTransactions, _>(&mut &bytes[..]);
        assert!(r.is_ok(), "depth {depth}: {r:?}");
    }
    for depth in [21usize, 64, 10_000] {
        let bytes = nested(depth);
        let t = Instant::now();
        assert!(xkr_epee::from_bytes::<NewTransactions, _>(&mut &bytes[..]).is_err());
        assert!(t.elapsed() < Duration::from_secs(1), "depth {depth}");
    }
}

#[test]
fn epee_count_and_length_limits() {
    // more than MAX_NUM_FIELDS (1000) fields: varint 1001 = (1001 << 2) | 1, two bytes
    let mut e = EPEE_HEADER.to_vec();
    e.extend_from_slice(&u16::try_from((1001 << 2) | 1).unwrap().to_le_bytes());
    assert!(xkr_epee::from_bytes::<NewTransactions, _>(&mut &e[..]).is_err());

    // a string claiming 2^61 bytes: rejected before allocation
    let mut e = EPEE_HEADER.to_vec();
    e.push(0x04);
    push_name(&mut e, "blockTemplate");
    e.push(0x0a);
    e.extend_from_slice(&((1u64 << 61 << 2) | 3).to_le_bytes());
    let (r, peak) = peak_alloc(|| xkr_epee::from_bytes::<NewLiteBlock, _>(&mut &e[..]).is_ok());
    assert!(!r);
    assert!(peak < 64 * 1024, "{peak}");

    // an array of tx strings claiming 2^61 entries: bounded up-front capacity, then an error
    let mut e = EPEE_HEADER.to_vec();
    e.push(0x04);
    push_name(&mut e, "txs");
    e.push(0x8a);
    e.extend_from_slice(&((1u64 << 61 << 2) | 3).to_le_bytes());
    let (r, peak) = peak_alloc(|| xkr_epee::from_bytes::<NewTransactions, _>(&mut &e[..]).is_ok());
    assert!(!r);
    assert!(peak <= 2_100_000, "{peak}");

    // more blocks than MAX_BLOCKS_IN_RESPONSE in one response
    let n = MAX_BLOCKS_IN_RESPONSE + 1;
    let mut e = EPEE_HEADER.to_vec();
    e.push(0x04);
    push_name(&mut e, "blocks");
    e.push(0x8c);
    e.extend_from_slice(&u16::try_from((n << 2) | 1).unwrap().to_le_bytes());
    e.extend(std::iter::repeat_n(0u8, n));
    assert!(xkr_epee::from_bytes::<ResponseGetObjects, _>(&mut &e[..]).is_err());
}

/// A peer list as large as a 1 MiB timed-sync response allows (43 690 entries) merges in
/// bounded time (`MAX_REMOTE_PEERLIST_ENTRIES`, one trim at the end).
#[test]
fn huge_remote_peer_lists_merge_quickly() {
    let entries: Vec<PeerlistEntry> = (0..43_690u32)
        .map(|i| PeerlistEntry {
            ip: u32::from_le_bytes([8, (i >> 16) as u8, (i >> 8) as u8, i as u8]),
            port: 11897,
            id: u64::from(i),
            last_seen: u64::from(i),
        })
        .collect();
    let mut pl = PeerList::new(false);
    // fill the gray list first so every insert would trigger a trim
    let pre: Vec<PeerlistEntry> = (0..P2P_LOCAL_GRAY_PEERLIST_LIMIT as u32)
        .map(|i| PeerlistEntry {
            ip: u32::from_le_bytes([9, (i >> 16) as u8, (i >> 8) as u8, i as u8]),
            port: 1,
            id: 0,
            last_seen: 100_000,
        })
        .collect();
    // (one by one: a remote list adds at most `MAX_NEW_GRAY_PER_MERGE` new addresses)
    for e in pre {
        pl.append_gray(e);
    }
    assert_eq!(pl.gray.len(), P2P_LOCAL_GRAY_PEERLIST_LIMIT);
    let t = Instant::now();
    pl.merge_remote(&entries, 100_000, 100_000).unwrap();
    assert!(t.elapsed() < Duration::from_secs(5), "{:?}", t.elapsed());
    assert!(pl.gray.len() <= P2P_LOCAL_GRAY_PEERLIST_LIMIT);
}
