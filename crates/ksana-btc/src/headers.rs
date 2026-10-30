//! The fast-lane block DAG (SPEC §6.4, §10.3; D-025, D-035, D-041, D-048, D-063).
//!
//! Headers come only from the settlement and republish payloads on BTC and prove themselves by PoW and parent hashes
//! (§7.1):
//!
//! - When all parents are already in the DAG, the header is checked by the node's header rules: the PoW against the
//!   header's own `bits` first ([`Consensus::check_header_pow`]), then GHOSTDAG's parent and merge limits
//!   ([`BlockDag::compute`]) and the rest of [`Consensus::check_header`] (version, `parents[0]` is the selected parent
//!   and the rest ascending, height, `bits` by `daa_score`); the anchor height is not below the highest anchor
//!   among the parents (same as the node). The reception time rules (median time, local time, anchor lag) are not
//!   checked, just as when the node syncs historical blocks; the timestamp must not exceed the time of the anchored
//!   BTC block plus `MAX_TIMESTAMP_AHEAD_OF_ANCHOR` (D-083; the BTC block at the anchor height in the caller's BTC
//!   snapshot, and the check is skipped when the snapshot does not have that height). Headers that fail are dropped.
//! - The headers come only from payloads that count (D-100, [`crate::tracker`]): every header of such a payload
//!   anchors below the payload's BTC height, so the anchored BTC block has been scanned and the timestamp bound above
//!   is checked.
//! - A header joins only once all its parents are in the DAG: the tracker asks about a header only once its parents
//!   have counted on BTC (D-135), so the headers arrive parents first, and a header whose parent is missing is not
//!   added and not held ([`Insert::MissingParent`]).
//! - The main chain is the same as the node's (D-063, D-104): the selected chain of the block with the largest
//!   fork-choice key `(chain_work, hash)` (blue work plus the block's own work, D-100; with equal chain work the larger
//!   hash wins) among the blocks whose selected chain passes through the finality floor and has no ineligible block
//!   above it. A block is ineligible when it cannot land, or when its own anchor or the anchor of a block it merges is
//!   off the BTC best chain of the caller's snapshot (the BTC block at that height is another one, D-101 item c): the
//!   finality decision finalizes neither such a block nor any block whose selected chain passes through it. Each tip
//!   gives one candidate, the highest block of its selected chain below the ineligible block closest to the main
//!   chain. Every operation that re-chooses the main chain takes the caller's BTC snapshot, and
//!   [`HeaderDag::reselect`] first retreats the main chain below a main-chain block that a BTC reorg made ineligible.
//!   Blocks the indexer finds cannot land during execution (`state_root` mismatch etc.) are marked by
//!   [`HeaderDag::disqualify`], and the main chain is re-chosen accordingly. A mark keeps the basis of the verdict
//!   (the BTC block at the block's anchor height, [`crate::verdict::basis`], D-101): once a BTC reorg changed it, the
//!   verdict no longer holds, and the mark of a block the finality decision then finalizes is dropped
//!   ([`HeaderDag::requalify`]).
//! - The finality floor (D-082): the main chain must pass through the finalized block [`HeaderDag::floor`] (the
//!   finality decision is given by [`crate::finality`], and the caller sets it with [`HeaderDag::set_floor`]); tips
//!   whose selected chain does not pass through it do not take part in main-chain selection, and finalized blocks
//!   cannot be marked as unable to land either.
//!
//! Accepted headers are deleted only by pruning (D-097, [`HeaderDag::prune`]): when a BTC reorg takes a payload off the
//! best chain they stay in the DAG (the headers themselves are valid), but the corresponding blocks no longer have
//! data landed, and the settled closure (D-038, D-063) excludes them from the processable range. Pruning keeps the
//! pruning point (on the finalized chain) and its future, and the main chain from the pruning point up; a header that
//! can never be in the future of the pruning point (its anchor is below the pruning point's, or it needs a pruned
//! block) is not added ([`Insert::Pruned`], [`Insert::MissingParent`]).

use std::collections::BTreeMap;

use borsh::{BorshDeserialize, BorshSerialize};
use ksana_consensus::{
    check_anchor_time, BlockDag, Consensus, ConsensusError, DagCheckpoint, GhostdagData,
    GhostdagError, GhostdagParams, Hash, PowConsensus, U256,
};
use ksana_primitives::{BlockHeader, BtcAnchor, Params};

use crate::tracker::BtcSnapshot;

/// The result of adding a header.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Insert {
    /// Already in the DAG.
    Duplicate,
    /// Some parent is not in the DAG: the header is not added (D-135: the tracker asks about a header only once its
    /// parents have counted; after pruning, D-097, a parent may be below the pruning point).
    MissingParent,
    /// Added to the DAG: the number of blocks added (always 1).
    Connected(usize),
    /// The header is invalid and dropped (reason).
    Invalid(String),
    /// This header cannot be checked locally (e.g. the PoW value cannot be computed, D-094): it is neither connected
    /// nor judged invalid, and this insertion is aborted (reason). This is a local fault; the caller handles it as a
    /// local fault (alert, stop this round of processing) and must not treat the header as invalid.
    Unavailable(String),
    /// The header is not in the future of the pruning point (D-097: its anchor is below the pruning point's, or it
    /// needs a pruned block): this DAG no longer keeps that part and drops it without judging it. Such a header cannot
    /// be on, or merged by, a chain through the finalized blocks.
    Pruned,
}

/// Why [`HeaderDag::accept`] rejected a header.
enum Reject {
    /// The header is invalid.
    Invalid(String),
    /// Cannot be checked locally (local fault).
    Unavailable(String),
    /// Needs a block pruned away (D-097).
    Pruned,
}

impl From<GhostdagError> for Reject {
    fn from(e: GhostdagError) -> Self {
        match e {
            GhostdagError::Pruned(_) => Reject::Pruned,
            other => Reject::Invalid(other.to_string()),
        }
    }
}

impl From<ConsensusError> for Reject {
    fn from(e: ConsensusError) -> Self {
        match e {
            ConsensusError::PowUnavailable(_)
            | ConsensusError::InvalidParams(_)
            | ConsensusError::InvalidPowLimit(_) => Reject::Unavailable(e.to_string()),
            other => Reject::Invalid(other.to_string()),
        }
    }
}

/// The block DAG (headers only) and the main chain.
pub struct HeaderDag {
    dag: BlockDag<BlockHeader>,
    /// Blocks that cannot land (found by the indexer during execution, D-063), with the basis of that verdict (D-101).
    disqualified: BTreeMap<Hash, Option<[u8; 32]>>,
    /// The main chain from height `main_base` (index = height − `main_base`): from the genesis block, or from the
    /// pruning point after pruning (D-097).
    main: Vec<Hash>,
    /// The height of the first block of `main`.
    main_base: u64,
    /// The finality floor (D-082): the block the main chain must pass through; starts at the genesis block.
    floor: Hash,
}

/// A [`HeaderDag`] in the local format of a pruning checkpoint (D-097).
#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct HeaderDagRecord {
    dag: DagCheckpoint<BlockHeader>,
    disqualified: BTreeMap<Hash, Option<[u8; 32]>>,
    main: Vec<Hash>,
    main_base: u64,
    floor: Hash,
}

impl HeaderDag {
    /// A DAG with only the genesis block.
    pub fn new(
        params: &Params,
        consensus: &PowConsensus,
        genesis: BlockHeader,
    ) -> Result<Self, String> {
        let hash = genesis.hash();
        let work = consensus
            .genesis_work(&genesis)
            .map_err(|e| e.to_string())?;
        Ok(HeaderDag {
            dag: BlockDag::new(GhostdagParams::from_params(params), hash, work, genesis),
            disqualified: BTreeMap::new(),
            main: vec![hash],
            main_base: 0,
            floor: hash,
        })
    }

