//! Single-key CLSAG ring signatures for transaction version 2 (fork 14, `docs/tx-v2.md`).
//!
//! CLSAG ("Concise Linkable Spontaneous Anonymous Group", Goodell et al. 2019) as
//! Monero uses it since 2020 (`CLSAG_Gen` / `verRctCLSAGSimple` in `src/ringct/rctSigs.cpp`),
//! with **one** key component: XKR has no `RingCT`, so there is no commitment column `C`, no
//! commitment offset and no auxiliary key image `D`. What is left is a compact linkable ring
//! signature: one response per ring member plus one challenge, instead of the `(c, r)` pair per
//! member of the CryptoNote ring signature ([`crate::ring`]).
//!
//! With ring `P_0 .. P_{n-1}`, key image `I = x * H_p(P_l)`, message `m` (the transaction prefix
//! hash) and `H_s` = [`hash_to_scalar`] (Keccak-256, reduced mod `l`), `H_p` = [`hash_to_ec`]:
//!
//! ```text
//! mu    = H_s(pad32("XKR_CLSAG_agg_0") || P_0 || .. || P_{n-1} || I)
//! c_i+1 = H_s(pad32("XKR_CLSAG_round") || P_0 || .. || P_{n-1} || m || L_i || R_i)
//! L_i   = s_i * G      + (c_i * mu) * P_i
//! R_i   = s_i * H_p(P_i) + (c_i * mu) * I
//! valid <=> c_n == c_0 (indexes mod n), c1 = c_0 stored
//! ```
//!
//! `pad32(s)` is the ASCII string padded with zero bytes to 32 bytes (Monero pads its
//! `config::HASH_KEY_CLSAG_*` strings the same way). Points are 32-byte compressed encodings,
//! scalars 32-byte little endian. The key image is the CryptoNote one, so the spent-key-image
//! set is shared between transaction versions 1 and 2.

use curve25519_dalek::edwards::EdwardsPoint;
use curve25519_dalek::scalar::Scalar;
use curve25519_dalek::traits::{IsIdentity, VartimeMultiscalarMul};
use rand_core::CryptoRng;
use subtle::ConstantTimeEq;
use xkr_types::{ClsagSignature, EcScalar, Hash, KeyImage, PublicKey, SecretKey};
use zeroize::Zeroize;

use crate::ops::{
    canonical, decompress, hash_to_ec, hash_to_scalar, point_in_prime_subgroup, random_scalar,
};

/// Domain separator of the aggregation coefficient `mu` (zero-padded to 32 bytes).
pub const CLSAG_AGG_DOMAIN: &[u8] = b"XKR_CLSAG_agg_0";
/// Domain separator of the round hash (zero-padded to 32 bytes).
pub const CLSAG_ROUND_DOMAIN: &[u8] = b"XKR_CLSAG_round";

fn pad32(domain: &[u8]) -> [u8; 32] {
    let mut out = [0u8; 32];
    out[..domain.len()].copy_from_slice(domain);
    out
}

/// `mu = H_s(pad32(CLSAG_AGG_DOMAIN) || P_0 || .. || P_{n-1} || I)`.
#[must_use]
pub fn clsag_aggregation_coefficient(ring: &[PublicKey], key_image: &KeyImage) -> Scalar {
    let mut buf = Vec::with_capacity(32 * (ring.len() + 2));
    buf.extend_from_slice(&pad32(CLSAG_AGG_DOMAIN));
    for p in ring {
        buf.extend_from_slice(&p.0);
    }
    buf.extend_from_slice(&key_image.0);
    hash_to_scalar(&buf)
}

/// The fixed part of the round hash: `pad32(CLSAG_ROUND_DOMAIN) || P_0 || .. || P_{n-1} || m`,
/// with room for `L || R`.
fn round_transcript(ring: &[PublicKey], msg: &Hash) -> Vec<u8> {
    let mut buf = Vec::with_capacity(32 * (ring.len() + 4));
    buf.extend_from_slice(&pad32(CLSAG_ROUND_DOMAIN));
    for p in ring {
        buf.extend_from_slice(&p.0);
    }
    buf.extend_from_slice(&msg.0);
    buf
}

