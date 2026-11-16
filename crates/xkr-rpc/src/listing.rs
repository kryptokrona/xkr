//! JSON answers written straight into a byte buffer with a size budget: the transaction-pool
//! listings (`/get_pool_changes_lite`, `/get_pool_changes`, `/get_pool`) and the shared list
//! writer the detail routes use.
//!
//! A listing writes its entries one at a time (one pool entry is cloned at a time) until the
//! answer holds [`MAX_POOL_ANSWER_BYTES`]; the first entry is always written. The remaining
//! entries follow on the next poll: wallet-backend-js adds every returned hash to its known
//! list, and `/get_pool` lists the oldest transactions first, so a caller asks again with a
//! later `timestampBegin`.

use std::fmt::Display;

use axum::http::header;
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use serde::ser::{SerializeMap, Serializer};
use xkr_txpool::PoolEntry;
use xkr_types::{Hash, TransactionPrefix, TxInput, TxOutput, TxOutputTarget};

/// Most bytes of JSON in one pool listing answer (one entry more may follow the last check).
pub(crate) const MAX_POOL_ANSWER_BYTES: usize = 8 * 1024 * 1024;

/// A JSON response from serialized bytes.
pub(crate) fn json_response(body: Vec<u8>) -> Response {
    ([(header::CONTENT_TYPE, "application/json")], body).into_response()
}

/// A JSON array written into a buffer after `prefix`, entry by entry, until the buffer holds
/// `budget` bytes.
pub(crate) struct JsonList {
    out: Vec<u8>,
    count: usize,
    budget: usize,
}

impl JsonList {
    /// Starts `prefix` (for example `{"addedTxs":`) followed by `[`.
    pub(crate) fn new(prefix: &[u8], budget: usize) -> Self {
        let mut out = Vec::with_capacity(prefix.len().saturating_add(64));
        out.extend_from_slice(prefix);
        out.push(b'[');
        Self {
            out,
            count: 0,
            budget,
        }
    }

    /// Appends `item` unless the buffer already holds the budget (the first item is always
    /// appended). False once the budget is used up.
    pub(crate) fn push<T: Serialize>(&mut self, item: &T) -> bool {
        if self.count > 0 && self.out.len() >= self.budget {
            return false;
        }
        let mark = self.out.len();
        if self.count > 0 {
            self.out.push(b',');
        }
        if serde_json::to_writer(&mut self.out, item).is_err() {
            self.out.truncate(mark);
            return true;
        }
        self.count += 1;
        true
    }

    /// Like [`Self::push`] with an entry that is already serialized JSON.
    pub(crate) fn push_raw(&mut self, item: &[u8]) -> bool {
        if self.count > 0 && self.out.len() >= self.budget {
            return false;
        }
        if self.count > 0 {
            self.out.push(b',');
        }
        self.out.extend_from_slice(item);
        self.count += 1;
        true
    }

    /// Entries written.
    #[cfg(test)]
    pub(crate) fn count(&self) -> usize {
        self.count
    }

    /// Closes the array and appends `suffix` (for example `,"status":"OK"}`).
    pub(crate) fn finish(mut self, suffix: &[u8]) -> Vec<u8> {
        self.out.push(b']');
        self.out.extend_from_slice(suffix);
        self.out
    }
}

/// A value written with its `Display` form as a JSON string.
pub(crate) struct Shown<'a, T: Display>(pub(crate) &'a T);

impl<T: Display> Serialize for Shown<'_, T> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self.0)
    }
}

/// Bytes as a JSON array of numbers (C++ serializes a `BinaryArray` that way).
pub(crate) struct ByteArray<'a>(pub(crate) &'a [u8]);

impl Serialize for ByteArray<'_> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_seq(self.0.iter())
    }
}

/// C++ JSON of a `TransactionPrefix` (`explorer::prefix_json` is this as a `Value`); keys in
/// alphabetical order like `serde_json::Value`.
pub(crate) struct PrefixJson<'a>(pub(crate) &'a TransactionPrefix);

