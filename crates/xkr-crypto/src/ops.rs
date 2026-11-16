//! Ed25519 operations of `src/crypto/crypto.cpp` on top of `curve25519-dalek`.
//!
//! Point decoding follows ref10 `ge_frombytes_vartime` exactly (see [`decompress`]); hashing to
//! the curve is a literal port of `ge_fromfe_frombytes_vartime` (see [`hash_to_ec`]). Scalars
//! are `curve25519_dalek::Scalar`s; every C++ `sc_*` call maps to scalar arithmetic modulo `l`
//! on canonical inputs.

use curve25519_dalek::edwards::{CompressedEdwardsY, EdwardsPoint};
use curve25519_dalek::scalar::Scalar;
use curve25519_dalek::traits::IsIdentity;
use rand_core::CryptoRng;
use xkr_types::{Hash, KeyDerivation, KeyImage, PublicKey, SecretKey, Signature};

use crate::field::{Fe, divpowm1};
use crate::keccak::cn_fast_hash;

/// C++ `ge_frombytes_vartime` (`crypto-ops.c`) as a validity predicate + decoder.
///
/// ref10 rejects (a) a non-canonical `y` (`y >= p`, bit 255 ignored) and (b) `x = 0` with the
/// sign bit set, and otherwise accepts every `y` with a square `(y^2 - 1)/(d y^2 + 1)`.
/// `CompressedEdwardsY::decompress` accepts both (a) and (b), so we additionally require the
/// re-encoding to be byte-identical, which rejects exactly those two cases.
#[must_use]
pub fn decompress(bytes: &[u8; 32]) -> Option<EdwardsPoint> {
    let point = CompressedEdwardsY(*bytes).decompress()?;
    (point.compress().to_bytes() == *bytes).then_some(point)
}

/// C++ `crypto_ops::check_key`: `ge_frombytes_vartime(key) == 0`.
#[must_use]
pub fn check_key(key: &PublicKey) -> bool {
    decompress(&key.0).is_some()
}

/// C++ `sc_check(s) == 0`: `s < l` as a 256-bit integer.
#[must_use]
pub fn sc_check(s: &[u8; 32]) -> bool {
    bool::from(Scalar::from_canonical_bytes(*s).is_some())
}

/// C++ `random_scalar`: 64 random bytes reduced mod `l` (`sc_reduce`).
pub fn random_scalar<R: CryptoRng + ?Sized>(rng: &mut R) -> Scalar {
    let mut wide = [0u8; 64];
    rng.fill_bytes(&mut wide);
    Scalar::from_bytes_mod_order_wide(&wide)
}

/// C++ `hash_to_scalar`: `sc_reduce32(cn_fast_hash(data))`.
#[must_use]
pub fn hash_to_scalar(data: &[u8]) -> Scalar {
    Scalar::from_bytes_mod_order(cn_fast_hash(data).0)
}

/// C++ `crypto_ops::generate_keys`.
pub fn generate_keys<R: CryptoRng + ?Sized>(rng: &mut R) -> (PublicKey, SecretKey) {
    let s = random_scalar(rng);
    let p = EdwardsPoint::mul_base(&s);
    (PublicKey(p.compress().to_bytes()), SecretKey(s.to_bytes()))
}

/// C++ `crypto_ops::secret_key_to_public_key`: `None` if `sec` is not a canonical scalar.
#[must_use]
pub fn secret_key_to_public_key(sec: &SecretKey) -> Option<PublicKey> {
    let s = Option::<Scalar>::from(Scalar::from_canonical_bytes(sec.0))?;
    Some(PublicKey(EdwardsPoint::mul_base(&s).compress().to_bytes()))
}

