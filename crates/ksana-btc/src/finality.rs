//! BTC-anchored finality (the checkpoint rule, D-082): shared by node and indexer.
//!
//! Finality depends only on the data on the BTC best chain: for the same BTC chain, node and indexer compute the same
//! finalized chain, which is the guarantee that the two never fork (previously the node chose the main chain by the
//! heaviest chain it saw and the indexer by the heaviest chain on BTC, and a divergence deeper than `REORG_WINDOW`
//! could not heal automatically).
//!
//! - The header DAG ([`HeaderDag`]) is built from the headers that count in BTC payloads (D-135). The **connection
//!   height** of block B, `conn(B) = seen(B)`, the height at which its header counts on the current best chain: a header
//!   counts only once its parents have counted, so it and all headers of its past have counted by then (the genesis
//!   block counts as always on the chain).
//! - **Height-by-height decision**: at each BTC height H, the candidate `best_H` is the block with the largest
//!   fork-choice key `(chain_work, hash)` (D-100) among the blocks whose connection height does not exceed H and whose
//!   selected chain passes through the finalized tip `fin` (`fin` advances only along its selected chain, so `best_H`
//!   is monotonically non-decreasing in H). Walk the selected chain of `best_H` block by block after `fin`: block C is
//!   final when the landing height `landed(C)` of the data of its closure (C and the blocks it merges, together with
//!   the closure of `fin`) satisfies `landed(C) + BTC_FINALITY_CONFS − 1 ≤ H` (the data of every block in the closure
//!   has at least `BTC_FINALITY_CONFS` confirmations); otherwise stop (later blocks have larger closures and cannot
//!   satisfy it either). So to revert a block, an attacker must publish a heavier chain on BTC within
//!   `BTC_FINALITY_CONFS` BTC blocks after the block's data landed; after that it is ignored however heavy it is.
//! - **Only a valid block can become final** (D-101): when block C on the walk meets the landing condition, the
//!   verdict of executing C as a main-chain block on top of its selected parent (the finalized tip at that point) is
//!   asked ([`Validity`]). A valid C is final. An invalid C is **excluded** at that BTC height: it and every block
//!   whose selected chain passes through it stop being candidates, `best_H` is chosen again among the rest, and the
//!   walk goes on at the same height (a block that merges C without passing through it on its selected chain stays a
//!   candidate). While the verdict cannot be given yet the decision pauses at that height ([`Outcome::validating`]).
//!   The verdict is asked only at that point and never earlier: the data of
//!   C's closure has landed by then, so the verdict depends only on the BTC chain up to that height, and every node and
//!   indexer reaches the same decisions however early it learned that a block is invalid.
//! - **Only a block anchored on the BTC chain can become final** (D-101): when the walk reaches block C, C is excluded
//!   at once, before its landing status or verdict is asked, when the anchor of C or of a block C merges is not on the
//!   BTC best chain (the BTC block at that anchor height is another one: an anchor hash no BTC block has, or a block a
//!   BTC reorg orphaned; [`Exclusion::Unanchored`]). A payload counts only when the headers it carries anchor below its
//!   own height (D-100), so these anchors lie below the height at which C's header counts, below the height being
//!   decided: the rule depends only on the BTC chain up to it. Since D-135 a header counts only with its anchor on the
//!   best chain, so a connected block never meets this rule; it stays as a check.
//! - **A finalized block cannot be reorged**: the main chain of node and indexer must pass through it; a chain forking
//!   below it does not take part in main-chain selection however heavy it is ([`HeaderDag::set_floor`], the node's
//!   `Chain::best_candidate`). A finalized block was judged valid before it became final, so a finalized block that
//!   later turns out invalid means the implementation has a non-determinism bug: the only option is to stop and alert.
//! - **Landing status**: the snapshot answers it alone ([`BtcSnapshot::block_data_height`]): a block that needs a body
//!   lands with the first payload landing its body, checked when that BTC block was scanned, and every other block
//!   when its header counts (D-135). Nothing waits for the node or the indexer to know a block; the decision pauses only
//!   for a verdict, and the node then requests the block from its peers.
//! - **BTC reorg**: every evaluated height records the BTC block hash at the time; when the best chain no longer
//!   contains that block, all decisions, exclusions and connections from that height on are revoked and decided again.
//!   What was done at a height the evaluation paused at is done again from the start when the evaluation resumes: the
//!   BTC block at that height may have been replaced in the meantime (D-114). A decision rests on two things: the data
//!   of the block's closure, `BTC_FINALITY_CONFS` blocks deep when the decision is made, and there being no heavier
//!   candidate connected by the height being decided, which is only as deep as the BTC block that made the decision. A
//!   reorg of that one BTC block whose new branch carries the headers of a heavier chain forking below the block
//!   therefore revokes the decision for good (one more BTC block for each block mined since the decision); without
//!   such a chain the decision is made again the same way unless the reorg removes the block's data.
//! - **BTC-final, as reported** (D-114, [`Finality::btc_final`]): node and indexer report a block as BTC-final only
//!   once the BTC block that decided it has `BTC_FINALITY_CONFS` confirmations. A BTC reorg then takes it back only
//!   when it reaches that BTC block, `BTC_FINALITY_CONFS` blocks or more. It takes the block off the finalized chain
//!   when its new branch carries a heavier chain, or when it removes the block's data, `2 × BTC_FINALITY_CONFS − 1`
//!   blocks or more; otherwise the block is decided again on the new branch and reported again once the BTC block of
//!   that decision has as many confirmations, at once when the new branch is already that long. The decision itself
//!   does not wait: the floor of main-chain selection, the verdicts and pruning follow the decided tip
//!   ([`Finality::tip`]), so nothing a block's validity or the encodings depend on changes.
//!
//! Validity is not computed here (invalid blocks are found by execution): the inputs of [`Finality`] are only the
//! header DAG, the BTC snapshot (whose landing heights are those of the settled closure, D-038, D-077) and the verdict
//! of executing a block that meets the landing condition ([`Validity`], D-101).
//!
//! - **Pruning** (D-097, [`Finality::prune`]): after the header DAG is pruned at a finalized block P, the finalized
//!   chain is held from a finalized root R at or above P (the node and the indexer take the highest block decided at
//!   least their pruning margin of BTC blocks ago, [`Finality::decided_by`], and prune P deep enough below it), the
//!   evaluated BTC heights from the snapshot's new base up, and the connection heights, the closure cache and the
//!   exclusions (D-101) only for the blocks the DAG keeps. Candidates pass through the finalized tip, which is in P's future, so nothing the decision
//!   needs is dropped. A BTC reorg that would revoke R's own decision cannot be followed any more:
//!   [`Finality::evaluate`] fails, and the node or indexer must be resynced (that needs a reorg deeper than the
//!   margin).

use std::collections::BTreeMap;

use borsh::{BorshDeserialize, BorshSerialize};
use ksana_consensus::{Hash, U256};

use crate::headers::HeaderDag;
use crate::tracker::BtcSnapshot;

/// The verdict of executing a block as a main-chain block on top of its selected parent, the finalized tip (D-101):
/// the same execution as when a node connects a main-chain block or the indexer executes one (the body rules of the
/// block and of the blocks it merges, the settlement backlog limit, lenient then strict execution, the `state_root`).
/// Asked only for a block whose closure's data has landed with enough confirmations, so the answer depends only on the
/// BTC chain up to the height being decided.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Validity {
    /// The execution succeeds and the `state_root` matches.
    Valid,
    /// The block cannot be on the main chain: it, or a block it merges, breaks a body rule, or its execution fails, or
    /// its `state_root` does not match.
    Invalid,
    /// This side has no verdict yet: it does not have the block or has not executed it yet, or a local fault stopped the
    /// execution. (A block anchored, or merging a block anchored, off the BTC chain is never asked about: it is excluded
    /// first, [`Exclusion::Unanchored`].)
    Undetermined,
}

/// Why a block was excluded from the finality decision (D-101).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Exclusion {
    /// It met the landing condition and was judged invalid ([`Validity::Invalid`]).
    Invalid,
    /// The anchor of this block (the excluded block itself or a block it merges) is not on the BTC best chain: the
    /// BTC block at its anchor height is another one.
    Unanchored(Hash),
}

/// One finality decision: block `hash` (height `height`) was decided final at BTC height `btc_height` (block hash
/// `btc_hash`).
#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct Decision {
    /// The finalized block.
    pub hash: Hash,
    /// Its height (the finalized chain is contiguous from the root, [`Finality::root`]).
    pub height: u64,
    /// The BTC height at which it was decided final.
    pub btc_height: u32,
    /// The block hash at that BTC height (the BTC chain the decision was based on; revoked on a reorg).
    pub btc_hash: [u8; 32],
    /// The landing height of the data of its closure (it and all of its past).
    pub landed: u32,
}

/// The BTC-final tip as node and indexer report it ([`Finality::btc_final`], D-114).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BtcFinal {
    /// The block.
    pub hash: Hash,
    /// Its height.
    pub height: u64,
    /// The BTC height and block hash of its decision; `None` for the root.
    pub decided: Option<(u32, [u8; 32])>,
}

impl From<&Root> for BtcFinal {
    /// The root is BTC-final: the genesis block, or a block decided at least the pruning margin of BTC blocks ago
    /// (D-097).
    fn from(root: &Root) -> Self {
        BtcFinal {
            hash: root.hash,
            height: root.height,
            decided: None,
        }
    }
}

/// The result of one evaluation.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Outcome {
    /// Whether the finalized tip changed (advanced, or went back after a BTC reorg and advanced again).
    pub advanced: bool,
    /// The evaluation paused because this block meets the landing condition and its verdict ([`Validity`]) cannot be
    /// given yet (D-101): the node or indexer executes it on top of the finalized tip and evaluates again; `None` when
    /// evaluated up to the snapshot tip.
    pub validating: Option<Hash>,
    /// The blocks excluded in this evaluation, each with the reason (D-101).
    pub excluded: Vec<(Hash, Exclusion)>,
    /// A BTC reorg revoked decisions (the finalized tip went back).
    pub rolled_back: bool,
}

enum Advance {
    Done,
    Validating(Hash),
}

fn corrupt(what: &str) -> String {
    format!("inconsistent data in the finality decision: {what}")
}

/// The lowest finalized block held: the genesis block, or after pruning a finalized block at or above the pruning
/// point (D-097).
#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct Root {
    /// The block.
    pub hash: Hash,
    /// Its height.
    pub height: u64,
    /// The landing height of the data of its closure (0 for the genesis block).
    pub landed: u32,
    /// The BTC height at which it was decided final; `None` for the genesis block (never revoked).
    pub btc_height: Option<u32>,
    /// Its connection height (0 for the genesis block).
    pub conn: u32,
}

/// The state of the finality decision (see the module docs). Node and indexer each hold one and call
/// [`Finality::evaluate`] with the current snapshot after every BTC sync. The Borsh encoding is only the local format
/// of pruning checkpoints (D-097).
#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct Finality {
    /// `BTC_FINALITY_CONFS`.
    confs: u32,
    /// The lowest finalized block held.
    root: Root,
    /// The finalized chain above the root, by height; `decisions[i]` has height `root.height + i + 1`.
    decisions: Vec<Decision>,
    /// The evaluated BTC heights (contiguous) and their block hashes at the time.
    evaluated: Vec<(u32, [u8; 32])>,
    /// The connection height of each block; only connected blocks.
    conn: BTreeMap<Hash, u32>,
    /// The candidate: the block with the largest sort key among those whose connection height does not exceed the
    /// evaluated height and whose selected chain passes through the finalized tip without passing through an excluded
    /// block.
    best: Option<(U256, Hash)>,
    /// The maximum landing height of the data of a block and the blocks it merges (blocks whose data has all landed).
    closure_landed: BTreeMap<Hash, u32>,
    /// The blocks excluded (D-101: judged invalid when the rule asked, or anchored, or merging a block anchored, off the
    /// BTC chain), with the BTC height of that decision: neither they nor the blocks whose selected chain passes through
    /// them are candidates.
    excluded: BTreeMap<Hash, u32>,
}

