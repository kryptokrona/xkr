//! Stratum constants, login parsing, job targets and the JSON lines the server writes
//! (`src/stratum_server.cpp`: constants, `get_custom_user`, `get_custom_diff`, `on_login`,
//! `on_submit`, `on_blobs_ready`, `on_after_share_found`).
//!
//! Targets are 64-bit: a hash meets a target when its last 8 bytes, read little-endian, are
//! below it. The text of a target is its 8 little-endian bytes as hex, or only the high 4 bytes
//! when the target is at least [`TARGET_4_BYTES_LIMIT`] (difficulty up to 4 million), which is
//! the 4-byte compact target of `xkr_stratum::target` (`0xFFFFFFFF / difficulty`).

use core::fmt::Write as _;

use xkr_types::Hash;

use crate::difficulty::Difficulty;

/// `MIN_DIFF`: the least share difficulty a miner gets.
pub const MIN_DIFF: u64 = 1000;
/// `MAX_TARGET`: the target of [`MIN_DIFF`] (`u64::MAX / MIN_DIFF + 1`).
pub const MAX_TARGET: u64 = u64::MAX / MIN_DIFF + 1;
/// `AUTO_DIFF_TARGET_TIME`: seconds between shares autodiff aims at.
pub const AUTO_DIFF_TARGET_TIME: u64 = 30;
/// `TARGET_4_BYTES_LIMIT`: targets from here on (difficulty up to 4 million) are sent as 4 bytes.
pub const TARGET_4_BYTES_LIMIT: u64 = u64::MAX / 4_000_001;
/// `AUTODIFF_START`: the target of a new miner without shares (difficulty 500 000).
pub const AUTODIFF_START: u64 = u64::MAX / 500_001;
/// `BAD_SHARE_POINTS`: score of a low-difficulty share or one with a wrong result.
pub const BAD_SHARE_POINTS: i32 = -5;
/// `GOOD_SHARE_POINTS`: score of a share whose proof of work was checked.
pub const GOOD_SHARE_POINTS: i32 = 1;
/// `BAN_THRESHOLD_POINTS`: a bad share that brings the score to this or lower bans the miner.
pub const BAN_THRESHOLD_POINTS: i32 = -15;
/// `MAX_SCORE`.
pub const MAX_SCORE: i32 = 1000;
/// `StratumClient::JOBS_SIZE`: jobs remembered per connection.
pub const JOBS_SIZE: usize = 4;
/// `StratumClient::CUSTOM_USER_SIZE`: the user name holds one byte less.
pub const CUSTOM_USER_SIZE: usize = 32;
/// The `algo` of every job.
pub const ALGO: &str = "cryptonight_turtle";

/// The answer to an HTTP `GET` (`StratumClient::send_http_response`); a `HEAD` gets the same
/// without the last 21 bytes.
pub const HTTP_RESPONSE: &str = "HTTP/1.1 200 OK\r\nContent-Length: 21\r\nContent-Type: text/plain\r\nConnection: Closed\r\n\r\nP2Pool Stratum online";

/// `get_custom_user`: the login up to the first `+` or `.`, printable ASCII without `,`, `"`
/// and `\`, at most 31 characters; `None` when nothing is left.
#[must_use]
pub fn custom_user(login: &str) -> Option<String> {
    let mut user = String::new();
    for c in login.bytes() {
        if user.len() >= CUSTOM_USER_SIZE - 1 || c == 0 || c == b'+' || c == b'.' {
            break;
        }
        if (b' '..=b'~').contains(&c) && c != b',' && c != b'"' && c != b'\\' {
            user.push(char::from(c));
        }
    }
    (!user.is_empty()).then_some(user)
}

/// `get_custom_diff`: the number after the last `+` or `.` (`strtoull`), stored as `n + 1` and
/// at least [`MIN_DIFF`]; `None` when there is none or it is 0.
#[must_use]
pub fn custom_diff(login: &str) -> Option<Difficulty> {
    let bytes = login.as_bytes();
    let end = bytes.iter().position(|&c| c == 0).unwrap_or(bytes.len());
    let pos = bytes[..end].iter().rposition(|&c| c == b'+' || c == b'.')?;
    let t = strtoull(&bytes[pos + 1..end]);
    (t != 0).then(|| Difficulty::from_u64(t.wrapping_add(1).max(MIN_DIFF)))
}

