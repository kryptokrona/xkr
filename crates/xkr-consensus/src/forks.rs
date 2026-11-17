//! The fork table (`docs/forks.md`): one [`ForkSchedule`] per network instead of
//! height comparisons spread over the crate.
//!
//! The C++ node keeps three unrelated lists in `src/config/cryptonote_config.h`:
//!
//! * `FORK_HEIGHTS[0..=12]` with `SOFTWARE_SUPPORTED_FORK_INDEX = 12`, which is purely
//!   informational (`/getinfo` `upgrade_heights` / `supported_height` and the "update needed"
//!   status of `format_tools.cpp`);
//! * the block major version upgrade heights `UPGRADE_HEIGHT_V2..V5` that `Core`,
//!   `BlockchainCache` and `DatabaseBlockchainCache` register with their `UpgradeManager`;
//! * per-rule `*_HEIGHT` / `*_INDEX` constants (mixin limits, LWMA, dust thresholds, ...).
//!
//! The table joins them: every fork index has a height (or "not scheduled"), the block major
//! version in force after it, the minor version that signals support for it, the transaction
//! versions allowed after it and the rules it introduced.
//!
//! **Activation convention** (C++ `UpgradeManager::getBlockMajorVersion`: the highest version
//! whose `upgradeIndex < blockIndex`): a fork at height `F` governs the blocks `F + 1, F + 2, ...`;
//! block `F` is the last block under the old rules. [`ForkSchedule::is_fork_active`] uses that
//! convention (it is also how LWMA is selected: by the *previous* block index `>= F`).
//! [`ForkSchedule::has_reached`] is the `height >= F` comparison some historical rules use
//! (mixin limits, dust thresholds); those keep their exact C++ comparison in their own functions
//! and the unit tests below pin every such constant to its table entry.
//!
//! **Fork 13** ("flag day", `docs/forks.md` §2): block major version 6 with the block format,
//! proof of work (CN-Turtle-Lite v2), difficulty (LWMA-3) and reward of version 5. Only the
//! version changes, so a node that does not know version 6 (C++ v1.2.2 and older:
//! `serializeBlockHeader` throws "Wrong major version" for `majorVersion > 5`) stops at height
//! `F`. Mainnet height: 2 700 000 ([`FORK_13_HEIGHT_MAINNET`]); testnet:
//! set at run time (`xkrd --fork13-height`, [`NetworkParams::with_fork13_height`]).
//!
//! **Fork 14** (`docs/tx-v2.md`): block major version 7 and transaction version 2
//! (view tags, single-key CLSAG). Version 1 stays valid for [`Fork14Params::grace_blocks`]
//! blocks after the fork, then only version 2. Scheduled on mainnet at
//! [`FORK_14_HEIGHT_MAINNET`] (2 750 000); on testnet it is set at run time
//! ([`NetworkParams::with_fork14_height`]). It is only part of [`ForkSchedule::forks`] once a
//! height is set. [`SUPPORTED_FORK_INDEX`] (13) is the baseline; a schedule with fork 14 set
//! supports and signals 14 ([`ForkSchedule::supported_index`],
//! [`ForkSchedule::template_minor_version`]), which also signals 13 ([`signals_fork`] is
//! `minor >= target`). [`ForkSchedule::fork14_entry`] shows the entry, scheduled or not.

use crate::params::{
    BLOCK_MAJOR_VERSION_5, BLOCK_MAJOR_VERSION_6, BLOCK_MAJOR_VERSION_7, FORK_HEIGHTS,
    NetworkParams, TRANSACTION_VERSION_1, TRANSACTION_VERSION_2,
};

/// Fork 13 on mainnet: height **2 700 000**, set on 2026-11-17 (block 2 700 001 is the first
/// version-6 block). C++ nodes need the v1.2 release with this height, p2pool its patches
/// (`docs/forks.md` §6).
pub const FORK_13_HEIGHT_MAINNET: Option<u32> = Some(2_700_000);
/// Fork 13 on testnet by default: not scheduled; `xkrd --fork13-height <h>` sets it.
pub const FORK_13_HEIGHT_TESTNET: Option<u32> = None;
/// Fork 14 (transaction version 2) on mainnet: height **2 750 000**, set on 2026-11-17 (block
/// 2 750 001 is the first version-7 block; version-1 transactions stay valid for
/// [`FORK_14_PARAMS_MAINNET`]`.grace_blocks` more blocks).
pub const FORK_14_HEIGHT_MAINNET: Option<u32> = Some(2_750_000);
/// Fork 14 on testnet by default: not scheduled; set at run time
/// ([`NetworkParams::with_fork14_height`]).
pub const FORK_14_HEIGHT_TESTNET: Option<u32> = None;

/// How the ring size of the key inputs of version-2 transactions is limited, on top of the
/// mixin limits every transaction obeys ([`crate::reward::mixin_limits`]: ring size 2..=6 today).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RingSizeRule {
    /// Only the mixin limits (no extra rule).
    MixinLimits,
    /// Every key input of a version-2 transaction has a ring size in `min..=max` (`min == max`
    /// is a fixed ring size; Monero uses 16).
    Range {
        /// Smallest ring size.
        min: u16,
        /// Largest ring size.
        max: u16,
    },
}

/// Network parameters of fork 14 (`docs/tx-v2.md` §8).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Fork14Params {
    /// Blocks after the fork during which version-1 transactions are still valid: blocks
    /// `F + 1 ..= F + grace_blocks` accept versions 1 and 2, later blocks only version 2.
    pub grace_blocks: u64,
    /// Extra ring-size rule for version-2 transactions.
    pub v2_ring_size: RingSizeRule,
}

