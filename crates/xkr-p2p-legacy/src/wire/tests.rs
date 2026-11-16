//! Codec tests: exact bytes against hand-derived C++ encodings, and round trips.

use bytes::BytesMut;
use tokio_util::codec::{Decoder, Encoder};
use xkr_levin::{BucketBuilder, LevinBody, LevinMessage, LevinMessageCodec, Protocol};
use xkr_types::{Hash, RawBlock};

use super::*;
use crate::network::MAINNET_NETWORK_ID;

fn h(b: u8) -> Hash {
    Hash([b; 32])
}

fn sample_node_data() -> BasicNodeData {
    BasicNodeData {
        network_id: MAINNET_NETWORK_ID,
        version: 5,
        peer_id: 0x0102_0304_0506_0708,
        local_time: 1_700_000_000,
        my_port: 11897,
    }
}

fn sample_sync() -> CoreSyncData {
    CoreSyncData {
        current_height: 2_617_000,
        top_id: h(0xab),
        pruning_seed: 0,
    }
}

fn push_name(out: &mut Vec<u8>, name: &str) {
    out.push(name.len() as u8);
    out.extend_from_slice(name.as_bytes());
}

/// `COMMAND_HANDSHAKE::request` exactly as the C++ `KVBinaryOutputStreamSerializer` writes it.
///
/// Derivation (`src/serialization/kv_binary_output_stream_serializer.cpp`):
/// - `dump()`: `KVBinaryStorageBlockHeader` (packed) = `u32 0x01011101`, `u32 0x01020101`,
///   `u8 1` -> `01 11 01 01 | 01 01 02 01 | 01`, then `writeArraySize(root.count)`.
/// - `writeArraySize(v)` for `v <= 63` is one byte `v << 2` (low 2 bits = size mark 0);
///   so 2 fields -> `08`, 5 fields -> `14`, a 16-byte string -> `40`, 32 bytes -> `80`.
/// - `writeElementPrefix(type, name)`: `u8 name_len`, name bytes, `u8 type`.
///   Types: UINT64 = 5, UINT32 = 6, UINT8 = 8, STRING = 10, OBJECT = 12.
/// - `endObject()` writes the prefix (OBJECT) for the nested object, then its field count,
///   then the buffered fields.
/// - `basic_node_data::serialize` order: `network_id` (`binary(&uuid, 16)` -> STRING),
///   version (UINT8), `peer_id` (UINT64), `local_time` (UINT64), `my_port` (UINT32).
/// - `CORE_SYNC_DATA::serialize` order: `current_height` (UINT32), `top_id`
///   (`serializePod` -> `binary(&hash, 32)` -> STRING).
/// - Integers are raw little-endian `writePod`.
fn expected_handshake_request_bytes() -> Vec<u8> {
    let mut e = vec![0x01, 0x11, 0x01, 0x01, 0x01, 0x01, 0x02, 0x01, 0x01];
    e.push(0x08); // root: 2 fields

    push_name(&mut e, "node_data");
    e.push(0x0c); // OBJECT
    e.push(0x14); // 5 fields
    push_name(&mut e, "network_id");
    e.extend_from_slice(&[0x0a, 0x40]); // STRING, len 16
    e.extend_from_slice(&MAINNET_NETWORK_ID);
    push_name(&mut e, "version");
    e.extend_from_slice(&[0x08, 0x05]); // UINT8 5
    push_name(&mut e, "peer_id");
    e.push(0x05); // UINT64
    e.extend_from_slice(&[0x08, 0x07, 0x06, 0x05, 0x04, 0x03, 0x02, 0x01]);
    push_name(&mut e, "local_time");
    e.push(0x05); // UINT64 1_700_000_000 = 0x6553F100
    e.extend_from_slice(&[0x00, 0xf1, 0x53, 0x65, 0x00, 0x00, 0x00, 0x00]);
    push_name(&mut e, "my_port");
    e.push(0x06); // UINT32 11897 = 0x2E79
    e.extend_from_slice(&[0x79, 0x2e, 0x00, 0x00]);

    push_name(&mut e, "payload_data");
    e.push(0x0c); // OBJECT
    e.push(0x08); // 2 fields
    push_name(&mut e, "current_height");
    e.push(0x06); // UINT32 2_617_000 = 0x0027EEA8
    e.extend_from_slice(&[0xa8, 0xee, 0x27, 0x00]);
    push_name(&mut e, "top_id");
    e.extend_from_slice(&[0x0a, 0x80]); // STRING, len 32
    e.extend_from_slice(&[0xab; 32]);
    e
}

