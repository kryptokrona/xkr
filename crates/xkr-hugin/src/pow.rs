//! Proof of work and message checks, ported from hugin-node `nodes.js` and hugin-utils.
//!
//! A Hugin message carries `pow = { version: 2, job, shares: [{ job_id, nonce, result }], auth }`:
//!
//! * `job` is a mining job of an XKR pool (`{ job_id, blob, target }`), handed to the client by
//!   the node (`job_request`). The client searches a nonce so that
//!   `cn_turtle_lite_slow_hash_v2(blob with nonce)` meets `target` (an XKR block-template
//!   share, the same algorithm as block PoW since major version 5).
//! * `auth = { pub, sig, nonce }` binds one share to the message: an ed25519 signature with a
//!   one-time key over `powsig:v2:<hash>:<timestamp>:<job_id>:<nonce>:<result>:<context>`.
//!
//! The node checks the shapes, the freshness of the job (`prev_id` among the last three pool
//! jobs), the signature, hashes the signed share itself, and submits it to its p2pool node. See
//! `docs/hugin-relay.md` §5 for the exact rules and where we are stricter than hugin-node.

use curve25519_dalek::edwards::{CompressedEdwardsY, EdwardsPoint};
use curve25519_dalek::scalar::Scalar;
use serde_json::Value;
use sha2::{Digest, Sha512};

// Blob and target helpers shared with the miner (`xkr-stratum`), under their old paths.
pub use xkr_stratum::blob::{
    DEFAULT_NONCE_OFFSET, extract_prev_id, insert_nonce, is_hex, nonce_offset,
};
pub use xkr_stratum::target::{
    meets_target, parse_target, share_difficulty, target_difficulty, target_for_difficulty,
};

/// The only `pow.version` hugin-node 2.0.0 accepts and relays.
pub const POW_VERSION: u64 = 2;
/// At most this many shares per message.
pub const MAX_SHARES_PER_MESSAGE: usize = 5;
/// Largest job blob (bytes) accepted inside a message.
pub const MAX_JOB_BLOB_BYTES: usize = 1024;
/// Longest `job_id` (JavaScript string length).
pub const MAX_JOB_ID_LEN: usize = 32;
/// Longest `cipher` (hex characters); equal to the client packet limit.
pub const MAX_CIPHER_LEN: usize = 9000;
/// Messages older than this (by their own timestamp) are refused from clients and gossip.
pub const MAX_MESSAGE_PAST_MS: f64 = 60.0 * 60.0 * 1000.0;
/// Messages dated further in the future are refused.
pub const MAX_MESSAGE_FUTURE_MS: f64 = 5.0 * 60.0 * 1000.0;
/// Accepted `kind` values (absent means `dm`).
pub const MESSAGE_KINDS: [&str; 3] = ["dm", "call", "room"];

/// JavaScript `string.length` (UTF-16 code units).
#[must_use]
pub fn js_len(s: &str) -> usize {
    s.encode_utf16().count()
}

/// JavaScript template-literal rendering (`${value}`) of a JSON value; `None` is `undefined`.
#[must_use]
pub fn js_display(v: Option<&Value>) -> String {
    match v {
        None => "undefined".into(),
        Some(Value::Null) => "null".into(),
        Some(Value::Bool(b)) => b.to_string(),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => js_number(n),
        Some(Value::Array(a)) => a
            .iter()
            .map(|x| match x {
                Value::Null => String::new(),
                other => js_display(Some(other)),
            })
            .collect::<Vec<_>>()
            .join(","),
        Some(Value::Object(_)) => "[object Object]".into(),
    }
}

fn js_number(n: &serde_json::Number) -> String {
    if n.is_u64() || n.is_i64() {
        return n.to_string();
    }
    let f = n.as_f64().unwrap_or(f64::NAN);
    if f.is_finite() && f.trunc().to_bits() == f.to_bits() && f.abs() < 1e21 {
        format!("{f:.0}")
    } else {
        f.to_string()
    }
}

/// A JSON number as `f64` (`typeof x === 'number'`).
#[must_use]
pub fn number(v: Option<&Value>) -> Option<f64> {
    v.and_then(Value::as_f64)
}

// ---------------------------------------------------------------------------------------------
// block blob helpers (hugin-utils)

/// A job as embedded in a message (`pow.job`), already shape-checked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PowJob {
    pub job_id: String,
    pub blob: Vec<u8>,
    pub target: String,
}

impl PowJob {
    /// hugin-utils `isValidPowJob` (plus hugin-node's blob size limit when `max_blob` is set).
    #[must_use]
    pub fn from_value(job: &Value, max_blob: Option<usize>) -> Option<Self> {
        let job_id = job.get("job_id")?.as_str()?;
        let blob = job.get("blob")?.as_str()?;
        let target = job.get("target")?.as_str()?;
        if js_len(job_id) > MAX_JOB_ID_LEN
            || !is_hex(blob)
            || blob.len() % 2 != 0
            || !is_hex(target)
            || target.len() != 8
        {
            return None;
        }
        if max_blob.is_some_and(|m| blob.len() / 2 > m) {
            return None;
        }
        Some(Self {
            job_id: job_id.to_owned(),
            blob: hex::decode(blob).ok()?,
            target: target.to_owned(),
        })
    }
}

