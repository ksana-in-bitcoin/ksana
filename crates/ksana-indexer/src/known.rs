//! What the indexer knows about fast-lane blocks (D-043, D-048, D-063, D-082, D-135): the block DAG, the block bodies
//! and BTC finality.
//!
//! - **Headers** ([`Known::count`]): the tracker asks about a header carried by a payload that counts once its anchor
//!   is on the BTC best chain and its parents have counted (D-135); it counts when the block DAG takes it, by the same
//!   rules as the nodes.
//! - **Block bodies**: decoded from the BTC snapshot by [`ksana_btc::bodies`], shared by the nodes and the indexer (the
//!   forced-inclusion entries derived from BTC, a body that needs to land taken from the transaction that landed it,
//!   checked when its BTC block was scanned).
//! - **Finality** ([`Known::finalize`], D-082): after every BTC sync the final chain is decided BTC height by BTC
//!   height per the rules of [`ksana_btc::finality`], the landing status of each block's data being the snapshot's (the
//!   same rules as the settled closure); the verdict on a block meeting the landing condition (D-101) is answered
//!   from the verdicts recorded on the current BTC chain ([`Verdicts`]: the executions of main-chain blocks and the
//!   judging of the blocks the decision waited on, [`Known::record_verdict`]) and from the marks of blocks unable to
//!   land; the final tip becomes the main-chain floor of the block DAG ([`HeaderDag::set_floor`]), and a final block
//!   cannot be marked as unable to land.
//! - **Processable prefix** ([`Known::plan`]): the longest prefix of the main chain from the genesis block in which the
//!   data of each block and of the blocks it merges has landed (the settled closure, D-038, D-063); after pruning it
//!   starts at the pruning root (D-097).
//! - **Pruning** ([`Known::prune`], D-097): the block DAG keeps the pruning point and its future and the finality
//!   decision continues from the pruning root; [`Known::record`] and [`Known::restore`] carry both through a pruning
//!   checkpoint.

use std::collections::BTreeSet;

use ksana_btc::bodies::Bodies;
use ksana_btc::finality::{Finality, Outcome, Validity};
use ksana_btc::headers::{HeaderDag, HeaderDagRecord, Insert};
use ksana_btc::tracker::{BlockSummary, BtcSnapshot, MainEntry};
use ksana_btc::verdict::{self, Candidate, StoredVerdicts, Verdicts};
use ksana_consensus::PowConsensus;
use ksana_primitives::{BlockHeader, Params};

/// The processable prefix of the main chain (D-038, D-063): the longest main-chain prefix in which the data of each
/// block and of the blocks it merges has landed; from height [`Plan::base`] up.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Plan {
    /// The height of the first entry: 0 (the genesis block), or the pruning root's after pruning (D-097: no main chain
    /// still reachable forks below it).
    pub base: u64,
    /// Summaries of the blocks in the prefix (including the blocks they merge; index = height − `base`).
    pub entries: Vec<MainEntry>,
    /// The `state_root` of each block header in the prefix (index = height − `base`).
    pub state_roots: Vec<[u8; 32]>,
}

impl Plan {
    /// The highest height of the processable prefix; `base − 1` when it is empty (nothing processable from `base` on).
    pub fn processable(&self) -> u64 {
        let len = u64::try_from(self.entries.len()).unwrap_or(u64::MAX);
        self.base.saturating_add(len).saturating_sub(1)
    }

    fn index(&self, height: u64) -> Option<usize> {
        usize::try_from(height.checked_sub(self.base)?).ok()
    }

    /// Summary of the block at that height in the prefix.
    pub fn entry(&self, height: u64) -> Option<&MainEntry> {
        self.entries.get(self.index(height)?)
    }

    /// The `state_root` of the block at that height in the prefix.
    pub fn state_root(&self, height: u64) -> Option<[u8; 32]> {
        self.state_roots.get(self.index(height)?).copied()
    }
}

/// Result of one round of finality decision ([`Known::finalize`]).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Finalized {
    /// Progress of the decision.
    pub outcome: Outcome,
    /// The final chain contains a block this indexer has judged unable to land (a final block is invalid): the reason.
    /// The indexer must halt and raise an alarm (D-082).
    pub conflict: Option<String>,
    /// The main chain retreated below this main-chain block at this height because a BTC reorg took its anchor, or the
    /// anchor of a block it merges, off the best chain ([`HeaderDag::reselect`], D-104).
    pub retreated: Option<([u8; 32], u64)>,
}

