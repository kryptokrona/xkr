//! Peer lists, the version record, bans and the broadcast throttle (`on_peer_list_request`,
//! `on_peer_list_response`, `Peer::normalize`, `TCPServer::ban`, `throttle_broadcast` in the
//! C++ p2pool), plus the session rules around them.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::{Duration, Instant};

use xkr_p2pool::p2p::handshake::PeerIds;
use xkr_p2pool::p2p::message::{Message, PeerEntry};
use xkr_p2pool::p2p::peers::{
    BAN_FAULT_THRESHOLD, BAN_TIME, BanKey, BanList, BroadcastThrottle, MAX_PEER_LIST_SIZE,
    PEER_EXPIRY, PROTOCOL_1_0, PROTOCOL_1_2, PeerBook, SUPPORTED_PROTOCOL, VersionRecord,
    filter_received, negotiate_protocol, peers_to_send, response_entries, select_peers,
};
use xkr_p2pool::p2p::session::{
    Direction, Domain, Event, MAX_PENDING_BLOCK_REQUESTS, PeerError, PeerSession, SessionConfig,
};
use xkr_p2pool::p2p::{P2pConfig, WireError};

fn v4(a: u8, b: u8, c: u8, d: u8, port: u16) -> PeerEntry {
    PeerEntry::from_socket_addr(SocketAddr::new(IpAddr::V4(Ipv4Addr::new(a, b, c, d)), port))
}

fn v6(s: &str, port: u16) -> PeerEntry {
    PeerEntry::from_socket_addr(SocketAddr::new(
        IpAddr::V6(s.parse::<Ipv6Addr>().unwrap()),
        port,
    ))
}

#[test]
fn entries_and_socket_addresses() {
    let e = v4(1, 2, 3, 4, 37889);
    assert!(!e.is_v6);
    assert_eq!(e.ip, [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0xff, 0xff, 1, 2, 3, 4]);
    assert_eq!(e.socket_addr(), "1.2.3.4:37889".parse().unwrap());

    // an IPv4-mapped IPv6 address is IPv4
    let mapped = v6("::ffff:5.6.7.8", 1);
    assert!(!mapped.is_v6);
    assert_eq!(mapped.socket_addr(), "5.6.7.8:1".parse().unwrap());

    let six = v6("2001:db8::7", 37890);
    assert!(six.is_v6);
    assert_eq!(six.socket_addr(), "[2001:db8::7]:37890".parse().unwrap());

    // Peer::normalize: IPv4 entries get the ::ffff: prefix whatever was sent
    let raw = PeerEntry {
        is_v6: false,
        ip: [9; 16],
        port: 1,
    };
    assert_eq!(
        raw.normalized().ip,
        [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0xff, 0xff, 9, 9, 9, 9]
    );
}

#[test]
fn version_record() {
    let ours = VersionRecord::ours();
    assert_eq!(ours.protocol, 0x0001_0004);
    assert_eq!(ours.software_id, u32::from_le_bytes(*b"XKRD"));
    assert_eq!(ours.software_id, xkr_p2pool::SOFTWARE_ID);
    assert_eq!(ours.software_version, xkr_p2pool::SOFTWARE_VERSION);
    let e = ours.to_entry();
    assert_eq!(VersionRecord::from_entry(&e), Some(ours));
    assert_eq!(e.port, 0xffff);
    // a real address is not a record
    assert_eq!(VersionRecord::from_entry(&v4(1, 2, 3, 4, 0xffff)), None);
}

#[test]
fn protocol_negotiation() {
    // same major: max(current, min(announced, 1.4)), never lower
    assert_eq!(negotiate_protocol(PROTOCOL_1_0, 0x0001_0002), PROTOCOL_1_2);
    assert_eq!(
        negotiate_protocol(PROTOCOL_1_0, 0x0001_0009),
        SUPPORTED_PROTOCOL
    );
    assert_eq!(negotiate_protocol(PROTOCOL_1_2, PROTOCOL_1_0), PROTOCOL_1_2);
    // another major: 1.0
    assert_eq!(negotiate_protocol(PROTOCOL_1_2, 0x0002_0000), PROTOCOL_1_0);
    assert_eq!(negotiate_protocol(SUPPORTED_PROTOCOL, 0), PROTOCOL_1_0);
}

