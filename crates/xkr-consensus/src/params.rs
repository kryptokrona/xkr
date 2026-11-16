//! Every constant of `src/config/cryptonote_config.h`, with the `#ifdef USE_TESTNET` switches
//! turned into a runtime [`Network`] (see [`NetworkParams`]).

/// Which chain the parameters describe (C++ selects these at compile time with `USE_TESTNET`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Network {
    /// Kryptokrona mainnet.
    #[default]
    Mainnet,
    /// The `USE_TESTNET` build.
    Testnet,
}

/// The network-dependent values (`#ifdef USE_TESTNET` blocks and `Currency::m_testnet`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NetworkParams {
    /// Which network.
    pub network: Network,
    /// `CRYPTONOTE_MINED_MONEY_UNLOCK_WINDOW`.
    pub mined_money_unlock_window: u32,
    /// `TESTNET_FIXED_DIFFICULTY` (0 = use the real algorithm).
    pub fixed_difficulty: u64,
    /// `UPGRADE_HEIGHT_V2..V5`.
    pub upgrade_heights: [u32; 4],
    /// Height of fork 13 (block major version 6, `docs/forks.md`); `None` = not scheduled.
    /// The block after it is the first v6 block. See [`crate::forks`].
    pub fork13_height: Option<u32>,
    /// Height of fork 14 (block major version 7, transaction version 2, `docs/tx-v2.md`);
    /// `None` = not scheduled. The block after it is the first v7 block.
    pub fork14_height: Option<u32>,
    /// The parameters of fork 14 (grace period for version-1 transactions, ring sizes).
    pub fork14: crate::forks::Fork14Params,
    /// `P2P_DEFAULT_PORT`.
    pub p2p_port: u16,
    /// `RPC_DEFAULT_PORT`.
    pub rpc_port: u16,
    /// `CRYPTONOTE_NETWORK` (Levin handshake network id).
    pub network_id: [u8; 16],
    /// `SEED_NODES`.
    pub seed_nodes: &'static [&'static str],
    /// Genesis nonce. `Currency::generateGenesisBlock` uses 70 and `++nonce` only if
    /// `Currency::m_testnet`, but nothing ever calls `CurrencyBuilder::testnet(true)`, so the
    /// `USE_TESTNET` build also has nonce 70 and the mainnet genesis hash (confirmed against a
    /// master-built testnet daemon, 2026-11-12).
    pub genesis_nonce: u32,
}

/// Mainnet parameters.
pub const MAINNET: NetworkParams = NetworkParams {
    network: Network::Mainnet,
    mined_money_unlock_window: 20,
    fixed_difficulty: 0,
    upgrade_heights: [1, 2, 3, 4],
    fork13_height: crate::forks::FORK_13_HEIGHT_MAINNET,
    fork14_height: crate::forks::FORK_14_HEIGHT_MAINNET,
    fork14: crate::forks::FORK_14_PARAMS_MAINNET,
    p2p_port: 11897,
    rpc_port: 11898,
    network_id: [
        0xf1, 0x4b, 0xb8, 0xc8, 0xb2, 0x56, 0x45, 0x2e, 0xee, 0xf0, 0xb4, 0x99, 0xab, 0x71, 0x6c,
        0xcc,
    ],
    seed_nodes: &[
        "185.238.128.190:11897", // dutchpool
        "172.94.14.157:11897",   // xkr.network
        "95.216.136.222:11897",  // TechyPool
        "130.237.83.246:20191",  // KTH Node
        "149.28.87.122:11897",   // node.xkr.la
    ],
    genesis_nonce: 70,
};

/// Testnet parameters (`USE_TESTNET`).
pub const TESTNET: NetworkParams = NetworkParams {
    network: Network::Testnet,
    mined_money_unlock_window: 1,
    fixed_difficulty: 1,
    upgrade_heights: [0, 0, 0, 0],
    fork13_height: crate::forks::FORK_13_HEIGHT_TESTNET,
    fork14_height: crate::forks::FORK_14_HEIGHT_TESTNET,
    fork14: crate::forks::FORK_14_PARAMS_TESTNET,
    p2p_port: 11898,
    rpc_port: 11899,
    network_id: [
        0xf1, 0x4c, 0xb8, 0xc8, 0xb3, 0x56, 0x45, 0x2e, 0xee, 0xf0, 0xb4, 0x99, 0xab, 0x71, 0x6c,
        0xcc,
    ],
    seed_nodes: &["127.0.0.1:11898"],
    genesis_nonce: 70,
};

