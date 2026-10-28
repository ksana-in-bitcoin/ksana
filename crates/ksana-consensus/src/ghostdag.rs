//! GHOSTDAG (D-063): the block DAG, selected parent, merge set, k-cluster coloring, blue score and blue work.
//!
//! The definitions and the algorithm follow Kaspa (`consensus/src/processes/ghostdag/protocol.rs`):
//!
//! - `selected_parent(B)` = the parent with the greatest fork-choice key `(chain_work, hash)` (hashes compared byte by
//!   byte over 32 bytes), where `chain_work(P) = blue_work(P) + work(P)` counts the parent's own work as well (D-100:
//!   Kaspa compares `blue_work` alone, which leaves a block's own work out until a block is built on it; with
//!   two-speed difficulty an empty block weighs up to `MAX_EMPTY_BLOCK_WEIGHT` ordinary blocks, and a run of cheap
//!   weight-1 blocks would otherwise overtake it). The main chain is chosen by the same key;
//! - `mergeset(B)` = `past(B) \ past(SP)`, including SP itself; apart from SP, ordered by `(blue_work, hash)` ascending
//!   (a topological order: `blue_work`, like `chain_work`, strictly increases along every parent-child edge, because
//!   `blue_work(B) ≥ chain_work(SP) ≥ chain_work(P) > blue_work(P)` for every parent P);
//! - coloring: SP is blue; the rest are decided one by one in the above order, and a candidate c is blue if and only if
//!   the merge set does not yet have K + 1 blue blocks, the anticone of c contains at most K blue blocks, and no blue
//!   block in the anticone of c already has K blue blocks in its own anticone;
//! - `blue_score(B) = blue_score(SP) + |mergeset_blues|`, `blue_work(B) = blue_work(SP) + Σ work(mergeset_blues)`,
//!   both 0 for the genesis block; `daa_score(B) = daa_score(SP) + Σ_{m ∈ mergeset(B) \ {SP}} weight(m) + weight(B)`
//!   (two-speed difficulty, D-084: the weight of an empty block doubles from `EMPTY_BLOCK_DIFFICULTY` up to the cap
//!   according to the number of consecutive empty blocks on the selected chain (the empty-block streak, D-090), all
//!   others are 1; when all weights are 1 it equals `|past(B)|`), 0 for the genesis block. The streak is passed along
//!   the selected parent ([`BlockDag::parent_streak`]) and is computed by the caller together with the weight before
//!   insertion.
//!
//! An ancestor query (`x ∈ past(y)`) checks the merge sets block by block along y's selected chain:
//! `past(C) = mergeset(C) ∪ past(SP(C))`; when a chain block's `blue_work` is not greater than x's `blue_work`, x
//! cannot be in its past and the query stops. The cost of a query is proportional to the length of the selected chain
//! between x and y; the merge limits (merge set size, merge depth) guarantee that the queries during block validation
//! all stay nearby. For that the merge-depth root (D-097) and the merge depth of a direct parent are checked before its
//! ancestor query: a header may name any block as a parent, and its PoW is only checked after the GHOSTDAG computation.
//!
//! Merge limits (violating them makes the block invalid): 1-`max_block_parents` parents, pairwise distinct and none an
//! ancestor of another; the merge set (including SP) has at most `max_mergeset_size` blocks; every block in the merge
//! set other than SP has `blue_score + merge_depth ≥ blue_score(SP)`, and its selected chain passes through the
//! **merge-depth root** (D-097): the highest block on SP's selected chain whose blue score is at most
//! `blue_score(SP) − merge_depth` (the genesis block when there is none). The root rule bounds how far below the
//! selected chain a block's computation can reach, which is what makes pruning possible: a DAG holding only a pruning
//! point deep enough on the finalized chain and its future ([`BlockDag::prune`],
//! [`GhostdagParams::min_pruning_depth`]) computes every block a chain through the finalized blocks needs exactly as
//! the full DAG does.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::ops::Bound;

use borsh::{BorshDeserialize, BorshSerialize};
use ksana_primitives::Params;

use crate::U256;

/// Block hash.
pub type Hash = [u8; 32];

/// GHOSTDAG parameters (D-063).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GhostdagParams {
    /// K: the maximum number of blue blocks in a blue block's anticone.
    pub k: u16,
    /// The maximum number of parents of a non-genesis block.
    pub max_block_parents: usize,
    /// The maximum size of the merge set (including the selected parent).
    pub max_mergeset_size: usize,
    /// The merge depth (in blue score).
    pub merge_depth: u64,
}

impl GhostdagParams {
    /// Takes the GHOSTDAG parameters from the network params.
    pub fn from_params(params: &Params) -> Self {
        GhostdagParams {
            k: params.ghostdag_k,
            max_block_parents: params.max_block_parents,
            max_mergeset_size: params.max_mergeset_size,
            merge_depth: params.merge_depth,
        }
    }

    /// The least blue-score distance between a pruning point and a block F on whose selected chain it lies, such that
    /// a DAG pruned there after everything in F's past was inserted can still process every block that a later chain
    /// through F needs (D-097): `3 × (merge_depth + K) + 1`. Such a block's chain passes through F or through the
    /// merge-depth root of a chain block above F (at most `merge_depth + K` below F), and its own computation reaches
    /// at most `2 × (merge_depth + K)` further down that chain ([`BlockDag::prune`]).
    pub fn min_pruning_depth(&self) -> u64 {
        self.merge_depth
            .saturating_add(u64::from(self.k))
            .saturating_mul(3)
            .saturating_add(1)
    }
}

/// The GHOSTDAG data of a block (depends only on its past). The Borsh encoding is only the local format of DAG
/// checkpoints (D-097), not a consensus format.
#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct GhostdagData {
    /// The selected parent; `None` for the genesis block.
    pub selected_parent: Option<Hash>,
    /// Blue score.
    pub blue_score: u64,
    /// Blue work.
    pub blue_work: U256,
    /// DAA score (D-063, D-084): the selected parent's DAA score plus the weights of the blocks in the merge set other
    /// than the selected parent plus this block's weight; when all weights are 1 it equals `|past(B)|`.
    /// The difficulty adjustment, the protocol version (D-076) and the block reward (D-074) all go by it.
    pub daa_score: u64,
    /// The blue blocks in the merge set: the selected parent first, the rest in consensus order.
    pub mergeset_blues: Vec<Hash>,
    /// The red blocks in the merge set, in consensus order.
    pub mergeset_reds: Vec<Hash>,
    /// The blocks in the merge set other than the selected parent, in consensus order (`(blue_work, hash)` ascending,
    /// blue and red mixed): the execution order (D-063).
    pub mergeset_ordered: Vec<Hash>,
    /// The merge set (including the selected parent) sorted by hash, for membership queries.
    mergeset_by_hash: Vec<Hash>,
    /// The blue anticone sizes of the blue blocks in this block's blue set that are affected by this block's merge set
    /// (Kaspa's `blues_anticone_sizes`).
    blues_anticone_sizes: BTreeMap<Hash, u16>,
}

impl GhostdagData {
    /// The genesis block's data: no selected parent, score and work 0, empty merge set.
    pub fn genesis() -> Self {
        GhostdagData {
            selected_parent: None,
            blue_score: 0,
            blue_work: U256::ZERO,
            daa_score: 0,
            mergeset_blues: Vec::new(),
            mergeset_reds: Vec::new(),
            mergeset_ordered: Vec::new(),
            mergeset_by_hash: Vec::new(),
            blues_anticone_sizes: BTreeMap::new(),
        }
    }

    fn with_selected_parent(selected_parent: Hash) -> Self {
        GhostdagData {
            selected_parent: Some(selected_parent),
            mergeset_blues: vec![selected_parent],
            blues_anticone_sizes: BTreeMap::from([(selected_parent, 0)]),
            ..GhostdagData::genesis()
        }
    }

    /// Whether `block` is in this block's merge set (including the selected parent).
    pub fn mergeset_contains(&self, block: &Hash) -> bool {
        self.mergeset_by_hash.binary_search(block).is_ok()
    }

    /// The size of the merge set (including the selected parent).
    pub fn mergeset_len(&self) -> usize {
        self.mergeset_by_hash.len()
    }

    /// Whether `block` is a blue block in this block's merge set (including the selected parent).
    pub fn is_merged_blue(&self, block: &Hash) -> bool {
        self.mergeset_blues.contains(block)
    }
}

/// GHOSTDAG errors. Except for [`GhostdagError::UnknownParent`] (the parent has not arrived yet) and
/// [`GhostdagError::Corrupt`] (local data inconsistent), they all mean that combination of parents is invalid.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum GhostdagError {
    /// A non-genesis block has no parents.
    #[error("no parents")]
    NoParents,
    /// Too many parents.
    #[error("{count} parents, exceeds the limit {max}")]
    TooManyParents {
        /// The number of parents.
        count: usize,
        /// The limit.
        max: usize,
    },
    /// Duplicate parent.
    #[error("duplicate parent")]
    DuplicateParent,
    /// Unknown parent.
    #[error("unknown parent")]
    UnknownParent(Hash),
    /// A parent is in the past of another parent.
    #[error("parent is in the past of another parent")]
    ParentInPast(Hash),
    /// The merge set exceeds the limit.
    #[error("merge set exceeds the limit {max}")]
    MergesetTooLarge {
        /// The limit.
        max: usize,
    },
    /// Merged a block whose blue score is too low (beyond the merge depth). A parent other than the selected parent
    /// that passes the merge-depth root but is this deep is reported so before checking whether it is in the past of
    /// the selected parent.
    #[error("merged a block beyond the merge depth")]
    MergeDepth(Hash),
    /// Merged a block whose selected chain does not pass through the merge-depth root (D-097).
    #[error("merged a block whose selected chain does not pass through the merge-depth root")]
    MergeDepthRoot(Hash),
    /// The block already exists.
    #[error("block already exists")]
    AlreadyKnown,
    /// The computation needs this block, which has been pruned (D-097: only the pruning point and its future are kept
    /// in memory). This DAG cannot process the block; it is not a judgment that the block is invalid.
    #[error("needs a pruned block")]
    Pruned(Hash),
    /// Local data inconsistent (internal error).
    #[error("GHOSTDAG data inconsistent: {0}")]
    Corrupt(&'static str),
}

