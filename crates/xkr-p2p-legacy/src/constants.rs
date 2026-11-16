//! P2P constants of the C++ node.
//!
//! Mirrors `src/config/cryptonote_config.h` (namespace `CryptoNote`, P2P section) and
//! `src/p2p/levin_protocol.cpp` of kryptokrona v1.1.x. Names are kept identical so a grep
//! against the C++ tree finds the source.

use std::time::Duration;

/// `P2P_CURRENT_VERSION` — the version we announce in `basic_node_data.version`.
pub const P2P_CURRENT_VERSION: u8 = 5;
/// `P2P_MINIMUM_VERSION` — peers below this are dropped after the handshake.
pub const P2P_MINIMUM_VERSION: u8 = 4;
/// `P2P_LITE_BLOCKS_PROPOGATION_VERSION` (sic) — peers at or above get `NOTIFY_NEW_LITE_BLOCK`
/// instead of `NOTIFY_NEW_BLOCK` when relaying.
pub const P2P_LITE_BLOCKS_PROPAGATION_VERSION: u8 = 4;
/// `P2P_UPGRADE_WINDOW` — only used for an "update your software" warning.
pub const P2P_UPGRADE_WINDOW: u8 = 2;

/// `P2P_LOCAL_WHITE_PEERLIST_LIMIT`
pub const P2P_LOCAL_WHITE_PEERLIST_LIMIT: usize = 1000;
/// `P2P_LOCAL_GRAY_PEERLIST_LIMIT`
pub const P2P_LOCAL_GRAY_PEERLIST_LIMIT: usize = 5000;

/// `P2P_CONNECTION_MAX_WRITE_BUFFER_SIZE` (32 MiB): the C++ node interrupts a connection
/// whose write queue grows past this.
pub const P2P_CONNECTION_MAX_WRITE_BUFFER_SIZE: usize = 32 * 1024 * 1024;
/// `P2P_DEFAULT_CONNECTIONS_COUNT` — outgoing connections the C++ node maintains.
pub const P2P_DEFAULT_CONNECTIONS_COUNT: usize = 8;
/// `P2P_DEFAULT_WHITELIST_CONNECTIONS_PERCENT`
pub const P2P_DEFAULT_WHITELIST_CONNECTIONS_PERCENT: usize = 70;
/// `P2P_DEFAULT_HANDSHAKE_INTERVAL` (60 s) — the timed-sync period.
pub const P2P_DEFAULT_HANDSHAKE_INTERVAL: Duration = Duration::from_secs(60);
/// `P2P_DEFAULT_PACKET_MAX_SIZE` (50 MB). Stored in the C++ `network_config` but **not**
/// enforced by its Levin reader, which uses [`LEVIN_DEFAULT_MAX_PACKET_SIZE`].
pub const P2P_DEFAULT_PACKET_MAX_SIZE: u64 = 50_000_000;
/// `P2P_DEFAULT_PEERS_IN_HANDSHAKE` — depth of `get_peerlist_head`.
pub const P2P_DEFAULT_PEERS_IN_HANDSHAKE: usize = 250;
/// `P2P_DEFAULT_CONNECTION_TIMEOUT` (5 s) — TCP connect timeout.
pub const P2P_DEFAULT_CONNECTION_TIMEOUT: Duration = Duration::from_millis(5000);
/// `P2P_DEFAULT_PING_CONNECTION_TIMEOUT` (2 s)
pub const P2P_DEFAULT_PING_CONNECTION_TIMEOUT: Duration = Duration::from_millis(2000);
/// `P2P_DEFAULT_INVOKE_TIMEOUT` (2 min) — the C++ `timeoutLoop` drops a connection whose
/// pending write is older than this; we use it as the request/response timeout.
pub const P2P_DEFAULT_INVOKE_TIMEOUT: Duration = Duration::from_secs(120);
/// `P2P_DEFAULT_HANDSHAKE_INVOKE_TIMEOUT` (5 s). Defined in C++ but the code actually
/// waits `connection_timeout * 3` for a handshake (one for the handshake, two for the
/// back-ping the remote does to our `my_port`), see [`HANDSHAKE_TIMEOUT`].
pub const P2P_DEFAULT_HANDSHAKE_INVOKE_TIMEOUT: Duration = Duration::from_millis(5000);
/// Effective handshake timeout used by `NodeServer::try_to_connect_and_handshake_with_new_peer`
/// (3 x 5 s).
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(15);