impl Serialize for PrefixJson<'_> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let p = self.0;
        let mut m = s.serialize_map(Some(5))?;
        m.serialize_entry("extra", &hex::encode(&p.extra))?;
        m.serialize_entry("unlock_time", &p.unlock_time)?;
        m.serialize_entry("version", &p.version)?;
        m.serialize_entry("vin", &Inputs(&p.inputs))?;
        m.serialize_entry("vout", &Outputs(&p.outputs))?;
        m.end()
    }
}

struct Inputs<'a>(&'a [TxInput]);

impl Serialize for Inputs<'_> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_seq(self.0.iter().map(InputJson))
    }
}

struct Outputs<'a>(&'a [TxOutput]);

impl Serialize for Outputs<'_> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_seq(self.0.iter().map(OutputJson))
    }
}

#[derive(Serialize)]
struct CoinbaseValue {
    height: u32,
}

#[derive(Serialize)]
struct KeyValue<'a> {
    amount: u64,
    k_image: Shown<'a, xkr_types::KeyImage>,
    key_offsets: &'a [u32],
}

struct InputJson<'a>(&'a TxInput);

impl Serialize for InputJson<'_> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut m = s.serialize_map(Some(2))?;
        match self.0 {
            TxInput::Coinbase { height } => {
                m.serialize_entry("type", "ff")?;
                m.serialize_entry("value", &CoinbaseValue { height: *height })?;
            }
            TxInput::Key {
                amount,
                output_indexes,
                key_image,
            } => {
                m.serialize_entry("type", "02")?;
                m.serialize_entry(
                    "value",
                    &KeyValue {
                        amount: *amount,
                        k_image: Shown(key_image),
                        key_offsets: output_indexes,
                    },
                )?;
            }
        }
        m.end()
    }
}

struct OutputJson<'a>(&'a TxOutput);

impl Serialize for OutputJson<'_> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut m = s.serialize_map(Some(2))?;
        m.serialize_entry("amount", &self.0.amount)?;
        m.serialize_entry("target", &TargetJson(&self.0.target))?;
        m.end()
    }
}

/// `{ "data": { "key" }, "type": "02" }` or, tagged, `{ "data": { "key", "view_tag" }, "type": "03" }`.
struct TargetJson<'a>(&'a TxOutputTarget);

#[derive(Serialize)]
struct TargetData<'a> {
    key: Shown<'a, xkr_types::PublicKey>,
    #[serde(skip_serializing_if = "Option::is_none")]
    view_tag: Option<String>,
}

impl Serialize for TargetJson<'_> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut m = s.serialize_map(Some(2))?;
        match self.0 {
            TxOutputTarget::Key(key) => {
                m.serialize_entry(
                    "data",
                    &TargetData {
                        key: Shown(key),
                        view_tag: None,
                    },
                )?;
                m.serialize_entry("type", "02")?;
            }
            TxOutputTarget::TaggedKey { key, view_tag } => {
                m.serialize_entry(
                    "data",
                    &TargetData {
                        key: Shown(key),
                        view_tag: Some(hex::encode([*view_tag])),
                    },
                )?;
                m.serialize_entry("type", "03")?;
            }
        }
        m.end()
    }
}

/// One C++ `TransactionPrefixInfo` (`src/cryptonote_core/icore_definitions.h`, serialized by
/// `serialize(TransactionPrefixInfo &, ISerializer &)` in `src/rpc/rpc_server.cpp`): an
/// `addedTxs` entry of `/get_pool_changes_lite` and `/get_pool`, and a `txPrefixes` entry of
/// `/queryblockslite`. The C++ keys contain a dot (`KV_MEMBER(transactionPrefixInfo.txHash)`).
#[derive(Serialize)]
pub(crate) struct PrefixEntry<'a> {
    #[serde(rename = "transactionPrefixInfo.txHash")]
    hash: Shown<'a, Hash>,
    #[serde(rename = "transactionPrefixInfo.txPrefix")]
    prefix: PrefixJson<'a>,
}