/// A block in the DAG.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DagBlock<T> {
    /// The parents (in the order given at insertion).
    pub parents: Vec<Hash>,
    /// The known children (in insertion order).
    pub children: Vec<Hash>,
    /// This block's work (D-025).
    pub work: U256,
    /// This block's weight (D-084, D-090): the DAA score increment and the number of block reward slots; an empty
    /// block's doubles with the streak from `EMPTY_BLOCK_DIFFICULTY` up to the cap, all others (including the genesis
    /// block) are 1.
    pub weight: u64,
    /// This block's empty-block streak (D-090): the number of consecutive empty blocks on the selected chain up to and
    /// including this block, 0 for a non-empty block (including the genesis block). It determines the weight of an
    /// empty block following this block.
    pub streak: u64,
    /// GHOSTDAG data.
    pub data: GhostdagData,
    /// Data attached by the caller (e.g. the block header).
    pub payload: T,
    /// The lowest block kept on this block's selected chain, which is kept from this block down to it: the genesis
    /// block until the DAG is pruned (D-097). Not in a checkpoint ([`BlockDag::restore`] derives it again).
    chain_base: Hash,
}

/// The block DAG: the inserted blocks with their GHOSTDAG data, plus the set of tips (blocks without children). A block
/// can only be inserted after all its parents. Blocks are removed only by [`BlockDag::prune`] (D-097), which keeps the
/// pruning point and its future.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlockDag<T> {
    params: GhostdagParams,
    genesis: Hash,
    blocks: BTreeMap<Hash, DagBlock<T>>,
    tips: BTreeSet<Hash>,
    /// Selected-chain tips: blocks that are not the selected parent of any block (the main-chain candidates of D-082).
    selected_tips: BTreeSet<Hash>,
    /// The selected-chain tips by fork-choice key `(chain_work, hash)` (D-100), so that the main-chain candidates can be
    /// visited from the heaviest down without scanning all of them.
    selected_tips_by_key: BTreeSet<(U256, Hash)>,
    /// All blocks by sort key `(blue_work, hash)` ascending (topological order), for fetching blocks in batches during
    /// sync.
    order: BTreeSet<(U256, Hash)>,
    /// The pruning point (D-097): `None` until the first [`BlockDag::prune`]; after it, the blocks kept are the
    /// pruning point and its future, and a block missing from the DAG that a computation reaches has been pruned.
    pruning_point: Option<Hash>,
}

/// The result of deciding a blue candidate: its blue anticone size, and the previous blue anticone size of each blue
/// block in its anticone.
type BlueCandidate = (u16, BTreeMap<Hash, u16>);

/// One block of a [`DagCheckpoint`]: the block as stored in the DAG, with its hash. The children are kept as recorded
/// (in insertion order); the tips and the indexes are rebuilt by [`BlockDag::restore`].
#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct StoredBlock<T> {
    /// The block hash.
    pub hash: Hash,
    /// The parents (some may have been pruned).
    pub parents: Vec<Hash>,
    /// The children in the DAG (all kept: the future of a kept block is kept).
    pub children: Vec<Hash>,
    /// This block's work.
    pub work: U256,
    /// This block's weight (D-084, D-090).
    pub weight: u64,
    /// This block's empty-block streak (D-090).
    pub streak: u64,
    /// GHOSTDAG data.
    pub data: GhostdagData,
    /// The attached data.
    pub payload: T,
}

/// A copy of a (usually pruned) DAG that [`BlockDag::restore`] turns back into the same DAG (D-097): the node and the
/// indexer write one at every pruning, so that a restart continues from the pruned DAG instead of rebuilding the whole
/// history. Local format, not part of consensus.
#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct DagCheckpoint<T> {
    /// The genesis block hash (the genesis block itself is gone once pruned).
    pub genesis: Hash,
    /// The pruning point; `None` when the DAG was never pruned.
    pub pruning_point: Option<Hash>,
    /// The blocks, by sort key `(blue_work, hash)` ascending (parents before children).
    pub blocks: Vec<StoredBlock<T>>,
}

/// The selected chain of a new block's selected parent down to the merge-depth root (D-097), walked lazily: from the
/// selected parent down only as far as the merged blocks' checks need, so that a block whose merged blocks meet the
/// chain near its top does not walk the merge depth. Also the verdicts of the merged blocks' walks, so that no block
/// is walked twice in one computation.
struct MergeRoot {
    /// The root's blue score bound: the root is the first block down the chain whose blue score is at most this.
    root_score: u64,
    /// The sort keys of the chain blocks walked so far, from the selected parent down (strictly decreasing).
    chain: Vec<(U256, Hash)>,
    /// Whether the last block of `chain` is the root (the walk stops there).
    reached: bool,
    /// Whether the selected chain of each block a walk of [`BlockDag::passes_root`] went through passes through the
    /// root (a block's chain continues as that walk did, so it has the walk's verdict).
    verdicts: BTreeMap<Hash, bool>,
}

/// The merge-depth root of new blocks on one selected parent (D-097), shared by their computations while the DAG does
/// not change (it borrows the DAG): the selected parent's chain and the merged blocks' chains are walked once for all
/// of them instead of once per computation. A block template tries one tip after another, each with the main-chain tip
/// as the selected parent (second audit, P4a-2).
pub struct MergeRootCache<'a, T> {
    dag: &'a BlockDag<T>,
    sp: Hash,
    root: MergeRoot,
}

impl<T> MergeRootCache<'_, T> {
    /// [`BlockDag::compute`], with the shared root when the selected parent of `parents` is this root's (any other
    /// selected parent gets its own root, as in [`BlockDag::compute`]). The result is the same either way.
    pub fn compute(
        &mut self,
        parents: &[Hash],
        weight: u64,
    ) -> Result<GhostdagData, GhostdagError> {
        self.dag
            .compute_with(parents, weight, Some((&self.sp, &mut self.root)))
    }
}

/// A chain block: the new block being computed (no hash yet) or an inserted block.
enum ChainBlock<'a> {
    New(&'a GhostdagData),
    Stored(Hash),
}

impl<T> BlockDag<T> {
    /// A DAG containing only the genesis block. The genesis block is recorded with weight 1, streak 0 and DAA score 0
    /// (D-084, D-090).
    pub fn new(params: GhostdagParams, genesis: Hash, genesis_work: U256, payload: T) -> Self {
        let block = DagBlock {
            parents: Vec::new(),
            children: Vec::new(),
            work: genesis_work,
            weight: 1,
            streak: 0,
            data: GhostdagData::genesis(),
            payload,
            chain_base: genesis,
        };
        BlockDag {
            params,
            genesis,
            blocks: BTreeMap::from([(genesis, block)]),
            tips: BTreeSet::from([genesis]),
            selected_tips: BTreeSet::from([genesis]),
            // the genesis block's chain work is its own work (its blue work is zero)
            selected_tips_by_key: BTreeSet::from([(genesis_work, genesis)]),
            order: BTreeSet::from([(U256::ZERO, genesis)]),
            pruning_point: None,
        }
    }

    /// The parameters.
    pub fn params(&self) -> &GhostdagParams {
        &self.params
    }

    /// The genesis block hash.
    pub fn genesis(&self) -> &Hash {
        &self.genesis
    }

    /// The pruning point (D-097); `None` when the DAG has never been pruned (it still holds every block from the
    /// genesis block on).
    pub fn pruning_point(&self) -> Option<&Hash> {
        self.pruning_point.as_ref()
    }

    /// The number of blocks kept (including the genesis block until it is pruned).
    pub fn len(&self) -> usize {
        self.blocks.len()
    }

    /// Always `false` (there is at least the genesis block, or the pruning point).
    pub fn is_empty(&self) -> bool {
        self.blocks.is_empty()
    }

    /// Whether the block has been inserted.
    pub fn contains(&self, hash: &Hash) -> bool {
        self.blocks.contains_key(hash)
    }

    /// An inserted block.
    pub fn get(&self, hash: &Hash) -> Option<&DagBlock<T>> {
        self.blocks.get(hash)
    }

    /// The GHOSTDAG data of an inserted block.
    pub fn data(&self, hash: &Hash) -> Option<&GhostdagData> {
        self.blocks.get(hash).map(|b| &b.data)
    }

    /// The data attached to an inserted block.
    pub fn payload(&self, hash: &Hash) -> Option<&T> {
        self.blocks.get(hash).map(|b| &b.payload)
    }

    /// The weight of an inserted block (D-084, D-090).
    pub fn weight(&self, hash: &Hash) -> Option<u64> {
        self.blocks.get(hash).map(|b| b.weight)
    }

    /// The empty-block streak of an inserted block (D-090).
    pub fn streak(&self, hash: &Hash) -> Option<u64> {
        self.blocks.get(hash).map(|b| b.streak)
    }

    /// The selected parent of a new block with parents `parents` (order irrelevant): the parent with the greatest
    /// fork-choice key `(chain_work, hash)` ([`BlockDag::chain_key`], D-100). Only checks that there are parents and
    /// that all are inserted; the remaining merge limits are checked by [`BlockDag::compute`].
    pub fn selected_parent(&self, parents: &[Hash]) -> Result<Hash, GhostdagError> {
        let mut selected: Option<(U256, Hash)> = None;
        for parent in parents {
            let block = self
                .blocks
                .get(parent)
                .ok_or(GhostdagError::UnknownParent(*parent))?;
            let key = (Self::chain_work_of(block)?, *parent);
            if selected.as_ref().is_none_or(|best| key > *best) {
                selected = Some(key);
            }
        }
        selected.map(|(_, sp)| sp).ok_or(GhostdagError::NoParents)
    }

