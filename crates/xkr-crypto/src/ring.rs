//! Legacy CryptoNote ring signatures (`crypto_ops::generateRingSignatures` /
//! `crypto_ops::checkRingSignature` in `src/crypto/crypto.cpp`).
//!
//! Signature element `i` is `(c_i, r_i)`; with `P_i` the ring keys, `I` the key image and
//! `H_p` = [`hash_to_ec`]:
//!
//! ```text
//! L_i = c_i * P_i + r_i * G
//! R_i = r_i * H_p(P_i) + c_i * I
//! valid  <=>  H_s(prefix_hash || L_0 || R_0 || ... ) == sum(c_i)   (mod l)
//! ```

use curve25519_dalek::edwards::EdwardsPoint;
use curve25519_dalek::scalar::Scalar;
use curve25519_dalek::traits::VartimeMultiscalarMul;
use rand_core::CryptoRng;
use xkr_types::{Hash, KeyImage, PublicKey, SecretKey, Signature};

use crate::ops::{
    CheckedKeyImage, canonical, decompress, hash_to_ec, hash_to_scalar, random_scalar,
};

/// One ring-signature verification job (data-oriented batch input).
#[derive(Debug, Clone, Copy)]
pub struct RingSignatureCheck<'a> {
    /// Transaction prefix hash (the signed message).
    pub prefix_hash: &'a Hash,
    /// Key image of the input.
    pub key_image: &'a KeyImage,
    /// Ring member output keys, in ring order.
    pub ring: &'a [PublicKey],
    /// One signature element per ring member.
    pub signatures: &'a [Signature],
}

/// C++ `crypto_ops::checkRingSignature`.
///
/// Check order and rejections follow the C++ code: the key image must decode
/// (`ge_frombytes_vartime`) and lie in the prime-order subgroup
/// (`ge_check_subgroup_precomp_vartime`); every `c_i`, `r_i` must be canonical (`sc_check`) and
/// every ring key must decode.
///
/// One signature per ring member is what the deserializer produces. A shorter `sigs` returns
/// `false`; extra signatures beyond `pubs.len()` are ignored, as in C++. Consensus code checks
/// the count separately (`TRANSACTION_SIGNATURE_COUNT_VALIDATION_HEIGHT`).
#[must_use]
pub fn check_ring_signature(
    prefix_hash: &Hash,
    image: &KeyImage,
    pubs: &[PublicKey],
    sigs: &[Signature],
) -> bool {
    if sigs.len() < pubs.len() {
        return false;
    }
    CheckedKeyImage::new(image)
        .is_some_and(|image| check_ring_signature_checked(prefix_hash, &image, pubs, sigs))
}

/// [`check_ring_signature`] with the key image decoded and in the prime-order subgroup
/// ([`CheckedKeyImage`]), for callers that checked it while validating the transaction.
#[must_use]
pub fn check_ring_signature_checked(
    prefix_hash: &Hash,
    image: &CheckedKeyImage,
    pubs: &[PublicKey],
    sigs: &[Signature],
) -> bool {
    if sigs.len() < pubs.len() {
        return false;
    }
    let image_point = *image.point();
    let mut buf = Vec::with_capacity(32 + 64 * pubs.len());
    buf.extend_from_slice(&prefix_hash.0);
    let mut sum = Scalar::ZERO;
    for (public, sig) in pubs.iter().zip(sigs) {
        let (Some(c), Some(r)) = (canonical(&sig.c()), canonical(&sig.r())) else {
            return false;
        };
        let Some(p) = decompress(&public.0) else {
            return false;
        };
        let l = EdwardsPoint::vartime_double_scalar_mul_basepoint(&c, &p, &r);
        buf.extend_from_slice(l.compress().as_bytes());
        let hp = hash_to_ec(public);
        let rr = EdwardsPoint::vartime_multiscalar_mul([r, c], [hp, image_point]);
        buf.extend_from_slice(rr.compress().as_bytes());
        sum += c;
    }
    let h = hash_to_scalar(&buf);
    h - sum == Scalar::ZERO
}

