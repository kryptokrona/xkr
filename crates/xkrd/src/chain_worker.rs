//! The chain writer thread.
//!
//! One OS thread owns all chain writes: verified sync batches and relayed blocks are queued
//! to it and processed in order. Verification inside `Chain::add_blocks` fans out on the rayon
//! pool, so neither the tokio workers nor this thread do the heavy lifting alone, and tokio
//! never blocks on CPU work.

use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use tokio::sync::{mpsc, oneshot};
use xkr_chain::{AddBlockResult, BatchReport, Chain};
use xkr_types::{Hash, RawBlock};

/// Work for the chain thread.
#[derive(Debug)]
pub enum ChainJob {
    /// A contiguous batch from the downloader.
    Batch {
        /// Downloader generation (results of older generations are ignored).
        generation: u64,
        /// Height of the first block.
        start: u64,
        /// The blocks, in height order.
        blocks: Vec<RawBlock>,
        /// The ids that were requested for them.
        expected: Vec<Hash>,
        /// `(first height, connection)` of every peer batch merged into this job.
        sources: Vec<(u64, u64)>,
    },
    /// A relayed block (`NOTIFY_NEW_BLOCK` or a completed lite block), or a mined one
    /// (`submitblock`, built-in miner).
    Block {
        /// Connection it came from (`None` for local blocks).
        conn: Option<u64>,
        /// The block.
        block: RawBlock,
        /// Relay hop count it arrived with.
        hop: u32,
        /// Mined locally: relayed with hop 0 (C++ `on_submitblock`), not `hop + 1`.
        local: bool,
        /// Receives the outcome as well (submitters wait for it).
        reply: Option<oneshot::Sender<Result<AddBlockResult, String>>>,
    },
    /// Database maintenance (repack + compact) after the initial sync; no event is sent.
    Maintain,
}

/// Results of the chain thread.
#[derive(Debug)]
pub enum ChainEvent {
    /// A batch was processed.
    BatchDone {
        /// Generation of the job.
        generation: u64,
        /// Height of the first block.
        start: u64,
        /// Blocks submitted.
        count: usize,
        /// Where they came from.
        sources: Vec<(u64, u64)>,
        /// Outcome (`Err` = node failure, e.g. storage).
        report: Result<Box<BatchReport>, String>,
        /// Time spent.
        elapsed: Duration,
    },
    /// A relayed block was processed.
    BlockDone {
        /// Source connection.
        conn: Option<u64>,
        /// The block (for relaying).
        block: RawBlock,
        /// Hop count it arrived with.
        hop: u32,
        /// Mined locally.
        local: bool,
        /// Outcome (`Err` = node failure).
        result: Result<AddBlockResult, String>,
    },
}

/// Runs a stripe-pruning step (`Chain::prune`: a no-op unless the database has a stripe seed
/// and the bound moved past its prune height).
fn prune(chain: &Chain) {
    let t = Instant::now();
    match chain.prune() {
        Ok(Some(r)) => tracing::info!(
            seed = %r.state.seed,
            pruned_below = r.state.pruned_below,
            chunks = r.chunks,
            transactions = r.transactions,
            mb = (r.parts * 4096) >> 20,
            secs = t.elapsed().as_secs(),
            "pruned the signatures of the other stripes"
        ),
        Ok(None) => {}
        Err(e) => tracing::warn!(error = %e, "stripe pruning failed"),
    }
}

/// Starts the chain thread. It ends when every job sender is dropped, after flushing the
/// database. With a stripe seed it prunes once at start (a full database started with
/// `--prune`) and after every batch and block.
pub fn spawn(
    chain: Arc<Chain>,
    mut jobs: mpsc::UnboundedReceiver<ChainJob>,
    events: mpsc::UnboundedSender<ChainEvent>,
) -> std::io::Result<JoinHandle<()>> {
    std::thread::Builder::new()
        .name("xkr-chain".into())
        .spawn(move || {
            prune(&chain);
            while let Some(job) = jobs.blocking_recv() {
                let event = match job {
                    ChainJob::Batch {
                        generation,
                        start,
                        blocks,
                        expected,
                        sources,
                    } => {
                        let t = Instant::now();
                        let count = blocks.len();
                        let report = chain
                            .add_blocks(blocks, Some(&expected))
                            .map(Box::new)
                            .map_err(|e| e.to_string());
                        prune(&chain);
                        ChainEvent::BatchDone {
                            generation,
                            start,
                            count,
                            sources,
                            report,
                            elapsed: t.elapsed(),
                        }
                    }
                    ChainJob::Maintain => {
                        let t = Instant::now();
                        match chain.maintain(&xkr_storage::MaintainOptions::default()) {
                            Ok(r) => tracing::info!(
                                repacked = r.repacked.len(),
                                compacted = r.compacted,
                                file_mb_before = r.file_bytes_before >> 20,
                                file_mb_after = r.file_bytes_after >> 20,
                                secs = t.elapsed().as_secs(),
                                "database maintenance done"
                            ),
                            Err(e) => tracing::warn!(error = %e, "database maintenance failed"),
                        }
                        continue;
                    }
                    ChainJob::Block {
                        conn,
                        block,
                        hop,
                        local,
                        reply,
                    } => {
                        let result = chain.add_block(block.clone()).map_err(|e| e.to_string());
                        if result
                            .as_ref()
                            .is_ok_and(|r| r.outcome.main_chain_changed())
                        {
                            prune(&chain);
                        }
                        if let Some(reply) = reply {
                            let _ = reply.send(result.clone());
                        }
                        ChainEvent::BlockDone {
                            conn,
                            block,
                            hop,
                            local,
                            result,
                        }
                    }
                };
                if events.send(event).is_err() {
                    break;
                }
            }
            match chain.flush() {
                Ok(()) => tracing::info!("chain flushed"),
                Err(e) => tracing::error!(error = %e, "flushing the chain failed"),
            }
        })
}
