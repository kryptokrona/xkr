//! The message pool: hugin-node's in-memory `Map(hash -> message)` with its 24 h lifetime,
//! bounded in count and bytes, plus an optional append-only file so a restart keeps the pool.
//!
//! hugin-node keeps every accepted DM for 24 hours (its `cleaner` runs every 10 minutes and drops
//! messages whose own `timestamp` is a day old) and answers
//! `{ request: true, type: 'some', timestamp }` with the messages newer than `timestamp`. It
//! saves the pool to `RocksDB` (`hyperdb`) only on SIGINT/SIGTERM. We append each message to
//! `<data-dir>/hugin/messages.jsonl` as it arrives and compact the file on start and when it
//! has grown to twice the live set.
//!
//! Two views are kept per message, both as serialized JSON: the full message (with `pow.job`,
//! so xkrd peers can re-verify it after a sync) and the client view without `pow.job` (what
//! hugin-node stores and serves). The byte bound counts both views plus a fixed per-message
//! overhead ([`ENTRY_OVERHEAD`]).

use std::collections::{BTreeSet, HashMap};
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::ops::Bound;
use std::path::{Path, PathBuf};

use bytes::Bytes;
use serde_json::Value;

/// hugin-node `ONE_DAY`.
pub const ONE_DAY_MS: f64 = 24.0 * 60.0 * 60.0 * 1000.0;
/// File name of the persisted pool inside the Hugin data directory.
pub const POOL_FILE: &str = "messages.jsonl";
/// Bytes counted per message on top of its two serialized views and its hash (map and index
/// entries, allocation headers).
pub const ENTRY_OVERHEAD: usize = 256;

/// Default of [`StoreLimits::max_bytes`]: 128 MiB, small enough for a relay that runs in
/// every node.
pub const DEFAULT_MAX_BYTES: usize = 128 * 1024 * 1024;

/// Bounds of the pool.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StoreLimits {
    /// Most messages kept (oldest evicted first).
    pub max_messages: usize,
    /// Most bytes kept (oldest evicted first): both serialized views of every message, its hash
    /// twice and [`ENTRY_OVERHEAD`].
    pub max_bytes: usize,
    /// Lifetime by message timestamp (hugin-node: 24 h).
    pub ttl_ms: f64,
}

impl Default for StoreLimits {
    fn default() -> Self {
        Self {
            max_messages: 100_000,
            max_bytes: DEFAULT_MAX_BYTES,
            ttl_ms: ONE_DAY_MS,
        }
    }
}

/// One pooled message.
#[derive(Debug, Clone)]
pub struct StoredMessage {
    pub hash: String,
    /// The message's own `timestamp` (ms).
    pub timestamp: f64,
    /// The message as inserted, including `pow.job` (serialized JSON).
    pub full: Bytes,
    /// What clients get: without `pow.job` (hugin-node strips it before storing).
    pub client: Bytes,
    size: usize,
}

/// Outcome of [`MessagePool::insert`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Insert {
    Added,
    /// Already pooled (dedupe by `hash`).
    Duplicate,
    /// No string `hash`, or larger than the whole byte budget.
    Invalid,
    /// Older than the pool lifetime.
    Expired,
}

/// hugin-node's `{ ...rest, pow: { ...pow, job: undefined } }`.
#[must_use]
pub fn client_view(message: &Value) -> Value {
    let mut out = message.clone();
    if let Some(pow) = out.get_mut("pow").and_then(Value::as_object_mut) {
        pow.remove("job");
    }
    out
}

/// Order key: whole milliseconds, then hash.
type TimeKey = (i64, String);

#[derive(Debug)]
struct Persist {
    path: PathBuf,
    file: File,
    lines: usize,
}

/// The pool. Not synchronized; the relay keeps it behind a mutex.
#[derive(Debug)]
pub struct MessagePool {
    limits: StoreLimits,
    by_hash: HashMap<String, StoredMessage>,
    by_time: BTreeSet<TimeKey>,
    bytes: usize,
    persist: Option<Persist>,
}

impl MessagePool {
    /// An in-memory pool.
    #[must_use]
    pub fn new(limits: StoreLimits) -> Self {
        Self {
            limits,
            by_hash: HashMap::new(),
            by_time: BTreeSet::new(),
            bytes: 0,
            persist: None,
        }
    }