/// The block DAG, the decoder of the block bodies, and BTC finality.
pub struct Known {
    params: Params,
    consensus: PowConsensus,
    /// The genesis block's hash: it always counts and carries no body (D-135).
    genesis: [u8; 32],
    tree: HeaderDag,
    finality: Finality,
    bodies: Bodies,
    /// The verdicts on blocks recorded on their basis (D-101); the indexer also keeps them in its index, asked when
    /// none is recorded here (after a restart).
    verdicts: Verdicts,
    /// Block header versions unknown to this indexer that have already been alarmed (D-076, one alarm per version; at
    /// most [`MAX_UNKNOWN_VERSIONS`] of them).
    unknown_versions: BTreeSet<u32>,
}

/// At most this many unknown block versions are remembered for the one-alarm-per-version rule (D-076).
const MAX_UNKNOWN_VERSIONS: usize = 16;

/// What [`Known::record`] writes into a pruning checkpoint (D-097): the block DAG and the finality decision.
pub type KnownRecord = (HeaderDagRecord, Finality);

impl Known {
    /// Knows only the genesis block.
    pub fn new(
        params: Params,
        consensus: PowConsensus,
        genesis: BlockHeader,
    ) -> Result<Self, String> {
        let genesis_hash = genesis.hash();
        let tree = HeaderDag::new(&params, &consensus, genesis)?;
        Ok(Known {
            finality: Finality::new(params.btc_finality_confs, genesis_hash),
            bodies: Bodies::new(params.clone(), genesis_hash),
            params,
            consensus,
            genesis: genesis_hash,
            tree,
            verdicts: Verdicts::default(),
            unknown_versions: BTreeSet::new(),
        })
    }

    /// Continues from the records of a pruning checkpoint ([`Known::record`], D-097) of the chain whose genesis block
    /// is `genesis`.
    pub fn restore(
        params: Params,
        consensus: PowConsensus,
        genesis: [u8; 32],
        record: KnownRecord,
    ) -> Result<Self, String> {
        let (headers, finality) = record;
        Ok(Known {
            tree: HeaderDag::from_record(&params, headers)?,
            finality,
            bodies: Bodies::new(params.clone(), genesis),
            params,
            consensus,
            genesis,
            verdicts: Verdicts::default(),
            unknown_versions: BTreeSet::new(),
        })
    }

    /// The block DAG and the finality decision in the local format of a pruning checkpoint (D-097).
    pub fn record(&self) -> Result<KnownRecord, String> {
        Ok((self.tree.record()?, self.finality.clone()))
    }

    /// Prunes at the pruning point `point` (D-097): the block DAG keeps it and its future, and the finality decision
    /// continues from the pruning root `root` and drops the BTC heights below `cut`. The DAG's main-chain floor must
    /// already be the final tip ([`Known::finalize`]). Returns the number of blocks dropped from the DAG.
    pub fn prune(&mut self, point: &[u8; 32], root: &[u8; 32], cut: u32) -> Result<usize, String> {
        let dropped = self.tree.prune(point)?;
        self.finality.prune(&self.tree, root, cut)?;
        let tree = &self.tree;
        self.verdicts.retain(|h| tree.dag().contains(h));
        Ok(dropped)
    }

    /// The block DAG.
    pub fn tree(&self) -> &HeaderDag {
        &self.tree
    }

    /// The BTC finality decision (D-082).
    pub fn finality(&self) -> &Finality {
        &self.finality
    }

    /// Marks a block as unable to land (including a block that is invalid or merges an invalid block, which the nodes
    /// reject) by a verdict given on `basis` (D-101) and re-selects the main chain under `snapshot` (D-063, D-104). A
    /// final block cannot be marked (D-082): returns an error, and the caller halts and raises an alarm.
    pub fn disqualify(
        &mut self,
        hash: &[u8; 32],
        basis: Option<[u8; 32]>,
        snapshot: &BtcSnapshot,
    ) -> Result<(), String> {
        self.tree.disqualify(hash, basis, snapshot)
    }

    /// Records the verdict on a block given on `basis` (D-101): from executing it as a main-chain block, or from
    /// judging it for the finality decision.
    pub fn record_verdict(&mut self, hash: [u8; 32], valid: bool, basis: Option<[u8; 32]>) {
        self.verdicts.record(hash, valid, basis);
    }