#[test]
fn received_lists_are_filtered() {
    let record = VersionRecord {
        protocol: 0x0001_0004,
        software_version: 2,
        software_id: 0,
    };
    let list = [
        record.to_entry(),
        v4(127, 0, 0, 1, 37889), // localhost
        v4(1, 2, 3, 4, 0),       // port 0
        v4(0, 1, 2, 3, 37889),   // 0.0.0.0/8
        v4(127, 9, 9, 9, 37889), // 127.0.0.0/8
        v4(224, 0, 0, 1, 37889), // multicast
        v4(250, 1, 1, 1, 37889), // reserved
        v6("::1", 37889),        // localhost
        v4(10, 0, 0, 1, 37889),  // private addresses are kept
        v4(92, 34, 6, 2, 37889), // kept
        PeerEntry {
            is_v6: true,
            ..v4(93, 1, 1, 1, 37888)
        }, // mapped, sent as v6: kept as v4
        v6("2001:db8::1", 37890), // kept
    ];
    let r = filter_received(&list);
    assert_eq!(r.version, Some(record));
    assert_eq!(
        r.peers
            .iter()
            .map(PeerEntry::socket_addr)
            .collect::<Vec<_>>(),
        vec![
            "10.0.0.1:37889".parse::<SocketAddr>().unwrap(),
            "92.34.6.2:37889".parse().unwrap(),
            "93.1.1.1:37888".parse().unwrap(),
            "[2001:db8::1]:37890".parse().unwrap(),
        ]
    );
    assert!(!r.peers[2].is_v6);
}

#[test]
fn answer_size_and_selection() {
    assert_eq!(peers_to_send(0), 1);
    assert_eq!(peers_to_send(7), 1);
    assert_eq!(peers_to_send(8), 2);
    assert_eq!(peers_to_send(64), 16);
    assert_eq!(peers_to_send(1000), 16);

    let c: Vec<PeerEntry> = (1..=6).map(|i| v4(10, 0, 0, i, 37889)).collect();
    // fewer candidates than wanted: all, in order
    assert_eq!(select_peers(&c[..2], 4, || unreachable!()), c[..2].to_vec());
    // r = 0: every later candidate replaces slot 0
    assert_eq!(select_peers(&c, 2, || 0), vec![c[5], c[1]]);
    // r = max: k = n - 1 >= target, nothing is replaced
    assert_eq!(select_peers(&c, 2, || u64::MAX), vec![c[0], c[1]]);

    // the first answer puts the version record in slot 0
    let rec = VersionRecord::ours();
    assert_eq!(
        response_entries(Vec::new(), true, rec),
        vec![rec.to_entry()]
    );
    assert_eq!(
        response_entries(vec![c[0], c[1]], true, rec),
        vec![rec.to_entry(), c[1]]
    );
    assert_eq!(response_entries(vec![c[0]], false, rec), vec![c[0]]);
    assert!(response_entries(Vec::new(), false, rec).is_empty());
}

#[test]
fn peer_book() {
    let t0 = Instant::now();
    let mut book = PeerBook::new();
    let a = v4(1, 1, 1, 1, 37889);
    let b = v4(2, 2, 2, 2, 37889);
    assert_eq!(book.add(&[a, b, a], t0, |_| false), 2);
    assert_eq!(book.add(&[v4(127, 0, 0, 1, 1)], t0, |_| false), 0);
    assert_eq!(book.add(&[v4(3, 3, 3, 3, 1)], t0, |e| e.ip[12] == 3), 0);
    assert_eq!(book.len(), 2);

    // b is seen again later; a expires after an hour
    let later = t0 + PEER_EXPIRY;
    book.add(&[b], later, |_| false);
    book.expire(later + Duration::from_secs(1));
    assert_eq!(book.entries().copied().collect::<Vec<_>>(), vec![b]);

    book.remove(&b.ip);
    assert!(book.is_empty());

    let many: Vec<PeerEntry> = (0..MAX_PEER_LIST_SIZE as u32 + 10)
        .map(|i| {
            let [_, x, y, z] = i.to_be_bytes();
            v4(11, x, y, z, 1)
        })
        .collect();
    assert_eq!(book.add(&many, t0, |_| false), MAX_PEER_LIST_SIZE);
}