    /// A pool persisted in `dir/messages.jsonl`: loads what is still within the lifetime,
    /// rewrites the file with exactly that, and appends from then on.
    pub fn open(dir: &Path, limits: StoreLimits, now_ms: f64) -> io::Result<Self> {
        fs::create_dir_all(dir)?;
        let path = dir.join(POOL_FILE);
        let mut pool = Self::new(limits);
        match File::open(&path) {
            Ok(f) => {
                let mut bad = 0usize;
                for line in BufReader::new(f).lines() {
                    let line = line?;
                    if line.trim().is_empty() {
                        continue;
                    }
                    if let Ok(v) = serde_json::from_str::<Value>(&line) {
                        pool.insert(v, now_ms);
                    } else {
                        bad += 1;
                    }
                }
                if bad > 0 {
                    tracing::warn!(path = %path.display(), bad, "skipped unreadable lines in the Hugin pool file");
                }
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        let file = pool.rewrite(&path)?;
        pool.persist = Some(Persist {
            path,
            file,
            lines: pool.len(),
        });
        Ok(pool)
    }

    /// Writes the live set to `path` (via a temporary file) and returns it opened for appending.
    fn rewrite(&self, path: &Path) -> io::Result<File> {
        let tmp = path.with_extension("jsonl.tmp");
        {
            let mut out = io::BufWriter::new(File::create(&tmp)?);
            for (_, hash) in &self.by_time {
                if let Some(m) = self.by_hash.get(hash) {
                    out.write_all(&m.full)?;
                    out.write_all(b"\n")?;
                }
            }
            out.flush()?;
        }
        fs::rename(&tmp, path)?;
        OpenOptions::new().append(true).open(path)
    }

    fn append(&mut self, line: &[u8]) {
        let live_count = self.by_hash.len();
        let Some(p) = self.persist.as_mut() else {
            return;
        };
        let mut buf = Vec::with_capacity(line.len() + 1);
        buf.extend_from_slice(line);
        buf.push(b'\n');
        if let Err(e) = p.file.write_all(&buf) {
            tracing::warn!(path = %p.path.display(), error = %e, "cannot append to the Hugin pool file");
            return;
        }
        p.lines += 1;
        if p.lines > 2 * live_count + 1024 {
            let path = p.path.clone();
            match self.rewrite(&path) {
                Ok(file) => {
                    if let Some(p) = self.persist.as_mut() {
                        p.file = file;
                        p.lines = live_count;
                    }
                }
                Err(e) => {
                    tracing::warn!(path = %path.display(), error = %e, "cannot compact the Hugin pool file");
                }
            }
        }
    }

    #[must_use]
    pub fn limits(&self) -> StoreLimits {
        self.limits
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.by_hash.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.by_hash.is_empty()
    }

    /// Bytes of all pooled messages as counted against [`StoreLimits::max_bytes`].
    #[must_use]
    pub fn bytes(&self) -> usize {
        self.bytes
    }

    #[must_use]
    pub fn contains(&self, hash: &str) -> bool {
        self.by_hash.contains_key(hash)
    }

    #[must_use]
    pub fn get(&self, hash: &str) -> Option<&StoredMessage> {
        self.by_hash.get(hash)
    }

    /// hugin-node `add` minus verification: dedupe by `hash`, store, evict the oldest beyond the
    /// bounds. A message without a numeric `timestamp` gets `now_ms` (as hugin-node does).
    pub fn insert(&mut self, mut message: Value, now_ms: f64) -> Insert {
        let Some(hash) = message
            .get("hash")
            .and_then(Value::as_str)
            .map(str::to_owned)
        else {
            return Insert::Invalid;
        };
        if self.by_hash.contains_key(&hash) {
            return Insert::Duplicate;
        }
        let timestamp = if let Some(t) = message.get("timestamp").and_then(Value::as_f64) {
            t
        } else {
            if let Some(obj) = message.as_object_mut() {
                obj.insert("timestamp".into(), Value::from(now_ms as i64));
            }
            now_ms.floor()
        };
        if timestamp <= now_ms - self.limits.ttl_ms {
            return Insert::Expired;
        }
        let Ok(full) = serde_json::to_vec(&message) else {
            return Insert::Invalid;
        };
        let Ok(client) = serde_json::to_vec(&client_view(&message)) else {
            return Insert::Invalid;
        };
        drop(message);
        let size = full.len() + client.len() + 2 * hash.len() + ENTRY_OVERHEAD;
        if size > self.limits.max_bytes {
            return Insert::Invalid;
        }
        let full = Bytes::from(full);
        let stored = StoredMessage {
            hash: hash.clone(),
            timestamp,
            full: full.clone(),
            client: Bytes::from(client),
            size,
        };
        self.by_time.insert((timestamp as i64, hash.clone()));
        self.by_hash.insert(hash, stored);
        self.bytes += size;
        while self.by_hash.len() > self.limits.max_messages || self.bytes > self.limits.max_bytes {
            if !self.pop_oldest() {
                break;
            }
        }
        self.append(&full);
        Insert::Added
    }

    fn pop_oldest(&mut self) -> bool {
        let Some(key) = self.by_time.pop_first() else {
            return false;
        };
        if let Some(m) = self.by_hash.remove(&key.1) {
            self.bytes -= m.size;
        }
        true
    }

    /// hugin-node `cleaner`: drops messages whose timestamp is at least `ttl` old. Returns how
    /// many were dropped.
    ///
    /// The time key holds whole milliseconds (truncated) and then the hash, so within one
    /// millisecond the order is by hash, not by the exact timestamp: the scan goes on past live
    /// entries until a key whose every possible timestamp is newer than the limit (a bucket
    /// `ms` holds timestamps above `ms - 1`), instead of stopping at the first live entry.
    #[allow(clippy::cast_precision_loss)] // millisecond timestamps are far below 2^53
    pub fn evict_expired(&mut self, now_ms: f64) -> usize {
        let limit = now_ms - self.limits.ttl_ms;
        let mut doomed: Vec<TimeKey> = Vec::new();
        for key in &self.by_time {
            if (key.0 as f64) - 1.0 >= limit {
                break;
            }
            if self
                .by_hash
                .get(&key.1)
                .is_none_or(|m| m.timestamp <= limit)
            {
                doomed.push(key.clone());
            }
        }
        for key in &doomed {
            self.by_time.remove(key);
            if let Some(m) = self.by_hash.remove(&key.1) {
                self.bytes -= m.size;
            }
        }
        doomed.len()
    }

    fn after(&self, since: f64) -> impl DoubleEndedIterator<Item = &StoredMessage> {
        let start = (since.floor() as i64, String::new());
        self.by_time
            .range((Bound::Included(start), Bound::Unbounded))
            .filter_map(|(_, h)| self.by_hash.get(h))
            .filter(move |m| m.timestamp > since)
    }

    /// Client views (serialized) of the newest messages with `timestamp > since`, at most `max`,
    /// returned oldest first (hugin-node `onrequest` type `some`, `MAX_SYNC_MESSAGES` = 500).
    #[must_use]
    pub fn since(&self, since: f64, max: usize) -> Vec<Bytes> {
        self.since_capped(since, max, usize::MAX)
    }

    /// [`Self::since`] that also stops once the client views add up to `max_bytes` (the newest
    /// message is always included).
    #[must_use]
    pub fn since_capped(&self, since: f64, max: usize, max_bytes: usize) -> Vec<Bytes> {
        self.since_capped_dated(since, max, max_bytes)
            .into_iter()
            .map(|(_, m)| m)
            .collect()
    }

    /// [`Self::since_capped`] with each message's timestamp.
    #[must_use]
    pub fn since_capped_dated(
        &self,
        since: f64,
        max: usize,
        max_bytes: usize,
    ) -> Vec<(f64, Bytes)> {
        let mut out = Vec::new();
        let mut bytes = 0usize;
        for m in self.after(since).rev().take(max) {
            bytes = bytes.saturating_add(m.client.len());
            if !out.is_empty() && bytes > max_bytes {
                break;
            }
            out.push((m.timestamp, m.client.clone()));
        }
        out.reverse();
        out
    }

    /// Full messages (serialized, with `pow.job`) with `timestamp > since`, oldest first, at
    /// most `max`.
    #[must_use]
    pub fn since_full(&self, since: f64, max: usize) -> Vec<(f64, Bytes)> {
        self.after(since)
            .take(max)
            .map(|m| (m.timestamp, m.full.clone()))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    const NOW: f64 = 1_731_500_000_000.0;

    fn msg(hash: &str, ts: f64) -> Value {
        json!({
            "hash": hash,
            "timestamp": ts,
            "cipher": "ab",
            "pow": { "version": 2, "job": { "job_id": "j" }, "shares": [] },
        })
    }

    #[test]
    fn dedupe_and_client_view() {
        let mut p = MessagePool::new(StoreLimits::default());
        assert_eq!(p.insert(msg("a", NOW), NOW), Insert::Added);
        assert_eq!(p.insert(msg("a", NOW + 1.0), NOW), Insert::Duplicate);
        assert_eq!(p.insert(json!({ "timestamp": 1 }), NOW), Insert::Invalid);
        assert_eq!(p.len(), 1);
        let m = p.get("a").unwrap();
        let full: Value = serde_json::from_slice(&m.full).unwrap();
        let client: Value = serde_json::from_slice(&m.client).unwrap();
        assert!(full["pow"].get("job").is_some());
        assert!(client["pow"].get("job").is_none());
        assert_eq!(client["pow"]["version"], 2);
        assert_eq!(
            p.bytes(),
            m.full.len() + m.client.len() + 2 * "a".len() + ENTRY_OVERHEAD
        );
        // a missing timestamp becomes "now"
        assert_eq!(p.insert(json!({ "hash": "b" }), NOW), Insert::Added);
        assert_eq!(p.get("b").unwrap().timestamp as i64, NOW as i64);
    }

    #[test]
    fn ttl_eviction() {
        let mut p = MessagePool::new(StoreLimits::default());
        assert_eq!(p.insert(msg("old", NOW - ONE_DAY_MS), NOW), Insert::Expired);
        p.insert(msg("a", NOW - ONE_DAY_MS + 10.0), NOW);
        p.insert(msg("b", NOW - 5.0), NOW);
        p.insert(msg("c", NOW), NOW);
        assert_eq!(p.evict_expired(NOW), 0);
        assert_eq!(p.evict_expired(NOW + 10.0), 1);
        assert!(!p.contains("a"));
        assert_eq!(p.evict_expired(NOW + ONE_DAY_MS), 2);
        assert!(p.is_empty());
        assert_eq!(p.bytes(), 0);
    }

    /// Within one millisecond the time key orders by hash, so an expired fractional-millisecond
    /// message sorted behind a live one of the same millisecond used to survive the clean. Every
    /// expired message goes, every live one stays.
    #[test]
    fn eviction_is_exact_within_a_millisecond() {
        let mut p = MessagePool::new(StoreLimits::default());
        let base = NOW - ONE_DAY_MS + 100.0; // bucket of whole millisecond `base`
        // "a" sorts first but is live at the clean below; "b" and "c" are expired.
        p.insert(msg("a", base + 0.9), NOW);
        p.insert(msg("b", base + 0.2), NOW);
        p.insert(msg("c", base - 3.0), NOW);
        p.insert(msg("d", base + 2.0), NOW);
        let clean_at = NOW + 100.5; // limit = base + 0.5
        assert_eq!(p.evict_expired(clean_at), 2);
        assert!(p.contains("a"), "live message in the same millisecond kept");
        assert!(
            !p.contains("b"),
            "expired message behind a live one evicted"
        );
        assert!(!p.contains("c"));
        assert!(p.contains("d"));
        assert_eq!(p.evict_expired(clean_at), 0);
        assert!(p.bytes() > 0);
        assert_eq!(p.evict_expired(NOW + ONE_DAY_MS + 200.0), 2);
        assert_eq!(p.bytes(), 0);
    }

    #[test]
    fn bounds_evict_the_oldest() {
        let mut p = MessagePool::new(StoreLimits {
            max_messages: 3,
            ..StoreLimits::default()
        });
        for (d, h) in [(0.0, "a"), (1.0, "b"), (2.0, "c"), (3.0, "d")] {
            p.insert(msg(h, NOW + d), NOW);
        }
        assert_eq!(p.len(), 3);
        assert!(!p.contains("a"));
        let one = {
            let mut q = MessagePool::new(StoreLimits::default());
            q.insert(msg("x", NOW), NOW);
            q.bytes()
        };
        let mut p = MessagePool::new(StoreLimits {
            max_bytes: one * 2,
            ..StoreLimits::default()
        });
        for (d, h) in [(0.0, "x"), (1.0, "y"), (2.0, "z")] {
            p.insert(msg(h, NOW + d), NOW);
        }
        assert_eq!(p.len(), 2);
        assert!(!p.contains("x"));
        assert!(p.bytes() <= one * 2);
    }

    #[test]
    fn since_is_sorted_and_strict() {
        let mut p = MessagePool::new(StoreLimits::default());
        p.insert(msg("c", NOW + 2.0), NOW);
        p.insert(msg("a", NOW), NOW);
        p.insert(msg("b", NOW + 1.5), NOW);
        let hashes = |v: Vec<Bytes>| {
            v.iter()
                .map(|m| {
                    let m: Value = serde_json::from_slice(m).unwrap();
                    m["hash"].as_str().unwrap().to_owned()
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(hashes(p.since(0.0, 500)), ["a", "b", "c"]);
        assert_eq!(hashes(p.since(NOW, 500)), ["b", "c"]);
        assert_eq!(hashes(p.since(NOW + 1.5, 500)), ["c"]);
        // the newest `max`, oldest first
        assert_eq!(hashes(p.since(0.0, 2)), ["b", "c"]);
        assert_eq!(hashes(p.since(0.0, 1)), ["c"]);
        // a byte cap keeps the newest that fit, at least one
        let one = p.since(NOW + 1.5, 1)[0].len();
        assert_eq!(hashes(p.since_capped(0.0, 500, 2 * one)), ["b", "c"]);
        assert_eq!(hashes(p.since_capped(0.0, 500, 1)), ["c"]);
        assert_eq!(p.since_full(NOW, 10).len(), 2);
        let first: Value = serde_json::from_slice(&p.since_full(0.0, 1)[0].1).unwrap();
        assert_eq!(first["hash"], "a");
        assert!(first["pow"].get("job").is_some());
    }

    #[test]
    fn persistence_survives_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        {
            let mut p = MessagePool::open(dir.path(), StoreLimits::default(), NOW).unwrap();
            p.insert(msg("a", NOW - ONE_DAY_MS + 100.0), NOW);
            p.insert(msg("b", NOW), NOW);
            p.insert(msg("b", NOW), NOW);
        }
        let text = fs::read_to_string(dir.path().join(POOL_FILE)).unwrap();
        assert_eq!(text.lines().count(), 2);
        // a line of garbage is skipped
        fs::write(dir.path().join(POOL_FILE), format!("{text}not json\n")).unwrap();
        let p = MessagePool::open(dir.path(), StoreLimits::default(), NOW + 200.0).unwrap();
        assert_eq!(p.len(), 1, "the expired message is not reloaded");
        assert!(p.contains("b"));
        let full: Value = serde_json::from_slice(&p.get("b").unwrap().full).unwrap();
        assert!(full["pow"].get("job").is_some());
        let text = fs::read_to_string(dir.path().join(POOL_FILE)).unwrap();
        assert_eq!(text.lines().count(), 1, "compacted on open");
    }

    #[test]
    fn the_file_is_compacted_while_running() {
        let dir = tempfile::tempdir().unwrap();
        let mut p = MessagePool::open(
            dir.path(),
            StoreLimits {
                max_messages: 10,
                ..StoreLimits::default()
            },
            NOW,
        )
        .unwrap();
        for i in 0..1100 {
            p.insert(msg(&format!("h{i}"), NOW + f64::from(i)), NOW);
        }
        let lines = fs::read_to_string(dir.path().join(POOL_FILE))
            .unwrap()
            .lines()
            .count();
        assert!(lines < 1100, "{lines}");
        assert_eq!(p.len(), 10);
    }
}