#[test]
fn handshake_request_bytes_match_cpp() {
    let req = HandshakeRequest {
        node_data: sample_node_data(),
        payload_data: sample_sync(),
    };
    let bytes = xkr_epee::to_bytes(req.clone()).unwrap();
    assert_eq!(&bytes[..], expected_handshake_request_bytes().as_slice());

    let decoded: HandshakeRequest =
        xkr_epee::from_bytes(&mut expected_handshake_request_bytes().as_slice()).unwrap();
    assert_eq!(decoded, req);
}

/// The full Levin frame: `bucket_head2` (packed, 33 bytes) + body.
/// `sendMessage(1001, body, needResponse = true)`: signature, cb, `have_to_return_data = 1`,
/// command, `return_code = 0`, `flags = LEVIN_PACKET_REQUEST (1)`, `protocol_version = 1`.
#[test]
fn handshake_request_levin_header_matches_cpp() {
    let body = expected_handshake_request_bytes();
    let msg = LegacyMessage::Request(AdminRequest::Handshake(HandshakeRequest {
        node_data: sample_node_data(),
        payload_data: sample_sync(),
    }));

    let mut codec = LevinMessageCodec::<LegacyMessage>::default();
    let mut out = BytesMut::new();
    codec
        .encode(LevinMessage::Body(msg.clone()), &mut out)
        .unwrap();

    let mut expected = Vec::new();
    expected.extend_from_slice(&0x0101_0101_0101_2101_u64.to_le_bytes());
    expected.extend_from_slice(&(body.len() as u64).to_le_bytes());
    expected.push(1); // have_to_return_data
    expected.extend_from_slice(&1001_u32.to_le_bytes());
    expected.extend_from_slice(&0_i32.to_le_bytes());
    expected.extend_from_slice(&1_u32.to_le_bytes()); // LEVIN_PACKET_REQUEST
    expected.extend_from_slice(&1_u32.to_le_bytes()); // LEVIN_PROTOCOL_VER_1
    assert_eq!(expected.len(), 33);
    expected.extend_from_slice(&body);
    assert_eq!(&out[..], expected.as_slice());

    let decoded = codec.decode(&mut out).unwrap().unwrap();
    assert_eq!(decoded, msg);
}

/// The C++ `CORE_SYNC_DATA` (two fields), to show that a reader without the key skips our
/// `pruning_seed` (the C++ reader does the same: it loads the whole section into a `JsonValue`
/// tree and looks up the two keys by name).
#[derive(Debug, Clone, PartialEq, Eq)]
struct CppCoreSyncData {
    current_height: u32,
    top_id: Hash,
}

xkr_epee::epee_object!(
    CppCoreSyncData,
    current_height: u32,
    top_id: Hash as crate::blob::HashValue,
);

#[derive(Debug, Clone, PartialEq, Eq)]
struct CppHandshakeRequest {
    node_data: BasicNodeData,
    payload_data: CppCoreSyncData,
}

xkr_epee::epee_object!(
    CppHandshakeRequest,
    node_data: BasicNodeData,
    payload_data: CppCoreSyncData,
);

