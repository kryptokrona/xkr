//! Peer list persistence: `<data-dir>/p2pstate.json`.
//!
//! The C++ node keeps its peer lists and peer id in a boost-serialized `p2pstate.bin`; we use
//! our own small JSON file instead (readable, versioned, no binary compatibility promise):
//!
//! ```json
//! { "format": "xkr-p2pstate/1", "network": "mainnet", "peer_id": 1234, "saved_at": 1763000000,
//!   "white": [{ "addr": "1.2.3.4:11897", "id": 99, "last_seen": 1763000000 },
//!             { "addr": "1.2.3.5:11897", "id": 98, "last_seen": 1763000000,
//!               "pruning_seed": 1026 }],
//!   "gray":  [...],
//!   "bans":  [{ "ip": "5.6.7.8", "until": 1763086400 }] }
//! ```
//!
//! `pruning_seed` (optional, absent = full or unknown) is the raw seed the peer announced in its
//! `CORE_SYNC_DATA` (`stripes << 8 | stripe`, `0x0100` for "recent blocks only",
//! `docs/node-architecture.md`, "Pruned peers"), so a restarted node still knows which
//! addresses are pruned when it looks for a stripe. In memory the seeds live in a [`SeedBook`]
//! of at most [`MAX_PEER_SEEDS`] addresses.
//!
//! It is written every few minutes and on shutdown (to a temporary file, then renamed), and
//! read once at start. A missing or unreadable file just means an empty peer list.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr, SocketAddrV4};
use std::path::Path;

use serde_json::{Value, json};
use xkr_consensus::Network;
use xkr_p2p_legacy::wire::PeerlistEntry;

/// File name inside the data directory.
pub const STATE_FILE: &str = "p2pstate.json";
const FORMAT: &str = "xkr-p2pstate/1";

/// What is persisted.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SavedState {
    /// Our peer id (kept stable across restarts like C++).
    pub peer_id: Option<u64>,
    /// White list.
    pub white: Vec<PeerlistEntry>,
    /// Gray list.
    pub gray: Vec<PeerlistEntry>,
    /// Banned addresses with the unix time the ban ends.
    pub bans: Vec<(IpAddr, u64)>,
    /// Raw pruning seeds of white/gray addresses that announced a non-zero one.
    pub pruning_seeds: HashMap<SocketAddrV4, u32>,
}

fn network_name(network: Network) -> &'static str {
    match network {
        Network::Mainnet => "mainnet",
        Network::Testnet => "testnet",
    }
}

fn entry_json(e: &PeerlistEntry, seeds: &HashMap<SocketAddrV4, u32>) -> Value {
    let mut v = json!({ "addr": e.addr().to_string(), "id": e.id, "last_seen": e.last_seen });
    if let Some(seed) = seeds.get(&e.addr()).filter(|s| **s != 0)
        && let Some(o) = v.as_object_mut()
    {
        o.insert("pruning_seed".into(), Value::from(*seed));
    }
    v
}

fn entries_json(entries: &[PeerlistEntry], seeds: &HashMap<SocketAddrV4, u32>) -> Vec<Value> {
    entries.iter().map(|e| entry_json(e, seeds)).collect()
}

fn entry_from_json(v: &Value) -> Option<(PeerlistEntry, Option<u32>)> {
    let addr: SocketAddrV4 = v.get("addr")?.as_str()?.parse().ok()?;
    let (ip, port) = PeerlistEntry::encode_addr(addr);
    let seed = v
        .get("pruning_seed")
        .and_then(Value::as_u64)
        .and_then(|s| u32::try_from(s).ok())
        .filter(|s| *s != 0);
    Some((
        PeerlistEntry {
            ip,
            port,
            id: v.get("id").and_then(Value::as_u64).unwrap_or(0),
            last_seen: v.get("last_seen").and_then(Value::as_u64).unwrap_or(0),
        },
        seed,
    ))
}

/// Serializes `state` for `network`, stamped with `now`.
#[must_use]
pub fn encode(state: &SavedState, network: Network, now: u64) -> String {
    let v = json!({
        "format": FORMAT,
        "network": network_name(network),
        "peer_id": state.peer_id,
        "saved_at": now,
        "white": entries_json(&state.white, &state.pruning_seeds),
        "gray": entries_json(&state.gray, &state.pruning_seeds),
        "bans": state.bans.iter().map(|(ip, until)| json!({ "ip": ip.to_string(), "until": until })).collect::<Vec<_>>(),
    });
    serde_json::to_string_pretty(&v).unwrap_or_default()
}

