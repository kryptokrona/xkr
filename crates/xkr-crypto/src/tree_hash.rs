//! Merkle tree hashing, mirroring `src/crypto/tree-hash.c` exactly (including the
//! "largest power of two below `count`" layout and `tree_hash_from_branch` path bits).

use xkr_types::Hash;

use crate::keccak::cn_fast_hash_pair;

/// C++ `tree_hash`. The C++ code asserts `count > 0`; for an empty slice this returns
/// [`Hash::ZERO`] instead of invoking undefined behaviour.
#[must_use]
pub fn tree_hash(hashes: &[Hash]) -> Hash {
    let count = hashes.len();
    match count {
        0 => Hash::ZERO,
        1 => hashes[0],
        2 => cn_fast_hash_pair(&hashes[0], &hashes[1]),
        _ => {
            // cnt = largest power of two strictly smaller than count
            let mut cnt = count - 1;
            let mut i = 1u32;
            while i < usize::BITS {
                cnt |= cnt >> i;
                i <<= 1;
            }
            cnt &= !(cnt >> 1);

            let mut ints = vec![Hash::ZERO; cnt];
            let direct = 2 * cnt - count;
            ints[..direct].copy_from_slice(&hashes[..direct]);
            let mut i = direct;
            let mut j = direct;
            while j < cnt {
                ints[j] = cn_fast_hash_pair(&hashes[i], &hashes[i + 1]);
                i += 2;
                j += 1;
            }
            debug_assert_eq!(i, count);
            while cnt > 2 {
                cnt >>= 1;
                for j in 0..cnt {
                    let h = cn_fast_hash_pair(&ints[2 * j], &ints[2 * j + 1]);
                    ints[j] = h;
                }
            }
            cn_fast_hash_pair(&ints[0], &ints[1])
        }
    }
}

/// C++ `tree_depth`: `floor(log2(count))`, 0 for `count <= 1`.
#[must_use]
pub fn tree_depth(count: usize) -> usize {
    let mut count = count;
    let mut depth = 0usize;
    let mut i = usize::BITS as usize / 2; // sizeof(size_t) << 2
    while i > 0 {
        if count >> i > 0 {
            count >>= i;
            depth += i;
        }
        i >>= 1;
    }
    depth
}

/// C++ `tree_branch`: the merkle branch for `hashes[0]` (used to build parent-block
/// `base_transaction_branch`). Returns `tree_depth(hashes.len())` hashes; empty for an empty
/// input.
#[must_use]
pub fn tree_branch(hashes: &[Hash]) -> Vec<Hash> {
    let count = hashes.len();
    if count == 0 {
        return Vec::new();
    }
    let mut cnt = 1usize;
    let mut depth = 0usize;
    let mut i = usize::BITS as usize / 2;
    while i > 0 {
        if cnt << i <= count {
            cnt <<= i;
            depth += i;
        }
        i >>= 1;
    }
    let mut branch = vec![Hash::ZERO; depth];
    let mut ints = vec![Hash::ZERO; cnt.saturating_sub(1)];
    let direct = 2 * cnt - count - 1;
    ints[..direct].copy_from_slice(&hashes[1..=direct]);
    let mut i = 2 * cnt - count;
    let mut j = direct;
    while j + 1 < cnt {
        ints[j] = cn_fast_hash_pair(&hashes[i], &hashes[i + 1]);
        i += 2;
        j += 1;
    }
    debug_assert_eq!(i, count);
    while depth > 0 {
        cnt >>= 1;
        depth -= 1;
        branch[depth] = ints[0];
        let mut i = 1usize;
        let mut j = 0usize;
        while j + 1 < cnt {
            let h = cn_fast_hash_pair(&ints[i], &ints[i + 1]);
            ints[j] = h;
            i += 2;
            j += 1;
        }
    }
    branch
}

/// C++ `tree_hash_from_branch(branch, depth, leaf, path, root)` with `depth = branch.len()`.
///
/// `path = None` is the C++ `path == 0` case (leaf always on the left), used for the parent
/// block's miner-transaction branch. With `Some(path)`, bit `d` of `path` (byte `d >> 3`,
/// bit `d & 7`) puts the running hash on the right at depth `d`; C++ passes the genesis block
/// hash here when checking the aux-chain branch. Bits beyond 256 read as zero (C++ callers
/// reject branches longer than 256 first).
#[must_use]
pub fn tree_hash_from_branch(branch: &[Hash], leaf: &Hash, path: Option<&Hash>) -> Hash {
    let mut depth = branch.len();
    if depth == 0 {
        return *leaf;
    }
    let mut buffer = [Hash::ZERO; 2];
    let mut from_leaf = true;
    while depth > 0 {
        depth -= 1;
        let bit = path.is_some_and(|p| {
            p.0.get(depth >> 3)
                .is_some_and(|byte| byte & (1u8 << (depth & 7)) != 0)
        });
        let (leaf_idx, branch_idx) = if bit { (1, 0) } else { (0, 1) };
        if from_leaf {
            buffer[leaf_idx] = *leaf;
            from_leaf = false;
        } else {
            buffer[leaf_idx] = cn_fast_hash_pair(&buffer[0], &buffer[1]);
        }
        buffer[branch_idx] = branch[depth];
    }
    cn_fast_hash_pair(&buffer[0], &buffer[1])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(b: u8) -> Hash {
        Hash([b; 32])
    }

    #[test]
    fn depth() {
        assert_eq!(tree_depth(1), 0);
        assert_eq!(tree_depth(2), 1);
        assert_eq!(tree_depth(3), 1);
        assert_eq!(tree_depth(4), 2);
        assert_eq!(tree_depth(5), 2);
        assert_eq!(tree_depth(1 << 20), 20);
    }

    #[test]
    fn small_trees_by_definition() {
        let hs: Vec<Hash> = (1..=5).map(h).collect();
        assert_eq!(tree_hash(&hs[..1]), hs[0]);
        assert_eq!(tree_hash(&hs[..2]), cn_fast_hash_pair(&hs[0], &hs[1]));
        // count 3: cnt = 2, ints = [h0, H(h1,h2)]
        assert_eq!(
            tree_hash(&hs[..3]),
            cn_fast_hash_pair(&hs[0], &cn_fast_hash_pair(&hs[1], &hs[2]))
        );
        // count 5: cnt = 4, ints = [h0, h1, h2, H(h3,h4)]
        let l = cn_fast_hash_pair(&hs[0], &hs[1]);
        let r = cn_fast_hash_pair(&hs[2], &cn_fast_hash_pair(&hs[3], &hs[4]));
        assert_eq!(tree_hash(&hs), cn_fast_hash_pair(&l, &r));
    }

    #[test]
    fn branch_recomputes_root() {
        for n in 1..=17u8 {
            let hs: Vec<Hash> = (0..n).map(h).collect();
            let branch = tree_branch(&hs);
            assert_eq!(branch.len(), tree_depth(hs.len()), "n = {n}");
            assert_eq!(
                tree_hash_from_branch(&branch, &hs[0], None),
                tree_hash(&hs),
                "n = {n}"
            );
        }
    }
}