/// [`expected_handshake_request_bytes`] with a `pruning_seed` field appended to
/// `payload_data` (the last object of the root, so its fields end the body).
fn expected_handshake_request_bytes_with_seed(seed: u32) -> Vec<u8> {
    let mut e = expected_handshake_request_bytes();
    let mut marker = Vec::new();
    push_name(&mut marker, "payload_data");
    marker.extend_from_slice(&[0x0c, 0x08]); // OBJECT, 2 fields
    let at = e
        .windows(marker.len())
        .position(|w| w == marker.as_slice())
        .expect("payload_data header");
    e[at + marker.len() - 1] = 0x0c; // 3 fields
    push_name(&mut e, "pruning_seed");
    e.push(0x06); // UINT32
    e.extend_from_slice(&seed.to_le_bytes());
    e
}

/// Stripe pruning: a zero seed is not written (the C++ bytes above are unchanged), a non-zero
/// one is a third `payload_data` field, a missing one decodes as 0, and a reader that does not
/// know the key (C++) skips it.
#[test]
fn pruning_seed_is_an_optional_trailing_field() {
    // Absent: identical to the C++ encoding, decodes as a full node.
    let full = HandshakeRequest {
        node_data: sample_node_data(),
        payload_data: sample_sync(),
    };
    assert_eq!(
        &xkr_epee::to_bytes(full.clone()).unwrap()[..],
        expected_handshake_request_bytes().as_slice()
    );
    let decoded: HandshakeRequest =
        xkr_epee::from_bytes(&mut expected_handshake_request_bytes().as_slice()).unwrap();
    assert_eq!(decoded.payload_data.pruning_seed, 0);

    // Present: 4 stripes, stripe 2 (`stripes << 8 | stripe`).
    let seed = (4 << 8) | 2;
    let pruned = HandshakeRequest {
        node_data: sample_node_data(),
        payload_data: CoreSyncData {
            pruning_seed: seed,
            ..sample_sync()
        },
    };
    let expected = expected_handshake_request_bytes_with_seed(seed);
    assert_eq!(
        &xkr_epee::to_bytes(pruned.clone()).unwrap()[..],
        expected.as_slice()
    );
    let decoded: HandshakeRequest = xkr_epee::from_bytes(&mut expected.as_slice()).unwrap();
    assert_eq!(decoded, pruned);

    // A reader without the key (the C++ struct) skips it.
    let cpp: CppHandshakeRequest = xkr_epee::from_bytes(&mut expected.as_slice()).unwrap();
    assert_eq!(
        cpp.payload_data.current_height,
        sample_sync().current_height
    );
    assert_eq!(cpp.payload_data.top_id, sample_sync().top_id);

    // The "recent blocks only" seed of a completely pruned node (0x0100) travels the same way
    // and is skipped by the C++ reader too.
    let recent = HandshakeRequest {
        node_data: sample_node_data(),
        payload_data: CoreSyncData {
            pruning_seed: 0x0100,
            ..sample_sync()
        },
    };
    let bytes = expected_handshake_request_bytes_with_seed(0x0100);
    assert_eq!(
        &xkr_epee::to_bytes(recent.clone()).unwrap()[..],
        bytes.as_slice()
    );
    let decoded: HandshakeRequest = xkr_epee::from_bytes(&mut bytes.as_slice()).unwrap();
    assert_eq!(decoded, recent);
    let cpp: CppHandshakeRequest = xkr_epee::from_bytes(&mut bytes.as_slice()).unwrap();
    assert_eq!(cpp.payload_data.top_id, sample_sync().top_id);

    // Timed sync carries the same `payload_data`.
    let ts = TimedSyncResponse {
        local_time: 5,
        payload_data: pruned.payload_data.clone(),
        local_peerlist: vec![],
    };
    let bytes = xkr_epee::to_bytes(ts.clone()).unwrap();
    let back: TimedSyncResponse = xkr_epee::from_bytes(&mut &bytes[..]).unwrap();
    assert_eq!(back, ts);
}