impl<'a> PrefixEntry<'a> {
    /// The entry for the transaction `hash` with `prefix`.
    pub(crate) fn new(hash: &'a Hash, prefix: &'a TransactionPrefix) -> Self {
        Self {
            hash: Shown(hash),
            prefix: PrefixJson(prefix),
        }
    }
}

/// `,"deletedTxsIds":[...],"isTailBlockActual":...,"status":"OK"}`.
fn changes_suffix(deleted: &[Hash], tail_actual: bool) -> Vec<u8> {
    let mut out = Vec::with_capacity(deleted.len() * 67 + 64);
    out.extend_from_slice(b",\"deletedTxsIds\":");
    let ids: Vec<Shown<'_, Hash>> = deleted.iter().map(Shown).collect();
    if serde_json::to_writer(&mut out, &ids).is_err() {
        out.extend_from_slice(b"[]");
    }
    out.extend_from_slice(if tail_actual {
        b",\"isTailBlockActual\":true,\"status\":\"OK\"}".as_slice()
    } else {
        b",\"isTailBlockActual\":false,\"status\":\"OK\"}".as_slice()
    });
    out
}

/// `/get_pool_changes_lite`: `{"addedTxs":[prefix entries],"deletedTxsIds","isTailBlockActual","status"}`
/// with at most `max` entries of `added` and [`MAX_POOL_ANSWER_BYTES`].
pub(crate) fn pool_changes_lite(
    added: impl Iterator<Item = PoolEntry>,
    max: usize,
    deleted: &[Hash],
    tail_actual: bool,
) -> Vec<u8> {
    let mut list = JsonList::new(b"{\"addedTxs\":", MAX_POOL_ANSWER_BYTES);
    for e in added.take(max) {
        let entry = PrefixEntry {
            hash: Shown(&e.hash),
            prefix: PrefixJson(&e.tx.prefix),
        };
        if !list.push(&entry) {
            break;
        }
    }
    list.finish(&changes_suffix(deleted, tail_actual))
}

/// `/get_pool_changes`: like [`pool_changes_lite`] with whole transaction blobs as arrays of
/// byte values.
pub(crate) fn pool_changes(
    added: impl Iterator<Item = Vec<u8>>,
    max: usize,
    deleted: &[Hash],
    tail_actual: bool,
) -> Vec<u8> {
    let mut list = JsonList::new(b"{\"addedTxs\":", MAX_POOL_ANSWER_BYTES);
    for blob in added.take(max) {
        if !list.push(&ByteArray(&blob)) {
            break;
        }
    }
    list.finish(&changes_suffix(deleted, tail_actual))
}

/// `/get_pool`: `{"addedTxs":[prefix entries],"status":"OK"}` with at most `max` entries and
/// [`MAX_POOL_ANSWER_BYTES`].
pub(crate) fn pool_list(entries: impl Iterator<Item = PoolEntry>, max: usize) -> Vec<u8> {
    let mut list = JsonList::new(b"{\"addedTxs\":", MAX_POOL_ANSWER_BYTES);
    for e in entries.take(max) {
        let entry = PrefixEntry {
            hash: Shown(&e.hash),
            prefix: PrefixJson(&e.tx.prefix),
        };
        if !list.push(&entry) {
            break;
        }
    }
    list.finish(b",\"status\":\"OK\"}")
}

#[cfg(test)]
#[allow(unsafe_code)] // the counting global allocator below
mod tests {
    use std::alloc::{GlobalAlloc, Layout, System};
    use std::cell::Cell;
    use std::sync::Arc;

    use serde_json::{Value, json};
    use xkr_types::{KeyImage, PublicKey, Transaction};

    use super::*;

    struct Counting;

    thread_local! {
        static ACTIVE: Cell<bool> = const { Cell::new(false) };
        static CURRENT: Cell<usize> = const { Cell::new(0) };
        static PEAK: Cell<usize> = const { Cell::new(0) };
    }

