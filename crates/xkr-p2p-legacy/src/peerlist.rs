//! White/gray peer list bookkeeping, mirroring `src/p2p/peer_list_manager.cpp` and
//! `src/p2p/peerlist.cpp`.
//!
//! Plain data owned by one task (the node's connection manager); connections never touch
//! it directly. They report remote lists as [`crate::peer::PeerEvent::RemotePeerList`] and
//! read a snapshot of [`PeerList::head`] through a `watch` channel.

use std::collections::HashMap;
use std::net::Ipv4Addr;

use crate::{
    constants::{
        MAX_GRAY_SOURCES_TRACKED, MAX_NEW_GRAY_PER_MERGE, MAX_REMOTE_PEERLIST_ENTRIES,
        P2P_LOCAL_GRAY_PEERLIST_LIMIT, P2P_LOCAL_WHITE_PEERLIST_LIMIT,
    },
    wire::PeerlistEntry,
};

/// Errors from [`PeerList::merge_remote`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum PeerListError {
    /// `fix_time_delta`: an entry claims to be seen after the remote's own clock.
    #[error("remote peer list contains an entry from the future")]
    FutureEntry,
}

/// White list = peers we connected to (or that answered a back-ping); gray list = peers
/// we only heard about.
///
/// Gray-list merging: C++ merges every remote entry and trims by `last_seen`, so one peer
/// announcing hundreds of addresses "seen now" in every timed sync pushes all other entries out.
/// Here a remote list adds at most [`MAX_NEW_GRAY_PER_MERGE`] new addresses, addresses that cannot
/// be dialled are ignored ([`PeerList::is_gossip_allowed`]), and every gray entry remembers which
/// peers (by IPv4 address, up to [`MAX_GRAY_SOURCES_TRACKED`]) reported it: the trim drops
/// single-source entries first and [`PeerList::gray_multi_source`] lets the dialler prefer the
/// others.
#[derive(Debug, Clone)]
pub struct PeerList {
    pub white: Vec<PeerlistEntry>,
    pub gray: Vec<PeerlistEntry>,
    pub white_limit: usize,
    pub gray_limit: usize,
    /// `--allow-local-ip`: accept RFC 1918 addresses (needed on a docker testnet).
    pub allow_local_ip: bool,
    /// Gray `(ip, port)` -> IPv4 addresses (little-endian `u32`, like `PeerlistEntry::ip`) of
    /// the peers that reported it.
    sources: HashMap<(u32, u32), Vec<u32>>,
}

impl Default for PeerList {
    fn default() -> Self {
        Self::new(false)
    }
}

impl PeerList {
    #[must_use]
    pub fn new(allow_local_ip: bool) -> Self {
        Self {
            white: Vec::new(),
            gray: Vec::new(),
            white_limit: P2P_LOCAL_WHITE_PEERLIST_LIMIT,
            gray_limit: P2P_LOCAL_GRAY_PEERLIST_LIMIT,
            allow_local_ip,
            sources: HashMap::new(),
        }
    }

    /// [`Self::is_ip_allowed`] plus what no remote list should make us dial: a zero port, the
    /// unspecified, broadcast and multicast addresses, and (without `allow_local_ip`)
    /// link-local and shared (`100.64.0.0/10`) addresses.
    #[must_use]
    pub fn is_gossip_allowed(&self, entry: &PeerlistEntry) -> bool {
        if entry.port == 0 || entry.port > u32::from(u16::MAX) || !self.is_ip_allowed(entry.ip) {
            return false;
        }
        let addr = Ipv4Addr::from(entry.ip.to_le_bytes());
        let o = addr.octets();
        let shared = o[0] == 100 && (o[1] & 0xc0) == 64;
        !(addr.is_unspecified()
            || addr.is_broadcast()
            || addr.is_multicast()
            || (!self.allow_local_ip && (addr.is_link_local() || shared)))
    }

    /// How many distinct peers reported the gray entry `(ip, port)` (saturating at
    /// [`MAX_GRAY_SOURCES_TRACKED`]).
    #[must_use]
    pub fn gray_sources(&self, ip: u32, port: u32) -> usize {
        self.sources.get(&(ip, port)).map_or(0, Vec::len)
    }

    /// Gray entries reported by at least two distinct peers: the dialler prefers them, since
    /// one peer alone cannot put an address there.
    #[must_use]
    pub fn gray_multi_source(&self) -> Vec<PeerlistEntry> {
        self.gray
            .iter()
            .copied()
            .filter(|e| self.gray_sources(e.ip, e.port) >= 2)
            .collect()
    }