/// C++ `crypto_ops::generate_key_derivation`: `8 * sec * pub`.
///
/// C++ only `assert`s that `sec` is canonical (debug builds); this reduces `sec` mod `l`, which
/// gives the same result for every canonical key (and for every key below `2^255`, because the
/// cofactor multiplication removes the torsion component that reduction would change).
#[must_use]
pub fn generate_key_derivation(public: &PublicKey, sec: &SecretKey) -> Option<KeyDerivation> {
    let point = decompress(&public.0)?;
    let s = Scalar::from_bytes_mod_order(sec.0);
    Some(KeyDerivation(
        (point * s).mul_by_cofactor().compress().to_bytes(),
    ))
}

/// C++ `derivation_to_scalar`: `hash_to_scalar(derivation || varint(output_index))`.
#[must_use]
pub fn derivation_to_scalar(derivation: &KeyDerivation, output_index: u64) -> Scalar {
    let mut buf = [0u8; 32 + 10];
    buf[..32].copy_from_slice(&derivation.0);
    let mut len = 32;
    let mut i = output_index;
    while i >= 0x80 {
        buf[len] = (i as u8 & 0x7f) | 0x80;
        len += 1;
        i >>= 7;
    }
    buf[len] = i as u8;
    len += 1;
    hash_to_scalar(&buf[..len])
}

/// View tag of output `output_index` (transaction version 2, `docs/tx-v2.md`): Monero's
/// `crypto_ops::derive_view_tag`, the first byte of
/// `cn_fast_hash("view_tag" || derivation || varint(output_index))` (the salt is the 8 ASCII
/// bytes without a terminator). One Keccak per output instead of the point arithmetic of
/// [`derive_public_key`], so a scanning wallet skips ~255/256 of the outputs cheaply.
#[must_use]
pub fn view_tag(derivation: &KeyDerivation, output_index: u64) -> u8 {
    let mut buf = [0u8; 8 + 32 + 10];
    buf[..8].copy_from_slice(b"view_tag");
    buf[8..40].copy_from_slice(&derivation.0);
    let mut len = 40;
    let mut i = output_index;
    while i >= 0x80 {
        buf[len] = (i as u8 & 0x7f) | 0x80;
        len += 1;
        i >>= 7;
    }
    buf[len] = i as u8;
    len += 1;
    cn_fast_hash(&buf[..len]).0[0]
}

/// C++ `crypto_ops::derive_public_key`: `base + H_s(derivation, index) * G`.
#[must_use]
pub fn derive_public_key(
    derivation: &KeyDerivation,
    output_index: u64,
    base: &PublicKey,
) -> Option<PublicKey> {
    let b = decompress(&base.0)?;
    let s = derivation_to_scalar(derivation, output_index);
    Some(PublicKey(
        (b + EdwardsPoint::mul_base(&s)).compress().to_bytes(),
    ))
}

/// C++ `crypto_ops::underive_public_key`: `derived - H_s(derivation, index) * G`.
#[must_use]
pub fn underive_public_key(
    derivation: &KeyDerivation,
    output_index: u64,
    derived: &PublicKey,
) -> Option<PublicKey> {
    let d = decompress(&derived.0)?;
    let s = derivation_to_scalar(derivation, output_index);
    Some(PublicKey(
        (d - EdwardsPoint::mul_base(&s)).compress().to_bytes(),
    ))
}

/// C++ `crypto_ops::derive_secret_key`: `base + H_s(derivation, index)` mod `l`.
#[must_use]
pub fn derive_secret_key(
    derivation: &KeyDerivation,
    output_index: u64,
    base: &SecretKey,
) -> SecretKey {
    let s = derivation_to_scalar(derivation, output_index);
    SecretKey((Scalar::from_bytes_mod_order(base.0) + s).to_bytes())
}