/// Parses a state file; `None` if it is not ours or belongs to another network. Expired bans
/// (`until <= now`) are dropped.
#[must_use]
pub fn decode(text: &str, network: Network, now: u64) -> Option<SavedState> {
    let v: Value = serde_json::from_str(text).ok()?;
    if v.get("format")?.as_str()? != FORMAT || v.get("network")?.as_str()? != network_name(network)
    {
        return None;
    }
    let mut pruning_seeds = HashMap::new();
    let mut list = |key: &str| -> Vec<PeerlistEntry> {
        let entries: Vec<(PeerlistEntry, Option<u32>)> = v
            .get(key)
            .and_then(Value::as_array)
            .map_or_default(|a| a.iter().filter_map(entry_from_json).collect());
        entries
            .into_iter()
            .map(|(e, seed)| {
                if let Some(seed) = seed {
                    pruning_seeds.insert(e.addr(), seed);
                }
                e
            })
            .collect()
    };
    let white = list("white");
    let gray = list("gray");
    let bans = v.get("bans").and_then(Value::as_array).map_or_default(|a| {
        a.iter()
            .filter_map(|b| {
                let ip: IpAddr = b.get("ip")?.as_str()?.parse().ok()?;
                let until = b.get("until")?.as_u64()?;
                (until > now).then_some((ip, until))
            })
            .collect()
    });
    Some(SavedState {
        peer_id: v.get("peer_id").and_then(Value::as_u64),
        white,
        gray,
        bans,
        pruning_seeds,
    })
}

/// Reads the state file of `dir`.
#[must_use]
pub fn load(dir: &Path, network: Network, now: u64) -> Option<SavedState> {
    let path = dir.join(STATE_FILE);
    let text = std::fs::read_to_string(&path).ok()?;
    let state = decode(&text, network, now);
    if state.is_none() {
        tracing::warn!(path = %path.display(), "ignoring an unreadable peer state file");
    }
    state
}

/// Most peer addresses whose pruning seed is remembered in memory ([`SeedBook`]).
pub const MAX_PEER_SEEDS: usize = 4_096;

/// Raw pruning seeds (non-zero) of peer addresses, at most `cap` of them: remembering a new
/// address above the cap forgets the one remembered or refreshed longest ago. Peers that are
/// connected refresh their entry at every timed sync, so a stream of reconnects with new
/// ports only pushes out stale entries.
#[derive(Debug, Clone)]
pub struct SeedBook {
    /// Address -> (raw seed, sequence number of the last remember).
    seeds: HashMap<SocketAddrV4, (u32, u64)>,
    next: u64,
    cap: usize,
}

impl SeedBook {
    /// An empty book of at most `cap` addresses (at least one).
    #[must_use]
    pub fn new(cap: usize) -> Self {
        Self {
            seeds: HashMap::new(),
            next: 0,
            cap: cap.max(1),
        }
    }

    /// A book filled from the saved state (beyond `cap` entries, arbitrary ones are dropped).
    #[must_use]
    pub fn from_saved(saved: HashMap<SocketAddrV4, u32>, cap: usize) -> Self {
        let mut book = Self::new(cap);
        for (addr, raw) in saved {
            book.remember(addr, raw);
        }
        book
    }

    /// Records the raw seed `addr` announced; 0 (a full node) forgets the address.
    pub fn remember(&mut self, addr: SocketAddrV4, raw: u32) {
        if raw == 0 {
            self.seeds.remove(&addr);
            return;
        }
        self.next += 1;
        self.seeds.insert(addr, (raw, self.next));
        while self.seeds.len() > self.cap {
            let oldest = self
                .seeds
                .iter()
                .min_by_key(|(_, (_, seq))| *seq)
                .map(|(a, _)| *a);
            let Some(oldest) = oldest else {
                break;
            };
            self.seeds.remove(&oldest);
        }
    }

    /// The raw seed remembered for `addr`.
    #[must_use]
    pub fn get(&self, addr: SocketAddrV4) -> Option<u32> {
        self.seeds.get(&addr).map(|(raw, _)| *raw)
    }

    /// Every `(address, raw seed)`.
    pub fn entries(&self) -> impl Iterator<Item = (SocketAddrV4, u32)> + '_ {
        self.seeds.iter().map(|(a, (raw, _))| (*a, *raw))
    }
}

/// Writes `contents` to the state file of `dir` atomically (temporary file + rename).
pub fn store(dir: &Path, contents: &str) -> std::io::Result<()> {
    let path = dir.join(STATE_FILE);
    let tmp = dir.join(format!("{STATE_FILE}.tmp"));
    std::fs::write(&tmp, contents)?;
    std::fs::rename(&tmp, &path)
}