#[test]
fn ban_keys_and_expiry() {
    // localhost is never banned
    assert_eq!(BanKey::ip(IpAddr::V4(Ipv4Addr::LOCALHOST)), None);
    assert_eq!(BanKey::ip(IpAddr::V6(Ipv6Addr::LOCALHOST)), None);
    // IPv6 by /64
    let a = BanKey::ip("2001:db8:1:2:aaaa::1".parse().unwrap()).unwrap();
    let b = BanKey::ip("2001:db8:1:2:bbbb::9".parse().unwrap()).unwrap();
    let c = BanKey::ip("2001:db8:1:3::1".parse().unwrap()).unwrap();
    assert_eq!(a, b);
    assert_ne!(a, c);
    // IPv4 whole, the mapped form is the same key
    let d = BanKey::ip("1.2.3.4".parse().unwrap()).unwrap();
    assert_eq!(Some(d), BanKey::ip("::ffff:1.2.3.4".parse().unwrap()));
    assert_ne!(Some(d), BanKey::ip("1.2.3.5".parse().unwrap()));

    // DEFAULT_BAN_TIME is 60 s
    assert_eq!(BAN_TIME, Duration::from_secs(60));
    let t0 = Instant::now();
    let mut bans = BanList::new();
    bans.ban(a, t0);
    bans.ban(BanKey::Key([7; 32]), t0);
    assert!(bans.is_banned(&b, t0 + Duration::from_secs(59)));
    assert!(!bans.is_banned(&c, t0));
    assert!(bans.is_banned(&BanKey::Key([7; 32]), t0));
    assert!(!bans.is_banned(&a, t0 + BAN_TIME));
    assert_eq!(bans.len(), 1);
    bans.prune(t0 + BAN_TIME);
    assert!(bans.is_empty());
}

#[test]
fn broadcast_throttle() {
    // default sidechain: block time 10 s, time unit 1 s; 10 per unit, 60 per 60, 180 per 600
    let key = BanKey::ip("1.2.3.4".parse().unwrap()).unwrap();
    let other = BanKey::ip("5.6.7.8".parse().unwrap()).unwrap();
    let t0 = 1_700_000_000_000_000u64;
    let mut th = BroadcastThrottle::new(10);
    for i in 0..10 {
        assert!(!th.over_limit(key, t0 + i * 1000), "broadcast {i}");
    }
    assert!(th.over_limit(key, t0 + 10_000));
    assert!(!th.over_limit(other, t0 + 10_000));

    // five per second for a minute: the 61st within 60 s is over
    let mut th = BroadcastThrottle::new(10);
    for i in 0..60 {
        assert!(!th.over_limit(key, t0 + i * 200_000), "broadcast {i}");
    }
    assert!(th.over_limit(key, t0 + 60 * 200_000));

    // a new window after one unit
    let mut th = BroadcastThrottle::new(10);
    for i in 0..10 {
        assert!(!th.over_limit(key, t0 + i));
    }
    assert!(!th.over_limit(key, t0 + 1_000_000));

    // after 600 units every window starts over
    th.clean(t0 + 601_000_000);
    for i in 0..10 {
        assert!(!th.over_limit(key, t0 + 601_000_000 + i));
    }
}

#[test]
fn listening_is_off_by_default() {
    let c = P2pConfig::default();
    assert!(!c.accepts_incoming());
    assert_eq!(c.external_port, 37889);
    assert_eq!(
        (c.out_peers, c.in_peers, c.in_peers_localhost),
        (10, 450, 10)
    );
    let c = P2pConfig {
        listen: Some("0.0.0.0:37889".parse().unwrap()),
        ..P2pConfig::default()
    };
    assert!(c.accepts_incoming());
}

// --- session rules -------------------------------------------------------------------------

const CID: [u8; 32] = [0x22; 32];

fn config(direction: Direction, ids: [u64; 4]) -> SessionConfig {
    SessionConfig {
        consensus_id: CID,
        peer_ids: PeerIds(ids),
        domain: Domain::Tcp,
        direction,
        listen_port: 37889,
        version: VersionRecord::ours(),
    }
}

fn fresh(direction: Direction, now: Instant) -> PeerSession {
    let (ids, challenge) = match direction {
        Direction::Outgoing => ([1, 2, 3, 4], 1),
        Direction::Incoming => ([5, 6, 7, 8], 2),
    };
    PeerSession::with_values(config(direction, ids), challenge, 0, now)
}