/// Mainnet fork-14 parameters: 30 days of grace (28 800 blocks at 90 s) and the
/// existing mixin limits.
pub const FORK_14_PARAMS_MAINNET: Fork14Params = Fork14Params {
    grace_blocks: 28_800,
    v2_ring_size: RingSizeRule::MixinLimits,
};
/// Testnet fork-14 parameters: 20 blocks of grace (20 s at testnet difficulty 1 with the
/// built-in miner), mixin limits.
pub const FORK_14_PARAMS_TESTNET: Fork14Params = Fork14Params {
    grace_blocks: 20,
    v2_ring_size: RingSizeRule::MixinLimits,
};

/// Index of the LWMA-2 fork (620 000).
pub const FORK_LWMA_2: u8 = 3;
/// Index of the LWMA-2 V4 fork (700 000).
pub const FORK_LWMA_2_V2: u8 = 4;
/// Index of the LWMA-2 V5 fork (800 000).
pub const FORK_LWMA_2_V3: u8 = 5;
/// Index of the LWMA-3 fork (2 200 000), the last one of C++ v1.2.2.
pub const FORK_LWMA_3: u8 = 12;
/// Index of fork 13 (block major version 6).
pub const FORK_13: u8 = 13;
/// Index of fork 14 (block major version 7, transaction version 2).
pub const FORK_14: u8 = 14;

/// The highest fork index this software supports on a schedule without fork 14 (C++
/// `SOFTWARE_SUPPORTED_FORK_INDEX` of a fork-13 build; [`crate::params::SOFTWARE_SUPPORTED_FORK_INDEX`]
/// keeps the C++ v1.2.2 value 12). Once fork 14 is scheduled (mainnet 2 750 000, testnet
/// `--fork14-height`) the schedule supports 14: [`ForkSchedule::supported_index`].
pub const SUPPORTED_FORK_INDEX: u8 = FORK_13;

/// Number of entries in the table while fork 14 is not scheduled (forks 0..=13).
pub const FORK_COUNT: usize = 14;
/// Number of entries with fork 14 scheduled.
pub const MAX_FORK_COUNT: usize = FORK_COUNT + 1;

/// The minor version p2pool writes into every block it mines: Monero's
/// `HARDFORK_SUPPORTED_VERSION` (16, `p2pool/src/common.h`), inherited by the XKR fork. It says
/// nothing about XKR forks, so it never counts as a signal (mainnet blocks carry minor 0 from the
/// C++ daemon or 16 from p2pool).
pub const P2POOL_MINOR_VERSION: u8 = 16;

/// Blocks in the fork readiness window (`/getinfo` `fork_readiness`; 720 blocks = 18 h at 90 s).
pub const FORK_READINESS_WINDOW: u64 = 720;

/// Rules a fork introduced, as a bit set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Rules(u32);

impl Rules {
    /// Nothing (a release marker in C++ `FORK_HEIGHTS` without a rule in today's code).
    pub const NONE: Self = Self(0);
    /// The block major version changes to [`Fork::major_version`]. The only rule nodes that do
    /// not know the fork cannot follow (they fail to deserialise the block).
    pub const MAJOR_VERSION: Self = Self(1 << 0);
    /// `ZAWY_DIFFICULTY_BLOCK_INDEX` (187 000; dead for major >= 3, see `difficulty.rs`).
    pub const ZAWY_DIFFICULTY: Self = Self(1 << 1);
    /// `MIXIN_LIMITS_V1_HEIGHT` (`height >=`).
    pub const MIXIN_LIMITS_V1: Self = Self(1 << 2);
    /// `LWMA_2_DIFFICULTY_BLOCK_INDEX` (previous index `>=`).
    pub const LWMA_2: Self = Self(1 << 3);
    /// `MIXIN_LIMITS_V2_HEIGHT` (`height >=`).
    pub const MIXIN_LIMITS_V2: Self = Self(1 << 4);
    /// `DUST_THRESHOLD_V2_HEIGHT` (`height >=`).
    pub const DUST_THRESHOLD_V2: Self = Self(1 << 5);
    /// `CRYPTONOTE_BLOCK_FUTURE_TIME_LIMIT_V3` from `LWMA_2_DIFFICULTY_BLOCK_INDEX` (`height >=`).
    pub const FUTURE_TIME_LIMIT_V3: Self = Self(1 << 6);
    /// `LWMA_2_DIFFICULTY_BLOCK_INDEX_V2` (previous index `>=`).
    pub const LWMA_2_V2: Self = Self(1 << 7);
    /// `CRYPTONOTE_BLOCK_FUTURE_TIME_LIMIT_V4` (`height >=`).
    pub const FUTURE_TIME_LIMIT_V4: Self = Self(1 << 8);
    /// `LWMA_2_DIFFICULTY_BLOCK_INDEX_V3` (previous index `>=`).
    pub const LWMA_2_V3: Self = Self(1 << 9);
    /// `MIXIN_LIMITS_V3_HEIGHT` (`height >=`).
    pub const MIXIN_LIMITS_V3: Self = Self(1 << 10);
    /// `FUSION_DUST_THRESHOLD_HEIGHT_V2` (`height >=`).
    pub const FUSION_DUST_THRESHOLD_V2: Self = Self(1 << 11);
    /// `BLOCKCHAIN_TIMESTAMP_CHECK_WINDOW_V3` (`height >=`).
    pub const TIMESTAMP_WINDOW_V3: Self = Self(1 << 12);
    /// `MAX_EXTRA_SIZE_V2_HEIGHT` (wallet policy only, never checked on blocks).
    pub const MAX_EXTRA_SIZE_V2: Self = Self(1 << 13);
    /// `LWMA_3_DIFFICULTY_BLOCK_INDEX` (previous index `>=`).
    pub const LWMA_3: Self = Self(1 << 14);
    /// `MIXIN_LIMITS_V4_HEIGHT` (1 250 000, not a `FORK_HEIGHTS` entry: see [`OFF_TABLE_RULES`]).
    pub const MIXIN_LIMITS_V4: Self = Self(1 << 15);
    /// `BLOCK_BLOB_SHUFFLE_CHECK_HEIGHT` (1 750 000, off-table).
    pub const BLOCK_BLOB_SHUFFLE_CHECK: Self = Self(1 << 16);
    /// `TRANSACTION_SIGNATURE_COUNT_VALIDATION_HEIGHT` (1 750 000, off-table).
    pub const TX_SIGNATURE_COUNT: Self = Self(1 << 17);
    /// Transaction version 2 is allowed (fork 14); version 1 only during the grace period.
    pub const TX_VERSION_2: Self = Self(1 << 18);
    /// Version-2 outputs are tagged keys (view tags); coinbase transactions are version 2.
    pub const VIEW_TAGS: Self = Self(1 << 19);
    /// Version-2 inputs carry single-key CLSAG signatures.
    pub const CLSAG: Self = Self(1 << 20);