    /// The empty-block streak of the selected parent of a new block with parents `parents` (D-090): it determines the
    /// new block's streak and weight ([`Consensus::streak`], [`Consensus::weight`]), after which
    /// [`BlockDag::compute`] can be called.
    pub fn parent_streak(&self, parents: &[Hash]) -> Result<u64, GhostdagError> {
        let sp = self.selected_parent(parents)?;
        Ok(self.block(&sp)?.streak)
    }

    /// The tips (blocks without children), sorted by hash.
    pub fn tips(&self) -> &BTreeSet<Hash> {
        &self.tips
    }

    /// Selected-chain tips: blocks that are not the selected parent of any block, sorted by hash. They include both
    /// the tips without children and the blocks that are only merged by other blocks (as a non-selected parent).
    /// The main-chain candidates are taken from here (D-082): when the successors of a finalized block are merged by a
    /// heavier chain, only their selected chains pass through the finalized block, and they are no longer tips.
    pub fn selected_chain_tips(&self) -> &BTreeSet<Hash> {
        &self.selected_tips
    }

    /// The selected-chain tips with fork-choice key `(chain_work, hash)` greater than `above` (all of them for `None`),
    /// from the largest key down. A main-chain candidate derived from a tip never has a larger key than the tip (chain
    /// work strictly increases along a selected chain), so a search for the best candidate can stop at the first tip
    /// not above the best candidate found so far.
    pub fn selected_chain_tips_desc(
        &self,
        above: Option<&(U256, Hash)>,
    ) -> impl Iterator<Item = &(U256, Hash)> {
        let range = match above {
            Some(k) => self
                .selected_tips_by_key
                .range((Bound::Excluded(*k), Bound::Unbounded)),
            None => self.selected_tips_by_key.range(..),
        };
        range.rev()
    }

    /// All blocks kept (sorted by hash).
    pub fn iter(&self) -> impl Iterator<Item = (&Hash, &DagBlock<T>)> {
        self.blocks.iter()
    }

    /// The error for a block missing from the DAG that a computation reached through a kept block (a parent or a
    /// selected parent): after pruning it has been pruned, before pruning the data is inconsistent.
    fn missing(&self, hash: &Hash) -> GhostdagError {
        if self.pruning_point.is_some() {
            GhostdagError::Pruned(*hash)
        } else {
            GhostdagError::Corrupt("missing block")
        }
    }

    fn block(&self, hash: &Hash) -> Result<&DagBlock<T>, GhostdagError> {
        self.blocks.get(hash).ok_or_else(|| self.missing(hash))
    }

    /// The sort key `(blue_work, hash)`: a topological order (parents before children). The merge set is in ascending
    /// order of it, and sync hands out blocks by it; the fork choice goes by [`BlockDag::chain_key`].
    pub fn sort_key(&self, hash: &Hash) -> Result<(U256, Hash), GhostdagError> {
        Ok((self.block(hash)?.data.blue_work, *hash))
    }

    /// The chain work of an inserted block (D-100): its blue work plus its own work, i.e. the work of its blue past
    /// together with the block itself.
    pub fn chain_work(&self, hash: &Hash) -> Result<U256, GhostdagError> {
        Self::chain_work_of(self.block(hash)?)
    }

    /// The fork-choice key `(chain_work, hash)` (D-100): a block's selected parent is the parent with the greatest key,
    /// and the main chain is the selected chain of the eligible tip with the greatest key. A block's own work counts at
    /// once, not only after a block is built on it.
    pub fn chain_key(&self, hash: &Hash) -> Result<(U256, Hash), GhostdagError> {
        Ok((self.chain_work(hash)?, *hash))
    }

    fn chain_work_of(block: &DagBlock<T>) -> Result<U256, GhostdagError> {
        block
            .data
            .blue_work
            .checked_add(block.work)
            .ok_or(GhostdagError::Corrupt("chain work overflow"))
    }

    /// `x ∈ past(y)` (`x ≠ y`). Both must be in the DAG (the genesis block as `x` may already be pruned).
    ///
    /// After pruning (D-097) the walk may reach a pruned block on y's selected chain: then the answer is `false`, which
    /// is exact because x is kept, so x is the pruning point or in its future, while a pruned block is not; a block in
    /// the past of a pruned block would make that block a descendant of x and so a kept block.
    pub fn in_past(&self, x: &Hash, y: &Hash) -> Result<bool, GhostdagError> {
        if x == y {
            return Ok(false);
        }
        if *x == self.genesis {
            // the genesis block is in the past of every other block
            return self.block(y).map(|_| true);
        }
        let x_work = self.block(x)?.data.blue_work;
        let mut current = self.block(y)?;
        loop {
            #[cfg(test)]
            probe::QUERY_STEPS.with(|steps| steps.set(steps.get().saturating_add(1)));
            let d = &current.data;
            // the blocks in past(C) all have a smaller blue_work than C
            if d.blue_work <= x_work {
                return Ok(false);
            }
            if d.mergeset_contains(x) {
                return Ok(true);
            }
            let Some(sp) = d.selected_parent else {
                return Ok(false);
            };
            current = match self.blocks.get(&sp) {
                Some(block) => block,
                None if self.pruning_point.is_some() => return Ok(false),
                None => return Err(GhostdagError::Corrupt("missing block")),
            };
        }
    }

    /// `x ∈ past(y) ∪ {y}`.
    pub fn in_closed_past(&self, x: &Hash, y: &Hash) -> Result<bool, GhostdagError> {
        Ok(x == y || self.in_past(x, y)?)
    }

    /// The blocks with sort key greater than `key` (all blocks for `None`), by sort key ascending (topological order:
    /// parents always before children).
    pub fn after(&self, key: Option<&(U256, Hash)>) -> impl Iterator<Item = &Hash> {
        let range = match key {
            Some(k) => self.order.range((Bound::Excluded(*k), Bound::Unbounded)),
            None => self.order.range(..),
        };
        range.map(|(_, hash)| hash)
    }

    /// The sort key of the first block, going back from `from` along the selected chain, whose blue score does not
    /// exceed `blue_score(from) − depth` (the genesis block when the chain is not deep enough).
    pub fn chain_floor(&self, from: &Hash, depth: u64) -> Result<(U256, Hash), GhostdagError> {
        let mut current = *from;
        let target = self.block(from)?.data.blue_score.saturating_sub(depth);
        loop {
            let data = &self.block(&current)?.data;
            match data.selected_parent {
                Some(sp) if data.blue_score > target => current = sp,
                _ => return Ok((data.blue_work, current)),
            }
        }
    }

    /// The blocks in `past(of) ∪ {of}` with sort key greater than `floor` (searched backwards from `of` along the
    /// parents, stopping where the key is not greater than `floor`).
    pub fn closed_past_above(
        &self,
        of: &Hash,
        floor: &(U256, Hash),
    ) -> Result<BTreeSet<Hash>, GhostdagError> {
        let mut found = BTreeSet::new();
        if self.sort_key(of)? <= *floor {
            return Ok(found);
        }
        found.insert(*of);
        let mut queue = VecDeque::from([*of]);
        while let Some(current) = queue.pop_front() {
            for parent in &self.block(&current)?.parents {
                if found.contains(parent) {
                    continue;
                }
                let Some(block) = self.blocks.get(parent) else {
                    // pruned (D-097): a kept block in its past would make it a kept block, so nothing kept is lost
                    if self.pruning_point.is_some() {
                        continue;
                    }
                    return Err(GhostdagError::Corrupt("missing block"));
                };
                if (block.data.blue_work, *parent) > *floor {
                    found.insert(*parent);
                    queue.push_back(*parent);
                }
            }
        }
        Ok(found)
    }

    /// The GHOSTDAG data of a new block with parents `parents` (order irrelevant) and weight `weight` (D-084, D-090,
    /// computed from [`BlockDag::parent_streak`]), checking the merge limits. Does not modify the DAG. The weight only
    /// enters `daa_score`: the selected parent, the merge set, the coloring and the blue work are independent of it.
    pub fn compute(&self, parents: &[Hash], weight: u64) -> Result<GhostdagData, GhostdagError> {
        self.compute_with(parents, weight, None)
    }

