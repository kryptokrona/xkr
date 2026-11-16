//! xkr-storage — Kryptokrona chain storage on redb (schema v2, `docs/storage.md`).
//!
//! - [`schema`]: every table with its key/value layout.
//! - [`codec`]: block records, key-stripped blobs and chunk bodies.
//! - [`ops`]: batch writes ([`ops::push_blocks`], [`ops::pop_blocks`], [`ops::prune_stripe`],
//!   [`ops::prune_all_below`]) and reads ([`ops::Reader`]) used by consensus, P2P and RPC.
//! - [`pruning`]: stripe pruning ([`PruningSeed`], [`STRIPE_SPAN`]).
//! - [`compress`]: zstd framing with an optional trained dictionary.
//! - [`Store`]: opening the database, dictionaries, [`stats`] and [`maintain`] (repack,
//!   compact, re-compress); [`legacy`] reads schema v1 files for `xkrd db convert`.
//!
//! One write transaction per verified batch; readers get MVCC snapshots and never block
//! the writer. No shared mutable state lives here besides the database file.

pub mod codec;
pub mod compress;
pub mod error;
pub mod legacy;
pub mod maintain;
pub mod ops;
pub mod pruning;
pub mod schema;
pub mod stats;
mod store;

pub use compress::Compressor;
pub use error::{Error, Result};
pub use maintain::{MaintainOptions, MaintainReport};
pub use ops::{Reader, VerifiedBlock, VerifiedTx};
pub use pruning::{DEFAULT_STRIPES, PruneMode, PruneReport, PruneState, PruningSeed, STRIPE_SPAN};
pub use schema::{BlockInfo, OutputInfo, TxLoc};
pub use stats::DbStats;
pub use store::{OpenEvent, PRUNE_STEP_BLOCKS, Store};

/// The trained mainnet dictionary shipped with the binary (`dicts/README.md`). Empty until one
/// is trained; an empty file means "no dictionary".
static MAINNET_DICTIONARY: &[u8] = include_bytes!("../dicts/mainnet.zdict");

/// The zstd dictionary compiled in for `network` (`"mainnet"`), if one is shipped. A fresh
/// database installs it before the first chunk is sealed, so every node writes identical
/// chunks and a later dictionary change is an explicit re-compression.
#[must_use]
pub fn builtin_dictionary(network: &str) -> Option<&'static [u8]> {
    match network {
        "mainnet" if !MAINNET_DICTIONARY.is_empty() => Some(MAINNET_DICTIONARY),
        _ => None,
    }
}

#[cfg(test)]
mod tests;