/// `H_s(transcript || L || R)`, reusing the buffer.
fn round_hash(
    transcript: &mut Vec<u8>,
    base_len: usize,
    l: &EdwardsPoint,
    r: &EdwardsPoint,
) -> Scalar {
    transcript.truncate(base_len);
    transcript.extend_from_slice(l.compress().as_bytes());
    transcript.extend_from_slice(r.compress().as_bytes());
    hash_to_scalar(transcript)
}

/// Signs `msg` with the secret `secret` of ring member `index` (`ring[index] = secret * G`).
///
/// The key image is `x * H_p(P_index)` ([`crate::generate_key_image`]); the caller puts it into
/// the input. Returns `None` if `index` is out of range, a ring member does not decode, or
/// `secret` does not belong to `ring[index]`. The nonce and the decoy responses come from `rng`
/// ([`random_scalar`] each: `alpha` first, then `s_{index+1}, s_{index+2}, ..` wrapping to
/// `s_{index-1}`); [`clsag_sign_with_randomness`] takes them explicitly instead.
///
/// Secret-dependent arithmetic is constant time (dalek scalar arithmetic, fixed-base and
/// variable-base constant-time multiplication for the nonce); the work per ring position is the
/// same wherever `index` is. The nonce and the secret scalar are zeroized.
#[allow(clippy::similar_names)] // `ring` and `rng` are the conventional names
pub fn clsag_sign<R: CryptoRng + ?Sized>(
    msg: &Hash,
    ring: &[PublicKey],
    secret: &SecretKey,
    index: usize,
    rng: &mut R,
) -> Option<ClsagSignature> {
    sign_with(msg, ring, secret, index, &mut || random_scalar(rng))
}

/// [`clsag_sign`] with the randomness given by the caller: the nonce `alpha` and the decoy
/// responses in consumption order `s_{index+1}, s_{index+2}, .., s_{index-1}` (indexes mod
/// `n`, so `ring.len() - 1` of them). Deterministic: for test vectors (`docs/tx-v2.md` §7)
/// and for wallets that split signing. Every scalar must be canonical (`< l`).
///
/// Returns `None` where [`clsag_sign`] does, and also for a wrong number of decoy responses or
/// a non-canonical scalar. With the same scalars it returns exactly what [`clsag_sign`] returns
/// for an RNG whose [`random_scalar`] draws yield them in that order.
#[must_use]
pub fn clsag_sign_with_randomness(
    msg: &Hash,
    ring: &[PublicKey],
    secret: &SecretKey,
    index: usize,
    alpha: &EcScalar,
    decoy_responses: &[EcScalar],
) -> Option<ClsagSignature> {
    if decoy_responses.len() + 1 != ring.len() {
        return None;
    }
    let mut draws = Vec::with_capacity(ring.len());
    draws.push(canonical(&alpha.0)?);
    for s in decoy_responses {
        draws.push(canonical(&s.0)?);
    }
    let mut next = draws.into_iter();
    let mut draw = || next.next().unwrap_or(Scalar::ZERO);
    let sig = sign_with(msg, ring, secret, index, &mut draw);
    // Left over only if signing stopped early (bad index, ring or secret).
    for mut rest in next {
        rest.zeroize();
    }
    sig
}