    /// The DAG in the local format of a pruning checkpoint (D-097).
    pub fn record(&self) -> Result<HeaderDagRecord, String> {
        Ok(HeaderDagRecord {
            dag: self.dag.checkpoint().map_err(|e| e.to_string())?,
            disqualified: self.disqualified.clone(),
            main: self.main.clone(),
            main_base: self.main_base,
            floor: self.floor,
        })
    }

    /// The DAG a [`HeaderDag::record`] was taken from.
    pub fn from_record(params: &Params, record: HeaderDagRecord) -> Result<Self, String> {
        let dag = BlockDag::restore(GhostdagParams::from_params(params), record.dag)
            .map_err(|e| e.to_string())?;
        if record.main.is_empty() || record.main.iter().any(|h| !dag.contains(h)) {
            return Err("the main chain in the checkpoint is not in its DAG".into());
        }
        if !dag.contains(&record.floor) {
            return Err("the finality floor in the checkpoint is not in its DAG".into());
        }
        Ok(HeaderDag {
            dag,
            disqualified: record.disqualified,
            main: record.main,
            main_base: record.main_base,
            floor: record.floor,
        })
    }

    /// Prunes at `point` (D-097): keeps it and its future in the DAG ([`BlockDag::prune`]) and the main chain from it
    /// up, and drops the marks of blocks no longer kept. `point` must be on the main chain at or below the finality
    /// floor. Returns the number of blocks dropped from the DAG.
    pub fn prune(&mut self, point: &Hash) -> Result<usize, String> {
        let header = self
            .dag
            .payload(point)
            .ok_or("the pruning point is not in the header DAG")?;
        let height = header.height;
        if !self.on_main(point) || height > self.floor_height() {
            return Err(
                "the pruning point is not on the main chain at or below the finality floor".into(),
            );
        }
        let dropped = self.dag.prune(point).map_err(|e| e.to_string())?;
        let skip = usize::try_from(height.saturating_sub(self.main_base))
            .map_err(|_| "main-chain height overflow".to_string())?;
        self.main.drain(..skip.min(self.main.len()));
        self.main_base = height;
        let dag = &self.dag;
        self.disqualified.retain(|h, _| dag.contains(h));
        Ok(dropped)
    }

    /// The GHOSTDAG DAG (with the headers attached).
    pub fn dag(&self) -> &BlockDag<BlockHeader> {
        &self.dag
    }

    /// The header of a block in the DAG.
    pub fn header(&self, hash: &Hash) -> Option<&BlockHeader> {
        self.dag.payload(hash)
    }

    /// The GHOSTDAG data of a block in the DAG.
    pub fn ghostdag(&self, hash: &Hash) -> Option<&GhostdagData> {
        self.dag.data(hash)
    }

    /// The main chain held, from height [`HeaderDag::main_base`] (index = height − base).
    pub fn main(&self) -> &[Hash] {
        &self.main
    }

    /// The height of the first block of [`HeaderDag::main`]: 0 (the genesis block), or the pruning point's height after
    /// pruning (D-097).
    pub fn main_base(&self) -> u64 {
        self.main_base
    }

    fn main_index(&self, height: u64) -> Option<usize> {
        usize::try_from(height.checked_sub(self.main_base)?).ok()
    }

    /// The hash of the main-chain block at that height (within the part held).
    pub fn main_hash(&self, height: u64) -> Option<&Hash> {
        self.main.get(self.main_index(height)?)
    }

    /// The header of the main-chain block at that height.
    pub fn main_at(&self, height: u64) -> Option<&BlockHeader> {
        self.dag.payload(self.main_hash(height)?)
    }

    /// The height of the main-chain tip.
    pub fn main_height(&self) -> u64 {
        let len = u64::try_from(self.main.len()).unwrap_or(u64::MAX);
        self.main_base.saturating_add(len.saturating_sub(1))
    }

    /// Whether the block's header is in the DAG. After pruning (D-097) these are the pruning point and its future.
    pub fn holds(&self, hash: &Hash) -> bool {
        self.dag.contains(hash)
    }

    /// Whether the block is marked as unable to land.
    pub fn is_disqualified(&self, hash: &Hash) -> bool {
        self.disqualified.contains_key(hash)
    }

    /// Whether the block is marked as unable to land by a verdict given on `basis` (D-101): the mark still holds on the
    /// current BTC chain when `basis` is the current basis ([`crate::verdict::basis`]).
    pub fn disqualified_on(&self, hash: &Hash, basis: Option<[u8; 32]>) -> bool {
        basis.is_some() && self.disqualified.get(hash) == Some(&basis)
    }

    /// The finality floor (D-082): the block the main chain must pass through.
    pub fn floor(&self) -> &Hash {
        &self.floor
    }

    /// The height of the finality floor.
    pub fn floor_height(&self) -> u64 {
        self.height_of(&self.floor).unwrap_or(0)
    }

    /// The height of a block in the DAG.
    pub fn height_of(&self, hash: &Hash) -> Option<u64> {
        self.dag.payload(hash).map(|h| h.height)
    }

    /// Whether the block is on the selected chain of the finality floor (including the floor itself), i.e. final
    /// (D-082). The main chain always passes through the floor, so this is a lookup in the main chain; blocks pruned
    /// away (D-097) are not in the DAG and are not answered here.
    pub fn is_final(&self, hash: &Hash) -> bool {
        match self.height_of(hash) {
            Some(height) if height <= self.floor_height() => self.main_hash(height) == Some(hash),
            _ => false,
        }
    }

    /// Sets the finality floor (D-082): `hash` must be in the DAG. The floor normally advances along the finalized
    /// chain; after a BTC reorg revoked decisions it may go back, or even switch to another chain (the decision is a
    /// deterministic function of the BTC chain data). When the main chain does not pass through it, the main chain
    /// becomes its selected chain and is then extended to the best candidate under the BTC snapshot `btc` as usual;
    /// when its selected chain contains a block already marked as unable to land, an error is returned (a finalized
    /// block is invalid: the caller stops processing and alerts).
    pub fn set_floor(&mut self, hash: Hash, btc: &BtcSnapshot) -> Result<(), String> {
        if hash == self.floor {
            return Ok(());
        }
        if !self.dag.contains(&hash) {
            return Err("the finality floor is not in the DAG".into());
        }
        if !self.on_main(&hash) {
            // the main chain becomes the selected chain of the new floor: walk it down to the main chain
            let mut branch = Vec::new();
            let mut cursor = hash;
            while !self.on_main(&cursor) {
                if self.disqualified.contains_key(&cursor) {
                    return Err(format!(
                        "block {} on the BTC-finalized chain has been judged unable to land (D-082)",
                        hex::encode(cursor)
                    ));
                }
                branch.push(cursor);
                cursor = self
                    .dag
                    .data(&cursor)
                    .and_then(|d| d.selected_parent)
                    .filter(|sp| self.dag.contains(sp))
                    .ok_or("the finalized chain does not pass through the pruning point (D-097)")?;
            }
            let fork = self
                .height_of(&cursor)
                .and_then(|h| self.main_index(h))
                .ok_or("main-chain height out of range")?;
            self.main.truncate(fork.saturating_add(1));
            self.main.extend(branch.into_iter().rev());
        }
        self.floor = hash;
        self.update_main(btc);
        Ok(())
    }

    /// Re-chooses the main chain under the BTC snapshot `btc` (D-104): every main-chain block above the finality floor
    /// must be eligible (see the module docs), so when a BTC reorg took the anchor of one of them, or of a block it
    /// merges, off the best chain, the main chain first retreats to just below the lowest such block; then it goes to
    /// the best candidate as usual. The result depends only on the DAG, `btc`, the floor and the marks, however the
    /// main chain got here. Returns the lowest ineligible block and its height when the main chain retreated.
    pub fn reselect(&mut self, btc: &BtcSnapshot) -> Option<(Hash, u64)> {
        let above_floor = usize::try_from(
            self.floor_height()
                .saturating_add(1)
                .saturating_sub(self.main_base),
        )
        .unwrap_or(usize::MAX);
        let lowest = self
            .main
            .iter()
            .enumerate()
            .skip(above_floor)
            .find(|(_, hash)| self.unanchored(btc, hash).is_some())
            .map(|(index, hash)| (index, *hash));
        let retreated = lowest.map(|(index, hash)| {
            self.main.truncate(index);
            let offset = u64::try_from(index).unwrap_or(u64::MAX);
            (hash, self.main_base.saturating_add(offset))
        });
        self.update_main(btc);
        retreated
    }