#[test]
fn notification_and_response_headers() {
    let protocol = Protocol::default();

    let mut b = BucketBuilder::new(&protocol);
    LegacyMessage::Notification(ProtocolMessage::RequestChain(RequestChain {
        block_ids: vec![h(1), h(0)],
    }))
    .encode(&mut b)
    .unwrap();
    let bucket = b.finish();
    assert!(!bucket.header.have_to_return_data);
    assert_eq!(u32::from(bucket.header.command), 2006);
    assert_eq!(bucket.header.return_code, 0);
    assert_eq!(u32::from(bucket.header.flags), 1);

    let mut b = BucketBuilder::new(&protocol);
    LegacyMessage::Response(AdminResponse::Ping(PingResponse {
        status: "OK".into(),
        peer_id: 7,
    }))
    .encode(&mut b)
    .unwrap();
    let bucket = b.finish();
    assert!(!bucket.header.have_to_return_data);
    assert_eq!(bucket.header.return_code, 1);
    assert_eq!(u32::from(bucket.header.flags), 2);

    let mut b = BucketBuilder::new(&protocol);
    LegacyMessage::ErrorResponse {
        command: LegacyCommand::RequestPeerId,
        return_code: -6,
    }
    .encode(&mut b)
    .unwrap();
    let bucket = b.finish();
    assert_eq!(bucket.header.return_code, -6);
    assert!(bucket.body.is_empty());
}

/// `COMMAND_PING::request` has no fields: header + count 0.
#[test]
fn empty_object_bytes() {
    let bytes = xkr_epee::to_bytes(EmptyMessage).unwrap();
    assert_eq!(
        &bytes[..],
        &[0x01, 0x11, 0x01, 0x01, 0x01, 0x01, 0x02, 0x01, 0x01, 0x00]
    );
}

/// `serializeAsBinary(m_block_ids, "m_block_ids")` -> one STRING of 32*n bytes; the
/// struct field is renamed in Rust but the key must stay `m_block_ids`.
#[test]
fn chain_entry_uses_cpp_keys_and_blob() {
    let msg = ResponseChainEntry {
        start_height: 1,
        total_height: 3,
        block_ids: vec![h(1), h(2)],
    };
    let bytes = xkr_epee::to_bytes(msg.clone()).unwrap();

    let mut e = vec![0x01, 0x11, 0x01, 0x01, 0x01, 0x01, 0x02, 0x01, 0x01, 0x0c];
    push_name(&mut e, "start_height");
    e.extend_from_slice(&[0x06, 1, 0, 0, 0]);
    push_name(&mut e, "total_height");
    e.extend_from_slice(&[0x06, 3, 0, 0, 0]);
    push_name(&mut e, "m_block_ids");
    // 64 bytes > 63 -> two-byte size: (64 << 2) | 1 = 0x0101 LE
    e.extend_from_slice(&[0x0a, 0x01, 0x01]);
    e.extend_from_slice(&[1; 32]);
    e.extend_from_slice(&[2; 32]);
    assert_eq!(&bytes[..], e.as_slice());

    let back: ResponseChainEntry = xkr_epee::from_bytes(&mut e.as_slice()).unwrap();
    assert_eq!(back, msg);
}

/// `NOTIFY_NEW_BLOCK` uses key "b" for the block entry and an *array of strings* for txs.
#[test]
fn new_block_keys() {
    let msg = NewBlock {
        block: RawBlockLegacy {
            block: vec![9, 9],
            // tx blobs must take >= MIN_TX_BLOB_WIRE_SIZE (32) bytes on the wire
            txs: vec![vec![1; 40], vec![2; 41]],
        },
        current_blockchain_height: 5,
        hop: 0,
    };
    let bytes = xkr_epee::to_bytes(msg.clone()).unwrap();

    let mut e = vec![0x01, 0x11, 0x01, 0x01, 0x01, 0x01, 0x02, 0x01, 0x01, 0x0c];
    push_name(&mut e, "b");
    e.extend_from_slice(&[0x0c, 0x08]); // OBJECT, 2 fields
    push_name(&mut e, "block");
    e.extend_from_slice(&[0x0a, 0x08, 9, 9]); // STRING len 2
    push_name(&mut e, "txs");
    // array flag | STRING, count 2, then each string as size + bytes (no per-item type):
    // 40 << 2 = 0xa0, 41 << 2 = 0xa4
    e.extend_from_slice(&[0x8a, 0x08, 0xa0]);
    e.extend_from_slice(&[1; 40]);
    e.push(0xa4);
    e.extend_from_slice(&[2; 41]);
    push_name(&mut e, "current_blockchain_height");
    e.extend_from_slice(&[0x06, 5, 0, 0, 0]);
    push_name(&mut e, "hop");
    e.extend_from_slice(&[0x06, 0, 0, 0, 0]); // hop 0 is still written (no default)
    assert_eq!(&bytes[..], e.as_slice());

    let back: NewBlock = xkr_epee::from_bytes(&mut e.as_slice()).unwrap();
    assert_eq!(back, msg);
    let raw: RawBlock = back.block.into();
    assert_eq!(raw.transactions.len(), 2);
}