    /// Both sets.
    #[must_use]
    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    /// Whether every rule of `other` is in `self`.
    #[must_use]
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    /// The raw bits.
    #[must_use]
    pub const fn bits(self) -> u32 {
        self.0
    }
}

/// Transaction versions a block may contain after a fork (inclusive range).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TxVersions {
    /// Lowest allowed version.
    pub min: u8,
    /// Highest allowed version.
    pub max: u8,
}

impl TxVersions {
    /// Version 1 only (every fork up to 13; tx v2 is fork 14, `docs/tx-v2.md`).
    pub const V1: Self = Self {
        min: TRANSACTION_VERSION_1,
        max: TRANSACTION_VERSION_1,
    };
    /// Versions 1 and 2 (fork 14 during the grace period).
    pub const V1_V2: Self = Self {
        min: TRANSACTION_VERSION_1,
        max: TRANSACTION_VERSION_2,
    };
    /// Version 2 only (fork 14 after the grace period).
    pub const V2: Self = Self {
        min: TRANSACTION_VERSION_2,
        max: TRANSACTION_VERSION_2,
    };

    /// Whether `version` is allowed.
    #[must_use]
    pub const fn allows(self, version: u8) -> bool {
        version >= self.min && version <= self.max
    }
}

/// One entry of the fork table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Fork {
    /// Index (C++ `FORK_HEIGHTS` position).
    pub index: u8,
    /// Height `F`: block `F` is the last under the old rules. `None` = not scheduled.
    pub height: Option<u64>,
    /// Block major version of the blocks after the fork.
    pub major_version: u8,
    /// Minor version a block writes to signal that its miner supports this fork
    /// ([`signals_fork`]).
    pub minor_version_signal: u8,
    /// Transaction versions allowed after the fork.
    pub tx_versions: TxVersions,
    /// What changed.
    pub rules: Rules,
}

/// Rules whose C++ activation heights are not `FORK_HEIGHTS` entries; they keep their own
/// constants (`reward.rs`, `validation.rs`).
pub const OFF_TABLE_RULES: [(u64, Rules); 2] = [
    (1_250_000, Rules::MIXIN_LIMITS_V4),
    (
        1_750_000,
        Rules::BLOCK_BLOB_SHUFFLE_CHECK.union(Rules::TX_SIGNATURE_COUNT),
    ),
];

const fn historical(index: u8, rules: Rules) -> Fork {
    Fork {
        index,
        height: Some(FORK_HEIGHTS[index as usize]),
        major_version: BLOCK_MAJOR_VERSION_5,
        minor_version_signal: index,
        tx_versions: TxVersions::V1,
        rules,
    }
}

/// Forks 0..=12: C++ `FORK_HEIGHTS` (identical on mainnet and testnet; the testnet build only
/// changes `UPGRADE_HEIGHT_V2..V5`). Every block after height 4 is major 5 on mainnet.
const HISTORICAL: [Fork; 13] = [
    historical(0, Rules::ZAWY_DIFFICULTY),
    historical(1, Rules::NONE),
    historical(2, Rules::MIXIN_LIMITS_V1),
    historical(
        3,
        Rules::LWMA_2
            .union(Rules::MIXIN_LIMITS_V2)
            .union(Rules::DUST_THRESHOLD_V2)
            .union(Rules::FUTURE_TIME_LIMIT_V3),
    ),
    historical(4, Rules::LWMA_2_V2.union(Rules::FUTURE_TIME_LIMIT_V4)),
    historical(
        5,
        Rules::LWMA_2_V3
            .union(Rules::MIXIN_LIMITS_V3)
            .union(Rules::FUSION_DUST_THRESHOLD_V2)
            .union(Rules::TIMESTAMP_WINDOW_V3),
    ),
    historical(6, Rules::NONE),
    historical(7, Rules::NONE),
    historical(8, Rules::MAX_EXTRA_SIZE_V2),
    historical(9, Rules::NONE),
    historical(10, Rules::NONE),
    historical(11, Rules::NONE),
    historical(12, Rules::LWMA_3),
];

