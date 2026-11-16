//! Writing shares and the hashes derived from them (`src/pool_block.cpp`:
//! `xkr_serialize_real_coinbase`, `xkr_aux_header_hash`, `xkr_serialize_parent_coinbase`,
//! `xkr_serialize_pow_blob`, `serialize_mainchain_data`, `serialize_sidechain_data`,
//! `build_pow_blob`, `calculate_tx_key_seed`; the id with zeroed regions of
//! `BlockTemplate::calc_sidechain_hash`, `src/block_template.cpp`).
//!
//! The main-chain part always gets its derived fields recomputed: the unlock time
//! (`txin_gen_height + unlock window`), the parent coinbase sizes and the aux hash. A share
//! received from a peer is identified by the bytes it arrived with ([`crate::parse`]), never by
//! a re-serialization.

use xkr_crypto::{cn_fast_hash, cn_turtle_lite_slow_hash_v2, tree_hash};
use xkr_types::Hash;

use crate::block::{
    EXTRA_NONCE_SIZE, NONCE_SIZE, PARENT_MM_TAG_DEPTH, PoolBlock, TX_EXTRA_MERGE_MINING_TAG,
    TX_EXTRA_NONCE, TX_EXTRA_TAG_PUBKEY, TXIN_GEN, TXOUT_TO_KEY, TXOUT_TO_TAGGED_KEY,
};
use crate::forks::{COINBASE_TX_VERSION_2, coinbase_tx_version};
use crate::varint::{varint_len, write_varint};

/// Domain of [`PoolBlock::calculate_tx_key_seed`]: 12 bytes, the terminator included.
const TX_KEY_SEED_DOMAIN: &[u8; 12] = b"tx_key_seed\0";

/// Where the regions excluded from the share id start, as byte offsets into a full blob
/// (main-chain part followed by the sidechain part).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IdRegions {
    /// The 4 nonce bytes of the parent block header.
    pub nonce: usize,
    /// The 32-byte aux hash in the parent coinbase.
    pub aux_hash: usize,
    /// The 4-byte coinbase extra nonce.
    pub extra_nonce: usize,
    /// The 32-byte sidechain merkle root in the coinbase.
    pub merkle_root: usize,
    /// `sidechainExtraBuf[3]`, the last 4 bytes of the sidechain part.
    pub sidechain_extra_nonce: usize,
}

/// The main-chain part of a share with the offsets of its mining regions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MainchainData {
    /// The bytes (a complete Kryptokrona block).
    pub bytes: Vec<u8>,
    /// Offset of the nonce.
    pub nonce_offset: usize,
    /// Offset of the aux hash in the parent coinbase.
    pub aux_hash_offset: usize,
    /// Offset of the coinbase extra nonce.
    pub extra_nonce_offset: usize,
    /// Offset of the coinbase sidechain merkle root.
    pub merkle_root_offset: usize,
}

/// The proof-of-work input of a share (`build_pow_blob`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PowBlob {
    /// The parent block hashing blob that CryptoNight-Turtle hashes.
    pub blob: Vec<u8>,
    /// Offset of the 4 nonce bytes in [`Self::blob`].
    pub nonce_offset: usize,
    /// Hash of the real coinbase (`m_transactions[0]`).
    pub coinbase_hash: Hash,
}

/// `keccak(data)` (`cn_fast_hash`).
fn keccak(data: &[u8]) -> Hash {
    cn_fast_hash(data)
}

/// Copies the 32 bytes of a key or hash.
fn put32(out: &mut Vec<u8>, bytes: &[u8; 32]) {
    out.extend_from_slice(bytes);
}

/// Keccak of `blob` with the id regions zeroed, followed by `consensus_id`.
#[must_use]
pub fn id_hash(blob: &[u8], regions: &IdRegions, consensus_id: &Hash) -> Hash {
    let mut buf = Vec::with_capacity(blob.len() + 32);
    buf.extend_from_slice(blob);
    for (start, len) in [
        (regions.nonce, NONCE_SIZE),
        (regions.aux_hash, 32),
        (regions.extra_nonce, EXTRA_NONCE_SIZE),
        (regions.merkle_root, 32),
        (regions.sidechain_extra_nonce, 4),
    ] {
        let end = (start + len).min(buf.len());
        if let Some(region) = buf.get_mut(start.min(end)..end) {
            region.fill(0);
        }
    }
    buf.extend_from_slice(&consensus_id.0);
    keccak(&buf)
}

