//! The handshake hash, its proof of work, peer ids, and handshakes between two sessions
//! (`send_handshake_solution`, `check_handshake_solution`, `on_handshake_challenge`,
//! `on_handshake_solution` in `src/p2p_server.cpp` of the C++ p2pool).

use std::time::Instant;

use xkr_p2pool::p2p::handshake::{
    CHALLENGE_DIFFICULTY, HandshakeError, PeerIds, check_peer_id, check_solution, has_enough_work,
    solution_hash, solve,
};
use xkr_p2pool::p2p::message::Message;
use xkr_p2pool::p2p::peers::VersionRecord;
use xkr_p2pool::p2p::session::{Direction, Domain, Event, PeerError, PeerSession, SessionConfig};

/// The default sidechain's consensus id (`src/side_chain.cpp:62-64`).
const DEFAULT_ID: &str = "22af7ee7b50b6892e399da6b2c6c4427b25104d4a9048e00b16e9df04407f918";

fn cid(s: &str) -> [u8; 32] {
    hex::decode(s).unwrap().try_into().unwrap()
}

fn other_cid() -> [u8; 32] {
    // the mini sidechain (`src/side_chain.cpp:62-64`)
    cid("3982c91a95aec7fa4250bd126cd8c2dc88173f184071dd2cdb5627a335187ec4")
}

#[test]
fn hash_is_keccak_of_challenge_id_salt() {
    // keccak_custom over CHALLENGE_SIZE * 2 + 32 bytes: challenge, consensus id, salt
    let challenge = [1, 2, 3, 4, 5, 6, 7, 8];
    let salt = [9, 10, 11, 12, 13, 14, 15, 16];
    let id = cid(DEFAULT_ID);
    let mut data = Vec::new();
    data.extend_from_slice(&challenge);
    data.extend_from_slice(&id);
    data.extend_from_slice(&salt);
    assert_eq!(data.len(), 48);
    assert_eq!(
        solution_hash(challenge, &id, salt),
        xkr_crypto::cn_fast_hash(&data).0
    );
    assert_ne!(
        solution_hash(challenge, &other_cid(), salt),
        solution_hash(challenge, &id, salt)
    );
}

fn with_word3(w: u64) -> [u8; 32] {
    let mut h = [0xffu8; 32];
    h[24..].copy_from_slice(&w.to_le_bytes());
    h
}

#[test]
fn work_threshold() {
    // umul128(value[3], 10000, &high); high == 0  <=>  word3 <= (2^64 - 1) / 10000
    assert_eq!(CHALLENGE_DIFFICULTY, 10_000);
    let limit = u64::MAX / CHALLENGE_DIFFICULTY;
    assert_eq!(limit, 1_844_674_407_370_955);
    assert!(has_enough_work(&with_word3(0)));
    assert!(has_enough_work(&with_word3(limit)));
    assert!(!has_enough_work(&with_word3(limit + 1)));
    assert!(!has_enough_work(&with_word3(u64::MAX)));
    // only bytes 24..32 count
    let mut h = with_word3(0);
    h[..24].fill(0xff);
    assert!(has_enough_work(&h));
}

#[test]
fn outgoing_side_finds_work_incoming_side_does_not_search() {
    let challenge = 0x0123_4567_89ab_cdefu64.to_le_bytes();
    let id = cid(DEFAULT_ID);

    let (sol, salt) = solve(challenge, &id, 1000, true);
    assert!(has_enough_work(&sol));
    assert!(u64::from_le_bytes(salt) >= 1000);
    assert_eq!(sol, solution_hash(challenge, &id, salt));
    assert_eq!(check_solution(challenge, &id, &sol, salt, true), Ok(()));

    let (first, salt0) = solve(challenge, &id, 1000, false);
    assert_eq!(u64::from_le_bytes(salt0), 1000);
    assert_eq!(first, solution_hash(challenge, &id, 1000u64.to_le_bytes()));
    assert_eq!(check_solution(challenge, &id, &first, salt0, false), Ok(()));
}

#[test]
fn solution_failures() {
    let challenge = [7u8; 8];
    let id = cid(DEFAULT_ID);
    let (sol, salt) = solve(challenge, &id, 42, true);

    // another sidechain
    assert_eq!(
        check_solution(challenge, &other_cid(), &sol, salt, true),
        Err(HandshakeError::WrongSolution)
    );
    // another challenge
    assert_eq!(
        check_solution([8u8; 8], &id, &sol, salt, true),
        Err(HandshakeError::WrongSolution)
    );
    // another salt
    let mut salt2 = salt;
    salt2[0] ^= 1;
    let r = check_solution(challenge, &id, &sol, salt2, true);
    assert_eq!(r, Err(HandshakeError::WrongSolution));
    // a changed hash byte outside the work word
    let mut bad = sol;
    bad[0] ^= 0x80;
    let r = check_solution(challenge, &id, &bad, salt, true);
    assert_eq!(r, Err(HandshakeError::WrongSolution));

    // a correct hash without work, from a peer that opened the connection
    let mut s = 0u64;
    let (lazy, lazy_salt) = loop {
        let h = solution_hash(challenge, &id, s.to_le_bytes());
        if !has_enough_work(&h) {
            break (h, s.to_le_bytes());
        }
        s += 1;
    };
    assert_eq!(
        check_solution(challenge, &id, &lazy, lazy_salt, true),
        Err(HandshakeError::NotEnoughWork)
    );
    // the same hash is fine from a peer we connected to
    assert_eq!(
        check_solution(challenge, &id, &lazy, lazy_salt, false),
        Ok(())
    );
}

