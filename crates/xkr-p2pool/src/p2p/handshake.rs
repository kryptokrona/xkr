//! The handshake of `P2PServer::P2PClient` (`src/p2p_server.h:99-112`, `src/p2p_server.cpp`
//! `send_handshake_challenge`, `send_handshake_solution`, `check_handshake_solution`,
//! `on_handshake_challenge`, `on_handshake_solution`).
//!
//! Both sides send a random 8-byte challenge and their peer id. Each answers the other's
//! challenge with `H = keccak(challenge || consensus id || salt)` and the salt; the side that
//! opened the connection must find a salt whose `H` carries a little proof of work
//! (difficulty 10 000). The consensus id itself is never sent: two nodes of different sidechains
//! cannot produce each other's `H`.

use super::message::{CHALLENGE_SIZE, HASH_SIZE};

/// `CHALLENGE_DIFFICULTY`.
pub const CHALLENGE_DIFFICULTY: u64 = 10_000;

/// Why a handshake failed. Every case bans the peer.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum HandshakeError {
    /// The peer sent peer id 0.
    #[error("peer id 0")]
    ZeroPeerId,
    /// The peer sent one of our own peer ids (a connection to ourselves).
    #[error("connection to self")]
    OwnPeerId,
    /// The solution is not `keccak(our challenge || consensus id || salt)`; a peer of another
    /// sidechain ends here.
    #[error("wrong handshake solution")]
    WrongSolution,
    /// The solution of an incoming peer does not meet [`CHALLENGE_DIFFICULTY`].
    #[error("not enough work in the handshake solution")]
    NotEnoughWork,
}

/// `H = keccak(challenge || consensus_id || salt)`, the hash of `keccak_custom` in
/// `send_handshake_solution` and `check_handshake_solution`.
#[must_use]
pub fn solution_hash(
    challenge: [u8; CHALLENGE_SIZE],
    consensus_id: &[u8; 32],
    salt: [u8; CHALLENGE_SIZE],
) -> [u8; HASH_SIZE] {
    let mut data = [0u8; CHALLENGE_SIZE * 2 + 32];
    data[..CHALLENGE_SIZE].copy_from_slice(&challenge);
    data[CHALLENGE_SIZE..CHALLENGE_SIZE + 32].copy_from_slice(consensus_id);
    data[CHALLENGE_SIZE + 32..].copy_from_slice(&salt);
    xkr_crypto::cn_fast_hash(&data).0
}

/// The work test: the high 64 bits of `word3 * CHALLENGE_DIFFICULTY` are zero, where `word3` is
/// bytes 24..32 of `H` as a little-endian `u64` (`umul128(value[3], CHALLENGE_DIFFICULTY)`).
#[must_use]
pub fn has_enough_work(solution: &[u8; HASH_SIZE]) -> bool {
    let mut w = [0u8; 8];
    w.copy_from_slice(&solution[24..32]);
    let word3 = u64::from_le_bytes(w);
    (u128::from(word3) * u128::from(CHALLENGE_DIFFICULTY)) >> 64 == 0
}

/// Answers a peer's challenge, as `send_handshake_solution` does. The incoming side (the peer
/// opened the connection) sends the hash of its first salt; the outgoing side increments the
/// salt until [`has_enough_work`] holds (about 10 000 hashes, a few milliseconds).
#[must_use]
pub fn solve(
    challenge: [u8; CHALLENGE_SIZE],
    consensus_id: &[u8; 32],
    first_salt: u64,
    outgoing: bool,
) -> ([u8; HASH_SIZE], [u8; CHALLENGE_SIZE]) {
    let mut salt = first_salt;
    loop {
        let salt_bytes = salt.to_le_bytes();
        let h = solution_hash(challenge, consensus_id, salt_bytes);
        if !outgoing || has_enough_work(&h) {
            return (h, salt_bytes);
        }
        salt = salt.wrapping_add(1);
    }
}

/// Checks a peer's answer to our challenge (`on_handshake_solution`): the work test when the
/// peer opened the connection, then `H` against our own computation.
pub fn check_solution(
    our_challenge: [u8; CHALLENGE_SIZE],
    consensus_id: &[u8; 32],
    solution: &[u8; HASH_SIZE],
    salt: [u8; CHALLENGE_SIZE],
    peer_is_outgoing: bool,
) -> Result<(), HandshakeError> {
    if peer_is_outgoing && !has_enough_work(solution) {
        return Err(HandshakeError::NotEnoughWork);
    }
    if solution_hash(our_challenge, consensus_id, salt) != *solution {
        return Err(HandshakeError::WrongSolution);
    }
    Ok(())
}

/// Checks a peer id from a challenge (`on_handshake_challenge`): not 0, not one of ours.
pub fn check_peer_id(peer_id: u64, own_ids: &[u64]) -> Result<(), HandshakeError> {
    if peer_id == 0 {
        return Err(HandshakeError::ZeroPeerId);
    }
    if own_ids.contains(&peer_id) {
        return Err(HandshakeError::OwnPeerId);
    }
    Ok(())
}

/// The node's peer ids (`P2PServer::m_peerId`): four random, non-zero, distinct values for
/// clearnet, Tor, I2P and both. TCP connections use [`PeerIds::clearnet`]; Hyperswarm
/// connections are a separate domain and use [`PeerIds::hyper`] (the "both" slot), so one peer
/// reached over both transports is not taken for a duplicate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PeerIds(pub [u64; 4]);

impl PeerIds {
    /// Four random ids from the OS random source.
    #[must_use]
    pub fn random() -> Self {
        Self::from_source(super::random_u64)
    }

    /// Four ids drawn from `next`, redrawn while zero or equal to an earlier one.
    #[must_use]
    pub fn from_source(mut next: impl FnMut() -> u64) -> Self {
        let mut ids = [0u64; 4];
        for i in 0..ids.len() {
            loop {
                let v = next();
                if v != 0 && !ids[..i].contains(&v) {
                    ids[i] = v;
                    break;
                }
            }
        }
        Self(ids)
    }

    /// The id sent on TCP connections.
    #[must_use]
    pub const fn clearnet(&self) -> u64 {
        self.0[0]
    }

    /// The id sent on Hyperswarm connections.
    #[must_use]
    pub const fn hyper(&self) -> u64 {
        self.0[3]
    }

    /// All four, for the self-connection test.
    #[must_use]
    pub const fn all(&self) -> &[u64; 4] {
        &self.0
    }
}