impl NetworkParams {
    /// The fork table of these parameters.
    #[must_use]
    pub const fn forks(&self) -> crate::forks::ForkSchedule {
        crate::forks::ForkSchedule::for_params(self)
    }

    /// These parameters with fork 13 at `height` (`None` or `Some(0)`: not scheduled). Meant for
    /// testnets (`xkrd --fork13-height`); mainnet's height is a source constant.
    #[must_use]
    pub const fn with_fork13_height(self, height: Option<u32>) -> Self {
        let fork13_height = match height {
            Some(0) | None => None,
            Some(h) => Some(h),
        };
        Self {
            fork13_height,
            ..self
        }
    }

    /// These parameters with fork 14 at `height` (`None` or `Some(0)`: not scheduled). Meant for
    /// testnets (`xkrd --fork14-height`, mirroring `--fork13-height`); mainnet's height is a
    /// source constant. Schedule it after fork 13.
    #[must_use]
    pub const fn with_fork14_height(self, height: Option<u32>) -> Self {
        let fork14_height = match height {
            Some(0) | None => None,
            Some(h) => Some(h),
        };
        Self {
            fork14_height,
            ..self
        }
    }

    /// These parameters with other fork-14 parameters (grace period, ring sizes).
    #[must_use]
    pub const fn with_fork14_params(self, fork14: crate::forks::Fork14Params) -> Self {
        Self { fork14, ..self }
    }
}

impl Network {
    /// The parameter set of this network.
    #[must_use]
    pub const fn params(self) -> &'static NetworkParams {
        match self {
            Self::Mainnet => &MAINNET,
            Self::Testnet => &TESTNET,
        }
    }
}

// ---------------------------------------------------------------------------------------------
// parameters:: (network independent)
// ---------------------------------------------------------------------------------------------