/// Checks a batch; `true` only if every job verifies. Sequential; callers parallelise over
/// transactions (e.g. with rayon) since jobs are independent.
#[must_use]
pub fn check_ring_signatures(jobs: &[RingSignatureCheck<'_>]) -> bool {
    jobs.iter()
        .all(|j| check_ring_signature(j.prefix_hash, j.key_image, j.ring, j.signatures))
}

/// C++ `crypto_ops::generateRingSignatures`. Returns `None` where C++ returns `false` (key image
/// or a decoy key does not decode) and for an out-of-range `real_index`.
pub fn generate_ring_signature<R: CryptoRng + ?Sized>(
    prefix_hash: &Hash,
    image: &KeyImage,
    pubs: &[PublicKey],
    sec: &SecretKey,
    real_index: usize,
    rng: &mut R,
) -> Option<Vec<Signature>> {
    if real_index >= pubs.len() {
        return None;
    }
    let image_point = decompress(&image.0)?;
    let mut sigs = vec![Signature::ZERO; pubs.len()];
    let mut buf = Vec::with_capacity(32 + 64 * pubs.len());
    buf.extend_from_slice(&prefix_hash.0);
    let mut sum = Scalar::ZERO;
    let mut k = Scalar::ZERO;

    for (i, public) in pubs.iter().enumerate() {
        if i == real_index {
            k = random_scalar(rng);
            let l = EdwardsPoint::mul_base(&k);
            buf.extend_from_slice(l.compress().as_bytes());
            let rr = hash_to_ec(public) * k;
            buf.extend_from_slice(rr.compress().as_bytes());
        } else {
            let c = random_scalar(rng);
            let r = random_scalar(rng);
            sigs[i] = Signature::from_parts(&c.to_bytes(), &r.to_bytes());
            let p = decompress(&public.0)?;
            let l = EdwardsPoint::vartime_double_scalar_mul_basepoint(&c, &p, &r);
            buf.extend_from_slice(l.compress().as_bytes());
            let rr =
                EdwardsPoint::vartime_multiscalar_mul([r, c], [hash_to_ec(public), image_point]);
            buf.extend_from_slice(rr.compress().as_bytes());
            sum += c;
        }
    }

    let h = hash_to_scalar(&buf);
    let c_real = h - sum;
    let r_real = k - c_real * Scalar::from_bytes_mod_order(sec.0);
    sigs[real_index] = Signature::from_parts(&c_real.to_bytes(), &r_real.to_bytes());
    Some(sigs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ops::{generate_key_image, generate_keys};

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
    fn sign_and_verify_rings_of_every_size() {
        let mut rng = TestRng(0xdead_beef_cafe_f00d);
        for ring_size in 1..=8usize {
            let keys: Vec<_> = (0..ring_size).map(|_| generate_keys(&mut rng)).collect();
            let pubs: Vec<PublicKey> = keys.iter().map(|k| k.0).collect();
            for real in 0..ring_size {
                let image = generate_key_image(&pubs[real], &keys[real].1);
                let msg = crate::keccak::cn_fast_hash(&[ring_size as u8, real as u8]);
                let sigs =
                    generate_ring_signature(&msg, &image, &pubs, &keys[real].1, real, &mut rng)
                        .unwrap();
                assert!(check_ring_signature(&msg, &image, &pubs, &sigs));
                let checked = CheckedKeyImage::new(&image).unwrap();
                assert!(check_ring_signature_checked(&msg, &checked, &pubs, &sigs));

                // wrong message
                let other = crate::keccak::cn_fast_hash(b"other");
                assert!(!check_ring_signature(&other, &image, &pubs, &sigs));
                assert!(!check_ring_signature_checked(
                    &other, &checked, &pubs, &sigs
                ));
                // altered signature
                let mut bad = sigs.clone();
                bad[0].0[40] ^= 1;
                assert!(!check_ring_signature(&msg, &image, &pubs, &bad));
                assert!(!check_ring_signature_checked(&msg, &checked, &pubs, &bad));
                // too few signatures
                assert!(!check_ring_signature(
                    &msg,
                    &image,
                    &pubs,
                    &sigs[..ring_size - 1]
                ));
                assert!(!check_ring_signature_checked(
                    &msg,
                    &checked,
                    &pubs,
                    &sigs[..ring_size - 1]
                ));
            }
        }
    }

    #[test]
    fn non_canonical_scalar_is_rejected() {
        let mut rng = TestRng(99);
        let (p, s) = generate_keys(&mut rng);
        let image = generate_key_image(&p, &s);
        let msg = crate::keccak::cn_fast_hash(b"m");
        let mut sigs = generate_ring_signature(&msg, &image, &[p], &s, 0, &mut rng).unwrap();
        // c + l is the same scalar mod l but not canonical
        let l: [u8; 32] = [
            0xed, 0xd3, 0xf5, 0x5c, 0x1a, 0x63, 0x12, 0x58, 0xd6, 0x9c, 0xf7, 0xa2, 0xde, 0xf9,
            0xde, 0x14, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x10,
        ];
        let c = sigs[0].c();
        let mut sum = [0u8; 32];
        let mut carry = 0u16;
        for i in 0..32 {
            let v = u16::from(c[i]) + u16::from(l[i]) + carry;
            sum[i] = v as u8;
            carry = v >> 8;
        }
        if carry == 0 && sum[31] < 0x80 {
            sigs[0] = Signature::from_parts(&sum, &sigs[0].r());
            assert!(!check_ring_signature(&msg, &image, &[p], &sigs));
        }
    }
}