/// Whether a socket address can be stored in a (IPv4-only) peer list entry.
#[must_use]
pub fn as_v4(addr: &SocketAddr) -> Option<SocketAddrV4> {
    match addr {
        SocketAddr::V4(a) => Some(*a),
        SocketAddr::V6(a) => a
            .ip()
            .to_ipv4_mapped()
            .map(|ip| SocketAddrV4::new(ip, a.port())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_expiry() {
        let (ip, port) = PeerlistEntry::encode_addr("1.2.3.4:11897".parse().unwrap());
        let state = SavedState {
            peer_id: Some(u64::MAX - 5),
            white: vec![PeerlistEntry {
                ip,
                port,
                id: 7,
                last_seen: 100,
            }],
            gray: vec![],
            bans: vec![
                ("5.6.7.8".parse().unwrap(), 2_000),
                ("9.9.9.9".parse().unwrap(), 500),
            ],
            pruning_seeds: HashMap::new(),
        };
        let text = encode(&state, Network::Testnet, 1_000);
        assert!(!text.contains("pruning_seed"), "full peers carry no seed");
        let back = decode(&text, Network::Testnet, 1_000).unwrap();
        assert_eq!(back.peer_id, state.peer_id);
        assert_eq!(back.white, state.white);
        assert_eq!(back.bans, vec![("5.6.7.8".parse().unwrap(), 2_000)]);
        assert!(back.pruning_seeds.is_empty());
        assert!(decode(&text, Network::Mainnet, 1_000).is_none());
        assert!(decode("{}", Network::Testnet, 0).is_none());
    }

    /// Stripe-pruned peers keep their seed across restarts, in the white and the gray list; a
    /// seed for an address in neither list is not written.
    #[test]
    fn pruning_seeds_round_trip() {
        let entry = |addr: &str, id: u64| {
            let (ip, port) = PeerlistEntry::encode_addr(addr.parse().unwrap());
            PeerlistEntry {
                ip,
                port,
                id,
                last_seen: 100,
            }
        };
        let white = entry("1.2.3.4:11897", 1);
        let gray = entry("1.2.3.5:11897", 2);
        let full = entry("1.2.3.6:11897", 3);
        let mut seeds = HashMap::new();
        seeds.insert(white.addr(), 0x0402);
        seeds.insert(gray.addr(), 0x0404);
        seeds.insert("9.9.9.9:1".parse().unwrap(), 0x0401);
        let state = SavedState {
            peer_id: Some(5),
            white: vec![white, full],
            gray: vec![gray],
            bans: vec![],
            pruning_seeds: seeds,
        };
        let text = encode(&state, Network::Mainnet, 1_000);
        let back = decode(&text, Network::Mainnet, 1_000).unwrap();
        assert_eq!(back.white, vec![white, full]);
        assert_eq!(back.gray, vec![gray]);
        assert_eq!(back.pruning_seeds.len(), 2);
        assert_eq!(back.pruning_seeds.get(&white.addr()), Some(&0x0402));
        assert_eq!(back.pruning_seeds.get(&gray.addr()), Some(&0x0404));
        assert_eq!(back.pruning_seeds.get(&full.addr()), None);

        // Files written before stripe pruning load as before.
        let old = r#"{ "format": "xkr-p2pstate/1", "network": "mainnet",
            "white": [{ "addr": "1.2.3.4:11897", "id": 1, "last_seen": 100 }] }"#;
        let back = decode(old, Network::Mainnet, 1_000).unwrap();
        assert_eq!(back.white, vec![white]);
        assert!(back.pruning_seeds.is_empty());
    }

    /// An inbound peer reconnecting with a new announced port every time cannot grow the seed
    /// book beyond its cap; the entries refreshed most recently stay.
    #[test]
    fn seed_book_stays_bounded() {
        let addr = |port: u16| SocketAddrV4::new(std::net::Ipv4Addr::new(10, 0, 0, 1), port);
        let mut book = SeedBook::new(4);
        for port in 1..=1_000 {
            book.remember(addr(port), 0x0401);
        }
        assert_eq!(book.entries().count(), 4);
        for port in 997..=1_000 {
            assert_eq!(book.get(addr(port)), Some(0x0401));
        }
        assert_eq!(book.get(addr(1)), None);
        // A refresh (a timed sync of a connected peer) keeps an entry; the oldest other goes.
        book.remember(addr(997), 0x0402);
        book.remember(addr(2_000), 0x0401);
        assert_eq!(book.entries().count(), 4);
        assert_eq!(book.get(addr(997)), Some(0x0402));
        assert_eq!(book.get(addr(998)), None);
        // A full seed forgets the address.
        book.remember(addr(997), 0);
        assert_eq!(book.get(addr(997)), None);
        assert_eq!(book.entries().count(), 3);
        // A saved state larger than the cap loads bounded.
        let saved: HashMap<SocketAddrV4, u32> = (1..=50).map(|p| (addr(p), 0x0403)).collect();
        let loaded = SeedBook::from_saved(saved, 8);
        assert_eq!(loaded.entries().count(), 8);
        assert_eq!(
            loaded.entries().filter(|(_, raw)| *raw == 0x0403).count(),
            8
        );
        assert_eq!(SeedBook::new(0).entries().count(), 0);
        assert_eq!(MAX_PEER_SEEDS, 4_096);
    }
}