    /// Block `hash` itself, or else the first block it merges, whose anchor is off the best chain of the BTC snapshot
    /// `btc` (the BTC block at the anchor height is another one, D-101 item c): such a block is ineligible for the main
    /// chain, and so is every block whose selected chain passes through it (D-104). `None` when every anchor is on it
    /// or above its tip, or when the block is not in the DAG; a merged block pruned away (D-097) is not looked at.
    pub fn unanchored(&self, btc: &BtcSnapshot, hash: &Hash) -> Option<Hash> {
        let data = self.dag.data(hash)?;
        std::iter::once(hash)
            .chain(data.mergeset_ordered.iter())
            .find(|member| {
                self.header(member)
                    .is_some_and(|header| off_chain(btc, &header.btc_anchor))
            })
            .copied()
    }

    /// Adds a header (see the module docs) and then updates the main chain under the BTC snapshot `btc`, whose block at
    /// the anchor height gives the BTC header time the timestamp bound is based on (D-083: the check is skipped when
    /// the snapshot does not have that height).
    pub fn insert(
        &mut self,
        consensus: &PowConsensus,
        header: BlockHeader,
        btc: &BtcSnapshot,
    ) -> Insert {
        let hash = header.hash();
        if self.dag.contains(&hash) {
            return Insert::Duplicate;
        }
        if self.below_pruning_point(&header) {
            return Insert::Pruned;
        }
        if header.parents.iter().any(|p| !self.dag.contains(p)) {
            return Insert::MissingParent;
        }
        match self.accept(consensus, header, btc) {
            Ok(()) => {
                self.update_main(btc);
                Insert::Connected(1)
            }
            Err(Reject::Unavailable(reason)) => Insert::Unavailable(reason),
            Err(Reject::Invalid(reason)) => Insert::Invalid(reason),
            // not in the future of the pruning point (D-097): dropped without a judgment
            Err(Reject::Pruned) => Insert::Pruned,
        }
    }

    /// A header whose parents are all in the DAG: checked, then inserted.
    fn accept(
        &mut self,
        consensus: &PowConsensus,
        header: BlockHeader,
        btc: &BtcSnapshot,
    ) -> Result<(), Reject> {
        if header.parents.is_empty() {
            return Err(Reject::Invalid(
                "a non-genesis block must have parents".into(),
            ));
        }
        // weight (D-084, D-090): an empty block's weight grows with the selected parent's empty-block streak and enters
        // the DAA score, the effective target and the work
        let invalid = |m: String| Reject::Invalid(m);
        let sp_streak = self.dag.parent_streak(&header.parents)?;
        let weight = consensus.weight(&header, sp_streak);
        let streak = consensus.streak(&header, sp_streak);
        // the PoW against the header's own bits before GHOSTDAG, which costs more the longer the merged chains: a
        // header without valid PoW does not cost it (second audit, P1-1); `bits` is compared with GHOSTDAG's after
        consensus.check_header_pow(&header, weight)?;
        let data = self.dag.compute(&header.parents, weight)?;
        let sp = data
            .selected_parent
            .ok_or_else(|| invalid("no selected parent".into()))?;
        let sp_header = self
            .dag
            .payload(&sp)
            .ok_or_else(|| invalid("selected parent missing".into()))?;
        consensus.check_header_rules(sp_header, data.daa_score, weight, &header)?;
        let mut parent_anchor = 0u32;
        for parent in &header.parents {
            let anchor = self
                .dag
                .payload(parent)
                .ok_or_else(|| invalid("parent missing".into()))?
                .btc_anchor
                .height;
            parent_anchor = parent_anchor.max(anchor);
        }
        if header.btc_anchor.height < parent_anchor {
            return Err(invalid(
                "the height of btc_anchor is below the parents' anchor".into(),
            ));
        }
        if let Some(time) = btc.header_time_at(header.btc_anchor.height) {
            check_anchor_time(consensus.params(), &header, time)?;
        }
        let work = consensus.work(&header, weight)?;
        self.dag.insert(
            header.hash(),
            header.parents.clone(),
            work,
            weight,
            streak,
            header,
        )?;
        Ok(())
    }

    /// Whether the header's anchor is below the pruning point's (D-097): anchors never decrease along parent edges, so
    /// such a header can never be in the pruning point's future.
    fn below_pruning_point(&self, header: &BlockHeader) -> bool {
        self.dag
            .pruning_point()
            .and_then(|p| self.dag.payload(p))
            .is_some_and(|p| header.btc_anchor.height < p.btc_anchor.height)
    }

    /// Marks the block as unable to land (D-063) by a verdict given on `basis` (D-101) and re-chooses the main chain
    /// under the BTC snapshot `btc`. A finalized block (D-082) cannot be marked: an error is returned, and the caller
    /// stops processing and alerts.
    pub fn disqualify(
        &mut self,
        hash: &Hash,
        basis: Option<[u8; 32]>,
        btc: &BtcSnapshot,
    ) -> Result<(), String> {
        if self.is_final(hash) {
            return Err(format!(
                "block {} is already BTC-final (D-082) and cannot be marked as unable to land",
                hex::encode(hash)
            ));
        }
        self.disqualified.insert(*hash, basis);
        // when the main chain passes through it, truncate to just before it, then re-choose
        if let Some(position) = self.main.iter().position(|h| h == hash) {
            self.main.truncate(position);
        }
        self.update_main(btc);
        Ok(())
    }

    /// Drops the marks on the selected chain of `hash` (down to the main chain) whose verdict no longer holds on the
    /// current BTC chain (D-101): `basis` gives the current basis of a block ([`crate::verdict::basis`]). Called before
    /// the floor moves to `hash`: the finality decision judged the blocks on its selected chain valid on the current
    /// chain, so a mark left there means a non-determinism bug, which [`HeaderDag::set_floor`] reports. Returns the
    /// number of marks dropped.
    pub fn requalify(
        &mut self,
        hash: &Hash,
        basis: &dyn Fn(&BlockHeader) -> Option<[u8; 32]>,
    ) -> usize {
        let mut dropped = 0usize;
        let mut cursor = *hash;
        while !self.on_main(&cursor) {
            let Some(block) = self.dag.get(&cursor) else {
                break;
            };
            if let Some(recorded) = self.disqualified.get(&cursor) {
                let current = basis(&block.payload);
                if current.is_none() || *recorded != current {
                    self.disqualified.remove(&cursor);
                    dropped = dropped.saturating_add(1);
                }
            }
            match block.data.selected_parent {
                Some(sp) => cursor = sp,
                None => break,
            }
        }
        dropped
    }

    fn key(&self, hash: &Hash) -> Option<(U256, Hash)> {
        self.dag.chain_key(hash).ok()
    }

    fn on_main(&self, hash: &Hash) -> bool {
        self.dag
            .payload(hash)
            .and_then(|h| self.main_hash(h.height))
            == Some(hash)
    }

    /// Whether the block is ineligible for the main chain under the BTC snapshot `btc` (see the module docs): it cannot
    /// land, or its own anchor or that of a block it merges is off the BTC best chain (D-101, D-104).
    fn ineligible(&self, btc: &BtcSnapshot, hash: &Hash) -> bool {
        self.disqualified.contains_key(hash) || self.unanchored(btc, hash).is_some()
    }