#[test]
fn peer_ids() {
    assert_eq!(check_peer_id(0, &[1, 2]), Err(HandshakeError::ZeroPeerId));
    assert_eq!(check_peer_id(2, &[1, 2]), Err(HandshakeError::OwnPeerId));
    assert_eq!(check_peer_id(3, &[1, 2]), Ok(()));

    let mut seq = [0u64, 5, 5, 0, 7, 5, 9, 11].into_iter();
    let ids = PeerIds::from_source(|| seq.next().unwrap());
    assert_eq!(ids.all(), &[5, 7, 9, 11]);
    assert_eq!(ids.clearnet(), 5);
    assert_eq!(ids.hyper(), 11);

    let r = PeerIds::random();
    assert!(r.all().iter().all(|v| *v != 0));
    let mut v = r.all().to_vec();
    v.sort_unstable();
    v.dedup();
    assert_eq!(v.len(), 4);
}

fn config(direction: Direction, consensus_id: [u8; 32], ids: [u64; 4]) -> SessionConfig {
    SessionConfig {
        consensus_id,
        peer_ids: PeerIds(ids),
        domain: Domain::Tcp,
        direction,
        listen_port: 37889,
        version: VersionRecord::ours(),
    }
}

fn session(
    direction: Direction,
    consensus_id: [u8; 32],
    ids: [u64; 4],
    challenge: u64,
    now: Instant,
) -> PeerSession {
    PeerSession::with_values(config(direction, consensus_id, ids), challenge, 0, now)
}

type Fed = Result<Vec<Event>, PeerError>;

/// Moves bytes both ways until both outboxes are empty.
fn pump(out: &mut PeerSession, inc: &mut PeerSession, now: Instant) -> (Fed, Fed) {
    let mut ev_out = Vec::new();
    let mut ev_inc = Vec::new();
    loop {
        let to_inc = out.take_outbox();
        let to_out = inc.take_outbox();
        if to_inc.is_empty() && to_out.is_empty() {
            return (Ok(ev_out), Ok(ev_inc));
        }
        match inc.feed(&to_inc, now) {
            Ok(e) => ev_inc.extend(e),
            Err(e) => return (Ok(ev_out), Err(e)),
        }
        match out.feed(&to_out, now) {
            Ok(e) => ev_out.extend(e),
            Err(e) => return (Err(e), Ok(ev_inc)),
        }
    }
}

#[test]
fn sessions_complete_the_handshake() {
    let now = Instant::now();
    let id = cid(DEFAULT_ID);
    let mut out = session(Direction::Outgoing, id, [1, 2, 3, 4], 11, now);
    let mut inc = session(Direction::Incoming, id, [5, 6, 7, 8], 22, now);

    // the first bytes on the wire are the challenges
    let first = out.take_outbox();
    assert_eq!(
        first,
        Message::HandshakeChallenge {
            challenge: 11u64.to_le_bytes(),
            peer_id: 1
        }
        .to_bytes()
    );
    let ev = inc.feed(&first, now).unwrap();
    assert_eq!(ev, vec![Event::PeerIdentified { peer_id: 1 }]);

    let (a, b) = pump(&mut out, &mut inc, now);
    let (a, b) = (a.unwrap(), b.unwrap());
    assert!(a.contains(&Event::PeerIdentified { peer_id: 5 }));
    assert!(a.contains(&Event::HandshakeComplete));
    assert!(b.contains(&Event::HandshakeComplete));
    assert!(a.contains(&Event::ListenPort(37889)));
    assert!(b.contains(&Event::ListenPort(37889)));
    // each asked for the other's tip
    assert!(a.contains(&Event::BlockRequest(None)));
    assert!(b.contains(&Event::BlockRequest(None)));
    assert!(out.is_good() && inc.is_good());
    assert_eq!(out.pending_block_requests(), 1);
    assert_eq!(out.peer_id(), Some(5));
    assert_eq!(inc.peer_id(), Some(1));
}