/// Idle limit of a running connection (not in C++, whose `timeoutLoop` only watches pending
/// writes): nothing received for timed-sync interval + invoke timeout + one minute of slack
/// (60 s + 120 s + 60 s).
/// Both sides send `COMMAND_TIMED_SYNC` every minute, so only dead or stuck peers hit it.
pub const P2P_IDLE_TIMEOUT: Duration = Duration::from_mins(4);

/// How long a misbehaving address stays banned. Kryptokrona's C++ node has no ban list; this
/// is Monero's `P2P_IP_BLOCKTIME` (24 h), the default of `xkrd --ban-time`.
pub const P2P_IP_BLOCKTIME: Duration = Duration::from_hours(24);

/// Default limit of simultaneous connections from one IP address (`xkrd --max-conns-per-ip`;
/// Monero's `--max-connections-per-ip` defaults to 1, we allow a few for NAT and test setups).
pub const P2P_DEFAULT_MAX_CONNECTIONS_PER_IP: usize = 4;

/// `LEVIN_DEFAULT_MAX_PACKET_SIZE` (100 MB) — enforced in `LevinProtocol::readCommand`.
pub const LEVIN_DEFAULT_MAX_PACKET_SIZE: u64 = 100_000_000;
/// `LEVIN_SIGNATURE` ("Bender's nightmare"), identical to Monero.
pub const LEVIN_SIGNATURE: u64 = 0x0101_0101_0101_2101;
/// `LEVIN_PROTOCOL_VER_1`
pub const LEVIN_PROTOCOL_VERSION: u32 = 1;
/// `LEVIN_PROTOCOL_RETCODE_SUCCESS` — handlers in `NodeServer` return 1.
pub const LEVIN_RETCODE_SUCCESS: i32 = 1;
/// `LevinError::ERROR_CONNECTION_HANDLER_NOT_DEFINED` — reply code for unknown requests.
pub const LEVIN_ERROR_HANDLER_NOT_DEFINED: i32 = -6;
/// `LevinError::ERROR_FORMAT`
pub const LEVIN_ERROR_FORMAT: i32 = -7;

/// `BLOCKS_IDS_SYNCHRONIZING_DEFAULT_COUNT` — max hashes in a `NOTIFY_RESPONSE_CHAIN_ENTRY`.
pub const BLOCKS_IDS_SYNCHRONIZING_DEFAULT_COUNT: usize = 10_000;
/// `BLOCKS_SYNCHRONIZING_DEFAULT_COUNT` — blocks per `NOTIFY_REQUEST_GET_OBJECTS`.
pub const BLOCKS_SYNCHRONIZING_DEFAULT_COUNT: usize = 100;

/// Least wire bytes (length prefix included) one transaction blob in an epee `txs` array must
/// take. No real transaction is that small (a key input alone is 36 bytes); without it an
/// array of empty strings costs one byte on the wire but a 24-byte `Vec` header in memory, so a
/// 100 MB `NOTIFY_NEW_TRANSACTIONS` could allocate 2.4 GB (Cuprate uses the same bound).
pub const MIN_TX_BLOB_WIRE_SIZE: usize = 32;
/// Most blocks accepted in one `NOTIFY_RESPONSE_GET_OBJECTS`. We request at most
/// [`BLOCKS_SYNCHRONIZING_DEFAULT_COUNT`] and serve at most 500; an empty block entry is one
/// byte on the wire but 48 bytes in memory, so the count must be bounded.
pub const MAX_BLOCKS_IN_RESPONSE: usize = 500;
/// Most entries of a remote peer list that are merged (`PeerList::merge_remote`). C++ sends at
/// most `P2P_DEFAULT_PEERS_IN_HANDSHAKE` (+2); a 1 MiB timed-sync response could carry 43 690.
pub const MAX_REMOTE_PEERLIST_ENTRIES: usize = 2 * P2P_DEFAULT_PEERS_IN_HANDSHAKE;

// ------------------------------------------------------------------ Resource limits
//
// The C++ node has none of these unless a C++ constant is named; each value is far above what a
// C++ or xkrd peer sends.