/// A pair after the handshake, with outboxes drained and events dropped.
fn connected(now: Instant) -> (PeerSession, PeerSession) {
    let mut out = fresh(Direction::Outgoing, now);
    let mut inc = fresh(Direction::Incoming, now);
    for _ in 0..3 {
        let a = out.take_outbox();
        let b = inc.take_outbox();
        inc.feed(&a, now).unwrap();
        out.feed(&b, now).unwrap();
    }
    assert!(out.is_good() && inc.is_good());
    (out, inc)
}

#[test]
fn peer_list_exchange_round_trip() {
    let t0 = Instant::now();
    let (mut out, mut inc) = connected(t0);
    assert_eq!(out.protocol(), PROTOCOL_1_0);

    // the tip request is answered empty; that triggers a peer list request
    inc.respond_block(&[], &[]);
    let ev = out.feed(&inc.take_outbox(), t0).unwrap();
    assert_eq!(
        ev,
        vec![Event::BlockResponse {
            requested: None,
            blob: Vec::new()
        }]
    );
    let req = out.take_outbox();
    assert_eq!(req, Message::PeerListRequest.to_bytes());
    assert!(!out.peer_list_due(t0 + Duration::from_secs(59)));
    assert!(out.peer_list_due(t0 + Duration::from_secs(120)));

    let ev = inc.feed(&req, t0).unwrap();
    assert_eq!(ev, vec![Event::PeerListRequest]);
    let candidates = [v4(1, 2, 3, 4, 37889), v4(5, 6, 7, 8, 37889)];
    inc.respond_peer_list(&candidates, 8, t0);
    let answer = inc.take_outbox();
    // two slots for eight connections; slot 0 is the version record
    assert_eq!(
        answer,
        Message::PeerListResponse(vec![VersionRecord::ours().to_entry(), candidates[1]]).to_bytes()
    );
    let ev = out.feed(&answer, t0).unwrap();
    let Event::PeerList(r) = &ev[0] else {
        panic!("{ev:?}")
    };
    assert_eq!(r.version, Some(VersionRecord::ours()));
    assert_eq!(r.peers, vec![candidates[1]]);
    assert_eq!(out.protocol(), SUPPORTED_PROTOCOL);
    assert_eq!(out.software(), Some(VersionRecord::ours()));

    // a second request within 60 s gets an empty list
    inc.respond_peer_list(&candidates, 8, t0 + Duration::from_secs(59));
    assert_eq!(inc.take_outbox(), vec![7, 0]);
    // after 60 s peers again, without the record
    inc.respond_peer_list(&candidates[..1], 8, t0 + Duration::from_secs(60));
    assert_eq!(
        inc.take_outbox(),
        Message::PeerListResponse(vec![candidates[0]]).to_bytes()
    );
}

#[test]
fn unrequested_answers_ban() {
    let t0 = Instant::now();
    let (mut out, _inc) = connected(t0);
    assert_eq!(
        out.feed(&Message::PeerListResponse(Vec::new()).to_bytes(), t0),
        Err(PeerError::UnexpectedPeerList)
    );
    let (mut out, _inc) = connected(t0);
    // one tip request is open: one answer is fine, the second is not
    out.feed(&Message::BlockResponse(Vec::new()).to_bytes(), t0)
        .unwrap();
    assert_eq!(
        out.feed(&Message::BlockResponse(vec![1]).to_bytes(), t0),
        Err(PeerError::UnexpectedBlockResponse)
    );
}

#[test]
fn answers_follow_request_order() {
    let t0 = Instant::now();
    let (mut out, _inc) = connected(t0);
    assert!(out.request_block([1; 32]));
    assert!(out.request_block([2; 32]));
    let mut bytes = Message::BlockResponse(vec![0xa]).to_bytes();
    bytes.extend(Message::BlockResponse(vec![0xb]).to_bytes());
    bytes.extend(Message::BlockResponse(Vec::new()).to_bytes());
    let ev = out.feed(&bytes, t0).unwrap();
    let got: Vec<_> = ev
        .into_iter()
        .filter_map(|e| match e {
            Event::BlockResponse { requested, blob } => Some((requested, blob)),
            _ => None,
        })
        .collect();
    assert_eq!(
        got,
        vec![
            (None, vec![0xa]),
            (Some([1; 32]), vec![0xb]),
            (Some([2; 32]), vec![])
        ]
    );
}