/// hugin-utils `verifyShare(job, nonce, result)`: `result` must be the lowercase hex of
/// `cn_turtle_lite_slow_hash_v2(blob with nonce)` and meet the target. Costs one CN-Turtle hash
/// (run it on a blocking thread). Stricter than hugin-utils: a zero target is refused.
#[must_use]
pub fn verify_share(job: &PowJob, nonce: &str, result: &str) -> bool {
    if nonce.len() != 8 || !is_hex(nonce) || result.len() != 64 || !is_hex(result) {
        return false;
    }
    if parse_target(&job.target).is_none() {
        return false;
    }
    let Ok(nonce_bytes) = <[u8; 4]>::try_from(hex::decode(nonce).unwrap_or_default()) else {
        return false;
    };
    let Some(blob) = insert_nonce(&job.blob, &nonce_bytes) else {
        return false;
    };
    let hash = xkr_crypto::cn_turtle_lite_slow_hash_v2_reused(&blob);
    hex::encode(hash.0) == result && meets_target(&hash.0, &job.target)
}

/// Whether an already verified share result (64 hex characters, [`verify_share`]) also meets
/// `target` (4 or 8 bytes, [`parse_target`]). False for a malformed result or an unusable
/// target. The relay uses it to pick the app shares that are also p2pool shares.
#[must_use]
pub fn result_meets_target(result: &str, target: &str) -> bool {
    let Some(target) = parse_target(target) else {
        return false;
    };
    let Ok(hash) = <[u8; 32]>::try_from(hex::decode(result).unwrap_or_default()) else {
        return false;
    };
    xkr_stratum::target::meets_target_u64(&hash, target)
}

// ---------------------------------------------------------------------------------------------
// message shape (hugin-node `check`)

/// hugin-node `check(message)` with a configurable age limit (`max_past_ms`; hugin-node uses
/// [`MAX_MESSAGE_PAST_MS`]; our node-to-node sync allows 24 h).
#[must_use]
pub fn check_message(m: &Value, now_ms: f64, max_past_ms: f64) -> bool {
    let Some(obj) = m.as_object() else {
        return false;
    };
    let (Some(cipher), Some(hash), Some(ts)) = (
        obj.get("cipher").and_then(Value::as_str),
        obj.get("hash").and_then(Value::as_str),
        number(obj.get("timestamp")),
    ) else {
        return false;
    };
    // typeof pow === 'object' && pow && pow.job && Array.isArray(pow.shares)
    let Some(pow) = obj.get("pow").filter(|p| p.is_object() || p.is_array()) else {
        return false;
    };
    if !truthy(pow.get("job")) || !pow.get("shares").is_some_and(Value::is_array) {
        return false;
    }
    if pow.get("version").is_some_and(|v| !v.is_number()) {
        return false;
    }
    let Some(job) = pow.get("job").filter(|j| j.is_object() || j.is_array()) else {
        return false;
    };
    if PowJob::from_value(job, Some(MAX_JOB_BLOB_BYTES)).is_none() {
        return false;
    }
    if js_len(cipher) > MAX_CIPHER_LEN || !is_hex(cipher) {
        return false;
    }
    if let Some(kind) = obj.get("kind") {
        match kind.as_str() {
            Some(k) if MESSAGE_KINDS.contains(&k) => {}
            _ => return false,
        }
    }
    if js_len(hash) > 64 || !is_hex(hash) {
        return false;
    }
    let shares = pow
        .get("shares")
        .and_then(Value::as_array)
        .map_or(0, Vec::len);
    ts >= 0.0
        && shares <= MAX_SHARES_PER_MESSAGE
        && ts <= now_ms + MAX_MESSAGE_FUTURE_MS
        && ts >= now_ms - max_past_ms
}

/// JavaScript truthiness of an optional JSON value.
#[must_use]
pub fn truthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().is_some_and(|f| f.abs() > 0.0),
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(_) | Value::Object(_)) => true,
    }
}

/// `pow.version` as hugin-node reads it (a number, default 1).
#[must_use]
pub fn pow_version(pow: Option<&Value>) -> f64 {
    number(pow.and_then(|p| p.get("version"))).unwrap_or(1.0)
}

/// `pow.version === POW_VERSION` (2), with hugin-node's default of 1 for a missing version.
#[must_use]
pub fn pow_version_is_current(pow: Option<&Value>) -> bool {
    pow_version(pow).to_bits() == 2.0f64.to_bits()
}

/// The `kind` of a message (`dm` unless a string says otherwise).
#[must_use]
pub fn message_kind(m: &Value) -> &str {
    m.get("kind").and_then(Value::as_str).unwrap_or("dm")
}

// ---------------------------------------------------------------------------------------------
// job freshness (hugin-node `currentPrevId` / `previousPrevId` / `twoBackPrevId`)

/// `prev_id`s of the last three distinct pool templates.
///
/// hugin-node shifts on every job, so a pool that re-sends jobs for the same block narrows the
/// window to one block; we shift only when the `prev_id` changes (the evident intent).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PrevIds {
    pub current: Option<[u8; 32]>,
    pub previous: Option<[u8; 32]>,
    pub two_back: Option<[u8; 32]>,
}