    /// The best candidate for the main-chain tip under the BTC snapshot `btc` (same as the node's
    /// `Chain::best_candidate`): must be strictly better than the current tip, and its selected chain must pass through
    /// the finality floor (D-082: the main chain passes through the floor, so it suffices that the junction of the
    /// tip's selected chain with the main chain is not below the floor) and have no ineligible block above the main
    /// chain. Candidates are taken from the selected-chain tips (blocks that are no block's selected parent) rather
    /// than only from the tips without children: when the successors of a finalized block are merged by a heavier
    /// chain, their selected chains pass through the floor while the blocks merging them do not.
    fn best_candidate(&self, btc: &BtcSnapshot) -> Option<Hash> {
        let tip_key = self.main.last().and_then(|h| self.key(h))?;
        let floor_height = self.floor_height();
        let mut best: Option<(U256, Hash)> = None;
        // a candidate's key never exceeds its tip's: only tips above the current tip can give a better candidate, and
        // once a tip is not above the best candidate found, no later (lighter) tip can beat it
        for tip_key_of in self.dag.selected_chain_tips_desc(Some(&tip_key)) {
            if best.as_ref().is_some_and(|b| tip_key_of <= b) {
                break;
            }
            // the tip's selected chain, down to the main chain (exclusive)
            let mut path = Vec::new();
            let mut cursor = tip_key_of.1;
            let mut joined = true;
            while !self.on_main(&cursor) {
                path.push(cursor);
                match self.dag.data(&cursor).and_then(|d| d.selected_parent) {
                    Some(sp) if self.dag.contains(&sp) => cursor = sp,
                    // forks below the pruning point (D-097) or reaches the genesis block off the main chain
                    _ => {
                        joined = false;
                        break;
                    }
                }
            }
            if !joined || self.height_of(&cursor).is_none_or(|h| h < floor_height) {
                // forks below a finalized block: does not take part in main-chain selection
                continue;
            }
            let candidate = match path.iter().rposition(|h| self.ineligible(btc, h)) {
                Some(lowest) => path.get(lowest.saturating_add(1)).copied(),
                None => path.first().copied(),
            };
            let Some(key) = candidate.and_then(|c| self.key(&c)) else {
                continue;
            };
            if best.as_ref().is_none_or(|b| key > *b) {
                best = Some(key);
            }
        }
        best.filter(|key| *key > tip_key).map(|(_, hash)| hash)
    }

    /// Changes the main chain to the best candidate's selected chain under the BTC snapshot `btc`: walk back from the
    /// candidate to a block on the main chain and replace the part after it.
    fn update_main(&mut self, btc: &BtcSnapshot) {
        let Some(candidate) = self.best_candidate(btc) else {
            return;
        };
        let mut branch = Vec::new();
        let mut cursor = candidate;
        while !self.on_main(&cursor) {
            branch.push(cursor);
            match self.dag.data(&cursor).and_then(|d| d.selected_parent) {
                Some(sp) => cursor = sp,
                // cannot happen: the candidate's chain joins the main chain (checked by `best_candidate`)
                None => return,
            }
        }
        let Some(fork) = self.height_of(&cursor).and_then(|h| self.main_index(h)) else {
            return;
        };
        self.main.truncate(fork.saturating_add(1));
        self.main.extend(branch.into_iter().rev());
    }
}

/// Whether `anchor` is off the best chain of the BTC snapshot `btc`: the BTC block at its height is another one (D-101
/// item c). An anchor above the snapshot's tip is not off it.
fn off_chain(btc: &BtcSnapshot, anchor: &BtcAnchor) -> bool {
    btc.hash_at(anchor.height)
        .is_some_and(|on_chain| on_chain != anchor.hash)
}

#[cfg(test)]
pub(crate) mod test_support {
    //! For tests: the genesis block, headers with the PoW solved.
    use super::*;
    use ksana_primitives::{BLOCK_VERSION, DEVNET};

    pub(crate) fn genesis() -> (PowConsensus, BlockHeader) {
        let genesis = ksana_consensus::genesis_header(
            &DEVNET,
            1_790_000_000,
            BtcAnchor {
                height: 10,
                hash: [7; 32],
            },
            [1; 32],
        )
        .unwrap();
        (PowConsensus::new(&DEVNET, &genesis).unwrap(), genesis)
    }

    pub(crate) fn new_dag(consensus: &PowConsensus, g: &BlockHeader) -> HeaderDag {
        HeaderDag::new(&DEVNET, consensus, g.clone()).unwrap()
    }

    /// An empty BTC snapshot: no timestamp bound is checked (the anchor block's header cannot be obtained), and no
    /// anchor is off its chain.
    pub(crate) fn no_btc() -> BtcSnapshot {
        BtcSnapshot::default()
    }

    /// A header with `parents` as parents (ordered per GHOSTDAG: the selected parent first, the rest ascending) (`tag`
    /// distinguishes different blocks), with the PoW solved. `dag` must already contain all parents.
    pub(crate) fn block(
        consensus: &PowConsensus,
        dag: &HeaderDag,
        parents: &[&BlockHeader],
        tag: u8,
    ) -> BlockHeader {
        block_anchored(consensus, dag, parents, tag, None)
    }

    /// Like [`block`], anchored at BTC height `anchor` (the anchor hash derived from the height) instead of the
    /// selected parent's anchor when given.
    pub(crate) fn block_anchored(
        consensus: &PowConsensus,
        dag: &HeaderDag,
        parents: &[&BlockHeader],
        tag: u8,
        anchor: Option<u32>,
    ) -> BlockHeader {
        block_at(consensus, dag, parents, tag, |sp_anchor| match anchor {
            Some(height) if height != sp_anchor.height => BtcAnchor {
                height,
                hash: [u8::try_from(height % 256).unwrap(); 32],
            },
            _ => sp_anchor.clone(),
        })
    }