    /// The merge-depth root of new blocks with selected parent `sp`, for computing several of them with
    /// [`MergeRootCache::compute`]. Fails as their [`BlockDag::compute`] would when the chain of `sp` is not kept deep
    /// enough after pruning ([`GhostdagError::Pruned`]).
    pub fn merge_root_cache(&self, sp: &Hash) -> Result<MergeRootCache<'_, T>, GhostdagError> {
        Ok(MergeRootCache {
            dag: self,
            sp: *sp,
            root: self.merge_root(sp)?,
        })
    }

    /// [`BlockDag::compute`] with the merge-depth root `shared` of the selected parent it names, when that is the
    /// selected parent of `parents` (the root and the verdicts of the walks depend only on the selected parent and the
    /// DAG).
    fn compute_with(
        &self,
        parents: &[Hash],
        weight: u64,
        shared: Option<(&Hash, &mut MergeRoot)>,
    ) -> Result<GhostdagData, GhostdagError> {
        if parents.is_empty() {
            return Err(GhostdagError::NoParents);
        }
        if parents.len() > self.params.max_block_parents {
            return Err(GhostdagError::TooManyParents {
                count: parents.len(),
                max: self.params.max_block_parents,
            });
        }
        let distinct: BTreeSet<Hash> = parents.iter().copied().collect();
        if distinct.len() != parents.len() {
            return Err(GhostdagError::DuplicateParent);
        }
        let sp = self.selected_parent(parents)?;
        let mut own: MergeRoot;
        let root = match shared {
            Some((shared_sp, root)) if *shared_sp == sp => root,
            _ => {
                own = self.merge_root(&sp)?;
                &mut own
            }
        };
        let ordered = self.ordered_mergeset(&sp, &distinct, root)?;

        let mut data = GhostdagData::with_selected_parent(sp);
        for candidate in &ordered {
            match self.check_blue_candidate(&data, candidate)? {
                Some((anticone_size, affected)) => {
                    data.mergeset_blues.push(*candidate);
                    data.blues_anticone_sizes.insert(*candidate, anticone_size);
                    for (blue, size) in affected {
                        let size = size
                            .checked_add(1)
                            .ok_or(GhostdagError::Corrupt("anticone size overflow"))?;
                        data.blues_anticone_sizes.insert(blue, size);
                    }
                }
                None => data.mergeset_reds.push(*candidate),
            }
        }

        let sp_block = self.block(&sp)?;
        let added_score = u64::try_from(data.mergeset_blues.len())
            .map_err(|_| GhostdagError::Corrupt("blue block count overflow"))?;
        data.blue_score = sp_block
            .data
            .blue_score
            .checked_add(added_score)
            .ok_or(GhostdagError::Corrupt("blue score overflow"))?;
        let mut blue_work = sp_block.data.blue_work;
        for blue in &data.mergeset_blues {
            blue_work = blue_work
                .checked_add(self.block(blue)?.work)
                .ok_or(GhostdagError::Corrupt("blue work overflow"))?;
        }
        data.blue_work = blue_work;
        // DAA score: the selected parent's score plus the weights of the blocks in the merge set (blue and red alike),
        // plus this block's weight (D-084)
        let mut daa_score = sp_block
            .data
            .daa_score
            .checked_add(weight)
            .ok_or(GhostdagError::Corrupt("daa_score overflow"))?;
        for merged in &ordered {
            daa_score = daa_score
                .checked_add(self.block(merged)?.weight)
                .ok_or(GhostdagError::Corrupt("daa_score overflow"))?;
        }
        data.daa_score = daa_score;
        let mut by_hash = ordered.clone();
        by_hash.push(sp);
        by_hash.sort_unstable();
        data.mergeset_by_hash = by_hash;
        data.mergeset_ordered = ordered;
        Ok(data)
    }

    /// Inserts a block (all parents must be inserted) and returns its GHOSTDAG data. The order of the parents is kept
    /// as is and does not affect the computation; `weight` and `streak` are this block's weight and empty-block streak
    /// (D-084, D-090, [`Consensus::weight`] / [`Consensus::streak`], from [`BlockDag::parent_streak`]).
    pub fn insert(
        &mut self,
        hash: Hash,
        parents: Vec<Hash>,
        work: U256,
        weight: u64,
        streak: u64,
        payload: T,
    ) -> Result<&GhostdagData, GhostdagError> {
        if self.blocks.contains_key(&hash) {
            return Err(GhostdagError::AlreadyKnown);
        }
        let data = self.compute(&parents, weight)?;
        let chain_work = data
            .blue_work
            .checked_add(work)
            .ok_or(GhostdagError::Corrupt("chain work overflow"))?;
        let (sp_key, chain_base) = match &data.selected_parent {
            Some(sp) => (Some(self.chain_key(sp)?), self.block(sp)?.chain_base),
            None => (None, hash),
        };
        for parent in &parents {
            self.tips.remove(parent);
            if let Some(p) = self.blocks.get_mut(parent) {
                p.children.push(hash);
            }
        }
        self.tips.insert(hash);
        if let Some(key) = sp_key {
            if self.selected_tips.remove(&key.1) {
                self.selected_tips_by_key.remove(&key);
            }
        }
        self.selected_tips.insert(hash);
        self.selected_tips_by_key.insert((chain_work, hash));
        self.order.insert((data.blue_work, hash));
        let block = self.blocks.entry(hash).or_insert(DagBlock {
            parents,
            children: Vec::new(),
            work,
            weight,
            streak,
            data,
            payload,
            chain_base,
        });
        Ok(&block.data)
    }

    /// Prunes the DAG at `point` (D-097): keeps `point` and its future (the blocks that have it in their past), drops
    /// every other block, and returns the number of blocks dropped. The kept set is closed under taking descendants,
    /// which is what keeps the ancestor queries among kept blocks exact ([`BlockDag::in_past`]). `point` must be in
    /// the DAG; the caller picks it deep enough on the finalized chain that no block it will still need reaches a
    /// pruned block (otherwise that computation fails with [`GhostdagError::Pruned`]).
    pub fn prune(&mut self, point: &Hash) -> Result<usize, GhostdagError> {
        self.block(point)?;
        let mut keep = BTreeSet::from([*point]);
        let mut queue = VecDeque::from([*point]);
        while let Some(current) = queue.pop_front() {
            for child in &self.block(&current)?.children {
                if keep.insert(*child) {
                    queue.push_back(*child);
                }
            }
        }
        let before = self.blocks.len();
        self.blocks.retain(|hash, _| keep.contains(hash));
        self.tips.retain(|hash| keep.contains(hash));
        self.selected_tips.retain(|hash| keep.contains(hash));
        self.selected_tips_by_key
            .retain(|(_, hash)| keep.contains(hash));
        self.order.retain(|(_, hash)| keep.contains(hash));
        self.pruning_point = Some(*point);
        Self::set_chain_bases(&mut self.blocks, &self.order)?;
        Ok(before.saturating_sub(self.blocks.len()))
    }

    /// Sets the chain base of every block (the lowest block kept on its selected chain), in topological order: the
    /// selected parent's chain base when the selected parent is kept, otherwise the block itself.
    fn set_chain_bases(
        blocks: &mut BTreeMap<Hash, DagBlock<T>>,
        order: &BTreeSet<(U256, Hash)>,
    ) -> Result<(), GhostdagError> {
        for (_, hash) in order {
            let selected_parent = blocks
                .get(hash)
                .ok_or(GhostdagError::Corrupt("missing block"))?
                .data
                .selected_parent;
            let base = match selected_parent.and_then(|sp| blocks.get(&sp)) {
                Some(sp) => sp.chain_base,
                None => *hash,
            };
            if let Some(block) = blocks.get_mut(hash) {
                block.chain_base = base;
            }
        }
        Ok(())
    }

    /// A copy of this DAG for [`BlockDag::restore`] (D-097).
    pub fn checkpoint(&self) -> Result<DagCheckpoint<T>, GhostdagError>
    where
        T: Clone,
    {
        let mut blocks = Vec::with_capacity(self.blocks.len());
        for (_, hash) in &self.order {
            let block = self.block(hash)?;
            blocks.push(StoredBlock {
                hash: *hash,
                parents: block.parents.clone(),
                children: block.children.clone(),
                work: block.work,
                weight: block.weight,
                streak: block.streak,
                data: block.data.clone(),
                payload: block.payload.clone(),
            });
        }
        Ok(DagCheckpoint {
            genesis: self.genesis,
            pruning_point: self.pruning_point,
            blocks,
        })
    }

    /// Rebuilds the DAG a [`BlockDag::checkpoint`] was taken from (D-097): the blocks with their recorded data, the
    /// tips (blocks without children), the selected-chain tips (blocks that are no kept block's selected parent) and
    /// the indexes. The checkpoint must be consistent: every child is in it, the genesis block or the pruning point is
    /// in it, and an unpruned checkpoint has every parent; otherwise [`GhostdagError::Corrupt`].
    pub fn restore(
        params: GhostdagParams,
        checkpoint: DagCheckpoint<T>,
    ) -> Result<Self, GhostdagError> {
        let DagCheckpoint {
            genesis,
            pruning_point,
            blocks: stored,
        } = checkpoint;
        let mut blocks = BTreeMap::new();
        for b in stored {
            let block = DagBlock {
                parents: b.parents,
                children: b.children,
                work: b.work,
                weight: b.weight,
                streak: b.streak,
                data: b.data,
                payload: b.payload,
                chain_base: b.hash,
            };
            if blocks.insert(b.hash, block).is_some() {
                return Err(GhostdagError::Corrupt("duplicate block in the checkpoint"));
            }
        }
        let root = pruning_point.unwrap_or(genesis);
        if !blocks.contains_key(&root) {
            return Err(GhostdagError::Corrupt(
                "the checkpoint lacks the genesis block or the pruning point",
            ));
        }
        let mut tips = BTreeSet::new();
        let mut selected_tips: BTreeSet<Hash> = blocks.keys().copied().collect();
        let mut order = BTreeSet::new();
        for (hash, block) in &blocks {
            if block.children.iter().any(|c| !blocks.contains_key(c)) {
                return Err(GhostdagError::Corrupt(
                    "a child is missing from the checkpoint",
                ));
            }
            if pruning_point.is_none() && block.parents.iter().any(|p| !blocks.contains_key(p)) {
                return Err(GhostdagError::Corrupt(
                    "a parent is missing from the checkpoint",
                ));
            }
            if block.children.is_empty() {
                tips.insert(*hash);
            }
            if let Some(sp) = &block.data.selected_parent {
                selected_tips.remove(sp);
            }
            order.insert((block.data.blue_work, *hash));
        }
        let mut selected_tips_by_key = BTreeSet::new();
        for hash in &selected_tips {
            let block = blocks
                .get(hash)
                .ok_or(GhostdagError::Corrupt("missing block"))?;
            selected_tips_by_key.insert((Self::chain_work_of(block)?, *hash));
        }
        Self::set_chain_bases(&mut blocks, &order)?;
        Ok(BlockDag {
            params,
            genesis,
            blocks,
            tips,
            selected_tips,
            selected_tips_by_key,
            order,
            pruning_point,
        })
    }

    /// The merge-depth root of a new block with selected parent `sp` (D-097): the first block down the selected chain
    /// of `sp` whose blue score is at most `blue_score(sp) − merge_depth` (the genesis block at the latest), found
    /// lazily by [`BlockDag::passes_root`]. After pruning, first checks that the selected chain of `sp` is kept down to
    /// blue score `blue_score(sp) − 2 × (merge_depth + K)`: every block the computation can reach is then kept (see
    /// [`BlockDag::prune`]); otherwise the block cannot be processed here ([`GhostdagError::Pruned`], naming the
    /// first pruned block of that chain, as [`BlockDag::chain_floor`] does).
    fn merge_root(&self, sp: &Hash) -> Result<MergeRoot, GhostdagError> {
        let depth = self.params.merge_depth;
        let sp_block = self.block(sp)?;
        let score = sp_block.data.blue_score;
        if self.pruning_point.is_some() {
            let k = u64::from(self.params.k);
            let floor = score.saturating_sub(depth.saturating_add(k).saturating_mul(2));
            // the chain is kept down to its chain base, below which it ends (the genesis block) or is pruned
            let base = &self.block(&sp_block.chain_base)?.data;
            if base.blue_score > floor {
                if let Some(pruned) = &base.selected_parent {
                    return Err(self.missing(pruned));
                }
            }
        }
        let root_score = score.saturating_sub(depth);
        Ok(MergeRoot {
            root_score,
            chain: vec![(sp_block.data.blue_work, *sp)],
            reached: score <= root_score || sp_block.data.selected_parent.is_none(),
            verdicts: BTreeMap::new(),
        })
    }

    /// Walks the selected chain of the new block's selected parent down until its last block has a sort key not
    /// greater than `key`, or is the merge-depth root.
    fn extend_chain(&self, root: &mut MergeRoot, key: &(U256, Hash)) -> Result<(), GhostdagError> {
        while !root.reached {
            let Some((_, last)) = root.chain.last().filter(|last| *last > key) else {
                break;
            };
            let next = self
                .block(last)?
                .data
                .selected_parent
                .ok_or(GhostdagError::Corrupt(
                    "the merge-depth root walk passed the genesis block",
                ))?;
            #[cfg(test)]
            probe::ROOT_STEPS.with(|steps| steps.set(steps.get().saturating_add(1)));
            let data = &self.block(&next)?.data;
            root.chain.push((data.blue_work, next));
            root.reached = data.blue_score <= root.root_score || data.selected_parent.is_none();
        }
        Ok(())
    }

    /// Whether the selected chain of `block` passes through the merge-depth root (D-097): walks it down until it meets
    /// the selected chain of the new block's selected parent above the root (or the root itself), or falls below the
    /// root's sort key (the sort key strictly decreases along a selected chain, so the root can no longer be below).
    /// The selected parent's chain is walked along only as far as the walk of `block` has come down (two selected
    /// chains that meet coincide from there on). A pruned block on the walk means the chain does not pass through the
    /// root: every block above the root on a chain through it is in the root's future, so it is kept.
    ///
    /// A walk stops at a block an earlier walk of the same computation went through, and every block it goes through
    /// takes its verdict: the walks of one computation visit each block once in all. Blue work, not blue score, bounds
    /// a walk, so a long chain of light blocks above the root costs its length once rather than once per merged block
    /// (second audit, P1-1).
    fn passes_root(&self, block: &Hash, root: &mut MergeRoot) -> Result<bool, GhostdagError> {
        let mut walked = Vec::new();
        let mut current = *block;
        let verdict = loop {
            if let Some(verdict) = root.verdicts.get(&current) {
                break *verdict;
            }
            let Some(found) = self.blocks.get(&current) else {
                if self.pruning_point.is_some() {
                    break false;
                }
                return Err(GhostdagError::Corrupt("missing block"));
            };
            #[cfg(test)]
            probe::WALK_STEPS.with(|steps| steps.set(steps.get().saturating_add(1)));
            let key = (found.data.blue_work, current);
            self.extend_chain(root, &key)?;
            // the chain now holds every block of the selected parent's chain down to `key` or to the root
            if root.chain.binary_search_by(|probe| key.cmp(probe)).is_ok() {
                break true;
            }
            if root.reached && root.chain.last().is_some_and(|last| *last > key) {
                break false;
            }
            walked.push(current);
            match found.data.selected_parent {
                Some(parent) => current = parent,
                None => break false,
            }
        };
        for hash in walked {
            root.verdicts.insert(hash, verdict);
        }
        Ok(verdict)
    }

    /// The merge set other than the selected parent, in consensus order; also checks that no parent is an ancestor of
    /// another, the merge set size, the merge depth and the merge-depth root (D-097).
    ///
    /// A direct parent is checked against the merge-depth root first (its walk stops at the root's sort key, and a
    /// parent that passes it keeps every later walk above the root), then against the merge depth, and only then
    /// queried as an ancestor of SP (M26): a header can name any block as a parent, its PoW only shows that it meets
    /// its own `bits` (the easiest are cheap, and `bits` is checked after this computation), and the query walks down
    /// the selected chain of SP as far as the parent's blue work. A parent beyond the merge depth makes the block
    /// invalid either way (`ParentInPast` when it is in the past of SP, `MergeDepth` when it is not), so checking the
    /// depth before the query accepts and rejects exactly the same parents and only reports such a parent as
    /// `MergeDepth`.
    fn ordered_mergeset(
        &self,
        sp: &Hash,
        parents: &BTreeSet<Hash>,
        root: &mut MergeRoot,
    ) -> Result<Vec<Hash>, GhostdagError> {
        let sp_score = self.block(sp)?.data.blue_score;
        let max = self.params.max_mergeset_size;
        let mut mergeset = BTreeSet::new();
        let mut queue = VecDeque::new();
        for parent in parents.iter().filter(|p| *p != sp) {
            // the merge-depth root first: it also bounds the ancestor query below to the chain above the root
            if !self.passes_root(parent, root)? {
                return Err(GhostdagError::MergeDepthRoot(*parent));
            }
            if Self::parent_depth_first() && self.beyond_merge_depth(parent, sp_score)? {
                return Err(GhostdagError::MergeDepth(*parent));
            }
            if self.in_past(parent, sp)? {
                return Err(GhostdagError::ParentInPast(*parent));
            }
            self.admit(parent, sp_score, &mut mergeset, max)?;
            queue.push_back(*parent);
        }
        let mut sp_past = BTreeSet::new();
        while let Some(current) = queue.pop_front() {
            for parent in &self.block(&current)?.parents {
                if mergeset.contains(parent) {
                    if parents.contains(parent) {
                        // a direct parent is in the past of another direct parent
                        return Err(GhostdagError::ParentInPast(*parent));
                    }
                    continue;
                }
                if sp_past.contains(parent) {
                    continue;
                }
                if !self.blocks.contains_key(parent) && self.pruning_point.is_some() {
                    // pruned (D-097): `current` passed the merge-depth root and the chain of the selected parent is
                    // kept deep enough (`merge_root`), so a pruned parent of `current` is below the root on its
                    // selected chain, in the past of the selected parent
                    sp_past.insert(*parent);
                    continue;
                }
                if self.in_closed_past(parent, sp)? {
                    sp_past.insert(*parent);
                    continue;
                }
                if !self.passes_root(parent, root)? {
                    return Err(GhostdagError::MergeDepthRoot(*parent));
                }
                self.admit(parent, sp_score, &mut mergeset, max)?;
                queue.push_back(*parent);
            }
        }
        let mut keyed = Vec::with_capacity(mergeset.len());
        for hash in &mergeset {
            keyed.push(self.sort_key(hash)?);
        }
        keyed.sort_unstable();
        Ok(keyed.into_iter().map(|(_, hash)| hash).collect())
    }

    /// Adds `block` to the merge set (without the selected parent), checking the merge depth and the size limit.
    fn admit(
        &self,
        block: &Hash,
        sp_score: u64,
        mergeset: &mut BTreeSet<Hash>,
        max: usize,
    ) -> Result<(), GhostdagError> {
        if self.beyond_merge_depth(block, sp_score)? {
            return Err(GhostdagError::MergeDepth(*block));
        }
        mergeset.insert(*block);
        // plus the selected parent itself
        if mergeset.len().saturating_add(1) > max {
            return Err(GhostdagError::MergesetTooLarge { max });
        }
        Ok(())
    }

    /// Whether `block` is beyond the merge depth of a selected parent with blue score `sp_score`: a merged block needs
    /// `blue_score + merge_depth ≥ blue_score(SP)`.
    fn beyond_merge_depth(&self, block: &Hash, sp_score: u64) -> Result<bool, GhostdagError> {
        let score = self.block(block)?.data.blue_score;
        Ok(score.saturating_add(self.params.merge_depth) < sp_score)
    }

    /// Whether [`BlockDag::compute`] checks the merge depth of a direct parent before its ancestor query: always,
    /// except in the tests that compare with the order before the check was moved forward.
    #[cfg(not(test))]
    fn parent_depth_first() -> bool {
        true
    }

    #[cfg(test)]
    fn parent_depth_first() -> bool {
        !probe::QUERY_FIRST.with(std::cell::Cell::get)
    }

    /// Decides the color of a candidate against the new block's current blue set: when blue, returns its blue anticone
    /// size and the previous blue anticone size of each blue block in its anticone; when red, returns `None`.
    fn check_blue_candidate(
        &self,
        new: &GhostdagData,
        candidate: &Hash,
    ) -> Result<Option<BlueCandidate>, GhostdagError> {
        let k = self.params.k;
        if new.mergeset_blues.len() == usize::from(k).saturating_add(1) {
            return Ok(None);
        }
        let mut affected = BTreeMap::new();
        let mut anticone_size: u16 = 0;
        let mut chain = ChainBlock::New(new);
        loop {
            let data = match chain {
                ChainBlock::New(data) => data,
                ChainBlock::Stored(hash) => {
                    // the candidate is in the future of this chain block: the remaining blue blocks are all in its
                    // past, so the anticone grows no further
                    if self.in_past(&hash, candidate)? {
                        return Ok(Some((anticone_size, affected)));
                    }
                    &self.block(&hash)?.data
                }
            };
            for blue in &data.mergeset_blues {
                if self.in_past(blue, candidate)? {
                    continue;
                }
                let size = self.blue_anticone_size(blue, new)?;
                affected.insert(*blue, size);
                anticone_size = anticone_size
                    .checked_add(1)
                    .ok_or(GhostdagError::Corrupt("anticone size overflow"))?;
                if anticone_size > k || size >= k {
                    return Ok(None);
                }
            }
            let sp = data.selected_parent.ok_or(GhostdagError::Corrupt(
                "coloring went past the genesis block",
            ))?;
            chain = ChainBlock::Stored(sp);
        }
    }

    /// The blue anticone size of blue block `block` in the blue set of inserted block `context` (the blue blocks in the
    /// past of `context`).
    pub fn blue_anticone_size_in(
        &self,
        block: &Hash,
        context: &Hash,
    ) -> Result<u16, GhostdagError> {
        self.blue_anticone_size(block, &self.block(context)?.data)
    }

    /// The blue anticone size of blue block `block` in the blue set of `context`: finds the first block along the
    /// selected chain of `context` that recorded it.
    fn blue_anticone_size(
        &self,
        block: &Hash,
        context: &GhostdagData,
    ) -> Result<u16, GhostdagError> {
        let mut current = context;
        loop {
            if let Some(size) = current.blues_anticone_sizes.get(block) {
                return Ok(*size);
            }
            let sp = current.selected_parent.ok_or(GhostdagError::Corrupt(
                "block is not in the context's blue set",
            ))?;
            current = &self.block(&sp)?.data;
        }
    }
}