    /// Block `hash` prepared for judging or executing as a main-chain block ([`verdict::prepare`]): `None` while a body
    /// (its own or a merged block's) has not landed.
    pub fn candidate(
        &mut self,
        hash: &[u8; 32],
        snapshot: &BtcSnapshot,
    ) -> Result<Option<Candidate>, String> {
        let Known {
            params,
            tree,
            bodies,
            ..
        } = self;
        let tree: &HeaderDag = tree;
        verdict::prepare(params, tree, snapshot, hash, &mut |h| {
            bodies.body(snapshot, h)
        })
    }

    /// Whether `header`, which the tracker asks about once its anchor is on the BTC best chain and its parents have
    /// counted (D-135), counts: the block DAG takes it, or holds it already (`snapshot`, the BTC snapshot being built,
    /// gives the header time of the anchored BTC block, D-083, and the BTC chain the main chain is chosen under,
    /// D-104). Returns an error when the header cannot be checked locally (e.g. the PoW value cannot be computed,
    /// D-094): a local fault, the caller stops this round and raises an alarm, and the header is not treated as
    /// invalid.
    pub fn count(&mut self, header: &BlockHeader, snapshot: &BtcSnapshot) -> Result<bool, String> {
        let max_known = self.params.max_known_version();
        if header.version > max_known {
            // An upgrade unknown to this indexer (D-076): the header cannot be judged and does not count here, so the
            // indexer stops before the fork
            if self.unknown_versions.len() < MAX_UNKNOWN_VERSIONS
                && self.unknown_versions.insert(header.version)
            {
                eprintln!(
                    "[ksana-indexer] alarm (highest level): the payload contains a block header of protocol version {}, but this indexer only supports up to {max_known}: \
                     the network may have upgraded; the indexer software needs upgrading (D-076)",
                    header.version
                );
            }
            return Ok(false);
        }
        match self.tree.insert(&self.consensus, header.clone(), snapshot) {
            Insert::Connected(_) | Insert::Duplicate => Ok(true),
            Insert::Invalid(reason) => {
                eprintln!(
                    "[ksana-indexer] discarding invalid block header {} (height {}): {reason}",
                    hex::encode(header.hash()),
                    header.height
                );
                Ok(false)
            }
            Insert::Unavailable(reason) => {
                eprintln!(
                    "[ksana-indexer] alarm (highest level): cannot check block header {} (height {}) in the payload: {reason}",
                    hex::encode(header.hash()),
                    header.height
                );
                Err(format!("cannot check block header: {reason}"))
            }
            // Below the pruning point, or on a block pruned away (D-097): it can be neither on nor merged by a chain
            // through the final blocks
            Insert::Pruned | Insert::MissingParent => Ok(false),
        }
    }

    /// Advances the BTC finality decision by the snapshot (D-082), sets the final tip as the main-chain floor of the
    /// block DAG (re-selecting when the main chain does not pass through it), then re-selects the main chain under the
    /// snapshot, retreating it below a main-chain block a BTC reorg made ineligible ([`HeaderDag::reselect`], D-104,
    /// recorded in [`Finalized::retreated`]). The verdict on a block meeting the landing condition (D-101) comes from
    /// the verdicts recorded on the current BTC chain (here or in `stored`, the ones kept on disk), or from a mark of
    /// the block as unable to land given on it; without either the decision waits ([`Outcome::validating`]), and the
    /// indexer judges the block and decides again. The marks on the final chain whose basis a BTC reorg changed are
    /// dropped; when the final chain contains a block still marked, the floor is unchanged and the reason is recorded
    /// in [`Finalized::conflict`].
    pub fn finalize(
        &mut self,
        snapshot: &BtcSnapshot,
        stored: &StoredVerdicts,
    ) -> Result<Finalized, String> {
        let Known {
            tree,
            finality,
            verdicts,
            ..
        } = self;
        let outcome = {
            let tree: &HeaderDag = tree;
            let verdicts: &Verdicts = verdicts;
            finality.evaluate(tree, snapshot, &mut |hash| {
                Ok(match verdicts.validity(tree, snapshot, hash, stored)? {
                    Validity::Undetermined => {
                        let basis = tree.header(hash).and_then(|h| verdict::basis(snapshot, h));
                        if tree.disqualified_on(hash, basis) {
                            Validity::Invalid
                        } else {
                            Validity::Undetermined
                        }
                    }
                    known => known,
                })
            })?
        };
        tree.requalify(&finality.tip(), &|header| verdict::basis(snapshot, header));
        let conflict = tree.set_floor(finality.tip(), snapshot).err();
        let retreated = tree.reselect(snapshot);
        Ok(Finalized {
            outcome,
            conflict,
            retreated,
        })
    }