impl PrevIds {
    pub fn push(&mut self, prev_id: [u8; 32]) {
        if self.current == Some(prev_id) {
            return;
        }
        self.two_back = self.previous;
        self.previous = self.current;
        self.current = Some(prev_id);
    }

    #[must_use]
    pub fn contains(&self, prev_id: &[u8; 32]) -> bool {
        [self.current, self.previous, self.two_back].contains(&Some(*prev_id))
    }
}

/// Result of [`validate_pow_payload`].
#[derive(Debug, Clone)]
pub struct ValidatedPow<'a> {
    pub job: PowJob,
    /// At most [`MAX_SHARES_PER_MESSAGE`].
    pub shares: &'a [Value],
    pub prev_id: [u8; 32],
}

/// hugin-node `validate_pow_payload(hash, pow, { requireFreshTemplate })`: `fresh` is `Some` to
/// require the job's `prev_id` among the pool's last three (client posts), `None` for gossip.
pub fn validate_pow_payload<'a>(
    hash: Option<&Value>,
    pow: &'a Value,
    fresh: Option<&PrevIds>,
) -> Result<ValidatedPow<'a>, &'static str> {
    let hash = hash.and_then(Value::as_str).unwrap_or_default();
    if hash.len() != 64 || !is_hex(hash) {
        return Err("invalid_hash");
    }
    let job = pow
        .get("job")
        .filter(|j| j.is_object())
        .ok_or("invalid_job")?;
    match job.get("job_id").and_then(Value::as_str) {
        Some(id) if js_len(id) <= MAX_JOB_ID_LEN => {}
        _ => return Err("invalid_job_id"),
    }
    let blob = job.get("blob").and_then(Value::as_str).unwrap_or_default();
    if !is_hex(blob) {
        return Err("invalid_blob");
    }
    if blob.len() % 2 != 0 {
        return Err("invalid_blob_len");
    }
    if blob.len() / 2 > MAX_JOB_BLOB_BYTES {
        return Err("blob_too_large");
    }
    match job.get("target").and_then(Value::as_str) {
        Some(t) if is_hex(t) && t.len() == 8 => {}
        _ => return Err("invalid_target"),
    }
    let shares = pow
        .get("shares")
        .and_then(Value::as_array)
        .map_or(&[][..], Vec::as_slice);
    if shares.is_empty() {
        return Err("no_shares");
    }
    if shares.len() > MAX_SHARES_PER_MESSAGE {
        return Err("too_many_shares");
    }
    let job = PowJob::from_value(job, Some(MAX_JOB_BLOB_BYTES)).ok_or("invalid_job")?;
    let prev_id = extract_prev_id(&job.blob).ok_or("no_prev_id")?;
    if let Some(state) = fresh {
        if state.current.is_none() {
            return Err("no_current_prev_id");
        }
        if !state.contains(&prev_id) {
            return Err("prev_id_mismatch");
        }
    }
    Ok(ValidatedPow {
        job,
        shares,
        prev_id,
    })
}

/// One share after hugin-node `validate_share_fast`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FastShare<'a> {
    pub job_id: &'a str,
    pub nonce: &'a str,
    pub result: &'a str,
}

impl FastShare<'_> {
    /// `job_id:nonce:result` in lowercase (hugin-node `share_cache_key`).
    #[must_use]
    pub fn key(&self) -> String {
        format!(
            "{}:{}:{}",
            self.job_id,
            self.nonce.to_lowercase(),
            self.result.to_lowercase()
        )
    }
}

/// hugin-node `validate_share_fast(share, jobId)`.
pub fn validate_share_fast<'a>(
    share: &'a Value,
    job_id: &str,
) -> Result<FastShare<'a>, &'static str> {
    let Some(id) = share
        .get("job_id")
        .and_then(Value::as_str)
        .filter(|id| *id == job_id)
    else {
        return Err("job_id_mismatch");
    };
    let nonce = share
        .get("nonce")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if nonce.len() != 8 || !is_hex(nonce) {
        return Err("bad_nonce");
    }
    let result = share
        .get("result")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if result.len() != 64 || !is_hex(result) {
        return Err("bad_result");
    }
    Ok(FastShare {
        job_id: id,
        nonce,
        result,
    })
}

// ---------------------------------------------------------------------------------------------
// pow.auth

/// hugin-node `build_pow_sig_payload` (the string the client signs).
#[must_use]
pub fn pow_sig_payload(hash: &str, timestamp: &str, share: &Value, context: &str) -> String {
    let field = |k: &str| share.get(k).and_then(Value::as_str).unwrap_or_default();
    format!(
        "powsig:v2:{hash}:{timestamp}:{}:{}:{}:{context}",
        field("job_id"),
        field("nonce").to_lowercase(),
        field("result").to_lowercase()
    )
}

/// Signature context of a message: `kind:cipher` (or just `cipher` without a kind).
#[must_use]
pub fn auth_context_message(m: &Value) -> String {
    let cipher = m.get("cipher").and_then(Value::as_str).unwrap_or_default();
    match m.get("kind").and_then(Value::as_str) {
        Some(kind) if !kind.is_empty() => format!("{kind}:{cipher}"),
        _ => cipher.to_owned(),
    }
}

