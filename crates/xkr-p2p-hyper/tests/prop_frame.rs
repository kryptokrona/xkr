//! Property and limit tests for the Rust <-> Bare bridge frames (`docs/hyper-bridge-protocol.md`):
//! every body (TLVs included) round-trips, garbage and truncated frames are errors or "need more
//! bytes", never panics, and `max_frame` is enforced on the length prefix before the body is
//! buffered. Cases: `PROPTEST_CASES` (default 128).
#![allow(unsafe_code)] // the counting global allocator below

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use bytes::{Bytes, BytesMut};
use proptest::collection::vec;
use proptest::prelude::*;
use tokio_util::codec::{Decoder, Encoder};
use xkr_p2p_hyper::frame::{
    Ack, Announce, Body, Close, DhtStatus, ErrorFrame, Frame, FrameCodec, FrameError, HEADER_LEN,
    Hello, Lookup, LookupPeer, LookupResult, Open, Opened, PeerInfo, Tlv, Unannounce, WireAddr,
};

// ------------------------------------------------------------------ allocation accounting

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

const KNOWN_TYPES: [u8; 16] = [
    0x01, 0x02, 0x03, 0x10, 0x11, 0x12, 0x20, 0x21, 0x30, 0x31, 0x32, 0x33, 0x34, 0x40, 0x41, 0x7f,
];

fn bytes(max: usize) -> impl Strategy<Value = Bytes> {
    vec(any::<u8>(), 0..max).prop_map(Bytes::from)
}

fn tlvs() -> impl Strategy<Value = Vec<Tlv>> {
    vec(
        (any::<u8>(), bytes(48)).prop_map(|(tag, value)| Tlv { tag, value }),
        0..4,
    )
}

fn addr() -> impl Strategy<Value = WireAddr> {
    (any::<u8>(), any::<u16>(), any::<[u8; 16]>()).prop_map(|(family, port, addr)| WireAddr {
        family,
        port,
        addr,
    })
}

fn arr64() -> impl Strategy<Value = [u8; 64]> {
    vec(any::<u8>(), 64).prop_map(|v| <[u8; 64]>::try_from(v).expect("64 bytes"))
}

fn body() -> impl Strategy<Value = Body> {
    prop_oneof![
        (
            any::<u16>(),
            any::<u16>(),
            any::<u32>(),
            any::<u32>(),
            any::<u16>(),
            any::<u16>(),
            any::<u64>(),
            tlvs()
        )
            .prop_map(
                |(major, minor, max_frame, initial_credit, workers, worker, features, tlvs)| {
                    Body::Hello(Hello {
                        major,
                        minor,
                        max_frame,
                        initial_credit,
                        workers,
                        worker,
                        features,
                        tlvs,
                    })
                }
            ),
        any::<u64>().prop_map(Body::Ping),
        any::<u64>().prop_map(Body::Pong),
        (
            any::<u32>(),
            any::<[u8; 32]>(),
            any::<[u8; 32]>(),
            any::<u32>(),
            tlvs()
        )
            .prop_map(|(request_id, remote_public_key, topic, open_flags, tlvs)| {
                Body::Open(Box::new(Open {
                    request_id,
                    remote_public_key,
                    topic,
                    open_flags,
                    tlvs,
                }))
            }),
        (
            any::<u32>(),
            any::<[u8; 32]>(),
            arr64(),
            any::<[u8; 32]>(),
            any::<bool>(),
            addr(),
            tlvs()
        )
            .prop_map(
                |(request_id, remote_public_key, handshake_hash, topic, initiator, addr, tlvs)| {
                    Body::Opened(Box::new(Opened {
                        request_id,
                        remote_public_key,
                        handshake_hash,
                        topic,
                        initiator,
                        addr,
                        tlvs,
                    }))
                }
            ),
        (any::<u8>(), any::<u32>(), any::<String>()).prop_map(|(reason, code, message)| {
            Body::Close(Close {
                reason,
                code,
                message,
            })
        }),
        bytes(512).prop_map(Body::Data),
        any::<u32>().prop_map(Body::Credit),
        (any::<u32>(), any::<[u8; 32]>(), any::<u8>(), tlvs()).prop_map(
            |(request_id, topic, mode, tlvs)| Body::Announce(Box::new(Announce {
                request_id,
                topic,
                mode,
                tlvs
            }))
        ),
        (any::<u32>(), any::<[u8; 32]>(), tlvs()).prop_map(|(request_id, topic, tlvs)| {
            Body::Unannounce(Unannounce {
                request_id,
                topic,
                tlvs,
            })
        }),
        (any::<u32>(), any::<[u8; 32]>(), any::<u16>(), tlvs()).prop_map(
            |(request_id, topic, max_peers, tlvs)| Body::Lookup(Lookup {
                request_id,
                topic,
                max_peers,
                tlvs
            })
        ),
        (
            any::<u32>(),
            vec(
                (any::<[u8; 32]>(), addr())
                    .prop_map(|(public_key, addr)| LookupPeer { public_key, addr }),
                0..6
            ),
            tlvs()
        )
            .prop_map(
                |(request_id, peers, tlvs)| Body::LookupResult(LookupResult {
                    request_id,
                    peers,
                    tlvs
                })
            ),
        (any::<u32>(), tlvs()).prop_map(|(request_id, tlvs)| Body::Ack(Ack { request_id, tlvs })),
        Just(Body::PeerInfoRequest),
        (
            any::<u32>(),
            any::<u64>(),
            any::<u64>(),
            any::<bool>(),
            any::<bool>(),
            addr()
        )
            .prop_map(
                |(rtt_us, bytes_in, bytes_out, relayed, holepunched, addr)| {
                    Body::PeerInfo(PeerInfo {
                        rtt_us,
                        bytes_in,
                        bytes_out,
                        relayed,
                        holepunched,
                        addr,
                    })
                }
            ),
        (
            any::<u8>(),
            any::<bool>(),
            any::<u8>(),
            any::<u16>(),
            tlvs()
        )
            .prop_map(|(state, firewalled, nat, udp_port, tlvs)| {
                Body::DhtStatus(DhtStatus {
                    state,
                    firewalled,
                    nat,
                    udp_port,
                    tlvs,
                })
            }),
        (any::<u32>(), any::<u16>(), any::<String>()).prop_map(|(request_id, code, message)| {
            Body::Error(ErrorFrame {
                request_id,
                code,
                message,
            })
        }),
        (
            any::<u8>().prop_filter("unknown type", |t| !KNOWN_TYPES.contains(t)),
            bytes(64)
        )
            .prop_map(|(frame_type, raw)| Body::Unknown { frame_type, raw }),
    ]
}