/// `Some(0)` means "not scheduled" too (a fork at height 0 would rewrite the whole chain).
const fn normalize(height: Option<u32>) -> Option<u32> {
    match height {
        Some(0) | None => None,
        Some(h) => Some(h),
    }
}

/// Whether a block with `minor_version` signals support for fork `target`: the minor version is
/// at least the fork index, and it is not p2pool's constant [`P2POOL_MINOR_VERSION`].
#[must_use]
pub const fn signals_fork(minor_version: u8, target: u8) -> bool {
    minor_version >= target && minor_version != P2POOL_MINOR_VERSION
}

/// Minor version of the blocks this node builds (`getblocktemplate`, built-in miner): the
/// supported fork index for major >= 2. The C++ daemon writes 0 (`Core::getBlockTemplate`; its
/// `UpgradeManager` ignores minor versions), so the value is free for signalling. Never 1:
/// `BLOCK_MINOR_VERSION_1` is the vote of the (unused) C++ `BasicUpgradeDetector`.
#[must_use]
pub const fn template_minor_version(major_version: u8) -> u8 {
    if major_version >= 2 {
        SUPPORTED_FORK_INDEX
    } else {
        0
    }
}

/// Rules of fork 14.
pub const FORK_14_RULES: Rules = Rules::MAJOR_VERSION
    .union(Rules::TX_VERSION_2)
    .union(Rules::VIEW_TAGS)
    .union(Rules::CLSAG);

/// The fork table of one network.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ForkSchedule {
    /// `UPGRADE_HEIGHT_V2..V5` (the versions before the fork table).
    upgrade_heights: [u32; 4],
    /// Height of fork 13, `None` = not scheduled.
    fork13: Option<u32>,
    /// Height of fork 14, `None` = not scheduled.
    fork14: Option<u32>,
    /// Parameters of fork 14.
    fork14_params: Fork14Params,
}

impl ForkSchedule {
    /// A schedule from the legacy upgrade heights and the fork-13 height (`Some(0)` = not
    /// scheduled). Fork 14 is not scheduled (see [`Self::with_fork14`]).
    #[must_use]
    pub const fn new(upgrade_heights: [u32; 4], fork13_height: Option<u32>) -> Self {
        Self {
            upgrade_heights,
            fork13: normalize(fork13_height),
            fork14: None,
            fork14_params: FORK_14_PARAMS_MAINNET,
        }
    }

    /// This schedule with fork 14 at `height` (`None`/`Some(0)` = not scheduled) and `params`.
    #[must_use]
    pub const fn with_fork14(self, height: Option<u32>, params: Fork14Params) -> Self {
        Self {
            fork14: normalize(height),
            fork14_params: params,
            ..self
        }
    }

    /// The schedule of `params`.
    #[must_use]
    pub const fn for_params(params: &NetworkParams) -> Self {
        Self::new(params.upgrade_heights, params.fork13_height)
            .with_fork14(params.fork14_height, params.fork14)
    }

    /// [`SUPPORTED_FORK_INDEX`].
    #[must_use]
    pub const fn supported_fork_index() -> u8 {
        SUPPORTED_FORK_INDEX
    }

    /// The highest fork index this software supports on this schedule: 14 once fork 14 is
    /// scheduled (its parameters are then fixed), else [`SUPPORTED_FORK_INDEX`] (13). Used by
    /// `/getinfo` `supported_height`, `/status.fork.supported_index` and the template signal.
    #[must_use]
    pub const fn supported_index(&self) -> u8 {
        if self.fork14.is_some() {
            FORK_14
        } else {
            SUPPORTED_FORK_INDEX
        }
    }

    /// Minor version of the blocks this node builds on this schedule: [`Self::supported_index`]
    /// for major >= 2 (see the free [`template_minor_version`]). A fork-14 node writes 14, which
    /// signals forks 13 and 14.
    #[must_use]
    pub const fn template_minor_version(&self, major_version: u8) -> u8 {
        if major_version >= 2 {
            self.supported_index()
        } else {
            0
        }
    }

    /// Height of fork 13, if scheduled.
    #[must_use]
    pub const fn fork13_height(&self) -> Option<u32> {
        self.fork13
    }

    /// Height of fork 14, if scheduled.
    #[must_use]
    pub const fn fork14_height(&self) -> Option<u32> {
        self.fork14
    }

    /// Parameters of fork 14 (grace period, ring sizes).
    #[must_use]
    pub const fn fork14_params(&self) -> Fork14Params {
        self.fork14_params
    }

    /// The fork-14 entry, scheduled or not (`height: None` when not). [`Self::forks`] only
    /// contains it once it is scheduled.
    #[must_use]
    pub fn fork14_entry(&self) -> Fork {
        Fork {
            index: FORK_14,
            height: self.fork14.map(u64::from),
            major_version: BLOCK_MAJOR_VERSION_7,
            minor_version_signal: FORK_14,
            tx_versions: TxVersions::V1_V2,
            rules: FORK_14_RULES,
        }
    }