/// `DIFFICULTY_TARGET` (seconds).
pub const DIFFICULTY_TARGET: u64 = 90;
/// `CRYPTONOTE_MAX_BLOCK_NUMBER`: unlock times below this are block indexes.
pub const CRYPTONOTE_MAX_BLOCK_NUMBER: u32 = 500_000_000;
/// `CRYPTONOTE_MAX_BLOCK_BLOB_SIZE`. Kept for C++ parity; it never binds (above the 100 MB Levin
/// and 2 MB RPC limits). The binding limit on received blocks is the consensus cumulative size,
/// applied to the raw bytes before decoding (`xkr_chain::prepare::check_raw_sizes`).
pub const CRYPTONOTE_MAX_BLOCK_BLOB_SIZE: usize = 500_000_000;
/// `CRYPTONOTE_MAX_TX_SIZE`. Checked like C++ but never binds (see
/// [`CRYPTONOTE_MAX_BLOCK_BLOB_SIZE`]); pool transactions are bounded by
/// `max_transaction_allowed_size`, checked before decoding (`xkr-txpool`).
pub const CRYPTONOTE_MAX_TX_SIZE: usize = 1_000_000_000;
/// `CRYPTONOTE_PUBLIC_ADDRESS_BASE58_PREFIX` (`SEKR...`).
pub const CRYPTONOTE_PUBLIC_ADDRESS_BASE58_PREFIX: u64 = 2_239_254;
/// `CRYPTONOTE_PUBLIC_ADDRESS_BASE58_PREFIX_ALT` (`Xkr...`).
pub const CRYPTONOTE_PUBLIC_ADDRESS_BASE58_PREFIX_ALT: u64 = 45_239;
/// `CRYPTONOTE_MINED_MONEY_UNLOCK_WINDOW` on mainnet (testnet: 1, see [`NetworkParams`]).
pub const CRYPTONOTE_MINED_MONEY_UNLOCK_WINDOW: u32 = 20;
/// `CRYPTONOTE_BLOCK_FUTURE_TIME_LIMIT`.
pub const CRYPTONOTE_BLOCK_FUTURE_TIME_LIMIT: u64 = 60 * 60 * 2;
/// `CRYPTONOTE_BLOCK_FUTURE_TIME_LIMIT_V3`.
pub const CRYPTONOTE_BLOCK_FUTURE_TIME_LIMIT_V3: u64 = 3 * DIFFICULTY_TARGET;
/// `CRYPTONOTE_BLOCK_FUTURE_TIME_LIMIT_V4`.
pub const CRYPTONOTE_BLOCK_FUTURE_TIME_LIMIT_V4: u64 = 6 * DIFFICULTY_TARGET;
/// `BLOCKCHAIN_TIMESTAMP_CHECK_WINDOW`.
pub const BLOCKCHAIN_TIMESTAMP_CHECK_WINDOW: usize = 60;
/// `BLOCKCHAIN_TIMESTAMP_CHECK_WINDOW_V3`.
pub const BLOCKCHAIN_TIMESTAMP_CHECK_WINDOW_V3: usize = 11;
/// `MONEY_SUPPLY`.
pub const MONEY_SUPPLY: u64 = 100_000_000_000_000;
/// `ZAWY_DIFFICULTY_BLOCK_INDEX`.
pub const ZAWY_DIFFICULTY_BLOCK_INDEX: u32 = 187_000;
/// `ZAWY_DIFFICULTY_V2`.
pub const ZAWY_DIFFICULTY_V2: usize = 0;
/// `ZAWY_DIFFICULTY_DIFFICULTY_BLOCK_VERSION`.
pub const ZAWY_DIFFICULTY_DIFFICULTY_BLOCK_VERSION: u8 = 3;
/// `LWMA_2_DIFFICULTY_BLOCK_INDEX`.
pub const LWMA_2_DIFFICULTY_BLOCK_INDEX: u64 = 620_000;
/// `LWMA_2_DIFFICULTY_BLOCK_INDEX_V2`.
pub const LWMA_2_DIFFICULTY_BLOCK_INDEX_V2: u64 = 700_000;
/// `LWMA_2_DIFFICULTY_BLOCK_INDEX_V3`.
pub const LWMA_2_DIFFICULTY_BLOCK_INDEX_V3: u64 = 800_000;
/// `LWMA_3_DIFFICULTY_BLOCK_INDEX`.
pub const LWMA_3_DIFFICULTY_BLOCK_INDEX: u64 = 2_200_000;
/// `EMISSION_SPEED_FACTOR`.
pub const EMISSION_SPEED_FACTOR: u32 = 21;
/// `GENESIS_BLOCK_REWARD` (no premine).
pub const GENESIS_BLOCK_REWARD: u64 = 0;
/// `GENESIS_COINBASE_TX_HEX`.
pub const GENESIS_COINBASE_TX_HEX: &str = "010a01ff000188f3b501029b2e4c0281c0b02e7c53291a94d1d0cbff8883f8024f5142ee494ffbbd088071210142694232c5b04151d9e4c27d31ec7a68ea568b19488cfcb422659a07a0e44dd5";
/// `GENESIS_BLOCK_TIMESTAMP` (wallet import hint, not the genesis block's timestamp, which is 0).
pub const GENESIS_BLOCK_TIMESTAMP: u64 = 1_512_800_692;
/// `CRYPTONOTE_REWARD_BLOCKS_WINDOW`.
pub const CRYPTONOTE_REWARD_BLOCKS_WINDOW: usize = 100;
/// `CRYPTONOTE_BLOCK_GRANTED_FULL_REWARD_ZONE` (major >= 3).
pub const CRYPTONOTE_BLOCK_GRANTED_FULL_REWARD_ZONE: usize = 100_000;
/// `CRYPTONOTE_BLOCK_GRANTED_FULL_REWARD_ZONE_V2` (major 2).
pub const CRYPTONOTE_BLOCK_GRANTED_FULL_REWARD_ZONE_V2: usize = 20_000;
/// `CRYPTONOTE_BLOCK_GRANTED_FULL_REWARD_ZONE_V1` (major 1).
pub const CRYPTONOTE_BLOCK_GRANTED_FULL_REWARD_ZONE_V1: usize = 10_000;
/// `CRYPTONOTE_BLOCK_GRANTED_FULL_REWARD_ZONE_CURRENT`.
pub const CRYPTONOTE_BLOCK_GRANTED_FULL_REWARD_ZONE_CURRENT: usize =
    CRYPTONOTE_BLOCK_GRANTED_FULL_REWARD_ZONE;