impl PoolBlock {
    /// `xkr_serialize_real_coinbase`: the PPLNS payout transaction. Version 1 with `0x02`
    /// outputs below major version 7, version 2 with `0x03` outputs and view tags from 7 on; the
    /// extra is `01 R 02 24 extra_nonce(4) merkle_root(32)`; no `RingCT` byte.
    #[must_use]
    pub fn real_coinbase(&self, unlock_window: u64, extra_nonce: u32) -> Vec<u8> {
        let version = coinbase_tx_version(self.major_version);
        let tagged = version >= COINBASE_TX_VERSION_2;
        let mut out = Vec::with_capacity(16 + self.outputs.len() * 44 + 72);
        write_varint(&mut out, u64::from(version));
        write_varint(&mut out, self.txin_gen_height.wrapping_add(unlock_window));
        out.push(1);
        out.push(TXIN_GEN);
        write_varint(&mut out, self.txin_gen_height);
        write_varint(&mut out, self.outputs.len() as u64);
        for (o, key) in self.outputs.iter().zip(&self.eph_public_keys) {
            write_varint(&mut out, o.reward);
            out.push(if tagged {
                TXOUT_TO_TAGGED_KEY
            } else {
                TXOUT_TO_KEY
            });
            put32(&mut out, &key.0);
            if tagged {
                out.push(o.view_tag);
            }
        }
        let mut extra = Vec::with_capacity(71);
        extra.push(TX_EXTRA_TAG_PUBKEY);
        put32(&mut extra, &self.txkey_pub.0);
        extra.push(TX_EXTRA_NONCE);
        write_varint(&mut extra, (EXTRA_NONCE_SIZE + 32) as u64);
        extra.extend_from_slice(&extra_nonce.to_le_bytes());
        put32(&mut extra, &self.merkle_root.0);
        write_varint(&mut out, extra.len() as u64);
        out.extend_from_slice(&extra);
        out
    }

    /// `xkr_aux_header_hash`: `keccak(varint(len) || B)` with `B = varint(major) ||
    /// varint(minor) || prev_id || tree_hash([coinbase hash, tx…]) || varint(tx count + 1)`
    /// (the daemon's `getAuxiliaryBlockHeaderHash`). The coinbase slot of
    /// [`Self::transactions`] is replaced by `coinbase_hash`.
    #[must_use]
    pub fn aux_hash(&self, coinbase_hash: &Hash) -> Hash {
        let mut hashes = Vec::with_capacity(self.transactions.len().max(1));
        hashes.push(*coinbase_hash);
        hashes.extend(self.transactions.iter().skip(1).copied());
        let root = tree_hash(&hashes);
        let mut b = Vec::with_capacity(48);
        write_varint(&mut b, u64::from(self.major_version));
        write_varint(&mut b, u64::from(self.minor_version));
        put32(&mut b, &self.prev_id.0);
        put32(&mut b, &root.0);
        write_varint(&mut b, hashes.len() as u64);
        let mut obj = Vec::with_capacity(b.len() + 2);
        write_varint(&mut obj, b.len() as u64);
        obj.extend_from_slice(&b);
        keccak(&obj)
    }

    /// `xkr_serialize_parent_coinbase`: `00 00 00 00 | varint 35 | 03 | varint 33 | 00 |
    /// aux_hash`.
    #[must_use]
    pub fn parent_coinbase(aux_hash: &Hash) -> Vec<u8> {
        let mut mm = Vec::with_capacity(33);
        write_varint(&mut mm, PARENT_MM_TAG_DEPTH);
        put32(&mut mm, &aux_hash.0);
        let mut extra = Vec::with_capacity(35);
        extra.push(TX_EXTRA_MERGE_MINING_TAG);
        write_varint(&mut extra, mm.len() as u64);
        extra.extend_from_slice(&mm);
        let mut out = vec![0, 0, 0, 0];
        write_varint(&mut out, extra.len() as u64);
        out.extend_from_slice(&extra);
        out
    }

    /// `build_pow_blob(extra_nonce, nonce)`: `varint 0 | varint 0 | varint timestamp | 32 zero
    /// bytes | nonce | keccak(parent coinbase) | varint 1`.
    #[must_use]
    pub fn pow_blob(&self, unlock_window: u64, extra_nonce: u32, nonce: u32) -> PowBlob {
        let coinbase = self.real_coinbase(unlock_window, extra_nonce);
        let coinbase_hash = keccak(&coinbase);
        let aux = self.aux_hash(&coinbase_hash);
        let parent_hash = keccak(&Self::parent_coinbase(&aux));
        let mut blob = Vec::with_capacity(80);
        write_varint(&mut blob, 0);
        write_varint(&mut blob, 0);
        write_varint(&mut blob, self.timestamp);
        blob.extend_from_slice(&[0u8; 32]);
        let nonce_offset = blob.len();
        blob.extend_from_slice(&nonce.to_le_bytes());
        put32(&mut blob, &parent_hash.0);
        write_varint(&mut blob, 1);
        debug_assert_eq!(nonce_offset, 2 + varint_len(self.timestamp) + 32);
        PowBlob {
            blob,
            nonce_offset,
            coinbase_hash,
        }
    }

    /// The share's proof-of-work hash: CryptoNight-Turtle-Lite v2 of [`Self::pow_blob`] with
    /// its own nonce and extra nonce (`get_pow_hash`).
    #[must_use]
    pub fn pow_hash(&self, unlock_window: u64) -> Hash {
        cn_turtle_lite_slow_hash_v2(
            &self
                .pow_blob(unlock_window, self.extra_nonce, self.nonce)
                .blob,
        )
    }