/// The signer of [`clsag_sign`] / [`clsag_sign_with_randomness`]; `draw` yields `alpha` and then
/// the decoy responses, and is only called once every check has passed.
#[allow(clippy::similar_names)] // as in the former body of `clsag_sign`
fn sign_with(
    msg: &Hash,
    ring: &[PublicKey],
    secret: &SecretKey,
    index: usize,
    draw: &mut dyn FnMut() -> Scalar,
) -> Option<ClsagSignature> {
    let n = ring.len();
    if index >= n {
        return None;
    }
    let points: Vec<EdwardsPoint> = ring
        .iter()
        .map(|p| decompress(&p.0))
        .collect::<Option<_>>()?;
    let mut x = Scalar::from_bytes_mod_order(secret.0);
    let own = EdwardsPoint::mul_base(&x).compress();
    if !bool::from(own.as_bytes()[..].ct_eq(&ring[index].0[..])) {
        x.zeroize();
        return None;
    }
    let hp_real = hash_to_ec(&ring[index]);
    let image_point = hp_real * x;
    let key_image = KeyImage(image_point.compress().to_bytes());
    let mu = clsag_aggregation_coefficient(ring, &key_image);

    let mut transcript = round_transcript(ring, msg);
    let base_len = transcript.len();

    let mut alpha = draw();
    let nonce_g = EdwardsPoint::mul_base(&alpha);
    let nonce_hp = hp_real * alpha;
    let mut c = round_hash(&mut transcript, base_len, &nonce_g, &nonce_hp);

    let mut s = vec![EcScalar::ZERO; n];
    let mut c1 = Scalar::ZERO;
    let mut i = (index + 1) % n;
    if i == 0 {
        c1 = c;
    }
    while i != index {
        let si = draw();
        s[i] = EcScalar(si.to_bytes());
        let cp = c * mu;
        let l = EdwardsPoint::vartime_double_scalar_mul_basepoint(&cp, &points[i], &si);
        let r =
            EdwardsPoint::vartime_multiscalar_mul([si, cp], [hash_to_ec(&ring[i]), image_point]);
        c = round_hash(&mut transcript, base_len, &l, &r);
        i = (i + 1) % n;
        if i == 0 {
            c1 = c;
        }
    }
    // s_l = alpha - c_l * mu * x
    let s_real = alpha - c * mu * x;
    s[index] = EcScalar(s_real.to_bytes());
    alpha.zeroize();
    x.zeroize();
    Some(ClsagSignature {
        s,
        c1: EcScalar(c1.to_bytes()),
    })
}

/// The challenges `c_0 .. c_{n-1}` a verifier walks through (a debug aid for test vectors,
/// `docs/tx-v2.md` §7): `c_0 = c1` and `c_{i+1} = H_s(round transcript with L_i, R_i)`.
/// For a valid signature the round hash of the last member is `c1` again. `None` for an empty
/// ring, `s.len() != ring.len()`, a non-canonical scalar, or a key image or ring member that
/// does not decode (subgroup and zero-hash checks are left to [`clsag_verify`]).
#[must_use]
pub fn clsag_challenge_chain(
    msg: &Hash,
    ring: &[PublicKey],
    key_image: &KeyImage,
    sig: &ClsagSignature,
) -> Option<Vec<EcScalar>> {
    let n = ring.len();
    if n == 0 || sig.s.len() != n {
        return None;
    }
    let mut c = canonical(&sig.c1.0)?;
    let image_point = decompress(&key_image.0)?;
    let mu = clsag_aggregation_coefficient(ring, key_image);
    let mut transcript = round_transcript(ring, msg);
    let base_len = transcript.len();
    let mut chain = Vec::with_capacity(n);
    for (key, si) in ring.iter().zip(&sig.s) {
        chain.push(EcScalar(c.to_bytes()));
        let si = canonical(&si.0)?;
        let point = decompress(&key.0)?;
        let cp = c * mu;
        let l = EdwardsPoint::vartime_double_scalar_mul_basepoint(&cp, &point, &si);
        let r = EdwardsPoint::vartime_multiscalar_mul([si, cp], [hash_to_ec(key), image_point]);
        c = round_hash(&mut transcript, base_len, &l, &r);
    }
    Some(chain)
}