    /// Every entry, by index: forks 0..=13, plus fork 14 once it is scheduled.
    #[must_use]
    pub fn forks(&self) -> Vec<Fork> {
        let mut all = Vec::with_capacity(MAX_FORK_COUNT);
        all.extend_from_slice(&HISTORICAL);
        all.push(Fork {
            index: FORK_13,
            height: self.fork13.map(u64::from),
            major_version: BLOCK_MAJOR_VERSION_6,
            minor_version_signal: FORK_13,
            tx_versions: TxVersions::V1,
            rules: Rules::MAJOR_VERSION,
        });
        if self.fork14.is_some() {
            all.push(self.fork14_entry());
        }
        all
    }

    /// Whether fork 14 (transaction version 2) governs the block at `height` (`height > F`).
    #[must_use]
    pub fn is_tx_v2_active(&self, height: u64) -> bool {
        self.fork14.is_some_and(|f| height > u64::from(f))
    }

    /// Transaction versions a non-coinbase transaction in the block at `height` may have:
    /// version 1 before fork 14, 1 and 2 in the grace period `F + 1 ..= F + grace_blocks`, then
    /// only 2. (Version 0 counts as 1, see [`Self::allows_tx_version`].)
    #[must_use]
    pub fn tx_versions_at(&self, height: u64) -> TxVersions {
        match self.fork14 {
            Some(f) if height > u64::from(f) => {
                if height - u64::from(f) > self.fork14_params.grace_blocks {
                    TxVersions::V2
                } else {
                    TxVersions::V1_V2
                }
            }
            _ => TxVersions::V1,
        }
    }

    /// Whether a non-coinbase transaction of `version` is valid in the block at `height`.
    /// Version 0 is the CryptoNote layout too and C++ accepts it wherever version 1 is valid
    /// (its deserializer only rejects versions above `CURRENT_TRANSACTION_VERSION`), so it is
    /// treated as version 1.
    #[must_use]
    pub fn allows_tx_version(&self, height: u64, version: u8) -> bool {
        self.tx_versions_at(height)
            .allows(version.max(TRANSACTION_VERSION_1))
    }

    /// Version of the coinbase transaction of the block at `height`: 2 from fork 14 on (tagged
    /// outputs, so pools and p2pool must build version-2 coinbases at the fork), else 1.
    #[must_use]
    pub fn coinbase_tx_version(&self, height: u64) -> u8 {
        if self.is_tx_v2_active(height) {
            TRANSACTION_VERSION_2
        } else {
            TRANSACTION_VERSION_1
        }
    }

    /// Whether a coinbase transaction of `version` is valid in the block at `height`: exactly 2
    /// from fork 14 on, 0 or 1 before (as today).
    #[must_use]
    pub fn allows_coinbase_version(&self, height: u64, version: u8) -> bool {
        if self.is_tx_v2_active(height) {
            version == TRANSACTION_VERSION_2
        } else {
            version <= TRANSACTION_VERSION_1
        }
    }

    /// Entry `index`.
    #[must_use]
    pub fn fork(&self, index: u8) -> Option<Fork> {
        self.forks().get(usize::from(index)).copied()
    }

    /// Whether fork `index` governs the block at `height` (`height > F`; `false` if not
    /// scheduled).
    #[must_use]
    pub fn is_fork_active(&self, index: u8, height: u64) -> bool {
        self.fork(index)
            .and_then(|f| f.height)
            .is_some_and(|f| height > f)
    }

    /// `height >= F` (the comparison of the historical `*_HEIGHT` rules); `false` if not
    /// scheduled.
    #[must_use]
    pub fn has_reached(&self, index: u8, height: u64) -> bool {
        self.fork(index)
            .and_then(|f| f.height)
            .is_some_and(|f| height >= f)
    }

    /// The fork in force for the block at `height`: the highest index that is active.
    #[must_use]
    pub fn fork_at(&self, height: u64) -> Option<Fork> {
        self.forks()
            .into_iter()
            .filter(|f| f.height.is_some_and(|h| height > h))
            .max_by_key(|f| f.index)
    }

    /// The next scheduled fork after [`Self::fork_at`]`(height)`: the lowest height among the
    /// scheduled forks with a higher index that are not active yet (C++ `next_fork_index` uses
    /// the same `F >= height`). `None` when nothing is scheduled.
    #[must_use]
    pub fn next_fork(&self, height: u64) -> Option<Fork> {
        let active = self.fork_at(height).map(|f| f.index);
        self.forks()
            .into_iter()
            .filter(|f| active.is_none_or(|a| f.index > a))
            .filter_map(|f| f.height.map(|h| (h, f)))
            .filter(|(h, _)| *h >= height)
            .min_by_key(|(h, f)| (*h, f.index))
            .map(|(_, f)| f)
    }

    /// The most recent block-version fork in force for the block at `height`
    /// ([`Rules::MAJOR_VERSION`], `height > F`).
    #[must_use]
    pub fn last_version_fork_at(&self, height: u64) -> Option<Fork> {
        self.forks().into_iter().rev().find(|f| {
            f.rules.contains(Rules::MAJOR_VERSION) && f.height.is_some_and(|h| height > h)
        })
    }

    /// The block-version fork that miners should signal for at `height`: the lowest-index
    /// [`Rules::MAJOR_VERSION`] fork that is not active yet, **scheduled or not** (readiness is
    /// what maintainers look at before picking a height).
    #[must_use]
    pub fn signal_target(&self, height: u64) -> Option<Fork> {
        self.forks().into_iter().find(|f| {
            f.rules.contains(Rules::MAJOR_VERSION) && f.height.is_none_or(|h| height <= h)
        })
    }