/// Test hooks, per thread.
#[cfg(test)]
mod probe {
    use std::cell::Cell;

    std::thread_local! {
        /// The number of selected-chain blocks visited by ancestor queries ([`super::BlockDag::in_past`]).
        pub(super) static QUERY_STEPS: Cell<u64> = const { Cell::new(0) };
        /// The number of blocks the merge-depth root walks added below the selected parent
        /// ([`super::BlockDag::passes_root`]).
        pub(super) static ROOT_STEPS: Cell<u64> = const { Cell::new(0) };
        /// The number of blocks the merge-depth root walks of merged blocks went through
        /// ([`super::BlockDag::passes_root`]).
        pub(super) static WALK_STEPS: Cell<u64> = const { Cell::new(0) };
        /// Restores the order before the fix: the ancestor query of a direct parent before its merge depth (the
        /// merge-depth root, D-097, comes first in both orders).
        pub(super) static QUERY_FIRST: Cell<bool> = const { Cell::new(false) };
    }
}

#[cfg(test)]
#[allow(clippy::cast_possible_truncation)] // test code: indices drawn from u64 random numbers
mod tests {
    use super::*;
    use ksana_primitives::hash::sha256;
    use ksana_primitives::DEVNET;
    use std::cell::Cell;

    fn id(i: u64) -> Hash {
        sha256(&i.to_le_bytes())
    }