    fn note_source(&mut self, ip: u32, port: u32, source: u32) {
        let v = self.sources.entry((ip, port)).or_default();
        if v.len() < MAX_GRAY_SOURCES_TRACKED && !v.contains(&source) {
            v.push(source);
        }
    }

    /// The gray list above its limit: entries reported by fewer peers go first, then the
    /// least recently seen (C++ trims by `last_seen` only).
    fn trim_gray(&mut self) {
        if self.gray.len() > self.gray_limit {
            let sources = &self.sources;
            let count = |e: &PeerlistEntry| sources.get(&(e.ip, e.port)).map_or(0, Vec::len);
            self.gray.sort_unstable_by(|a, b| {
                count(b).cmp(&count(a)).then(b.last_seen.cmp(&a.last_seen))
            });
            self.gray.truncate(self.gray_limit);
        }
        if self.sources.len() > self.gray.len() {
            let keep: std::collections::HashSet<(u32, u32)> =
                self.gray.iter().map(|e| (e.ip, e.port)).collect();
            self.sources.retain(|k, _| keep.contains(k));
        }
    }

    /// `PeerlistManager::is_ip_allowed`: never loopback; private ranges only with
    /// `allow_local_ip`.
    #[must_use]
    pub fn is_ip_allowed(&self, ip: u32) -> bool {
        let addr = Ipv4Addr::from(ip.to_le_bytes());
        if addr.is_loopback() {
            return false;
        }
        if !self.allow_local_ip && addr.is_private() {
            return false;
        }
        true
    }

    /// `NodeServer::handle_remote_peerlist` = `fix_time_delta` + `merge_peerlist`.
    ///
    /// Shifts every `last_seen` by `now - remote_local_time` (the remote's clock offset) and
    /// adds the entries to the gray list. Fails without modifying anything if an entry is
    /// newer than `remote_local_time` (the C++ node then drops the connection).
    ///
    /// Only the first [`MAX_REMOTE_PEERLIST_ENTRIES`] entries are merged, and the gray list is
    /// trimmed once at the end: a 1 MiB peer list (43 690 entries) with a trim (sort of up to
    /// 5 000 entries) per insert cost seconds of CPU on the node's owner task.
    pub fn merge_remote(
        &mut self,
        entries: &[PeerlistEntry],
        remote_local_time: u64,
        now: u64,
    ) -> Result<(), PeerListError> {
        self.merge_remote_from(entries, remote_local_time, now, None)
    }

    /// [`Self::merge_remote`] for a list reported by the peer at IPv4 address `source`
    /// (little-endian `u32`), which is recorded per gray entry (see the type docs). At most
    /// [`MAX_NEW_GRAY_PER_MERGE`] addresses not yet in either list are added per call; known
    /// ones are refreshed as before.
    pub fn merge_remote_from(
        &mut self,
        entries: &[PeerlistEntry],
        remote_local_time: u64,
        now: u64,
        source: Option<u32>,
    ) -> Result<(), PeerListError> {
        if entries.iter().any(|e| e.last_seen > remote_local_time) {
            return Err(PeerListError::FutureEntry);
        }
        let delta = now.wrapping_sub(remote_local_time) as i64;
        let mut added = 0usize;
        for e in entries.iter().take(MAX_REMOTE_PEERLIST_ENTRIES) {
            let mut e = *e;
            if !self.is_gossip_allowed(&e) {
                continue;
            }
            let known = self.white.iter().any(|p| same_addr(p, &e))
                || self.gray.iter().any(|p| same_addr(p, &e));
            if !known {
                if added >= MAX_NEW_GRAY_PER_MERGE {
                    continue;
                }
                added += 1;
            }
            e.last_seen = e.last_seen.wrapping_add_signed(delta);
            self.insert_gray(e);
            if let Some(src) = source
                && self.gray.iter().any(|p| same_addr(p, &e))
            {
                self.note_source(e.ip, e.port, src);
            }
        }
        self.trim_gray();
        Ok(())
    }

    /// `append_with_peer_white`: insert or update by address, remove from gray.
    pub fn append_white(&mut self, entry: PeerlistEntry) {
        if !self.is_ip_allowed(entry.ip) {
            return;
        }
        if let Some(p) = self.white.iter_mut().find(|p| same_addr(p, &entry)) {
            *p = entry;
        } else {
            self.white.push(entry);
            trim(&mut self.white, self.white_limit);
        }
        self.gray.retain(|p| !same_addr(p, &entry));
        self.sources.remove(&(entry.ip, entry.port));
    }