/// Signature context of a `register` packet: its `data` string.
#[must_use]
pub fn auth_context_register(p: &Value) -> String {
    p.get("data")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

/// hugin-node `verify_pow_auth(hash, timestamp, pow, context)`.
pub fn verify_pow_auth(
    hash: Option<&Value>,
    timestamp: Option<&Value>,
    pow: &Value,
    context: &str,
) -> Result<(), &'static str> {
    if !(pow.is_object() || pow.is_array()) {
        return Err("missing_pow");
    }
    let Some(auth) = pow.get("auth").filter(|a| a.is_object() || a.is_array()) else {
        return Err("missing_pow_auth");
    };
    let lower = |k: &str| {
        auth.get(k)
            .and_then(Value::as_str)
            .map_or_default(str::to_lowercase)
    };
    let (public, sig) = (lower("pub"), lower("sig"));
    if !is_hex(&public) || !is_hex(&sig) || public.len() != 64 || sig.len() != 128 {
        return Err("invalid_pow_auth");
    }
    let share = auth_share(pow)?;
    let payload = pow_sig_payload(&js_display(hash), &js_display(timestamp), share, context);
    let public: [u8; 32] = hex::decode(&public)
        .ok()
        .and_then(|b| b.try_into().ok())
        .ok_or("invalid_pow_auth")?;
    let sig: [u8; 64] = hex::decode(&sig)
        .ok()
        .and_then(|b| b.try_into().ok())
        .ok_or("invalid_pow_auth")?;
    if ed25519_verify(&public, payload.as_bytes(), &sig) {
        Ok(())
    } else {
        Err("invalid_pow_signature")
    }
}

/// The share `pow.auth` signs: the one whose nonce equals `auth.nonce` (case-insensitive), or
/// `shares[0]` when `auth.nonce` is absent or empty (hugin-node `verify_pow_auth`).
pub fn auth_share(pow: &Value) -> Result<&Value, &'static str> {
    let shares = pow
        .get("shares")
        .and_then(Value::as_array)
        .map_or(&[][..], Vec::as_slice);
    let Some(first) = shares.first() else {
        return Err("no_shares");
    };
    let nonce = pow
        .get("auth")
        .and_then(|a| a.get("nonce"))
        .and_then(Value::as_str)
        .map_or_default(str::to_lowercase);
    if nonce.is_empty() {
        return Ok(first);
    }
    shares
        .iter()
        .find(|s| {
            s.get("nonce")
                .and_then(Value::as_str)
                .is_some_and(|n| n.to_lowercase() == nonce)
        })
        .ok_or("pow_auth_nonce_mismatch")
}

/// Largest stored or relayed message in its canonical form ([`canonical_message`]), in bytes of
/// JSON: the Hugin packet limit, which a client post (message plus wrapper) already has to fit.
pub const MAX_MESSAGE_BYTES: usize = 9000;

/// The message as the relay stores and forwards it: only the fields Hugin apps and nodes use
/// (`cipher`, `timestamp`, `hash`, and when present with the expected type `id`, `push`,
/// `viewtag`, `kind`), and `pow` reduced to `version`, `job { job_id, blob, target }`,
/// `auth { pub, sig, nonce }` and a `shares` list holding only the share `pow.auth` signs
/// ([`auth_share`], else the first). The signature covers the hash, timestamp, that share and
/// `kind:cipher`, so it stays valid. `None` when `cipher`, `hash`, `timestamp` or `pow` is
/// missing or the result is larger than [`MAX_MESSAGE_BYTES`].
#[must_use]
pub fn canonical_message(m: &Value) -> Option<Value> {
    let obj = m.as_object()?;
    let mut out = serde_json::Map::new();
    for key in ["cipher", "hash"] {
        out.insert(
            key.into(),
            Value::String(obj.get(key)?.as_str()?.to_owned()),
        );
    }
    let ts = obj.get("timestamp").filter(|t| t.is_number())?;
    out.insert("timestamp".into(), ts.clone());
    if let Some(id) = obj
        .get("id")
        .filter(|v| v.is_number() || v.as_str().is_some_and(|s| s.len() <= 64))
    {
        out.insert("id".into(), id.clone());
    }
    if let Some(push) = obj.get("push").filter(|v| v.is_boolean()) {
        out.insert("push".into(), push.clone());
    }
    if let Some(tag) = obj
        .get("viewtag")
        .filter(|v| v.as_str().is_some_and(|s| s.len() <= 64))
    {
        out.insert("viewtag".into(), tag.clone());
    }
    if let Some(kind) = obj.get("kind").filter(|v| v.is_string()) {
        out.insert("kind".into(), kind.clone());
    }
    let pow = obj.get("pow").filter(|p| p.is_object())?;
    let mut p = serde_json::Map::new();
    if let Some(v) = pow.get("version").filter(|v| v.is_number()) {
        p.insert("version".into(), v.clone());
    }
    let strings = |src: &Value, keys: &[&str]| -> Value {
        let mut o = serde_json::Map::new();
        for k in keys {
            if let Some(v) = src.get(*k).filter(|v| v.is_string()) {
                o.insert((*k).into(), v.clone());
            }
        }
        Value::Object(o)
    };
    if let Some(job) = pow.get("job").filter(|j| j.is_object()) {
        p.insert("job".into(), strings(job, &["job_id", "blob", "target"]));
    }
    if let Some(auth) = pow.get("auth").filter(|a| a.is_object()) {
        p.insert("auth".into(), strings(auth, &["pub", "sig", "nonce"]));
    }
    let share = auth_share(pow).ok().or_else(|| {
        pow.get("shares")
            .and_then(Value::as_array)
            .and_then(|a| a.first())
    });
    let shares: Vec<Value> = share
        .map(|s| strings(s, &["job_id", "nonce", "result"]))
        .into_iter()
        .collect();
    p.insert("shares".into(), Value::Array(shares));
    out.insert("pow".into(), Value::Object(p));
    let out = Value::Object(out);
    (serde_json::to_vec(&out).ok()?.len() <= MAX_MESSAGE_BYTES).then_some(out)
}