    /// Runs `f` and returns its result together with the number of selected-chain blocks the ancestor queries visited.
    fn steps<R>(f: impl FnOnce() -> R) -> (R, u64) {
        probe::QUERY_STEPS.with(|s| s.set(0));
        let result = f();
        (result, probe::QUERY_STEPS.with(Cell::get))
    }

    /// Runs `f` and returns its result together with the number of blocks the merge-depth root walks added.
    fn root_steps<R>(f: impl FnOnce() -> R) -> (R, u64) {
        probe::ROOT_STEPS.with(|s| s.set(0));
        let result = f();
        (result, probe::ROOT_STEPS.with(Cell::get))
    }

    /// Runs `f` and returns its result together with the number of blocks the merge-depth root walks of merged blocks
    /// went through.
    fn walk_steps<R>(f: impl FnOnce() -> R) -> (R, u64) {
        probe::WALK_STEPS.with(|s| s.set(0));
        let result = f();
        (result, probe::WALK_STEPS.with(Cell::get))
    }

    /// Runs `f` with the order before the fix: the ancestor query of a direct parent before its merge depth.
    fn query_first<R>(f: impl FnOnce() -> R) -> R {
        probe::QUERY_FIRST.with(|q| q.set(true));
        let result = f();
        probe::QUERY_FIRST.with(|q| q.set(false));
        result
    }

    /// The audit finding (M26): a header naming the tip of a long chain and a block near its start as parents. Before
    /// the fix the ancestor query of the deep parent walked the whole chain, and `compute` runs before the header's
    /// PoW is checked. The merge-depth root (D-097), checked first in both orders, now rejects such a parent without a
    /// walk, and every parent within the merge depth of this chain costs at most `merge_depth` chain steps.
    #[test]
    fn a_parent_beyond_the_merge_depth_is_rejected_without_walking_the_chain() {
        const N: u64 = 50_000;
        let params = GhostdagParams::from_params(&DEVNET);
        let depth = params.merge_depth;
        let genesis = id(0);
        let mut dag = BlockDag::new(params, genesis, U256::ONE, ());
        let mut tip = genesis;
        for i in 1..=N {
            dag.insert(id(i), vec![tip], U256::ONE, 1, 0, ()).unwrap();
            tip = id(i);
        }
        // a block on the genesis block: beyond the merge depth and not in the past of the tip
        let side = id(N + 1);
        dag.insert(side, vec![genesis], U256::ONE, 1, 0, ())
            .unwrap();

        // block 1 is in the past of the tip, the side block is not: both are below the merge-depth root, rejected
        // without a walk in either order (before M26 and D-097, the first was ParentInPast after walking the chain down
        // to block 2, the second MergeDepth after walking it down to block 1)
        for parent in [id(1), side] {
            let expected = (Err(GhostdagError::MergeDepthRoot(parent)), 0);
            let before = query_first(|| steps(|| dag.compute(&[tip, parent], 1)));
            assert_eq!(before, expected);
            assert_eq!(steps(|| dag.compute(&[tip, parent], 1)), expected);
        }

        // every block of the chain as the second parent: the boundary is exactly the merge depth (on a chain the
        // merge-depth root is the block merge_depth below the tip), and no query walks more than merge_depth chain
        // blocks
        let mut worst = 0;
        for i in 0..N {
            let (result, walked) = steps(|| dag.compute(&[tip, id(i)], 1));
            let expected = if i + depth < N {
                GhostdagError::MergeDepthRoot(id(i))
            } else {
                GhostdagError::ParentInPast(id(i))
            };
            assert_eq!(result, Err(expected), "block {i}");
            worst = worst.max(walked);
        }
        assert_eq!(worst, depth);
    }

    /// The merge depth still decides a direct parent that passes the merge-depth root (D-097): the root is the highest
    /// block of the selected chain at or below the merge depth, so it lies below it when a chain block's blue score
    /// jumps over that line, and a parent between the two passes the root but is beyond the merge depth. Here c merges
    /// two siblings of its selected parent (blue score 2 to 5), and with merge depth 3 the root of the tip e (blue
    /// score 7) is c's selected parent, at blue score 2 below the line at 4.
    #[test]
    fn the_merge_depth_decides_a_parent_between_the_root_and_the_merge_depth() {
        let params = GhostdagParams {
            k: 2,
            max_block_parents: 4,
            max_mergeset_size: 8,
            merge_depth: 3,
        };
        let genesis = id(0);
        let mut dag = BlockDag::new(params, genesis, U256::ONE, ());
        let insert = |dag: &mut BlockDag<()>, i: u64, parents: Vec<Hash>| {
            dag.insert(id(i), parents, U256::ONE, 1, 0, ()).unwrap();
            id(i)
        };
        let r = insert(&mut dag, 1, vec![genesis]);
        let siblings: Vec<Hash> = (2..=4).map(|i| insert(&mut dag, i, vec![r])).collect();
        let c = insert(&mut dag, 5, siblings);
        let root = dag.data(&c).unwrap().selected_parent.unwrap();
        // a block on the root that no other block merges
        let side = insert(&mut dag, 6, vec![root]);
        let d = insert(&mut dag, 7, vec![c]);
        let e = insert(&mut dag, 8, vec![d]);
        assert_eq!(dag.data(&c).unwrap().blue_score, 5);
        assert_eq!(dag.data(&e).unwrap().blue_score, 7);
        assert_eq!(dag.chain_floor(&e, 3).unwrap().1, root);

        // the root itself (in the past of e): before M26 ParentInPast after a walk down to c, now MergeDepth at once
        let (before, walked) = query_first(|| steps(|| dag.compute(&[e, root], 1)));
        assert_eq!(before, Err(GhostdagError::ParentInPast(root)));
        assert!(walked > 0);
        let after = steps(|| dag.compute(&[e, root], 1));
        assert_eq!(after, (Err(GhostdagError::MergeDepth(root)), 0));
        // the block on the root (not in the past of e): MergeDepth either way, now without the walk
        let (before, walked) = query_first(|| steps(|| dag.compute(&[e, side], 1)));
        assert_eq!(before, Err(GhostdagError::MergeDepth(side)));
        assert!(walked > 0);
        let after = steps(|| dag.compute(&[e, side], 1));
        assert_eq!(after, (Err(GhostdagError::MergeDepth(side)), 0));
        // r, below the root: MergeDepthRoot either way, without a walk
        let expected = (Err(GhostdagError::MergeDepthRoot(r)), 0);
        assert_eq!(query_first(|| steps(|| dag.compute(&[e, r], 1))), expected);
        assert_eq!(steps(|| dag.compute(&[e, r], 1)), expected);
    }

