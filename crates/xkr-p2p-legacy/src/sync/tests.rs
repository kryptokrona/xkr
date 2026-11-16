use super::*;

/// Hash with the index in the first 4 bytes (distinct for every index).
fn hid(i: u32) -> Hash {
    let mut b = [0_u8; 32];
    b[..4].copy_from_slice(&i.to_le_bytes());
    b[31] = 0xee;
    Hash(b)
}

/// Mock blob format for blocks: 32-byte id followed by the tx hashes (32 bytes each).
fn block_blob(id: Hash, txs: &[Hash]) -> Vec<u8> {
    let mut v = id.0.to_vec();
    for t in txs {
        v.extend_from_slice(&t.0);
    }
    v
}

/// Mock tx blob: the first 32 bytes are its id.
fn tx_blob(id: Hash) -> Vec<u8> {
    let mut v = id.0.to_vec();
    v.push(1);
    v
}

#[derive(Default)]
struct MockChain {
    /// main chain ids, index = height index
    main: Vec<Hash>,
    pool: Vec<Hash>,
}

impl ChainView for MockChain {
    fn top(&self) -> (u32, Hash) {
        (self.main.len() as u32, *self.main.last().unwrap())
    }
    fn main_chain_index(&self, hash: &Hash) -> Option<u32> {
        self.main.iter().position(|h| h == hash).map(|p| p as u32)
    }
    fn has_block(&self, hash: &Hash) -> bool {
        self.main.contains(hash)
    }
    fn block_hash_at(&self, index: u32) -> Option<Hash> {
        self.main.get(index as usize).copied()
    }
    fn block_hashes(&self, start: u32, max: usize) -> Vec<Hash> {
        self.main
            .iter()
            .skip(start as usize)
            .take(max)
            .copied()
            .collect()
    }
    fn raw_blocks(&self, hashes: &[Hash], found: &mut Vec<RawBlock>, missed: &mut Vec<Hash>) {
        for h in hashes {
            if self.has_block(h) {
                found.push(RawBlock {
                    block: block_blob(*h, &[]),
                    transactions: vec![],
                });
            } else {
                missed.push(*h);
            }
        }
    }
    fn transaction_blob(&self, hash: &Hash) -> Option<Vec<u8>> {
        self.pool.contains(hash).then(|| tx_blob(*hash))
    }
    fn pool_hashes(&self) -> Vec<Hash> {
        self.pool.clone()
    }
    fn pool_txs_not_in(&self, known: &[Hash]) -> Vec<Vec<u8>> {
        self.pool
            .iter()
            .filter(|h| !known.contains(h))
            .map(|h| tx_blob(*h))
            .collect()
    }
    fn parse_block_template(&self, blob: &[u8]) -> Option<TemplateInfo> {
        if blob.len() < 32 || !blob.len().is_multiple_of(32) {
            return None;
        }
        let mut chunks = blob.as_chunks::<32>().0.iter().map(|c| Hash(*c));
        let hash = chunks.next()?;
        Some(TemplateInfo {
            hash,
            tx_hashes: chunks.collect(),
        })
    }
    fn tx_hash(&self, tx_blob: &[u8]) -> Hash {
        Hash(tx_blob[..32].try_into().unwrap())
    }
}

fn chain(len: u32) -> MockChain {
    MockChain {
        main: (0..len).map(hid).collect(),
        pool: vec![],
    }
}

fn sends(out: &[SyncAction]) -> Vec<&ProtocolMessage> {
    out.iter()
        .filter_map(|a| match a {
            SyncAction::Send(m) => Some(m),
            _ => None,
        })
        .collect()
}