/// Digest of what a share pays for: a domain (`msg` or `reg`) and the signed context
/// ([`auth_context_message`] / [`auth_context_register`]). Two messages with the same digest
/// carry the same content.
#[must_use]
pub fn content_digest(domain: &str, context: &str) -> [u8; 32] {
    let h = Sha512::new()
        .chain_update(domain.as_bytes())
        .chain_update([0u8])
        .chain_update(context.as_bytes())
        .finalize();
    let mut out = [0u8; 32];
    out.copy_from_slice(&h[..32]);
    out
}

/// `pow.auth.sig` as lowercase hex, if well-formed (hugin-node `pow_auth_cache_key`).
#[must_use]
pub fn auth_cache_key(hash: Option<&Value>, pow: Option<&Value>) -> Option<String> {
    let hash = hash?.as_str()?;
    if hash.len() != 64 || !is_hex(hash) {
        return None;
    }
    let sig = pow?.get("auth")?.get("sig")?.as_str()?.to_lowercase();
    (sig.len() == 128 && is_hex(&sig)).then(|| format!("{hash}:{sig}"))
}

/// ed25519 verification with libsodium's `crypto_sign_verify_detached` rules (what keypear's
/// `verify` calls): canonical `S`, canonical non-small-order public key, non-small-order `R`,
/// cofactorless equation `R == S·B − H(R‖A‖M)·A` compared as encodings.
#[must_use]
pub fn ed25519_verify(public: &[u8; 32], msg: &[u8], sig: &[u8; 64]) -> bool {
    let mut big_r = [0u8; 32];
    big_r.copy_from_slice(&sig[..32]);
    let mut scalar = [0u8; 32];
    scalar.copy_from_slice(&sig[32..]);
    let Some(s) = Option::<Scalar>::from(Scalar::from_canonical_bytes(scalar)) else {
        return false;
    };
    match CompressedEdwardsY(big_r).decompress() {
        Some(r) if !r.is_small_order() => {}
        _ => return false,
    }
    let a = match CompressedEdwardsY(*public).decompress() {
        Some(a) if !a.is_small_order() && a.compress().to_bytes() == *public => a,
        _ => return false,
    };
    let h = Sha512::new()
        .chain_update(big_r)
        .chain_update(public)
        .chain_update(msg)
        .finalize();
    let mut wide = [0u8; 64];
    wide.copy_from_slice(&h[..]);
    let k = Scalar::from_bytes_mod_order_wide(&wide);
    let check = EdwardsPoint::vartime_double_scalar_mul_basepoint(&k, &(-a), &s);
    check.compress().to_bytes() == big_r
}

/// ed25519 signing from a 32-byte seed the way keypear does it: the scalar is the clamped
/// `SHA-512(seed)[..32]` (`extension_tweak_ed25519_sk_to_scalar`) and the signature is
/// sodium-native's `extension_tweak_ed25519_sign_detached`, whose nonce is
/// `SHA-512(dom2 prefix || scalar || message)` instead of RFC 8032's `SHA-512(prefix || message)`.
/// Verification is standard ed25519, so both forms verify; this one reproduces hugin's bytes.
/// Only used by tests and the test client; the relay never signs.
/// `dom2(x = 0, y = "crypto_tweak_ed25519")` as in sodium-native's `extensions/tweak/tweak.c`.
const TWEAK_PREFIX: &[u8] = b"SigEd25519 no Ed25519 collisions\x00\x14crypto_tweak_ed25519";