    /// C++ `UpgradeManager::getBlockMajorVersion(height)`: the version of the newest
    /// block-version fork with `F < height`, else the highest of `UPGRADE_HEIGHT_V2..V5` below
    /// `height`, else 1.
    #[must_use]
    pub fn block_major_version_for(&self, height: u64) -> u8 {
        if let Some(f) = self.last_version_fork_at(height) {
            return f.major_version;
        }
        for (i, upgrade) in self.upgrade_heights.iter().enumerate().rev() {
            if u64::from(*upgrade) < height {
                return i as u8 + 2;
            }
        }
        1
    }

    /// C++ `Currency::upgradeHeight(majorVersion)`; `None` = `UNDEF_HEIGHT`.
    #[must_use]
    pub fn upgrade_height(&self, major_version: u8) -> Option<u32> {
        match major_version {
            2..=5 => Some(self.upgrade_heights[usize::from(major_version - 2)]),
            BLOCK_MAJOR_VERSION_6 => self.fork13,
            BLOCK_MAJOR_VERSION_7 => self.fork14,
            _ => None,
        }
    }

    /// `/getinfo` `upgrade_heights`: the scheduled fork heights in index order (C++
    /// `FORK_HEIGHTS`; 13 entries until fork 13 is scheduled, then 14 like a patched C++ node,
    /// 15 once fork 14 is scheduled too).
    #[must_use]
    pub fn upgrade_heights(&self) -> Vec<u64> {
        self.forks().iter().filter_map(|f| f.height).collect()
    }