    /// The selected parent's chain is walked for the merge-depth root (D-097) only as far as the merged blocks need: a
    /// block with no other parent walks none of it, a block merging a parallel block walks down to where their chains
    /// meet, and only a merged block whose chain meets it lower (here one below the root, rejected) walks down to the
    /// root, `merge_depth` blocks on a chain. The whole walk used to run for every block.
    #[test]
    fn the_merge_depth_root_walk_goes_only_as_deep_as_the_merged_blocks() {
        const N: u64 = 2_000;
        let params = GhostdagParams::from_params(&DEVNET);
        let depth = params.merge_depth;
        let genesis = id(0);
        let mut dag = BlockDag::new(params, genesis, U256::ONE, ());
        let mut tip = genesis;
        for i in 1..=N {
            dag.insert(id(i), vec![tip], U256::ONE, 1, 0, ()).unwrap();
            tip = id(i);
        }
        assert_eq!(
            root_steps(|| dag.compute(&[tip], 1).map(|_| ())),
            (Ok(()), 0)
        );

        // blocks on the chain blocks two and five below the tip: their chains meet the tip's there
        let parallel = id(N + 1);
        dag.insert(parallel, vec![id(N - 2)], U256::ONE, 1, 0, ())
            .unwrap();
        let late = id(N + 2);
        dag.insert(late, vec![id(N - 5)], U256::ONE, 1, 0, ())
            .unwrap();
        let (data, walked) = root_steps(|| dag.compute(&[tip, parallel], 1));
        assert_eq!(data.unwrap().mergeset_ordered, vec![parallel]);
        assert_eq!(walked, 2);
        let (data, walked) = root_steps(|| dag.compute(&[tip, parallel, late], 1));
        assert_eq!(data.unwrap().mergeset_ordered.len(), 2);
        assert_eq!(walked, 5);

        // a block on the chain block just below the root (the chain block `merge_depth` below the tip): rejected after
        // walking the tip's chain down to the root
        let below = id(N + 3);
        dag.insert(below, vec![id(N - depth - 1)], U256::ONE, 1, 0, ())
            .unwrap();
        let (result, walked) = root_steps(|| dag.compute(&[tip, below], 1));
        assert_eq!(result, Err(GhostdagError::MergeDepthRoot(below)));
        assert_eq!(walked, depth);
    }

    /// A chain of light blocks above the merge-depth root is walked once per computation, not once per merged block
    /// (second audit, P1-1). The walk of a merged block is bounded by blue work, and a side chain of light blocks
    /// branching above the root keeps its blue work above the root's however long it grows. A block merging the heavy
    /// tip and the side chain's tip admits side blocks until its merge set is full: each of them used to walk the side
    /// chain down to the branch point again, about `max_mergeset_size × L` blocks in all.
    #[test]
    fn a_long_light_side_chain_is_walked_once_per_computation() {
        const N: u64 = 800;
        const L: u64 = 2_000;
        let params = GhostdagParams::from_params(&DEVNET);
        let max = params.max_mergeset_size;
        let genesis = id(0);
        let mut dag = BlockDag::new(params, genesis, U256::ONE, ());
        // heavy chain blocks, so that the light side chain stays lighter than the tip
        let heavy = U256::from_u64(120);
        let mut tip = genesis;
        for i in 1..=N {
            dag.insert(id(i), vec![tip], heavy, 1, 0, ()).unwrap();
            tip = id(i);
        }
        // the side chain branches off the chain block 100 below the tip, far above the root (`merge_depth` below)
        let mut side = id(N - 100);
        for i in 1..=L {
            let hash = id(N + i);
            dag.insert(hash, vec![side], U256::ONE, 1, 0, ()).unwrap();
            side = hash;
        }
        let (result, walked) = walk_steps(|| dag.compute(&[tip, side], 1).map(|_| ()));
        assert_eq!(result, Err(GhostdagError::MergesetTooLarge { max }));
        // the first walk goes through the L side blocks to the branch point; the other side blocks have its verdict
        assert_eq!(walked, L + 1);
    }

    /// The computations of a block template share one merge-depth root (second audit, P4a-2): every block it tries has
    /// the main-chain tip as its selected parent. A dead tip (a block on a chain block below the root, cheap to create
    /// and never mergeable again) made each computation walk the tip's chain down to the root, `merge_depth` blocks per
    /// dead tip; with the shared root that chain is walked once. The results are those of `compute`, and a parent set
    /// with another selected parent gets its own root.
    #[test]
    fn computations_on_one_selected_parent_share_the_merge_depth_root() {
        const N: u64 = 2_000;
        const DEAD: u64 = 100;
        let params = GhostdagParams::from_params(&DEVNET);
        let depth = params.merge_depth;
        let genesis = id(0);
        let mut dag = BlockDag::new(params, genesis, U256::ONE, ());
        let mut tip = genesis;
        for i in 1..=N {
            dag.insert(id(i), vec![tip], U256::ONE, 1, 0, ()).unwrap();
            tip = id(i);
        }
        // dead tips on the chain blocks from two below the root down (lighter than the root), and a live tip on the
        // block two below the tip
        let dead: Vec<Hash> = (1..=DEAD)
            .map(|i| {
                dag.insert(id(N + i), vec![id(N - depth - 1 - i)], U256::ONE, 1, 0, ())
                    .unwrap();
                id(N + i)
            })
            .collect();
        let live = id(N + DEAD + 1);
        dag.insert(live, vec![id(N - 2)], U256::ONE, 1, 0, ())
            .unwrap();
        let trials: Vec<[Hash; 2]> = dead.iter().chain([&live]).map(|t| [tip, *t]).collect();

        let (fresh, fresh_steps) =
            root_steps(|| trials.iter().map(|t| dag.compute(t, 1)).collect::<Vec<_>>());
        let mut shared = dag.merge_root_cache(&tip).unwrap();
        let (cached, cached_steps) = root_steps(|| {
            trials
                .iter()
                .map(|t| shared.compute(t, 1))
                .collect::<Vec<_>>()
        });
        assert_eq!(cached, fresh);
        for (result, dead) in cached.iter().zip(&dead) {
            assert_eq!(result, &Err(GhostdagError::MergeDepthRoot(*dead)));
        }
        assert_eq!(
            cached[dead.len()].as_ref().unwrap().mergeset_ordered,
            vec![live]
        );
        // each dead tip walked the chain down to the root, the live tip two blocks of it; shared, the first dead tip
        // walks it down to the root and every later one finds it there
        assert_eq!(fresh_steps, DEAD * depth + 2);
        assert_eq!(cached_steps, depth);

        // another selected parent: its own root, the same result as `compute`
        let other = [id(N - 1), live];
        assert_eq!(shared.compute(&other, 1), dag.compute(&other, 1));
        assert!(dag.compute(&other, 1).is_ok());
    }

