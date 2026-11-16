//! Share depths (`SideChain::update_depths`, `src/side_chain.cpp`).
//!
//! The depth of a share is how many shares are built on top of it: a child adds 1, a share
//! naming it as an uncle `i` heights up adds `i`. A new share takes its depth from the stored
//! shares above it, then pushes depths down to its parent and uncles and up to its children,
//! until nothing changes. A share deep enough that nothing in a window can depend on it any
//! more (or at height 0) is verified on the way.

use xkr_types::Hash;

use super::SideChain;
use crate::config::UNCLE_BLOCK_DEPTH;
use crate::mainchain::MainChainView;

impl SideChain {
    /// C++ `update_depths(block)`.
    pub(super) fn update_depths(&mut self, start: Hash, main: &dyn MainChainView) {
        let Some(b) = self.blocks.get(&start) else {
            return;
        };
        let height = b.sidechain_height;
        let mut depth = b.offchain.depth;
        for i in 1..=UNCLE_BLOCK_DEPTH {
            for child_id in self.by_height.get(&(height + i)).into_iter().flatten() {
                let Some(c) = self.blocks.get(child_id) else {
                    continue;
                };
                if i == 1 && c.parent == start {
                    depth = depth.max(c.offchain.depth + 1);
                }
                if c.uncles.contains(&start) {
                    depth = depth.max(c.offchain.depth + i);
                }
            }
        }
        if let Some(b) = self.blocks.get_mut(&start) {
            b.offchain.depth = depth;
        }

        let verify_depth = self.options.config.unchecked_depth() + UNCLE_BLOCK_DEPTH;
        let mut stack = vec![start];
        while let Some(id) = stack.pop() {
            let Some(b) = self.blocks.get(&id) else {
                continue;
            };
            let height = b.sidechain_height;
            let depth = b.offchain.depth;
            let parent = b.parent;
            let uncles = b.uncles.clone();
            if !b.offchain.verified && (depth > verify_depth || height == 0) {
                self.verify_loop(id, main);
            }

            for i in 1..=UNCLE_BLOCK_DEPTH {
                for child_id in self.ids_at(height + i) {
                    let Some(c) = self.blocks.get_mut(&child_id) else {
                        continue;
                    };
                    let old = c.offchain.depth;
                    if i == 1 && c.parent == id && depth > 0 {
                        c.offchain.depth = c.offchain.depth.max(depth - 1);
                    }
                    if depth > i && c.uncles.contains(&id) {
                        c.offchain.depth = c.offchain.depth.max(depth - i);
                    }
                    if c.offchain.depth > old {
                        stack.push(child_id);
                    }
                }
            }

            if let Some(p) = self.blocks.get_mut(&parent)
                && p.sidechain_height + 1 == height
                && p.offchain.depth < depth + 1
            {
                p.offchain.depth = depth + 1;
                stack.push(parent);
            }

            for uncle_id in uncles {
                let Some(u) = self.blocks.get_mut(&uncle_id) else {
                    continue;
                };
                if u.sidechain_height >= height || u.sidechain_height + UNCLE_BLOCK_DEPTH < height {
                    continue;
                }
                let d = height - u.sidechain_height;
                if u.offchain.depth < depth + d {
                    u.offchain.depth = depth + d;
                    stack.push(uncle_id);
                }
            }
        }
    }
}