/// `CRYPTONOTE_COINBASE_BLOB_RESERVED_SIZE`.
pub const CRYPTONOTE_COINBASE_BLOB_RESERVED_SIZE: usize = 600;
/// `CRYPTONOTE_DISPLAY_DECIMAL_POINT`.
pub const CRYPTONOTE_DISPLAY_DECIMAL_POINT: usize = 5;
/// `MINIMUM_FEE`.
pub const MINIMUM_FEE: u64 = 10;
/// `MINIMUM_MIXIN_V1`.
pub const MINIMUM_MIXIN_V1: u64 = 0;
/// `MAXIMUM_MIXIN_V1`.
pub const MAXIMUM_MIXIN_V1: u64 = 100;
/// `MINIMUM_MIXIN_V2`.
pub const MINIMUM_MIXIN_V2: u64 = 7;
/// `MAXIMUM_MIXIN_V2`.
pub const MAXIMUM_MIXIN_V2: u64 = 7;
/// `MINIMUM_MIXIN_V3`.
pub const MINIMUM_MIXIN_V3: u64 = 3;
/// `MAXIMUM_MIXIN_V3`.
pub const MAXIMUM_MIXIN_V3: u64 = 3;
/// `MINIMUM_MIXIN_V4`.
pub const MINIMUM_MIXIN_V4: u64 = 1;
/// `MAXIMUM_MIXIN_V4`.
pub const MAXIMUM_MIXIN_V4: u64 = 5;
/// `MIXIN_LIMITS_V1_HEIGHT`.
pub const MIXIN_LIMITS_V1_HEIGHT: u32 = 440_000;
/// `MIXIN_LIMITS_V2_HEIGHT`.
pub const MIXIN_LIMITS_V2_HEIGHT: u32 = 620_000;
/// `MIXIN_LIMITS_V3_HEIGHT`.
pub const MIXIN_LIMITS_V3_HEIGHT: u32 = 800_000;
/// `MIXIN_LIMITS_V4_HEIGHT`.
pub const MIXIN_LIMITS_V4_HEIGHT: u32 = 1_250_000;
/// `DEFAULT_MIXIN_V0`.
pub const DEFAULT_MIXIN_V0: u64 = 3;
/// `DEFAULT_MIXIN_V1`.
pub const DEFAULT_MIXIN_V1: u64 = MAXIMUM_MIXIN_V1;
/// `DEFAULT_MIXIN_V2`.
pub const DEFAULT_MIXIN_V2: u64 = MAXIMUM_MIXIN_V2;
/// `DEFAULT_MIXIN_V3`.
pub const DEFAULT_MIXIN_V3: u64 = MAXIMUM_MIXIN_V3;
/// `DEFAULT_MIXIN_V4`.
pub const DEFAULT_MIXIN_V4: u64 = MAXIMUM_MIXIN_V4;
/// `DEFAULT_DUST_THRESHOLD`.
pub const DEFAULT_DUST_THRESHOLD: u64 = 10;
/// `DEFAULT_DUST_THRESHOLD_V2`.
pub const DEFAULT_DUST_THRESHOLD_V2: u64 = 0;
/// `DUST_THRESHOLD_V2_HEIGHT`.
pub const DUST_THRESHOLD_V2_HEIGHT: u32 = MIXIN_LIMITS_V2_HEIGHT;
/// `FUSION_DUST_THRESHOLD_HEIGHT_V2`.
pub const FUSION_DUST_THRESHOLD_HEIGHT_V2: u32 = 800_000;
/// `EXPECTED_NUMBER_OF_BLOCKS_PER_DAY`.
pub const EXPECTED_NUMBER_OF_BLOCKS_PER_DAY: u64 = 24 * 60 * 60 / DIFFICULTY_TARGET;
/// `DIFFICULTY_WINDOW` (major >= 3, before LWMA).
pub const DIFFICULTY_WINDOW: usize = 17;
/// `DIFFICULTY_WINDOW_V1`.
pub const DIFFICULTY_WINDOW_V1: usize = 2880;
/// `DIFFICULTY_WINDOW_V2`.
pub const DIFFICULTY_WINDOW_V2: usize = 2880;
/// `DIFFICULTY_WINDOW_V3` (LWMA `N`).
pub const DIFFICULTY_WINDOW_V3: u64 = 60;
/// `DIFFICULTY_BLOCKS_COUNT_V3`.
pub const DIFFICULTY_BLOCKS_COUNT_V3: u64 = DIFFICULTY_WINDOW_V3 + 1;
/// `DIFFICULTY_CUT`.
pub const DIFFICULTY_CUT: usize = 0;
/// `DIFFICULTY_CUT_V1`.
pub const DIFFICULTY_CUT_V1: usize = 60;
/// `DIFFICULTY_CUT_V2`.
pub const DIFFICULTY_CUT_V2: usize = 60;
/// `DIFFICULTY_LAG`.
pub const DIFFICULTY_LAG: usize = 0;
/// `DIFFICULTY_LAG_V1`.
pub const DIFFICULTY_LAG_V1: usize = 15;
/// `DIFFICULTY_LAG_V2`.
pub const DIFFICULTY_LAG_V2: usize = 15;
/// `MAX_BLOCK_SIZE_INITIAL`.
pub const MAX_BLOCK_SIZE_INITIAL: usize = 100_000;
/// `MAX_BLOCK_SIZE_GROWTH_SPEED_NUMERATOR`.
pub const MAX_BLOCK_SIZE_GROWTH_SPEED_NUMERATOR: u64 = 100 * 1024;
/// `MAX_BLOCK_SIZE_GROWTH_SPEED_DENOMINATOR`.
pub const MAX_BLOCK_SIZE_GROWTH_SPEED_DENOMINATOR: u64 = 365 * 24 * 60 * 60 / DIFFICULTY_TARGET;
/// `MAX_EXTRA_SIZE`.
pub const MAX_EXTRA_SIZE: u64 = 2200;
/// `MAX_EXTRA_SIZE_V2`.
pub const MAX_EXTRA_SIZE_V2: u64 = 1024;
/// `MAX_EXTRA_SIZE_V2_HEIGHT`.
pub const MAX_EXTRA_SIZE_V2_HEIGHT: u64 = 1_300_000;
/// `MAX_EXTRA_SIZE_POOL`.
pub const MAX_EXTRA_SIZE_POOL: u64 = 2200;
/// `MAX_EXTRA_SIZE_BLOCK`.
pub const MAX_EXTRA_SIZE_BLOCK: u64 = 128;
/// `BLOCK_BLOB_SHUFFLE_CHECK_HEIGHT`.
pub const BLOCK_BLOB_SHUFFLE_CHECK_HEIGHT: u64 = 1_750_000;
/// `TRANSACTION_SIGNATURE_COUNT_VALIDATION_HEIGHT`.
pub const TRANSACTION_SIGNATURE_COUNT_VALIDATION_HEIGHT: u64 = 1_750_000;
/// `CRYPTONOTE_LOCKED_TX_ALLOWED_DELTA_BLOCKS`.
pub const CRYPTONOTE_LOCKED_TX_ALLOWED_DELTA_BLOCKS: u64 = 1;
/// `CRYPTONOTE_LOCKED_TX_ALLOWED_DELTA_SECONDS`.
pub const CRYPTONOTE_LOCKED_TX_ALLOWED_DELTA_SECONDS: u64 =
    DIFFICULTY_TARGET * CRYPTONOTE_LOCKED_TX_ALLOWED_DELTA_BLOCKS;
