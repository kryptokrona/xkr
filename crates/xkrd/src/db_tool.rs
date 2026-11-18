//! `xkrd db ...`: chain database tools (`docs/storage.md` "Measuring").

use std::collections::HashMap;
use std::error::Error;
use std::io::{IsTerminal, Write};
use std::path::Path;
use std::time::{Duration, Instant};

use xkr_chain::state::{StateHashes, chain_state, complete_spans, span_digest};
use xkr_term::steps::{Outcome, render_finished, render_running};
use xkr_term::term::Style;

use xkr_serialization::{
    block_hash, decode_block, decode_transaction, decode_transaction_prefix, transaction_hash,
    transaction_hash_from_blob,
};
use xkr_storage::{
    BlockInfo, DEFAULT_STRIPES, DbStats, MaintainOptions, PruneMode, PruneReport, PruningSeed,
    STRIPE_SPAN, Store, VerifiedBlock, VerifiedTx,
    compress::train_dictionary,
    legacy::V1Reader,
    ops::push_blocks,
    schema::CHUNK_BLOCKS,
    stats::{TableSize, stats_for_path},
};
use xkr_types::{Hash, TransactionPrefix, TxInput, TxOutputTarget, payment_id};

use crate::cli::{DbArgs, DbCommand};

type DynResult<T = ()> = Result<T, Box<dyn Error>>;

/// Runs one `xkrd db` subcommand.
pub fn run(cmd: &DbCommand) -> DynResult {
    match cmd {
        DbCommand::Stats(db) => stats(db),
        DbCommand::Compact {
            db,
            force,
            no_repack,
        } => compact(db, *force, *no_repack),
        DbCommand::TrainDict {
            db,
            samples,
            max_dict_bytes,
            out,
            dict,
            no_recompress,
        } => train(
            db,
            *samples,
            *max_dict_bytes,
            out.as_deref(),
            dict.as_deref(),
            *no_recompress,
        ),
        DbCommand::Verify { db, from, to } => verify(db, *from, *to),
        DbCommand::Prune {
            db,
            all,
            stripes,
            stripe,
            below,
            force_below,
        } => prune(db, *all, *stripes, *stripe, *below, *force_below),
        DbCommand::Convert { from, db, batch } => convert(from, db, *batch),
        DbCommand::StateHashes { db, to } => state_hashes(db, *to),
        DbCommand::VerifyState(db) => verify_state(db),
    }
}

fn open(db: &DbArgs) -> DynResult<Store> {
    let path = db.db_path();
    if !path.exists() {
        return Err(format!("no database at {}", path.display()).into());
    }
    Ok(Store::open_with_cache(
        &path,
        db.db_cache_mb.max(16) * 1024 * 1024,
    )?)
}

#[allow(clippy::cast_precision_loss)]
fn mb(bytes: u64) -> f64 {
    bytes as f64 / 1e6
}

#[allow(clippy::cast_precision_loss)]
fn ratio(a: u64, b: u64) -> f64 {
    if b == 0 { 0.0 } else { a as f64 / b as f64 }
}

/// Which part of the chain a table holds (v1 and v2 names).
fn group(name: &str) -> &'static str {
    match name {
        "body_parts" | "hot_blocks" | "block_blobs" | "tx_blobs" => "blobs (blocks + prefixes)",
        "sig_parts" | "hot_sigs" | "tx_prunable" => "signatures (prunable)",
        "outputs" | "unlock_overflow" | "output_counts" | "tx_output_indexes" => "outputs",
        "key_images" => "key images",
        "tx_hash_to_loc" | "payment_ids" => "transaction index",
        "block_infos" | "block_hash_to_height" | "timestamps" => "block index",
        _ => "other",
    }
}

