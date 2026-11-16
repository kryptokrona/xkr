//! The template next to the node: the stratum server's [`TemplateSource`] and [`ShareSink`]
//! (`p2pool::submit_block_async`, `p2pool::submit_sidechain_block`,
//! `BlockTemplate::submit_sidechain_block`, `src/p2pool.cpp`, `src/block_template.cpp`).
//!
//! [`PoolTemplates`] keeps the [`BlockTemplate`] behind a lock and reaches the sidechain, the
//! main-chain view, the P2P side and the daemon through [`PoolNode`]. A found share goes into
//! the sidechain as an own, verified share and the sidechain's events (new tip, broadcast,
//! cache) go back to the node; a found main-chain block goes to the node as a
//! [`FoundBlock`].

use core::fmt;
use std::sync::{Arc, PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard};

use xkr_types::Hash;

use super::{BlockTemplate, Template, TemplateError, TemplateMinerData, make_template, own_share};
use crate::difficulty::Difficulty;
use crate::mainchain::MainChainView;
use crate::sidechain::{AddError, SideChain, SidechainEvent};
use crate::stratum::server::now_secs;
use crate::stratum::{HashingBlob, ShareSink, TemplateDifficulties, TemplateSource};
use crate::wallet::MinerWallet;

/// What the template needs from the node it runs in.
pub trait PoolNode: Send + Sync {
    /// Runs `f` with the sidechain (read access) and the main-chain view.
    fn read_sidechain(&self, f: &mut dyn FnMut(&SideChain, &dyn MainChainView));

    /// Runs `f` with the sidechain (write access) and the main-chain view.
    fn write_sidechain(&self, f: &mut dyn FnMut(&mut SideChain, &dyn MainChainView));

    /// The sidechain's events after an own share was added (new tip: build a new template;
    /// broadcast: send the share to the peers; store in the block cache).
    fn on_sidechain_events(&self, events: Vec<SidechainEvent>);

    /// A main-chain block was found: submit it to the daemon. Must not block.
    fn submit_mainchain_block(&self, block: FoundBlock);

    /// The P2P side had peers and has none now (stratum logins are refused).
    fn disconnected(&self) -> bool {
        false
    }
}

/// A found main-chain block (`p2pool::submit_block`).
#[derive(Clone, PartialEq, Eq)]
pub struct FoundBlock {
    /// The template it was mined on.
    pub template_id: u32,
    /// Main-chain height.
    pub height: u64,
    /// The block it builds on.
    pub prev_id: Hash,
    /// Main-chain difficulty of the template.
    pub difficulty: Difficulty,
    /// Id of the share inside it (its coinbase merkle root).
    pub sidechain_id: Hash,
    /// Nonce.
    pub nonce: u32,
    /// Extra nonce.
    pub extra_nonce: u32,
    /// The block blob (the main-chain part of the share).
    pub blob: Vec<u8>,
}

impl fmt::Debug for FoundBlock {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FoundBlock")
            .field("template_id", &self.template_id)
            .field("height", &self.height)
            .field("prev_id", &self.prev_id)
            .field("difficulty", &self.difficulty)
            .field("sidechain_id", &self.sidechain_id)
            .field("nonce", &self.nonce)
            .field("extra_nonce", &self.extra_nonce)
            .field("blob_len", &self.blob.len())
            .finish_non_exhaustive()
    }
}

/// Why a found share did not go into the sidechain.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SubmitError {
    /// The template is gone (more than [`super::OLD_TEMPLATES`] updates ago).
    #[error("template {0} is not kept any more")]
    UnknownTemplate(u32),
    /// The same share (id, nonce, extra nonce) was submitted before.
    #[error("share already submitted")]
    AlreadySubmitted,
    /// The sidechain refused it.
    #[error(transparent)]
    Add(#[from] AddError),
}

/// The block template of a node, as the stratum server uses it.
pub struct PoolTemplates {
    node: Arc<dyn PoolNode>,
    templates: RwLock<BlockTemplate>,
}

impl fmt::Debug for PoolTemplates {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let current = self.current().map(|t| t.id);
        f.debug_struct("PoolTemplates")
            .field("current", &current)
            .finish_non_exhaustive()
    }
}

impl PoolTemplates {
    /// No template yet.
    #[must_use]
    pub fn new(node: Arc<dyn PoolNode>) -> Self {
        Self {
            node,
            templates: RwLock::new(BlockTemplate::new()),
        }
    }

    /// The current template.
    #[must_use]
    pub fn current(&self) -> Option<Arc<Template>> {
        self.read().current().cloned()
    }

    /// The current or a kept template by id.
    #[must_use]
    pub fn template(&self, id: u32) -> Option<Arc<Template>> {
        self.read().get(id).cloned()
    }