    /// `/getinfo` `supported_height`: the height of the highest supported fork that is
    /// scheduled (C++ `FORK_HEIGHTS[CURRENT_FORK_INDEX]`; 2 200 000 while neither fork 13 nor
    /// fork 14 is scheduled, the fork-14 height once it is).
    #[must_use]
    pub fn supported_height(&self) -> u64 {
        let supported = self.supported_index();
        self.forks()
            .iter()
            .filter(|f| f.index <= supported)
            .filter_map(|f| f.height)
            .next_back()
            .unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::params::{
        BLOCK_BLOB_SHUFFLE_CHECK_HEIGHT, CURRENT_FORK_INDEX, DUST_THRESHOLD_V2_HEIGHT,
        FUSION_DUST_THRESHOLD_HEIGHT_V2, LWMA_2_DIFFICULTY_BLOCK_INDEX,
        LWMA_2_DIFFICULTY_BLOCK_INDEX_V2, LWMA_2_DIFFICULTY_BLOCK_INDEX_V3,
        LWMA_3_DIFFICULTY_BLOCK_INDEX, MAINNET, MAX_EXTRA_SIZE_V2_HEIGHT, MIXIN_LIMITS_V1_HEIGHT,
        MIXIN_LIMITS_V2_HEIGHT, MIXIN_LIMITS_V3_HEIGHT, MIXIN_LIMITS_V4_HEIGHT, TESTNET,
        TRANSACTION_SIGNATURE_COUNT_VALIDATION_HEIGHT, ZAWY_DIFFICULTY_BLOCK_INDEX,
    };

    fn height(index: u8) -> u64 {
        MAINNET.forks().fork(index).unwrap().height.unwrap()
    }

    /// Mainnet without fork 14 (the table before it was scheduled).
    fn mainnet_without_forks_13_and_14() -> ForkSchedule {
        MAINNET
            .with_fork13_height(None)
            .with_fork14_height(None)
            .forks()
    }

    #[test]
    fn table_matches_the_rule_constants() {
        assert_eq!(height(0), u64::from(ZAWY_DIFFICULTY_BLOCK_INDEX));
        assert_eq!(height(2), u64::from(MIXIN_LIMITS_V1_HEIGHT));
        assert_eq!(height(FORK_LWMA_2), LWMA_2_DIFFICULTY_BLOCK_INDEX);
        assert_eq!(height(3), u64::from(MIXIN_LIMITS_V2_HEIGHT));
        assert_eq!(height(3), u64::from(DUST_THRESHOLD_V2_HEIGHT));
        assert_eq!(height(FORK_LWMA_2_V2), LWMA_2_DIFFICULTY_BLOCK_INDEX_V2);
        assert_eq!(height(FORK_LWMA_2_V3), LWMA_2_DIFFICULTY_BLOCK_INDEX_V3);
        assert_eq!(height(5), u64::from(MIXIN_LIMITS_V3_HEIGHT));
        assert_eq!(height(5), u64::from(FUSION_DUST_THRESHOLD_HEIGHT_V2));
        assert_eq!(height(8), MAX_EXTRA_SIZE_V2_HEIGHT);
        assert_eq!(height(FORK_LWMA_3), LWMA_3_DIFFICULTY_BLOCK_INDEX);
        assert_eq!(OFF_TABLE_RULES[0].0, u64::from(MIXIN_LIMITS_V4_HEIGHT));
        assert_eq!(OFF_TABLE_RULES[1].0, BLOCK_BLOB_SHUFFLE_CHECK_HEIGHT);
        assert_eq!(
            OFF_TABLE_RULES[1].0,
            TRANSACTION_SIGNATURE_COUNT_VALIDATION_HEIGHT
        );
        for (i, f) in MAINNET.forks().forks().iter().enumerate() {
            assert_eq!(usize::from(f.index), i);
            assert_eq!(f.minor_version_signal, f.index);
            if f.index < FORK_14 {
                assert!(f.tx_versions.allows(1) && !f.tx_versions.allows(2));
            } else {
                assert!(f.tx_versions.allows(1) && f.tx_versions.allows(2));
            }
        }
        assert_eq!(usize::from(CURRENT_FORK_INDEX) + 2, FORK_COUNT);
        assert_eq!(MAINNET.forks().forks().len(), MAX_FORK_COUNT);
    }

    #[test]
    fn mainnet_fork_13_at_2_700_000_and_fork_14_at_2_750_000() {
        let s = MAINNET.forks();
        assert_eq!(s.fork13_height(), Some(2_700_000));
        assert_eq!(s.fork14_height(), Some(2_750_000));
        let mut heights = FORK_HEIGHTS.to_vec();
        heights.extend([2_700_000, 2_750_000]);
        assert_eq!(s.upgrade_heights(), heights);
        assert_eq!(s.supported_index(), FORK_14);
        assert_eq!(s.supported_height(), 2_750_000);
        assert_eq!(s.template_minor_version(5), FORK_14);
        // below fork 13 everything is as before
        for h in [2_617_000, 2_699_999, 2_700_000] {
            assert_eq!(s.block_major_version_for(h), 5);
            assert_eq!(s.tx_versions_at(h), TxVersions::V1);
            assert_eq!(s.coinbase_tx_version(h), 1);
            assert!(!s.is_tx_v2_active(h));
        }
        assert_eq!(s.fork_at(2_700_000).map(|f| f.index), Some(12));
        assert_eq!(s.next_fork(2_617_000).map(|f| f.index), Some(FORK_13));
        assert_eq!(s.next_fork(2_700_000).map(|f| f.index), Some(FORK_13));
        assert_eq!(s.signal_target(2_617_000).map(|f| f.index), Some(FORK_13));
        assert_eq!(s.upgrade_height(6), Some(2_700_000));
        // fork 13: major 6, version-1 transactions only
        for h in [2_700_001, 2_749_999, 2_750_000] {
            assert_eq!(s.block_major_version_for(h), 6);
            assert_eq!(s.tx_versions_at(h), TxVersions::V1);
            assert_eq!(s.coinbase_tx_version(h), 1);
            assert!(!s.is_tx_v2_active(h));
        }
        assert_eq!(s.fork_at(2_700_001).map(|f| f.index), Some(FORK_13));
        assert_eq!(s.next_fork(2_700_001).map(|f| f.index), Some(FORK_14));
        assert_eq!(s.next_fork(2_750_000).map(|f| f.index), Some(FORK_14));
        assert_eq!(s.signal_target(2_700_001).map(|f| f.index), Some(FORK_14));
        // from 2 750 001: major 7, v2 coinbase, both versions for the grace, then v2 only
        assert_eq!(s.block_major_version_for(2_750_001), 7);
        assert_eq!(s.fork_at(2_750_001).map(|f| f.index), Some(FORK_14));
        assert_eq!(s.coinbase_tx_version(2_750_001), 2);
        assert_eq!(s.tx_versions_at(2_750_001), TxVersions::V1_V2);
        assert_eq!(s.tx_versions_at(2_778_800), TxVersions::V1_V2);
        assert_eq!(s.tx_versions_at(2_778_801), TxVersions::V2);
        assert_eq!(s.next_fork(2_750_001), None);
        assert_eq!(s.signal_target(2_750_001), None);
        assert_eq!(s.upgrade_height(7), Some(2_750_000));
    }

    #[test]
    fn mainnet_is_unchanged_without_forks_13_and_14() {
        let s = mainnet_without_forks_13_and_14();
        assert_eq!(s.fork13_height(), None);
        assert_eq!(s.upgrade_heights(), FORK_HEIGHTS.to_vec());
        assert_eq!(s.supported_index(), SUPPORTED_FORK_INDEX);
        assert_eq!(s.supported_height(), 2_200_000);
        let v: Vec<u8> = (0..8).map(|h| s.block_major_version_for(h)).collect();
        assert_eq!(v, vec![1, 1, 2, 3, 4, 5, 5, 5]);
        assert_eq!(s.block_major_version_for(u64::from(u32::MAX)), 5);
        assert_eq!(s.fork_at(2_617_000).map(|f| f.index), Some(12));
        assert_eq!(s.fork_at(2_200_000).map(|f| f.index), Some(11));
        assert_eq!(s.fork_at(2_200_001).map(|f| f.index), Some(12));
        assert_eq!(s.fork_at(187_000), None);
        assert_eq!(s.next_fork(2_617_000), None);
        assert_eq!(s.next_fork(1).map(|f| f.index), Some(0));
        assert_eq!(s.next_fork(2_200_000).map(|f| f.index), Some(12));
        assert!(s.is_fork_active(12, 2_200_001) && !s.is_fork_active(12, 2_200_000));
        assert!(s.has_reached(12, 2_200_000) && !s.has_reached(12, 2_199_999));
        assert!(!s.is_fork_active(13, u64::MAX));
        assert_eq!(s.last_version_fork_at(u64::MAX), None);
        assert_eq!(s.signal_target(2_617_000).map(|f| f.index), Some(13));
        assert_eq!(s.upgrade_height(5), Some(4));
        assert_eq!(s.upgrade_height(6), None);
    }

    #[test]
    fn testnet_fork_13() {
        assert_eq!(TESTNET.with_fork13_height(Some(0)).fork13_height, None);
        let p = TESTNET.with_fork13_height(Some(300));
        let s = p.forks();
        assert_eq!(s.block_major_version_for(0), 1);
        assert_eq!(s.block_major_version_for(1), 5);
        assert_eq!(s.block_major_version_for(300), 5);
        assert_eq!(s.block_major_version_for(301), 6);
        assert_eq!(s.upgrade_height(6), Some(300));
        assert_eq!(
            s.next_fork(100).map(|f| (f.index, f.height)),
            Some((13, Some(300)))
        );
        assert_eq!(s.next_fork(300).map(|f| f.index), Some(13));
        assert_eq!(s.next_fork(301), None);
        assert_eq!(s.fork_at(301).map(|f| f.index), Some(13));
        assert_eq!(s.last_version_fork_at(301).map(|f| f.index), Some(13));
        assert_eq!(s.last_version_fork_at(300), None);
        assert_eq!(s.signal_target(301), None);
        let heights = s.upgrade_heights();
        assert_eq!(heights.len(), FORK_COUNT);
        assert_eq!(heights[usize::from(FORK_13)], 300);
        assert_eq!(s.supported_height(), 300);
    }

    #[test]
    fn fork_14_is_absent_until_scheduled() {
        let s = mainnet_without_forks_13_and_14();
        assert_eq!(s.fork14_height(), None);
        assert_eq!(s.forks().len(), FORK_COUNT);
        assert_eq!(s.fork(FORK_14), None);
        assert!(!s.is_tx_v2_active(u64::MAX));
        assert_eq!(s.tx_versions_at(u64::MAX), TxVersions::V1);
        assert!(s.allows_tx_version(u64::MAX, 0) && s.allows_tx_version(u64::MAX, 1));
        assert!(!s.allows_tx_version(u64::MAX, 2));
        assert_eq!(s.coinbase_tx_version(u64::MAX), 1);
        assert!(s.allows_coinbase_version(5, 0) && s.allows_coinbase_version(5, 1));
        assert!(!s.allows_coinbase_version(5, 2));
        assert_eq!(s.upgrade_height(7), None);
        let entry = s.fork14_entry();
        assert_eq!(entry.height, None);
        assert_eq!(entry.major_version, 7);
        assert!(entry.rules.contains(Rules::CLSAG) && entry.rules.contains(Rules::VIEW_TAGS));
        assert!(entry.tx_versions.allows(1) && entry.tx_versions.allows(2));
        assert_eq!(TESTNET.with_fork14_height(Some(0)).fork14_height, None);
    }

    #[test]
    fn testnet_fork_14_versions_and_grace() {
        let p = TESTNET
            .with_fork13_height(Some(10))
            .with_fork14_height(Some(20))
            .with_fork14_params(Fork14Params {
                grace_blocks: 5,
                v2_ring_size: RingSizeRule::MixinLimits,
            });
        let s = p.forks();
        assert_eq!(s.forks().len(), MAX_FORK_COUNT);
        assert_eq!(s.fork(FORK_14).and_then(|f| f.height), Some(20));
        assert_eq!(s.block_major_version_for(10), 5);
        assert_eq!(s.block_major_version_for(11), 6);
        assert_eq!(s.block_major_version_for(20), 6);
        assert_eq!(s.block_major_version_for(21), 7);
        assert_eq!(s.upgrade_height(7), Some(20));
        assert_eq!(s.fork_at(21).map(|f| f.index), Some(FORK_14));
        assert_eq!(s.next_fork(11).map(|f| f.index), Some(FORK_14));
        assert_eq!(s.last_version_fork_at(21).map(|f| f.index), Some(FORK_14));
        // versions: v1 up to F, both in F+1..=F+5, v2 only from F+6
        assert_eq!(s.tx_versions_at(20), TxVersions::V1);
        assert_eq!(s.tx_versions_at(21), TxVersions::V1_V2);
        assert_eq!(s.tx_versions_at(25), TxVersions::V1_V2);
        assert_eq!(s.tx_versions_at(26), TxVersions::V2);
        assert!(!s.allows_tx_version(20, 2) && s.allows_tx_version(21, 2));
        assert!(s.allows_tx_version(25, 1) && s.allows_tx_version(25, 0));
        assert!(!s.allows_tx_version(26, 1) && !s.allows_tx_version(26, 0));
        assert!(!s.allows_tx_version(26, 3));
        // coinbase: v1 up to F, exactly v2 after
        assert!(s.allows_coinbase_version(20, 1) && !s.allows_coinbase_version(20, 2));
        assert!(s.allows_coinbase_version(21, 2) && !s.allows_coinbase_version(21, 1));
        assert_eq!(s.coinbase_tx_version(21), 2);
        let heights = s.upgrade_heights();
        assert_eq!(heights.len(), MAX_FORK_COUNT);
        assert_eq!(heights[usize::from(FORK_14)], 20);
        assert_eq!(s.supported_index(), FORK_14);
        assert_eq!(s.supported_height(), 20);
    }

    #[test]
    fn signals() {
        assert!(signals_fork(13, 13));
        assert!(signals_fork(14, 13));
        assert!(!signals_fork(0, 13));
        assert!(!signals_fork(P2POOL_MINOR_VERSION, 13));
        assert_eq!(template_minor_version(5), SUPPORTED_FORK_INDEX);
        assert_eq!(template_minor_version(1), 0);
        let f14 = TESTNET
            .with_fork13_height(Some(10))
            .with_fork14_height(Some(20))
            .forks();
        assert_eq!(f14.template_minor_version(6), FORK_14);
        assert!(signals_fork(f14.template_minor_version(6), FORK_13));
        assert_eq!(TESTNET.forks().template_minor_version(5), FORK_13);
        assert_eq!(f14.template_minor_version(1), 0);
    }
}