fn print_stats(path: &Path, s: &DbStats) {
    println!(
        "database {} ({}, {})",
        path.display(),
        if s.read_only {
            "opened read-only"
        } else {
            "opened read-write"
        },
        s.chain.map_or_else(
            || "no chain tables".to_owned(),
            |c| format!("schema v{}", c.schema_version)
        )
    );
    println!(
        "{:<24} {:>12} {:>10} {:>10} {:>9} {:>9} {:>6} {:>9}",
        "table", "entries", "total MB", "stored MB", "meta MB", "frag MB", "fill", "B/entry"
    );
    for t in &s.tables {
        println!(
            "{:<24} {:>12} {:>10.2} {:>10.2} {:>9.2} {:>9.2} {:>5.0}% {:>9.1}",
            t.name,
            t.entries,
            mb(t.total_bytes()),
            mb(t.stored_bytes),
            mb(t.metadata_bytes),
            mb(t.fragmented_bytes),
            t.fill() * 100.0,
            ratio(t.total_bytes(), t.entries)
        );
    }
    let mut groups: HashMap<&str, u64> = HashMap::new();
    for t in &s.tables {
        *groups.entry(group(&t.name)).or_default() += t.total_bytes();
    }
    let mut groups: Vec<_> = groups.into_iter().collect();
    groups.sort_by_key(|g| std::cmp::Reverse(g.1));
    let tables = s.table_bytes();
    println!();
    for (g, bytes) in &groups {
        println!(
            "{g:<28} {:>10.2} MB {:>5.1}%",
            mb(*bytes),
            ratio(*bytes, tables) * 100.0
        );
    }
    println!();
    println!("file size            {:>12.2} MB", mb(s.file_bytes));
    println!("sum of table pages   {:>12.2} MB", mb(tables));
    println!(
        "not in any table     {:>12.2} MB (free pages, allocator and system tables)",
        mb(s.file_bytes.saturating_sub(tables))
    );
    if let Some(f) = &s.file {
        let allocated = f.allocated_pages * f.page_size as u64;
        println!(
            "allocated pages      {:>12} ({:.2} MB), leaf {} branch {}",
            f.allocated_pages,
            mb(allocated),
            f.leaf_pages,
            f.branch_pages
        );
        println!(
            "free in file         {:>12.2} MB; fragmented incl. free pages {:.2} MB",
            mb(s.file_bytes.saturating_sub(allocated)),
            mb(f.fragmented_bytes)
        );
    }
    if let Some(c) = &s.chain {
        let raw = c.sum_block_size;
        println!();
        println!(
            "blocks {} (top {:?}), transactions incl. miner txs {}",
            c.blocks(),
            c.top_height,
            c.transactions
        );
        if c.schema_version >= 2 {
            println!(
                "sealed below {}, pruned below {}",
                c.sealed_below, c.pruned_below
            );
            let seed = u32::try_from(c.pruning_seed)
                .ok()
                .and_then(PruningSeed::decode);
            let pruning = match seed {
                None => format!("invalid stored pruning seed {:#x}", c.pruning_seed),
                Some(s) if s.is_stripe() => format!(
                    "stripe pruning: seed {s}, signatures kept below {} only for that stripe",
                    c.pruned_below
                ),
                Some(s) if s.is_recent_only() || c.pruned_below > 0 => {
                    format!("pruned completely: no signatures below {}", c.pruned_below)
                }
                Some(_) => "full node: every signature kept".to_owned(),
            };
            println!("{pruning}");
        }
        println!("raw chain (sum of block_size)      {:>12.2} MB", mb(raw));
        println!(
            "file / raw  {:.3}   tables / raw  {:.3}   bytes per block {:.0}",
            ratio(s.file_bytes, raw),
            ratio(tables, raw),
            ratio(s.file_bytes, c.blocks())
        );
        if let Some(o) = s.table("outputs") {
            println!(
                "outputs {}: {:.1} B per output in the outputs table",
                o.entries,
                ratio(o.total_bytes(), o.entries)
            );
        }
        if let Some(k) = s.table("key_images") {
            println!(
                "key images {}: {:.1} B each",
                k.entries,
                ratio(k.total_bytes(), k.entries)
            );
        }
    }
}

fn stats(db: &DbArgs) -> DynResult {
    let path = db.db_path();
    let s = stats_for_path(&path, db.db_cache_mb.max(16) * 1024 * 1024)?;
    print_stats(&path, &s);
    Ok(())
}

fn compact(db: &DbArgs, force: bool, no_repack: bool) -> DynResult {
    let store = open(db)?;
    let t = Instant::now();
    let mut opts = MaintainOptions {
        force,
        ..MaintainOptions::default()
    };
    if no_repack {
        // No table is less than 0 % full: nothing is repacked, and the file is compacted.
        opts.repack_below_fill = 0.0;
        opts.compact_min_free_fraction = 0.0;
    }
    let report = store.maintain(&opts, &mut |msg| println!("{msg}"))?;
    for (table, rows, fill) in &report.repacked {
        println!(
            "repacked {table}: {rows} rows (was {:.0} % full)",
            fill * 100.0
        );
    }
    println!(
        "compacted: {}; file {:.2} MB -> {:.2} MB in {:.0} s",
        report.compacted,
        mb(report.file_bytes_before),
        mb(report.file_bytes_after),
        t.elapsed().as_secs_f64()
    );
    Ok(())
}