#[test]
fn pending_request_limit() {
    let t0 = Instant::now();
    let (mut out, _inc) = connected(t0);
    // the tip request is one of the 25
    for i in 1..MAX_PENDING_BLOCK_REQUESTS {
        assert!(out.request_block([i as u8; 32]), "request {i}");
    }
    assert!(!out.request_block([0xee; 32]));
    assert_eq!(out.pending_block_requests(), MAX_PENDING_BLOCK_REQUESTS);
}

/// A session whose handshake is done but whose peer has not sent `LISTEN_PORT` yet.
fn without_listen_port(now: Instant) -> PeerSession {
    let mut out = fresh(Direction::Outgoing, now);
    let mut inc = fresh(Direction::Incoming, now);
    inc.feed(&out.take_outbox(), now).unwrap();
    // the peer's challenge and solution; it has not completed its side yet
    out.feed(&inc.take_outbox(), now).unwrap();
    assert!(out.handshake_complete() && !out.is_good());
    out
}

#[test]
fn listen_port_rules() {
    let t0 = Instant::now();
    for bad in [0, -1, 65536, i32::MIN] {
        let mut out = without_listen_port(t0);
        assert_eq!(
            out.feed(&Message::ListenPort(bad).to_bytes(), t0),
            Err(PeerError::BadListenPort(bad))
        );
    }
    let mut out = without_listen_port(t0);
    let ev = out
        .feed(&Message::ListenPort(65535).to_bytes(), t0)
        .unwrap();
    assert_eq!(ev, vec![Event::ListenPort(65535)]);
    assert!(out.is_good());
    let (mut out, _inc) = connected(t0);
    assert_eq!(
        out.feed(&Message::ListenPort(37889).to_bytes(), t0),
        Err(PeerError::RepeatedListenPort)
    );
}

#[test]
fn messages_before_the_handshake_ban() {
    let t0 = Instant::now();
    let new_session = || fresh(Direction::Outgoing, t0);
    for m in [
        Message::ListenPort(37889),
        Message::BlockRequest([0; 32]),
        Message::BlockBroadcast(vec![1]),
        Message::PeerListRequest,
        Message::BlockNotify([1; 32]),
        Message::HandshakeSolution {
            solution: [0; 32],
            salt: [0; 8],
        },
    ] {
        let id = m.id();
        assert_eq!(
            new_session().feed(&m.to_bytes(), t0),
            Err(PeerError::OutOfOrder(id))
        );
    }
    // and a second challenge after the handshake
    let (mut out, _inc) = connected(t0);
    let again = Message::HandshakeChallenge {
        challenge: [0; 8],
        peer_id: 9,
    };
    assert_eq!(
        out.feed(&again.to_bytes(), t0),
        Err(PeerError::OutOfOrder(again.id()))
    );
}

#[test]
fn per_read_request_limits() {
    let t0 = Instant::now();
    let (mut out, _inc) = connected(t0);
    let req = Message::BlockRequest([3; 32]).to_bytes();
    assert_eq!(out.feed(&req.repeat(100), t0).unwrap().len(), 100);
    assert_eq!(
        out.feed(&req.repeat(101), t0),
        Err(PeerError::TooManyBlockRequests)
    );

    let (mut out, _inc) = connected(t0);
    assert_eq!(out.feed(&[6; 10], t0).unwrap().len(), 10);
    assert_eq!(
        out.feed(&[6; 11], t0),
        Err(PeerError::TooManyPeerListRequests)
    );
}

#[test]
fn bad_frames_ban() {
    let t0 = Instant::now();
    let (mut out, _inc) = connected(t0);
    assert_eq!(
        out.feed(&[0x0c], t0),
        Err(PeerError::Wire(WireError::UnknownId(0x0c)))
    );
    let (mut out, _inc) = connected(t0);
    assert!(matches!(
        out.feed(&[5, 0xfc, 0xff, 0x01, 0x00], t0),
        Err(PeerError::Wire(WireError::TooBig { len: 131_068, .. }))
    ));
    let (mut out, _inc) = connected(t0);
    assert_eq!(
        out.feed(&[5, 0, 0, 0, 0], t0),
        Err(PeerError::EmptyBroadcast)
    );
    let (mut out, _inc) = connected(t0);
    assert_eq!(
        out.feed(&[8, 0, 0, 0, 0], t0),
        Err(PeerError::EmptyBroadcast)
    );
}