#[must_use]
pub fn ed25519_sign(seed: &[u8; 32], msg: &[u8]) -> ([u8; 32], [u8; 64]) {
    let h = Sha512::digest(seed);
    let mut clamped = [0u8; 32];
    clamped.copy_from_slice(&h[..32]);
    clamped[0] &= 0xf8;
    clamped[31] &= 0x7f;
    clamped[31] |= 0x40;
    let a = Scalar::from_bytes_mod_order(clamped);
    let public = EdwardsPoint::mul_base(&a).compress().to_bytes();
    let nonce_hash = Sha512::new()
        .chain_update(TWEAK_PREFIX)
        .chain_update(clamped)
        .chain_update(msg)
        .finalize();
    let mut wide = [0u8; 64];
    wide.copy_from_slice(&nonce_hash[..]);
    let nonce = Scalar::from_bytes_mod_order_wide(&wide);
    let big_r = EdwardsPoint::mul_base(&nonce).compress().to_bytes();
    let challenge = Sha512::new()
        .chain_update(big_r)
        .chain_update(public)
        .chain_update(msg)
        .finalize();
    wide.copy_from_slice(&challenge[..]);
    let k = Scalar::from_bytes_mod_order_wide(&wide);
    let s = nonce + k * a;
    let mut sig = [0u8; 64];
    sig[..32].copy_from_slice(&big_r);
    sig[32..].copy_from_slice(s.as_bytes());
    (public, sig)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    const VECTORS: &str = include_str!("../../../docs/test-vectors/hugin-relay.json");

    fn vectors() -> Value {
        serde_json::from_str(VECTORS).unwrap()
    }

    fn b32(h: &str) -> [u8; 32] {
        hex::decode(h).unwrap().try_into().unwrap()
    }

    #[test]
    fn blob_helpers_match_hugin_utils() {
        let v = vectors();
        let p = &v["pow"];
        let blob = hex::decode(p["job"]["blob"].as_str().unwrap()).unwrap();
        assert_eq!(
            nonce_offset(&blob),
            p["nonce_offset"].as_u64().unwrap() as usize
        );
        assert_eq!(
            extract_prev_id(&blob).map(hex::encode).as_deref(),
            p["prev_id"].as_str()
        );
        let nonce: [u8; 4] = hex::decode(p["nonce"].as_str().unwrap())
            .unwrap()
            .try_into()
            .unwrap();
        assert_eq!(
            hex::encode(insert_nonce(&blob, &nonce).unwrap()),
            p["hashing_blob"].as_str().unwrap()
        );
        let short = hex::decode(p["short_blob"].as_str().unwrap()).unwrap();
        assert_eq!(
            nonce_offset(&short),
            p["short_nonce_offset"].as_u64().unwrap() as usize
        );
        assert_eq!(
            extract_prev_id(&short).map(hex::encode).as_deref(),
            p["short_prev_id"].as_str()
        );
        assert!(p["bad_prev_id"].is_null());
        assert_eq!(extract_prev_id(&[5, 0, 0xff]), None);
        assert_eq!(nonce_offset(&[0x80; 20]), DEFAULT_NONCE_OFFSET);
    }

    #[test]
    fn targets_match_hugin_utils() {
        let v = vectors();
        for case in v["pow"]["meets_target"].as_array().unwrap() {
            let hash = b32(case["hash"].as_str().unwrap());
            let target = case["target"].as_str().unwrap();
            assert_eq!(
                meets_target(&hash, target),
                case["meets"].as_bool().unwrap(),
                "{case}"
            );
        }
        assert_eq!(parse_target("ffffffff"), Some(u64::MAX));
        assert_eq!(parse_target("00000000"), None);
        assert_eq!(parse_target("0102"), None);
        assert_eq!(target_for_difficulty(1), "ffffffff");
        assert_eq!(target_difficulty(&target_for_difficulty(5000)), Some(5000));
    }

    #[test]
    fn share_verification_matches_hugin_utils() {
        let v = vectors();
        let p = &v["pow"];
        let job = PowJob::from_value(&p["job"], None).unwrap();
        let nonce = p["nonce"].as_str().unwrap();
        let result = p["result"].as_str().unwrap();
        assert!(p["verify_share"].as_bool().unwrap());
        assert!(verify_share(&job, nonce, result));
        assert!(!p["verify_share_wrong_nonce"].as_bool().unwrap());
        assert!(!verify_share(&job, "a1b2c3d5", result));
        // hugin-utils compares the result case-sensitively
        assert!(!verify_share(&job, nonce, &result.to_uppercase()));
        // a target the hash does not meet
        let hard = PowJob {
            target: "01000000".into(),
            ..job.clone()
        };
        assert!(!verify_share(&hard, nonce, result));
        // stricter than hugin-utils: a zero target is not "always met"
        let zero = PowJob {
            target: "00000000".into(),
            ..job
        };
        assert!(!verify_share(&zero, nonce, result));
    }

    #[test]
    fn verified_results_against_other_targets() {
        let mut hash = [0u8; 32];
        hash[24..32].copy_from_slice(&(u64::MAX / 1000).to_le_bytes());
        let result = hex::encode(hash);
        // difficulty 1000 is met exactly, a harder target is not
        assert!(result_meets_target(&result, &target_for_difficulty(200)));
        assert!(result_meets_target(&result, &target_for_difficulty(999)));
        assert!(!result_meets_target(&result, &target_for_difficulty(1100)));
        // 8-byte targets as p2pool may send them
        assert!(result_meets_target(
            &result,
            &hex::encode((u64::MAX / 1000).to_le_bytes())
        ));
        assert!(!result_meets_target(
            &result,
            &hex::encode((u64::MAX / 1000 - 1).to_le_bytes())
        ));
        assert!(!result_meets_target(&result, "00000000"));
        assert!(!result_meets_target(&result[2..], "ffffffff"));
        assert!(!result_meets_target("zz", "ffffffff"));
    }

    #[test]
    fn auth_signatures_match_keypear() {
        let v = vectors();
        let p = &v["pow"];
        let share = json!({
            "job_id": p["job"]["job_id"],
            "nonce": p["nonce"],
            "result": p["result"],
        });
        for case in v["auth"].as_array().unwrap() {
            let payload = pow_sig_payload(
                case["hash"].as_str().unwrap(),
                &js_display(Some(&case["timestamp"])),
                &share,
                case["context"].as_str().unwrap(),
            );
            assert_eq!(payload, case["payload"].as_str().unwrap());
            let public = b32(case["pub"].as_str().unwrap());
            let sig: [u8; 64] = hex::decode(case["sig"].as_str().unwrap())
                .unwrap()
                .try_into()
                .unwrap();
            assert!(ed25519_verify(&public, payload.as_bytes(), &sig));
            assert!(!ed25519_verify(&public, b"other", &sig));
            // our signer produces the same (deterministic) signature as libsodium
            let (pk, own) = ed25519_sign(&[7u8; 32], payload.as_bytes());
            assert_eq!(pk, public);
            assert_eq!(own, sig);

            let pow = json!({
                "version": 2,
                "job": p["job"],
                "shares": [share],
                "auth": { "pub": case["pub"], "sig": case["sig"], "nonce": p["nonce"] },
            });
            assert_eq!(
                verify_pow_auth(
                    Some(&case["hash"]),
                    Some(&case["timestamp"]),
                    &pow,
                    case["context"].as_str().unwrap()
                ),
                Ok(())
            );
            assert_eq!(
                verify_pow_auth(Some(&case["hash"]), Some(&json!(1)), &pow, ""),
                Err("invalid_pow_signature")
            );
        }
    }

    #[test]
    fn auth_rejections() {
        let pow = json!({ "shares": [{ "nonce": "aa" }], "auth": { "pub": "zz", "sig": "00" } });
        assert_eq!(
            verify_pow_auth(None, None, &pow, ""),
            Err("invalid_pow_auth")
        );
        assert_eq!(
            verify_pow_auth(None, None, &json!({}), ""),
            Err("missing_pow_auth")
        );
        assert_eq!(
            verify_pow_auth(None, None, &json!(1), ""),
            Err("missing_pow")
        );
        let pow = json!({
            "shares": [{ "nonce": "aa" }],
            "auth": { "pub": "00".repeat(32), "sig": "00".repeat(64), "nonce": "bb" }
        });
        assert_eq!(
            verify_pow_auth(None, None, &pow, ""),
            Err("pow_auth_nonce_mismatch")
        );
        // small-order public key (identity) is refused
        let mut id = [0u8; 32];
        id[0] = 1;
        assert!(!ed25519_verify(&id, b"", &[0u8; 64]));
    }

    #[test]
    fn share_difficulty_matches_hugin_node() {
        assert_eq!(share_difficulty("285c8f02"), Some(100));
        assert_eq!(target_for_difficulty(100), "285c8f02");
        assert_eq!(share_difficulty("ffffffff"), Some(1));
        assert_eq!(share_difficulty("00000000"), None);
        assert_eq!(share_difficulty("ffff"), None);
        assert_eq!(share_difficulty("zzzzzzzz"), None);
        assert_eq!(share_difficulty("ffffffffffffffff"), None);
    }

    #[test]
    fn canonical_messages_keep_known_fields_and_the_signed_share() {
        let now = 1_731_500_000_000.0;
        let mut m = message(now);
        let signed =
            json!({ "job_id": "j", "nonce": "0000000a", "result": "11".repeat(32), "x": 1 });
        m["pow"]["shares"] = json!([m["pow"]["shares"][0].clone(), signed]);
        m["pow"]["auth"] =
            json!({ "pub": "ab", "sig": "cd", "nonce": "0000000A", "extra": [1, 2] });
        m["pow"]["job"]["height"] = json!(5);
        m["pow"]["pad"] = json!([0, 0, 0]);
        m["pad"] = json!(vec![0u8; 100]);
        m["id"] = json!(7);
        m["push"] = json!(true);
        m["viewtag"] = json!("abc");
        let c = canonical_message(&m).unwrap();
        assert_eq!(
            c,
            json!({
                "cipher": "abcd",
                "hash": "ab".repeat(32),
                "timestamp": now,
                "kind": "dm",
                "id": 7,
                "push": true,
                "viewtag": "abc",
                "pow": {
                    "version": 2,
                    "job": { "job_id": "j", "blob": m["pow"]["job"]["blob"], "target": "ffffffff" },
                    "shares": [{ "job_id": "j", "nonce": "0000000a", "result": "11".repeat(32) }],
                    "auth": { "pub": "ab", "sig": "cd", "nonce": "0000000A" },
                },
            })
        );
        // without auth.nonce the first share is kept
        m["pow"]["auth"]["nonce"] = json!("");
        let c = canonical_message(&m).unwrap();
        assert_eq!(c["pow"]["shares"][0]["nonce"], "00000000");
        // wrong types are left out, a missing required field is refused
        m["push"] = json!("yes");
        m["id"] = json!({ "a": 1 });
        let c = canonical_message(&m).unwrap();
        assert!(c.get("push").is_none() && c.get("id").is_none());
        let mut no_cipher = m.clone();
        no_cipher.as_object_mut().unwrap().remove("cipher");
        assert!(canonical_message(&no_cipher).is_none());
        // larger than MAX_MESSAGE_BYTES in canonical form
        m["cipher"] = json!("ab".repeat(4_500));
        assert!(canonical_message(&m).is_none());
    }

    #[test]
    fn auth_share_picks_the_signed_one() {
        let pow =
            json!({ "shares": [{ "nonce": "aa" }, { "nonce": "BB" }], "auth": { "nonce": "bb" } });
        assert_eq!(auth_share(&pow).unwrap()["nonce"], "BB");
        let pow = json!({ "shares": [{ "nonce": "aa" }, { "nonce": "bb" }], "auth": {} });
        assert_eq!(auth_share(&pow).unwrap()["nonce"], "aa");
        let pow = json!({ "shares": [], "auth": {} });
        assert_eq!(auth_share(&pow).err(), Some("no_shares"));
        assert_ne!(
            content_digest("msg", "dm:aa"),
            content_digest("reg", "dm:aa")
        );
        assert_ne!(
            content_digest("msg", "dm:aa"),
            content_digest("msg", "dm:ab")
        );
    }

    #[test]
    fn js_rendering() {
        assert_eq!(js_display(None), "undefined");
        assert_eq!(
            js_display(Some(&json!(1_731_500_000_000u64))),
            "1731500000000"
        );
        assert_eq!(js_display(Some(&json!(1.5))), "1.5");
        assert_eq!(js_display(Some(&json!(2.0))), "2");
        assert_eq!(js_display(Some(&json!(null))), "null");
        assert_eq!(js_display(Some(&json!([1, null, "a"]))), "1,,a");
        assert_eq!(js_display(Some(&json!({}))), "[object Object]");
    }

    fn message(now: f64) -> Value {
        json!({
            "cipher": "abcd",
            "hash": "ab".repeat(32),
            "timestamp": now,
            "kind": "dm",
            "pow": {
                "version": 2,
                "job": { "job_id": "j", "blob": "0500".to_owned() + &"11".repeat(40), "target": "ffffffff" },
                "shares": [{ "job_id": "j", "nonce": "00000000", "result": "00".repeat(32) }],
            },
        })
    }

    #[test]
    fn message_shape_rules() {
        let now = 1_731_500_000_000.0;
        assert!(check_message(&message(now), now, MAX_MESSAGE_PAST_MS));
        let mut m = message(now - MAX_MESSAGE_PAST_MS - 1.0);
        assert!(!check_message(&m, now, MAX_MESSAGE_PAST_MS));
        assert!(check_message(&m, now, 24.0 * MAX_MESSAGE_PAST_MS));
        m = message(now + MAX_MESSAGE_FUTURE_MS + 1.0);
        assert!(!check_message(&m, now, MAX_MESSAGE_PAST_MS));
        for (k, bad) in [
            ("cipher", json!("xyz")),
            ("cipher", json!(1)),
            ("hash", json!("ab".repeat(33))),
            ("kind", json!("feed")),
            ("kind", json!(null)),
            ("timestamp", json!("1")),
        ] {
            let mut m = message(now);
            m[k] = bad;
            assert!(!check_message(&m, now, MAX_MESSAGE_PAST_MS), "{k}");
        }
        let mut m = message(now);
        m["pow"]["version"] = json!("2");
        assert!(!check_message(&m, now, MAX_MESSAGE_PAST_MS));
        let mut m = message(now);
        m["pow"]["shares"] = json!([{}, {}, {}, {}, {}, {}]);
        assert!(!check_message(&m, now, MAX_MESSAGE_PAST_MS));
        let mut m = message(now);
        m["pow"]["job"]["target"] = json!("fffffff");
        assert!(!check_message(&m, now, MAX_MESSAGE_PAST_MS));
        let mut m = message(now);
        m.as_object_mut().unwrap().remove("kind");
        assert!(check_message(&m, now, MAX_MESSAGE_PAST_MS));
    }

    #[test]
    fn payload_validation_and_freshness() {
        let now = 1_731_500_000_000.0;
        let m = message(now);
        let ok = validate_pow_payload(m.get("hash"), &m["pow"], None).unwrap();
        assert_eq!(ok.shares.len(), 1);
        assert_eq!(ok.prev_id, [0x11; 32]);
        let mut prev = PrevIds::default();
        assert_eq!(
            validate_pow_payload(m.get("hash"), &m["pow"], Some(&prev)).err(),
            Some("no_current_prev_id")
        );
        prev.push([0x11; 32]);
        prev.push([0x22; 32]);
        prev.push([0x22; 32]); // a re-sent job for the same block does not shift
        prev.push([0x33; 32]);
        assert!(validate_pow_payload(m.get("hash"), &m["pow"], Some(&prev)).is_ok());
        prev.push([0x44; 32]);
        assert_eq!(
            validate_pow_payload(m.get("hash"), &m["pow"], Some(&prev)).err(),
            Some("prev_id_mismatch")
        );
        assert_eq!(
            validate_pow_payload(Some(&json!("ab")), &m["pow"], None).err(),
            Some("invalid_hash")
        );
        assert_eq!(
            validate_share_fast(&m["pow"]["shares"][0], "j").map(|s| s.key()),
            Ok(format!("j:00000000:{}", "00".repeat(32)))
        );
        assert_eq!(
            validate_share_fast(&m["pow"]["shares"][0], "k").err(),
            Some("job_id_mismatch")
        );
        assert_eq!(
            validate_share_fast(&json!({"job_id": "j", "nonce": "0"}), "j").err(),
            Some("bad_nonce")
        );
    }
}