fn train(
    db: &DbArgs,
    samples: usize,
    max_dict_bytes: usize,
    out: Option<&Path>,
    dict_file: Option<&Path>,
    no_recompress: bool,
) -> DynResult {
    let store = open(db)?;
    let t = Instant::now();
    let dict = if let Some(p) = dict_file {
        std::fs::read(p)?
    } else {
        let samples = store.training_samples(samples, 64 * 1024)?;
        let total: usize = samples.iter().map(Vec::len).sum();
        println!(
            "training over {} records ({:.2} MB)",
            samples.len(),
            mb(total as u64)
        );
        train_dictionary(&samples, max_dict_bytes)?
    };
    println!(
        "dictionary: {} bytes, id {:#010x} ({:.0} s)",
        dict.len(),
        xkr_storage::compress::dictionary_id(&dict),
        t.elapsed().as_secs_f64()
    );
    if let Some(out) = out {
        std::fs::write(out, &dict)?;
        println!("written to {}", out.display());
    }
    if no_recompress {
        return Ok(());
    }
    let before = store.stats()?;
    let body_before = before.table("body_parts").map_or(0, TableSize::total_bytes);
    let mut last = Instant::now();
    let n = store.recompress(&dict, &mut |done, total| {
        if last.elapsed().as_secs() >= 5 || done == total {
            println!("re-compressed {done}/{total} chunks");
            last = Instant::now();
        }
    })?;
    let report = store.maintain(&MaintainOptions::default(), &mut |msg| println!("{msg}"))?;
    let after = store.stats()?;
    let body_after = after.table("body_parts").map_or(0, TableSize::total_bytes);
    println!(
        "{n} chunks rewritten; body_parts {:.2} MB -> {:.2} MB ({:.1} %); file {:.2} MB -> {:.2} MB",
        mb(body_before),
        mb(body_after),
        (1.0 - ratio(body_after, body_before)) * 100.0,
        mb(before.file_bytes),
        mb(report.file_bytes_after)
    );
    Ok(())
}

/// A stripe seed for `stripes` with a random stripe (`None` if `stripes` is not in `2..=8`).
#[must_use]
pub fn pick_seed(stripes: u8) -> Option<PruningSeed> {
    let stripe = crate::rng::Rng::new().below(usize::from(stripes)) + 1;
    PruningSeed::new(stripes, u8::try_from(stripe).ok()?)
}

/// The height `db prune` prunes below: `below` or the safety `bound`; above the bound only
/// with `force`, else an error naming the bound.
fn prune_height(below: Option<u64>, bound: u64, force: bool) -> Result<u64, String> {
    match below {
        Some(b) if b > bound && !force => Err(format!(
            "--below {b} is above the safety bound {bound} (min(last checkpoint, top - {}) \
             rounded down to {STRIPE_SPAN}): reorgs into pruned blocks would fail and peers \
             would miss blocks they assume whole; add --force-below to prune there anyway",
            2 * STRIPE_SPAN
        )),
        Some(b) => {
            if b > bound {
                eprintln!(
                    "warning: --below {b} is above the safety bound {bound} (--force-below): \
                     reorgs into pruned blocks will fail"
                );
            }
            Ok(b)
        }
        None => Ok(bound),
    }
}

fn prune(
    db: &DbArgs,
    all: bool,
    stripes: Option<u8>,
    stripe: Option<u8>,
    below: Option<u64>,
    force_below: bool,
) -> DynResult {
    let store = open(db)?;
    let (state, top) = {
        let r = store.reader()?;
        (r.prune_state(), r.top_height()?)
    };
    let Some(top) = top else {
        println!("empty chain, nothing to prune");
        return Ok(());
    };
    let bound = xkr_chain::Checkpoints::for_network(db.network.into()).prune_bound(top);
    let below = prune_height(below, bound, force_below)?;
    let mode = if all {
        PruneMode::All
    } else if stripes.is_some() || stripe.is_some() {
        let n = stripes.unwrap_or(DEFAULT_STRIPES);
        let seed = match stripe {
            Some(k) => PruningSeed::new(n, k),
            None => pick_seed(n),
        };
        PruneMode::Stripe(
            seed.ok_or_else(|| format!("--stripe {} is not in 1..={n}", stripe.unwrap_or(0)))?,
        )
    } else if state.seed.is_stripe() {
        PruneMode::Stripe(state.seed)
    } else if state.seed.is_recent_only() {
        PruneMode::All
    } else {
        return Err(
            "the database has no pruning seed: give --stripes N [--stripe K] or --all".into(),
        );
    };
    match mode {
        PruneMode::Stripe(seed) => println!("pruning below {below} with seed {seed}"),
        PruneMode::All => println!(
            "pruning every signature below {} (rounded down to {CHUNK_BLOCKS})",
            below.min(top + 1)
        ),
    }
    let t = Instant::now();
    let mut last = Instant::now();
    let report = store.prune(mode, below, &mut |r: &PruneReport| {
        if last.elapsed().as_secs() >= 5 {
            println!(
                "pruned below {}: {} chunks, {} transactions so far",
                r.state.pruned_below, r.chunks, r.transactions
            );
            last = Instant::now();
        }
    })?;
    println!(
        "pruned the signatures of {} transactions ({} chunks, {} parts = {:.2} MB of pages, {} hot rows) in {:.0} s",
        report.transactions,
        report.chunks,
        report.parts,
        mb(report.parts * 4096),
        report.hot_rows,
        t.elapsed().as_secs_f64()
    );
    println!(
        "seed {}, pruned below {} (run `xkrd db compact` to shrink the file)",
        report.state.seed, report.state.pruned_below
    );
    Ok(())
}

/// Progress of a command whose stdout is data (`db state-hashes`): on a terminal a step line
/// on stderr redrawn in place (spinner, then ✔ / ✗ with the time); otherwise a plain line every
/// 10 s and a final line. Colours follow `NO_COLOR` and `TERM=dumb` like the node's own output.
struct StderrProgress {
    name: &'static str,
    tty: bool,
    style: Style,
    started: Instant,
    shown: Instant,
    frame: usize,
}