/// Inbound handshake timeout: the first message (`COMMAND_HANDSHAKE`, or `COMMAND_PING` for a
/// back-ping) must arrive this soon after the TCP accept. A C++ node sends it right after
/// connecting; 2 x `P2P_DEFAULT_CONNECTION_TIMEOUT`, shorter than the 15 s outbound budget
/// (which includes the remote's back-ping to us).
pub const INBOUND_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// Longest `NOTIFY_REQUEST_CHAIN` block-id list that is looked up. A sparse chain has
/// `2 + log2(height)` ids (C++ `buildSparseChain`: at most 34 for a 32-bit height; Monero's
/// form about 45); a longer list is a protocol violation.
pub const MAX_REQUEST_CHAIN_IDS: usize = 512;
/// Most bytes of blocks in one `NOTIFY_RESPONSE_GET_OBJECTS` we send, and of transactions in
/// one `NOTIFY_NEW_TRANSACTIONS` answering `NOTIFY_MISSING_TXS`. Below
/// [`P2P_CONNECTION_MAX_WRITE_BUFFER_SIZE`]: a C++ node interrupts its own connection when one
/// queued message is larger, so no C++ node ever sends a larger answer either.
pub const MAX_RESPONSE_BYTES: usize = P2P_CONNECTION_MAX_WRITE_BUFFER_SIZE - 1024 * 1024;
/// Most bytes of pool transactions in one answer to `NOTIFY_REQUEST_TX_POOL` (C++ sends its
/// whole pool; the protocol only carries full blobs, so the answer is cut, not replaced by
/// hashes; the rest arrives through relays).
pub const MAX_TX_POOL_RESPONSE_BYTES: usize = 16 * 1024 * 1024;
/// Most transactions one `NOTIFY_MISSING_TXS` may ask for, and most transaction hashes in a
/// lite block template we look up. A block's transactions must fit its cumulative size limit
/// (`2 x max(median, 100 000)` bytes, today 200 KB; `maxBlockCumulativeSize` ~860 KB at height
/// 2.6 M) and none is smaller than ~200 bytes (one key input, one output, one ring
/// signature), so no real block carries 10 000.
pub const MAX_MISSING_TXS: usize = 10_000;
/// Read limit for bulk messages (`NOTIFY_NEW_BLOCK`, `NOTIFY_NEW_TRANSACTIONS`,
/// `NOTIFY_RESPONSE_GET_OBJECTS`, unknown commands) that we did not ask for. Every peer of P2P
/// version >= 4 relays blocks as lite blocks; a full block is at most `2 x max(median,
/// 100 000)` bytes; transaction relays are single transactions or small batches. Above it the
/// connection is closed without a ban ([`crate::PeerError::is_protocol_violation`]).
pub const UNSOLICITED_MAX_PACKET_SIZE: u64 = 8 * 1024 * 1024;
/// Read limit while we wait for an answer we asked for (`NOTIFY_RESPONSE_GET_OBJECTS`, or the
/// `NOTIFY_NEW_TRANSACTIONS` answering our `NOTIFY_REQUEST_TX_POOL` / `NOTIFY_MISSING_TXS`).
/// Every C++ notification and reply goes through `P2pConnectionContext::pushMessage`, which
/// interrupts the sender's own connection when one message exceeds
/// [`P2P_CONNECTION_MAX_WRITE_BUFFER_SIZE`], so no C++ node ever sends a larger body (xkrd's
/// own answers stay below [`MAX_RESPONSE_BYTES`]). The global
/// [`LEVIN_DEFAULT_MAX_PACKET_SIZE`] (100 MB) stays the hard protocol limit.
pub const SOLICITED_MAX_PACKET_SIZE: u64 = P2P_CONNECTION_MAX_WRITE_BUFFER_SIZE as u64;
/// Connections that may read with [`SOLICITED_MAX_PACKET_SIZE`] at the same time (node-wide).
/// The others read at most [`UNSOLICITED_MAX_PACKET_SIZE`], which every realistic answer fits:
/// worst case `24 x 32 MiB` plus `8 MiB` per other connection instead of 100 MB each.
pub const MAX_LARGE_READERS: usize = 24;
/// Our own `NOTIFY_NEW_TRANSACTIONS` relays are split into messages of at most this many bytes,
/// so peers that apply [`UNSOLICITED_MAX_PACKET_SIZE`] accept them.
pub const RELAY_CHUNK_BYTES: usize = 4 * 1024 * 1024;
/// New addresses one remote peer list may add to the gray list (C++ merges all of them).
pub const MAX_NEW_GRAY_PER_MERGE: usize = 64;
/// Distinct reporters remembered per gray entry (only "one or more than one" matters).
pub const MAX_GRAY_SOURCES_TRACKED: usize = 4;

/// Per-connection token buckets for incoming messages (`crate::ratelimit`). A bucket holds
/// `burst` tokens and gains one every `every`. A message beyond its bucket is dropped and costs
/// a strike; a peer out of strikes ([`STRIKES`](rate_limits::STRIKES)) is a protocol violation
/// (ban). Answers to our own requests are not counted.
pub mod rate_limits {
    use std::time::Duration;