impl Finality {
    /// No height evaluated yet: the finalized chain is only the genesis block.
    pub fn new(confs: u32, genesis: Hash) -> Self {
        Finality {
            confs: confs.max(1),
            root: Root {
                hash: genesis,
                height: 0,
                landed: 0,
                btc_height: None,
                conn: 0,
            },
            decisions: Vec::new(),
            evaluated: Vec::new(),
            conn: BTreeMap::from([(genesis, 0)]),
            best: None,
            closure_landed: BTreeMap::new(),
            excluded: BTreeMap::new(),
        }
    }

    /// The finalized tip (the root when there are no decisions above it).
    pub fn tip(&self) -> Hash {
        self.decisions.last().map_or(self.root.hash, |d| d.hash)
    }

    /// The height of the finalized tip.
    pub fn tip_height(&self) -> u64 {
        let len = u64::try_from(self.decisions.len()).unwrap_or(u64::MAX);
        self.root.height.saturating_add(len)
    }

    /// The lowest finalized block held: the genesis block, or after pruning a finalized block at or above the pruning
    /// point (D-097).
    pub fn root(&self) -> &Root {
        &self.root
    }

    /// The decisions above the root, by height.
    pub fn decisions(&self) -> &[Decision] {
        &self.decisions
    }

    /// The finalized block at height `height` (the genesis block is height 0); `None` above the finalized tip and below
    /// the root (pruned, D-097).
    pub fn final_at(&self, height: u64) -> Option<Hash> {
        if height == self.root.height {
            return Some(self.root.hash);
        }
        self.decision_at(height).map(|d| d.hash)
    }

    /// Whether the block is final.
    pub fn is_final(&self, hash: &Hash, height: u64) -> bool {
        self.final_at(height) == Some(*hash)
    }

    /// The BTC-final tip as node and indexer report it (D-114), given the one reported before (`None` when there is
    /// none, as at startup): the highest finalized block whose decision, made at an evaluated BTC height, has
    /// `BTC_FINALITY_CONFS` confirmations on the best chain of `snapshot`, or the one reported before when it is higher
    /// and its decision still stands (the same BTC block, still on that chain: a BTC reorg that does not reach it takes
    /// nothing back, even when the new branch is shorter for now); the root when there is none (the genesis block, or a
    /// block decided at least the pruning margin of BTC blocks ago, D-097). The decisions above it wait for their BTC
    /// blocks to get deep enough: a reorg reaching one revokes it, and a heavier chain on the new branch would take its
    /// block back (see the module docs).
    pub fn btc_final(&self, snapshot: &BtcSnapshot, before: Option<&BtcFinal>) -> BtcFinal {
        let evaluated = self.evaluated_height();
        let tip = snapshot.tip().map(|(h, _)| h);
        let stands = |d: &Decision| {
            evaluated.is_some_and(|e| d.btc_height <= e)
                && snapshot.hash_at(d.btc_height) == Some(d.btc_hash)
        };
        let deep = |d: &Decision| {
            d.btc_height
                .checked_add(self.confs.saturating_sub(1))
                .is_some_and(|confirmed| tip.is_some_and(|t| confirmed <= t))
        };
        // the decisions are in BTC order: those deep enough are a prefix
        let confirmed = self
            .decisions
            .iter()
            .take_while(|d| stands(d) && deep(d))
            .last();
        let kept = before.and_then(|b| {
            let decision = self.decision_at(b.height)?;
            let same = decision.hash == b.hash
                && b.decided == Some((decision.btc_height, decision.btc_hash));
            (same && stands(decision)).then_some(decision)
        });
        let reported = match (confirmed, kept) {
            (Some(c), Some(k)) => Some(if k.height > c.height { k } else { c }),
            (c, k) => c.or(k),
        };
        match reported {
            Some(d) => BtcFinal {
                hash: d.hash,
                height: d.height,
                decided: Some((d.btc_height, d.btc_hash)),
            },
            None => BtcFinal::from(&self.root),
        }
    }

    /// Whether `now`, the BTC-final tip just computed ([`Finality::btc_final`]), takes back `before`, the one reported
    /// before (D-114): the block reported is no longer on the chain reported. Only a BTC reorg reaching the BTC block
    /// that decided it does that; a block below the root never counts (pruned after it was reported, D-097).
    pub fn taken_back(&self, before: &BtcFinal, now: &BtcFinal) -> bool {
        before.height >= self.root.height
            && !(before.height <= now.height && self.is_final(&before.hash, before.height))
    }

    /// The decision of the finalized block at `height`; `None` for the root and below it, and above the finalized tip.
    fn decision_at(&self, height: u64) -> Option<&Decision> {
        let above = height.checked_sub(self.root.height)?.checked_sub(1)?;
        self.decisions.get(usize::try_from(above).ok()?)
    }

    /// The BTC height evaluated up to; `None` when no height has been evaluated yet.
    pub fn evaluated_height(&self) -> Option<u32> {
        self.evaluated.last().map(|(h, _)| *h)
    }

    /// The block's connection height (the height at which its header counts on the current BTC best chain, D-135).
    pub fn connected_at(&self, hash: &Hash) -> Option<u32> {
        self.conn.get(hash).copied()
    }

    /// The BTC height at which the block was excluded (D-101); `None` when it is not excluded.
    pub fn excluded_at(&self, hash: &Hash) -> Option<u32> {
        self.excluded.get(hash).copied()
    }

    /// The excluded blocks (D-101) with the BTC heights they were excluded at.
    pub fn excluded(&self) -> &BTreeMap<Hash, u32> {
        &self.excluded
    }

    /// The highest finalized block decided at BTC height `btc_height` or below, with its height (D-097: a BTC reorg
    /// that leaves that height in place cannot revoke it). The root counts as decided at its own decision height (the
    /// genesis block at any height); `None` when even the root was decided above `btc_height`. A height the evaluation
    /// paused at (waiting for a verdict, D-101) is not evaluated yet: the decisions already made at it do not count until
    /// it is.
    pub fn decided_by(&self, btc_height: u32) -> Option<(Hash, u64)> {
        let evaluated = self.evaluated_height();
        let decided = self.decisions.iter().rposition(|d| {
            d.btc_height <= btc_height && evaluated.is_some_and(|e| d.btc_height <= e)
        });
        match decided {
            Some(index) => {
                let decision = self.decisions.get(index)?;
                Some((decision.hash, decision.height))
            }
            None if self.root.btc_height.is_none_or(|h| h <= btc_height) => {
                Some((self.root.hash, self.root.height))
            }
            None => None,
        }
    }

    /// Whether the BTC chain of `snapshot` revokes the decision of the root (D-097): a BTC reorg deeper than the
    /// pruning margin, which [`Finality::evaluate`] cannot follow (it fails); whoever holds this decision must start
    /// over.
    pub fn root_revoked(&self, snapshot: &BtcSnapshot) -> bool {
        let Some(decided) = self.root.btc_height else {
            return false;
        };
        let still_there = |(h, hash): &(u32, [u8; 32])| snapshot.hash_at(*h) == Some(*hash);
        if self.evaluated.last().is_none_or(still_there) {
            return false;
        }
        let fork = self
            .evaluated
            .iter()
            .rposition(still_there)
            .and_then(|i| self.evaluated.get(i))
            .map(|(h, _)| *h);
        !fork.is_some_and(|f| decided <= f)
    }

    /// Continues the height-by-height evaluation from the last evaluated height up to the tip by snapshot `snapshot`
    /// (the current BTC best chain) (see the module docs). `valid` answers the verdict of executing a block that meets
    /// the landing condition on top of its selected parent, the finalized tip (D-101); it is asked for nothing else.
    /// `dag` must contain all headers that count in the snapshot (added by the caller while the tracker scanned).
    ///
    /// Returns `Err` only when the DAG data is inconsistent or `valid` fails (local fault, retry later); it does not
    /// mean any block is invalid.
    pub fn evaluate(
        &mut self,
        dag: &HeaderDag,
        snapshot: &BtcSnapshot,
        valid: &mut dyn FnMut(&Hash) -> Result<Validity, String>,
    ) -> Result<Outcome, String> {
        let mut outcome = Outcome::default();
        let tip_before = self.tip();

        // BTC reorg: go back to the highest evaluated height still on the best chain
        let still_there = |(h, hash): &(u32, [u8; 32])| snapshot.hash_at(*h) == Some(*hash);
        if !self.evaluated.last().is_none_or(still_there) {
            let keep = self.evaluated.iter().rposition(still_there);
            let fork = keep.and_then(|i| self.evaluated.get(i)).map(|(h, _)| *h);
            // checks the root first: a failure leaves the decision unchanged
            self.rollback(dag, fork)?;
            self.evaluated
                .truncate(keep.map_or(0, |i| i.saturating_add(1)));
            outcome.rolled_back = self.tip() != tip_before;
        }
        // The height the last evaluation paused at, if it did, is evaluated again from the start (D-114): what was
        // decided, excluded and connected there was made on the BTC block then at that height, which a reorg may have
        // replaced without touching any evaluated height. Decisions made there on a block no longer on the best chain
        // were revoked by a BTC reorg; an exclusion made again at the same height is not reported again
        let evaluated = self.evaluated_height();
        let above = |h: u32| evaluated.is_none_or(|e| h > e);
        let replaced = self
            .decisions
            .iter()
            .any(|d| above(d.btc_height) && snapshot.hash_at(d.btc_height) != Some(d.btc_hash));
        let reported: BTreeMap<Hash, u32> = self
            .excluded
            .iter()
            .filter(|(_, h)| above(**h))
            .map(|(hash, h)| (*hash, *h))
            .collect();
        self.rollback(dag, evaluated)?;
        outcome.rolled_back |= replaced;

        let start = match self.evaluated.last() {
            Some((h, _)) => h.checked_add(1),
            None => Some(snapshot.base()),
        };
        let (Some(mut height), Some((btc_tip, _))) = (start, snapshot.tip()) else {
            outcome.advanced = self.tip() != tip_before;
            return Ok(outcome);
        };
        while height <= btc_tip {
            let Some(btc_hash) = snapshot.hash_at(height) else {
                break;
            };
            // the headers that count at this height, each after its parents (D-135)
            for hash in snapshot.headers_at(height) {
                self.connect(dag, *hash, height)?;
            }
            let advance = self.advance(
                dag,
                snapshot,
                (height, btc_hash),
                valid,
                &mut outcome.excluded,
            )?;
            match advance {
                Advance::Done => {}
                Advance::Validating(hash) => {
                    outcome.validating = Some(hash);
                    break;
                }
            }
            self.evaluated.push((height, btc_hash));
            let Some(next) = height.checked_add(1) else {
                break;
            };
            height = next;
        }
        outcome
            .excluded
            .retain(|(hash, _)| reported.get(hash) != self.excluded.get(hash));
        outcome.advanced = self.tip() != tip_before;
        Ok(outcome)
    }