#[test]
fn sparse_chain_matches_cpp() {
    // doBuildSparseChain(top index 20): 20, 19, 18, 16, 12, 4, then genesis
    assert_eq!(sparse_chain_indices(20), vec![20, 19, 18, 16, 12, 4, 0]);
    assert_eq!(sparse_chain_indices(0), vec![0]);
    assert_eq!(sparse_chain_indices(1), vec![1, 0]);
    assert_eq!(sparse_chain_indices(2), vec![2, 1, 0]);
    let big = sparse_chain_indices(2_617_000);
    assert_eq!(big.first(), Some(&2_617_000));
    assert_eq!(big.last(), Some(&0));
    assert!(big.len() < 30);

    let c = chain(21);
    let sparse = build_sparse_chain(&c);
    assert_eq!(sparse[0], hid(20));
    assert_eq!(*sparse.last().unwrap(), hid(0));
    assert_eq!(
        find_supplement_start(&c, &[hid(99), hid(7), hid(0)]),
        Some(7)
    );
}

#[test]
fn full_sync_flow() {
    // We have blocks 0..=9, the peer has 0..=249.
    let mut local = chain(10);
    let mut st = PeerSync::default();
    let mut out = Vec::new();

    step(
        &mut st,
        SyncInput::Handshake {
            core_sync: CoreSyncData {
                current_height: 250,
                top_id: hid(249),
                pruning_seed: 0,
            },
            version: 5,
        },
        &local,
        &mut out,
    );
    assert_eq!(st.status, SyncStatus::Synchronizing);
    assert_eq!(st.remote_height, 250);
    match sends(&out)[..] {
        [ProtocolMessage::RequestChain(r)] => {
            assert_eq!(r.block_ids[0], hid(9));
            assert_eq!(*r.block_ids.last().unwrap(), hid(0));
        }
        _ => panic!("expected one RequestChain, got {out:?}"),
    }

    // Peer answers with ids 9..=249 (start = 9, the first block we have).
    out.clear();
    step(
        &mut st,
        SyncInput::Message(ProtocolMessage::ResponseChainEntry(ResponseChainEntry {
            start_height: 9,
            total_height: 250,
            block_ids: (9..250).map(hid).collect(),
        })),
        &local,
        &mut out,
    );
    assert_eq!(st.last_response_height, 249);
    let first_batch: Vec<Hash> = match sends(&out)[..] {
        [ProtocolMessage::RequestGetObjects(r)] => r.blocks.clone(),
        _ => panic!("expected RequestGetObjects, got {out:?}"),
    };
    assert_eq!(first_batch.len(), BLOCKS_SYNCHRONIZING_DEFAULT_COUNT);
    assert_eq!(first_batch[0], hid(10));
    assert_eq!(st.needed.len(), 240 - 100);

    // Peer sends the 100 blocks.
    let mut batch = first_batch.clone();
    loop {
        out.clear();
        step(
            &mut st,
            SyncInput::Message(ProtocolMessage::ResponseGetObjects(ResponseGetObjects {
                txs: vec![],
                blocks: batch
                    .iter()
                    .map(|h| RawBlockLegacy {
                        block: block_blob(*h, &[]),
                        txs: vec![],
                    })
                    .collect(),
                missed_ids: vec![],
                current_blockchain_height: 250,
            })),
            &local,
            &mut out,
        );
        let received = out.iter().find_map(|a| match a {
            SyncAction::Event(SyncEvent::BlocksReceived { hashes, .. }) => Some(hashes.clone()),
            _ => None,
        });
        assert_eq!(received.as_ref(), Some(&batch));
        assert!(st.awaiting_batch);

        // Node stored them.
        local.main.extend_from_slice(&batch);
        out.clear();
        step(
            &mut st,
            SyncInput::BlocksProcessed(BatchOutcome::Added),
            &local,
            &mut out,
        );
        match sends(&out)[..] {
            [ProtocolMessage::RequestGetObjects(r)] => batch = r.blocks.clone(),
            [ProtocolMessage::RequestTxPool(_)] => break,
            _ => panic!("unexpected {out:?}"),
        }
    }
    assert_eq!(local.main.len(), 250);
    assert_eq!(st.status, SyncStatus::Normal);
    assert!(
        out.iter()
            .any(|a| matches!(a, SyncAction::Event(SyncEvent::Synchronized)))
    );
}

