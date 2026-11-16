//! Byte vectors of every sidechain P2P message, written by hand from the C++ writers and
//! readers in `src/p2p_server.cpp` of the C++ p2pool (line numbers of that file), and the
//! frames the decoder refuses.

use xkr_p2pool::p2p::message::{
    MAX_MESSAGE_SIZE, MAX_PAYLOAD, Message, MessageId, PeerEntry, WireError, decode,
};
use xkr_p2pool::p2p::peers::VersionRecord;

fn h(s: &str) -> Vec<u8> {
    hex::decode(s.replace(' ', "")).unwrap()
}

/// Encodes `m`, compares with `expected`, decodes it back whole and refuses every prefix.
fn golden(m: &Message, expected: &str) {
    let bytes = h(expected);
    assert_eq!(
        hex::encode(m.to_bytes()),
        hex::encode(&bytes),
        "{:?}",
        m.id()
    );
    let (back, used) = decode(&bytes).unwrap().unwrap();
    assert_eq!(&back, m);
    assert_eq!(used, bytes.len());
    for cut in 0..bytes.len() {
        assert_eq!(
            decode(&bytes[..cut]).unwrap(),
            None,
            "{:?} cut at {cut}",
            m.id()
        );
    }
    // a following message is left alone
    let mut two = bytes.clone();
    two.push(MessageId::PeerListRequest as u8);
    assert_eq!(decode(&two).unwrap().unwrap().1, bytes.len());
}

fn id32(first: u8) -> [u8; 32] {
    std::array::from_fn(|i| first.wrapping_add(i as u8))
}

#[test]
fn ids_are_the_cpp_enum() {
    // src/p2p_server.h:53-69, enum class MessageId in declaration order
    let names = [
        "HANDSHAKE_CHALLENGE",
        "HANDSHAKE_SOLUTION",
        "LISTEN_PORT",
        "BLOCK_REQUEST",
        "BLOCK_RESPONSE",
        "BLOCK_BROADCAST",
        "PEER_LIST_REQUEST",
        "PEER_LIST_RESPONSE",
        "BLOCK_BROADCAST_COMPACT",
        "BLOCK_NOTIFY",
        "AUX_JOB_DONATION",
        "MONERO_BLOCK_BROADCAST",
    ];
    for (i, name) in names.iter().enumerate() {
        let id = MessageId::from_byte(i as u8).unwrap();
        assert_eq!(id as u8, i as u8);
        assert_eq!(id.name(), *name);
    }
    assert_eq!(MessageId::from_byte(12), None);
    assert_eq!(MAX_PAYLOAD, 131_067);
    // P2P_BUF_SIZE = MAX_BLOCK_SIZE + 5 = 128 KiB
    assert_eq!(MAX_MESSAGE_SIZE, 128 * 1024);
}

#[test]
fn handshake_challenge() {
    // :2701-2736: id 0, the u64 challenge low byte first, then memcpy of the u64 peer id
    golden(
        &Message::HandshakeChallenge {
            challenge: 0x0807_0605_0403_0201u64.to_le_bytes(),
            peer_id: 0x1122_3344_5566_7788,
        },
        "00 0102030405060708 8877665544332211",
    );
}

#[test]
fn handshake_solution() {
    // :2849-2855: id 1, 32-byte H, 8-byte salt (salt u64 low byte first, :2779-2784)
    golden(
        &Message::HandshakeSolution {
            solution: [0xaa; 32],
            salt: 7u64.to_le_bytes(),
        },
        &format!("01 {} 0700000000000000", "aa".repeat(32)),
    );
}

#[test]
fn listen_port() {
    // :3017-3021: id 2, int32 port by memcpy (little-endian)
    golden(&Message::ListenPort(37889), "02 01940000");
    // the reader takes any int32 (:2407-2425); the range check is the session's
    golden(&Message::ListenPort(-1), "02 ffffffff");
    golden(&Message::ListenPort(65536), "02 00000100");
}

#[test]
fn block_request() {
    // :3024-3028: id 3 and an all-zero id for the chain tip; :3587-3590 by id
    golden(
        &Message::BlockRequest([0; 32]),
        &format!("03 {}", "00".repeat(32)),
    );
    golden(
        &Message::BlockRequest(id32(0x10)),
        "03 101112131415161718191a1b1c1d1e1f202122232425262728292a2b2c2d2e2f",
    );
}

#[test]
fn block_response() {
    // :3118-3127: id 4, u32 length, blob; length 0 when the block is unknown
    golden(
        &Message::BlockResponse(vec![0xde, 0xad, 0xbe, 0xef]),
        "04 04000000 deadbeef",
    );
    golden(&Message::BlockResponse(Vec::new()), "04 00000000");
}

#[test]
fn block_broadcast() {
    // :1395-1403: id 5 (full blob), u32 length, blob
    golden(
        &Message::BlockBroadcast(vec![1, 2, 3]),
        "05 03000000 010203",
    );
}

#[test]
fn peer_list_request() {
    // :601: the id alone
    golden(&Message::PeerListRequest, "06");
}

