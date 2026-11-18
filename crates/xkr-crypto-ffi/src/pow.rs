//! `findPowShare`: the Hugin message-share search of the two native modules
//! (`TurtleCoinModule.mm` / `Java.cpp` `findPowShare`), one implementation for both.
//!
//! The blob is a block hashing blob; the 4-byte nonce sits after the three header varints and
//! the 32-byte previous block id (offset 39 if the header does not parse). The 4-byte compact
//! target `raw` (8 hex characters, little-endian) means `u64::MAX / (0xFFFFFFFF / raw)`; a
//! share is a nonce whose `cn_turtle_lite_slow_hash_v2` has its last 8 bytes (little-endian)
//! `<=` that value. `threads` workers each try a contiguous slice of `ceil(attempts / threads)`
//! nonces starting at `startNonce + t * slice` (32-bit wrapping); the first worker that finds a
//! share stops the others.
//!
//! `nonceTagBits` / `nonceTagValue` are accepted by the JavaScript method and ignored, as both
//! native modules ignore them today.

use std::num::NonZero;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// Longest blob accepted, in hex characters (1024 bytes).
pub const MAX_BLOB_HEX_CHARS: usize = 2048;
/// Most nonces tried in one call.
pub const MAX_POW_ATTEMPTS: u64 = 5_000_000;
/// Most worker threads.
pub const MAX_THREADS: u32 = 8;
/// Nonce offset when the blob header does not parse.
pub const FALLBACK_NONCE_OFFSET: usize = 39;

/// A share that meets the target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PowShare {
    /// The 4 nonce bytes as they sit in the blob, 8 lowercase hex characters.
    pub nonce: String,
    /// The hash of the blob with that nonce, 64 lowercase hex characters.
    pub result: String,
}

/// The outcome of a search (`{ job_id: '', nonce, result, hashes_performed, threads }` when a
/// share was found, `{ hashes_performed, threads }` when not).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PowSearch {
    /// The share, if one was found.
    pub share: Option<PowShare>,
    /// Hashes computed by all workers together.
    pub hashes_performed: u64,
    /// Number of worker threads used.
    pub threads: u32,
}

/// The worker count when the caller passes 0: 80 % of the available cores (at least 1, at most
/// [`MAX_THREADS`]), the rule of the iOS module, which leaves cores for the UI.
#[must_use]
pub fn default_threads() -> u32 {
    let cores = std::thread::available_parallelism().map_or(1, NonZero::get);
    let cores = u32::try_from(cores).unwrap_or(u32::MAX);
    (cores.saturating_mul(4) / 5).clamp(1, MAX_THREADS)
}

/// hugin-utils `getNonceOffsetFromBuffer` as the native modules implement it: three varints
/// (at most 10 bytes each), then the 32-byte previous block id.
#[must_use]
pub fn nonce_offset(blob: &[u8]) -> usize {
    let mut offset = 0usize;
    for _ in 0..3 {
        let mut shift = 0u32;
        loop {
            let Some(byte) = blob.get(offset) else {
                return FALLBACK_NONCE_OFFSET;
            };
            offset += 1;
            if byte & 0x80 == 0 {
                break;
            }
            shift += 7;
            if shift > 63 {
                return FALLBACK_NONCE_OFFSET;
            }
        }
    }
    offset + 32
}

/// The 64-bit comparison value of a 4-byte compact target, `None` if the target is not 8 hex
/// characters or is unusable (`raw == 0`).
#[must_use]
pub fn target_value(target_hex: &str) -> Option<u64> {
    if target_hex.len() != 8 {
        return None;
    }
    let mut raw = [0u8; 4];
    hex::decode_to_slice(target_hex, &mut raw).ok()?;
    let raw = u64::from(u32::from_le_bytes(raw));
    if raw == 0 {
        return None;
    }
    let denom = 0xFFFF_FFFFu64 / raw;
    (denom != 0).then(|| u64::MAX / denom)
}