    /// `append_with_peer_gray`: ignored if already white; insert or update otherwise.
    pub fn append_gray(&mut self, entry: PeerlistEntry) {
        if self.insert_gray(entry) {
            self.trim_gray();
        }
    }

    /// [`Self::append_gray`] without the trim; true if the gray list grew.
    fn insert_gray(&mut self, entry: PeerlistEntry) -> bool {
        if !self.is_ip_allowed(entry.ip) {
            return false;
        }
        if self.white.iter().any(|p| same_addr(p, &entry)) {
            return false;
        }
        if let Some(p) = self.gray.iter_mut().find(|p| same_addr(p, &entry)) {
            *p = entry;
            false
        } else {
            self.gray.push(entry);
            true
        }
    }

    /// Removes `(ip, port)` from both lists (failed outbound attempts, bans).
    pub fn remove(&mut self, ip: u32, port: u32) {
        self.white.retain(|p| !(p.ip == ip && p.port == port));
        self.gray.retain(|p| !(p.ip == ip && p.port == port));
        self.sources.remove(&(ip, port));
    }

    /// Removes every entry with address `ip` (the address was banned).
    pub fn remove_ip(&mut self, ip: u32) {
        self.white.retain(|p| p.ip != ip);
        self.gray.retain(|p| p.ip != ip);
        self.sources.retain(|k, _| k.0 != ip);
    }

    /// `set_peer_just_seen`: whitelist `(ip, port)` with `last_seen = now`.
    pub fn set_just_seen(&mut self, peer_id: u64, ip: u32, port: u32, now: u64) {
        self.append_white(PeerlistEntry {
            ip,
            port,
            id: peer_id,
            last_seen: now,
        });
    }

    /// `get_peerlist_head`: newest white entries (skipping `last_seen == 0`), at most
    /// `depth` of them. (The C++ loop has an off-by-two and can return `depth + 2`; we
    /// return exactly `depth`.)
    #[must_use]
    pub fn head(&self, depth: usize) -> Vec<PeerlistEntry> {
        let mut v: Vec<PeerlistEntry> = self
            .white
            .iter()
            .copied()
            .filter(|p| p.last_seen != 0)
            .collect();
        v.sort_unstable_by_key(|p| std::cmp::Reverse(p.last_seen));
        v.truncate(depth);
        v
    }

    /// `Peerlist::get(entry, index)` on a newest-first ordering, used to pick candidates.
    #[must_use]
    pub fn white_by_recency(&self, index: usize) -> Option<PeerlistEntry> {
        nth_newest(&self.white, index)
    }

    /// Same as [`Self::white_by_recency`] for the gray list.
    #[must_use]
    pub fn gray_by_recency(&self, index: usize) -> Option<PeerlistEntry> {
        nth_newest(&self.gray, index)
    }
}

fn same_addr(a: &PeerlistEntry, b: &PeerlistEntry) -> bool {
    a.ip == b.ip && a.port == b.port
}

/// `Peerlist::trim`: keep the `max` most recently seen.
fn trim(list: &mut Vec<PeerlistEntry>, max: usize) {
    if list.len() <= max {
        return;
    }
    list.sort_unstable_by_key(|p| std::cmp::Reverse(p.last_seen));
    list.truncate(max);
}