    /// Like [`block`], anchored where `anchor` says given the selected parent's anchor.
    pub(crate) fn block_at(
        consensus: &PowConsensus,
        dag: &HeaderDag,
        parents: &[&BlockHeader],
        tag: u8,
        anchor: impl FnOnce(&BtcAnchor) -> BtcAnchor,
    ) -> BlockHeader {
        let hashes: Vec<Hash> = parents.iter().map(|p| p.hash()).collect();
        // a non-empty block (entries_root is not the root of the empty entry list): weight 1
        let data = dag.dag().compute(&hashes, 1).unwrap();
        let sp = data.selected_parent.unwrap();
        let sp_header = dag.header(&sp).unwrap().clone();
        let mut rest: Vec<Hash> = hashes.into_iter().filter(|h| *h != sp).collect();
        rest.sort_unstable();
        let btc_anchor = anchor(&sp_header.btc_anchor);
        let mut header = BlockHeader {
            version: BLOCK_VERSION,
            height: sp_header.height + 1,
            parents: [vec![sp], rest].concat(),
            btc_anchor,
            entries_root: [tag; 32],
            state_root: [tag; 32],
            producer: [tag; 32],
            timestamp: sp_header.timestamp + 5,
            bits: consensus.next_bits(&sp_header, data.daa_score, 1).unwrap(),
            nonce: 0,
        };
        assert!(consensus.solve(&mut header, 1, 0, 1 << 20).unwrap());
        header
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::*;
    use super::*;
    use ksana_primitives::DEVNET;

    #[test]
    fn heavier_tip_wins_and_ties_go_to_the_larger_hash() {
        let (consensus, g) = genesis();
        let mut dag = new_dag(&consensus, &g);
        let a1 = block(&consensus, &dag, &[&g], 1);
        let b1 = block(&consensus, &dag, &[&g], 2);
        assert_eq!(
            dag.insert(&consensus, a1.clone(), &no_btc()),
            Insert::Connected(1)
        );
        assert_eq!(
            dag.insert(&consensus, b1.clone(), &no_btc()),
            Insert::Connected(1)
        );
        assert_eq!(
            dag.insert(&consensus, b1.clone(), &no_btc()),
            Insert::Duplicate
        );
        // equal blue work: the larger hash is the main-chain tip
        let best = a1.hash().max(b1.hash());
        assert_eq!(dag.main(), &[g.hash(), best]);
        // a block merging two parallel blocks: the main chain passes through the selected parent
        let m = block(&consensus, &dag, &[&a1, &b1], 3);
        assert_eq!(m.parents.len(), 2);
        assert_eq!(
            dag.insert(&consensus, m.clone(), &no_btc()),
            Insert::Connected(1)
        );
        assert_eq!(dag.main(), &[g.hash(), best, m.hash()]);
        assert_eq!(dag.main_height(), 2);
        assert_eq!(dag.main_at(2), Some(&m));
        let data = dag.ghostdag(&m.hash()).unwrap();
        assert_eq!(data.mergeset_ordered, vec![a1.hash().min(b1.hash())]);
    }

    /// D-135: a header whose parents are not all in the DAG is not added, and nothing of it is held; it joins once it
    /// is inserted again after them.
    #[test]
    fn a_header_joins_only_after_all_its_parents() {
        let (consensus, g) = genesis();
        // first make all headers on a scratch copy
        let mut scratch = new_dag(&consensus, &g);
        let h1 = block(&consensus, &scratch, &[&g], 1);
        scratch.insert(&consensus, h1.clone(), &no_btc());
        let side = block(&consensus, &scratch, &[&g], 9);
        scratch.insert(&consensus, side.clone(), &no_btc());
        let h2 = block(&consensus, &scratch, &[&h1, &side], 2);
        scratch.insert(&consensus, h2.clone(), &no_btc());
        let h3 = block(&consensus, &scratch, &[&h2], 3);

        let mut dag = new_dag(&consensus, &g);
        for header in [&h3, &h2] {
            assert_eq!(
                dag.insert(&consensus, header.clone(), &no_btc()),
                Insert::MissingParent
            );
        }
        assert_eq!(
            dag.insert(&consensus, h1.clone(), &no_btc()),
            Insert::Connected(1)
        );
        // h2 is still missing side
        assert_eq!(
            dag.insert(&consensus, h2.clone(), &no_btc()),
            Insert::MissingParent
        );
        assert!(!dag.holds(&h2.hash()) && !dag.holds(&h3.hash()));
        for header in [&side, &h2, &h3] {
            assert_eq!(
                dag.insert(&consensus, header.clone(), &no_btc()),
                Insert::Connected(1)
            );
        }
        assert_eq!(dag.main().last(), Some(&h3.hash()));
        assert_eq!(dag.main().len(), 4);
    }

    #[test]
    fn invalid_headers_are_dropped() {
        let (consensus, g) = genesis();
        let mut dag = new_dag(&consensus, &g);
        // bits does not match ASERT
        let mut bad_bits = block(&consensus, &dag, &[&g], 1);
        bad_bits.bits = 0x1f7f_ffff;
        assert!(matches!(
            dag.insert(&consensus, bad_bits, &no_btc()),
            Insert::Invalid(_)
        ));
        // wrong height
        let mut bad_height = block(&consensus, &dag, &[&g], 2);
        bad_height.height = 5;
        assert!(consensus.solve(&mut bad_height, 1, 0, 1 << 20).unwrap());
        assert!(matches!(
            dag.insert(&consensus, bad_height, &no_btc()),
            Insert::Invalid(_)
        ));
        // anchor height below the parent's (the node's header rule)
        let mut low_anchor = block(&consensus, &dag, &[&g], 3);
        low_anchor.btc_anchor.height = 9;
        assert!(consensus.solve(&mut low_anchor, 1, 0, 1 << 20).unwrap());
        assert!(matches!(
            dag.insert(&consensus, low_anchor.clone(), &no_btc()),
            Insert::Invalid(_)
        ));
        // wrong parent order (parents[0] is not the selected parent)
        let p1 = block(&consensus, &dag, &[&g], 4);
        let p2 = block(&consensus, &dag, &[&g], 5);
        dag.insert(&consensus, p1.clone(), &no_btc());
        dag.insert(&consensus, p2.clone(), &no_btc());
        let mut swapped = block(&consensus, &dag, &[&p1, &p2], 6);
        swapped.parents.swap(0, 1);
        assert!(consensus.solve(&mut swapped, 1, 0, 1 << 20).unwrap());
        assert!(matches!(
            dag.insert(&consensus, swapped.clone(), &no_btc()),
            Insert::Invalid(_)
        ));
        assert!(dag.header(&swapped.hash()).is_none());
        assert!(dag.header(&low_anchor.hash()).is_none());
    }

    /// A header whose parents are all present has its PoW checked against its own `bits` before GHOSTDAG (second
    /// audit, P1-1), which costs more the longer the merged chains. Here the second parent is in the past of the first,
    /// which GHOSTDAG rejects: with valid PoW that is the reason given; without it, the PoW is, before GHOSTDAG runs.
    #[test]
    fn the_pow_is_checked_before_ghostdag() {
        let (consensus, g) = genesis();
        let mut dag = new_dag(&consensus, &g);
        let a1 = block(&consensus, &dag, &[&g], 1);
        assert_eq!(
            dag.insert(&consensus, a1.clone(), &no_btc()),
            Insert::Connected(1)
        );
        let mut header = block(&consensus, &dag, &[&a1], 2);
        header.parents.push(g.hash());
        assert!(consensus.solve(&mut header, 1, 0, 1 << 20).unwrap());
        assert_eq!(
            dag.insert(&consensus, header.clone(), &no_btc()),
            Insert::Invalid(GhostdagError::ParentInPast(g.hash()).to_string())
        );
        while consensus.check_header_pow(&header, 1).is_ok() {
            header.nonce += 1;
        }
        assert_eq!(
            dag.insert(&consensus, header, &no_btc()),
            Insert::Invalid(ConsensusError::InsufficientPow.to_string())
        );
    }

    #[test]
    fn a_disqualified_block_leaves_the_main_chain() {
        let (consensus, g) = genesis();
        let mut dag = new_dag(&consensus, &g);
        let a1 = block(&consensus, &dag, &[&g], 1);
        dag.insert(&consensus, a1.clone(), &no_btc());
        let a2 = block(&consensus, &dag, &[&a1], 2);
        dag.insert(&consensus, a2.clone(), &no_btc());
        let a3 = block(&consensus, &dag, &[&a2], 3);
        dag.insert(&consensus, a3.clone(), &no_btc());
        let b1 = block(&consensus, &dag, &[&g], 4);
        dag.insert(&consensus, b1.clone(), &no_btc());
        let b2 = block(&consensus, &dag, &[&b1], 5);
        dag.insert(&consensus, b2.clone(), &no_btc());
        assert_eq!(dag.main(), &[g.hash(), a1.hash(), a2.hash(), a3.hash()]);
        // a2 cannot land: a3's selected chain passes through it, the main chain becomes the b branch (the a branch,
        // reduced to a1, has less blue work)
        dag.disqualify(&a2.hash(), Some([5; 32]), &no_btc())
            .unwrap();
        assert!(dag.is_disqualified(&a2.hash()));
        assert_eq!(dag.main(), &[g.hash(), b1.hash(), b2.hash()]);
        // once the b branch is heavier than a3, its blocks merge the a branch as usual (merged blocks need not be able
        // to land) and become main-chain blocks
        let b3 = block(&consensus, &dag, &[&b2], 6);
        dag.insert(&consensus, b3.clone(), &no_btc());
        let b4 = block(&consensus, &dag, &[&b3], 7);
        dag.insert(&consensus, b4.clone(), &no_btc());
        let m = block(&consensus, &dag, &[&b4, &a3], 8);
        assert_eq!(m.parents, vec![b4.hash(), a3.hash()]);
        assert_eq!(
            dag.insert(&consensus, m.clone(), &no_btc()),
            Insert::Connected(1)
        );
        assert_eq!(dag.main().last(), Some(&m.hash()));
        assert_eq!(dag.main().len(), 6);
        let merged = &dag.ghostdag(&m.hash()).unwrap().mergeset_ordered;
        assert_eq!(merged.len(), 3, "merges a1, a2, a3");
    }

    /// D-083: when the header time of the anchored BTC block can be obtained, a header whose timestamp is ahead of it
    /// by more than MAX_TIMESTAMP_AHEAD_OF_ANCHOR is invalid; when it cannot be obtained there is no check.
    #[test]
    fn timestamps_are_bounded_by_the_anchor_time() {
        let (consensus, g) = genesis();
        let mut dag = new_dag(&consensus, &g);
        let limit = DEVNET.max_timestamp_ahead_of_anchor_secs;
        let mut late = block(&consensus, &dag, &[&g], 1);
        late.timestamp = g.timestamp + limit + 1;
        assert!(consensus.solve(&mut late, 1, 0, 1 << 20).unwrap());
        // the genesis anchor (BTC height 10) with the genesis block's time
        let mut btc = BtcSnapshot::testing(10);
        btc.push_block([7; 32], u32::try_from(g.timestamp).unwrap());
        assert!(matches!(
            dag.insert(&consensus, late.clone(), &btc),
            Insert::Invalid(_)
        ));
        // exactly on the bound: valid
        let mut on_limit = block(&consensus, &dag, &[&g], 2);
        on_limit.timestamp = g.timestamp + limit;
        assert!(consensus.solve(&mut on_limit, 1, 0, 1 << 20).unwrap());
        assert_eq!(
            dag.insert(&consensus, on_limit.clone(), &btc),
            Insert::Connected(1)
        );
        // no check when the anchor block's header cannot be obtained
        let mut other = new_dag(&consensus, &g);
        assert_eq!(
            other.insert(&consensus, late, &no_btc()),
            Insert::Connected(1)
        );
    }

    /// D-082: the main chain must pass through the finality floor; a heavier chain forking below it does not take part
    /// in main-chain selection, and finalized blocks cannot be marked as unable to land.
    #[test]
    fn the_main_chain_passes_through_the_floor() {
        let (consensus, g) = genesis();
        let mut dag = new_dag(&consensus, &g);
        let a1 = block(&consensus, &dag, &[&g], 1);
        dag.insert(&consensus, a1.clone(), &no_btc());
        let a2 = block(&consensus, &dag, &[&a1], 2);
        dag.insert(&consensus, a2.clone(), &no_btc());
        // the b chain is heavier, the main chain is currently b
        let b1 = block(&consensus, &dag, &[&g], 3);
        dag.insert(&consensus, b1.clone(), &no_btc());
        let b2 = block(&consensus, &dag, &[&b1], 4);
        dag.insert(&consensus, b2.clone(), &no_btc());
        let b3 = block(&consensus, &dag, &[&b2], 5);
        dag.insert(&consensus, b3.clone(), &no_btc());
        assert_eq!(dag.main().last(), Some(&b3.hash()));
        // the b chain merged a2: a2 is no longer a tip, but still a selected-chain tip
        let m0 = block(&consensus, &dag, &[&b3, &a2], 6);
        assert_eq!(m0.parents, vec![b3.hash(), a2.hash()]);
        assert_eq!(
            dag.insert(&consensus, m0.clone(), &no_btc()),
            Insert::Connected(1)
        );
        assert_eq!(dag.main().last(), Some(&m0.hash()));
        // a1 final: the main chain becomes the a chain and extends to the merged a2 (the best block whose selected
        // chain passes through a1)
        dag.set_floor(a1.hash(), &no_btc()).unwrap();
        assert_eq!(dag.main(), &[g.hash(), a1.hash(), a2.hash()]);
        assert_eq!(dag.floor_height(), 1);
        assert!(dag.is_final(&a1.hash()));
        assert!(dag.is_final(&g.hash()));
        assert!(!dag.is_final(&a2.hash()));
        assert!(!dag.is_final(&b1.hash()));
        // however long the b chain grows, it does not take part
        let b4 = block(&consensus, &dag, &[&m0], 7);
        assert_eq!(
            dag.insert(&consensus, b4.clone(), &no_btc()),
            Insert::Connected(1)
        );
        assert_eq!(dag.main().last(), Some(&a2.hash()));
        // the floor must be in the DAG; finalized blocks cannot be marked as unable to land
        assert!(dag.set_floor([9; 32], &no_btc()).is_err());
        assert!(dag.disqualify(&a1.hash(), None, &no_btc()).is_err());
        // a2 cannot land: the main chain goes back to a1 (it cannot switch to the b chain past the floor)
        dag.disqualify(&a2.hash(), Some([5; 32]), &no_btc())
            .unwrap();
        assert_eq!(dag.main(), &[g.hash(), a1.hash()]);
        // the a chain extends itself: becomes the main chain
        let a2b = block(&consensus, &dag, &[&a1], 8);
        assert_eq!(
            dag.insert(&consensus, a2b.clone(), &no_btc()),
            Insert::Connected(1)
        );
        assert_eq!(dag.main(), &[g.hash(), a1.hash(), a2b.hash()]);
        // a block whose selected parent is on the b chain (a2b is just another parent it merges) does not take part
        // either
        let m = block(&consensus, &dag, &[&a2b, &b4], 7);
        assert_eq!(m.parents[0], b4.hash());
        assert_eq!(
            dag.insert(&consensus, m.clone(), &no_btc()),
            Insert::Connected(1)
        );
        assert_eq!(dag.main(), &[g.hash(), a1.hash(), a2b.hash()]);
        // the main-chain tip can be the new floor
        dag.set_floor(a2b.hash(), &no_btc()).unwrap();
        assert_eq!(dag.floor_height(), 2);
        assert!(dag.is_final(&a2b.hash()));
        // a BTC reorg revoked the decisions: the floor goes back to the genesis block, the main chain returns to the
        // heaviest chain (m's selected chain is the b chain)
        dag.set_floor(g.hash(), &no_btc()).unwrap();
        assert_eq!(dag.floor_height(), 0);
        assert_eq!(dag.main().last(), Some(&m.hash()));
        assert!(!dag.is_final(&a1.hash()));
        // the decision switches to another chain: the main chain follows; a chain passing through a block that cannot
        // land (a2) cannot become the floor
        dag.set_floor(b2.hash(), &no_btc()).unwrap();
        assert_eq!(dag.main().last(), Some(&m.hash()));
        assert!(dag.is_final(&b2.hash()));
        assert!(dag.set_floor(a2.hash(), &no_btc()).is_err());
        assert_eq!(dag.floor(), &b2.hash());
        // D-101: the mark on a2 holds while its basis is the current one, and is dropped once a BTC reorg changed it
        assert!(dag.disqualified_on(&a2.hash(), Some([5; 32])));
        assert!(!dag.disqualified_on(&a2.hash(), Some([6; 32])));
        assert!(!dag.disqualified_on(&a2.hash(), None));
        assert_eq!(dag.requalify(&a2.hash(), &|_| Some([5; 32])), 0);
        assert!(dag.set_floor(a2.hash(), &no_btc()).is_err());
        assert_eq!(dag.requalify(&a2.hash(), &|_| Some([6; 32])), 1);
        assert!(!dag.is_disqualified(&a2.hash()));
        dag.set_floor(a2.hash(), &no_btc()).unwrap();
        assert_eq!(dag.main(), &[g.hash(), a1.hash(), a2.hash()]);
    }

    /// A BTC snapshot from height 10 (the genesis anchor) holding `hashes`, every block with the genesis block's time.
    fn btc_chain(g: &BlockHeader, hashes: &[[u8; 32]]) -> BtcSnapshot {
        let mut btc = BtcSnapshot::testing(10);
        for hash in hashes {
            btc.push_block(*hash, u32::try_from(g.timestamp).unwrap());
        }
        btc
    }

    /// D-104: when a BTC reorg takes the anchor of a main-chain block, or of a block it merges, off the BTC best chain,
    /// [`HeaderDag::reselect`] takes the main chain back below the lowest such block and on to the best eligible
    /// candidate. An anchor above the snapshot's tip does not count, and the result depends only on the snapshot, not
    /// on how the main chain got there.
    #[test]
    fn a_reorged_anchor_takes_the_main_chain_back() {
        let (consensus, g) = genesis();
        let mut dag = new_dag(&consensus, &g);
        // BTC heights 10, 11 and 12 as the a chain anchored them; the reorged chain replaced height 11 and has not
        // reached 12; a lagging view has only height 10
        let before = btc_chain(&g, &[[7; 32], [11; 32], [12; 32]]);
        let after = btc_chain(&g, &[[7; 32], [0xee; 32]]);
        let lagging = btc_chain(&g, &[[7; 32]]);
        let add = |dag: &mut HeaderDag, header: &BlockHeader, btc: &BtcSnapshot| {
            assert_eq!(
                dag.insert(&consensus, header.clone(), btc),
                Insert::Connected(1)
            );
        };
        // the a chain anchored at BTC height 11 (a3 at 12), the lighter b chain at the genesis anchor
        let a1 = block_anchored(&consensus, &dag, &[&g], 1, Some(11));
        add(&mut dag, &a1, &before);
        let a2 = block(&consensus, &dag, &[&a1], 2);
        add(&mut dag, &a2, &before);
        let a3 = block_anchored(&consensus, &dag, &[&a2], 3, Some(12));
        add(&mut dag, &a3, &before);
        let b1 = block(&consensus, &dag, &[&g], 4);
        add(&mut dag, &b1, &before);
        let b2 = block(&consensus, &dag, &[&b1], 5);
        add(&mut dag, &b2, &before);
        assert_eq!(dag.reselect(&before), None);
        assert_eq!(dag.main(), &[g.hash(), a1.hash(), a2.hash(), a3.hash()]);
        assert_eq!(dag.unanchored(&before, &a1.hash()), None);
        // the reorg: a1 and a2 are anchored off the best chain, and a3 sits on them, so the main chain retreats to the
        // genesis block and goes to the b chain
        assert_eq!(dag.unanchored(&after, &a2.hash()), Some(a2.hash()));
        assert_eq!(dag.unanchored(&after, &a3.hash()), None);
        assert_eq!(dag.reselect(&after), Some((a1.hash(), 1)));
        assert_eq!(dag.main(), &[g.hash(), b1.hash(), b2.hash()]);
        assert_eq!(dag.reselect(&after), None);
        assert_eq!(dag.main(), &[g.hash(), b1.hash(), b2.hash()]);
        // a view below height 11 takes nothing off the chain: the a chain is the main chain again
        assert_eq!(dag.reselect(&lagging), None);
        assert_eq!(dag.main(), &[g.hash(), a1.hash(), a2.hash(), a3.hash()]);
        assert_eq!(dag.reselect(&after), Some((a1.hash(), 1)));
        // the b chain grows heavier and m merges the a chain: m is ineligible after the reorg, so is n on top of it
        let b3 = block(&consensus, &dag, &[&b2], 6);
        add(&mut dag, &b3, &after);
        let b4 = block(&consensus, &dag, &[&b3], 7);
        add(&mut dag, &b4, &after);
        let m = block_anchored(&consensus, &dag, &[&b4, &a3], 8, Some(12));
        assert_eq!(m.parents, vec![b4.hash(), a3.hash()]);
        add(&mut dag, &m, &after);
        let n = block(&consensus, &dag, &[&m], 9);
        add(&mut dag, &n, &after);
        assert_eq!(dag.unanchored(&after, &m.hash()), Some(a1.hash()));
        let b = [g.hash(), b1.hash(), b2.hash(), b3.hash(), b4.hash()];
        assert_eq!(dag.main(), &b);
        // before the reorg, m and n are the main chain; the reorg takes it back to b4
        assert_eq!(dag.reselect(&before), None);
        assert_eq!(dag.main(), &[&b[..], &[m.hash(), n.hash()]].concat());
        assert_eq!(dag.reselect(&after), Some((m.hash(), 5)));
        assert_eq!(dag.main(), &b);
        // the b chain goes on without merging the a chain
        let b5 = block(&consensus, &dag, &[&b4], 10);
        add(&mut dag, &b5, &after);
        assert_eq!(dag.main(), &[&b[..], &[b5.hash()]].concat());
    }

    /// D-104: the floor and the blocks below it are not looked at again (a BTC reorg that reaches them revokes the
    /// finality decisions first, which lowers the floor).
    #[test]
    fn reselect_starts_above_the_floor() {
        let (consensus, g) = genesis();
        let mut dag = new_dag(&consensus, &g);
        let before = btc_chain(&g, &[[7; 32], [11; 32]]);
        let after = btc_chain(&g, &[[7; 32], [0xee; 32]]);
        let a1 = block_anchored(&consensus, &dag, &[&g], 1, Some(11));
        dag.insert(&consensus, a1.clone(), &before);
        let a2 = block(&consensus, &dag, &[&a1], 2);
        dag.insert(&consensus, a2.clone(), &before);
        let b1 = block(&consensus, &dag, &[&g], 3);
        dag.insert(&consensus, b1.clone(), &before);
        dag.set_floor(a1.hash(), &before).unwrap();
        assert_eq!(dag.main(), &[g.hash(), a1.hash(), a2.hash()]);
        // only a2 is looked at, and b1 forks below the floor
        assert_eq!(dag.reselect(&after), Some((a2.hash(), 2)));
        assert_eq!(dag.main(), &[g.hash(), a1.hash()]);
        // with the floor back at the genesis block, a1 is looked at too and the main chain goes to b1 (lowering the
        // floor already switches to b1 when b1 outranks a1)
        dag.set_floor(g.hash(), &after).unwrap();
        dag.reselect(&after);
        assert_eq!(dag.main(), &[g.hash(), b1.hash()]);
    }

    /// D-104: the main chain is a function of the DAG and the BTC view alone. The same headers inserted in any order
    /// their parents allow (a header joins only after its parents, D-135), each under any of the BTC views and with
    /// `reselect` run at any point, end on the same main chain once `reselect` runs under the final view, whatever main
    /// chain they had on the way.
    #[test]
    fn the_main_chain_depends_only_on_the_dag_and_the_btc_view() {
        use proptest::prelude::*;
        use proptest::test_runner::{Config, TestRunner};

        let (consensus, g) = genesis();
        // BTC heights 10 to 12 as the a chain anchored them; a reorg replacing 11, and then 12 as well; a lagging view
        let views = [
            btc_chain(&g, &[[7; 32], [11; 32], [12; 32]]),
            btc_chain(&g, &[[7; 32], [0xee; 32]]),
            btc_chain(&g, &[[7; 32], [0xee; 32], [0xef; 32]]),
            btc_chain(&g, &[[7; 32]]),
        ];
        let on = |height: u32, hash: u8| {
            move |_: &BtcAnchor| BtcAnchor {
                height,
                hash: [hash; 32],
            }
        };
        // the a chain anchored at 11 and 12, the heavier b chain at the genesis anchor, m on b merging the a chain and
        // n on m, the e chain on b anchored on the reorg's block 11, and f on n anchored on its block 12
        let mut builder = new_dag(&consensus, &g);
        let mut headers = Vec::new();
        let mut add = |make: &dyn Fn(&HeaderDag) -> BlockHeader| {
            let header = make(&builder);
            assert_eq!(
                builder.insert(&consensus, header.clone(), &no_btc()),
                Insert::Connected(1)
            );
            headers.push(header.clone());
            header
        };
        let a1 = add(&|dag| block_at(&consensus, dag, &[&g], 1, on(11, 11)));
        let a2 = add(&|dag| block(&consensus, dag, &[&a1], 2));
        let a3 = add(&|dag| block_at(&consensus, dag, &[&a2], 3, on(12, 12)));
        let mut b = g.clone();
        for tag in 4..8 {
            b = add(&|dag| block(&consensus, dag, &[&b], tag));
        }
        let m = add(&|dag| block_at(&consensus, dag, &[&b, &a3], 8, on(12, 12)));
        let n = add(&|dag| block(&consensus, dag, &[&m], 9));
        let e1 = add(&|dag| block_at(&consensus, dag, &[&b], 10, on(11, 0xee)));
        let e2 = add(&|dag| block(&consensus, dag, &[&e1], 11));
        let f = add(&|dag| block_at(&consensus, dag, &[&n], 12, on(12, 0xef)));
        // the main chain under each view, from the headers in the order they were built
        let expected: Vec<Vec<Hash>> = views
            .iter()
            .map(|view| {
                let mut dag = new_dag(&consensus, &g);
                for header in &headers {
                    dag.insert(&consensus, header.clone(), view);
                }
                dag.reselect(view);
                dag.main().to_vec()
            })
            .collect();
        let tips: Vec<Hash> = expected
            .iter()
            .filter_map(|main| main.last().copied())
            .collect();
        assert_eq!(tips, vec![n.hash(), e2.hash(), e2.hash(), f.hash()]);

        let mut runner = TestRunner::new(Config::with_cases(64));
        let steps = proptest::collection::vec((0..views.len(), any::<bool>()), headers.len());
        runner
            .run(
                &(Just(headers.clone()).prop_shuffle(), steps, 0..views.len()),
                |(mut order, steps, last)| {
                    let mut dag = new_dag(&consensus, &g);
                    for (view, reselect) in steps {
                        // the first header in the shuffled order whose parents have joined
                        let next = order
                            .iter()
                            .position(|h| h.parents.iter().all(|p| dag.dag().contains(p)))
                            .unwrap();
                        let header = order.remove(next);
                        prop_assert_eq!(
                            dag.insert(&consensus, header, &views[view]),
                            Insert::Connected(1)
                        );
                        if reselect {
                            dag.reselect(&views[view]);
                        }
                    }
                    dag.reselect(&views[last]);
                    prop_assert_eq!(dag.main(), &expected[last][..]);
                    Ok(())
                },
            )
            .unwrap();
    }

    /// Pruning (D-097) keeps the pruning point, its future and the main chain from it up: the pruned DAG answers like
    /// the full one for those blocks and takes the same new blocks, does not add what is anchored below the pruning
    /// point or needs a pruned block, refuses a pruning point off the main chain or above the floor, and its checkpoint
    /// restores the same DAG.
    #[test]
    fn pruning_keeps_the_future_of_the_pruning_point() {
        // a small merge depth so that a short chain is deep enough (minimum pruning depth 3 × (4 + 2) + 1 = 19)
        let params = Params {
            ghostdag_k: 2,
            merge_depth: 4,
            ..DEVNET
        };
        let (consensus, g) = genesis();
        let mut dag = HeaderDag::new(&params, &consensus, g.clone()).unwrap();
        let anchor = |i: u8| Some(10 + u32::from(i) / 4);
        let add = |dag: &mut HeaderDag, header: &BlockHeader| {
            assert_eq!(
                dag.insert(&consensus, header.clone(), &no_btc()),
                Insert::Connected(1)
            );
        };
        // the chain a1..a40, one BTC block higher every 4 blocks; a side branch from a3; around a34 two parallel
        // blocks merged by a35
        let mut a = vec![g.clone()];
        for i in 1..=33u8 {
            let header = block_anchored(&consensus, &dag, &[a.last().unwrap()], i, anchor(i));
            add(&mut dag, &header);
            a.push(header);
        }
        let side = block(&consensus, &dag, &[&a[3]], 200);
        add(&mut dag, &side);
        let parallel = block_anchored(&consensus, &dag, &[&a[33]], 201, anchor(34));
        add(&mut dag, &parallel);
        let a34 = block_anchored(&consensus, &dag, &[&a[33]], 34, anchor(34));
        add(&mut dag, &a34);
        a.push(a34);
        let a35 = block_anchored(&consensus, &dag, &[&a[34], &parallel], 35, anchor(35));
        add(&mut dag, &a35);
        a.push(a35);
        for i in 36..=40u8 {
            let header = block_anchored(&consensus, &dag, &[a.last().unwrap()], i, anchor(i));
            add(&mut dag, &header);
            a.push(header);
        }
        dag.set_floor(a[30].hash(), &no_btc()).unwrap();
        let point = a[10].hash();
        assert_eq!(dag.header(&point).unwrap().btc_anchor.height, 12);
        let mut full = HeaderDag::from_record(&params, dag.record().unwrap()).unwrap();
        assert_eq!(full.record().unwrap(), dag.record().unwrap());

        // refused: off the main chain, above the floor
        let off_main = if dag.main_hash(34) == Some(&parallel.hash()) {
            a[34].hash()
        } else {
            parallel.hash()
        };
        assert!(dag.prune(&off_main).is_err());
        assert!(dag.prune(&a[31].hash()).is_err());
        assert!(dag.prune(&side.hash()).is_err());
        // genesis..a9 and the side branch
        assert_eq!(dag.prune(&point), Ok(11));
        assert_eq!(dag.dag().pruning_point(), Some(&point));
        assert_eq!(dag.main_base(), 10);
        assert_eq!(dag.main().first(), Some(&point));
        assert_eq!(dag.main_height(), full.main_height());
        for height in 0..=full.main_height() {
            let expected = full.main_hash(height).filter(|_| height >= 10);
            assert_eq!(dag.main_hash(height), expected, "height {height}");
        }
        for header in a.iter().chain([&side, &parallel]) {
            let hash = header.hash();
            let kept = header.height >= 10 && hash != side.hash();
            assert_eq!(dag.dag().contains(&hash), kept);
            assert_eq!(dag.is_final(&hash), kept && full.is_final(&hash));
            if kept {
                assert_eq!(dag.ghostdag(&hash), full.ghostdag(&hash));
            }
        }
        assert!(dag.holds(&point) && !dag.holds(&a[9].hash()) && !dag.holds(&side.hash()));
        // a pruned DAG prunes again only upwards along the main chain
        assert_eq!(dag.prune(&point), Ok(0));

        // new blocks: the same as in the full DAG
        let mut tip = a.last().unwrap().clone();
        for i in 41..=45u8 {
            let header = block_anchored(&consensus, &full, &[&tip], i, anchor(i));
            add(&mut full, &header);
            add(&mut dag, &header);
            assert_eq!(
                dag.main_hash(dag.main_height()),
                full.main_hash(full.main_height())
            );
            assert_eq!(dag.ghostdag(&header.hash()), full.ghostdag(&header.hash()));
            tip = header;
        }
        // anchored below the pruning point: dropped without a judgment; the full DAG takes it
        let low = block_anchored(&consensus, &full, &[&a[4]], 202, Some(11));
        assert_eq!(
            dag.insert(&consensus, low.clone(), &no_btc()),
            Insert::Pruned
        );
        add(&mut full, &low);
        // anchored at the pruning point's anchor on a pruned parent: not added either (D-135)
        let stray = block_anchored(&consensus, &full, &[&a[9]], 203, Some(12));
        assert_eq!(
            dag.insert(&consensus, stray.clone(), &no_btc()),
            Insert::MissingParent
        );
        add(&mut full, &stray);

        // the checkpoint of the pruned DAG restores it, and the restored DAG goes on the same way
        let record = dag.record().unwrap();
        let bytes = borsh::to_vec(&record).unwrap();
        let mut restored =
            HeaderDag::from_record(&params, borsh::from_slice(&bytes).unwrap()).unwrap();
        assert_eq!(restored.record().unwrap(), record);
        let next = block_anchored(&consensus, &dag, &[&tip], 46, anchor(46));
        add(&mut dag, &next);
        add(&mut restored, &next);
        assert_eq!(restored.record().unwrap(), dag.record().unwrap());
        dag.set_floor(a[40].hash(), &no_btc()).unwrap();
        assert_eq!(dag.prune(&a[20].hash()), Ok(10));
        assert_eq!(dag.main_base(), 20);
        assert!(dag.set_floor(a[15].hash(), &no_btc()).is_err());
    }
}