    /// When the current template was built, 0 without one.
    #[must_use]
    pub fn last_updated(&self) -> u64 {
        self.read().last_updated()
    }

    /// C++ `p2pool::update_block_template`: a new template for `wallet` from the node's miner
    /// data and the sidechain tip, built at time `now`. On failure the current template
    /// stays.
    pub fn update(
        &self,
        data: &TemplateMinerData,
        wallet: &MinerWallet,
        now: u64,
    ) -> Result<Arc<Template>, TemplateError> {
        // Built while the sidechain is read-locked, stored after it is released.
        let mut built = Err(TemplateError::Unavailable);
        let mut rng = xkr_crypto::os_rng();
        self.node.read_sidechain(&mut |sc, main| {
            built = make_template(data, sc, main, wallet, now, &mut rng);
        });
        Ok(self.write().insert(built?))
    }

    /// C++ `BlockTemplate::submit_sidechain_block` at time `now`: the share of `template_id`
    /// with this nonce and extra nonce goes into the sidechain as a verified own share, unless
    /// it was submitted before; the sidechain's events go to the node. Returns the share id.
    pub fn submit_share_at(
        &self,
        template_id: u32,
        nonce: u32,
        extra_nonce: u32,
        now: u64,
    ) -> Result<Hash, SubmitError> {
        let template = self
            .template(template_id)
            .ok_or(SubmitError::UnknownTemplate(template_id))?;
        let mut share = Some(own_share(&template, nonce, extra_nonce));
        let id = template.block.sidechain_id;
        let mut result = Err(SubmitError::AlreadySubmitted);
        let mut events = Vec::new();
        self.node.write_sidechain(&mut |sc, main| {
            let Some(block) = share.take() else {
                return;
            };
            if sc.incoming_block_seen(&block, now) {
                return;
            }
            result = sc
                .add_block(block, main, now)
                .map(|()| id)
                .map_err(SubmitError::from);
            events = sc.take_events();
        });
        if !events.is_empty() {
            self.node.on_sidechain_events(events);
        }
        result
    }

    fn read(&self) -> RwLockReadGuard<'_, BlockTemplate> {
        self.templates
            .read()
            .unwrap_or_else(PoisonError::into_inner)
    }

    fn write(&self) -> RwLockWriteGuard<'_, BlockTemplate> {
        self.templates
            .write()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// One reading of the sidechain.
    fn with_sidechain<T: Default>(&self, f: impl Fn(&SideChain) -> T) -> T {
        let mut out = T::default();
        self.node.read_sidechain(&mut |sc, _| out = f(sc));
        out
    }
}

impl TemplateSource for PoolTemplates {
    fn hashing_blob(&self, extra_nonce: u32) -> Option<HashingBlob> {
        self.current().map(|t| t.hashing_blob(extra_nonce))
    }

    /// All blobs from one template (`get_hashing_blobs`).
    fn hashing_blobs(&self, extra_nonce_start: u32, count: u32) -> Vec<HashingBlob> {
        let Some(t) = self.current() else {
            return Vec::new();
        };
        (0..count)
            .map(|i| t.hashing_blob(extra_nonce_start.wrapping_add(i)))
            .collect()
    }

    fn hashing_blob_for(&self, template_id: u32, extra_nonce: u32) -> Option<HashingBlob> {
        self.template(template_id)
            .map(|t| t.hashing_blob(extra_nonce))
    }

    fn difficulties(&self, template_id: u32) -> Option<TemplateDifficulties> {
        self.template(template_id).map(|t| t.difficulties())
    }

    fn sidechain_difficulty(&self) -> Difficulty {
        self.with_sidechain(SideChain::difficulty)
    }

    fn sidechain_tip_height(&self) -> Option<u64> {
        self.with_sidechain(|sc| sc.chain_tip().map(|t| t.sidechain_height))
    }

    fn chain_window_size(&self) -> u64 {
        self.with_sidechain(|sc| sc.config().pplns_window)
    }

    fn disconnected(&self) -> bool {
        self.node.disconnected()
    }
}

impl ShareSink for PoolTemplates {
    /// `p2pool::submit_block_async` + `submit_block`: the main-chain block of the template
    /// with this nonce and extra nonce goes to the node; nothing happens for a template that
    /// is gone.
    fn submit_block(&self, template_id: u32, nonce: u32, extra_nonce: u32) {
        if let Some(t) = self.template(template_id) {
            self.node
                .submit_mainchain_block(t.found_block(nonce, extra_nonce));
        }
    }

    /// `p2pool::submit_sidechain_block` at the current time.
    fn submit_share(&self, template_id: u32, nonce: u32, extra_nonce: u32) -> bool {
        self.submit_share_at(template_id, nonce, extra_nonce, now_secs())
            .is_ok()
    }
}