/// Searches for a share. `None` where the native modules resolve `null` for unusable input:
/// an empty or too long blob, a blob that is not hex, a blob too short for the nonce, or an
/// unusable target. `max_attempts` is clamped to `1..=MAX_POW_ATTEMPTS`; `threads == 0` means
/// [`default_threads`], other values are clamped to `1..=MAX_THREADS`.
#[must_use]
pub fn find_pow_share(
    blob_hex: &str,
    target_hex: &str,
    start_nonce: u32,
    max_attempts: u64,
    threads: u32,
) -> Option<PowSearch> {
    if blob_hex.is_empty() || blob_hex.len() > MAX_BLOB_HEX_CHARS {
        return None;
    }
    let blob = hex::decode(blob_hex).ok()?;
    let offset = nonce_offset(&blob);
    if offset + 4 > blob.len() {
        return None;
    }
    let target = target_value(target_hex)?;
    let attempts = max_attempts.clamp(1, MAX_POW_ATTEMPTS);
    let threads = if threads == 0 {
        default_threads()
    } else {
        threads.min(MAX_THREADS)
    };
    let per_thread = attempts.div_ceil(u64::from(threads));

    let found = AtomicBool::new(false);
    let total = AtomicU64::new(0);
    let winner: Mutex<Option<PowShare>> = Mutex::new(None);
    std::thread::scope(|scope| {
        for t in 0..threads {
            let (blob, found, total, winner) = (&blob, &found, &total, &winner);
            scope.spawn(move || {
                let first = start_nonce.wrapping_add((u64::from(t) * per_thread) as u32);
                let hashes = search_slice(blob, offset, target, first, per_thread, found, winner);
                total.fetch_add(hashes, Ordering::Relaxed);
            });
        }
    });
    let share = winner
        .into_inner()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    Some(PowSearch {
        share,
        hashes_performed: total.into_inner(),
        threads,
    })
}

/// One worker: tries `count` nonces from `first`, returns the number of hashes computed.
fn search_slice(
    blob: &[u8],
    offset: usize,
    target: u64,
    first: u32,
    count: u64,
    found: &AtomicBool,
    winner: &Mutex<Option<PowShare>>,
) -> u64 {
    let mut local = blob.to_vec();
    let mut nonce = first;
    let mut hashes = 0u64;
    for _ in 0..count {
        if found.load(Ordering::Relaxed) {
            break;
        }
        hashes += 1;
        local[offset..offset + 4].copy_from_slice(&nonce.to_le_bytes());
        let hash = xkr_crypto::cn_turtle_lite_slow_hash_v2_reused(&local);
        let mut tail = [0u8; 8];
        tail.copy_from_slice(&hash.0[24..]);
        if u64::from_le_bytes(tail) <= target {
            if found
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                let share = PowShare {
                    nonce: hex::encode(nonce.to_le_bytes()),
                    result: hash.to_hex(),
                };
                *winner
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(share);
            }
            break;
        }
        nonce = nonce.wrapping_add(1);
    }
    hashes
}

/// The `startNonce` of the JavaScript call as the native modules convert it (a double to a
/// 64-bit integer, then the low 32 bits).
#[must_use]
pub fn start_nonce_from_js(value: f64) -> u32 {
    (value as i64) as u32
}

/// The `maxAttempts` of the JavaScript call (a double to a 64-bit integer; `<= 0` becomes 1
/// later through the clamp of [`find_pow_share`]).
#[must_use]
pub fn max_attempts_from_js(value: f64) -> u64 {
    u64::try_from(value as i64).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nonce_offset_follows_the_header() {
        // major 1, minor 0, timestamp 2 bytes, prev id 32 bytes -> 4 + 32
        let mut blob = vec![1u8, 0, 0x80, 0x01];
        blob.extend_from_slice(&[0u8; 40]);
        assert_eq!(nonce_offset(&blob), 36);
        assert_eq!(nonce_offset(&[0x80, 0x80]), FALLBACK_NONCE_OFFSET);
        assert_eq!(nonce_offset(&[0xff; 12]), FALLBACK_NONCE_OFFSET);
    }

    #[test]
    fn targets() {
        assert_eq!(target_value("ffffffff"), Some(u64::MAX));
        assert_eq!(target_value("00000000"), None);
        assert_eq!(target_value("fffffff"), None);
        assert_eq!(target_value("zzzzzzzz"), None);
        // raw = 0x7fffffff -> denom 2
        assert_eq!(target_value("ffffff7f"), Some(u64::MAX / 2));
    }

    #[test]
    fn js_number_conversion() {
        assert_eq!(start_nonce_from_js(5.0), 5);
        assert_eq!(start_nonce_from_js(4_294_967_295.0), u32::MAX);
        assert_eq!(start_nonce_from_js(4_294_967_296.0), 0);
        assert_eq!(start_nonce_from_js(-1.0), u32::MAX);
        assert_eq!(start_nonce_from_js(f64::NAN), 0);
        assert_eq!(max_attempts_from_js(-3.0), 0);
        assert_eq!(max_attempts_from_js(10.7), 10);
    }

    #[test]
    fn default_threads_is_in_range() {
        let t = default_threads();
        assert!((1..=MAX_THREADS).contains(&t));
    }
}