/// Port of C++ `ge_fromfe_frombytes_vartime` (`crypto-ops.c`), returning the projective
/// `(X, Y, Z)` of the `ge_p2` result. Statement-for-statement translation; the comments are the
/// C++ ones.
fn ge_fromfe_frombytes_vartime(s: &[u8; 32]) -> (Fe, Fe, Fe) {
    // fe_ma2 = -A^2, fe_ma = -A, A = 486662; fe_fffb1..4 and sqrt(-1) from crypto-ops-data.c.
    const FE_MA2: Fe = Fe::from_limbs([
        0xffff_ffc8_db3d_e3c9,
        u64::MAX,
        u64::MAX,
        0x7fff_ffff_ffff_ffff,
    ]);
    const FE_MA: Fe = Fe::from_limbs([
        0xffff_ffff_fff8_92e7,
        u64::MAX,
        u64::MAX,
        0x7fff_ffff_ffff_ffff,
    ]);
    const FE_SQRTM1: Fe = Fe::from_limbs([
        0xc4ee_1b27_4a0e_a0b0,
        0x2f43_1806_ad2f_e478,
        0x2b4d_0099_3dfb_d7a7,
        0x2b83_2480_4fc1_df0b,
    ]);
    const FE_FFFB1: Fe = Fe::from_limbs([
        0x22a7_6975_321c_41ee,
        0xd194_04a8_b92a_738d,
        0x20a9_c537_41fb_19e3,
        0x7e71_fbef_dad6_1b17,
    ]);
    const FE_FFFB2: Fe = Fe::from_limbs([
        0xd2de_6483_607c_9ae0,
        0x51de_0346_0456_f7df,
        0x91d4_51b7_c016_5fbe,
        0x4d06_1e0a_045a_2cf6,
    ]);
    const FE_FFFB3: Fe = Fe::from_limbs([
        0x581b_7d87_1730_2c66,
        0x4024_ff4e_a596_4229,
        0xb895_4640_3f0d_a2ed,
        0x674a_110d_14c2_08ef,
    ]);
    const FE_FFFB4: Fe = Fe::from_limbs([
        0x853d_1903_b6b3_9186,
        0xee46_fc08_a13f_4a49,
        0x26c0_f488_7ef7_432e,
        0x1a43_f303_1067_dbf9,
    ]);

    let u = Fe::from_bytes_wide(s);
    let v = u.square().add(u.square()); // 2 * u^2
    let w = v.add(Fe::ONE); // w = 2 * u^2 + 1
    let mut x = w.square(); // w^2
    let mut y = FE_MA2.mul(v); // -2 * A^2 * u^2
    x = x.add(y); // x = w^2 - 2 * A^2 * u^2
    let mut rx = divpowm1(w, x); // (w / x)^(m + 1)
    y = rx.square();
    x = y.mul(x);
    y = w.sub(x);
    let mut z = FE_MA;
    let sign;
    let mut negative = false;
    if y.is_nonzero() {
        y = w.add(x);
        if y.is_nonzero() {
            negative = true;
        } else {
            rx = rx.mul(FE_FFFB1);
        }
    } else {
        rx = rx.mul(FE_FFFB2);
    }
    if negative {
        x = x.mul(FE_SQRTM1);
        y = w.sub(x);
        if y.is_nonzero() {
            // C++ asserts w + x == 0 here.
            rx = rx.mul(FE_FFFB3);
        } else {
            rx = rx.mul(FE_FFFB4);
        }
        // r->X = sqrt(A * (A + 2) * w / x), z = -A
        sign = true;
    } else {
        rx = rx.mul(u); // u * sqrt(2 * A * (A + 2) * w / x)
        z = z.mul(v); // -2 * A * u^2
        sign = false;
    }
    if rx.is_negative() != sign {
        rx = rx.neg();
    }
    let rz = z.add(w);
    let ry = z.sub(w);
    let rx = rx.mul(rz);
    (rx, ry, rz)
}

/// `ge_tobytes` of a projective point given as `(X, Y, Z)`.
fn p2_to_bytes(x: Fe, y: Fe, z: Fe) -> [u8; 32] {
    let zi = z.invert();
    let ax = x.mul(zi);
    let ay = y.mul(zi);
    let mut out = ay.to_bytes();
    out[31] ^= u8::from(ax.is_negative()) << 7;
    out
}

