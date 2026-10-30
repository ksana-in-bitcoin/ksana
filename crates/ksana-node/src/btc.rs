//! The node's BTC state: the tracker, snapshots, the settled prefix and the execution-layer view live in
//! `ksana-btc::tracker` (shared by the node and the indexer, D-049).

pub use ksana_btc::tracker::*;

use std::collections::BTreeMap;

use ksana_primitives::Params;

use crate::NodeError;

/// The carriers of the blocks the main chain accepted (D-095) as [`carrier_of`] determines them without a landing
/// limit, for answering many blocks at once: `ksana_getAccount` asks for every block after the unlock cursor, and each
/// [`carrier_of`] walked the window from the tip for the block and again for its carrier, so a call cost the number of
/// those blocks times the window (F-29, D-132). Here the blocks are looked up in an index of the main chain from a
/// height on, extended to the whole window at the first block it lacks, and below the window in the
/// [`MainHistory`] once per block. Only for the RPC: the execution layer asks [`carrier_of`] itself.
pub struct Carriers<'a> {
    params: &'a Params,
    snapshot: &'a BtcSnapshot,
    main: &'a MainChain,
    /// The blocks the main chain accepted at heights from `from` on, by hash.
    accepted: BTreeMap<[u8; 32], BlockSummary>,
    from: u64,
    /// The answers of the [`MainHistory`] for the blocks not in the window.
    below: BTreeMap<[u8; 32], Option<BlockSummary>>,
}

impl<'a> Carriers<'a> {
    /// The index of the blocks the main chain `main` accepted from height `from` on.
    pub fn new(
        params: &'a Params,
        snapshot: &'a BtcSnapshot,
        main: &'a MainChain,
        from: u64,
    ) -> Self {
        let mut carriers = Carriers {
            params,
            snapshot,
            main,
            accepted: BTreeMap::new(),
            from: main.tip_height().saturating_add(1),
            below: BTreeMap::new(),
        };
        carriers.extend(from);
        carriers
    }

    /// Add the heights from `from` below those indexed. As [`accepted_summary`] walks from the tip and takes a
    /// main-chain block before the blocks it merges, the first summary of a hash, walking down, stays.
    fn extend(&mut self, from: u64) {
        let main = self.main;
        for (height, entry) in main.iter_from(from).rev() {
            if height >= self.from {
                continue;
            }
            for block in [entry.summary()]
                .into_iter()
                .chain(entry.merged.iter().copied())
            {
                self.accepted.entry(block.hash).or_insert(block);
            }
        }
        self.from = self.from.min(from.max(main.base()));
    }

    /// The summary of block `hash` when the main chain accepted it, as [`accepted_summary`] answers.
    fn summary(&mut self, hash: &[u8; 32]) -> Result<Option<BlockSummary>, String> {
        if let Some(block) = self.accepted.get(hash) {
            return Ok(Some(*block));
        }
        let base = self.main.base();
        if self.from > base {
            self.extend(base);
            if let Some(block) = self.accepted.get(hash) {
                return Ok(Some(*block));
            }
        }
        if let Some(found) = self.below.get(hash) {
            return Ok(*found);
        }
        let found = match self.main.history() {
            Some(history) => history
                .accepted(hash)?
                .filter(|(height, _)| *height < base)
                .map(|(_, summary)| summary),
            None => None,
        };
        self.below.insert(*hash, found);
        Ok(found)
    }

    /// The carrier of block `block_hash`: what [`carrier_of`] answers with no limit.
    pub fn carrier(&mut self, block_hash: &[u8; 32]) -> Result<Option<[u8; 32]>, String> {
        let Some(block) = self.summary(block_hash)? else {
            return Ok(None);
        };
        let Some(landing) = self.snapshot.data_landing(&block.hash) else {
            return Ok(None);
        };
        match landing.sender {
            Sender::Block(carrier_block) => Ok(self.summary(&carrier_block)?.map(|c| c.producer)),
            Sender::Payee(payee) => {
                let waited =
                    u64::from(landing.height).saturating_sub(u64::from(block.anchor_height));
                let bound = u64::from(self.params.anchor_depth)
                    .saturating_add(u64::from(self.params.carrier_repost_delay));
                Ok((payee != [0; 32] && waited > bound).then_some(payee))
            }
        }
    }
}

impl From<TrackerError> for NodeError {
    fn from(e: TrackerError) -> Self {
        match e {
            TrackerError::Btc(m) => NodeError::Btc(m),
            // A BTC reorg below what this node keeps (D-097): it cannot continue and must be resynced
            TrackerError::TooDeep(m) => NodeError::Btc(format!(
                "{m}; this node cannot follow it and must be resynced (D-097)"
            )),
            TrackerError::Config(m) => NodeError::Config(m),
            TrackerError::Known(m) => NodeError::Storage(m),
            TrackerError::Internal(m) => NodeError::Internal(m),
        }
    }
}