#[test]
fn unrequested_block_disconnects() {
    let local = chain(10);
    let mut st = PeerSync {
        status: SyncStatus::Synchronizing,
        requested: vec![hid(10)],
        ..PeerSync::default()
    };
    let mut out = Vec::new();
    step(
        &mut st,
        SyncInput::Message(ProtocolMessage::ResponseGetObjects(ResponseGetObjects {
            blocks: vec![RawBlockLegacy {
                block: block_blob(hid(11), &[]),
                txs: vec![],
            }],
            current_blockchain_height: 20,
            ..ResponseGetObjects::default()
        })),
        &local,
        &mut out,
    );
    assert_eq!(st.status, SyncStatus::Shutdown);
    assert!(matches!(out.last(), Some(SyncAction::Disconnect(_))));
}

#[test]
fn lite_block_with_missing_tx() {
    let mut local = chain(10);
    local.pool = vec![hid(1000)];
    let mut st = PeerSync {
        status: SyncStatus::Normal,
        version: 5,
        ..PeerSync::default()
    };
    let template = block_blob(hid(10), &[hid(1000), hid(1001)]);
    let mut out = Vec::new();
    step(
        &mut st,
        SyncInput::Message(ProtocolMessage::NewLiteBlock(NewLiteBlock {
            current_blockchain_height: 11,
            hop: 1,
            block_template: template.clone(),
        })),
        &local,
        &mut out,
    );
    match sends(&out)[..] {
        [ProtocolMessage::MissingTxs(m)] => {
            assert_eq!(m.missing_txs, vec![hid(1001)]);
            assert_eq!(m.block_hash, hid(10));
        }
        _ => panic!("expected MissingTxs, got {out:?}"),
    }
    assert!(st.pending_lite.is_some());

    out.clear();
    step(
        &mut st,
        SyncInput::Message(ProtocolMessage::NewTransactions(NewTransactions {
            txs: vec![tx_blob(hid(1001))],
        })),
        &local,
        &mut out,
    );
    let block = out
        .iter()
        .find_map(|a| match a {
            SyncAction::Event(SyncEvent::NewBlock { block, hop, .. }) => {
                Some((block.clone(), *hop))
            }
            _ => None,
        })
        .expect("lite block completed");
    assert_eq!(block.1, 1);
    assert_eq!(block.0.block, template);
    assert_eq!(
        block.0.transactions,
        vec![tx_blob(hid(1000)), tx_blob(hid(1001))]
    );
    assert!(st.pending_lite.is_none());
}

/// An answer to `NOTIFY_MISSING_TXS` with more transactions than the lite block lists ends the
/// connection, even when the missing one is among them.
#[test]
fn lite_block_answer_with_extra_transactions() {
    let mut local = chain(10);
    local.pool = vec![hid(1000)];
    let mut st = PeerSync {
        status: SyncStatus::Normal,
        version: 5,
        ..PeerSync::default()
    };
    let mut out = Vec::new();
    step(
        &mut st,
        SyncInput::Message(ProtocolMessage::NewLiteBlock(NewLiteBlock {
            current_blockchain_height: 11,
            hop: 1,
            block_template: block_blob(hid(10), &[hid(1000), hid(1001)]),
        })),
        &local,
        &mut out,
    );
    assert!(st.pending_lite.is_some());
    out.clear();
    step(
        &mut st,
        SyncInput::Message(ProtocolMessage::NewTransactions(NewTransactions {
            txs: vec![tx_blob(hid(1001)), tx_blob(hid(2000)), tx_blob(hid(2001))],
        })),
        &local,
        &mut out,
    );
    assert_eq!(st.status, SyncStatus::Shutdown);
    assert!(matches!(out.last(), Some(SyncAction::Disconnect(_))));
    assert!(
        !out.iter()
            .any(|a| matches!(a, SyncAction::Event(SyncEvent::NewBlock { .. })))
    );
}