#[test]
fn peer_list_response() {
    // :3427-3440: id 7, u8 count, per peer u8 is_v6, 16 bytes raw_ip, u16 port (memcpy of 2
    // bytes of an int, little-endian). Slot 0 is the version record of a C++ 0.0.2 node
    // (:3394-3409): protocol 1.4, P2POOL_VERSION 0x000002, SoftwareID::P2Pool 0, ff ff ff ff,
    // port 0xffff. Slot 1 is 1.2.3.4:37889, slot 2 is [2001:db8::1]:37890.
    let cpp_record = VersionRecord {
        protocol: 0x0001_0004,
        software_version: 2,
        software_id: 0,
    };
    let v4 = PeerEntry {
        is_v6: false,
        ip: [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0xff, 0xff, 1, 2, 3, 4],
        port: 37889,
    };
    let mut ip6 = [0u8; 16];
    ip6[..4].copy_from_slice(&[0x20, 0x01, 0x0d, 0xb8]);
    ip6[15] = 1;
    let v6 = PeerEntry {
        is_v6: true,
        ip: ip6,
        port: 37890,
    };
    golden(
        &Message::PeerListResponse(vec![cpp_record.to_entry(), v4, v6]),
        "07 03 \
         00 04000100 02000000 00000000 ffffffff ffff \
         00 00000000000000000000ffff01020304 0194 \
         01 20010db8000000000000000000000001 0294",
    );
    // the empty answer to a request within 60 s
    golden(&Message::PeerListResponse(Vec::new()), "07 00");
    // xkrd's own record carries "XKRD" as the software id
    let ours = VersionRecord::ours().to_entry();
    assert_eq!(&ours.ip[8..12], b"XKRD");
    assert_eq!(&ours.ip[0..4], &[4, 0, 1, 0]);
}

#[test]
fn block_broadcast_compact() {
    // id 8 with the same length prefix (:1395, `what >= COMPACT`)
    golden(
        &Message::BlockBroadcastCompact(vec![0xff]),
        "08 01000000 ff",
    );
}

#[test]
fn block_notify() {
    // :1319-1322 and :3111-3114: id 9, 32-byte id
    golden(
        &Message::BlockNotify(id32(0xe0)),
        "09 e0e1e2e3e4e5e6e7e8e9eaebecedeeeff0f1f2f3f4f5f6f7f8f9fafbfcfdfeff",
    );
}

#[test]
fn aux_job_donation_is_read() {
    // :1957-1963: id 10, u32 length, signed blob; xkrd reads it whole and drops it
    golden(
        &Message::AuxJobDonation(vec![0xab, 0xcd]),
        "0a 02000000 abcd",
    );
}

#[test]
fn monero_block_broadcast_is_read() {
    // :2139-2145: id 11, u32 length, data
    golden(
        &Message::MoneroBlockBroadcast(vec![9; 5]),
        "0b 05000000 0909090909",
    );
    golden(&Message::MoneroBlockBroadcast(Vec::new()), "0b 00000000");
}

#[test]
fn unknown_id_is_refused() {
    // :2346-2351: an id above MONERO_BLOCK_BROADCAST bans
    for b in [12u8, 0x7f, 0xff] {
        assert_eq!(decode(&[b]), Err(WireError::UnknownId(b)));
        assert_eq!(decode(&[b, 0, 0, 0]), Err(WireError::UnknownId(b)));
    }
}

#[test]
fn oversized_lengths_are_refused_as_soon_as_known() {
    // :2461-2466, :2495-2500, :2582-2587, :2607-2612: a length above MAX_BLOCK_SIZE bans
    // before the payload arrives
    for id in [4u8, 5, 8, 10, 11] {
        let max = MAX_PAYLOAD as u32;
        let mut ok = vec![id];
        ok.extend_from_slice(&max.to_le_bytes());
        assert_eq!(
            decode(&ok).unwrap(),
            None,
            "id {id} at the limit waits for the payload"
        );

        for len in [max + 1, u32::MAX] {
            let mut bad = vec![id];
            bad.extend_from_slice(&len.to_le_bytes());
            let id = MessageId::from_byte(id).unwrap();
            assert_eq!(decode(&bad), Err(WireError::TooBig { id, len }));
        }
    }
    // a payload of exactly MAX_BLOCK_SIZE is read
    let big = Message::BlockBroadcast(vec![0x55; MAX_PAYLOAD]);
    let bytes = big.to_bytes();
    assert_eq!(bytes.len(), MAX_MESSAGE_SIZE);
    assert_eq!(decode(&bytes).unwrap().unwrap(), (big, MAX_MESSAGE_SIZE));
}

#[test]
fn long_peer_lists_are_refused() {
    // :2544-2550: more than 16 peers bans
    assert_eq!(decode(&[7, 17]), Err(WireError::TooManyPeers(17)));
    assert_eq!(decode(&[7, 0xff]), Err(WireError::TooManyPeers(0xff)));
    let mut sixteen = vec![7, 16];
    sixteen.extend_from_slice(&[0u8; 16 * 19]);
    assert!(matches!(
        decode(&sixteen).unwrap().unwrap().0,
        Message::PeerListResponse(p) if p.len() == 16
    ));
}

#[test]
fn any_nonzero_is_v6_byte_reads_as_ipv6() {
    // :3455: `is_v6 = *(buf++) != 0`
    let mut bytes = vec![7, 1, 2];
    bytes.extend_from_slice(&[0u8; 16]);
    bytes.extend_from_slice(&[0x01, 0x94]);
    let (m, _) = decode(&bytes).unwrap().unwrap();
    let want = PeerEntry {
        is_v6: true,
        ip: [0; 16],
        port: 37889,
    };
    assert_eq!(m, Message::PeerListResponse(vec![want]));
}

#[test]
fn garbage_streams_never_panic() {
    // a deterministic byte soup: every prefix either waits, reads a message or is refused
    let mut x = 0x2545_f491_4f6c_dd1du64;
    let mut soup = Vec::new();
    for _ in 0..4096 {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        soup.push((x >> 24) as u8);
    }
    for start in 0..soup.len() {
        let mut rest = &soup[start..];
        while let Ok(Some((_, used))) = decode(rest) {
            assert!(used > 0 && used <= rest.len());
            rest = &rest[used..];
        }
    }
}