/// Peer list entries are 24-byte packed records inside one string.
#[test]
fn handshake_response_peerlist_blob() {
    let entry = PeerlistEntry {
        ip: u32::from_le_bytes([10, 0, 0, 1]),
        port: 11897,
        id: 42,
        last_seen: 1_700_000_000,
    };
    assert_eq!(entry.addr().to_string(), "10.0.0.1:11897");
    let msg = HandshakeResponse {
        node_data: sample_node_data(),
        payload_data: sample_sync(),
        local_peerlist: vec![entry, entry],
    };
    let bytes = xkr_epee::to_bytes(msg.clone()).unwrap();

    let mut tail = Vec::new();
    push_name(&mut tail, "local_peerlist");
    tail.extend_from_slice(&[0x0a, 0xc0]); // STRING, len 48 -> 48 << 2 = 0xc0
    for _ in 0..2 {
        tail.extend_from_slice(&[10, 0, 0, 1]);
        tail.extend_from_slice(&11897_u32.to_le_bytes());
        tail.extend_from_slice(&42_u64.to_le_bytes());
        tail.extend_from_slice(&1_700_000_000_u64.to_le_bytes());
    }
    assert!(bytes.ends_with(&tail));
    assert_eq!(bytes[9], 0x0c); // 3 root fields

    let back: HandshakeResponse = xkr_epee::from_bytes(&mut &bytes[..]).unwrap();
    assert_eq!(back, msg);

    // Empty peer list: the field is omitted (C++ `binary()` with size 0 writes nothing)
    // and decodes back to an empty list.
    let empty = HandshakeResponse {
        local_peerlist: vec![],
        ..msg
    };
    let bytes = xkr_epee::to_bytes(empty.clone()).unwrap();
    assert_eq!(bytes[9], 0x08); // 2 root fields
    let back: HandshakeResponse = xkr_epee::from_bytes(&mut &bytes[..]).unwrap();
    assert_eq!(back, empty);
}

#[test]
fn bad_blob_length_is_rejected() {
    let mut e = vec![0x01, 0x11, 0x01, 0x01, 0x01, 0x01, 0x02, 0x01, 0x01, 0x04];
    push_name(&mut e, "block_ids");
    e.extend_from_slice(&[0x0a, 0x0c, 1, 2, 3]); // 3 bytes, not a multiple of 32
    assert!(xkr_epee::from_bytes::<RequestChain, _>(&mut e.as_slice()).is_err());
}

#[test]
fn connection_entry_padding() {
    let msg = NetworkStateResponse {
        local_peerlist_white: vec![],
        local_peerlist_gray: vec![],
        connections_list: vec![ConnectionEntry {
            ip: 1,
            port: 2,
            id: 3,
            is_income: true,
        }],
        my_id: 4,
        local_time: 5,
    };
    let bytes = xkr_epee::to_bytes(msg.clone()).unwrap();
    let back: NetworkStateResponse = xkr_epee::from_bytes(&mut &bytes[..]).unwrap();
    assert_eq!(back, msg);
}

