//! Read-only access to schema v1 databases, for `xkrd db convert` (measure v1 and v2 on the
//! same chain without a second sync). v1 stored per block `block_infos[h]` (104-byte
//! [`BlockInfo`]), `block_blobs[h]` (framed zstd block blob), and per transaction
//! `tx_blobs[(h, i)]` (framed zstd prefix) and `tx_prunable[(h, i)]` (raw signatures).

use std::path::Path;

use redb::{Database, ReadOnlyDatabase, ReadOnlyTable, ReadableDatabase, TableDefinition};
use xkr_types::RawBlock;

use crate::{Compressor, Error, Result, compress::DEFAULT_LEVEL, schema::BlockInfo};

const V1_META: TableDefinition<&str, u64> = TableDefinition::new("meta");
const V1_META_BLOBS: TableDefinition<&str, &[u8]> = TableDefinition::new("meta_blobs");
const V1_BLOCK_INFOS: TableDefinition<u64, BlockInfo> = TableDefinition::new("block_infos");
const V1_BLOCK_BLOBS: TableDefinition<u64, &[u8]> = TableDefinition::new("block_blobs");
const V1_TX_BLOBS: TableDefinition<(u64, u32), &[u8]> = TableDefinition::new("tx_blobs");
const V1_TX_PRUNABLE: TableDefinition<(u64, u32), &[u8]> = TableDefinition::new("tx_prunable");

enum V1Db {
    ReadOnly(ReadOnlyDatabase),
    Writable(Database),
}

/// A v1 database opened for reading.
pub struct V1Reader {
    _db: V1Db,
    block_infos: ReadOnlyTable<u64, BlockInfo>,
    block_blobs: ReadOnlyTable<u64, &'static [u8]>,
    tx_blobs: ReadOnlyTable<(u64, u32), &'static [u8]>,
    tx_prunable: ReadOnlyTable<(u64, u32), &'static [u8]>,
    top: Option<u64>,
    comp: Compressor,
}

impl std::fmt::Debug for V1Reader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("V1Reader")
            .field("top", &self.top)
            .finish_non_exhaustive()
    }
}

impl V1Reader {
    /// Opens `path` (read-only if redb allows) and checks that it is a v1 database.
    pub fn open(path: &Path) -> Result<Self> {
        let db = match redb::Builder::new().open_read_only(path) {
            Ok(db) => V1Db::ReadOnly(db),
            Err(_) => V1Db::Writable(Database::open(path)?),
        };
        let txn = match &db {
            V1Db::ReadOnly(d) => d.begin_read()?,
            V1Db::Writable(d) => d.begin_read()?,
        };
        let meta = txn.open_table(V1_META)?;
        let version = meta.get("schema_version")?.map(|g| g.value());
        if version != Some(1) {
            return Err(Error::Unsupported("not a schema v1 database"));
        }
        let top = meta.get("top_height")?.map(|g| g.value());
        let mut comp = Compressor::new(DEFAULT_LEVEL)?;
        if let Ok(blobs) = txn.open_table(V1_META_BLOBS)
            && let Some(d) = blobs.get("zstd_dictionary")?
        {
            comp.add_decoder_dictionary(d.value())?;
        }
        Ok(Self {
            block_infos: txn.open_table(V1_BLOCK_INFOS)?,
            block_blobs: txn.open_table(V1_BLOCK_BLOBS)?,
            tx_blobs: txn.open_table(V1_TX_BLOBS)?,
            tx_prunable: txn.open_table(V1_TX_PRUNABLE)?,
            top,
            comp,
            _db: db,
        })
    }

    /// Height of the top block.
    #[must_use]
    pub const fn top_height(&self) -> Option<u64> {
        self.top
    }

    /// The block at `height` as on the wire, with its v1 header data. Pruned v1 signatures
    /// come back missing (the blob is then only the prefix).
    pub fn block(&mut self, height: u64) -> Result<Option<(BlockInfo, RawBlock)>> {
        let Some(info) = self.block_infos.get(height)?.map(|g| g.value()) else {
            return Ok(None);
        };
        let framed = self
            .block_blobs
            .get(height)?
            .ok_or(Error::Corrupt("v1 block info without block blob"))?;
        let block = self.comp.decompress(framed.value())?;
        // `tx_count` comes from the old database file: reserve a sane amount, grow past it.
        let mut transactions = Vec::with_capacity((info.tx_count as usize).min(1024));
        for idx in 1..=info.tx_count {
            let framed = self
                .tx_blobs
                .get((height, idx))?
                .ok_or(Error::Corrupt("v1 transaction prefix missing"))?;
            let mut blob = self.comp.decompress(framed.value())?;
            if let Some(sigs) = self.tx_prunable.get((height, idx))? {
                blob.extend_from_slice(sigs.value());
            }
            transactions.push(blob);
        }
        Ok(Some((
            info,
            RawBlock {
                block,
                transactions,
            },
        )))
    }
}