    fn on_alloc(n: usize) {
        if ACTIVE.try_with(Cell::get).unwrap_or(false) {
            let cur = CURRENT.try_with(Cell::get).unwrap_or(0).saturating_add(n);
            let _ = CURRENT.try_with(|c| c.set(cur));
            let _ = PEAK.try_with(|p| p.set(p.get().max(cur)));
        }
    }

    fn on_dealloc(n: usize) {
        if ACTIVE.try_with(Cell::get).unwrap_or(false) {
            let _ = CURRENT.try_with(|c| c.set(c.get().saturating_sub(n)));
        }
    }

    // SAFETY: forwards every call to `System` unchanged; the bookkeeping only touches
    // const-initialised thread-locals without destructors.
    unsafe impl GlobalAlloc for Counting {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            on_alloc(layout.size());
            unsafe { System.alloc(layout) }
        }

        unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
            on_alloc(layout.size());
            unsafe { System.alloc_zeroed(layout) }
        }

        unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
            on_dealloc(layout.size());
            unsafe { System.dealloc(ptr, layout) }
        }

        unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
            on_alloc(new_size);
            on_dealloc(layout.size());
            unsafe { System.realloc(ptr, layout, new_size) }
        }
    }

    #[global_allocator]
    static ALLOCATOR: Counting = Counting;

    /// Runs `f` and returns its result with the most bytes it had allocated at once on this
    /// thread.
    fn peak_alloc<R>(f: impl FnOnce() -> R) -> (R, usize) {
        let _ = CURRENT.try_with(|c| c.set(0));
        let _ = PEAK.try_with(|p| p.set(0));
        let _ = ACTIVE.try_with(|a| a.set(true));
        let r = f();
        let _ = ACTIVE.try_with(|a| a.set(false));
        (r, PEAK.try_with(Cell::get).unwrap_or(0))
    }

    /// A pool entry with `outputs` key outputs and a blob of `blob_len` bytes.
    fn entry(n: u8, outputs: usize, blob_len: usize) -> PoolEntry {
        let tx = Transaction {
            prefix: TransactionPrefix {
                version: 1,
                unlock_time: 0,
                inputs: vec![TxInput::Key {
                    amount: 10,
                    output_indexes: vec![1, 2, 3],
                    key_image: KeyImage([n; 32]),
                }],
                outputs: (0..outputs)
                    .map(|i| TxOutput {
                        amount: 1,
                        target: if i.is_multiple_of(2) {
                            TxOutputTarget::Key(PublicKey([n; 32]))
                        } else {
                            TxOutputTarget::TaggedKey {
                                key: PublicKey([n; 32]),
                                view_tag: 0xab,
                            }
                        },
                    })
                    .collect(),
                extra: vec![1, 2, 3],
            },
            signatures: Vec::new(),
            clsag_signatures: Vec::new(),
        };
        PoolEntry {
            hash: Hash([n; 32]),
            blob: Arc::from(vec![7u8; blob_len]),
            tx,
            fee: 10,
            received: u64::from(n),
            fusion: false,
        }
    }

    /// `PrefixJson` writes what `explorer::prefix_json` builds (both input and both output
    /// kinds, and a coinbase).
    #[test]
    fn prefix_json_matches_the_value_form() {
        let mut e = entry(3, 4, 10);
        e.tx.prefix.inputs.push(TxInput::Coinbase { height: 77 });
        let direct: Value = serde_json::to_value(PrefixJson(&e.tx.prefix)).unwrap();
        let expected = json!({
            "version": 1,
            "unlock_time": 0,
            "extra": "010203",
            "vin": [
                { "type": "02", "value": { "amount": 10, "key_offsets": [1, 2, 3], "k_image": KeyImage([3; 32]).to_string() } },
                { "type": "ff", "value": { "height": 77 } },
            ],
            "vout": (0..4usize).map(|i| if i.is_multiple_of(2) {
                json!({ "amount": 1, "target": { "type": "02", "data": { "key": PublicKey([3; 32]).to_string() } } })
            } else {
                json!({ "amount": 1, "target": { "type": "03", "data": { "key": PublicKey([3; 32]).to_string(), "view_tag": "ab" } } })
            }).collect::<Vec<_>>(),
        });
        assert_eq!(direct, expected);
        assert_eq!(crate::explorer::prefix_json(&e.tx.prefix), expected);
    }

    #[test]
    fn listings_keep_the_c_shape() {
        let entries = vec![entry(1, 2, 5), entry(2, 2, 5)];
        let deleted = [Hash([9; 32])];
        let v: Value = serde_json::from_slice(&pool_changes_lite(
            entries.clone().into_iter(),
            10,
            &deleted,
            true,
        ))
        .unwrap();
        assert_eq!(v["status"], "OK");
        assert_eq!(v["isTailBlockActual"], true);
        assert_eq!(v["deletedTxsIds"], json!([Hash([9; 32]).to_string()]));
        assert_eq!(v["addedTxs"].as_array().unwrap().len(), 2);
        assert_eq!(
            v["addedTxs"][0]["transactionPrefixInfo.txHash"],
            Hash([1; 32]).to_string()
        );
        assert_eq!(
            v["addedTxs"][0]["transactionPrefixInfo.txPrefix"],
            crate::explorer::prefix_json(&entries[0].tx.prefix)
        );
        let v: Value = serde_json::from_slice(&pool_changes(
            entries.iter().map(|e| e.blob.to_vec()),
            1,
            &[],
            false,
        ))
        .unwrap();
        assert_eq!(
            v,
            json!({ "addedTxs": [[7, 7, 7, 7, 7]], "deletedTxsIds": [], "isTailBlockActual": false, "status": "OK" })
        );
        let v: Value = serde_json::from_slice(&pool_list(std::iter::empty(), 10)).unwrap();
        assert_eq!(v, json!({ "addedTxs": [], "status": "OK" }));
    }

    #[test]
    fn json_list_stops_at_the_budget() {
        let mut list = JsonList::new(b"{\"x\":", 10);
        assert!(list.push(&"a".repeat(50)));
        assert!(!list.push(&"b"));
        assert_eq!(list.count(), 1);
        let v: Value = serde_json::from_slice(&list.finish(b"}")).unwrap();
        assert_eq!(v, json!({ "x": ["a".repeat(50)] }));
    }

    /// A pool of output-heavy transactions: each answer stays within the budget plus one entry,
    /// and building it allocates a small multiple of the budget.
    #[test]
    fn pool_listings_are_bounded_in_bytes_and_memory() {
        let entries: Vec<PoolEntry> = (0..40).map(|n| entry(n, 6_000, 200_000)).collect();
        let one_entry = {
            let e = &entries[0];
            serde_json::to_vec(&PrefixEntry {
                hash: Shown(&e.hash),
                prefix: PrefixJson(&e.tx.prefix),
            })
            .unwrap()
            .len()
        };
        let (lite, peak) =
            peak_alloc(|| pool_changes_lite(entries.iter().cloned(), usize::MAX, &[], true));
        assert!(
            lite.len() <= MAX_POOL_ANSWER_BYTES + one_entry + 1024,
            "{}",
            lite.len()
        );
        let v: Value = serde_json::from_slice(&lite).unwrap();
        let written = v["addedTxs"].as_array().unwrap().len();
        assert!(written > 1 && written < entries.len(), "{written}");
        assert!(peak <= 4 * MAX_POOL_ANSWER_BYTES, "peak {peak}");

        let (full, peak) = peak_alloc(|| {
            pool_changes(
                entries.iter().map(|e| e.blob.to_vec()),
                usize::MAX,
                &[],
                true,
            )
        });
        assert!(full.len() <= MAX_POOL_ANSWER_BYTES + 4 * 200_000 + 1024);
        assert!(peak <= 4 * MAX_POOL_ANSWER_BYTES, "peak {peak}");

        let (list, peak) = peak_alloc(|| pool_list(entries.iter().cloned(), usize::MAX));
        assert!(list.len() <= MAX_POOL_ANSWER_BYTES + one_entry + 1024);
        assert!(peak <= 4 * MAX_POOL_ANSWER_BYTES, "peak {peak}");
    }
}