/// `ge_fromfe_frombytes_vartime(s)` followed by `ge_mul8` (C++ `hash_to_ec` minus the hash).
fn map_to_point(s: &[u8; 32]) -> EdwardsPoint {
    let (x, y, z) = ge_fromfe_frombytes_vartime(s);
    // The map always lands on the curve (C++ checks this in debug builds), so the canonical
    // encoding always decompresses; the identity fallback is unreachable but avoids a panic.
    CompressedEdwardsY(p2_to_bytes(x, y, z))
        .decompress()
        .map_or_else(EdwardsPoint::default, |p| p.mul_by_cofactor())
}

/// C++ `hash_to_ec(key)`: `8 * ge_fromfe_frombytes_vartime(cn_fast_hash(key))`.
#[must_use]
pub fn hash_to_ec(key: &PublicKey) -> EdwardsPoint {
    map_to_point(&cn_fast_hash(&key.0).0)
}

/// C++ `crypto_ops::hash_data_to_ec`: same map applied to `cn_fast_hash(data)`, compressed.
#[must_use]
pub fn hash_data_to_ec(data: &[u8]) -> PublicKey {
    PublicKey(map_to_point(&cn_fast_hash(data).0).compress().to_bytes())
}

/// C++ `crypto_ops::generate_key_image`: `sec * hash_to_ec(pub)`.
#[must_use]
pub fn generate_key_image(public: &PublicKey, sec: &SecretKey) -> KeyImage {
    let hp = hash_to_ec(public);
    KeyImage(
        (hp * Scalar::from_bytes_mod_order(sec.0))
            .compress()
            .to_bytes(),
    )
}

/// The key-image domain check of C++ `Core::validateSemantic`:
/// `scalarmultKey(keyImage, L) == I`, i.e. `l * KI` is the identity, i.e. the key image lies in
/// the prime-order subgroup. `ge_check_subgroup_precomp_vartime` in `checkRingSignature` is the
/// same predicate (an addition chain for `l * P` followed by a `y == 1` test).
///
/// `curve25519-dalek`'s `is_torsion_free` computes exactly `l * P == identity`; we spell it out
/// as `(l - 1) * P + P` because `l` itself is not a representable (reduced) `Scalar`. dalek's
/// variable-base multiplication multiplies by the integer value of the canonical scalar, so
/// this is the full-group multiple `l * P`.
///
/// A key image that does not decode returns `false` (the transaction is rejected).
#[must_use]
pub fn key_image_in_prime_subgroup(image: &KeyImage) -> bool {
    decompress(&image.0).is_some_and(|p| point_in_prime_subgroup(&p))
}

/// `l * P == identity`, computed as `(l - 1) * P + P` (see [`key_image_in_prime_subgroup`]).
#[must_use]
pub fn point_in_prime_subgroup(p: &EdwardsPoint) -> bool {
    let l_minus_one = -Scalar::ONE;
    (p * l_minus_one + p).is_identity()
}

/// Message buffer of C++ `generate_signature` / `check_signature` (`s_comm`).
fn signature_challenge(prefix_hash: &Hash, public: &PublicKey, comm: &[u8; 32]) -> Scalar {
    let mut buf = [0u8; 96];
    buf[..32].copy_from_slice(&prefix_hash.0);
    buf[32..64].copy_from_slice(&public.0);
    buf[64..].copy_from_slice(comm);
    hash_to_scalar(&buf)
}

/// C++ `crypto_ops::generate_signature` (Schnorr signature used by e.g. reserve proofs).
pub fn generate_signature<R: CryptoRng + ?Sized>(
    prefix_hash: &Hash,
    public: &PublicKey,
    sec: &SecretKey,
    rng: &mut R,
) -> Signature {
    let k = random_scalar(rng);
    let comm = EdwardsPoint::mul_base(&k).compress().to_bytes();
    let c = signature_challenge(prefix_hash, public, &comm);
    let r = k - c * Scalar::from_bytes_mod_order(sec.0);
    Signature::from_parts(&c.to_bytes(), &r.to_bytes())
}

