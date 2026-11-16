//! Size statistics of a database file, for any schema version (`xkrd db stats`).
//!
//! Every table is listed generically (`list_tables` + untyped tables), so the same report
//! works on a v1 database (before) and a v2 one (after). The raw chain size is the sum of
//! the CryptoNote `block_size` of all blocks, i.e. the top block's `cumulative_size`.

use std::path::Path;

use redb::{
    Database, ReadableDatabase, ReadableTable, ReadableTableMetadata, TableDefinition, TableHandle,
};

use crate::{
    Result,
    schema::{BLOCK_INFOS, BlockInfo, META, meta},
};

/// Size of one table (from redb's `TableStats`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableSize {
    pub name: String,
    pub entries: u64,
    pub tree_height: u32,
    pub leaf_pages: u64,
    pub branch_pages: u64,
    /// Key and value bytes.
    pub stored_bytes: u64,
    /// Branch keys, per-entry length fields and page headers.
    pub metadata_bytes: u64,
    /// Unused space inside the table's pages (partly filled leaves, power-of-two page runs).
    pub fragmented_bytes: u64,
}

impl TableSize {
    /// Bytes of the pages the table occupies.
    #[must_use]
    pub const fn total_bytes(&self) -> u64 {
        self.stored_bytes + self.metadata_bytes + self.fragmented_bytes
    }

    /// `stored / total`: how full the table's pages are (1.0 for an empty table).
    #[must_use]
    #[allow(clippy::cast_precision_loss)]
    pub fn fill(&self) -> f64 {
        let total = self.total_bytes();
        if total == 0 {
            1.0
        } else {
            self.stored_bytes as f64 / total as f64
        }
    }
}

/// Database-wide page accounting (redb `DatabaseStats`; needs a writable open).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileStats {
    pub allocated_pages: u64,
    pub leaf_pages: u64,
    pub branch_pages: u64,
    pub stored_bytes: u64,
    pub metadata_bytes: u64,
    /// Includes free pages inside the file.
    pub fragmented_bytes: u64,
    pub page_size: usize,
}

/// Chain totals read from the meta and block-info tables.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChainTotals {
    pub schema_version: u64,
    pub top_height: Option<u64>,
    /// Transactions including miner transactions.
    pub transactions: u64,
    /// Sum of `block_size` (miner tx + transactions) over all blocks.
    pub sum_block_size: u64,
    pub sealed_below: u64,
    pub pruned_below: u64,
    /// `META["pruning_seed"]` as stored (0 = none, `0x0100` = pruned completely; see
    /// [`crate::PruningSeed::decode`]).
    pub pruning_seed: u64,
}

impl ChainTotals {
    /// Number of blocks.
    #[must_use]
    pub fn blocks(&self) -> u64 {
        self.top_height.map_or(0, |h| h + 1)
    }
}

/// Everything `xkrd db stats` prints.
#[derive(Debug, Clone)]
pub struct DbStats {
    pub file_bytes: u64,
    /// Opened read-only (no `file` stats then).
    pub read_only: bool,
    pub tables: Vec<TableSize>,
    pub file: Option<FileStats>,
    pub chain: Option<ChainTotals>,
}

impl DbStats {
    /// Sum of the tables' page bytes.
    #[must_use]
    pub fn table_bytes(&self) -> u64 {
        self.tables.iter().map(TableSize::total_bytes).sum()
    }

    /// One table by name.
    #[must_use]
    pub fn table(&self, name: &str) -> Option<&TableSize> {
        self.tables.iter().find(|t| t.name == name)
    }
}

/// Per-table sizes and chain totals of any readable database.
pub fn collect<D: ReadableDatabase>(db: &D) -> Result<(Vec<TableSize>, Option<ChainTotals>)> {
    let txn = db.begin_read()?;
    let mut tables = Vec::new();
    for handle in txn.list_tables()? {
        let name = handle.name().to_owned();
        let table = txn.open_untyped_table(handle)?;
        let s = table.stats()?;
        tables.push(TableSize {
            name,
            entries: table.len()?,
            tree_height: s.tree_height(),
            leaf_pages: s.leaf_pages(),
            branch_pages: s.branch_pages(),
            stored_bytes: s.stored_bytes(),
            metadata_bytes: s.metadata_bytes(),
            fragmented_bytes: s.fragmented_bytes(),
        });
    }
    tables.sort_by_key(|t| std::cmp::Reverse(t.total_bytes()));

    let has = |n: &str| tables.iter().any(|t| t.name == n);
    let chain = if has("meta") && has("block_infos") {
        let m = txn.open_table(META)?;
        let get = |k: &str| -> Result<Option<u64>> { Ok(m.get(k)?.map(|g| g.value())) };
        let schema_version = get(meta::SCHEMA_VERSION)?.unwrap_or(0);
        let top_height = get(meta::TOP_HEIGHT)?;
        let (transactions, sum_block_size) = if schema_version == 1 {
            // v1 kept the full `BlockInfo` under the same table name.
            const V1: TableDefinition<u64, BlockInfo> = TableDefinition::new("block_infos");
            let t = txn.open_table(V1)?;
            t.last()?.map_or((0, 0), |(_, v)| {
                let i = v.value();
                (i.cumulative_tx_count, i.cumulative_size)
            })
        } else {
            let t = txn.open_table(BLOCK_INFOS)?;
            t.last()?.map_or((0, 0), |(_, v)| {
                let r = v.value();
                (r.cumulative_tx_count, r.cumulative_size)
            })
        };
        Some(ChainTotals {
            schema_version,
            top_height,
            transactions,
            sum_block_size,
            sealed_below: get(meta::SEALED_BELOW)?.unwrap_or(0),
            pruned_below: get(meta::PRUNED_BELOW)?.unwrap_or(0),
            pruning_seed: get(meta::PRUNING_SEED)?.unwrap_or(0),
        })
    } else {
        None
    };
    Ok((tables, chain))
}

/// redb's database-wide statistics (opens and aborts a write transaction).
pub fn file_stats(db: &Database) -> Result<FileStats> {
    let txn = db.begin_write()?;
    let s = txn.stats()?;
    let out = FileStats {
        allocated_pages: s.allocated_pages(),
        leaf_pages: s.leaf_pages(),
        branch_pages: s.branch_pages(),
        stored_bytes: s.stored_bytes(),
        metadata_bytes: s.metadata_bytes(),
        fragmented_bytes: s.fragmented_bytes(),
        page_size: s.page_size(),
    };
    txn.abort()?;
    Ok(out)
}

/// Statistics of the database file at `path`: opened read-only if redb allows (a cleanly
/// closed file not open elsewhere), otherwise opened normally (which may repair it).
pub fn stats_for_path(path: &Path, cache_bytes: usize) -> Result<DbStats> {
    let file_bytes = std::fs::metadata(path)?.len();
    let mut builder = redb::Builder::new();
    builder.set_cache_size(cache_bytes);
    if let Ok(db) = builder.open_read_only(path) {
        let (tables, chain) = collect(&db)?;
        return Ok(DbStats {
            file_bytes,
            read_only: true,
            tables,
            file: None,
            chain,
        });
    }
    let db = builder.open(path)?;
    let (tables, chain) = collect(&db)?;
    let file = Some(file_stats(&db)?);
    Ok(DbStats {
        file_bytes,
        read_only: false,
        tables,
        file,
        chain,
    })
}