fn round_trip(msg: &LegacyMessage) {
    let mut codec = LevinMessageCodec::<LegacyMessage>::default();
    let mut buf = BytesMut::new();
    codec
        .encode(LevinMessage::Body(msg.clone()), &mut buf)
        .unwrap();
    let back = codec.decode(&mut buf).unwrap().unwrap();
    assert_eq!(back, *msg);
    assert!(buf.is_empty());
}

#[test]
fn every_message_round_trips() {
    let entry = PeerlistEntry {
        ip: 0x0100_007f,
        port: 11898,
        id: 1,
        last_seen: 2,
    };
    let raw = RawBlockLegacy {
        block: vec![1, 2, 3],
        txs: vec![vec![4; 40], vec![6; 33]],
    };

    round_trip(&LegacyMessage::Request(AdminRequest::Handshake(
        HandshakeRequest {
            node_data: sample_node_data(),
            payload_data: sample_sync(),
        },
    )));
    round_trip(&LegacyMessage::Response(AdminResponse::Handshake(
        HandshakeResponse {
            node_data: sample_node_data(),
            payload_data: sample_sync(),
            local_peerlist: vec![entry],
        },
    )));
    round_trip(&LegacyMessage::Request(AdminRequest::TimedSync(
        TimedSyncRequest {
            payload_data: sample_sync(),
        },
    )));
    round_trip(&LegacyMessage::Response(AdminResponse::TimedSync(
        TimedSyncResponse {
            local_time: 77,
            payload_data: sample_sync(),
            local_peerlist: vec![entry, entry],
        },
    )));
    round_trip(&LegacyMessage::Request(AdminRequest::Ping));
    round_trip(&LegacyMessage::Response(AdminResponse::Ping(
        PingResponse {
            status: "OK".into(),
            peer_id: 9,
        },
    )));
    round_trip(&LegacyMessage::Request(AdminRequest::StatInfo(
        StatInfoRequest {
            tr: ProofOfTrust {
                peer_id: 1,
                time: 2,
                sign: [7; 64],
            },
        },
    )));
    round_trip(&LegacyMessage::Response(AdminResponse::StatInfo(
        StatInfoResponse {
            version: "1.1.6".into(),
            os_version: "linux".into(),
            connections_count: 3,
            incoming_connections_count: 1,
            payload_info: CoreStatistics {
                tx_pool_size: 1,
                blockchain_height: 2,
                mining_speed: 3,
                alternative_blocks: 4,
                top_block_id_str: "ab".into(),
            },
        },
    )));
    round_trip(&LegacyMessage::Request(AdminRequest::PeerId));
    round_trip(&LegacyMessage::Response(AdminResponse::PeerId(
        PeerIdResponse { my_id: 5 },
    )));

    round_trip(&LegacyMessage::Notification(ProtocolMessage::NewBlock(
        NewBlock {
            block: raw.clone(),
            current_blockchain_height: 10,
            hop: 1,
        },
    )));
    round_trip(&LegacyMessage::Notification(
        ProtocolMessage::NewTransactions(NewTransactions {
            txs: vec![vec![1; 40], vec![3; 40]],
        }),
    ));
    round_trip(&LegacyMessage::Notification(
        ProtocolMessage::RequestGetObjects(RequestGetObjects {
            txs: vec![],
            blocks: vec![h(1), h(2), h(3)],
        }),
    ));
    round_trip(&LegacyMessage::Notification(
        ProtocolMessage::ResponseGetObjects(ResponseGetObjects {
            txs: vec![],
            blocks: vec![raw.clone(), raw],
            missed_ids: vec![h(4)],
            current_blockchain_height: 11,
        }),
    ));
    round_trip(&LegacyMessage::Notification(ProtocolMessage::RequestChain(
        RequestChain {
            block_ids: vec![h(5), h(0)],
        },
    )));
    round_trip(&LegacyMessage::Notification(
        ProtocolMessage::ResponseChainEntry(ResponseChainEntry {
            start_height: 0,
            total_height: 2,
            block_ids: vec![h(0), h(1)],
        }),
    ));
    round_trip(&LegacyMessage::Notification(
        ProtocolMessage::RequestTxPool(RequestTxPool { txs: vec![h(6)] }),
    ));
    round_trip(&LegacyMessage::Notification(ProtocolMessage::NewLiteBlock(
        NewLiteBlock {
            current_blockchain_height: 12,
            hop: 2,
            block_template: vec![1, 2, 3, 4],
        },
    )));
    round_trip(&LegacyMessage::Notification(ProtocolMessage::MissingTxs(
        MissingTxs {
            current_blockchain_height: 12,
            block_hash: h(7),
            missing_txs: vec![h(8), h(9)],
        },
    )));
}