    /// Deterministic pseudo-random numbers (xorshift64*).
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 >> 12;
            self.0 ^= self.0 << 25;
            self.0 ^= self.0 >> 27;
            self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
        }

        fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }

        fn pick<T: Copy>(&mut self, items: &[T]) -> T {
            items[self.below(items.len() as u64) as usize]
        }
    }

    /// The parent rules of SPEC §6.3 (D-063, D-097) computed directly with sets: the merge set other than the selected
    /// parent in consensus order, or `None` when the parents are invalid.
    fn reference_mergeset(
        dag: &BlockDag<()>,
        pasts: &BTreeMap<Hash, BTreeSet<Hash>>,
        parents: &[Hash],
    ) -> Option<Vec<Hash>> {
        let params = dag.params();
        let distinct: BTreeSet<Hash> = parents.iter().copied().collect();
        if parents.is_empty()
            || parents.len() > params.max_block_parents
            || distinct.len() != parents.len()
            || !parents.iter().all(|p| pasts.contains_key(p))
        {
            return None;
        }
        if parents
            .iter()
            .any(|a| parents.iter().any(|b| pasts[b].contains(a)))
        {
            return None;
        }
        let sp = *parents
            .iter()
            .max_by_key(|p| dag.chain_key(p).unwrap())
            .unwrap();
        let sp_score = dag.data(&sp).unwrap().blue_score;
        let merged: BTreeSet<Hash> = parents
            .iter()
            .flat_map(|p| pasts[p].iter().chain([p]))
            .filter(|h| **h != sp && !pasts[&sp].contains(*h))
            .copied()
            .collect();
        // the merge-depth root (D-097): the first block down the selected chain of SP whose blue score is at most
        // `blue_score(SP) − merge_depth` (the genesis block at the latest); every merged block has it on its selected
        // chain
        let selected_chain = |from: Hash| {
            std::iter::successors(Some(from), |h| dag.data(h).unwrap().selected_parent)
        };
        let root = selected_chain(sp)
            .find(|h| {
                dag.data(h).unwrap().blue_score <= sp_score.saturating_sub(params.merge_depth)
            })
            .unwrap();
        if merged.len() + 1 > params.max_mergeset_size
            || merged
                .iter()
                .any(|h| dag.data(h).unwrap().blue_score + params.merge_depth < sp_score)
            || merged
                .iter()
                .any(|h| !selected_chain(*h).any(|c| c == root))
        {
            return None;
        }
        let mut ordered: Vec<Hash> = merged.into_iter().collect();
        ordered.sort_by_key(|h| dag.sort_key(h).unwrap());
        Some(ordered)
    }

    /// What the random parent sets covered.
    #[derive(Debug, Default)]
    struct Coverage {
        accepted: u64,
        /// Blocks merged (other than the selected parent) and red blocks among them, over the accepted sets.
        merged: u64,
        reds: u64,
        rejected: u64,
        /// Sets with a merged block whose selected chain misses the merge-depth root (D-097): checked first in both
        /// orders, so rejected alike.
        below_root: u64,
        /// A direct parent that passes the merge-depth root but is beyond the merge depth (the root can lie up to K blue
        /// score below the merge depth), in the past of SP: `ParentInPast` before, `MergeDepth` now.
        deep_in_past: u64,
        /// Such a parent not in the past of SP: `MergeDepth` either way.
        deep_elsewhere: u64,
        /// Chain steps the depth check saved.
        saved_steps: u64,
    }

    /// Computes `parents` in both orders, checks the two against each other and against the rules computed with sets,
    /// and returns the result of the current order.
    fn compare(
        dag: &BlockDag<()>,
        pasts: &BTreeMap<Hash, BTreeSet<Hash>>,
        parents: &[Hash],
        weight: u64,
        seen: &mut Coverage,
        ctx: &str,
    ) -> Result<GhostdagData, GhostdagError> {
        let streak = dag.parent_streak(parents);
        assert_eq!(query_first(|| dag.parent_streak(parents)), streak, "{ctx}");
        let (new, new_steps) = steps(|| dag.compute(parents, weight));
        let (old, old_steps) = query_first(|| steps(|| dag.compute(parents, weight)));
        let reference = reference_mergeset(dag, pasts, parents);
        assert!(new_steps <= old_steps, "{ctx}");
        match (&new, &old) {
            (Ok(data), Ok(before)) => {
                // the same data, including the blue anticone sizes and the DAA score, after the same walks
                assert_eq!(data, before, "{ctx}");
                assert_eq!(new_steps, old_steps, "{ctx}");
                assert_eq!(reference.as_ref(), Some(&data.mergeset_ordered), "{ctx}");
                seen.accepted += 1;
                seen.merged += data.mergeset_ordered.len() as u64;
                seen.reds += data.mergeset_reds.len() as u64;
            }
            (Err(error), Err(before)) => {
                assert_eq!(reference, None, "{ctx}: the rules accept {error:?}");
                seen.rejected += 1;
                seen.saved_steps += old_steps - new_steps;
                match error {
                    GhostdagError::MergeDepth(p) if parents.contains(p) => {
                        // a direct parent is only reported as MergeDepth when it is beyond the merge depth: now
                        // before its ancestor query, before only when it was not in the past of SP
                        let sp = dag.selected_parent(parents).unwrap();
                        assert!(
                            dag.data(p).unwrap().blue_score + dag.params().merge_depth
                                < dag.data(&sp).unwrap().blue_score,
                            "{ctx}"
                        );
                        if before == error {
                            assert!(!dag.in_past(p, &sp).unwrap(), "{ctx}");
                            seen.deep_elsewhere += 1;
                        } else {
                            assert_eq!(before, &GhostdagError::ParentInPast(*p), "{ctx}");
                            assert!(dag.in_past(p, &sp).unwrap(), "{ctx}");
                            seen.deep_in_past += 1;
                        }
                    }
                    GhostdagError::MergeDepthRoot(_) => {
                        // the merge-depth root comes first in both orders (D-097)
                        assert_eq!(error, before, "{ctx}");
                        assert_eq!(new_steps, old_steps, "{ctx}");
                        seen.below_root += 1;
                    }
                    _ => {
                        assert_eq!(error, before, "{ctx}");
                        assert_eq!(new_steps, old_steps, "{ctx}");
                    }
                }
            }
            _ => panic!("{ctx}: one order accepts, the other rejects: {new:?} / {old:?}"),
        }
        new
    }

    /// One random DAG: grows it like producers do (the best tip plus the other tips that keep the parents valid) and
    /// with branches (on recent blocks, on any block, and a side chain of lower work per block, so with a higher blue
    /// score than its blue work suggests), and tries adversarial parent sets on it: random blocks next to the best tip
    /// (often beyond the merge depth, often in its past), blocks around the merge depth boundary, the tip of the
    /// low-work side chain, a block with one of its ancestors, random sets that may be too large or repeat a block,
    /// and unknown parents. Every set is compared with [`compare`].
    fn compare_on_random_dag(seed: u64, rounds: u64, seen: &mut Coverage) {
        let mut rng = Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1);
        let params = GhostdagParams {
            k: rng.below(4) as u16,
            max_block_parents: 2 + rng.below(4) as usize,
            max_mergeset_size: 3 + rng.below(28) as usize,
            merge_depth: rng.below(9),
        };
        let genesis = id(0);
        let mut dag = BlockDag::new(params, genesis, U256::ONE, ());
        let mut pasts = BTreeMap::from([(genesis, BTreeSet::new())]);
        let mut order = vec![genesis];
        let mut side = genesis;
        for round in 0..rounds {
            // the block with the greatest key, never the selected parent of another block
            let best = *dag
                .selected_chain_tips()
                .iter()
                .max_by_key(|h| dag.chain_key(h).unwrap())
                .unwrap();
            let best_score = dag.data(&best).unwrap().blue_score;
            let strategy = rng.below(16);
            let weight = rng.pick(&[1, 1, 1, 12, 24]);
            let ctx = format!("seed {seed} round {round} strategy {strategy} {params:?}");
            let (parents, keep): (Vec<Hash>, bool) = match strategy {
                // a producer: the best tip, then the other tips by key descending (at most 12 tried) as long as the
                // parents stay valid
                0..=3 => {
                    let mut tips: Vec<Hash> = dag.tips().iter().copied().collect();
                    tips.sort_by_key(|h| std::cmp::Reverse(dag.chain_key(h).unwrap()));
                    // the trials share one merge-depth root, as in the node's template (second audit, P4a-2)
                    let mut shared = dag.merge_root_cache(&best).unwrap();
                    let mut parents = vec![best];
                    for tip in tips.into_iter().filter(|h| *h != best).take(12) {
                        if parents.len() == params.max_block_parents {
                            break;
                        }
                        let mut trial = parents.clone();
                        trial.push(tip);
                        let result = compare(&dag, &pasts, &trial, weight, seen, &ctx);
                        assert_eq!(shared.compute(&trial, weight), result, "{ctx}");
                        if result.is_ok() {
                            parents = trial;
                        }
                    }
                    (parents, true)
                }
                // a parallel block: a branch on one of the latest blocks
                4 | 5 => (
                    vec![rng.pick(&order[order.len().saturating_sub(6)..])],
                    true,
                ),
                // the low-work side chain
                6 | 7 => (vec![side], true),
                // the best tip and random blocks
                8 | 9 => {
                    let mut parents = vec![best];
                    for _ in 0..=rng.below(3) {
                        parents.push(rng.pick(&order));
                    }
                    (parents, rng.below(3) == 0)
                }
                // the best tip and a block around the merge depth boundary
                10 => {
                    let near: Vec<Hash> = order
                        .iter()
                        .filter(|h| {
                            let reach = dag.data(h).unwrap().blue_score + params.merge_depth;
                            reach + 2 >= best_score && reach <= best_score + 1
                        })
                        .copied()
                        .collect();
                    let mut parents = vec![best];
                    if !near.is_empty() {
                        parents.push(rng.pick(&near));
                    }
                    (parents, rng.below(3) == 0)
                }
                // the best tip and the tip of the low-work side chain
                11 => (vec![best, side], true),
                // a block and one of its ancestors, in either order
                12 => {
                    let block = rng.pick(&order);
                    let ancestors: Vec<Hash> = pasts[&block].iter().copied().collect();
                    let mut parents = vec![block];
                    if !ancestors.is_empty() {
                        parents.push(rng.pick(&ancestors));
                    }
                    if rng.below(2) == 0 {
                        parents.reverse();
                    }
                    (parents, false)
                }
                // random blocks, possibly too many, possibly the same block twice
                13 => {
                    let n = 1 + rng.below(params.max_block_parents as u64 + 1);
                    let mut parents: Vec<Hash> = (0..n).map(|_| rng.pick(&order)).collect();
                    if rng.below(4) == 0 {
                        parents.push(parents[0]);
                    }
                    (parents, rng.below(3) == 0)
                }
                // an unknown parent
                14 => (vec![rng.pick(&order), id(u64::MAX - round)], false),
                // a branch on any block, from which the low-work side chain may restart
                _ => (vec![rng.pick(&order)], true),
            };
            let result = compare(&dag, &pasts, &parents, weight, seen, &ctx);

            let Ok(data) = result else { continue };
            if !keep {
                continue;
            }
            let hash = id((seed << 32) | (round + 1));
            // the low-work side chain has work 1 per block, everything else 2 to 4
            let work = if matches!(strategy, 6 | 7) {
                U256::ONE
            } else {
                U256::from_u64(2 + rng.below(3))
            };
            let streak = if weight > 1 {
                dag.parent_streak(&parents).unwrap() + 1
            } else {
                0
            };
            let inserted = dag
                .insert(hash, parents.clone(), work, weight, streak, ())
                .unwrap();
            assert_eq!(inserted, &data, "{ctx}");
            let mut past = BTreeSet::new();
            for p in &parents {
                past.insert(*p);
                past.extend(pasts[p].iter().copied());
            }
            pasts.insert(hash, past);
            order.push(hash);
            if matches!(strategy, 6 | 7) || (strategy == 15 && rng.below(2) == 0) {
                side = hash;
            }
        }
    }

    /// Checking the merge depth of the direct parents first changes no outcome: on random DAGs with honest and
    /// adversarial parent sets, `compute` accepts exactly the sets it accepted with the ancestor query first and the
    /// sets the rules computed with sets (including the merge-depth root, D-097) accept, returns identical data
    /// (selected parent, merge set order, blue and red blocks, blue score, blue work, DAA score, blue anticone sizes)
    /// after identical walks when it accepts, and only reports a direct parent beyond the merge depth as `MergeDepth`
    /// instead of `ParentInPast` when it is in the past of the selected parent; it never walks more. The merge-depth
    /// root, checked first in both orders, rejects most deep parents before either check.
    #[test]
    fn checking_the_depth_first_decides_the_same_parent_sets() {
        let mut seen = Coverage::default();
        for seed in 0..48 {
            compare_on_random_dag(seed, 400, &mut seen);
        }
        eprintln!("{seen:?}");
        // the scenarios cover merges with blue and red blocks, parents below the merge-depth root, and parents in the
        // past of SP between the root and the merge depth (a parent there that is not in the past of SP is rare on
        // these DAGs: see the_merge_depth_decides_a_parent_between_the_root_and_the_merge_depth)
        assert!(seen.accepted > 6_000, "{seen:?}");
        assert!(seen.merged > 1_500, "{seen:?}");
        assert!(seen.reds > 1_000, "{seen:?}");
        assert!(seen.rejected > 20_000, "{seen:?}");
        assert!(seen.below_root > 20_000, "{seen:?}");
        assert!(seen.deep_in_past > 10, "{seen:?}");
        assert!(seen.saved_steps > 50, "{seen:?}");
    }
}