fn frame() -> impl Strategy<Value = Frame> {
    (any::<u8>(), any::<u16>(), any::<u64>(), body()).prop_map(|(flags, shard, conn_id, body)| {
        Frame {
            flags,
            shard,
            conn_id,
            body,
        }
    })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(cases(128)))]

    #[test]
    fn frames_round_trip(f in frame()) {
        let wire = f.to_bytes().unwrap();
        prop_assert_eq!(wire.len(), f.encoded_len());
        prop_assert_eq!(Frame::decode(wire.clone()).unwrap(), f.clone());

        // through the codec, also byte by byte; every strict prefix only asks for more
        let mut codec = FrameCodec::default();
        let mut buf = BytesMut::new();
        codec.encode(f.clone(), &mut buf).unwrap();
        prop_assert_eq!(&buf[..], &wire[..]);
        for cut in 0..wire.len() {
            let mut part = BytesMut::from(&wire[..cut]);
            prop_assert!(matches!(FrameCodec::default().decode(&mut part), Ok(None)), "cut {}", cut);
        }
        prop_assert_eq!(codec.decode(&mut buf).unwrap(), Some(f));
        prop_assert!(buf.is_empty());
    }

    #[test]
    fn garbage_frames_never_panic(
        ty in any::<u8>(),
        raw in vec(any::<u8>(), 0..512),
        flags in any::<u8>(),
        conn in any::<u64>(),
    ) {
        // a well-formed header with a garbage body reaches every body decoder
        let mut wire = Vec::with_capacity(HEADER_LEN + raw.len());
        wire.extend_from_slice(&((12 + raw.len()) as u32).to_le_bytes());
        wire.push(ty);
        wire.push(flags);
        wire.extend_from_slice(&0u16.to_le_bytes());
        wire.extend_from_slice(&conn.to_le_bytes());
        wire.extend_from_slice(&raw);
        let len = wire.len();
        let (_, peak) = peak_alloc(|| Frame::decode(Bytes::from(wire)).is_ok());
        // LookupResult pre-allocates at most 1024 peers (~53 KB) whatever the count says
        prop_assert!(peak <= 4 * len + 128 * 1024, "{} bytes", peak);
    }

    #[test]
    fn garbage_streams_never_panic(stream in vec(any::<u8>(), 0..4096), chunk in 1usize..97) {
        let mut codec = FrameCodec::new(64 * 1024);
        let mut buf = BytesMut::new();
        let ((), peak) = peak_alloc(|| {
            for part in stream.chunks(chunk) {
                buf.extend_from_slice(part);
                while let Ok(Some(_)) = codec.decode(&mut buf) {}
            }
        });
        // the codec may reserve up to max_frame for a declared frame
        prop_assert!(peak <= 4 * stream.len() + 2 * 64 * 1024 + 128 * 1024, "{} bytes", peak);
    }
}

// ------------------------------------------------------------------ limits