#[test]
fn serves_request_chain_and_objects() {
    let local = chain(30);
    let mut st = PeerSync {
        status: SyncStatus::Normal,
        ..PeerSync::default()
    };
    let mut out = Vec::new();
    step(
        &mut st,
        SyncInput::Message(ProtocolMessage::RequestChain(RequestChain {
            block_ids: vec![hid(500), hid(12), hid(0)],
        })),
        &local,
        &mut out,
    );
    match sends(&out)[..] {
        [ProtocolMessage::ResponseChainEntry(r)] => {
            assert_eq!(r.start_height, 12);
            assert_eq!(r.total_height, 30);
            assert_eq!(r.block_ids.len(), 18);
        }
        _ => panic!("{out:?}"),
    }

    out.clear();
    step(
        &mut st,
        SyncInput::Message(ProtocolMessage::RequestGetObjects(RequestGetObjects {
            txs: vec![],
            blocks: vec![hid(3), hid(77)],
        })),
        &local,
        &mut out,
    );
    match sends(&out)[..] {
        [ProtocolMessage::ResponseGetObjects(r)] => {
            assert_eq!(r.blocks.len(), 1);
            assert_eq!(r.missed_ids, vec![hid(77)]);
            assert_eq!(r.current_blockchain_height, 30);
        }
        _ => panic!("{out:?}"),
    }

    // A chain request not ending in genesis is a protocol violation.
    out.clear();
    step(
        &mut st,
        SyncInput::Message(ProtocolMessage::RequestChain(RequestChain {
            block_ids: vec![hid(12)],
        })),
        &local,
        &mut out,
    );
    assert_eq!(st.status, SyncStatus::Shutdown);
}

#[test]
fn relay_split_by_version() {
    let raw = RawBlock {
        block: vec![1],
        transactions: vec![vec![2]],
    };
    let (lite, full) = relay_block_messages(&raw, 5, 2);
    assert!(
        matches!(lite, ProtocolMessage::NewLiteBlock(ref l) if l.hop == 2 && l.block_template == vec![1])
    );
    assert!(matches!(full, ProtocolMessage::NewBlock(ref b) if b.block.txs == vec![vec![2]]));
    assert!(wants_lite_blocks(4));
    assert!(!wants_lite_blocks(3));
}

#[test]
fn managed_download_hands_over_to_the_node() {
    let local = chain(10);
    let mut st = PeerSync {
        managed_download: true,
        ..PeerSync::default()
    };
    let mut out = Vec::new();
    step(
        &mut st,
        SyncInput::Handshake {
            core_sync: CoreSyncData {
                current_height: 250,
                top_id: hid(249),
                pruning_seed: 0,
            },
            version: 5,
        },
        &local,
        &mut out,
    );
    // No RequestChain of its own, the node's downloader takes over.
    assert_eq!(sends(&out), Vec::<&ProtocolMessage>::new());
    assert!(out.iter().any(|a| matches!(
        a,
        SyncAction::Event(SyncEvent::SyncRequired { remote_height: 250 })
    )));
    assert_eq!(st.status, SyncStatus::Synchronizing);

    // Chain entries and objects are forwarded untouched.
    out.clear();
    let entry = ResponseChainEntry {
        start_height: 9,
        total_height: 250,
        block_ids: (9..250).map(hid).collect(),
    };
    step(
        &mut st,
        SyncInput::Message(ProtocolMessage::ResponseChainEntry(entry.clone())),
        &local,
        &mut out,
    );
    assert!(matches!(&out[..], [SyncAction::Event(SyncEvent::ChainEntry(e))] if *e == entry));
    out.clear();
    step(
        &mut st,
        SyncInput::Message(ProtocolMessage::ResponseGetObjects(ResponseGetObjects {
            txs: vec![],
            blocks: vec![],
            missed_ids: vec![hid(10)],
            current_blockchain_height: 250,
        })),
        &local,
        &mut out,
    );
    assert!(
        out.iter()
            .any(|a| matches!(a, SyncAction::Event(SyncEvent::Objects(_))))
    );

    // Relayed blocks are ignored while synchronizing.
    out.clear();
    step(
        &mut st,
        SyncInput::Message(ProtocolMessage::NewLiteBlock(NewLiteBlock {
            current_blockchain_height: 251,
            hop: 1,
            block_template: block_blob(hid(250), &[]),
        })),
        &local,
        &mut out,
    );
    assert_eq!(sends(&out), Vec::<&ProtocolMessage>::new());

    // The node finished downloading: normal state, pool request, Synchronized.
    out.clear();
    step(&mut st, SyncInput::DownloadFinished, &local, &mut out);
    assert_eq!(st.status, SyncStatus::Normal);
    assert!(matches!(
        sends(&out)[..],
        [ProtocolMessage::RequestTxPool(_)]
    ));
    assert!(
        out.iter()
            .any(|a| matches!(a, SyncAction::Event(SyncEvent::Synchronized)))
    );

    // An orphan relayed block hands over to the downloader again.
    out.clear();
    step(
        &mut st,
        SyncInput::BlockProcessed(BlockOutcome::Rejected),
        &local,
        &mut out,
    );
    assert_eq!(st.status, SyncStatus::Synchronizing);
    assert_eq!(sends(&out), Vec::<&ProtocolMessage>::new());
    assert!(
        out.iter()
            .any(|a| matches!(a, SyncAction::Event(SyncEvent::SyncRequired { .. })))
    );
}