/// C++ `crypto_ops::check_signature`.
#[must_use]
pub fn check_signature(prefix_hash: &Hash, public: &PublicKey, sig: &Signature) -> bool {
    let Some(p) = decompress(&public.0) else {
        return false;
    };
    let (Some(c), Some(r)) = (canonical(&sig.c()), canonical(&sig.r())) else {
        return false;
    };
    let comm = EdwardsPoint::vartime_double_scalar_mul_basepoint(&c, &p, &r)
        .compress()
        .to_bytes();
    let c2 = signature_challenge(prefix_hash, public, &comm);
    c2 - c == Scalar::ZERO
}

/// `sc_check` + decode.
pub(crate) fn canonical(bytes: &[u8; 32]) -> Option<Scalar> {
    Scalar::from_canonical_bytes(*bytes).into()
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestRng(u64);
    impl rand_core::RngCore for TestRng {
        fn next_u32(&mut self) -> u32 {
            self.next_u64() as u32
        }
        fn next_u64(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
        fn fill_bytes(&mut self, dst: &mut [u8]) {
            for b in dst {
                *b = self.next_u64() as u8;
            }
        }
    }
    impl rand_core::CryptoRng for TestRng {}

    #[test]
    fn decompress_rejects_non_canonical_encodings() {
        // identity, canonical
        let mut id = [0u8; 32];
        id[0] = 1;
        assert!(decompress(&id).is_some());
        // identity with the sign bit set (x = 0, sign 1): ref10 rejects
        let mut id_neg = id;
        id_neg[31] |= 0x80;
        assert!(decompress(&id_neg).is_none());
        // y = p + 1 (non-canonical encoding of the identity): ref10 rejects
        let mut y = [0xffu8; 32];
        y[0] = 0xee;
        y[31] = 0x7f;
        assert!(decompress(&y).is_none());
    }

    #[test]
    fn key_derivation_roundtrip() {
        let mut rng = TestRng(0x1234_5678_9abc_def1);
        let (view_pub, view_sec) = generate_keys(&mut rng);
        let (spend_pub, spend_sec) = generate_keys(&mut rng);
        let (tx_pub, tx_sec) = generate_keys(&mut rng);
        assert_eq!(secret_key_to_public_key(&view_sec), Some(view_pub));

        let d_sender = generate_key_derivation(&view_pub, &tx_sec).unwrap();
        let d_receiver = generate_key_derivation(&tx_pub, &view_sec).unwrap();
        assert_eq!(d_sender, d_receiver);

        for index in [0u64, 1, 127, 128, 300] {
            let out = derive_public_key(&d_sender, index, &spend_pub).unwrap();
            let out_sec = derive_secret_key(&d_receiver, index, &spend_sec);
            assert_eq!(secret_key_to_public_key(&out_sec), Some(out));
            assert_eq!(
                underive_public_key(&d_receiver, index, &out),
                Some(spend_pub)
            );
            let ki = generate_key_image(&out, &out_sec);
            assert!(key_image_in_prime_subgroup(&ki));
        }
    }

    #[test]
    fn torsioned_key_image_is_rejected() {
        let mut rng = TestRng(42);
        let (p, s) = generate_keys(&mut rng);
        let ki = generate_key_image(&p, &s);
        let point = decompress(&ki.0).unwrap();
        let torsion = curve25519_dalek::constants::EIGHT_TORSION[1];
        let bad = KeyImage((point + torsion).compress().to_bytes());
        assert!(!key_image_in_prime_subgroup(&bad));
        assert!(key_image_in_prime_subgroup(&ki));
    }

    #[test]
    fn hash_to_ec_lands_in_prime_subgroup() {
        for i in 0u8..20 {
            let p = hash_to_ec(&PublicKey([i; 32]));
            assert!(p.is_torsion_free());
            assert!(!p.is_identity());
        }
    }

    /// All 70 `derive_view_tag` vectors of Monero's `tests/crypto/tests.txt`.
    #[test]
    fn monero_view_tag_vectors() {
        const VECTORS: [(&str, u64, u8); 70] = [
            (
                "0fc47054f355ced4d67de73bfa12e4c78ff19089548fffa7d07a674741860f97",
                0,
                0x76,
            ),
            (
                "0fc47054f355ced4d67de73bfa12e4c78ff19089548fffa7d07a674741860f97",
                1,
                0xd6,
            ),
            (
                "0fc47054f355ced4d67de73bfa12e4c78ff19089548fffa7d07a674741860f97",
                2,
                0x87,
            ),
            (
                "0fc47054f355ced4d67de73bfa12e4c78ff19089548fffa7d07a674741860f97",
                3,
                0x1b,
            ),
            (
                "0fc47054f355ced4d67de73bfa12e4c78ff19089548fffa7d07a674741860f97",
                12,
                0xd6,
            ),
            (
                "0fc47054f355ced4d67de73bfa12e4c78ff19089548fffa7d07a674741860f97",
                13,
                0xe9,
            ),
            (
                "0fc47054f355ced4d67de73bfa12e4c78ff19089548fffa7d07a674741860f97",
                14,
                0x12,
            ),
            (
                "0fc47054f355ced4d67de73bfa12e4c78ff19089548fffa7d07a674741860f97",
                15,
                0x26,
            ),
            (
                "0fc47054f355ced4d67de73bfa12e4c78ff19089548fffa7d07a674741860f97",
                127,
                0x2c,
            ),
            (
                "0fc47054f355ced4d67de73bfa12e4c78ff19089548fffa7d07a674741860f97",
                128,
                0x11,
            ),
            (
                "a36ba7b4d31349ad278a6df8f77adb76748b59f4929348e67dd92adb9fa174dc",
                0,
                0x70,
            ),
            (
                "a36ba7b4d31349ad278a6df8f77adb76748b59f4929348e67dd92adb9fa174dc",
                1,
                0x81,
            ),
            (
                "a36ba7b4d31349ad278a6df8f77adb76748b59f4929348e67dd92adb9fa174dc",
                2,
                0xa0,
            ),
            (
                "a36ba7b4d31349ad278a6df8f77adb76748b59f4929348e67dd92adb9fa174dc",
                3,
                0xec,
            ),
            (
                "a36ba7b4d31349ad278a6df8f77adb76748b59f4929348e67dd92adb9fa174dc",
                12,
                0x22,
            ),
            (
                "a36ba7b4d31349ad278a6df8f77adb76748b59f4929348e67dd92adb9fa174dc",
                13,
                0x0a,
            ),
            (
                "a36ba7b4d31349ad278a6df8f77adb76748b59f4929348e67dd92adb9fa174dc",
                14,
                0x87,
            ),
            (
                "a36ba7b4d31349ad278a6df8f77adb76748b59f4929348e67dd92adb9fa174dc",
                15,
                0x76,
            ),
            (
                "a36ba7b4d31349ad278a6df8f77adb76748b59f4929348e67dd92adb9fa174dc",
                127,
                0x4b,
            ),
            (
                "a36ba7b4d31349ad278a6df8f77adb76748b59f4929348e67dd92adb9fa174dc",
                128,
                0x39,
            ),
            (
                "7498d5bf0b69e08653f6d420a17f866dd2bd490ab43074f46065cb501fe7e2d8",
                0,
                0x93,
            ),
            (
                "7498d5bf0b69e08653f6d420a17f866dd2bd490ab43074f46065cb501fe7e2d8",
                1,
                0x67,
            ),
            (
                "7498d5bf0b69e08653f6d420a17f866dd2bd490ab43074f46065cb501fe7e2d8",
                2,
                0x9d,
            ),
            (
                "7498d5bf0b69e08653f6d420a17f866dd2bd490ab43074f46065cb501fe7e2d8",
                3,
                0x2d,
            ),
            (
                "7498d5bf0b69e08653f6d420a17f866dd2bd490ab43074f46065cb501fe7e2d8",
                12,
                0x63,
            ),
            (
                "7498d5bf0b69e08653f6d420a17f866dd2bd490ab43074f46065cb501fe7e2d8",
                13,
                0xcf,
            ),
            (
                "7498d5bf0b69e08653f6d420a17f866dd2bd490ab43074f46065cb501fe7e2d8",
                14,
                0xef,
            ),
            (
                "7498d5bf0b69e08653f6d420a17f866dd2bd490ab43074f46065cb501fe7e2d8",
                15,
                0x10,
            ),
            (
                "7498d5bf0b69e08653f6d420a17f866dd2bd490ab43074f46065cb501fe7e2d8",
                127,
                0xfc,
            ),
            (
                "7498d5bf0b69e08653f6d420a17f866dd2bd490ab43074f46065cb501fe7e2d8",
                128,
                0x73,
            ),
            (
                "fe7770c4b076e95ddb8026affcfab39d31c7c4a2266e0e25e343bc4badc907d0",
                0,
                0x90,
            ),
            (
                "fe7770c4b076e95ddb8026affcfab39d31c7c4a2266e0e25e343bc4badc907d0",
                1,
                0x5a,
            ),
            (
                "fe7770c4b076e95ddb8026affcfab39d31c7c4a2266e0e25e343bc4badc907d0",
                2,
                0xde,
            ),
            (
                "fe7770c4b076e95ddb8026affcfab39d31c7c4a2266e0e25e343bc4badc907d0",
                3,
                0x21,
            ),
            (
                "fe7770c4b076e95ddb8026affcfab39d31c7c4a2266e0e25e343bc4badc907d0",
                12,
                0x57,
            ),
            (
                "fe7770c4b076e95ddb8026affcfab39d31c7c4a2266e0e25e343bc4badc907d0",
                13,
                0x52,
            ),
            (
                "fe7770c4b076e95ddb8026affcfab39d31c7c4a2266e0e25e343bc4badc907d0",
                14,
                0x6f,
            ),
            (
                "fe7770c4b076e95ddb8026affcfab39d31c7c4a2266e0e25e343bc4badc907d0",
                15,
                0xeb,
            ),
            (
                "fe7770c4b076e95ddb8026affcfab39d31c7c4a2266e0e25e343bc4badc907d0",
                127,
                0x93,
            ),
            (
                "fe7770c4b076e95ddb8026affcfab39d31c7c4a2266e0e25e343bc4badc907d0",
                128,
                0xdc,
            ),
            (
                "ea9337d0ddf480abdc4fc56a0cb223702729cb230ae7b9de50243ad25ce90e8d",
                0,
                0xc6,
            ),
            (
                "ea9337d0ddf480abdc4fc56a0cb223702729cb230ae7b9de50243ad25ce90e8d",
                1,
                0x60,
            ),
            (
                "ea9337d0ddf480abdc4fc56a0cb223702729cb230ae7b9de50243ad25ce90e8d",
                2,
                0xf0,
            ),
            (
                "ea9337d0ddf480abdc4fc56a0cb223702729cb230ae7b9de50243ad25ce90e8d",
                3,
                0x71,
            ),
            (
                "ea9337d0ddf480abdc4fc56a0cb223702729cb230ae7b9de50243ad25ce90e8d",
                12,
                0x0e,
            ),
            (
                "ea9337d0ddf480abdc4fc56a0cb223702729cb230ae7b9de50243ad25ce90e8d",
                13,
                0x42,
            ),
            (
                "ea9337d0ddf480abdc4fc56a0cb223702729cb230ae7b9de50243ad25ce90e8d",
                14,
                0xb2,
            ),
            (
                "ea9337d0ddf480abdc4fc56a0cb223702729cb230ae7b9de50243ad25ce90e8d",
                15,
                0x61,
            ),
            (
                "ea9337d0ddf480abdc4fc56a0cb223702729cb230ae7b9de50243ad25ce90e8d",
                127,
                0xc3,
            ),
            (
                "ea9337d0ddf480abdc4fc56a0cb223702729cb230ae7b9de50243ad25ce90e8d",
                128,
                0x0a,
            ),
            (
                "25d538315bcb81aff9574189ea65f418aeb0392f5cbbc84cd8a33c7ade31ef0a",
                0,
                0x4c,
            ),
            (
                "25d538315bcb81aff9574189ea65f418aeb0392f5cbbc84cd8a33c7ade31ef0a",
                1,
                0x9b,
            ),
            (
                "25d538315bcb81aff9574189ea65f418aeb0392f5cbbc84cd8a33c7ade31ef0a",
                2,
                0x64,
            ),
            (
                "25d538315bcb81aff9574189ea65f418aeb0392f5cbbc84cd8a33c7ade31ef0a",
                3,
                0xff,
            ),
            (
                "25d538315bcb81aff9574189ea65f418aeb0392f5cbbc84cd8a33c7ade31ef0a",
                12,
                0xe3,
            ),
            (
                "25d538315bcb81aff9574189ea65f418aeb0392f5cbbc84cd8a33c7ade31ef0a",
                13,
                0x24,
            ),
            (
                "25d538315bcb81aff9574189ea65f418aeb0392f5cbbc84cd8a33c7ade31ef0a",
                14,
                0xea,
            ),
            (
                "25d538315bcb81aff9574189ea65f418aeb0392f5cbbc84cd8a33c7ade31ef0a",
                15,
                0x3b,
            ),
            (
                "25d538315bcb81aff9574189ea65f418aeb0392f5cbbc84cd8a33c7ade31ef0a",
                127,
                0x47,
            ),
            (
                "25d538315bcb81aff9574189ea65f418aeb0392f5cbbc84cd8a33c7ade31ef0a",
                128,
                0x5a,
            ),
            (
                "8edfabada2b24ef4d8d915826c9ff0245910e4b835b59c2cf8ed8fc991b2e1e8",
                0,
                0x74,
            ),
            (
                "8edfabada2b24ef4d8d915826c9ff0245910e4b835b59c2cf8ed8fc991b2e1e8",
                1,
                0x77,
            ),
            (
                "8edfabada2b24ef4d8d915826c9ff0245910e4b835b59c2cf8ed8fc991b2e1e8",
                2,
                0xa9,
            ),
            (
                "8edfabada2b24ef4d8d915826c9ff0245910e4b835b59c2cf8ed8fc991b2e1e8",
                3,
                0x44,
            ),
            (
                "8edfabada2b24ef4d8d915826c9ff0245910e4b835b59c2cf8ed8fc991b2e1e8",
                12,
                0x75,
            ),
            (
                "8edfabada2b24ef4d8d915826c9ff0245910e4b835b59c2cf8ed8fc991b2e1e8",
                13,
                0x05,
            ),
            (
                "8edfabada2b24ef4d8d915826c9ff0245910e4b835b59c2cf8ed8fc991b2e1e8",
                14,
                0xca,
            ),
            (
                "8edfabada2b24ef4d8d915826c9ff0245910e4b835b59c2cf8ed8fc991b2e1e8",
                15,
                0x00,
            ),
            (
                "8edfabada2b24ef4d8d915826c9ff0245910e4b835b59c2cf8ed8fc991b2e1e8",
                127,
                0xa6,
            ),
            (
                "8edfabada2b24ef4d8d915826c9ff0245910e4b835b59c2cf8ed8fc991b2e1e8",
                128,
                0x0d,
            ),
        ];
        for (derivation, index, tag) in VECTORS {
            let d: KeyDerivation = derivation.parse().unwrap();
            assert_eq!(view_tag(&d, index), tag, "{derivation} {index}");
        }
    }

    #[test]
    fn schnorr_signature_roundtrip() {
        let mut rng = TestRng(7);
        let (p, s) = generate_keys(&mut rng);
        let msg = cn_fast_hash(b"xkr");
        let sig = generate_signature(&msg, &p, &s, &mut rng);
        assert!(check_signature(&msg, &p, &sig));
        assert!(!check_signature(&cn_fast_hash(b"other"), &p, &sig));
    }
}