    /// Revokes all decisions, exclusions and connections above BTC height `fork` (`None` means all of them), and the
    /// cached closures whose data landed above it, then chooses the candidate again when anything was revoked. The
    /// root's own decision cannot be revoked after pruning (D-097): `Err`.
    fn rollback(&mut self, dag: &HeaderDag, fork: Option<u32>) -> Result<(), String> {
        let keep = |h: u32| fork.is_some_and(|f| h <= f);
        if self.root.btc_height.is_some_and(|h| !keep(h)) {
            return Err(format!(
                "a BTC reorg revoked the decision of the finality root (block {}, decided at BTC height {:?}): the reorg \
                 is deeper than the pruning margin (D-097); this side cannot continue and must be resynced",
                hex::encode(self.root.hash),
                self.root.btc_height
            ));
        }
        let sizes = |f: &Finality| {
            (
                f.decisions.len(),
                f.conn.len(),
                f.closure_landed.len(),
                f.excluded.len(),
            )
        };
        let before = sizes(self);
        self.decisions.retain(|d| keep(d.btc_height));
        self.conn.retain(|_, c| keep(*c));
        self.conn.insert(self.root.hash, self.root.conn);
        self.closure_landed.retain(|_, l| keep(*l));
        self.excluded.retain(|_, h| keep(*h));
        // nothing revoked: the candidate stands
        if sizes(self) == before {
            return Ok(());
        }
        self.choose_best(dag)
    }

    /// Chooses the candidate again from all connected blocks (after a BTC reorg, or after excluding a block, D-101):
    /// the one with the largest fork-choice key whose selected chain passes through the finalized tip and through no
    /// excluded block.
    fn choose_best(&mut self, dag: &HeaderDag) -> Result<(), String> {
        let mut keys = Vec::with_capacity(self.conn.len());
        for hash in self.conn.keys() {
            keys.push(dag.dag().chain_key(hash).map_err(|e| e.to_string())?);
        }
        // the keys are distinct (the hash is part of the key): the first eligible one from the top is the candidate
        keys.sort_unstable_by(|a, b| b.cmp(a));
        self.best = None;
        for key in keys {
            if self.extends_tip(dag, &key.1)? {
                self.best = Some(key);
                break;
            }
        }
        Ok(())
    }

    /// Block `hash`, whose header counts at BTC height `height`, connects at that height (D-135): its parents counted
    /// at that height or below, and connected before it. A block the header DAG does not hold (pruned away, D-097) or
    /// with a parent not connected is left out.
    fn connect(&mut self, dag: &HeaderDag, hash: Hash, height: u32) -> Result<(), String> {
        if self.conn.contains_key(&hash) {
            return Ok(());
        }
        let Some(block) = dag.dag().get(&hash) else {
            return Ok(());
        };
        if !block.parents.iter().all(|p| self.conn.contains_key(p)) {
            return Ok(());
        }
        self.conn.insert(hash, height);
        self.consider_best(dag, &hash)
    }

    /// The connected block `hash` becomes the candidate when its fork-choice key is larger than the current
    /// candidate's and its selected chain passes through the finalized tip and through no excluded block.
    fn consider_best(&mut self, dag: &HeaderDag, hash: &Hash) -> Result<(), String> {
        let key = dag.dag().chain_key(hash).map_err(|e| e.to_string())?;
        if self.best.is_some_and(|best| key <= best) {
            return Ok(());
        }
        if self.extends_tip(dag, hash)? {
            self.best = Some(key);
        }
        Ok(())
    }

    /// Whether the block's selected chain passes through the finalized tip, and above it through no block excluded
    /// as invalid (D-101).
    fn extends_tip(&self, dag: &HeaderDag, hash: &Hash) -> Result<bool, String> {
        let tip = self.tip();
        let tip_height = self.tip_height();
        let mut cursor = *hash;
        loop {
            let Some(block) = dag.dag().get(&cursor) else {
                if cursor != *hash && dag.dag().pruning_point().is_some() {
                    // the selected chain leaves the pruning point's future (D-097): it cannot pass through the tip
                    return Ok(false);
                }
                return Err(corrupt("a connected block is missing from the DAG"));
            };
            let height = block.payload.height;
            if height == tip_height {
                return Ok(cursor == tip);
            }
            if height < tip_height || self.excluded.contains_key(&cursor) {
                return Ok(false);
            }
            cursor = block
                .data
                .selected_parent
                .ok_or_else(|| corrupt("the selected chain does not reach the finalized tip"))?;
        }
    }

    /// Advances the finalized chain along the candidate's selected chain at BTC height `height` (block hash
    /// `btc_hash`). A block anchored, or merging a block anchored, off the BTC chain is excluded first; a block meeting
    /// the landing condition becomes final when `valid` judges it valid; one judged invalid is excluded. An excluded
    /// block is recorded in `excluded`, the candidate is chosen again and the walk goes on at the same height (D-101).
    fn advance(
        &mut self,
        dag: &HeaderDag,
        snapshot: &BtcSnapshot,
        (height, btc_hash): (u32, [u8; 32]),
        valid: &mut dyn FnMut(&Hash) -> Result<Validity, String>,
        excluded: &mut Vec<(Hash, Exclusion)>,
    ) -> Result<Advance, String> {
        // a closure whose data landed at a height not exceeding limit has at least confs confirmations
        let Some(limit) = height
            .checked_add(1)
            .and_then(|h| h.checked_sub(self.confs))
        else {
            return Ok(Advance::Done);
        };
        // each round of the walk either ends or excludes a block not excluded before (the candidate's selected chain
        // passes through none), so it ends
        'walk: loop {
            let Some((_, best)) = self.best else {
                return Ok(Advance::Done);
            };
            let tip = self.tip();
            if best == tip {
                return Ok(Advance::Done);
            }
            // the candidate's selected chain, from just after the finalized tip to the candidate
            let mut segment = Vec::new();
            let mut cursor = best;
            while cursor != tip {
                segment.push(cursor);
                cursor = dag
                    .ghostdag(&cursor)
                    .and_then(|d| d.selected_parent)
                    .ok_or_else(|| {
                        corrupt("the candidate's selected chain does not reach the finalized tip")
                    })?;
            }
            segment.reverse();
            let mut landed = self.decisions.last().map_or(self.root.landed, |d| d.landed);
            for block in segment {
                // anchored, or merging a block anchored, off the BTC chain: excluded before anything else is asked
                // (D-101)
                if let Some(member) = unanchored(dag, snapshot, &block)? {
                    self.excluded.insert(block, height);
                    excluded.push((block, Exclusion::Unanchored(member)));
                    self.choose_best(dag)?;
                    continue 'walk;
                }
                let Some(own) = self.closure_landed(dag, snapshot, &block)? else {
                    return Ok(Advance::Done);
                };
                landed = landed.max(own);
                if landed > limit {
                    return Ok(Advance::Done);
                }
                let block_height = dag.height_of(&block).ok_or_else(|| {
                    corrupt("a block on the candidate chain is missing from the DAG")
                })?;
                if block_height != self.tip_height().saturating_add(1) {
                    return Err(corrupt(
                        "the heights of the finalized chain are not contiguous",
                    ));
                }
                // the landing condition holds: only now is the verdict asked (D-101)
                match valid(&block)? {
                    Validity::Valid => self.decisions.push(Decision {
                        hash: block,
                        height: block_height,
                        btc_height: height,
                        btc_hash,
                        landed,
                    }),
                    Validity::Invalid => {
                        self.excluded.insert(block, height);
                        excluded.push((block, Exclusion::Invalid));
                        self.choose_best(dag)?;
                        continue 'walk;
                    }
                    Validity::Undetermined => return Ok(Advance::Validating(block)),
                }
            }
            return Ok(Advance::Done);
        }
    }

    /// The maximum landing height of the data of the block and the blocks it merges; `None` while the data of one of
    /// them has not landed.
    fn closure_landed(
        &mut self,
        dag: &HeaderDag,
        snapshot: &BtcSnapshot,
        hash: &Hash,
    ) -> Result<Option<u32>, String> {
        if let Some(landed) = self.closure_landed.get(hash) {
            return Ok(Some(*landed));
        }
        let ghostdag = dag
            .ghostdag(hash)
            .ok_or_else(|| corrupt("a block on the candidate chain is missing from the DAG"))?;
        let mut landed = 0u32;
        for member in std::iter::once(hash).chain(ghostdag.mergeset_ordered.iter()) {
            match snapshot.block_data_height(member) {
                Some(h) => landed = landed.max(h),
                None => return Ok(None),
            }
        }
        self.closure_landed.insert(*hash, landed);
        Ok(Some(landed))
    }

    /// Prunes after the header DAG was pruned at a finalized block (D-097, see the module docs): `root`, a finalized
    /// block at or above the DAG's pruning point (the node and the indexer use the highest block decided at least
    /// their pruning margin of BTC blocks ago, so that a shallower BTC reorg is still followed and can never bring the
    /// finalized tip below it), becomes the root; the evaluated BTC heights below `cut` (the BTC snapshot's new base)
    /// are dropped, and so are the connection heights and cached closures of the blocks the DAG no longer keeps.
    pub fn prune(&mut self, dag: &HeaderDag, root: &Hash, cut: u32) -> Result<(), String> {
        let point = *dag
            .dag()
            .pruning_point()
            .ok_or("the header DAG has not been pruned")?;
        let point_height = dag
            .height_of(&point)
            .ok_or("the pruning point is missing from the header DAG")?;
        // everything is checked before anything changes: a refused pruning leaves the decision as it was
        let new_root = if *root != self.root.hash {
            let height = dag
                .height_of(root)
                .ok_or("the new finality root is missing from the header DAG")?;
            if height < point_height {
                return Err("the new finality root is below the pruning point".into());
            }
            let index = height
                .checked_sub(self.root.height)
                .and_then(|h| h.checked_sub(1))
                .and_then(|i| usize::try_from(i).ok())
                .ok_or("the new finality root is below the current one")?;
            let decision = self
                .decisions
                .get(index)
                .filter(|d| d.hash == *root)
                .ok_or("the new finality root is not on the finalized chain")?;
            let conn = *self
                .conn
                .get(root)
                .ok_or("the new finality root is not connected")?;
            let new = Root {
                hash: *root,
                height,
                landed: decision.landed,
                btc_height: Some(decision.btc_height),
                conn,
            };
            Some((new, index))
        } else if self.root.height < point_height {
            return Err("the finality root is below the pruning point".into());
        } else {
            None
        };
        let root_btc_height = new_root
            .as_ref()
            .map_or(self.root.btc_height, |(new, _)| new.btc_height);
        if root_btc_height.is_some_and(|h| h < cut || self.evaluated_height().is_none_or(|e| e < h))
        {
            return Err("the BTC cut is above the decision of the finality root".into());
        }
        let kept = |h: &Hash| dag.dag().contains(h);
        if self.best.is_some_and(|(_, h)| !kept(&h)) {
            return Err(corrupt("the candidate is not in the pruned DAG"));
        }
        if let Some((new, index)) = new_root {
            self.root = new;
            self.decisions.drain(..=index);
        }
        self.evaluated.retain(|(h, _)| *h >= cut);
        self.conn.retain(|h, _| kept(h));
        self.closure_landed.retain(|h, _| kept(h));
        self.excluded.retain(|h, _| kept(h));
        Ok(())
    }
}