/// The template's hash list and the provided blobs are both peer-controlled: matching them must be
/// linear. Nested list scans made this ~10^9 comparisons on the P2P task before. A template may
/// list at most `MAX_MISSING_TXS` hashes, so that is the size tested.
#[test]
fn huge_lite_block_is_matched_in_linear_time() {
    const N: u32 = MAX_MISSING_TXS as u32;
    let local = chain(10);
    let mut st = PeerSync {
        status: SyncStatus::Normal,
        version: 5,
        ..PeerSync::default()
    };
    let txs: Vec<Hash> = (0..N).map(|i| hid(1_000_000 + i)).collect();
    let started = std::time::Instant::now();
    let mut out = Vec::new();
    step(
        &mut st,
        SyncInput::Message(ProtocolMessage::NewLiteBlock(NewLiteBlock {
            current_blockchain_height: 11,
            hop: 1,
            block_template: block_blob(hid(10), &txs),
        })),
        &local,
        &mut out,
    );
    assert!(st.pending_lite.is_some());
    out.clear();
    // reverse order: the worst case for a front-to-back scan
    step(
        &mut st,
        SyncInput::Message(ProtocolMessage::NewTransactions(NewTransactions {
            txs: txs.iter().rev().map(|h| tx_blob(*h)).collect(),
        })),
        &local,
        &mut out,
    );
    let block = out
        .iter()
        .find_map(|a| match a {
            SyncAction::Event(SyncEvent::NewBlock { block, .. }) => Some(block),
            _ => None,
        })
        .expect("lite block completed");
    assert_eq!(block.transactions.len(), N as usize);
    assert_eq!(block.transactions[0], tx_blob(txs[0]));
    let took = started.elapsed();
    assert!(took < std::time::Duration::from_secs(5), "{took:?}");
}

fn normal() -> PeerSync {
    PeerSync {
        status: SyncStatus::Normal,
        version: 5,
        ..PeerSync::default()
    }
}

/// A chain request longer than any sparse chain is a protocol violation.
#[test]
fn overlong_request_chain_is_a_violation() {
    let local = chain(30);
    let mut st = normal();
    let mut out = Vec::new();
    let mut ids: Vec<Hash> = (0..MAX_REQUEST_CHAIN_IDS as u32)
        .map(|i| hid(5000 + i))
        .collect();
    ids.push(hid(0));
    step(
        &mut st,
        SyncInput::Message(ProtocolMessage::RequestChain(RequestChain {
            block_ids: ids,
        })),
        &local,
        &mut out,
    );
    assert_eq!(st.status, SyncStatus::Shutdown);
    assert_eq!(sends(&out).len(), 0);
}