/// C `strtoull(s, nullptr, 10)`: leading white space, an optional sign, decimal digits; past
/// `u64::MAX` it gives `u64::MAX`, a `-` negates (wrapping).
fn strtoull(s: &[u8]) -> u64 {
    let mut i = 0;
    while i < s.len() && matches!(s[i], b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r') {
        i += 1;
    }
    let mut negative = false;
    if i < s.len() && (s[i] == b'+' || s[i] == b'-') {
        negative = s[i] == b'-';
        i += 1;
    }
    let mut v = 0u64;
    let mut overflow = false;
    while i < s.len() && s[i].is_ascii_digit() {
        match v
            .checked_mul(10)
            .and_then(|x| x.checked_add(u64::from(s[i] - b'0')))
        {
            Some(x) => v = x,
            None => overflow = true,
        }
        i += 1;
    }
    if overflow {
        u64::MAX
    } else if negative {
        v.wrapping_neg()
    } else {
        v
    }
}

/// The target a template allows: the easier of the main-chain and sidechain targets.
#[must_use]
pub fn template_target(mainchain: Difficulty, sidechain: Difficulty) -> u64 {
    mainchain.target().max(sidechain.target())
}

/// The target text of a job: 8 little-endian bytes as hex, or the high 4 of them from
/// [`TARGET_4_BYTES_LIMIT`] on.
#[must_use]
pub fn target_hex(target: u64) -> String {
    let b = target.to_le_bytes();
    if target >= TARGET_4_BYTES_LIMIT {
        hex::encode(&b[4..])
    } else {
        hex::encode(b)
    }
}

/// The target a share is checked against: a 4-byte target is first rounded the way miners read
/// it (`u64::MAX / (0xFFFFFFFF / (target >> 32))`).
#[must_use]
pub fn share_target(target: u64) -> u64 {
    if target >= TARGET_4_BYTES_LIMIT {
        let d = (u64::from(u32::MAX) / (target >> 32).max(1)).max(1);
        u64::MAX / d
    } else {
        target
    }
}

/// The hashes a share at `target` stands for (`2^64 / target`, 1 for targets up to 1).
#[must_use]
pub fn hashes_for_target(target: u64) -> u64 {
    if target > 1 {
        u64::try_from((1u128 << 64) / u128::from(target)).unwrap_or(u64::MAX)
    } else {
        1
    }
}

/// The difficulty of a target, rounded up (`show_workers`, the API's worker list); `fallback`
/// for targets up to 1.
#[must_use]
pub fn target_difficulty(target: u64, fallback: Difficulty) -> Difficulty {
    if target > 1 {
        let n = 1u128 << 64;
        let t = u128::from(target);
        Difficulty::from_u64(u64::try_from(n.div_ceil(t)).unwrap_or(u64::MAX))
    } else {
        fallback
    }
}

/// The last 8 bytes of a hash, little-endian: what is compared with a target.
#[must_use]
pub fn hash_tail(hash: &Hash) -> u64 {
    let mut w = [0u8; 8];
    w.copy_from_slice(&hash.0[24..]);
    u64::from_le_bytes(w)
}

/// A submitted `job_id`: hex digits (either case) into a `u32`, shifting out the high ones;
/// `None` for another character or a zero id.
#[must_use]
pub fn parse_job_id(s: &str) -> Option<u32> {
    let mut id = 0u32;
    for c in s.chars() {
        id = (id << 4) | c.to_digit(16)?;
    }
    (id != 0).then_some(id)
}

/// A submitted `nonce`: 8 hex characters, the nonce bytes in blob order (a little-endian u32).
#[must_use]
pub fn parse_nonce(s: &str) -> Option<u32> {
    if s.len() != 8 {
        return None;
    }
    let b: [u8; 4] = hex::decode(s).ok()?.try_into().ok()?;
    Some(u32::from_le_bytes(b))
}

/// A submitted `result`: 64 hex characters.
#[must_use]
pub fn parse_result(s: &str) -> Option<Hash> {
    if s.len() != 64 {
        return None;
    }
    let b: [u8; 32] = hex::decode(s).ok()?.try_into().ok()?;
    Some(Hash(b))
}