/// The block `hash` itself, or else the first block it merges, whose anchor is not on the BTC best chain of `snapshot`
/// (the BTC block at the anchor height is another one, D-101); `None` when every anchor is on it. Each of them first
/// appeared on BTC above its anchor height (D-100), so for a block connected by BTC height H the answer depends only on
/// the BTC chain up to H. The anchors are never below the snapshot's base: they do not decrease along parent edges, and
/// a pruned snapshot keeps every height from below the pruning point's anchor on (D-097).
fn unanchored(
    dag: &HeaderDag,
    snapshot: &BtcSnapshot,
    hash: &Hash,
) -> Result<Option<Hash>, String> {
    let missing = || corrupt("a block on the candidate chain is missing from the DAG");
    let ghostdag = dag.ghostdag(hash).ok_or_else(missing)?;
    for member in std::iter::once(hash).chain(ghostdag.mergeset_ordered.iter()) {
        let anchor = &dag.header(member).ok_or_else(missing)?.btc_anchor;
        if snapshot
            .hash_at(anchor.height)
            .is_some_and(|on_chain| on_chain != anchor.hash)
        {
            return Ok(Some(*member));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::BTreeSet;

    use proptest::prelude::*;

    use super::*;
    use crate::headers::test_support::{block, block_anchored, genesis, no_btc};
    use ksana_consensus::{Consensus, PowConsensus};
    use ksana_primitives::{BlockHeader, BtcAnchor, Params, DEVNET};

    const CONFS: u32 = 6;
    const BASE: u32 = 10;

    /// Test scene: the header DAG, the BTC snapshot and the verdict of each block.
    struct Scene {
        consensus: PowConsensus,
        dag: HeaderDag,
        snapshot: BtcSnapshot,
        /// Blocks whose execution fails (D-101).
        invalid: BTreeSet<Hash>,
        /// Blocks this side has no verdict for yet (D-101).
        unjudged: BTreeSet<Hash>,
        /// Every block the verdict was asked for, in order.
        asked: RefCell<Vec<Hash>>,
        genesis: BlockHeader,
    }

    impl Scene {
        fn new() -> Self {
            Scene::with_params(&DEVNET)
        }

        fn with_params(params: &Params) -> Self {
            let (consensus, g) = genesis();
            let dag = HeaderDag::new(params, &consensus, g.clone()).unwrap();
            // the genesis block anchors at BASE (D-101: an anchor off the BTC chain excludes a block)
            assert_eq!(g.btc_anchor.height, BASE);
            let mut snapshot = BtcSnapshot::testing(BASE);
            snapshot.push_block(g.btc_anchor.hash, 1_790_000_000);
            Scene {
                consensus,
                dag,
                snapshot,
                invalid: BTreeSet::new(),
                unjudged: BTreeSet::new(),
                asked: RefCell::new(Vec::new()),
                genesis: g,
            }
        }

        fn btc_tip(&self) -> u32 {
            self.snapshot.tip().unwrap().0
        }

        /// Mines BTC blocks up to height `height`.
        fn mine_to(&mut self, height: u32) {
            while self.btc_tip() < height {
                let next = self.btc_tip() + 1;
                self.snapshot.push_block(
                    [u8::try_from(next).unwrap(); 32],
                    1_790_000_000 + next * 600,
                );
            }
        }

        /// Makes a block with `parents` as parents and adds it to the DAG (the header has not appeared on BTC yet).
        fn block(&mut self, parents: &[&BlockHeader], tag: u8) -> BlockHeader {
            let header = block(&self.consensus, &self.dag, parents, tag);
            assert!(matches!(
                self.dag.insert(&self.consensus, header.clone(), &no_btc()),
                Insert::Connected(_)
            ));
            header
        }

        /// Like [`Scene::block`], anchored at `anchor` (its hash need not be on the BTC chain).
        fn block_on(
            &mut self,
            parents: &[&BlockHeader],
            tag: u8,
            anchor: BtcAnchor,
        ) -> BlockHeader {
            let mut header = block(&self.consensus, &self.dag, parents, tag);
            header.btc_anchor = anchor;
            header.nonce = 0;
            assert!(self.consensus.solve(&mut header, 1, 0, 1 << 20).unwrap());
            assert!(matches!(
                self.dag.insert(&self.consensus, header.clone(), &no_btc()),
                Insert::Connected(_)
            ));
            header
        }

        /// The header counts at BTC height `height` (mining up to that height first when necessary): its parents must
        /// have counted at that height or below (D-135).
        fn see(&mut self, header: &BlockHeader, height: u32) {
            self.mine_to(height);
            for parent in &header.parents {
                assert!(
                    *parent == self.genesis.hash()
                        || self
                            .snapshot
                            .counted_at(parent)
                            .is_some_and(|h| h <= height),
                    "a header counts only once its parents have (D-135)"
                );
            }
            self.snapshot.see_header(header.hash(), height);
        }

        /// The block needs a body; the payload carrying its body is confirmed at BTC height `height`.
        fn settle(&mut self, header: &BlockHeader, height: u32) {
            self.mine_to(height);
            self.snapshot
                .settle(header.hash(), [0xaa ^ header.producer[0]; 32], height);
        }

        /// The headers of `planned` (block hash → the BTC height at which it is carried) carried at heights up to the
        /// tip count, each at the lowest height at or above that at which its parents have counted (as when a header
        /// carried before its parents is carried again, D-135).
        fn count_planned(&mut self, planned: &BTreeMap<Hash, u32>) {
            let tip = self.btc_tip();
            loop {
                let mut counted = false;
                for (hash, at) in planned {
                    if *at > tip || self.snapshot.counted_at(hash).is_some() {
                        continue;
                    }
                    let parents = &self.dag.dag().get(hash).unwrap().parents;
                    let mut height = *at;
                    let mut ready = true;
                    for parent in parents {
                        if *parent == self.genesis.hash() {
                            continue;
                        }
                        match self.snapshot.counted_at(parent) {
                            Some(h) => height = height.max(h),
                            None => ready = false,
                        }
                    }
                    if ready && height <= tip {
                        self.snapshot.see_header(*hash, height);
                        counted = true;
                    }
                }
                if !counted {
                    return;
                }
            }
        }

        fn validity(&self, hash: &Hash) -> Result<Validity, String> {
            self.asked.borrow_mut().push(*hash);
            Ok(if self.unjudged.contains(hash) {
                Validity::Undetermined
            } else if self.invalid.contains(hash) {
                Validity::Invalid
            } else {
                Validity::Valid
            })
        }

        fn evaluate(&self, finality: &mut Finality) -> Outcome {
            finality
                .evaluate(&self.dag, &self.snapshot, &mut |h| self.validity(h))
                .unwrap()
        }

        /// Evaluates once from scratch (to compare with the incremental evaluation).
        fn evaluated_from_scratch(&self) -> Finality {
            let mut fresh = Finality::new(CONFS, self.genesis.hash());
            self.evaluate(&mut fresh);
            fresh
        }
    }

    use crate::headers::Insert;

    fn hashes(headers: &[&BlockHeader]) -> Vec<Hash> {
        headers.iter().map(|h| h.hash()).collect()
    }

    fn final_chain(f: &Finality) -> Vec<Hash> {
        f.decisions().iter().map(|d| d.hash).collect()
    }

    #[test]
    fn blocks_become_final_when_their_data_has_enough_confirmations() {
        let mut scene = Scene::new();
        let g = scene.genesis.clone();
        let a1 = scene.block(&[&g], 1);
        let a2 = scene.block(&[&a1], 2);
        let a3 = scene.block(&[&a2], 3);
        scene.see(&a1, 11);
        scene.see(&a2, 12);
        scene.see(&a3, 13);
        let mut f = Finality::new(CONFS, g.hash());
        let outcome = scene.evaluate(&mut f);
        assert_eq!(outcome, Outcome::default());
        assert_eq!(f.evaluated_height(), Some(13));
        assert_eq!(f.tip(), g.hash());
        assert_eq!(f.connected_at(&a3.hash()), Some(13));
        // a1's data landed at 11: the 6th confirmation is at 16
        scene.mine_to(15);
        assert!(!scene.evaluate(&mut f).advanced);
        scene.mine_to(16);
        let outcome = scene.evaluate(&mut f);
        assert!(outcome.advanced);
        assert_eq!(final_chain(&f), hashes(&[&a1]));
        assert_eq!(f.decisions()[0].btc_height, 16);
        assert_eq!(f.decisions()[0].landed, 11);
        assert!(f.is_final(&a1.hash(), 1));
        assert!(f.is_final(&g.hash(), 0));
        assert!(!f.is_final(&a2.hash(), 2));
        scene.mine_to(18);
        scene.evaluate(&mut f);
        assert_eq!(final_chain(&f), hashes(&[&a1, &a2, &a3]));
        assert_eq!(f.decisions()[1].btc_height, 17);
        assert_eq!(f.decisions()[2].btc_height, 18);
        assert_eq!(f.tip_height(), 3);
        assert_eq!(
            final_chain(&scene.evaluated_from_scratch()),
            final_chain(&f)
        );
    }

    #[test]
    fn a_heavier_chain_revealed_in_time_wins_and_one_revealed_late_is_ignored() {
        let mut scene = Scene::new();
        let g = scene.genesis.clone();
        let a1 = scene.block(&[&g], 1);
        let a2 = scene.block(&[&a1], 2);
        let b1 = scene.block(&[&g], 3);
        let b2 = scene.block(&[&b1], 4);
        let b3 = scene.block(&[&b2], 5);
        scene.see(&a1, 11);
        scene.see(&a2, 12);
        let mut late = Finality::new(CONFS, g.hash());
        scene.mine_to(16);
        scene.evaluate(&mut late);
        assert_eq!(final_chain(&late), hashes(&[&a1]));
        // the heavier b chain landed only after a1 became final: ignored
        scene.see(&b1, 17);
        scene.see(&b2, 17);
        scene.see(&b3, 17);
        scene.mine_to(30);
        scene.evaluate(&mut late);
        assert_eq!(final_chain(&late), hashes(&[&a1, &a2]));
        assert_eq!(
            final_chain(&scene.evaluated_from_scratch()),
            hashes(&[&a1, &a2])
        );

        // the same b chain landed before a1 became final (12): it is heavier, the a chain is abandoned
        let mut scene = Scene::new();
        let g = scene.genesis.clone();
        let a1 = scene.block(&[&g], 1);
        let a2 = scene.block(&[&a1], 2);
        let b1 = scene.block(&[&g], 3);
        let b2 = scene.block(&[&b1], 4);
        let b3 = scene.block(&[&b2], 5);
        scene.see(&a1, 11);
        scene.see(&a2, 12);
        scene.see(&b1, 12);
        scene.see(&b2, 12);
        scene.see(&b3, 12);
        scene.mine_to(30);
        let mut f = Finality::new(CONFS, g.hash());
        scene.evaluate(&mut f);
        assert_eq!(final_chain(&f), hashes(&[&b1, &b2, &b3]));
        assert!(f.decisions().iter().all(|d| d.btc_height == 17));
    }

    #[test]
    fn a_btc_reorg_revokes_the_decisions_made_on_the_orphaned_blocks() {
        let mut scene = Scene::new();
        let g = scene.genesis.clone();
        let a1 = scene.block(&[&g], 1);
        let a2 = scene.block(&[&a1], 2);
        scene.see(&a1, 11);
        scene.see(&a2, 12);
        scene.mine_to(17);
        let mut f = Finality::new(CONFS, g.hash());
        scene.evaluate(&mut f);
        assert_eq!(final_chain(&f), hashes(&[&a1, &a2]));
        // BTC reorg to 15: both decisions (16, 17) are revoked
        scene.snapshot.truncate_for_test(15);
        let outcome = scene.evaluate(&mut f);
        assert!(outcome.rolled_back);
        assert_eq!(final_chain(&f), Vec::<Hash>::new());
        assert_eq!(f.evaluated_height(), Some(15));
        // on the new branch a2's header does not appear again and another, heavier b chain appears at 16: a1 is still
        // final at 16 (decisions below the fork are unchanged), then along the b chain
        let b2 = scene.block(&[&a1], 3);
        let b3 = scene.block(&[&b2], 4);
        scene.see(&b2, 16);
        scene.see(&b3, 16);
        scene.mine_to(21);
        let outcome = scene.evaluate(&mut f);
        assert!(outcome.advanced);
        assert_eq!(final_chain(&f), hashes(&[&a1, &b2, &b3]));
        assert_eq!(f.decisions()[0].btc_height, 16);
        assert_eq!(f.decisions()[1].btc_height, 21);
        assert_eq!(
            final_chain(&scene.evaluated_from_scratch()),
            final_chain(&f)
        );
        // reorg to before the decision: no finalized block is revoked
        scene.snapshot.truncate_for_test(20);
        let outcome = scene.evaluate(&mut f);
        assert!(outcome.rolled_back);
        assert_eq!(final_chain(&f), hashes(&[&a1]));
        assert_eq!(f.evaluated_height(), Some(20));
    }

    #[test]
    fn closures_wait_for_bodies_and_for_merged_blocks() {
        let mut scene = Scene::new();
        let g = scene.genesis.clone();
        // a1 needs a body: the header counts at 11, the body lands at 14
        let a1 = scene.block(&[&g], 1);
        scene.see(&a1, 11);
        scene.settle(&a1, 14);
        // the header of the parallel block s (hash smaller than a1's, so a1 is m's selected parent) counts only at
        // 20; m merges it, so m counts at 20 at the earliest (D-135)
        let s = (9u8..=255)
            .map(|tag| block(&scene.consensus, &scene.dag, &[&g], tag))
            .find(|s| s.hash() < a1.hash())
            .unwrap();
        assert!(matches!(
            scene.dag.insert(&scene.consensus, s.clone(), &no_btc()),
            Insert::Connected(_)
        ));
        let m = scene.block(&[&a1, &s], 2);
        assert_eq!(m.parents[0], a1.hash());
        scene.mine_to(18);
        let mut f = Finality::new(CONFS, g.hash());
        scene.evaluate(&mut f);
        assert_eq!(f.evaluated_height(), Some(18));
        // a1's data landed at 14 → final at 19; m has to wait for s to connect
        assert_eq!(final_chain(&f), Vec::<Hash>::new());
        scene.mine_to(19);
        scene.evaluate(&mut f);
        assert_eq!(final_chain(&f), hashes(&[&a1]));
        assert_eq!(f.decisions()[0].landed, 14);
        assert_eq!(f.connected_at(&m.hash()), None);
        scene.see(&s, 20);
        scene.see(&m, 20);
        scene.mine_to(24);
        scene.evaluate(&mut f);
        assert_eq!(f.connected_at(&m.hash()), Some(20));
        assert_eq!(final_chain(&f), hashes(&[&a1]));
        scene.mine_to(25);
        scene.evaluate(&mut f);
        assert_eq!(final_chain(&f), hashes(&[&a1, &m]));
        assert_eq!(f.decisions()[1].landed, 20);
        // a block that needs a body whose body never lands: never final
        let n = scene.block(&[&m], 3);
        scene.see(&n, 26);
        scene.snapshot.needs_body_for_test(n.hash());
        scene.mine_to(60);
        scene.evaluate(&mut f);
        assert_eq!(final_chain(&f), hashes(&[&a1, &m]));
        assert_eq!(
            final_chain(&scene.evaluated_from_scratch()),
            final_chain(&f)
        );
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(24))]

        /// Incremental evaluation (once per BTC height) gives the same result as evaluating to the tip in one go, also
        /// when some blocks are invalid (D-101).
        #[test]
        fn incremental_evaluation_matches_evaluation_from_scratch(
            parents in proptest::collection::vec(0usize..4, 6..14),
            seen in proptest::collection::vec(0u32..12, 14),
            bodies in proptest::collection::vec(any::<bool>(), 14),
            settle_delay in proptest::collection::vec(0u32..4, 14),
            invalid in proptest::collection::vec(0u8..5, 14),
        ) {
            let mut scene = Scene::new();
            let g = scene.genesis.clone();
            let mut headers = vec![g.clone()];
            for (i, p) in parents.iter().enumerate() {
                // parents: one of the most recent few blocks, occasionally also merging an earlier one
                let first = headers.len() - 1 - (*p % headers.len());
                let mut ps: Vec<&BlockHeader> = vec![&headers[first]];
                if i % 3 == 0 && first > 0 {
                    // occasionally also merge an earlier block (it must not be an ancestor of the first parent)
                    let other = &headers[first - 1];
                    let related = scene.dag.dag().in_past(&other.hash(), &ps[0].hash()).unwrap()
                        || scene.dag.dag().in_past(&ps[0].hash(), &other.hash()).unwrap();
                    if !related {
                        ps.push(other);
                    }
                }
                let header = scene.block(&ps, u8::try_from(i + 1).unwrap());
                headers.push(header);
            }
            let mut counted_at: BTreeMap<Hash, u32> = BTreeMap::new();
            for (i, header) in headers.iter().enumerate().skip(1) {
                // a header carried before its parents count counts once it is carried again after them (D-135)
                let height = header
                    .parents
                    .iter()
                    .filter_map(|p| counted_at.get(p))
                    .fold(BASE + 1 + seen[i], |h, p| h.max(*p));
                counted_at.insert(header.hash(), height);
                scene.see(header, height);
                if bodies[i] {
                    scene.settle(header, height + settle_delay[i]);
                }
                if invalid[i] == 0 {
                    scene.invalid.insert(header.hash());
                }
            }
            let last = scene.btc_tip() + CONFS + 2;
            // incremental: release the snapshot height by height
            let full = scene.snapshot.clone();
            let mut incremental = Finality::new(CONFS, g.hash());
            for height in BASE..=last {
                scene.snapshot = full.clone();
                scene.mine_to(height);
                scene.snapshot.truncate_for_test(height);
                scene.evaluate(&mut incremental);
            }
            scene.snapshot = full;
            scene.mine_to(last);
            let scratch = scene.evaluated_from_scratch();
            prop_assert_eq!(final_chain(&incremental), final_chain(&scratch));
            prop_assert_eq!(incremental.decisions(), scratch.decisions());
            prop_assert_eq!(incremental.excluded(), scratch.excluded());
            // no invalid block is final
            for decision in incremental.decisions() {
                prop_assert!(!scene.invalid.contains(&decision.hash));
            }
        }
    }

    proptest! {
        // Reorgs from the height a pause stopped at (kind 6) make the case of D-114 item 6 common: with this many
        // cases, about 30 ms each, every run fails without that item
        #![proptest_config(ProptestConfig::with_cases(256))]

        /// Incremental evaluation through pauses for missing verdicts (D-101), and through BTC reorgs onto branches
        /// carrying the headers above the fork point at other heights and shorter for a while or not, gives the same
        /// result as evaluating from scratch after every step, also when a reorg replaces the height a pause stopped at
        /// (D-114); and no reorg of fewer than `BTC_FINALITY_CONFS` blocks (below the highest tip yet) takes back the
        /// BTC-final tip reported. Deeper reorgs, down to the genesis block's anchor, may take it back.
        #[test]
        fn incremental_evaluation_through_pauses_and_reorgs_matches_evaluation_from_scratch(
            parents in proptest::collection::vec(0usize..4, 6..12),
            seen in proptest::collection::vec(0u32..8, 12),
            invalid in proptest::collection::vec(0u8..6, 12),
            unjudged in proptest::collection::vec(0u8..3, 12),
            steps in proptest::collection::vec((0u8..8, 1u32..9, 0u32..3), 4..24),
        ) {
            let mut scene = Scene::new();
            let g = scene.genesis.clone();
            let mut headers = vec![g.clone()];
            for (i, p) in parents.iter().enumerate() {
                let first = headers.len() - 1 - (*p % headers.len());
                let mut ps: Vec<&BlockHeader> = vec![&headers[first]];
                if i % 3 == 0 && first > 0 {
                    let other = &headers[first - 1];
                    let related = scene.dag.dag().in_past(&other.hash(), &ps[0].hash()).unwrap()
                        || scene.dag.dag().in_past(&ps[0].hash(), &other.hash()).unwrap();
                    if !related {
                        ps.push(other);
                    }
                }
                let header = scene.block(&ps, u8::try_from(i + 1).unwrap());
                headers.push(header);
            }
            // the BTC height at which each header is carried on the current branch
            let mut plan: BTreeMap<Hash, u32> = BTreeMap::new();
            for (i, header) in headers.iter().enumerate().skip(1) {
                plan.insert(header.hash(), BASE + 1 + seen[i]);
                if invalid[i] == 0 {
                    scene.invalid.insert(header.hash());
                }
                if unjudged[i] == 0 {
                    scene.unjudged.insert(header.hash());
                }
            }
            let mine = |scene: &mut Scene, plan: &BTreeMap<Hash, u32>, branch: u8, to: u32| {
                while scene.btc_tip() < to {
                    let next = scene.btc_tip() + 1;
                    scene
                        .snapshot
                        .push_block(branch_hash(next, branch), 1_790_000_000 + next * 600);
                    scene.count_planned(plan);
                }
            };
            let mut f = Finality::new(CONFS, g.hash());
            let mut reported: Option<BtcFinal> = None;
            let mut branch = 0u8;
            let mut top = BASE;
            for (kind, n, shift) in steps {
                let tip = scene.btc_tip();
                top = top.max(tip);
                // a BTC reorg reaching BTC_FINALITY_CONFS blocks or more below the highest tip yet may take back what
                // was reported
                let mut deep = false;
                match kind {
                    0 | 1 => mine(&mut scene, &plan, branch, tip + n),
                    // a BTC reorg: the headers that appeared above the fork point appear `shift` blocks later on the
                    // new branch, as long as the old one or shorter for now
                    2..=6 => {
                        let fork = match kind {
                            // of n blocks, fewer than BTC_FINALITY_CONFS below the highest tip yet
                            2 | 3 => tip.saturating_sub(n).max(top + 1 - CONFS),
                            // of n blocks, down to the genesis block's anchor at most
                            4 | 5 => tip.saturating_sub(n),
                            // from the height the evaluation paused at, or of the tip when it did not pause
                            _ => f
                                .evaluated_height()
                                .map_or(BASE, |e| e.min(tip.saturating_sub(1))),
                        }
                        .max(BASE);
                        deep = fork + CONFS <= top;
                        scene.snapshot.truncate_for_test(fork);
                        branch += 1;
                        for at in plan.values_mut().filter(|at| **at > fork) {
                            *at += shift;
                        }
                        if n % 2 == 0 {
                            mine(&mut scene, &plan, branch, tip);
                        }
                    }
                    // the verdicts arrive
                    _ => scene.unjudged.clear(),
                }
                scene.evaluate(&mut f);
                let scratch = scene.evaluated_from_scratch();
                prop_assert_eq!(f.decisions(), scratch.decisions());
                prop_assert_eq!(f.excluded(), scratch.excluded());
                prop_assert_eq!(f.evaluated_height(), scratch.evaluated_height());
                prop_assert_eq!(
                    f.btc_final(&scene.snapshot, None),
                    scratch.btc_final(&scene.snapshot, None)
                );
                let now = f.btc_final(&scene.snapshot, reported.as_ref());
                if let Some(before) = reported.as_ref().filter(|_| !deep) {
                    prop_assert!(!f.taken_back(before, &now), "{before:?} taken back by {now:?}");
                }
                reported = Some(now);
            }
            scene.unjudged.clear();
            let last = scene.btc_tip().max(plan.values().max().copied().unwrap_or(BASE)) + 2 * CONFS;
            mine(&mut scene, &plan, branch, last);
            scene.evaluate(&mut f);
            let scratch = scene.evaluated_from_scratch();
            prop_assert_eq!(f.decisions(), scratch.decisions());
            prop_assert_eq!(f.excluded(), scratch.excluded());
            prop_assert_eq!(f.evaluated_height(), scratch.evaluated_height());
            let now = f.btc_final(&scene.snapshot, reported.as_ref());
            prop_assert_eq!(now, scratch.btc_final(&scene.snapshot, None));
        }
    }

    /// Pruning (D-097): after the header DAG is pruned at a finalized block and the snapshot at a cut below its anchor,
    /// the finality decision holds the chain from that block up and goes on exactly like the unpruned decision; its
    /// record restores it; a BTC reorg that would revoke the pruning point's own decision fails.
    #[test]
    fn a_pruned_decision_goes_on_like_the_full_one() {
        let params = Params {
            ghostdag_k: 2,
            merge_depth: 4,
            ..DEVNET
        };
        let mut scene = Scene::with_params(&params);
        let g = scene.genesis.clone();
        let anchor = |i: u8| Some(10 + u32::from(i) / 4);
        // a1..a40: block i anchored at 10 + i / 4, its header seen at 11 + i / 4
        let mut a = vec![g.clone()];
        for i in 1..=40u8 {
            let header = block_anchored(
                &scene.consensus,
                &scene.dag,
                &[a.last().unwrap()],
                i,
                anchor(i),
            );
            assert!(matches!(
                scene
                    .dag
                    .insert(&scene.consensus, header.clone(), &no_btc()),
                Insert::Connected(_)
            ));
            scene.see(&header, 11 + u32::from(i) / 4);
            a.push(header);
        }
        // a header seen at 18 is final at 23: up to a31
        scene.mine_to(23);
        let mut f = Finality::new(CONFS, g.hash());
        scene.evaluate(&mut f);
        assert_eq!(f.tip(), a[31].hash());
        // block i is decided at 16 + i / 4
        assert_eq!(f.decided_by(17), Some((a[7].hash(), 7)));
        assert_eq!(f.decided_by(15), Some((g.hash(), 0)), "the genesis block");
        let full_dag = HeaderDag::from_record(&params, scene.dag.record().unwrap()).unwrap();
        let mut full = Scene {
            consensus: scene.consensus.clone(),
            dag: full_dag,
            snapshot: scene.snapshot.clone(),
            invalid: BTreeSet::new(),
            unjudged: BTreeSet::new(),
            asked: RefCell::new(Vec::new()),
            genesis: g.clone(),
        };
        let mut full_f = f.clone();

        // prune at a10 (anchored at 12, decided at 18), the snapshot at 11
        let point = a[10].hash();
        scene.dag.set_floor(f.tip(), &no_btc()).unwrap();
        scene.dag.prune(&point).unwrap();
        let cut = 11;
        let kept = |hash: &[u8; 32]| scene.dag.dag().contains(hash);
        scene.snapshot.prune(cut, &kept);
        assert!(
            f.prune(&full.dag, &point, cut).is_err(),
            "the DAG passed is not pruned"
        );
        assert!(
            f.clone().prune(&scene.dag, &a[9].hash(), cut).is_err(),
            "a root below the pruning point"
        );
        assert!(
            f.clone().prune(&scene.dag, &a[35].hash(), cut).is_err(),
            "a root above the finalized tip"
        );
        // refused for a BTC cut above the new root's decision (a10 was decided at 18): a refused pruning changes nothing
        let mut refused = f.clone();
        let err = refused.prune(&scene.dag, &point, 19).unwrap_err();
        assert!(err.contains("BTC cut"), "{err}");
        assert_eq!(refused, f);
        // a root above the pruning point holds the finalized chain from there
        let mut higher = f.clone();
        higher.prune(&scene.dag, &a[12].hash(), cut).unwrap();
        assert_eq!(higher.root().height, 12);
        assert_eq!(higher.tip(), full_f.tip());
        assert_eq!(higher.decisions(), &full_f.decisions()[12..]);
        f.prune(&scene.dag, &point, cut).unwrap();
        assert_eq!(f.root().hash, point);
        assert_eq!(f.root().height, 10);
        assert_eq!(f.root().btc_height, Some(18));
        assert_eq!(f.decided_by(17), None, "the root itself was decided at 18");
        assert_eq!(f.decided_by(18), Some((a[11].hash(), 11)));
        assert_eq!(f.tip(), full_f.tip());
        assert_eq!(f.tip_height(), 31);
        assert_eq!(f.final_at(9), None);
        assert_eq!(f.final_at(10), Some(point));
        assert_eq!(f.final_at(20), full_f.final_at(20));
        assert_eq!(f.decisions(), &full_f.decisions()[10..]);
        assert_eq!(f.evaluated_height(), full_f.evaluated_height());
        assert_eq!(f.connected_at(&a[9].hash()), None);
        assert_eq!(
            f.connected_at(&a[20].hash()),
            full_f.connected_at(&a[20].hash())
        );
        let bytes = borsh::to_vec(&f).unwrap();
        assert_eq!(borsh::from_slice::<Finality>(&bytes).unwrap(), f);

        // both go on: new blocks, BTC blocks, evaluations
        let mut tip = a.last().unwrap().clone();
        for i in 41..=48u8 {
            let header = block_anchored(&scene.consensus, &scene.dag, &[&tip], i, anchor(i));
            for s in [&mut scene, &mut full] {
                assert!(matches!(
                    s.dag.insert(&s.consensus, header.clone(), &no_btc()),
                    Insert::Connected(_)
                ));
                s.see(&header, 24 + u32::from(i - 41) / 2);
            }
            tip = header;
        }
        for height in 24..=34 {
            scene.mine_to(height);
            full.mine_to(height);
            scene.evaluate(&mut f);
            full.evaluate(&mut full_f);
            assert_eq!(f.tip(), full_f.tip(), "BTC height {height}");
        }
        assert_eq!(f.tip(), tip.hash());
        assert_eq!(f.decisions(), &full_f.decisions()[10..]);

        // a BTC reorg down to the pruning point's decision is followed; one below it is not, and leaves the decision
        // unchanged
        assert!(!f.root_revoked(&scene.snapshot));
        scene.snapshot.truncate_for_test(18);
        full.snapshot.truncate_for_test(18);
        assert!(!f.root_revoked(&scene.snapshot));
        let outcome = scene.evaluate(&mut f);
        assert!(outcome.rolled_back);
        full.evaluate(&mut full_f);
        assert_eq!(f.tip(), full_f.tip());
        assert_eq!(f.decisions(), &full_f.decisions()[10..]);
        scene.snapshot.truncate_for_test(17);
        assert!(f.root_revoked(&scene.snapshot));
        let before = f.clone();
        let err = f
            .evaluate(&scene.dag, &scene.snapshot, &mut |h| scene.validity(h))
            .unwrap_err();
        assert!(err.contains("finality root"), "{err}");
        assert_eq!(f, before);
    }

    /// The attack of D-101: a block that cannot be on the main chain (x2) lands first on a branch heavier than the
    /// honest one (x3 on top of it), and the honest block (a2) lands later but before x2 reaches its confirmations.
    /// Returns the scene and the blocks `[a1, a2, x2, x3]`.
    fn invalid_branch_lands_first() -> (Scene, [BlockHeader; 4]) {
        let mut scene = Scene::new();
        let g = scene.genesis.clone();
        let a1 = scene.block(&[&g], 1);
        let a2 = scene.block(&[&a1], 2);
        let x2 = scene.block(&[&a1], 3);
        let x3 = scene.block(&[&x2], 4);
        scene.see(&a1, 11);
        scene.see(&x2, 12);
        scene.see(&x3, 12);
        scene.see(&a2, 13);
        scene.invalid.insert(x2.hash());
        (scene, [a1, a2, x2, x3])
    }

    #[test]
    fn an_invalid_candidate_is_excluded_and_the_next_best_becomes_final() {
        let (mut scene, [a1, a2, x2, x3]) = invalid_branch_lands_first();
        let g = scene.genesis.clone();
        scene.mine_to(16);
        let mut f = Finality::new(CONFS, g.hash());
        scene.evaluate(&mut f);
        assert_eq!(final_chain(&f), hashes(&[&a1]));
        // 17: x3 is the candidate; x2 meets the landing condition and is judged invalid, so it and x3 are excluded;
        // a2 (landed at 13) is the candidate now, final at 18
        scene.mine_to(17);
        let outcome = scene.evaluate(&mut f);
        assert_eq!(outcome.excluded, vec![(x2.hash(), Exclusion::Invalid)]);
        assert!(!outcome.advanced);
        assert_eq!(f.excluded_at(&x2.hash()), Some(17));
        assert_eq!(f.excluded_at(&x3.hash()), None, "excluded through x2");
        assert_eq!(f.evaluated_height(), Some(17));
        scene.mine_to(18);
        let outcome = scene.evaluate(&mut f);
        assert!(outcome.advanced);
        assert!(outcome.excluded.is_empty());
        assert_eq!(final_chain(&f), hashes(&[&a1, &a2]));
        assert_eq!(f.decisions()[1].btc_height, 18);
        // x3 is heavier than everything, but its selected chain passes through x2: never a candidate again
        scene.mine_to(40);
        scene.evaluate(&mut f);
        assert_eq!(final_chain(&f), hashes(&[&a1, &a2]));
        let scratch = scene.evaluated_from_scratch();
        assert_eq!(scratch.decisions(), f.decisions());
        assert_eq!(scratch.excluded(), f.excluded());
    }

    #[test]
    fn the_verdict_is_asked_only_when_the_landing_condition_holds() {
        let (mut scene, [a1, a2, x2, _]) = invalid_branch_lands_first();
        let g = scene.genesis.clone();
        let mut f = Finality::new(CONFS, g.hash());
        let mut asked_at = Vec::new();
        for height in 11..=20 {
            scene.mine_to(height);
            scene.evaluate(&mut f);
            asked_at.push((height, scene.asked.borrow().clone()));
        }
        // nothing before a1's 6th confirmation, a1 at 16, x2 at 17 (its 6th confirmation), a2 at 18; never x3, which
        // is invalid through x2 but never meets the landing condition on a candidate's walk with its verdict asked
        let expect = |height: u32| match height {
            ..=15 => Vec::new(),
            16 => hashes(&[&a1]),
            17 => hashes(&[&a1, &x2]),
            _ => hashes(&[&a1, &x2, &a2]),
        };
        for (height, asked) in asked_at {
            assert_eq!(asked, expect(height), "BTC height {height}");
        }
    }

    #[test]
    fn a_missing_verdict_pauses_the_decision_at_that_height() {
        let mut scene = Scene::new();
        let g = scene.genesis.clone();
        let a1 = scene.block(&[&g], 1);
        let a2 = scene.block(&[&a1], 2);
        let a3 = scene.block(&[&a2], 3);
        scene.see(&a1, 11);
        scene.see(&a2, 12);
        scene.see(&a3, 13);
        scene.unjudged.insert(a2.hash());
        scene.mine_to(20);
        let mut f = Finality::new(CONFS, g.hash());
        let outcome = scene.evaluate(&mut f);
        assert_eq!(final_chain(&f), hashes(&[&a1]));
        assert_eq!(outcome.validating, Some(a2.hash()));
        assert_eq!(f.evaluated_height(), Some(16));
        assert_eq!(scene.evaluate(&mut f).validating, Some(a2.hash()));
        // judged valid: decided at the height it was waiting at, then the rest
        scene.unjudged.clear();
        let outcome = scene.evaluate(&mut f);
        assert_eq!(outcome.validating, None);
        assert!(outcome.advanced);
        assert_eq!(final_chain(&f), hashes(&[&a1, &a2, &a3]));
        assert_eq!(f.decisions()[1].btc_height, 17);
        assert_eq!(f.decisions()[2].btc_height, 18);
        assert_eq!(f.evaluated_height(), Some(20));

        // judged invalid instead: excluded at the height it was waiting at
        let mut f = Finality::new(CONFS, g.hash());
        scene.unjudged.insert(a2.hash());
        assert_eq!(scene.evaluate(&mut f).validating, Some(a2.hash()));
        scene.unjudged.clear();
        scene.invalid.insert(a2.hash());
        let outcome = scene.evaluate(&mut f);
        assert_eq!(outcome.excluded, vec![(a2.hash(), Exclusion::Invalid)]);
        assert_eq!(f.excluded_at(&a2.hash()), Some(17));
        assert_eq!(final_chain(&f), hashes(&[&a1]));
        assert_eq!(f.evaluated_height(), Some(20));
        let scratch = scene.evaluated_from_scratch();
        assert_eq!(scratch.decisions(), f.decisions());
        assert_eq!(scratch.excluded(), f.excluded());
    }

    #[test]
    fn a_decision_at_a_paused_height_counts_only_once_the_height_is_evaluated() {
        let mut scene = Scene::new();
        let g = scene.genesis.clone();
        let a1 = scene.block(&[&g], 1);
        let a2 = scene.block(&[&a1], 2);
        // both land in the same BTC block: decided at the same height, one after the other
        scene.see(&a1, 11);
        scene.see(&a2, 11);
        scene.unjudged.insert(a2.hash());
        scene.mine_to(20);
        let mut f = Finality::new(CONFS, g.hash());
        let outcome = scene.evaluate(&mut f);
        assert_eq!(outcome.validating, Some(a2.hash()));
        assert_eq!(final_chain(&f), hashes(&[&a1]));
        assert_eq!(f.decisions()[0].btc_height, 16);
        assert_eq!(f.evaluated_height(), Some(15));
        // a1 is final, but height 16 is not evaluated yet: by no height is anything above the root decided
        assert_eq!(f.decided_by(16), Some((g.hash(), 0)));
        assert_eq!(f.decided_by(20), Some((g.hash(), 0)));
        scene.unjudged.clear();
        scene.evaluate(&mut f);
        assert_eq!(final_chain(&f), hashes(&[&a1, &a2]));
        assert_eq!(f.evaluated_height(), Some(20));
        assert_eq!(f.decided_by(15), Some((g.hash(), 0)));
        assert_eq!(f.decided_by(16), Some((a2.hash(), 2)));
    }

    #[test]
    fn a_btc_reorg_revokes_the_exclusions_made_on_the_orphaned_blocks() {
        let (mut scene, [a1, a2, x2, x3]) = invalid_branch_lands_first();
        let g = scene.genesis.clone();
        scene.mine_to(18);
        let mut f = Finality::new(CONFS, g.hash());
        scene.evaluate(&mut f);
        assert_eq!(final_chain(&f), hashes(&[&a1, &a2]));
        assert_eq!(f.excluded_at(&x2.hash()), Some(17));
        // BTC reorg to 16: a2's decision (18) and x2's exclusion (17) are revoked, a1's (16) stays
        scene.snapshot.truncate_for_test(16);
        let outcome = scene.evaluate(&mut f);
        assert!(outcome.rolled_back);
        assert_eq!(final_chain(&f), hashes(&[&a1]));
        assert_eq!(f.excluded_at(&x2.hash()), None);
        assert!(f.excluded().is_empty());
        // decided again on the new branch, where x2 passes: the x chain is final
        scene.invalid.clear();
        scene.mine_to(20);
        scene.evaluate(&mut f);
        assert_eq!(final_chain(&f), hashes(&[&a1, &x2, &x3]));
        assert!(f.excluded().is_empty());
        let scratch = scene.evaluated_from_scratch();
        assert_eq!(scratch.decisions(), f.decisions());
    }

    #[test]
    fn a_block_merging_an_excluded_block_can_become_final() {
        let (mut scene, [a1, a2, x2, _]) = invalid_branch_lands_first();
        let g = scene.genesis.clone();
        // a3 on the honest chain, m on a3 also merging x2 (a sibling of a2, not in a3's past)
        let a3 = scene.block(&[&a2], 5);
        let m = scene.block(&[&a3, &x2], 6);
        assert_eq!(m.parents[0], a3.hash(), "a3 is m's selected parent");
        scene.see(&a3, 18);
        scene.see(&m, 19);
        scene.mine_to(30);
        let mut f = Finality::new(CONFS, g.hash());
        scene.evaluate(&mut f);
        assert_eq!(f.excluded_at(&x2.hash()), Some(17));
        assert_eq!(final_chain(&f), hashes(&[&a1, &a2, &a3, &m]));
        assert_eq!(f.decisions()[3].btc_height, 24);
        assert!(scene.asked.borrow().contains(&m.hash()));
    }

    /// A block whose anchor names no block of the BTC chain (D-101, the review of the change): excluded as soon as the walk
    /// reaches it, without asking for its landing status or its verdict, so the decision does not wait on it; the next
    /// best candidate becomes final. (Since D-135 the tracker never counts such a header; the check stays.)
    #[test]
    fn a_block_anchored_off_the_btc_chain_is_excluded_before_anything_is_asked() {
        let mut scene = Scene::new();
        let g = scene.genesis.clone();
        let a1 = scene.block(&[&g], 1);
        let fabricated = BtcAnchor {
            height: 12,
            hash: [0xab; 32],
        };
        let x2 = scene.block_on(&[&a1], 2, fabricated);
        let x3 = scene.block(&[&x2], 3);
        let b2 = scene.block(&[&a1], 4);
        scene.see(&a1, 11);
        scene.see(&x2, 13);
        scene.see(&x3, 13);
        scene.see(&b2, 14);
        scene.unjudged.insert(x2.hash());
        let mut f = Finality::new(CONFS, g.hash());
        scene.mine_to(15);
        assert_eq!(scene.evaluate(&mut f), Outcome::default());
        // 16: a1 is final; the walk reaches x2 (x3 is the candidate): excluded for its anchor, with x3 through it; b2
        // is the candidate, landed at 14
        scene.mine_to(16);
        let outcome = scene.evaluate(&mut f);
        assert_eq!(
            outcome.excluded,
            vec![(x2.hash(), Exclusion::Unanchored(x2.hash()))]
        );
        assert_eq!(outcome.validating, None);
        assert_eq!(final_chain(&f), hashes(&[&a1]));
        assert_eq!(f.excluded_at(&x2.hash()), Some(16));
        assert_eq!(f.excluded_at(&x3.hash()), None, "excluded through x2");
        assert!(!scene.asked.borrow().contains(&x2.hash()));
        scene.mine_to(19);
        scene.evaluate(&mut f);
        assert_eq!(final_chain(&f), hashes(&[&a1, &b2]));
        assert_eq!(f.evaluated_height(), Some(19));
        let scratch = scene.evaluated_from_scratch();
        assert_eq!(scratch.decisions(), f.decisions());
        assert_eq!(scratch.excluded(), f.excluded());
    }

    /// A block merging a block anchored off the BTC chain is excluded the same way, for the merged block's anchor; the
    /// merged block itself is never a candidate (its selected chain does not pass through the finalized tip).
    #[test]
    fn a_block_merging_a_block_anchored_off_the_btc_chain_is_excluded() {
        let mut scene = Scene::new();
        let g = scene.genesis.clone();
        let a1 = scene.block(&[&g], 1);
        let a2 = scene.block(&[&a1], 2);
        let y = scene.block_on(
            &[&g],
            3,
            BtcAnchor {
                height: 11,
                hash: [0xab; 32],
            },
        );
        // m3 anchors on the BTC chain, not below y's anchor height
        let m3 = scene.block_on(
            &[&a2, &y],
            4,
            BtcAnchor {
                height: 11,
                hash: [11; 32],
            },
        );
        assert_eq!(m3.parents[0], a2.hash(), "a2 is m3's selected parent");
        let b3 = scene.block(&[&a2], 5);
        scene.see(&a1, 11);
        scene.see(&a2, 12);
        scene.see(&y, 12);
        scene.see(&m3, 13);
        scene.see(&b3, 14);
        scene.mine_to(17);
        let mut f = Finality::new(CONFS, g.hash());
        let outcome = scene.evaluate(&mut f);
        assert_eq!(
            outcome.excluded,
            vec![(m3.hash(), Exclusion::Unanchored(y.hash()))]
        );
        assert_eq!(final_chain(&f), hashes(&[&a1, &a2]));
        assert_eq!(f.excluded_at(&m3.hash()), Some(17));
        scene.mine_to(19);
        scene.evaluate(&mut f);
        assert_eq!(final_chain(&f), hashes(&[&a1, &a2, &b3]));
        assert!(!scene.asked.borrow().contains(&m3.hash()));
        assert_eq!(scene.evaluated_from_scratch().decisions(), f.decisions());
    }

    /// The exclusion of a block anchored off the chain is made at a BTC height like any other: a BTC reorg below it
    /// revokes it, and on a branch that holds the anchor block the block can become final.
    #[test]
    fn a_btc_reorg_onto_the_anchor_revokes_the_exclusion() {
        let mut scene = Scene::new();
        let g = scene.genesis.clone();
        let a1 = scene.block(&[&g], 1);
        let anchor = BtcAnchor {
            height: 12,
            hash: [0xab; 32],
        };
        let x2 = scene.block_on(&[&a1], 2, anchor.clone());
        scene.see(&a1, 11);
        scene.see(&x2, 13);
        scene.mine_to(16);
        let mut f = Finality::new(CONFS, g.hash());
        scene.evaluate(&mut f);
        assert_eq!(final_chain(&f), hashes(&[&a1]));
        assert_eq!(f.excluded_at(&x2.hash()), Some(16));
        // BTC reorg to 11; the new branch holds x2's anchor block at 12: a1's decision (16) and x2's exclusion (16) are
        // revoked
        scene.snapshot.truncate_for_test(11);
        scene
            .snapshot
            .push_block(anchor.hash, 1_790_000_000 + 12 * 600);
        let outcome = scene.evaluate(&mut f);
        assert!(outcome.rolled_back);
        assert_eq!(f.excluded_at(&x2.hash()), None);
        assert!(f.excluded().is_empty());
        // x2's header appears again at 13: final at 18, after a1 at 16
        scene.see(&x2, 13);
        scene.mine_to(18);
        scene.evaluate(&mut f);
        assert_eq!(final_chain(&f), hashes(&[&a1, &x2]));
        assert_eq!(f.decisions()[1].btc_height, 18);
        assert!(scene.asked.borrow().contains(&x2.hash()));
    }

    /// The hash of the BTC block at `height` on branch `branch`: distinct from the blocks [`Scene::mine_to`] mines and
    /// from those of other branches (a real block hash commits to the whole chain below it).
    fn branch_hash(height: u32, branch: u8) -> [u8; 32] {
        let mut hash = [branch; 32];
        hash[..4].copy_from_slice(&height.to_le_bytes());
        hash
    }

    /// Mines BTC blocks of branch `branch` up to height `height`.
    fn extend(scene: &mut Scene, height: u32, branch: u8) {
        while scene.btc_tip() < height {
            let next = scene.btc_tip() + 1;
            scene
                .snapshot
                .push_block(branch_hash(next, branch), 1_790_000_000 + next * 600);
        }
    }

    /// A BTC reorg replaces the blocks from height `from` up with branch `branch` up to height `to`; its first block
    /// carries `headers`.
    fn reorg(scene: &mut Scene, from: u32, to: u32, branch: u8, headers: &[&BlockHeader]) {
        scene.snapshot.truncate_for_test(from - 1);
        extend(scene, to, branch);
        for header in headers {
            scene.snapshot.see_header(header.hash(), from);
        }
    }

    /// The audit's scenario (D-114): a decision rests on the data of the block's closure, `BTC_FINALITY_CONFS` blocks
    /// deep, and on there being no heavier candidate connected by the height decided, which is only as deep as the BTC
    /// block that made the decision. Right after a1 is decided, a reorg of just that block, whose new block carries a
    /// heavier chain forking below a1, revokes a1 for good. a1 is reported BTC-final only once that BTC block has
    /// `BTC_FINALITY_CONFS` confirmations: then a reorg that does not reach it, even onto a branch shorter for now,
    /// takes nothing back. One reaching it with a heavier chain on the new branch takes a1 off the finalized chain.
    /// Without one, a1 is decided again on the new branch: onto a branch as long it stays reported until a reorg
    /// removes its data, `2 × BTC_FINALITY_CONFS − 1` blocks, and onto a branch shorter for now it is taken back until
    /// the BTC block of the new decision has `BTC_FINALITY_CONFS` confirmations.
    #[test]
    fn a_decision_is_reported_btc_final_once_the_btc_block_that_made_it_is_deep() {
        // a1's data lands at 11, so it is decided at 16; the heavier b chain forks below it
        let scene_with_a1 = || {
            let mut scene = Scene::new();
            let g = scene.genesis.clone();
            let a1 = scene.block(&[&g], 1);
            let b1 = scene.block(&[&g], 2);
            let b2 = scene.block(&[&b1], 3);
            scene.see(&a1, 11);
            scene.mine_to(16);
            let f = Finality::new(CONFS, g.hash());
            (scene, f, a1, b1, b2)
        };

        // decided at 16, not reported while BTC block 16 has fewer than 6 confirmations
        let (mut scene, mut f, a1, b1, b2) = scene_with_a1();
        let root = BtcFinal::from(f.root());
        scene.evaluate(&mut f);
        assert_eq!(final_chain(&f), hashes(&[&a1]));
        assert_eq!(f.decisions()[0].btc_height, 16);
        let reported = f.btc_final(&scene.snapshot, None);
        assert_eq!(reported, root);
        // a reorg of block 16 alone, the new one carrying the b chain: a1 is not final any more, and nothing reported
        // is taken back
        reorg(&mut scene, 16, 16, 0xb1, &[&b1, &b2]);
        assert!(scene.evaluate(&mut f).rolled_back);
        assert_eq!(final_chain(&f), Vec::<Hash>::new());
        assert_eq!(f.btc_final(&scene.snapshot, Some(&reported)), root);
        // the b chain is decided at 21 and reported at 26; a1 never comes back
        extend(&mut scene, 21, 0xb1);
        scene.evaluate(&mut f);
        assert_eq!(final_chain(&f), hashes(&[&b1, &b2]));
        assert_eq!(f.btc_final(&scene.snapshot, Some(&reported)), root);
        extend(&mut scene, 25, 0xb1);
        scene.evaluate(&mut f);
        assert_eq!(f.btc_final(&scene.snapshot, Some(&reported)), root);
        extend(&mut scene, 26, 0xb1);
        scene.evaluate(&mut f);
        let now = f.btc_final(&scene.snapshot, Some(&reported));
        assert_eq!((now.hash, now.height), (b2.hash(), 2));
        assert_eq!(now.decided, Some((21, branch_hash(21, 0xb1))));
        assert!(!f.taken_back(&reported, &now));
        assert_eq!(
            final_chain(&scene.evaluated_from_scratch()),
            final_chain(&f)
        );

        // reported at 21, when BTC block 16 has 6 confirmations
        let (mut scene, mut f, a1, b1, b2) = scene_with_a1();
        scene.mine_to(20);
        scene.evaluate(&mut f);
        assert_eq!(f.btc_final(&scene.snapshot, None), root);
        scene.mine_to(21);
        scene.evaluate(&mut f);
        let reported = f.btc_final(&scene.snapshot, None);
        assert_eq!((reported.hash, reported.height), (a1.hash(), 1));
        assert_eq!(reported.decided, Some((16, [16; 32])));
        // a reorg of the 5 blocks above block 16, onto a branch carrying the b chain and shorter for now: a1 stays
        // final, and reported, though block 16 has only 2 confirmations now
        reorg(&mut scene, 17, 17, 0xb1, &[&b1, &b2]);
        scene.evaluate(&mut f);
        assert_eq!(final_chain(&f), hashes(&[&a1]));
        let now = f.btc_final(&scene.snapshot, Some(&reported));
        assert_eq!(now, reported);
        assert!(!f.taken_back(&reported, &now));
        assert_eq!(f.btc_final(&scene.snapshot, None), root);
        extend(&mut scene, 21, 0xb1);
        scene.evaluate(&mut f);
        assert_eq!(final_chain(&f), hashes(&[&a1]));
        assert_eq!(f.btc_final(&scene.snapshot, Some(&reported)), reported);
        // a reorg reaching block 16, 6 blocks deep, with the b chain on the new branch takes a1 back
        reorg(&mut scene, 16, 21, 0xb2, &[&b1, &b2]);
        scene.evaluate(&mut f);
        assert_eq!(final_chain(&f), hashes(&[&b1, &b2]));
        let now = f.btc_final(&scene.snapshot, Some(&reported));
        assert_eq!(now, root);
        assert!(f.taken_back(&reported, &now));

        // without a heavier chain, a reorg reaching block 16 decides a1 again on the new block 16, whose confirmations
        // keep it reported; only one removing its data at 11, 11 blocks deep, takes it back
        let (mut scene, mut f, a1, _, _) = scene_with_a1();
        scene.mine_to(21);
        scene.evaluate(&mut f);
        let reported = f.btc_final(&scene.snapshot, None);
        reorg(&mut scene, 16, 21, 0xb1, &[]);
        scene.evaluate(&mut f);
        assert_eq!(f.decisions()[0].btc_hash, branch_hash(16, 0xb1));
        let now = f.btc_final(&scene.snapshot, Some(&reported));
        assert_eq!((now.hash, now.height), (a1.hash(), 1));
        assert!(!f.taken_back(&reported, &now));
        let reported = now;
        reorg(&mut scene, 11, 21, 0xb2, &[]);
        scene.evaluate(&mut f);
        assert_eq!(final_chain(&f), Vec::<Hash>::new());
        let now = f.btc_final(&scene.snapshot, Some(&reported));
        assert_eq!(now, root);
        assert!(f.taken_back(&reported, &now));

        // onto a branch shorter for now, a1 is decided again on the new block 16 but taken back until that block has
        // 6 confirmations
        let (mut scene, mut f, a1, _, _) = scene_with_a1();
        scene.mine_to(21);
        scene.evaluate(&mut f);
        let reported = f.btc_final(&scene.snapshot, None);
        reorg(&mut scene, 16, 17, 0xb1, &[]);
        scene.evaluate(&mut f);
        assert_eq!(final_chain(&f), hashes(&[&a1]));
        let now = f.btc_final(&scene.snapshot, Some(&reported));
        assert_eq!(now, root);
        assert!(f.taken_back(&reported, &now));
        extend(&mut scene, 21, 0xb1);
        scene.evaluate(&mut f);
        let again = f.btc_final(&scene.snapshot, Some(&now));
        assert_eq!((again.hash, again.height), (a1.hash(), 1));
        assert_eq!(again.decided, Some((16, branch_hash(16, 0xb1))));
    }

    /// The height the evaluation paused at (here waiting for a verdict, D-101) is evaluated again from the start when
    /// it resumes (D-114): a1 was decided there before the pause, on the BTC block then at that height. A reorg
    /// replacing just that block, whose new block carries a heavier chain forking below a1, revokes the decision, and
    /// the result is the same as evaluating the new chain from scratch.
    #[test]
    fn a_reorg_of_the_height_the_evaluation_paused_at_revokes_what_was_decided_there() {
        let mut scene = Scene::new();
        let g = scene.genesis.clone();
        let a1 = scene.block(&[&g], 1);
        let a2 = scene.block(&[&a1], 2);
        scene.see(&a1, 11);
        scene.see(&a2, 11);
        scene.unjudged.insert(a2.hash());
        scene.mine_to(16);
        let mut f = Finality::new(CONFS, g.hash());
        assert_eq!(scene.evaluate(&mut f).validating, Some(a2.hash()));
        assert_eq!(final_chain(&f), hashes(&[&a1]));
        assert_eq!(f.evaluated_height(), Some(15));
        // block 16 is replaced by one carrying the heavier b chain, and a2's verdict arrives
        let b1 = scene.block(&[&g], 3);
        let b2 = scene.block(&[&b1], 4);
        let b3 = scene.block(&[&b2], 5);
        reorg(&mut scene, 16, 16, 0xb1, &[&b1, &b2, &b3]);
        scene.unjudged.clear();
        let outcome = scene.evaluate(&mut f);
        assert_eq!(final_chain(&f), Vec::<Hash>::new());
        assert!(outcome.rolled_back);
        extend(&mut scene, 21, 0xb1);
        scene.evaluate(&mut f);
        assert_eq!(final_chain(&f), hashes(&[&b1, &b2, &b3]));
        let scratch = scene.evaluated_from_scratch();
        assert_eq!(f.decisions(), scratch.decisions());
        // the same block 16 back with the verdict still missing: what was decided there is decided again, and nothing
        // is reported as rolled back
        let mut scene = Scene::new();
        let g = scene.genesis.clone();
        let a1 = scene.block(&[&g], 1);
        let a2 = scene.block(&[&a1], 2);
        scene.see(&a1, 11);
        scene.see(&a2, 11);
        scene.unjudged.insert(a2.hash());
        scene.mine_to(16);
        let mut f = Finality::new(CONFS, g.hash());
        scene.evaluate(&mut f);
        let outcome = scene.evaluate(&mut f);
        assert_eq!(outcome.validating, Some(a2.hash()));
        assert!(!outcome.rolled_back && !outcome.advanced);
        assert_eq!(final_chain(&f), hashes(&[&a1]));
    }
}
