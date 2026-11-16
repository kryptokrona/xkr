//! Mining jobs: the blob to hash, where its nonce goes, the target, and the proof-of-work
//! algorithm of its block major version (C++ `src/miner/miner.cpp` `Miner::workerFunc`,
//! `CachedBlock::getBlockLongHash`).

use std::sync::Arc;

use xkr_consensus::{PowVariant, pow_variant_for};
use xkr_crypto::check_hash;
use xkr_crypto::cryptonight::{AesImpl, CnHasher, CnParams, Variant};
use xkr_serialization::{EncodeError, aux_block_header_hash, block_pow_blob};
use xkr_types::extra::TX_EXTRA_MERGE_MINING_TAG;
use xkr_types::{Block, Hash};

/// The proof-of-work algorithm of a block major version (C++ `CachedBlock::getBlockLongHash`,
/// `xkr_consensus::pow_variant_for`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PowAlgo {
    /// `cn_slow_hash_v0`: CryptoNight (major 1-3).
    CnV0,
    /// `cn_lite_slow_hash_v1`: CryptoNight-Lite variant 1 (major 4).
    CnLiteV1,
    /// `cn_turtle_lite_slow_hash_v2`: CryptoNight-Turtle-Lite variant 2 (major 5 and later).
    CnTurtleLiteV2,
}

impl PowAlgo {
    /// The algorithm of blocks with `major` version; `None` for 0.
    #[must_use]
    pub fn for_major(major: u8) -> Option<Self> {
        Some(match pow_variant_for(major)? {
            PowVariant::CryptoNightV0 => Self::CnV0,
            PowVariant::CryptoNightLiteV1 => Self::CnLiteV1,
            PowVariant::CryptoNightTurtleLiteV2 => Self::CnTurtleLiteV2,
        })
    }

    /// The algorithm a stratum job names in its `algo` field: xmrig names (`cn/0`,
    /// `cn-lite/1`, `cn-pico/trtl`), p2pool's `cryptonight_turtle` and the names of
    /// [`PowAlgo::name`]; `None` for others.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        match name.trim().to_ascii_lowercase().as_str() {
            "cn/0" | "cryptonight/0" | "cryptonight_v0" => Some(Self::CnV0),
            "cn-lite/1" | "cryptonight-lite/1" | "cryptonight_lite_v1" => Some(Self::CnLiteV1),
            "cn-turtle-lite/2" | "cryptonight_turtle" | "cryptonight-turtle" | "cn-pico/trtl"
            | "cn/turtle" | "cn-turtle" | "cn_turtle" | "turtle" => Some(Self::CnTurtleLiteV2),
            _ => None,
        }
    }

    /// The algorithm of the block at `height` on `network` (its fork table).
    #[must_use]
    pub fn for_height(network: xkr_consensus::Network, height: u64) -> Self {
        let major = network.params().forks().block_major_version_for(height);
        Self::for_major(major).unwrap_or(Self::CURRENT)
    }

    /// The algorithm of new blocks on mainnet and testnet today (blocks from major 5 on).
    pub const CURRENT: Self = Self::CnTurtleLiteV2;

    /// A hasher for this algorithm with its own scratchpad.
    #[must_use]
    pub fn hasher(self, aes: AesImpl) -> CnHasher {
        match self {
            Self::CnV0 => CnHasher::with_aes(Variant::V0, CnParams::CN, aes),
            Self::CnLiteV1 => CnHasher::with_aes(Variant::V1, CnParams::CN_LITE, aes),
            Self::CnTurtleLiteV2 => CnHasher::with_aes(Variant::V2, CnParams::CN_TURTLE_LITE, aes),
        }
    }

    /// Short name for logs.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::CnV0 => "cn/0",
            Self::CnLiteV1 => "cn-lite/1",
            Self::CnTurtleLiteV2 => "cn-turtle-lite/2",
        }
    }
}

