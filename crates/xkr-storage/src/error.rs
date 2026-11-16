//! Storage errors.

/// `Result` with [`Error`].
pub type Result<T, E = Error> = core::result::Result<T, E>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("database error: {0}")]
    Db(#[source] redb::Error),
    #[error("i/o (zstd) error: {0}")]
    Io(#[from] std::io::Error),
    #[error("database schema version {found}, this build expects {expected}")]
    SchemaVersion { found: u64, expected: u64 },
    #[error("block at height {got} does not extend the chain (expected height {expected})")]
    NonContiguous { expected: u64, got: u64 },
    #[error("key image {0} is already spent")]
    KeyImageSpent(String),
    #[error("output not found: amount {amount}, global index {index}")]
    OutputNotFound { amount: u64, index: u64 },
    #[error("block {0} not found")]
    BlockNotFound(u64),
    /// The block's ring signatures were dropped by pruning on this node (another stripe's
    /// block, or everything below `pruned_below` after `prune_all_below`).
    #[error("block {0} is pruned on this node (its ring signatures were dropped)")]
    Pruned(u64),
    #[error("block at height {0} is not the current top block or does not match")]
    PopMismatch(u64),
    #[error("transaction {tx} has prefix length {prefix_len} > blob length {blob_len}")]
    BadPrefixLen {
        tx: String,
        prefix_len: usize,
        blob_len: usize,
    },
    #[error("corrupt data: {0}")]
    Corrupt(&'static str),
    #[error("database maintenance in progress, try again later")]
    Busy,
    #[error("{0}")]
    Unsupported(&'static str),
    /// [`crate::Store::close`] ran: the node is stopping and the database file is closed.
    #[error("the database is closed")]
    Closed,
    /// [`crate::Store::close`] found a write transaction of another thread still open: redb
    /// closes the file when that transaction ends.
    #[error("a write transaction is still open; the database closes when it ends")]
    CloseDeferred,
}

impl Error {
    /// Whether this is [`Error::Closed`]: callers that may still run while the node stops
    /// treat it as "stop now" rather than as a failure.
    #[must_use]
    pub fn is_closed(&self) -> bool {
        matches!(self, Self::Closed)
    }
}

impl From<redb::Error> for Error {
    fn from(e: redb::Error) -> Self {
        match e {
            redb::Error::DatabaseClosed => Self::Closed,
            e => Self::Db(e),
        }
    }
}

macro_rules! from_redb {
    ($($t:ty),*) => {$(
        impl From<$t> for Error {
            fn from(e: $t) -> Self {
                redb::Error::from(e).into()
            }
        }
    )*};
}

from_redb!(
    redb::DatabaseError,
    redb::TransactionError,
    redb::TableError,
    redb::StorageError,
    redb::CommitError,
    redb::CursorError,
    redb::CompactionError,
    redb::SetDurabilityError
);