fn nth_newest(list: &[PeerlistEntry], index: usize) -> Option<PeerlistEntry> {
    if index >= list.len() {
        return None;
    }
    let mut v = list.to_vec();
    v.sort_unstable_by_key(|p| std::cmp::Reverse(p.last_seen));
    Some(v[index])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn e(a: [u8; 4], port: u32, last_seen: u64) -> PeerlistEntry {
        PeerlistEntry {
            ip: u32::from_le_bytes(a),
            port,
            id: u64::from(a[3]),
            last_seen,
        }
    }

    #[test]
    fn merge_shifts_time_and_rejects_future() {
        let mut pl = PeerList::new(false);
        pl.merge_remote(&[e([8, 8, 8, 8], 1, 90)], 100, 1000)
            .unwrap();
        assert_eq!(pl.gray[0].last_seen, 990);
        assert_eq!(
            pl.merge_remote(&[e([8, 8, 4, 4], 1, 101)], 100, 1000),
            Err(PeerListError::FutureEntry)
        );
    }

    #[test]
    fn white_moves_out_of_gray_and_filters_ips() {
        let mut pl = PeerList::new(false);
        pl.append_gray(e([1, 2, 3, 4], 11897, 5));
        pl.append_white(e([1, 2, 3, 4], 11897, 6));
        assert_eq!(pl.gray, Vec::<PeerlistEntry>::new());
        assert_eq!(pl.white.len(), 1);
        pl.append_white(e([127, 0, 0, 1], 11897, 6));
        pl.append_white(e([192, 168, 1, 1], 11897, 6));
        assert_eq!(pl.white.len(), 1);
        let mut local = PeerList::new(true);
        local.append_white(e([172, 20, 0, 2], 11898, 6));
        assert_eq!(local.white.len(), 1);
    }

    /// One remote list adds at most `MAX_NEW_GRAY_PER_MERGE` new addresses; known ones
    /// are still refreshed; undiallable addresses are ignored.
    #[test]
    fn one_merge_adds_a_bounded_number_of_new_entries() {
        let mut pl = PeerList::new(false);
        let many: Vec<PeerlistEntry> = (0..300u32)
            .map(|i| e([8, 1, (i >> 8) as u8, i as u8], 11897, 50))
            .collect();
        pl.merge_remote_from(&many, 100, 100, Some(1)).unwrap();
        assert_eq!(pl.gray.len(), MAX_NEW_GRAY_PER_MERGE);
        // The next list carries the same first entries (refreshed) and more new ones.
        pl.merge_remote_from(&many, 100, 100, Some(1)).unwrap();
        assert_eq!(pl.gray.len(), 2 * MAX_NEW_GRAY_PER_MERGE);
        let junk = [
            e([0, 0, 0, 0], 11897, 1),
            e([255, 255, 255, 255], 11897, 1),
            e([224, 0, 0, 1], 11897, 1),
            e([169, 254, 1, 1], 11897, 1),
            e([100, 64, 0, 1], 11897, 1),
            e([127, 0, 0, 1], 11897, 1),
            e([10, 0, 0, 1], 11897, 1),
            e([9, 9, 9, 9], 0, 1),
            e([9, 9, 9, 9], 70_000, 1),
        ];
        let mut fresh = PeerList::new(false);
        fresh.merge_remote(&junk, 100, 100).unwrap();
        assert!(fresh.gray.is_empty(), "{:?}", fresh.gray);
    }

    /// Entries reported by two peers survive a burst of "seen now" entries from one peer,
    /// and the dialler can pick them out.
    #[test]
    fn multi_source_entries_survive_a_single_source_burst() {
        let mut pl = PeerList::new(false);
        pl.gray_limit = 100;
        let shared: Vec<PeerlistEntry> = (0..20u8).map(|i| e([8, 8, 0, i], 11897, 10)).collect();
        pl.merge_remote_from(&shared, 1_000, 1_000, Some(101))
            .unwrap();
        pl.merge_remote_from(&shared, 1_000, 1_000, Some(102))
            .unwrap();
        assert_eq!(pl.gray_multi_source().len(), 20);
        for round in 0..20u32 {
            let burst: Vec<PeerlistEntry> = (0..MAX_NEW_GRAY_PER_MERGE as u32)
                .map(|i| {
                    let n = round * 1000 + i;
                    e([7, (n >> 16) as u8, (n >> 8) as u8, n as u8], 11897, 2_000)
                })
                .collect();
            pl.merge_remote_from(&burst, 2_000, 2_000, Some(666))
                .unwrap();
        }
        assert_eq!(pl.gray.len(), 100);
        for h in &shared {
            assert!(pl.gray.iter().any(|g| same_addr(g, h)), "{h:?} evicted");
            assert_eq!(pl.gray_sources(h.ip, h.port), 2);
        }
        pl.remove_ip(shared[0].ip);
        assert_eq!(pl.gray_sources(shared[0].ip, shared[0].port), 0);
        assert_eq!(pl.gray_multi_source().len(), 19);
    }

    #[test]
    fn trim_keeps_newest() {
        let mut pl = PeerList::new(false);
        pl.gray_limit = 2;
        for i in 1..=4_u8 {
            pl.append_gray(e([9, 9, 9, i], 1, u64::from(i)));
        }
        let mut seen: Vec<u64> = pl.gray.iter().map(|p| p.last_seen).collect();
        seen.sort_unstable();
        assert_eq!(seen, vec![3, 4]);
        assert_eq!(pl.head(10).len(), 0);
    }
}