#[test]
fn unknown_and_empty_messages() {
    let protocol = Protocol::default();
    let mut codec = LevinMessageCodec::<LegacyMessage>::default();

    // An empty-bodied response (C++ reply to a request it has no handler for).
    let mut buf = BytesMut::new();
    codec
        .encode(
            LevinMessage::Body(LegacyMessage::ErrorResponse {
                command: LegacyCommand::RequestStatInfo,
                return_code: -6,
            }),
            &mut buf,
        )
        .unwrap();
    assert_eq!(
        codec.decode(&mut buf).unwrap().unwrap(),
        LegacyMessage::EmptyResponse(LegacyCommand::RequestStatInfo)
    );

    // A request with a command we do not know.
    let mut b = BucketBuilder::<LegacyCommand>::new(&protocol);
    b.set_message_type(xkr_levin::MessageType::Request);
    b.set_command(LegacyCommand::Unknown(1007));
    b.set_return_code(0);
    b.set_body(bytes::Bytes::from_static(&[0; 4]));
    let mut buf = BytesMut::new();
    codec
        .encode(LevinMessage::Bucket(b.finish()), &mut buf)
        .unwrap();
    assert_eq!(
        codec.decode(&mut buf).unwrap().unwrap(),
        LegacyMessage::UnknownRequest(LegacyCommand::Unknown(1007))
    );
}

#[test]
fn command_ids() {
    for id in [
        1001, 1002, 1003, 1004, 1005, 1006, 2001, 2002, 2003, 2004, 2006, 2007, 2008, 2009, 2010,
    ] {
        let c = LegacyCommand::from(id);
        assert!(!matches!(c, LegacyCommand::Unknown(_)), "{id}");
        assert_eq!(u32::from(c), id);
    }
    assert_eq!(LegacyCommand::from(2005), LegacyCommand::Unknown(2005));
}

/// Allocation bounds: one-byte array elements that would cost a 24/48-byte
/// header in memory are refused.
#[test]
fn tiny_array_elements_are_rejected() {
    // NOTIFY_NEW_TRANSACTIONS with 3 empty tx strings
    let mut e = vec![0x01, 0x11, 0x01, 0x01, 0x01, 0x01, 0x02, 0x01, 0x01, 0x04];
    push_name(&mut e, "txs");
    e.extend_from_slice(&[0x8a, 0x0c, 0x00, 0x00, 0x00]);
    assert!(xkr_epee::from_bytes::<NewTransactions, _>(&mut e.as_slice()).is_err());

    // NOTIFY_RESPONSE_GET_OBJECTS with MAX_BLOCKS_IN_RESPONSE + 1 empty block objects
    let n = crate::constants::MAX_BLOCKS_IN_RESPONSE + 1;
    let mut e = vec![0x01, 0x11, 0x01, 0x01, 0x01, 0x01, 0x02, 0x01, 0x01, 0x08];
    push_name(&mut e, "blocks");
    e.push(0x8c); // array of OBJECT
    // epee varint, 2-byte form: (n << 2) | 1
    let v = u16::try_from((n << 2) | 1).unwrap();
    e.extend_from_slice(&v.to_le_bytes());
    e.extend(std::iter::repeat_n(0x00, n)); // every object has 0 fields
    push_name(&mut e, "current_blockchain_height");
    e.extend_from_slice(&[0x06, 1, 0, 0, 0]);
    assert!(xkr_epee::from_bytes::<ResponseGetObjects, _>(&mut e.as_slice()).is_err());
}