#[test]
fn byte_by_byte_handshake() {
    let now = Instant::now();
    let id = cid(DEFAULT_ID);
    let mut out = session(Direction::Outgoing, id, [1, 2, 3, 4], 1, now);
    let mut inc = session(Direction::Incoming, id, [5, 6, 7, 8], 2, now);
    let mut done = 0;
    let complete = |e: Vec<Event>| e.iter().filter(|e| **e == Event::HandshakeComplete).count();
    for _ in 0..4 {
        for b in out.take_outbox() {
            done += complete(inc.feed(&[b], now).unwrap());
        }
        for b in inc.take_outbox() {
            done += complete(out.feed(&[b], now).unwrap());
        }
    }
    assert_eq!(done, 2);
}

#[test]
fn wrong_consensus_id_fails_on_both_sides() {
    let now = Instant::now();
    let mut out = session(Direction::Outgoing, cid(DEFAULT_ID), [1, 2, 3, 4], 1, now);
    let mut inc = session(Direction::Incoming, other_cid(), [5, 6, 7, 8], 2, now);
    let c_out = out.take_outbox();
    let c_inc = inc.take_outbox();
    // both challenges are answered
    inc.feed(&c_out, now).unwrap();
    out.feed(&c_inc, now).unwrap();
    // neither answer checks out
    assert_eq!(
        inc.feed(&out.take_outbox(), now),
        Err(PeerError::Handshake(HandshakeError::WrongSolution))
    );
    assert_eq!(
        out.feed(&inc.take_outbox(), now),
        Err(PeerError::Handshake(HandshakeError::WrongSolution))
    );
    assert!(!out.handshake_complete() && !inc.handshake_complete());
}

#[test]
fn bad_solutions_are_refused() {
    let now = Instant::now();
    let id = cid(DEFAULT_ID);
    let mut inc = session(Direction::Incoming, id, [5, 6, 7, 8], 2, now);
    let our_challenge = 2u64.to_le_bytes();
    inc.take_outbox();
    inc.feed(
        &Message::HandshakeChallenge {
            challenge: [0; 8],
            peer_id: 77,
        }
        .to_bytes(),
        now,
    )
    .unwrap();

    // right hash, no work: the peer opened the connection, so it must have done the work
    let mut s = 0u64;
    let lazy = loop {
        let h = solution_hash(our_challenge, &id, s.to_le_bytes());
        if !has_enough_work(&h) {
            break Message::HandshakeSolution {
                solution: h,
                salt: s.to_le_bytes(),
            };
        }
        s += 1;
    };
    let mut fresh = session(Direction::Incoming, id, [5, 6, 7, 8], 2, now);
    fresh
        .feed(
            &Message::HandshakeChallenge {
                challenge: [0; 8],
                peer_id: 77,
            }
            .to_bytes(),
            now,
        )
        .unwrap();
    assert_eq!(
        fresh.feed(&lazy.to_bytes(), now),
        Err(PeerError::Handshake(HandshakeError::NotEnoughWork))
    );

    // work but a wrong hash
    let (sol, salt) = solve(our_challenge, &other_cid(), 0, true);
    assert_eq!(
        inc.feed(
            &Message::HandshakeSolution {
                solution: sol,
                salt
            }
            .to_bytes(),
            now
        ),
        Err(PeerError::Handshake(HandshakeError::WrongSolution))
    );
}

#[test]
fn peer_id_checks_in_the_session() {
    let now = Instant::now();
    let id = cid(DEFAULT_ID);
    let mk = || session(Direction::Outgoing, id, [1, 2, 3, 4], 3, now);

    let mut s = mk();
    assert_eq!(
        s.feed(
            &Message::HandshakeChallenge {
                challenge: [0; 8],
                peer_id: 0
            }
            .to_bytes(),
            now
        ),
        Err(PeerError::Handshake(HandshakeError::ZeroPeerId))
    );
    // our own challenge comes back: a connection to ourselves (any of the four ids)
    let mut s = mk();
    let own = s.take_outbox();
    assert_eq!(
        s.feed(&own, now),
        Err(PeerError::Handshake(HandshakeError::OwnPeerId))
    );
    let mut s = mk();
    assert_eq!(
        s.feed(
            &Message::HandshakeChallenge {
                challenge: [0; 8],
                peer_id: 4
            }
            .to_bytes(),
            now
        ),
        Err(PeerError::Handshake(HandshakeError::OwnPeerId))
    );
}

#[test]
fn hyperswarm_connections_send_the_fourth_id() {
    let now = Instant::now();
    let mut cfg = config(Direction::Outgoing, cid(DEFAULT_ID), [1, 2, 3, 4]);
    cfg.domain = Domain::Hyper;
    let mut s = PeerSession::with_values(cfg, 5, 0, now);
    assert_eq!(
        s.take_outbox(),
        Message::HandshakeChallenge {
            challenge: 5u64.to_le_bytes(),
            peer_id: 4
        }
        .to_bytes()
    );
}