/// `CRYPTONOTE_MEMPOOL_TX_LIVETIME`.
pub const CRYPTONOTE_MEMPOOL_TX_LIVETIME: u64 = 60 * 60 * 24;
/// `CRYPTONOTE_MEMPOOL_TX_FROM_ALT_BLOCK_LIVETIME`.
pub const CRYPTONOTE_MEMPOOL_TX_FROM_ALT_BLOCK_LIVETIME: u64 = 60 * 10;
/// `CRYPTONOTE_NUMBER_OF_PERIODS_TO_FORGET_TX_DELETED_FROM_POOL`.
pub const CRYPTONOTE_NUMBER_OF_PERIODS_TO_FORGET_TX_DELETED_FROM_POOL: u64 = 7;
/// `FUSION_TX_MAX_SIZE`.
pub const FUSION_TX_MAX_SIZE: usize = CRYPTONOTE_BLOCK_GRANTED_FULL_REWARD_ZONE_CURRENT * 30 / 100;
/// `FUSION_TX_MIN_INPUT_COUNT`.
pub const FUSION_TX_MIN_INPUT_COUNT: usize = 12;
/// `FUSION_TX_MIN_IN_OUT_COUNT_RATIO`.
pub const FUSION_TX_MIN_IN_OUT_COUNT_RATIO: usize = 4;
/// `CRYPTONOTE_MEMPOOL_MAX_FUSION_TRANSACTIONS` (relay policy).
pub const CRYPTONOTE_MEMPOOL_MAX_FUSION_TRANSACTIONS: usize = 256;
/// `CRYPTONOTE_MEMPOOL_RECENTLY_SEEN_TX_LIVETIME` (relay policy).
pub const CRYPTONOTE_MEMPOOL_RECENTLY_SEEN_TX_LIVETIME: u64 = 60 * 10;
/// `UPGRADE_HEIGHT_V2` (mainnet).
pub const UPGRADE_HEIGHT_V2: u32 = 1;
/// `UPGRADE_HEIGHT_V3` (mainnet).
pub const UPGRADE_HEIGHT_V3: u32 = 2;
/// `UPGRADE_HEIGHT_V4` (mainnet; CN-Lite variant 1).
pub const UPGRADE_HEIGHT_V4: u32 = 3;
/// `UPGRADE_HEIGHT_V5` (mainnet; CN-Turtle variant 2).
pub const UPGRADE_HEIGHT_V5: u32 = 4;
/// `UPGRADE_HEIGHT_CURRENT`.
pub const UPGRADE_HEIGHT_CURRENT: u32 = UPGRADE_HEIGHT_V5;
/// `UPGRADE_VOTING_THRESHOLD` (percent).
pub const UPGRADE_VOTING_THRESHOLD: u32 = 90;
/// `UPGRADE_VOTING_WINDOW`.
pub const UPGRADE_VOTING_WINDOW: u32 = EXPECTED_NUMBER_OF_BLOCKS_PER_DAY as u32;
/// `UPGRADE_WINDOW`.
pub const UPGRADE_WINDOW: u32 = EXPECTED_NUMBER_OF_BLOCKS_PER_DAY as u32;
/// `FORK_HEIGHTS` of C++ v1.2.2 (indexes 0..=12; fork 13 lives in [`crate::forks`]).
pub const FORK_HEIGHTS: [u64; 13] = [
    187_000, 350_000, 440_000, 620_000, 700_000, 800_000, 1_000_000, 1_200_000, 1_300_000,
    1_400_000, 1_600_000, 1_800_000, 2_200_000,
];
/// `SOFTWARE_SUPPORTED_FORK_INDEX` of C++ v1.2.2 (12). xkrd itself supports
/// [`crate::forks::SUPPORTED_FORK_INDEX`] (13); the fork table is [`crate::forks::ForkSchedule`].
pub const SOFTWARE_SUPPORTED_FORK_INDEX: u64 = 12;
/// `FORK_HEIGHTS_SIZE`.
pub const FORK_HEIGHTS_SIZE: u64 = FORK_HEIGHTS.len() as u64;
/// `CURRENT_FORK_INDEX`.
pub const CURRENT_FORK_INDEX: u8 = if FORK_HEIGHTS_SIZE == 0 {
    0
} else {
    SOFTWARE_SUPPORTED_FORK_INDEX as u8
};