/// Verifies a CLSAG signature (Monero `verRctCLSAGSimple` with one key column).
///
/// Rejects: an empty ring, `s.len() != ring.len()`, any non-canonical `s_i` or `c1` (`sc_check`),
/// a key image that does not decode, is the identity or is not in the prime-order subgroup
/// ([`crate::key_image_in_prime_subgroup`]), a ring member that does not decode, and a zero
/// round hash (as Monero does).
#[must_use]
pub fn clsag_verify(
    msg: &Hash,
    ring: &[PublicKey],
    key_image: &KeyImage,
    sig: &ClsagSignature,
) -> bool {
    let n = ring.len();
    if n == 0 || sig.s.len() != n {
        return false;
    }
    let Some(c1) = canonical(&sig.c1.0) else {
        return false;
    };
    let Some(s) = sig
        .s
        .iter()
        .map(|si| canonical(&si.0))
        .collect::<Option<Vec<Scalar>>>()
    else {
        return false;
    };
    let Some(image_point) = decompress(&key_image.0) else {
        return false;
    };
    if image_point.is_identity() || !point_in_prime_subgroup(&image_point) {
        return false;
    }
    let Some(points) = ring
        .iter()
        .map(|p| decompress(&p.0))
        .collect::<Option<Vec<EdwardsPoint>>>()
    else {
        return false;
    };
    let mu = clsag_aggregation_coefficient(ring, key_image);
    let mut transcript = round_transcript(ring, msg);
    let base_len = transcript.len();
    let mut c = c1;
    for ((key, point), si) in ring.iter().zip(&points).zip(&s) {
        let cp = c * mu;
        let l = EdwardsPoint::vartime_double_scalar_mul_basepoint(&cp, point, si);
        let r = EdwardsPoint::vartime_multiscalar_mul([*si, cp], [hash_to_ec(key), image_point]);
        c = round_hash(&mut transcript, base_len, &l, &r);
        if c == Scalar::ZERO {
            return false;
        }
    }
    c == c1
}

/// One CLSAG verification job (data-oriented batch input, like
/// [`crate::RingSignatureCheck`]).
#[derive(Debug, Clone, Copy)]
pub struct ClsagCheck<'a> {
    /// Transaction prefix hash (the signed message).
    pub msg: &'a Hash,
    /// Key image of the input.
    pub key_image: &'a KeyImage,
    /// Ring member output keys, in ring order.
    pub ring: &'a [PublicKey],
    /// The signature.
    pub signature: &'a ClsagSignature,
}

impl ClsagCheck<'_> {
    /// [`clsag_verify`] of this job.
    #[must_use]
    pub fn verify(&self) -> bool {
        clsag_verify(self.msg, self.ring, self.key_image, self.signature)
    }
}