/// At most `BLOCKS_SYNCHRONIZING_DEFAULT_COUNT` blocks are served per request.
#[test]
fn get_objects_serves_at_most_one_batch() {
    let local = chain(300);
    let req = RequestGetObjects {
        txs: vec![],
        blocks: (0..250).map(hid).collect(),
    };
    let resp = serve_get_objects(&local, &req);
    assert_eq!(resp.blocks.len(), BLOCKS_SYNCHRONIZING_DEFAULT_COUNT);
    assert_eq!(resp.missed_ids.len(), 0);
    assert_eq!(resp.blocks[0].block, block_blob(hid(0), &[]));
}

/// Stripe pruning: blocks the node does not hold in full are reported in `missed_ids` and
/// never read; the held ones are served in request order.
#[test]
fn get_objects_reports_blocks_not_held_as_missed() {
    let local = chain(20);
    let req = RequestGetObjects {
        txs: vec![],
        blocks: vec![hid(2), hid(3), hid(4), hid(99), hid(5)],
    };
    // Only even heights are held in full.
    let resp = serve_get_objects_held(&local, &req, |h| {
        local.main_chain_index(h).is_some_and(|i| i % 2 == 0)
    });
    let served: Vec<Vec<u8>> = resp.blocks.iter().map(|b| b.block.clone()).collect();
    assert_eq!(
        served,
        vec![block_blob(hid(2), &[]), block_blob(hid(4), &[])]
    );
    let mut missed = resp.missed_ids.clone();
    missed.sort_by_key(|h| h.0);
    let mut expected = vec![hid(3), hid(5), hid(99)];
    expected.sort_by_key(|h| h.0);
    assert_eq!(missed, expected);
    assert_eq!(resp.current_blockchain_height, 20);

    // Everything held: the same answer as `serve_get_objects`.
    assert_eq!(
        serve_get_objects_held(&local, &req, |_| true),
        serve_get_objects(&local, &req)
    );
}

/// A pruned node asked for more than 100 ids looks at the first 100 only: `held` is asked
/// for those, `missed_ids` never grows past 100 (not held, unknown and held-but-missing
/// together), and the ids beyond are neither served nor reported.
#[test]
fn get_objects_held_looks_at_most_at_one_batch() {
    let local = chain(300);
    let max = BLOCKS_SYNCHRONIZING_DEFAULT_COUNT;
    let asked = std::cell::Cell::new(0_usize);
    // 250 ids: known heights 0..200, then 50 unknown ones.
    let req = RequestGetObjects {
        txs: vec![],
        blocks: (0..200).chain(5_000..5_050).map(hid).collect(),
    };
    // Nothing held: every looked-at id is missed, at most 100 of them.
    let resp = serve_get_objects_held(&local, &req, |_| {
        asked.set(asked.get() + 1);
        false
    });
    assert_eq!(asked.get(), max);
    assert_eq!(resp.blocks.len(), 0);
    assert_eq!(resp.missed_ids, (0..100).map(hid).collect::<Vec<_>>());

    // Every third id held: the first 100 ids split between served and missed.
    asked.set(0);
    let resp = serve_get_objects_held(&local, &req, |h| {
        asked.set(asked.get() + 1);
        local
            .main_chain_index(h)
            .is_some_and(|i| i.is_multiple_of(3))
    });
    assert_eq!(asked.get(), max);
    assert_eq!(resp.blocks.len() + resp.missed_ids.len(), max);
    assert_eq!(resp.blocks.len(), 34);
    assert!(resp.missed_ids.len() <= max);

    // Unknown ids up front, everything "held": the lookups report them missed, still at most
    // 100 in all, and the known ids behind them are not reached.
    let req = RequestGetObjects {
        txs: vec![],
        blocks: (5_000..5_150).chain(0..100).map(hid).collect(),
    };
    let resp = serve_get_objects_held(&local, &req, |_| true);
    assert_eq!(resp.blocks.len(), 0);
    assert_eq!(resp.missed_ids, (5_000..5_100).map(hid).collect::<Vec<_>>());
    assert_eq!(resp.current_blockchain_height, 300);
}