/// What a hash must reach.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    /// A block: `check_hash(hash, difficulty)` (C++ `check_hash`, `check_difficulty.cpp`). The
    /// first solution ends the job.
    Block(u64),
    /// A pool share: the last 8 bytes of the hash, little-endian, at most this value
    /// (`xkr_stratum::target`). Hashing goes on after a share.
    Share(u64),
}

impl Target {
    /// Whether `hash` reaches the target.
    #[must_use]
    pub fn is_met(&self, hash: &[u8; 32]) -> bool {
        match *self {
            Self::Block(difficulty) => check_hash(&Hash(*hash), difficulty),
            Self::Share(target) => xkr_stratum::target::meets_target_u64(hash, target),
        }
    }

    /// Whether a solution is a block (and ends the job).
    #[must_use]
    pub fn is_block(&self) -> bool {
        matches!(self, Self::Block(_))
    }

    /// The difficulty this target stands for (a share target `t` is `u64::MAX / t`).
    #[must_use]
    pub fn difficulty(&self) -> u64 {
        match *self {
            Self::Block(d) => d,
            Self::Share(t) => u64::MAX / t.max(1),
        }
    }
}

/// A job is not usable.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum JobError {
    /// The nonce does not fit into the blob.
    #[error("nonce offset {offset} past the end of a {len}-byte blob")]
    NonceOffset {
        /// The offset.
        offset: usize,
        /// The blob length.
        len: usize,
    },
    /// No proof-of-work algorithm for this major version.
    #[error("unknown block major version {0}")]
    MajorVersion(u8),
    /// CryptoNight-Lite variant 1 needs at least 43 bytes.
    #[error("blob of {0} bytes is too short for its algorithm")]
    TooShort(usize),
    /// The block could not be serialized.
    #[error("cannot serialize the block: {0}")]
    Encode(String),
}

impl From<EncodeError> for JobError {
    fn from(e: EncodeError) -> Self {
        Self::Encode(e.to_string())
    }
}

/// What the hashing threads work on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Job {
    /// The source's id: the pool's `job_id`, or a counter for block templates.
    pub id: String,
    /// The proof-of-work blob (C++ `getBlockLongHash` input); the threads write their nonce
    /// into a copy.
    pub blob: Vec<u8>,
    /// Offset of the 4 little-endian nonce bytes in [`Self::blob`].
    pub nonce_offset: usize,
    /// What a hash must reach.
    pub target: Target,
    /// Height of the block (0 when a pool does not say).
    pub height: u64,
    /// The major version byte the blob starts with: the block's for a block template, the
    /// merge-mining parent's (0 or 1) for a pool job.
    pub major: u8,
    /// The proof-of-work algorithm: from the block major version of a template, or what a pool
    /// job says ([`crate::pool_job`]).
    pub algo: PowAlgo,
}

impl Job {
    /// A checked job.
    pub fn new(
        id: String,
        blob: Vec<u8>,
        nonce_offset: usize,
        target: Target,
        height: u64,
        major: u8,
    ) -> Result<Self, JobError> {
        let algo = PowAlgo::for_major(major).ok_or(JobError::MajorVersion(major))?;
        Self::with_algo(id, blob, nonce_offset, target, height, major, algo)
    }

    /// A checked job hashed with `algo` whatever `major` is (a pool job: its blob is the
    /// merge-mining parent's hashing blob, whose major version does not name the algorithm).
    pub fn with_algo(
        id: String,
        blob: Vec<u8>,
        nonce_offset: usize,
        target: Target,
        height: u64,
        major: u8,
        algo: PowAlgo,
    ) -> Result<Self, JobError> {
        if nonce_offset
            .checked_add(4)
            .is_none_or(|end| end > blob.len())
        {
            return Err(JobError::NonceOffset {
                offset: nonce_offset,
                len: blob.len(),
            });
        }
        if algo == PowAlgo::CnLiteV1 && blob.len() < 43 {
            return Err(JobError::TooShort(blob.len()));
        }
        Ok(Self {
            id,
            blob,
            nonce_offset,
            target,
            height,
            major,
            algo,
        })
    }

    /// The proof-of-work algorithm.
    #[must_use]
    pub fn algo(&self) -> PowAlgo {
        self.algo
    }