/// Checks a batch; `true` only if every job verifies. Sequential: CLSAG's hash chain has no
/// cheaper combined check, so callers parallelise over jobs (e.g. with rayon).
#[must_use]
pub fn clsag_verify_all(jobs: &[ClsagCheck<'_>]) -> bool {
    jobs.iter().all(ClsagCheck::verify)
}

#[cfg(test)]
#[allow(clippy::similar_names)] // `ring` and `rng`
mod tests {
    use super::*;
    use crate::keccak::cn_fast_hash;
    use crate::ops::{generate_key_image, generate_keys};

    /// Deterministic byte stream for test vectors: block `k` is
    /// `cn_fast_hash("xkr clsag test rng" || seed_le64 || k_le64)`. The Python reference
    /// (`crates/xkr-serialization/tests/fixtures/tools/clsag_ref.py`) uses the same stream.
    struct StreamRng {
        seed: u64,
        block: u64,
        buf: [u8; 32],
        pos: usize,
    }

    impl StreamRng {
        fn new(seed: u64) -> Self {
            Self {
                seed,
                block: 0,
                buf: [0; 32],
                pos: 32,
            }
        }
    }

    impl rand_core::RngCore for StreamRng {
        fn next_u32(&mut self) -> u32 {
            let mut b = [0u8; 4];
            self.fill_bytes(&mut b);
            u32::from_le_bytes(b)
        }
        fn next_u64(&mut self) -> u64 {
            let mut b = [0u8; 8];
            self.fill_bytes(&mut b);
            u64::from_le_bytes(b)
        }
        fn fill_bytes(&mut self, dst: &mut [u8]) {
            for b in dst {
                if self.pos == 32 {
                    let mut input = Vec::with_capacity(34);
                    input.extend_from_slice(b"xkr clsag test rng");
                    input.extend_from_slice(&self.seed.to_le_bytes());
                    input.extend_from_slice(&self.block.to_le_bytes());
                    self.buf = cn_fast_hash(&input).0;
                    self.block += 1;
                    self.pos = 0;
                }
                *b = self.buf[self.pos];
                self.pos += 1;
            }
        }
    }
    impl rand_core::CryptoRng for StreamRng {}

    /// Ring secret `i` of the vectors: `H_s("xkr clsag vector key" || i)`.
    fn vector_secret(i: u8) -> SecretKey {
        let mut data = b"xkr clsag vector key".to_vec();
        data.push(i);
        SecretKey(hash_to_scalar(&data).to_bytes())
    }

    fn vector_ring(n: u8) -> (Vec<PublicKey>, Vec<SecretKey>) {
        let secrets: Vec<SecretKey> = (0..n).map(vector_secret).collect();
        let ring = secrets
            .iter()
            .map(|s| crate::ops::secret_key_to_public_key(s).unwrap())
            .collect();
        (ring, secrets)
    }

    fn ring_of(rng: &mut StreamRng, n: usize) -> (Vec<PublicKey>, Vec<SecretKey>) {
        (0..n).map(|_| generate_keys(rng)).unzip()
    }

    #[test]
    fn sign_and_verify_every_ring_size_and_index() {
        let mut rng = StreamRng::new(1);
        for n in 1..=8usize {
            let (ring, secrets) = ring_of(&mut rng, n);
            for l in 0..n {
                let msg = cn_fast_hash(&[n as u8, l as u8]);
                let ki = generate_key_image(&ring[l], &secrets[l]);
                let sig = clsag_sign(&msg, &ring, &secrets[l], l, &mut rng).unwrap();
                assert_eq!(sig.s.len(), n);
                assert!(clsag_verify(&msg, &ring, &ki, &sig), "n {n} l {l}");
                assert!(
                    ClsagCheck {
                        msg: &msg,
                        key_image: &ki,
                        ring: &ring,
                        signature: &sig
                    }
                    .verify()
                );
            }
        }
    }

    /// The negative cases of Monero's `TEST(ringct, CLSAG)` (`tests/unit_tests/ringct.cpp`),
    /// adapted to one key column.
    #[test]
    fn monero_negative_cases() {
        let mut rng = StreamRng::new(2);
        let n = 11;
        let l = 4;
        let (ring, secrets) = ring_of(&mut rng, n);
        let msg = cn_fast_hash(b"message");
        let ki = generate_key_image(&ring[l], &secrets[l]);
        let sig = clsag_sign(&msg, &ring, &secrets[l], l, &mut rng).unwrap();
        assert!(clsag_verify(&msg, &ring, &ki, &sig));

        // zero message / other message
        assert!(!clsag_verify(&Hash::ZERO, &ring, &ki, &sig));
        assert!(!clsag_verify(&cn_fast_hash(b"other"), &ring, &ki, &sig));
        // wrong index: the secret does not belong to that member
        assert!(clsag_sign(&msg, &ring, &secrets[l], (l + 1) % n, &mut rng).is_none());
        // index out of range
        assert!(clsag_sign(&msg, &ring, &secrets[l], n, &mut rng).is_none());
        // wrong secret for the index
        let (_, other_secret) = generate_keys(&mut rng);
        assert!(clsag_sign(&msg, &ring, &other_secret, l, &mut rng).is_none());
        // wrong key image (another member's, a random point, identity, torsion)
        let other_ki = generate_key_image(&ring[0], &secrets[0]);
        assert!(!clsag_verify(&msg, &ring, &other_ki, &sig));
        let mut id = [0u8; 32];
        id[0] = 1;
        assert!(!clsag_verify(&msg, &ring, &KeyImage(id), &sig));
        let torsion = curve25519_dalek::constants::EIGHT_TORSION[1];
        let torsioned = decompress(&ki.0).unwrap() + torsion;
        assert!(!clsag_verify(
            &msg,
            &ring,
            &KeyImage(torsioned.compress().to_bytes()),
            &sig
        ));
        // altered s_i / c1
        for i in 0..n {
            let mut bad = sig.clone();
            bad.s[i].0[0] ^= 1;
            assert!(!clsag_verify(&msg, &ring, &ki, &bad), "s_{i}");
        }
        let mut bad = sig.clone();
        bad.c1.0[3] ^= 0x10;
        assert!(!clsag_verify(&msg, &ring, &ki, &bad));
        // wrong length
        let mut short = sig.clone();
        short.s.pop();
        assert!(!clsag_verify(&msg, &ring, &ki, &short));
        assert!(!clsag_verify(&msg, &ring[..n - 1], &ki, &sig));
        assert!(!clsag_verify(&msg, &[], &ki, &ClsagSignature::default()));
        // a ring member replaced, or the ring reordered
        let mut swapped = ring.clone();
        swapped.swap(0, 1);
        assert!(!clsag_verify(&msg, &swapped, &ki, &sig));
        let mut replaced = ring.clone();
        replaced[(l + 3) % n] = generate_keys(&mut rng).0;
        assert!(!clsag_verify(&msg, &replaced, &ki, &sig));
        // batch
        let good = ClsagCheck {
            msg: &msg,
            key_image: &ki,
            ring: &ring,
            signature: &sig,
        };
        let wrong = ClsagCheck {
            key_image: &other_ki,
            ..good
        };
        assert!(clsag_verify_all(&[good, good]));
        assert!(!clsag_verify_all(&[good, wrong]));
    }

    #[test]
    fn non_canonical_scalars_are_rejected() {
        // l, little endian
        const L: [u8; 32] = [
            0xed, 0xd3, 0xf5, 0x5c, 0x1a, 0x63, 0x12, 0x58, 0xd6, 0x9c, 0xf7, 0xa2, 0xde, 0xf9,
            0xde, 0x14, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x10,
        ];
        fn add_l(x: &[u8; 32]) -> Option<[u8; 32]> {
            let mut sum = [0u8; 32];
            let mut carry = 0u16;
            for ((out, a), b) in sum.iter_mut().zip(x).zip(&L) {
                let v = u16::from(*a) + u16::from(*b) + carry;
                *out = v as u8;
                carry = v >> 8;
            }
            (carry == 0).then_some(sum)
        }
        let mut rng = StreamRng::new(3);
        let (ring, secrets) = ring_of(&mut rng, 3);
        let msg = cn_fast_hash(b"m");
        let ki = generate_key_image(&ring[1], &secrets[1]);
        let sig = clsag_sign(&msg, &ring, &secrets[1], 1, &mut rng).unwrap();
        assert!(clsag_verify(&msg, &ring, &ki, &sig));
        // s_0 + l and c1 + l are the same scalars mod l but not canonical
        if let Some(big) = add_l(&sig.s[0].0) {
            let mut bad = sig.clone();
            bad.s[0] = EcScalar(big);
            assert!(!clsag_verify(&msg, &ring, &ki, &bad));
        }
        if let Some(big) = add_l(&sig.c1.0) {
            let mut bad = sig.clone();
            bad.c1 = EcScalar(big);
            assert!(!clsag_verify(&msg, &ring, &ki, &bad));
        }
    }

    #[test]
    fn signatures_are_unlinkable_but_key_images_link() {
        let mut rng = StreamRng::new(4);
        let (ring, secrets) = ring_of(&mut rng, 5);
        let msg = cn_fast_hash(b"a");
        let a = clsag_sign(&msg, &ring, &secrets[2], 2, &mut rng).unwrap();
        let b = clsag_sign(&msg, &ring, &secrets[2], 2, &mut rng).unwrap();
        // fresh randomness every time
        assert_ne!(a, b);
        // the same key image verifies both: the double-spend set links them
        let ki = generate_key_image(&ring[2], &secrets[2]);
        assert!(clsag_verify(&msg, &ring, &ki, &a) && clsag_verify(&msg, &ring, &ki, &b));
    }

    /// The explicit-randomness signer gives exactly what `clsag_sign` gives for an RNG that
    /// draws the same scalars (`alpha` first, then `s_{l+1} .. s_{l-1}`), and the challenge
    /// chain starts at `c1`.
    #[test]
    fn explicit_randomness_matches_the_rng_signer() {
        for n in 1..=6usize {
            let mut keys = StreamRng::new(20 + n as u64);
            let (ring, secrets) = ring_of(&mut keys, n);
            for l in 0..n {
                let msg = cn_fast_hash(&[0xc5, n as u8, l as u8]);
                let seed = 100 + (n * 10 + l) as u64;
                let mut rng = StreamRng::new(seed);
                let sig = clsag_sign(&msg, &ring, &secrets[l], l, &mut rng).unwrap();
                let mut draws = StreamRng::new(seed);
                let alpha = EcScalar(random_scalar(&mut draws).to_bytes());
                let decoys: Vec<EcScalar> = (1..n)
                    .map(|_| EcScalar(random_scalar(&mut draws).to_bytes()))
                    .collect();
                let x = &secrets[l];
                let explicit = clsag_sign_with_randomness(&msg, &ring, x, l, &alpha, &decoys);
                assert_eq!(explicit.as_ref(), Some(&sig), "n {n} l {l}");
                // the decoy responses land at l+1, l+2, .. (mod n)
                for (k, d) in decoys.iter().enumerate() {
                    assert_eq!(&sig.s[(l + 1 + k) % n], d, "n {n} l {l} k {k}");
                }
                let ki = generate_key_image(&ring[l], x);
                let chain = clsag_challenge_chain(&msg, &ring, &ki, &sig).unwrap();
                assert_eq!(chain.len(), n);
                assert_eq!(chain[0], sig.c1);
                assert!(clsag_verify(&msg, &ring, &ki, &sig));
                // a wrong number of decoy responses, a non-canonical alpha
                let few = &decoys[..decoys.len().saturating_sub(1)];
                if n > 1 {
                    assert!(clsag_sign_with_randomness(&msg, &ring, x, l, &alpha, few).is_none());
                }
                let big = EcScalar([0xff; 32]);
                assert!(clsag_sign_with_randomness(&msg, &ring, x, l, &big, &decoys).is_none());
            }
        }
    }

    /// Fixed vectors, recomputed independently by the Python reference
    /// `crates/xkr-serialization/tests/fixtures/tools/clsag_ref.py` (see `docs/tx-v2.md`).
    /// Ring secrets `x_i = H_s("xkr clsag vector key" || i)`, message
    /// `cn_fast_hash("xkr clsag vector message")`, nonces from `StreamRng::new(seed)`.
    #[test]
    fn fixed_vectors() {
        for v in VECTORS {
            let (ring, secrets) = vector_ring(v.n);
            let msg = cn_fast_hash(b"xkr clsag vector message");
            let l = usize::from(v.index);
            let ki = generate_key_image(&ring[l], &secrets[l]);
            assert_eq!(ki.to_hex(), v.key_image, "n {}", v.n);
            let mu = clsag_aggregation_coefficient(&ring, &ki);
            assert_eq!(hex::encode(mu.to_bytes()), v.mu, "n {}", v.n);
            let mut rng = StreamRng::new(v.seed);
            let sig = clsag_sign(&msg, &ring, &secrets[l], l, &mut rng).unwrap();
            assert_eq!(sig.c1.to_hex(), v.c1, "n {}", v.n);
            let s: Vec<String> = sig.s.iter().map(EcScalar::to_hex).collect();
            assert_eq!(s, v.s, "n {}", v.n);
            assert!(clsag_verify(&msg, &ring, &ki, &sig));
        }
    }

    struct Vector {
        n: u8,
        index: u8,
        seed: u64,
        key_image: &'static str,
        mu: &'static str,
        c1: &'static str,
        s: &'static [&'static str],
    }

    const VECTORS: &[Vector] = &[
        Vector {
            n: 1,
            index: 0,
            seed: 11,
            key_image: "7890cf3872bcf89f8caab0a5c81e52054f7ef3354d4db7d7df235b781f8f561a",
            mu: "99fef1b6556cf3fc57947bbae2b9873123a79f428b285b2047776eceeed7060d",
            c1: "0cf4cf2cf511c3964d923819e8f4d023c5f88805674ffdfa7528f6748785460b",
            s: &["d945d58cea7b0354a9b7d224d140461860f73fbef388f60250910f72e67a690e"],
        },
        Vector {
            n: 2,
            index: 1,
            seed: 12,
            key_image: "5dc37335c0e0eb0d120a43c025fe551aac9cd832a55190a9b8b80b271bb72b60",
            mu: "c6673391cef49f7027ae2bad8adb3da3275c8d295633f855da02c6c3e8af4406",
            c1: "388091a06538360540297dc771428616b714eed96c8bfa051bda8ee703b5fa03",
            s: &[
                "8c6c0c49b5f59f327287ee0dfac30d52c2966779f80f9a13951c207d202a0b03",
                "bb04440b851eb4ffb179f31932f0c7e95ffc7847b3f6e396b7bf8cb516106c0a",
            ],
        },
        Vector {
            n: 4,
            index: 2,
            seed: 13,
            key_image: "71d72cdd6cb5eec11c805b54917f5cb9bdc014c4adfb2fe70f5dcd59898b25b7",
            mu: "86f46b2989f3dd5acfc552d0e875455eaf53f551f91f8a9064b0de5248dc0a05",
            c1: "cf8b8573ad576afbde7308c43e09557fb9536187b0364abecf0e995133a3f805",
            s: &[
                "c3d7a90c814b56c1a28f3f7b2829aecafc2105b67717bbf588352f56482f0107",
                "db8e5e3bad0d8424607d3e8f835aa5f4c54eac5d0bc68ed2f66a6c91060cc108",
                "c15d302227647a94da85d1c8475c3d8314e3c75b2241e9d6d63098b660b98f00",
                "d90e534e0f450c6fe1109f71b6dae83272183ae46a47af512cb0a66ef14a5e01",
            ],
        },
        Vector {
            n: 11,
            index: 7,
            seed: 14,
            key_image: "15181b24bd60c0cac16ede80874ed6415d8f405301ae2349daa1f545e150ecb2",
            mu: "1a2d60143027cc63136ae1e9de8cc02250d914ab4609639d38305d84ecfd9a03",
            c1: "3301938e0940ec26b0aadb7a2dfab9caff28b9c863ecfb647e16361ee9643e05",
            s: &[
                "e71f8e40de7bd89a1c1944dacce5aad480d9a93ddfe5fc540bc4ff82d696a109",
                "e34c9de10654db46321aa36e670b73a2d6c3d30a4e895acb0fb4219a3ca7ec0d",
                "01265cd534bdb3d5e31555a882fd6865d31180ae4374eae62ba17c38953bb508",
                "5e62eaf5c8acd812d29742ec318e5f22af4c2b5b34d21b5b273b759c2887cc09",
                "a1bc5d7cb094d38a29d2925bb5ffb963fe01afeb5b6363e74b41d6f075955404",
                "6bf9ae8fc06f660b9c77d876424db68bb75afdfb6fe5a32b8afe997d9c783c04",
                "be37318036a6a7f013c78a9d3a1ae548f0867501f52a192ed2f3ba669cf5b905",
                "32b87cdb4ed7dc4bdd9096b55eff44678fe203f171fed0838cebde2d2101a807",
                "0b2b65e4e4ad80fea11205465077dd50cbd221aa9b0d90c8ebf60bc29bc5ae0a",
                "4a5485cf634cb3784336c5aebdb1dcbc678d59d6b17b4de3d420870cacce7803",
                "7892d2356fef6d65f1f3f6b6338b77cb47435564ec3d75790c22f35f85d7a70c",
            ],
        },
    ];
}