    /// Height of the BTC-final tip (D-082).
    pub fn finalized_height(&self) -> u64 {
        self.finality.tip_height()
    }

    /// Summary of a block whose header counts on the snapshot's chain (D-135; the genesis block always does):
    /// `rewarded` is whether it earns a block reward (a main-chain block itself, or blue in the merge set of the
    /// main-chain block merging it, D-074, D-079).
    fn summary(
        &self,
        hash: &[u8; 32],
        snapshot: &BtcSnapshot,
        rewarded: bool,
    ) -> Option<BlockSummary> {
        let settles = if *hash == self.genesis {
            false
        } else {
            snapshot.counted(hash)?.settles
        };
        let (header, block) = (self.tree.header(hash)?, self.tree.dag().get(hash)?);
        Some(BlockSummary {
            hash: *hash,
            settles,
            anchor_height: header.btc_anchor.height,
            producer: header.producer,
            daa_score: block.data.daa_score,
            weight: block.weight,
            rewarded,
        })
    }

    /// The processable prefix of the main chain (D-038, D-063): the consecutive main-chain blocks from the genesis
    /// block in which the data landing heights of each block and of the blocks it merges (the merge set except the
    /// selected parent, in consensus order) are all defined (i.e. the settled closure, the longest prefix where
    /// `prefix_height` is defined), and which have `SETTLEMENT_DEPTH` confirmations on our main
    /// chain or are BTC-final (D-082: the main chain must pass through the final blocks, which no longer need
    /// confirmations).
    ///
    /// The confirmation requirement (D-063): the settlement of a merged parallel block may appear on BTC first (a node
    /// broadcasts it as usual once the main-chain block merging it has `SETTLEMENT_DEPTH` confirmations), while the
    /// header of the main-chain block at the same height is brought up later by another payload; when only the former
    /// is seen it is temporarily the tip of our main chain and must not be executed as a main-chain block. Whenever a
    /// node broadcasts a settlement its main chain has already gone `SETTLEMENT_DEPTH` blocks past that block, and the
    /// headers of those blocks will land afterwards (settlement payload or republish, D-047).
    ///
    /// The prefix starts at height `from` (the pruning root's height after pruning, D-097: the main-chain blocks below it
    /// are final and executed, and those just above the pruning point may merge blocks pruned away).
    pub fn plan(&self, snapshot: &BtcSnapshot, from: u64) -> Plan {
        let main_base = self.tree.main_base();
        let base = from.max(main_base);
        let depth = usize::try_from(self.params.settlement_depth).unwrap_or(usize::MAX);
        let final_len = usize::try_from(self.tree.floor_height().saturating_sub(main_base))
            .unwrap_or(usize::MAX)
            .saturating_add(1);
        let all = self.tree.main();
        let len = all
            .len()
            .saturating_sub(depth)
            .max(final_len)
            .min(all.len());
        let skip = usize::try_from(base.saturating_sub(main_base)).unwrap_or(usize::MAX);
        let main: Vec<[u8; 32]> = all.iter().take(len).skip(skip).copied().collect();
        let mut plan = Plan {
            base,
            entries: Vec::with_capacity(main.len()),
            state_roots: Vec::with_capacity(main.len()),
        };
        'blocks: for hash in main {
            let Some(own) = self.summary(&hash, snapshot, true) else {
                break;
            };
            let Some(data) = self.tree.ghostdag(&hash) else {
                break;
            };
            let mut merged = Vec::with_capacity(data.mergeset_ordered.len());
            for m in &data.mergeset_ordered {
                let Some(summary) = self.summary(m, snapshot, data.is_merged_blue(m)) else {
                    break 'blocks;
                };
                merged.push(summary);
            }
            let Some(header) = self.tree.header(&hash) else {
                break;
            };
            let entry = MainEntry {
                hash,
                settles: own.settles,
                anchor_height: own.anchor_height,
                producer: own.producer,
                daa_score: own.daa_score,
                weight: own.weight,
                merged,
            };
            // The genesis block does not count toward the settled prefix (D-038)
            if header.height > 0
                && entry
                    .accepted()
                    .any(|block| snapshot.data_height(&block).is_none())
            {
                break;
            }
            plan.state_roots.push(header.state_root);
            plan.entries.push(entry);
        }
        plan
    }
}