    /// The blob with `nonce` written in.
    #[must_use]
    pub fn blob_with_nonce(&self, nonce: u32) -> Vec<u8> {
        let mut blob = self.blob.clone();
        blob[self.nonce_offset..self.nonce_offset + 4].copy_from_slice(&nonce.to_le_bytes());
        blob
    }

    /// The proof-of-work hash for `nonce` (one CryptoNight hash).
    #[must_use]
    pub fn hash(&self, nonce: u32, aes: AesImpl) -> [u8; 32] {
        self.algo()
            .hasher(aes)
            .hash(&self.blob_with_nonce(nonce))
            .unwrap_or_default()
    }
}

/// A nonce that reaches the target of its job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    /// The job it solves.
    pub job: Arc<Job>,
    /// The nonce (little-endian in the blob).
    pub nonce: u32,
    /// The proof-of-work hash.
    pub hash: [u8; 32],
}

impl Found {
    /// The nonce as stratum sends it: the 4 blob bytes as 8 hex characters.
    #[must_use]
    pub fn nonce_hex(&self) -> String {
        hex::encode(self.nonce.to_le_bytes())
    }

    /// The hash as 64 hex characters (stratum `result`).
    #[must_use]
    pub fn hash_hex(&self) -> String {
        hex::encode(self.hash)
    }
}

/// C++ `MinerManager` `adjustMergeMiningTag` (`src/miner/miner_manager.cpp`; also
/// `xkr_chain::set_merge_mining_tag`): for major version >= 2 the parent miner transaction's
/// extra becomes a depth-0 merge-mining tag committing to the block's auxiliary header hash.
/// Without it no nonce gives a valid proof of work.
pub fn set_merge_mining_tag(block: &mut Block) -> Result<(), EncodeError> {
    if block.header.major_version < 2 {
        return Ok(());
    }
    let aux = aux_block_header_hash(block)?;
    if let Some(pb) = block.parent_block.as_mut() {
        let mut extra = Vec::with_capacity(35);
        extra.push(TX_EXTRA_MERGE_MINING_TAG);
        extra.push(33);
        extra.push(0);
        extra.extend_from_slice(&aux.0);
        pb.base_transaction.prefix.extra = extra;
    }
    Ok(())
}

/// Offset of the 4 nonce bytes in the proof-of-work blob of `block`, found by encoding it with
/// two different nonces, so it holds for every major version (xkrd's built-in miner).
#[must_use]
pub fn block_nonce_offset(block: &Block) -> Option<usize> {
    let mut b = block.clone();
    b.header.nonce = 0;
    let zero = block_pow_blob(&b).ok()?;
    b.header.nonce = u32::MAX;
    let ones = block_pow_blob(&b).ok()?;
    if zero.len() != ones.len() {
        return None;
    }
    let first = zero.iter().zip(&ones).position(|(a, c)| a != c)?;
    (first + 4 <= zero.len()
        && zero[first..first + 4] == [0u8; 4]
        && ones[first..first + 4] == [0xffu8; 4])
        .then_some(first)
}

/// The job for a block template `block` (merge-mining tag already set) at `height` with
/// `difficulty`: its proof-of-work blob, the nonce offset and the algorithm of its major
/// version (C++ `MinerManager::startMining` -> `Miner::mine`).
pub fn block_job(block: &Block, id: String, height: u64, difficulty: u64) -> Result<Job, JobError> {
    let blob = block_pow_blob(block)?;
    let offset = block_nonce_offset(block).ok_or(JobError::NonceOffset {
        offset: usize::MAX,
        len: blob.len(),
    })?;
    Job::new(
        id,
        blob,
        offset,
        Target::Block(difficulty.max(1)),
        height,
        block.header.major_version,
    )
}

#[cfg(test)]
mod tests {
    use xkr_types::{BlockHeader, ParentBlock, Transaction};

    use super::*;