impl StderrProgress {
    fn new(name: &'static str) -> Self {
        let dumb = std::env::var_os("TERM").is_some_and(|t| t == "dumb");
        let no_color = std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty());
        let tty = std::io::stderr().is_terminal() && !dumb;
        let now = Instant::now();
        Self {
            name,
            tty,
            style: Style::new(tty && !no_color),
            started: now,
            shown: now.checked_sub(Duration::from_secs(60)).unwrap_or(now),
            frame: 0,
        }
    }

    /// `done` of `total` spans.
    fn update(&mut self, done: u64, total: u64) {
        let every = if self.tty {
            Duration::from_millis(100)
        } else {
            Duration::from_secs(10)
        };
        if self.shown.elapsed() < every {
            return;
        }
        self.shown = Instant::now();
        let status = format!("span {done}/{total}");
        let mut err = std::io::stderr().lock();
        if self.tty {
            self.frame += 1;
            let line = render_running(
                self.style,
                self.name,
                Some(&status),
                self.started.elapsed(),
                self.frame,
            );
            let _ = write!(err, "\r\x1b[2K{line}");
        } else {
            let _ = writeln!(err, "{}: {status}", self.name);
        }
        let _ = err.flush();
    }

    /// Reports a problem on a line of its own (the progress line is redrawn after it).
    fn problem(&mut self, p: &mut Problems, msg: &str) {
        if self.tty {
            eprint!("\r\x1b[2K");
        }
        p.report(msg);
    }

    /// The final line.
    fn finish(self, outcome: &Outcome) {
        let mut err = std::io::stderr().lock();
        let elapsed = self.started.elapsed();
        if self.tty {
            let line = render_finished(self.style, self.name, outcome, elapsed);
            let _ = writeln!(err, "\r\x1b[2K{line}");
        } else {
            let text = match outcome {
                Outcome::Done(Some(d)) | Outcome::Failed(d) => d.clone(),
                Outcome::Done(None) => "done".to_owned(),
                Outcome::TimedOut(_) => "timed out".to_owned(),
            };
            let _ = writeln!(
                err,
                "{}: {text} in {}",
                self.name,
                xkr_term::steps::elapsed_text(elapsed)
            );
        }
    }
}

/// `xkrd db state-hashes`: `S_k` of every complete span ending at or below `to`, printed as
/// Rust list lines.
fn state_hashes(db: &DbArgs, to: Option<u64>) -> DynResult {
    let store = open(db)?;
    let result = print_state_hashes(&store, to);
    store.close()?;
    result
}

fn print_state_hashes(store: &Store, to: Option<u64>) -> DynResult {
    let r = store.reader()?;
    let mut comp = store.compressor()?;
    let Some(top) = r.top_height()? else {
        return Err("empty chain".into());
    };
    let spans = complete_spans(to.unwrap_or(top).min(top));
    let mut progress = StderrProgress::new("state hashes");
    let mut prev = Hash::ZERO;
    let mut out = std::io::stdout().lock();
    for span in 0..spans {
        progress.update(span, spans);
        let state = match span_digest(&r, &mut comp, span) {
            Ok(digest) => chain_state(&prev, &digest),
            Err(e) => {
                progress.finish(&Outcome::Failed(format!("span {span}: {e}")));
                return Err(e.into());
            }
        };
        writeln!(out, "    ({span}, \"{state}\"),")?;
        prev = state;
    }
    out.flush()?;
    progress.finish(&Outcome::Done(Some(format!(
        "{spans} spans, up to height {}",
        (spans * STRIPE_SPAN).saturating_sub(1)
    ))));
    Ok(())
}

/// `xkrd db verify-state`: recomputes every complete span and compares it with the stored
/// hashes (`META_BLOBS["state_hashes"]`, also the hash of each span's last block) and the
/// built-in list of the network.
fn verify_state(db: &DbArgs) -> DynResult {
    let store = open(db)?;
    let result = check_state_hashes(&store, db.network.into());
    store.close()?;
    result
}