/// The fields of one job as the server sends them.
#[derive(Clone, Copy, Debug)]
pub struct JobFields<'a> {
    /// Hashing blob.
    pub blob: &'a [u8],
    /// Per-connection job id.
    pub job_id: u32,
    /// The target.
    pub target: u64,
    /// Main-chain height.
    pub height: u64,
    /// Seed hash.
    pub seed_hash: &'a Hash,
}

impl JobFields<'_> {
    /// The job object (`{"blob":...,"seed_hash":...}`) in the C++ field order.
    #[must_use]
    pub fn object(&self) -> String {
        let mut s = String::with_capacity(self.blob.len() * 2 + 200);
        s.push_str("{\"blob\":\"");
        s.push_str(&hex::encode(self.blob));
        let _ = write!(
            s,
            "\",\"job_id\":\"{:x}\",\"target\":\"{}\",\"algo\":\"{ALGO}\",\"height\":{},\"seed_hash\":\"{}\"}}",
            self.job_id,
            target_hex(self.target),
            self.height,
            self.seed_hash
        );
        s
    }
}

/// The answer to a `login` (`on_login`), with its `\n`.
#[must_use]
pub fn login_line(id: u32, rpc_id: u32, job: &JobFields<'_>) -> String {
    format!(
        "{{\"id\":{id},\"jsonrpc\":\"2.0\",\"result\":{{\"id\":\"{rpc_id:x}\",\"job\":{},\"extensions\":[\"algo\"],\"status\":\"OK\"}}}}\n",
        job.object()
    )
}

/// A pushed `job` notification (`on_blobs_ready`), with its `\n`.
#[must_use]
pub fn job_line(job: &JobFields<'_>) -> String {
    format!(
        "{{\"jsonrpc\":\"2.0\",\"method\":\"job\",\"params\":{}}}\n",
        job.object()
    )
}

/// The answer to a `getjob`: the job object as the result, with its `\n`.
#[must_use]
pub fn getjob_line(id: u32, job: &JobFields<'_>) -> String {
    format!(
        "{{\"id\":{id},\"jsonrpc\":\"2.0\",\"error\":null,\"result\":{}}}\n",
        job.object()
    )
}

/// A successful answer with a status (`OK` for a share, `KEEPALIVED`), with its `\n`.
#[must_use]
pub fn status_line(id: u32, status: &str) -> String {
    format!(
        "{{\"id\":{id},\"jsonrpc\":\"2.0\",\"error\":null,\"result\":{{\"status\":\"{status}\"}}}}\n"
    )
}

/// An error answer, with its `\n`. The error object carries the code `-1` next to the C++
/// message so typed clients (`xkr_stratum::messages::RpcError`) read it too.
#[must_use]
pub fn error_line(id: u32, message: &str) -> String {
    format!(
        "{{\"id\":{id},\"jsonrpc\":\"2.0\",\"error\":{{\"code\":-1,\"message\":\"{message}\"}}}}\n"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constants() {
        assert_eq!(MAX_TARGET, 18_446_744_073_709_552);
        assert_eq!(MAX_TARGET, Difficulty::from_u64(MIN_DIFF).target());
        assert_eq!(
            u128::from(TARGET_4_BYTES_LIMIT),
            u128::from(u64::MAX) / 4_000_001
        );
        assert_eq!(u128::from(AUTODIFF_START), u128::from(u64::MAX) / 500_001);
        const { assert!(AUTODIFF_START > TARGET_4_BYTES_LIMIT) };
        const { assert!(MAX_TARGET > AUTODIFF_START) };
    }

    #[test]
    fn numbers_like_strtoull() {
        assert_eq!(strtoull(b"1000"), 1000);
        assert_eq!(strtoull(b"  42x"), 42);
        assert_eq!(strtoull(b"+7"), 7);
        assert_eq!(strtoull(b"-1"), u64::MAX);
        assert_eq!(strtoull(b"rig1"), 0);
        assert_eq!(strtoull(b""), 0);
        assert_eq!(strtoull(b"99999999999999999999999"), u64::MAX);
    }
}