    /// `serialize_mainchain_data(…, nonce, extra_nonce)`: the main-chain part with the given
    /// nonce and extra nonce.
    #[must_use]
    pub fn serialize_mainchain_data(
        &self,
        unlock_window: u64,
        nonce: u32,
        extra_nonce: u32,
    ) -> MainchainData {
        let coinbase = self.real_coinbase(unlock_window, extra_nonce);
        let aux = self.aux_hash(&keccak(&coinbase));
        let parent = Self::parent_coinbase(&aux);
        let mut data = Vec::with_capacity(128 + coinbase.len() + self.transactions.len() * 32);
        write_varint(&mut data, u64::from(self.major_version));
        write_varint(&mut data, u64::from(self.minor_version));
        put32(&mut data, &self.prev_id.0);
        write_varint(&mut data, 0);
        write_varint(&mut data, 0);
        write_varint(&mut data, self.timestamp);
        data.extend_from_slice(&[0u8; 32]);
        let nonce_offset = data.len();
        data.extend_from_slice(&nonce.to_le_bytes());
        write_varint(&mut data, 1);
        data.extend_from_slice(&parent);
        let aux_hash_offset = data.len() - 32;
        data.extend_from_slice(&coinbase);
        let merkle_root_offset = data.len() - 32;
        let extra_nonce_offset = merkle_root_offset - EXTRA_NONCE_SIZE;
        write_varint(&mut data, self.transactions.len().saturating_sub(1) as u64);
        for h in self.transactions.iter().skip(1) {
            put32(&mut data, &h.0);
        }
        MainchainData {
            bytes: data,
            nonce_offset,
            aux_hash_offset,
            extra_nonce_offset,
            merkle_root_offset,
        }
    }

    /// `serialize_sidechain_data()`.
    #[must_use]
    pub fn serialize_sidechain_data(&self) -> Vec<u8> {
        let mut data = Vec::with_capacity((self.uncles.len() + 4) * 32 + 64);
        put32(&mut data, &self.miner_wallet.spend.0);
        put32(&mut data, &self.miner_wallet.view.0);
        put32(&mut data, &self.txkey_sec_seed.0);
        put32(&mut data, &self.parent.0);
        write_varint(&mut data, self.uncles.len() as u64);
        for u in &self.uncles {
            put32(&mut data, &u.0);
        }
        write_varint(&mut data, self.sidechain_height);
        write_varint(&mut data, self.difficulty.lo());
        write_varint(&mut data, self.difficulty.hi());
        write_varint(&mut data, self.cumulative_difficulty.lo());
        write_varint(&mut data, self.cumulative_difficulty.hi());
        data.push(self.merkle_proof.len() as u8);
        for h in &self.merkle_proof {
            put32(&mut data, &h.0);
        }
        write_varint(&mut data, self.merge_mining_extra.len() as u64);
        for (id, extra) in &self.merge_mining_extra {
            put32(&mut data, &id.0);
            write_varint(&mut data, extra.len() as u64);
            data.extend_from_slice(extra);
        }
        for w in self.sidechain_extra {
            data.extend_from_slice(&w.to_le_bytes());
        }
        data
    }

    /// The full blob (main-chain part with the share's own nonce and extra nonce, then the
    /// sidechain part): what a block cache slot and a `BLOCK_RESPONSE` carry.
    #[must_use]
    pub fn serialize(&self, unlock_window: u64) -> Vec<u8> {
        let mut out = self
            .serialize_mainchain_data(unlock_window, self.nonce, self.extra_nonce)
            .bytes;
        out.extend_from_slice(&self.serialize_sidechain_data());
        out
    }

    /// The share id of this block as a template would compute it
    /// (`BlockTemplate::calc_sidechain_hash`): keccak of the serialization with the id regions
    /// zeroed, followed by the consensus id.
    #[must_use]
    pub fn calculate_sidechain_id(&self, unlock_window: u64, consensus_id: &Hash) -> Hash {
        let main = self.serialize_mainchain_data(unlock_window, self.nonce, self.extra_nonce);
        let mut blob = main.bytes;
        blob.extend_from_slice(&self.serialize_sidechain_data());
        let regions = IdRegions {
            nonce: main.nonce_offset,
            aux_hash: main.aux_hash_offset,
            extra_nonce: main.extra_nonce_offset,
            merkle_root: main.merkle_root_offset,
            sidechain_extra_nonce: blob.len() - 4,
        };
        id_hash(&blob, &regions, consensus_id)
    }

    /// `calculate_tx_key_seed()`: `keccak("tx_key_seed\0" || main-chain part with nonce and
    /// extra nonce 0 || sidechain part)`. The seed of a share whose main-chain block differs
    /// from its parent's.
    #[must_use]
    pub fn calculate_tx_key_seed(&self, unlock_window: u64) -> Hash {
        let main = self.serialize_mainchain_data(unlock_window, 0, 0).bytes;
        let side = self.serialize_sidechain_data();
        let mut buf = Vec::with_capacity(TX_KEY_SEED_DOMAIN.len() + main.len() + side.len());
        buf.extend_from_slice(TX_KEY_SEED_DOMAIN);
        buf.extend_from_slice(&main);
        buf.extend_from_slice(&side);
        keccak(&buf)
    }
}