fn check_state_hashes(store: &Store, network: xkr_consensus::Network) -> DynResult {
    let r = store.reader()?;
    let mut comp = store.compressor()?;
    let Some(top) = r.top_height()? else {
        return Err("empty chain".into());
    };
    let saved = store.state_hashes()?;
    let builtin = StateHashes::for_network(network);
    let spans = complete_spans(top);
    let mut p = Problems { count: 0 };
    let mut progress = StderrProgress::new("state hashes");
    let mut prev = Hash::ZERO;
    let (mut matched_saved, mut matched_builtin) = (0_u64, 0_u64);
    for span in 0..spans {
        progress.update(span, spans);
        let state = match span_digest(&r, &mut comp, span) {
            Ok(digest) => chain_state(&prev, &digest),
            Err(e) => {
                progress.problem(&mut p, &format!("span {span}: {e}"));
                break;
            }
        };
        prev = state;
        if let Some((s, last)) = saved.get(span as usize) {
            let end_hash = r.block_hash((span + 1) * STRIPE_SPAN - 1)?;
            if end_hash != Some(Hash(*last)) {
                progress.problem(
                    &mut p,
                    &format!(
                        "span {span}: the saved hash belongs to another last block than the chain's"
                    ),
                );
            } else if *s == state.0 {
                matched_saved += 1;
            } else {
                progress.problem(
                    &mut p,
                    &format!("span {span}: computed {state}, saved {}", Hash(*s)),
                );
            }
        }
        match builtin.get(span) {
            Some(b) if b == state => matched_builtin += 1,
            Some(b) => progress.problem(
                &mut p,
                &format!("span {span}: computed {state}, built-in {b}"),
            ),
            None => {}
        }
    }
    if saved.len() as u64 > spans {
        progress.problem(
            &mut p,
            &format!(
                "{} saved state hashes for {spans} complete spans",
                saved.len()
            ),
        );
    }
    let summary = format!(
        "{spans} spans, {matched_saved} of {} saved and {matched_builtin} built-in match",
        saved.len()
    );
    if p.count > 0 {
        progress.finish(&Outcome::Failed(format!("{summary}: {} problems", p.count)));
        return Err(format!("{} state hash problems found", p.count).into());
    }
    progress.finish(&Outcome::Done(Some(summary)));
    Ok(())
}

/// Collects verification failures (prints the first ones).
struct Problems {
    count: u64,
}

impl Problems {
    fn report(&mut self, msg: &str) {
        self.count += 1;
        if self.count <= 50 {
            eprintln!("verify: {msg}");
        }
    }

    fn check(&mut self, ok: bool, msg: impl FnOnce() -> String) {
        if !ok {
            self.report(&msg());
        }
    }
}

/// Per-amount bitmaps of the global indexes referenced by transactions.
#[derive(Default)]
struct SeenOutputs {
    bits: HashMap<u64, Vec<u64>>,
    total: u64,
}

impl SeenOutputs {
    /// Marks `(amount, gi)`; false if it was referenced already.
    fn mark(&mut self, amount: u64, gi: u32) -> bool {
        let words = self.bits.entry(amount).or_default();
        let (w, b) = (gi as usize / 64, gi % 64);
        if words.len() <= w {
            words.resize(w + 1, 0);
        }
        let fresh = words[w] & (1 << b) == 0;
        words[w] |= 1 << b;
        self.total += 1;
        fresh
    }

    fn count(&self, amount: u64) -> u64 {
        self.bits
            .get(&amount)
            .map_or(0, |w| w.iter().map(|x| u64::from(x.count_ones())).sum())
    }
}

#[derive(Default)]
struct Totals {
    blocks: u64,
    txs: u64,
    inputs: u64,
    escaped: u64,
    payment_ids: u64,
}

/// Checks one transaction's outputs against the outputs table.
#[allow(clippy::too_many_arguments)]
fn check_outputs(
    r: &xkr_storage::Reader,
    p: &mut Problems,
    seen: &mut SeenOutputs,
    totals: &mut Totals,
    height: u64,
    tx_index: u32,
    prefix: &TransactionPrefix,
    gis: Option<Vec<u32>>,
) -> DynResult {
    let Some(gis) = gis else {
        p.report(&format!("{height}/{tx_index}: no global indexes"));
        return Ok(());
    };
    p.check(gis.len() == prefix.outputs.len(), || {
        format!(
            "{height}/{tx_index}: {} global indexes for {} outputs",
            gis.len(),
            prefix.outputs.len()
        )
    });
    if !u32::try_from(prefix.unlock_time).is_ok_and(|u| u != u32::MAX) {
        totals.escaped += prefix.outputs.len() as u64;
    }
    for (o, gi) in prefix.outputs.iter().zip(&gis) {
        let (TxOutputTarget::Key(k) | TxOutputTarget::TaggedKey { key: k, .. }) = &o.target;
        p.check(seen.mark(o.amount, *gi), || {
            format!(
                "{height}/{tx_index}: output ({}, {gi}) referenced twice",
                o.amount
            )
        });
        match r.outputs(&[(o.amount, u64::from(*gi))])?.pop().flatten() {
            Some(row) => p.check(
                row.key == k.0 && row.height == height && row.unlock_time == prefix.unlock_time,
                || {
                    format!(
                        "{height}/{tx_index}: output ({}, {gi}) row mismatch",
                        o.amount
                    )
                },
            ),
            None => p.report(&format!(
                "{height}/{tx_index}: output ({}, {gi}) missing",
                o.amount
            )),
        }
    }
    Ok(())
}

