//! `findPowShare`: found shares pass the relay's own share check (`xkr_hugin::pow`), the
//! search is reproducible with one worker, and unusable input gives `None` (JavaScript `null`).

use xkr_crypto_ffi::pow::{self, MAX_POW_ATTEMPTS, find_pow_share};
use xkr_hugin::pow::{PowJob, verify_share};

/// A block hashing blob: major 7, minor 0, a 5-byte timestamp varint, prev id, nonce, Merkle
/// root, transaction count.
fn blob() -> String {
    let mut b = vec![7u8, 0, 0xfb, 0x8e, 0x8a, 0xc8, 0x05];
    b.extend_from_slice(&[0x11; 32]);
    b.extend_from_slice(&[0, 0, 0, 0]);
    b.extend_from_slice(&[0x22; 32]);
    b.push(1);
    hex::encode(b)
}

fn job(target: &str) -> PowJob {
    PowJob {
        job_id: "1".to_owned(),
        blob: hex::decode(blob()).unwrap(),
        target: target.to_owned(),
    }
}

#[test]
fn shares_verify_with_the_relay_check() {
    // raw 0x3fffffff: difficulty 4, about one hash in four meets it
    let target = "ffffff3f";
    for threads in [1, 2, 3] {
        let search = find_pow_share(&blob(), target, 1000, 400, threads).unwrap();
        assert_eq!(search.threads, threads);
        let share = search.share.expect("a share within 400 attempts");
        assert!(search.hashes_performed >= 1);
        assert!(verify_share(&job(target), &share.nonce, &share.result));
    }
}

#[test]
fn one_worker_is_deterministic() {
    let target = "ffffff3f";
    let a = find_pow_share(&blob(), target, 5, 400, 1).unwrap();
    let b = find_pow_share(&blob(), target, 5, 400, 1).unwrap();
    assert_eq!(a, b);
    let share = a.share.unwrap();
    // the winning nonce is the first one from 5 that meets the target
    let nonce = u32::from_le_bytes(hex::decode(&share.nonce).unwrap().try_into().unwrap());
    assert_eq!(u64::from(nonce - 5) + 1, a.hashes_performed);
}

#[test]
fn no_share_reports_the_work_done() {
    // raw 1: the target is u64::MAX / 0xffffffff, about one hash in 4 billion
    let search = find_pow_share(&blob(), "01000000", 0, 3, 1).unwrap();
    assert_eq!(search.share, None);
    assert_eq!(search.hashes_performed, 3);
}

#[test]
fn attempts_are_clamped_and_split_over_workers() {
    let search = find_pow_share(&blob(), "01000000", 0, 0, 1).unwrap();
    assert_eq!(search.hashes_performed, 1);
    // 5 attempts over 2 workers: ceil(5 / 2) = 3 each
    let search = find_pow_share(&blob(), "01000000", 0, 5, 2).unwrap();
    assert_eq!(search.hashes_performed, 6);
    // and capped at 5 million
    assert_eq!(MAX_POW_ATTEMPTS, 5_000_000);
}

#[test]
fn unusable_input_is_none() {
    let b = blob();
    assert_eq!(find_pow_share("", "ffffffff", 0, 1, 1), None);
    assert_eq!(
        find_pow_share(&"00".repeat(1025), "ffffffff", 0, 1, 1),
        None
    );
    assert_eq!(find_pow_share("zz", "ffffffff", 0, 1, 1), None);
    assert_eq!(find_pow_share("0", "ffffffff", 0, 1, 1), None);
    // too short for the nonce
    assert_eq!(find_pow_share(&b[..80], "ffffffff", 0, 1, 1), None);
    assert_eq!(find_pow_share(&b, "00000000", 0, 1, 1), None);
    assert_eq!(find_pow_share(&b, "ffff", 0, 1, 1), None);
    assert_eq!(find_pow_share(&b, "ffffffffffffffff", 0, 1, 1), None);
    assert_eq!(pow::target_value("FFFFFFFF"), Some(u64::MAX));
}