#[test]
fn max_frame_is_enforced_on_the_length_prefix() {
    let mut codec = FrameCodec::new(64 * 1024);
    // 4 bytes claiming a 1 GiB frame: refused at once, nothing buffered
    let mut buf = BytesMut::from(&(1u32 << 30).to_le_bytes()[..]);
    let (res, peak) = peak_alloc(|| codec.decode(&mut buf).map(|f| f.is_some()));
    assert!(matches!(res, Err(FrameError::TooLarge { .. })), "{res:?}");
    assert!(peak < 4096, "{peak}");
    // u32::MAX plus the prefix is over any `max_frame`
    let mut buf = BytesMut::from(&u32::MAX.to_le_bytes()[..]);
    assert!(matches!(
        FrameCodec::new(u32::MAX).decode(&mut buf),
        Err(FrameError::TooLarge { .. })
    ));
    // below the header size
    let mut buf = BytesMut::from(&11u32.to_le_bytes()[..]);
    assert!(matches!(
        codec.decode(&mut buf),
        Err(FrameError::TooShort(11))
    ));
    // the encoder refuses frames over max_frame
    let big = Frame::conn(1, Body::Data(Bytes::from(vec![0u8; 64 * 1024])));
    let mut out = BytesMut::new();
    assert!(matches!(
        codec.encode(big, &mut out),
        Err(FrameError::TooLarge { .. })
    ));
    // a TLV over 65 535 bytes cannot be encoded
    let f = Frame::control(Body::Ack(Ack {
        request_id: 1,
        tlvs: vec![Tlv::new(1, vec![0u8; 70_000])],
    }));
    assert!(matches!(f.to_bytes(), Err(FrameError::TlvTooLong)));
}

#[test]
fn counts_and_lengths_beyond_the_body_are_truncation_errors() {
    fn frame_with(ty: u8, body: &[u8]) -> Bytes {
        let mut w = Vec::new();
        w.extend_from_slice(&((12 + body.len()) as u32).to_le_bytes());
        w.extend_from_slice(&[ty, 0, 0, 0]);
        w.extend_from_slice(&0u64.to_le_bytes());
        w.extend_from_slice(body);
        Bytes::from(w)
    }
    // LookupResult claiming 65 535 peers with none present
    let mut body = 7u32.to_le_bytes().to_vec();
    body.extend_from_slice(&u16::MAX.to_le_bytes());
    let (r, peak) = peak_alloc(|| Frame::decode(frame_with(0x33, &body)).is_ok());
    assert!(!r);
    assert!(peak < 128 * 1024, "{peak}");
    // a TLV whose length runs past the body
    let mut body = 7u32.to_le_bytes().to_vec(); // Ack request id
    body.extend_from_slice(&[0x01, 0xff, 0xff, 1, 2, 3]);
    assert!(matches!(
        Frame::decode(frame_with(0x34, &body)),
        Err(FrameError::Truncated)
    ));
    // trailing bytes after fixed-size bodies
    assert!(matches!(
        Frame::decode(frame_with(0x02, &[0; 9])),
        Err(FrameError::TrailingBytes)
    ));
    // the length field must match the buffer exactly
    let mut b = frame_with(0x02, &[0; 8]).to_vec();
    b.push(0);
    assert!(matches!(
        Frame::decode(Bytes::from(b)),
        Err(FrameError::Truncated)
    ));
}

/// At most `MAX_TLVS_PER_FRAME` TLVs per body. A large frame of empty
/// TLVs (3 wire bytes, ~40 bytes decoded each) fails on the first one past the cap, before it
/// can allocate a vector proportional to the frame.
#[test]
fn tlv_count_is_capped() {
    use xkr_p2p_hyper::frame::MAX_TLVS_PER_FRAME;
    fn ack_with_empty_tlvs(n: usize) -> Bytes {
        let mut body = 7u32.to_le_bytes().to_vec(); // Ack request id
        for _ in 0..n {
            body.extend_from_slice(&[0x55, 0, 0]);
        }
        let mut w = Vec::new();
        w.extend_from_slice(&((12 + body.len()) as u32).to_le_bytes());
        w.extend_from_slice(&[0x34, 0, 0, 0]);
        w.extend_from_slice(&0u64.to_le_bytes());
        w.extend_from_slice(&body);
        Bytes::from(w)
    }
    match Frame::decode(ack_with_empty_tlvs(MAX_TLVS_PER_FRAME)) {
        Ok(f) => assert!(matches!(f.body, Body::Ack(a) if a.tlvs.len() == MAX_TLVS_PER_FRAME)),
        Err(e) => panic!("{MAX_TLVS_PER_FRAME} TLVs must decode: {e}"),
    }
    assert!(matches!(
        Frame::decode(ack_with_empty_tlvs(MAX_TLVS_PER_FRAME + 1)),
        Err(FrameError::TooManyTlvs)
    ));
    let big = ack_with_empty_tlvs(1_000_000);
    let (r, peak) = peak_alloc(|| Frame::decode(big).is_ok());
    assert!(!r);
    assert!(peak < 64 * 1024, "{peak} bytes");
}