#[allow(clippy::too_many_lines)]
fn verify(db: &DbArgs, from: u64, to: Option<u64>) -> DynResult {
    let store = open(db)?;
    let r = store.reader()?;
    let mut comp = store.compressor()?;
    let Some(top) = r.top_height()? else {
        println!("empty chain");
        return Ok(());
    };
    let to = to.unwrap_or(top).min(top);
    let full = from == 0 && to == top;
    let sealed = r.sealed_below();
    let prune = r.prune_state();
    if prune.is_pruned() {
        println!(
            "pruned database: seed {}, pruned below {} (missing signatures of dropped stripes are expected)",
            prune.seed, prune.pruned_below
        );
    }
    let mut p = Problems { count: 0 };
    let mut seen = SeenOutputs::default();
    let mut totals = Totals::default();
    let mut prev: Option<BlockInfo> = match from.checked_sub(1) {
        Some(h) => r.block_info(h)?,
        None => None,
    };
    let t = Instant::now();
    let mut last_log = Instant::now();

    for h in from..=to {
        totals.blocks += 1;
        let Some(info) = r.block_info(h)? else {
            p.report(&format!("{h}: no block info"));
            continue;
        };
        p.check(r.has_hot_row(h)? != (h < sealed), || {
            format!(
                "{h}: hot row present = {}, sealed below {sealed}",
                h >= sealed
            )
        });
        p.check(r.height_of(&Hash(info.hash))? == Some(h), || {
            format!("{h}: block hash index does not point back")
        });
        if let Some(pr) = &prev {
            p.check(
                info.cumulative_size == pr.cumulative_size + info.block_size
                    && info.cumulative_tx_count
                        == pr.cumulative_tx_count + 1 + u64::from(info.tx_count)
                    && info.cumulative_difficulty()
                        == pr.cumulative_difficulty() + u128::from(info.difficulty),
                || format!("{h}: cumulative fields inconsistent with the previous block"),
            );
        }
        p.check(info.is_pruned() == prune.dropped(h), || {
            format!("{h}: pruned flag disagrees with the prune state")
        });
        // Sealed chunks: signature parts exactly when the chunk is kept (and has signatures,
        // which the transaction walk below checks).
        if h < sealed && h.is_multiple_of(CHUNK_BLOCKS) && prune.chunk_dropped(h / CHUNK_BLOCKS) {
            p.check(!r.has_signature_parts(h / CHUNK_BLOCKS)?, || {
                format!(
                    "{h}: chunk {} of a dropped stripe still has signature parts",
                    h / CHUNK_BLOCKS
                )
            });
        }
        prev = Some(info);

        let Some(blob) = r.block_blob(h, &mut comp)? else {
            p.report(&format!("{h}: no block blob"));
            continue;
        };
        let block = match decode_block(&blob) {
            Ok(b) => b,
            Err(e) => {
                p.report(&format!("{h}: block does not decode: {e}"));
                continue;
            }
        };
        p.check(block_hash(&block)?.0 == info.hash, || {
            format!("{h}: block hash mismatch")
        });
        p.check(block.header.timestamp == info.timestamp, || {
            format!("{h}: timestamp mismatch")
        });
        p.check(r.has_timestamp(block.header.timestamp, h)?, || {
            format!("{h}: timestamp not indexed")
        });
        p.check(block.tx_hashes.len() == info.tx_count as usize, || {
            format!("{h}: tx count mismatch")
        });

        totals.txs += 1;
        let miner_hash = transaction_hash(&block.miner_tx)?;
        p.check(
            r.tx_location(&miner_hash)?
                .is_some_and(|l| l.height == h && l.tx_index == 0),
            || format!("{h}: miner tx location"),
        );
        let gis = r.tx_global_indexes(h, 0, &mut comp)?;
        check_outputs(
            &r,
            &mut p,
            &mut seen,
            &mut totals,
            h,
            0,
            &block.miner_tx.prefix,
            gis,
        )?;

        for (i, th) in block.tx_hashes.iter().enumerate() {
            totals.txs += 1;
            let idx = u32::try_from(i + 1)?;
            let Some(prefix_blob) = r.tx_prefix_at(h, idx, &mut comp)? else {
                p.report(&format!("{h}/{idx}: no prefix"));
                continue;
            };
            let prefix = match decode_transaction_prefix(&prefix_blob) {
                Ok(x) => x,
                Err(e) => {
                    p.report(&format!("{h}/{idx}: prefix does not decode: {e}"));
                    continue;
                }
            };
            // Kept blocks must come back whole; dropped ones must answer `Pruned`.
            match r.transaction_blob(th, &mut comp) {
                Ok(Some(full)) => p.check(
                    !prune.dropped(h)
                        && full.starts_with(&prefix_blob)
                        && transaction_hash_from_blob(&full) == *th,
                    || format!("{h}/{idx}: transaction hash mismatch or pruned signatures present"),
                ),
                Ok(None) => p.report(&format!("{h}/{idx}: transaction blob missing")),
                Err(xkr_storage::Error::Pruned(_)) if prune.dropped(h) => {}
                Err(e) => p.report(&format!("{h}/{idx}: signatures: {e}")),
            }
            p.check(
                r.tx_location(th)?
                    .is_some_and(|l| l.height == h && l.tx_index == idx),
                || format!("{h}/{idx}: tx location"),
            );
            for input in &prefix.inputs {
                if let TxInput::Key { key_image, .. } = input {
                    totals.inputs += 1;
                    p.check(r.is_key_image_spent(key_image)?, || {
                        format!("{h}/{idx}: key image {key_image} not indexed")
                    });
                }
            }
            if let Some(pid) = payment_id(&prefix.extra) {
                totals.payment_ids += 1;
                p.check(r.has_payment_id(&pid.0, h, idx)?, || {
                    format!("{h}/{idx}: payment id not indexed")
                });
            }
            let gis = r.tx_global_indexes(h, idx, &mut comp)?;
            check_outputs(&r, &mut p, &mut seen, &mut totals, h, idx, &prefix, gis)?;
        }

        if last_log.elapsed().as_secs() >= 10 {
            println!(
                "verified up to {h} ({:.0} blocks/s), {} problems",
                ratio(h - from + 1, t.elapsed().as_secs().max(1)),
                p.count
            );
            last_log = Instant::now();
        }
    }

    if full {
        let lens: HashMap<&str, u64> = r.table_lens()?.into_iter().collect();
        let mut expect = |table: &str, n: u64| {
            p.check(lens.get(table) == Some(&n), || {
                format!("table {table}: {:?} entries, expected {n}", lens.get(table))
            });
        };
        expect("block_infos", totals.blocks);
        expect("block_hash_to_height", totals.blocks);
        expect("timestamps", totals.blocks);
        expect("hot_blocks", top + 1 - sealed);
        expect("tx_hash_to_loc", totals.txs);
        expect("key_images", totals.inputs);
        expect("outputs", seen.total);
        expect("unlock_overflow", totals.escaped);
        expect("payment_ids", totals.payment_ids);
        let counts = r.output_counts()?;
        expect("output_counts", counts.len() as u64);
        for (amount, count) in counts {
            p.check(seen.count(amount) == count, || {
                format!(
                    "amount {amount}: count {count}, {} outputs referenced",
                    seen.count(amount)
                )
            });
        }
    }

    println!(
        "verified blocks {from}..={to}: {} blocks, {} transactions, {} key images, {} outputs in {:.0} s: {} problems",
        totals.blocks,
        totals.txs,
        totals.inputs,
        seen.total,
        t.elapsed().as_secs_f64(),
        p.count
    );
    if p.count > 0 {
        return Err(format!("{} problems found", p.count).into());
    }
    Ok(())
}