#[test]
fn monero_only_messages_are_dropped() {
    let t0 = Instant::now();
    let (mut out, _inc) = connected(t0);
    let mut bytes = Message::AuxJobDonation(vec![1, 2, 3]).to_bytes();
    bytes.extend(Message::MoneroBlockBroadcast(vec![4; 100]).to_bytes());
    bytes.extend(Message::BlockNotify([9; 32]).to_bytes());
    let ev = out.feed(&bytes, t0).unwrap();
    assert_eq!(
        ev,
        vec![
            Event::Dropped(xkr_p2pool::p2p::MessageId::AuxJobDonation),
            Event::Dropped(xkr_p2pool::p2p::MessageId::MoneroBlockBroadcast),
            Event::BlockNotify([9; 32]),
        ]
    );
    // nothing is sent back
    assert_eq!(out.outbox_len(), 0);
}

#[test]
fn rejected_blocks_ban_on_the_fifth() {
    let t0 = Instant::now();
    let (mut out, _inc) = connected(t0);
    assert_eq!(BAN_FAULT_THRESHOLD, 5);
    for i in 1..BAN_FAULT_THRESHOLD {
        assert_eq!(out.reject_block(), Ok(()), "rejected block {i}");
    }
    assert_eq!(out.reject_block(), Err(PeerError::RejectedBlocks));
}

#[test]
fn broadcasts_and_notifies() {
    let t0 = Instant::now();
    let (mut out, mut inc) = connected(t0);
    let id = [0x42; 32];
    let blob = vec![1, 2, 3, 4];

    // protocol 1.0: always the full share
    out.note_broadcasted(id);
    assert!(out.broadcast_block(id, &blob));
    assert_eq!(
        out.take_outbox(),
        Message::BlockBroadcast(blob.clone()).to_bytes()
    );

    // after the version record (1.4): a share the peer sent us is only announced
    inc.respond_block(&[], &[]);
    out.feed(&inc.take_outbox(), t0).unwrap();
    inc.feed(&out.take_outbox(), t0).unwrap();
    inc.respond_peer_list(&[], 1, t0);
    out.feed(&inc.take_outbox(), t0).unwrap();
    assert_eq!(out.protocol(), SUPPORTED_PROTOCOL);
    assert!(out.broadcast_block(id, &blob));
    assert_eq!(out.take_outbox(), Message::BlockNotify(id).to_bytes());
    assert!(out.broadcast_block([0x43; 32], &blob));
    assert_eq!(
        out.take_outbox(),
        Message::BlockBroadcast(blob.clone()).to_bytes()
    );

    // only the last eight ids are remembered
    for i in 0..8u8 {
        out.note_broadcasted([i; 32]);
    }
    assert!(out.broadcast_block(id, &blob));
    assert_eq!(
        out.take_outbox(),
        Message::BlockBroadcast(blob.clone()).to_bytes()
    );

    // block answers carry notifies first, for peers speaking 1.2 or later
    out.respond_block(&blob, &[[1; 32], [2; 32]]);
    let mut want = Message::BlockNotify([1; 32]).to_bytes();
    want.extend(Message::BlockNotify([2; 32]).to_bytes());
    want.extend(Message::BlockResponse(blob.clone()).to_bytes());
    assert_eq!(out.take_outbox(), want);
    // an empty answer carries none
    out.respond_block(&[], &[[1; 32]]);
    assert_eq!(
        out.take_outbox(),
        Message::BlockResponse(Vec::new()).to_bytes()
    );
}

#[test]
fn not_good_peers_get_no_broadcasts() {
    let t0 = Instant::now();
    let mut s = fresh(Direction::Outgoing, t0);
    s.take_outbox();
    assert!(!s.broadcast_block([1; 32], &[1]));
    assert!(!s.request_block([1; 32]));
    assert_eq!(s.outbox_len(), 0);
}

#[test]
fn idle_limits() {
    let t0 = Instant::now();
    let s = fresh(Direction::Outgoing, t0);
    assert!(!s.idle(t0 + Duration::from_secs(9)));
    assert!(s.idle(t0 + Duration::from_secs(10)));
    let (out, _inc) = connected(t0);
    assert!(!out.idle(t0 + Duration::from_secs(299)));
    assert!(out.idle(t0 + Duration::from_secs(300)));
}