    fn block(major: u8) -> Block {
        let mut block = Block {
            header: BlockHeader {
                major_version: major,
                minor_version: 0,
                timestamp: 1_700_000_000,
                prev_hash: Hash([3; 32]),
                nonce: 0,
            },
            parent_block: (major >= 2).then(|| ParentBlock {
                major_version: 1,
                transaction_count: 1,
                ..ParentBlock::default()
            }),
            miner_tx: Transaction::default(),
            tx_hashes: vec![],
        };
        block.miner_tx.prefix.version = 1;
        set_merge_mining_tag(&mut block).unwrap();
        block
    }

    /// For majors 1-7 the nonce offset found by encoding twice is where the nonce lands in
    /// the proof-of-work blob, and equals the stratum offset (three header varints + 32).
    #[test]
    fn nonce_offset_for_majors_1_to_7() {
        for major in 1..=7u8 {
            let mut b = block(major);
            let off = block_nonce_offset(&b).unwrap_or_else(|| panic!("major {major}"));
            let job = block_job(&b, "1".into(), 10, 5).unwrap();
            assert_eq!(job.nonce_offset, off, "major {major}");
            assert_eq!(
                off,
                xkr_stratum::blob::nonce_offset(&job.blob),
                "major {major}"
            );
            b.header.nonce = 0x0403_0201;
            let blob = block_pow_blob(&b).unwrap();
            assert_eq!(&blob[off..off + 4], &[1, 2, 3, 4], "major {major}");
            assert_eq!(job.blob_with_nonce(0x0403_0201), blob, "major {major}");
            assert_eq!(job.algo(), PowAlgo::for_major(major).unwrap());
        }
        assert_eq!(PowAlgo::for_major(3), Some(PowAlgo::CnV0));
        assert_eq!(PowAlgo::for_major(4), Some(PowAlgo::CnLiteV1));
        assert_eq!(PowAlgo::for_major(7), Some(PowAlgo::CnTurtleLiteV2));
        assert_eq!(PowAlgo::for_major(0), None);
    }

    #[test]
    fn share_and_block_targets() {
        let mut hash = [0u8; 32];
        hash[24..32].copy_from_slice(&1000u64.to_le_bytes());
        assert!(Target::Share(1000).is_met(&hash));
        assert!(!Target::Share(999).is_met(&hash));
        assert!(!Target::Share(1).is_block());
        assert_eq!(Target::Share(u64::MAX).difficulty(), 1);
        // check_hash: hash * difficulty < 2^256
        assert!(Target::Block(1).is_met(&[0xff; 32]));
        assert!(!Target::Block(2).is_met(&[0xff; 32]));
        let mut small = [0u8; 32];
        small[31] = 0x10; // 2^252
        assert!(Target::Block(15).is_met(&small));
        assert!(!Target::Block(16).is_met(&small));
        assert!(Target::Block(3).is_block());
        // a compact pool target of difficulty 100 against its 64-bit form
        let t = xkr_stratum::target::parse_target("285c8f02").unwrap();
        assert_eq!(Target::Share(t).difficulty(), 100);
    }

    #[test]
    fn job_checks() {
        assert!(matches!(
            Job::new("j".into(), vec![5; 40], 37, Target::Share(1), 0, 5),
            Err(JobError::NonceOffset { .. })
        ));
        assert_eq!(
            Job::new("j".into(), vec![0; 76], 39, Target::Share(1), 0, 0),
            Err(JobError::MajorVersion(0))
        );
        assert_eq!(
            Job::new("j".into(), vec![4; 42], 38, Target::Share(1), 0, 4),
            Err(JobError::TooShort(42))
        );
        let job = Job::new("j".into(), vec![7; 76], 39, Target::Share(1), 0, 7).unwrap();
        assert_eq!(
            job.hash(9, AesImpl::Software),
            xkr_crypto::cn_turtle_lite_slow_hash_v2(&job.blob_with_nonce(9)).0
        );
        let found = Found {
            job: Arc::new(job),
            nonce: 0x0403_0201,
            hash: [0xab; 32],
        };
        assert_eq!(found.nonce_hex(), "01020304");
        assert_eq!(found.hash_hex(), "ab".repeat(32));
    }
}