/// The v1 block at `height` as a [`VerifiedBlock`] (trusted: the v1 node verified it).
fn v1_block(v1: &mut V1Reader, height: u64) -> DynResult<VerifiedBlock> {
    let (info, raw) = v1
        .block(height)?
        .ok_or_else(|| format!("v1 block {height} missing"))?;
    let block = decode_block(&raw.block)?;
    let miner_tx_hash = transaction_hash(&block.miner_tx)?;
    if block.tx_hashes.len() != raw.transactions.len() {
        return Err(format!("v1 block {height}: transaction count mismatch").into());
    }
    let mut txs = Vec::with_capacity(raw.transactions.len());
    for (blob, hash) in raw.transactions.into_iter().zip(&block.tx_hashes) {
        let tx = decode_transaction(&blob).map_err(|e| {
            format!("v1 block {height}: transaction {hash}: {e} (pruned v1 database?)")
        })?;
        let sig_bytes: usize = tx.signatures.iter().map(|s| s.len() * 64).sum();
        txs.push(VerifiedTx {
            hash: *hash,
            prefix_len: blob.len().saturating_sub(sig_bytes),
            payment_id: payment_id(&tx.prefix.extra).map(|h| h.0),
            tx,
            blob,
        });
    }
    Ok(VerifiedBlock {
        height,
        hash: Hash(info.hash),
        block_blob: raw.block,
        block,
        miner_tx_hash,
        difficulty: info.difficulty,
        already_generated_coins: info.already_generated_coins,
        block_size: info.block_size,
        txs,
    })
}