/// `NOTIFY_MISSING_TXS` answers duplicates once, refuses unknown transactions and too many
/// hashes (both shut the connection down, like C++ for an unknown one).
#[test]
fn missing_txs_are_bounded() {
    let mut local = chain(10);
    local.pool = vec![hid(1000), hid(1001)];
    let req = |hashes: Vec<Hash>| MissingTxs {
        current_blockchain_height: 10,
        block_hash: hid(10),
        missing_txs: hashes,
    };
    let ok = serve_missing_txs(&local, &req(vec![hid(1000), hid(1001), hid(1000)])).unwrap();
    assert_eq!(ok.txs, vec![tx_blob(hid(1000)), tx_blob(hid(1001))]);
    assert!(serve_missing_txs(&local, &req(vec![hid(1002)])).is_err());
    let many = vec![hid(1000); MAX_MISSING_TXS + 1];
    assert!(serve_missing_txs(&local, &req(many.clone())).is_err());
    let mut st = normal();
    let mut out = Vec::new();
    step(
        &mut st,
        SyncInput::Message(ProtocolMessage::MissingTxs(req(many))),
        &local,
        &mut out,
    );
    assert_eq!(st.status, SyncStatus::Shutdown);
}

/// Relayed blocks we already have cost a lookup only: no event, no `NOTIFY_MISSING_TXS`.
#[test]
fn known_relayed_blocks_are_ignored() {
    let local = chain(10);
    let mut st = normal();
    let mut out = Vec::new();
    step(
        &mut st,
        SyncInput::Message(ProtocolMessage::NewLiteBlock(NewLiteBlock {
            current_blockchain_height: 10,
            hop: 1,
            block_template: block_blob(hid(9), &[hid(5000)]),
        })),
        &local,
        &mut out,
    );
    step(
        &mut st,
        SyncInput::Message(ProtocolMessage::NewBlock(NewBlock {
            block: RawBlockLegacy {
                block: block_blob(hid(8), &[]),
                txs: vec![],
            },
            current_blockchain_height: 10,
            hop: 1,
        })),
        &local,
        &mut out,
    );
    assert!(sends(&out).is_empty(), "{out:?}");
    assert!(
        !out.iter()
            .any(|a| matches!(a, SyncAction::Event(SyncEvent::NewBlock { .. }))),
        "{out:?}"
    );
    assert!(st.pending_lite.is_none());
    assert_eq!(st.status, SyncStatus::Normal);
    // A template listing more transactions than any block can is a violation.
    let txs: Vec<Hash> = (0..=MAX_MISSING_TXS as u32)
        .map(|i| hid(100_000 + i))
        .collect();
    step(
        &mut st,
        SyncInput::Message(ProtocolMessage::NewLiteBlock(NewLiteBlock {
            current_blockchain_height: 11,
            hop: 1,
            block_template: block_blob(hid(10), &txs),
        })),
        &local,
        &mut out,
    );
    assert_eq!(st.status, SyncStatus::Shutdown);
}

/// The pool answer is capped in bytes.
#[test]
fn pool_answer_is_capped() {
    let mut local = chain(10);
    local.pool = (0..5).map(|i| hid(2000 + i)).collect();
    let blob_len = tx_blob(hid(2000)).len();
    assert_eq!(local.pool_txs_not_in_capped(&[], 2 * blob_len + 1).len(), 2);
    assert_eq!(
        local.pool_txs_not_in_capped(&[hid(2000)], usize::MAX).len(),
        4
    );
}