/// `CRYPTONOTE_NAME`.
pub const CRYPTONOTE_NAME: &str = "kryptokrona";
/// `TRANSACTION_VERSION_1`.
pub const TRANSACTION_VERSION_1: u8 = 1;
/// `TRANSACTION_VERSION_2`.
pub const TRANSACTION_VERSION_2: u8 = 2;
/// `CURRENT_TRANSACTION_VERSION`.
pub const CURRENT_TRANSACTION_VERSION: u8 = TRANSACTION_VERSION_1;
/// `BLOCK_MAJOR_VERSION_1`.
pub const BLOCK_MAJOR_VERSION_1: u8 = 1;
/// `BLOCK_MAJOR_VERSION_2`.
pub const BLOCK_MAJOR_VERSION_2: u8 = 2;
/// `BLOCK_MAJOR_VERSION_3`.
pub const BLOCK_MAJOR_VERSION_3: u8 = 3;
/// `BLOCK_MAJOR_VERSION_4`.
pub const BLOCK_MAJOR_VERSION_4: u8 = 4;
/// `BLOCK_MAJOR_VERSION_5`.
pub const BLOCK_MAJOR_VERSION_5: u8 = 5;
/// `BLOCK_MAJOR_VERSION_6` (fork 13; identical to version 5 apart from the number).
pub const BLOCK_MAJOR_VERSION_6: u8 = 6;
/// Block major version 7 (fork 14: transaction version 2, `docs/tx-v2.md`; the block format,
/// proof of work, difficulty and reward are those of version 5).
pub const BLOCK_MAJOR_VERSION_7: u8 = 7;
/// `BLOCK_MINOR_VERSION_0`.
pub const BLOCK_MINOR_VERSION_0: u8 = 0;
/// `BLOCK_MINOR_VERSION_1`.
pub const BLOCK_MINOR_VERSION_1: u8 = 1;
/// `BLOCKS_IDS_SYNCHRONIZING_DEFAULT_COUNT`.
pub const BLOCKS_IDS_SYNCHRONIZING_DEFAULT_COUNT: usize = 10_000;
/// `BLOCKS_SYNCHRONIZING_DEFAULT_COUNT`.
pub const BLOCKS_SYNCHRONIZING_DEFAULT_COUNT: u64 = 100;
/// `COMMAND_RPC_GET_BLOCKS_FAST_MAX_COUNT`. Unused: xkrd has no `/getblocks` or
/// `/queryblocks` route; one added later must cap its block count with this and its answer in
/// bytes.
pub const COMMAND_RPC_GET_BLOCKS_FAST_MAX_COUNT: usize = 1000;
/// `SERVICE_DEFAULT_PORT`.
pub const SERVICE_DEFAULT_PORT: u16 = 8070;
/// `P2P_LOCAL_WHITE_PEERLIST_LIMIT`.
pub const P2P_LOCAL_WHITE_PEERLIST_LIMIT: usize = 1000;
/// `P2P_LOCAL_GRAY_PEERLIST_LIMIT`.
pub const P2P_LOCAL_GRAY_PEERLIST_LIMIT: usize = 5000;
/// `P2P_CURRENT_VERSION`.
pub const P2P_CURRENT_VERSION: u8 = 5;
/// `P2P_MINIMUM_VERSION`.
pub const P2P_MINIMUM_VERSION: u8 = 4;
/// `P2P_LITE_BLOCKS_PROPOGATION_VERSION`.
pub const P2P_LITE_BLOCKS_PROPOGATION_VERSION: u8 = 4;
/// `P2P_UPGRADE_WINDOW`.
pub const P2P_UPGRADE_WINDOW: u8 = 2;
/// `P2P_CONNECTION_MAX_WRITE_BUFFER_SIZE`.
pub const P2P_CONNECTION_MAX_WRITE_BUFFER_SIZE: usize = 32 * 1024 * 1024;
/// `P2P_DEFAULT_CONNECTIONS_COUNT`.
pub const P2P_DEFAULT_CONNECTIONS_COUNT: u32 = 8;
/// `P2P_DEFAULT_WHITELIST_CONNECTIONS_PERCENT`.
pub const P2P_DEFAULT_WHITELIST_CONNECTIONS_PERCENT: usize = 70;
/// `P2P_DEFAULT_HANDSHAKE_INTERVAL` (seconds).
pub const P2P_DEFAULT_HANDSHAKE_INTERVAL: u32 = 60;
/// `P2P_DEFAULT_PACKET_MAX_SIZE`.
pub const P2P_DEFAULT_PACKET_MAX_SIZE: u32 = 50_000_000;
/// `P2P_DEFAULT_PEERS_IN_HANDSHAKE`.
pub const P2P_DEFAULT_PEERS_IN_HANDSHAKE: u32 = 250;
/// `P2P_DEFAULT_CONNECTION_TIMEOUT` (ms).
pub const P2P_DEFAULT_CONNECTION_TIMEOUT: u32 = 5000;
/// `P2P_DEFAULT_PING_CONNECTION_TIMEOUT` (ms).
pub const P2P_DEFAULT_PING_CONNECTION_TIMEOUT: u32 = 2000;
/// `P2P_DEFAULT_INVOKE_TIMEOUT` (ms).
pub const P2P_DEFAULT_INVOKE_TIMEOUT: u64 = 60 * 2 * 1000;
/// `P2P_DEFAULT_HANDSHAKE_INVOKE_TIMEOUT` (ms).
pub const P2P_DEFAULT_HANDSHAKE_INVOKE_TIMEOUT: usize = 5000;
/// `P2P_STAT_TRUSTED_PUB_KEY`.
pub const P2P_STAT_TRUSTED_PUB_KEY: &str = "";
/// `DATABASE_WRITE_BUFFER_MB_DEFAULT_SIZE`.
pub const DATABASE_WRITE_BUFFER_MB_DEFAULT_SIZE: u64 = 256;
/// `DATABASE_READ_BUFFER_MB_DEFAULT_SIZE`.
pub const DATABASE_READ_BUFFER_MB_DEFAULT_SIZE: u64 = 256;
/// `DATABASE_DEFAULT_MAX_OPEN_FILES`.
pub const DATABASE_DEFAULT_MAX_OPEN_FILES: u32 = 512;
/// `DATABASE_DEFAULT_BACKGROUND_THREADS_COUNT`.
pub const DATABASE_DEFAULT_BACKGROUND_THREADS_COUNT: u16 = 4;
/// `LATEST_VERSION_URL`.
pub const LATEST_VERSION_URL: &str = "https://github.com/kryptokrona/kryptokrona";
/// `LICENSE_URL`.
pub const LICENSE_URL: &str = "https://github.com/kryptokrona/kryptokrona/blob/master/LICENSE";
/// `CRYPTONOTE_BLOCKS_FILENAME`.
pub const CRYPTONOTE_BLOCKS_FILENAME: &str = "blocks.bin";
/// `CRYPTONOTE_BLOCKINDEXES_FILENAME`.
pub const CRYPTONOTE_BLOCKINDEXES_FILENAME: &str = "blockindexes.bin";
/// `CRYPTONOTE_POOLDATA_FILENAME`.
pub const CRYPTONOTE_POOLDATA_FILENAME: &str = "poolstate.bin";
/// `P2P_NET_DATA_FILENAME`.
pub const P2P_NET_DATA_FILENAME: &str = "p2pstate.bin";
/// `MINER_CONFIG_FILE_NAME`.
pub const MINER_CONFIG_FILE_NAME: &str = "miner_conf.json";

/// Index into [`FORK_HEIGHTS`] of the next fork at or after `height` (`None` once past the last
/// one), as used by the C++ "upgrade needed" warnings.
#[must_use]
pub fn next_fork_index(height: u64) -> Option<usize> {
    FORK_HEIGHTS.iter().position(|h| *h >= height)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derived_constants() {
        assert_eq!(EXPECTED_NUMBER_OF_BLOCKS_PER_DAY, 960);
        assert_eq!(MAX_BLOCK_SIZE_GROWTH_SPEED_DENOMINATOR, 350_400);
        assert_eq!(FUSION_TX_MAX_SIZE, 30_000);
        assert_eq!(CURRENT_FORK_INDEX, 12);
        assert_eq!(
            FORK_HEIGHTS[CURRENT_FORK_INDEX as usize],
            LWMA_3_DIFFICULTY_BLOCK_INDEX
        );
        assert_eq!(next_fork_index(2_617_000), None);
        assert_eq!(next_fork_index(1), Some(0));
    }
}