    /// A token bucket.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct Rate {
        /// Tokens when full.
        pub burst: u32,
        /// Time per new token.
        pub every: Duration,
    }

    /// `COMMAND_TIMED_SYNC` requests: C++ sends one per `P2P_DEFAULT_HANDSHAKE_INTERVAL` (60 s).
    pub const TIMED_SYNC: Rate = Rate {
        burst: 4,
        every: Duration::from_secs(15),
    };
    /// `COMMAND_PING` on an established connection (C++ back-pings use a new connection).
    pub const PING: Rate = Rate {
        burst: 4,
        every: Duration::from_secs(15),
    };
    /// `NOTIFY_NEW_BLOCK` and `NOTIFY_NEW_LITE_BLOCK` together (one block per 90 s target).
    pub const NEW_BLOCK: Rate = Rate {
        burst: 30,
        every: Duration::from_secs(2),
    };
    /// Unsolicited `NOTIFY_NEW_TRANSACTIONS` messages.
    pub const NEW_TRANSACTIONS: Rate = Rate {
        burst: 300,
        every: Duration::from_millis(100),
    };
    /// Bytes of unsolicited `NOTIFY_NEW_TRANSACTIONS`: 32 MiB burst, 2 MiB/s.
    pub const NEW_TRANSACTION_BYTES_BURST: u32 = 32 * 1024 * 1024;
    /// See [`NEW_TRANSACTION_BYTES_BURST`].
    pub const NEW_TRANSACTION_BYTES_PER_SECOND: u32 = 2 * 1024 * 1024;
    /// `NOTIFY_REQUEST_CHAIN` (a syncing C++ peer sends one per 10 000 block ids).
    pub const REQUEST_CHAIN: Rate = Rate {
        burst: 20,
        every: Duration::from_secs(2),
    };
    /// `NOTIFY_REQUEST_GET_OBJECTS`. A syncing peer waits for each answer, so this only stops
    /// bursts; the node also serves one request per peer at a time.
    pub const REQUEST_GET_OBJECTS: Rate = Rate {
        burst: 100,
        every: Duration::from_millis(10),
    };
    /// `NOTIFY_REQUEST_TX_POOL`: one per 30 s (C++ sends one when a peer reaches the normal
    /// state and one after a chain switch; it answers with its whole pool each time).
    pub const REQUEST_TX_POOL: Rate = Rate {
        burst: 1,
        every: Duration::from_secs(30),
    };
    /// `NOTIFY_MISSING_TXS` (one per lite block we relayed to the peer).
    pub const MISSING_TXS: Rate = Rate {
        burst: 10,
        every: Duration::from_secs(5),
    };
    /// Everything else: unsolicited `NOTIFY_RESPONSE_*`, unknown commands, debug requests.
    pub const OTHER: Rate = Rate {
        burst: 50,
        every: Duration::from_millis(200),
    };
    /// Strikes: 20, one back per 30 s.
    pub const STRIKES: Rate = Rate {
        burst: 20,
        every: Duration::from_secs(30),
    };
}

/// `PING_OK_RESPONSE_STATUS_TEXT`
pub const PING_OK_RESPONSE_STATUS_TEXT: &str = "OK";

/// `P2P_DEFAULT_PORT` without `USE_TESTNET`.
pub const MAINNET_P2P_PORT: u16 = 11897;
/// `P2P_DEFAULT_PORT` with `USE_TESTNET`.
pub const TESTNET_P2P_PORT: u16 = 11898;

/// Size limits we apply per Levin command before reading a body (memory bound; the C++ node
/// only has the global 100 MB limit). Values follow Cuprate/Monero where the message has
/// the same shape.
pub mod bucket_limits {
    /// Handshake and timed sync carry at most ~250 peerlist entries (24 B each).
    pub const ADMIN: u64 = 1024 * 1024;
    /// Ping / peer-id / stat-info.
    pub const SMALL: u64 = 64 * 1024;
    /// Request chain: a sparse chain is ~log2(height) hashes.
    pub const REQUEST_CHAIN: u64 = 512 * 1024;
    /// Response chain entry: up to 10 000 hashes = 320 KB.
    pub const RESPONSE_CHAIN: u64 = 4 * 1024 * 1024;
    /// Get-objects request: up to 100 block hashes (we accept more from odd peers).
    pub const REQUEST_GET_OBJECTS: u64 = 2 * 1024 * 1024;
    /// Lite block (template only) and missing-txs request.
    pub const LITE: u64 = 4 * 1024 * 1024;
    /// Blocks with transactions, transaction relay: the global Levin maximum.
    pub const BULK: u64 = super::LEVIN_DEFAULT_MAX_PACKET_SIZE;
}