fn convert(from: &Path, db: &DbArgs, batch: usize) -> DynResult {
    let mut v1 = V1Reader::open(from)?;
    let Some(top) = v1.top_height() else {
        return Err("the v1 database is empty".into());
    };
    let path = db.db_path();
    if path.exists() {
        return Err(format!("{} exists already; convert into a new file", path.display()).into());
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let store = Store::open_with_cache(&path, db.db_cache_mb.max(16) * 1024 * 1024)?;
    let net = match db.network {
        crate::cli::NetworkArg::Mainnet => "mainnet",
        crate::cli::NetworkArg::Testnet => "testnet",
    };
    if let Some(dict) = xkr_storage::builtin_dictionary(net)
        && store.install_dictionary_if_fresh(dict)?
    {
        println!(
            "installed the shipped {net} dictionary ({} bytes)",
            dict.len()
        );
    }
    let mut comp = store.compressor()?;
    let t = Instant::now();
    let mut last = Instant::now();
    let mut height = 0_u64;
    while height <= top {
        let end = (height + batch.max(1) as u64).min(top + 1);
        let blocks = (height..end)
            .map(|h| v1_block(&mut v1, h))
            .collect::<DynResult<Vec<_>>>()?;
        let txn = store.begin_write()?;
        push_blocks(&txn, &blocks, &mut comp)?;
        txn.commit()?;
        height = end;
        if last.elapsed().as_secs() >= 10 || height > top {
            println!(
                "converted {height}/{} blocks ({:.0} blocks/s)",
                top + 1,
                ratio(height, t.elapsed().as_secs().max(1))
            );
            last = Instant::now();
        }
    }
    drop(v1);
    let report = store.maintain(&MaintainOptions::default(), &mut |msg| println!("{msg}"))?;
    println!(
        "done in {:.0} s; new file {:.2} MB (v1 file {:.2} MB)",
        t.elapsed().as_secs_f64(),
        mb(report.file_bytes_after),
        mb(std::fs::metadata(from)?.len())
    );
    print_stats(&path, &store.stats()?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use crate::cli::{Args, Command, DbCommand};

    #[test]
    fn db_subcommands_parse_and_default_mode_is_unchanged() {
        let a = Args::try_parse_from(["xkrd", "db", "stats", "--data-dir", "/tmp/x"]).unwrap();
        let Some(Command::Db(DbCommand::Stats(db))) = a.command else {
            panic!("stats");
        };
        assert_eq!(db.db_path(), std::path::Path::new("/tmp/x/chain.redb"));

        let a = Args::try_parse_from([
            "xkrd",
            "db",
            "train-dict",
            "--network",
            "testnet",
            "--db",
            "/tmp/y.redb",
            "--samples",
            "100",
            "--max-dict-bytes",
            "110000",
        ])
        .unwrap();
        let Some(Command::Db(DbCommand::TrainDict {
            db,
            samples,
            max_dict_bytes,
            ..
        })) = a.command
        else {
            panic!("train-dict");
        };
        assert_eq!((samples, max_dict_bytes), (100, 110_000));
        assert_eq!(db.db_path(), std::path::Path::new("/tmp/y.redb"));

        assert!(Args::try_parse_from(["xkrd", "db", "compact", "--force", "--no-repack"]).is_err());

        let a = Args::try_parse_from(["xkrd", "db", "prune", "--stripes", "6", "--stripe", "2"])
            .unwrap();
        let Some(Command::Db(DbCommand::Prune {
            all,
            stripes,
            stripe,
            below,
            ..
        })) = a.command
        else {
            panic!("prune");
        };
        assert_eq!(
            (all, stripes, stripe, below),
            (false, Some(6), Some(2), None)
        );
        assert!(Args::try_parse_from(["xkrd", "db", "prune", "--all", "--below", "64"]).is_ok());
        assert!(
            Args::try_parse_from(["xkrd", "db", "prune", "--all", "--force-below"]).is_err(),
            "--force-below needs --below"
        );
        let a = Args::try_parse_from([
            "xkrd",
            "db",
            "prune",
            "--stripes",
            "4",
            "--below",
            "8192",
            "--force-below",
        ])
        .unwrap();
        let Some(Command::Db(DbCommand::Prune {
            below, force_below, ..
        })) = a.command
        else {
            panic!("prune --force-below");
        };
        assert_eq!((below, force_below), (Some(8192), true));
        // Above the bound only with the override; at or below it, or the bound itself, always.
        let err = super::prune_height(Some(8192), 0, false).unwrap_err();
        assert!(
            err.contains("safety bound 0") && err.contains("--force-below"),
            "{err}"
        );
        assert_eq!(super::prune_height(Some(8192), 0, true), Ok(8192));
        assert_eq!(super::prune_height(Some(4096), 8192, false), Ok(4096));
        assert_eq!(super::prune_height(None, 8192, false), Ok(8192));
        assert!(Args::try_parse_from(["xkrd", "db", "prune", "--all", "--stripes", "4"]).is_err());
        assert!(Args::try_parse_from(["xkrd", "db", "prune", "--stripes", "9"]).is_err());
        assert!(Args::try_parse_from(["xkrd", "db", "prune", "--stripes", "1"]).is_err());
        let a = Args::try_parse_from(["xkrd", "--prune", "--prune-stripes", "6"]).unwrap();
        assert!(a.prune);
        assert_eq!(a.prune_stripes, Some(6));
        assert!(Args::try_parse_from(["xkrd", "--prune-stripes", "4"]).is_err());
        assert!(Args::try_parse_from(["xkrd", "--prune", "--prune-stripes", "9"]).is_err());
        for n in 2..=8 {
            let seed = super::pick_seed(n).unwrap();
            assert_eq!(seed.stripes(), n);
            assert!((1..=n).contains(&seed.stripe()));
        }
        assert!(super::pick_seed(1).is_none());
        let a = Args::try_parse_from(["xkrd", "--network", "testnet", "--db-durable-secs", "9"])
            .unwrap();
        assert!(a.command.is_none());
        assert_eq!(a.db_durable_secs, 9);
        assert!(!a.no_db_maintenance);
    }
}
