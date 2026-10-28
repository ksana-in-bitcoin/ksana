//! What the indexer knows about fast-lane blocks (D-043, D-048, D-063, D-082): the block DAG, candidate and determined
//! block bodies, and BTC finality.
//!
//! - **Block bodies**: candidate and determined bodies are determined by [`ksana_btc::bodies`], shared by the nodes and
//!   the indexer (forced-inclusion entries are derived from BTC, the rest are taken from the payloads carrying bodies,
//!   blocks whose anchor is invalid are handled per D-056, D-093); the indexer collects all bodies carried by every
//!   scanned payload.
//! - **Body information** ([`Known::block_data`]): the tracker uses it to decide whether a payload on BTC can land a
//!   block's data (a payload that carried the same body, D-077), the same as the nodes.
//! - **Finality** ([`Known::finalize`], D-082): after every BTC sync the final chain is decided BTC height by BTC
//!   height per the rules of [`ksana_btc::finality`]; the landing status of each block's data is answered by the same
//!   rules as the settled closure ("undetermined" while the body cannot be determined yet, pausing the decision); the
//!   verdict on a block meeting the landing condition (D-101) is answered from the verdicts recorded on the current
//!   BTC chain ([`Verdicts`]: the executions of main-chain blocks and the judging of the blocks the decision waited
//!   on, [`Known::record_verdict`]) and from the marks of blocks unable to land; the final tip becomes the main-chain
//!   floor of the block DAG ([`HeaderDag::set_floor`]), and a final block cannot be marked as unable to land.
//! - **Processable prefix** ([`Known::plan`]): the longest prefix of the main chain from the genesis block in which the
//!   bodies of each block and of the blocks it merges are known and their data has landed (the settled closure, D-038,
//!   D-063); after pruning it starts at the pruning root (D-097).
//! - **Pruning** ([`Known::prune`], D-097): the block DAG keeps the pruning point and its future, the finality decision
//!   continues from the pruning root, and the bodies of the blocks no longer kept are dropped; [`Known::record`] and
//!   [`Known::restore`] carry all three through a pruning checkpoint.

use std::collections::BTreeSet;
use std::sync::Arc;

use ksana_btc::bodies::{Bodies, BodiesRecord};
use ksana_btc::finality::{Finality, Outcome, Validity};
use ksana_btc::headers::{HeaderDag, HeaderDagRecord, Insert};
use ksana_btc::payload::needs_body;
use ksana_btc::tracker::{BlockData, BlockSummary, BtcSnapshot, MainEntry, SettlementContext};
use ksana_btc::verdict::{self, Candidate, StoredVerdicts, Verdicts};
use ksana_consensus::PowConsensus;
use ksana_primitives::{BlockHeader, Entry, Params, Payload};

/// The processable prefix of the main chain (D-038, D-063): the longest main-chain prefix in which the bodies of each
/// block and of the blocks it merges are known and their data has landed; from height [`Plan::base`] up.
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

/// The block DAG, candidate and determined block bodies, and BTC finality.
pub struct Known {
    params: Params,
    consensus: PowConsensus,
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

/// What [`Known::record`] writes into a pruning checkpoint (D-097): the block DAG, the finality decision and the
/// bodies.
pub type KnownRecord = (HeaderDagRecord, Finality, BodiesRecord);

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
            bodies: Bodies::new(params.clone(), genesis_hash, "[ksana-indexer]"),
            params,
            consensus,
            tree,
            verdicts: Verdicts::default(),
            unknown_versions: BTreeSet::new(),
        })
    }

    /// Continues from the records of a pruning checkpoint ([`Known::record`], D-097).
    pub fn restore(
        params: Params,
        consensus: PowConsensus,
        record: KnownRecord,
    ) -> Result<Self, String> {
        let (headers, finality, bodies) = record;
        Ok(Known {
            tree: HeaderDag::from_record(&params, headers)?,
            finality,
            bodies: Bodies::from_record(params.clone(), "[ksana-indexer]", bodies),
            params,
            consensus,
            verdicts: Verdicts::default(),
            unknown_versions: BTreeSet::new(),
        })
    }

    /// The block DAG, the finality decision and the bodies in the local format of a pruning checkpoint (D-097).
    pub fn record(&self) -> Result<KnownRecord, String> {
        Ok((
            self.tree.record()?,
            self.finality.clone(),
            self.bodies.record(),
        ))
    }

    /// Prunes at the pruning point `point` (D-097): the block DAG keeps it and its future, the finality decision
    /// continues from the pruning root `root` and drops the BTC heights below `cut`, and the bodies of the blocks whose
    /// headers are no longer held are dropped. The DAG's main-chain floor must already be the final tip ([`Known::finalize`]).
    /// Returns the number of blocks dropped from the DAG.
    pub fn prune(&mut self, point: &[u8; 32], root: &[u8; 32], cut: u32) -> Result<usize, String> {
        let dropped = self.tree.prune(point)?;
        self.finality.prune(&self.tree, root, cut)?;
        self.bodies.prune_to(&self.tree);
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
    /// (its own or a merged block's) cannot be determined yet.
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
            bodies.body(tree, h, snapshot)
        })
    }

    /// A scanned settlement or republish payload: collects its block headers (`snapshot`, the BTC snapshot being built,
    /// gives the header time of the anchored BTC block, D-083, and the BTC chain the main chain is chosen under,
    /// D-104); the bodies carried by the payload (the entries of a settlement payload belong to
    /// the block the last header points at, the items in `bodies` belong to the headers they point at, D-077) become
    /// candidate bodies of those blocks. A duplicate payload (rescanned after a BTC reorg) is ignored. Returns an error
    /// when a header cannot be checked locally (e.g. the PoW value cannot be computed, D-094): a local fault, the
    /// caller stops this round and raises an alarm, and the header is not treated as invalid.
    pub fn observe(&mut self, payload: &Payload, snapshot: &BtcSnapshot) -> Result<(), String> {
        let headers = match payload {
            Payload::Settlement { headers, .. } | Payload::Headers { headers, .. } => headers,
            Payload::NativeOp { .. } => return Ok(()),
        };
        let max_known = self.params.max_known_version();
        for header in headers {
            if header.version > max_known {
                // An upgrade unknown to this indexer (D-076): the header cannot be judged, the indexer stops before the
                // fork
                if self.unknown_versions.len() < MAX_UNKNOWN_VERSIONS
                    && self.unknown_versions.insert(header.version)
                {
                    eprintln!(
                        "[ksana-indexer] alarm (highest level): the payload contains a block header of protocol version {}, but this indexer only supports up to {max_known}: \
                         the network may have upgraded; the indexer software needs upgrading (D-076)",
                        header.version
                    );
                }
                continue;
            }
            match self.tree.insert(&self.consensus, header.clone(), snapshot) {
                Insert::Invalid(reason) => eprintln!(
                    "[ksana-indexer] discarding invalid block header {} (height {}): {reason}",
                    hex::encode(header.hash()),
                    header.height
                ),
                Insert::Unavailable(reason) => {
                    eprintln!(
                        "[ksana-indexer] alarm (highest level): cannot check block header {} (height {}) in the payload: {reason}",
                        hex::encode(header.hash()),
                        header.height
                    );
                    return Err(format!("cannot check block header: {reason}"));
                }
                // Below the pruning point (D-097): it can be neither on nor merged by a chain through the final blocks
                Insert::Duplicate | Insert::Pending | Insert::Connected(_) | Insert::Pruned => {}
            }
        }
        self.bodies.observe(payload, &|_| true);
        Ok(())
    }

    /// The entries a block must force-include at its start (§7.4, D-063): determinable only when the block's anchor is
    /// on the snapshot's best chain.
    pub fn forced(&self, header: &BlockHeader, snapshot: &BtcSnapshot) -> Option<Vec<Entry>> {
        ksana_btc::bodies::forced(&self.params, &self.tree, header, snapshot)
    }

    /// Whether the block carries a settlement transaction (D-043): `entries_root(forced(B)) ≠ B.entries_root`; a block
    /// whose anchor is invalid is judged by its body (D-056). `None` while it cannot be determined yet.
    pub fn carries(
        &mut self,
        hash: &[u8; 32],
        snapshot: &BtcSnapshot,
    ) -> Result<Option<bool>, String> {
        self.bodies.carries(&self.tree, hash, snapshot)
    }

    /// The block's body (see the module docs); `None` while it cannot be determined yet.
    pub fn body(
        &mut self,
        hash: &[u8; 32],
        snapshot: &BtcSnapshot,
    ) -> Result<Option<Arc<Vec<Entry>>>, String> {
        self.bodies.body(&self.tree, hash, snapshot)
    }

    /// Body information (used by the tracker to decide whether a payload on BTC can land the block's data, D-077):
    /// `Unknown` while the body cannot be determined yet; `NoBody` when no body is needed; otherwise its header and
    /// entries.
    pub fn block_data(
        &mut self,
        hash: &[u8; 32],
        snapshot: &BtcSnapshot,
    ) -> Result<BlockData, String> {
        let Some(body) = self.body(hash, snapshot)? else {
            return Ok(BlockData::Unknown);
        };
        if !needs_body(&body) {
            return Ok(BlockData::NoBody);
        }
        let Some(header) = self.tree.header(hash).cloned() else {
            return Ok(BlockData::Unknown);
        };
        Ok(BlockData::Body(Box::new(SettlementContext {
            entries: ksana_btc::payload::entry_data(&body),
            header,
        })))
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
            bodies,
            verdicts,
            ..
        } = self;
        let outcome = {
            let tree: &HeaderDag = tree;
            let verdicts: &Verdicts = verdicts;
            finality.evaluate(
                tree,
                snapshot,
                &mut |hash| bodies.data_status(tree, hash, snapshot),
                &mut |hash| {
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
                },
            )?
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

    /// Summary of a block (when its body is known): `rewarded` is whether it earns a block reward (a main-chain block
    /// itself, or blue in the merge set of the main-chain block merging it, D-074, D-079).
    fn summary(
        &mut self,
        hash: &[u8; 32],
        snapshot: &BtcSnapshot,
        rewarded: bool,
    ) -> Result<Option<BlockSummary>, String> {
        let Some(body) = self.body(hash, snapshot)? else {
            return Ok(None);
        };
        let (Some(header), Some(block)) = (self.tree.header(hash), self.tree.dag().get(hash))
        else {
            return Ok(None);
        };
        Ok(Some(BlockSummary {
            hash: *hash,
            settles: needs_body(&body),
            anchor_height: header.btc_anchor.height,
            producer: header.producer,
            daa_score: block.data.daa_score,
            weight: block.weight,
            rewarded,
        }))
    }

    /// The processable prefix of the main chain (D-038, D-063): the consecutive main-chain blocks from the genesis
    /// block in which the bodies of each block and of the blocks it merges (the merge set except the selected parent,
    /// in consensus order) are known and the data landing heights are all defined (i.e. the settled closure, the
    /// longest prefix where `prefix_height` is defined), and which have `SETTLEMENT_DEPTH` confirmations on our main
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
    pub fn plan(&mut self, snapshot: &BtcSnapshot, from: u64) -> Result<Plan, String> {
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
            let Some(own) = self.summary(&hash, snapshot, true)? else {
                break;
            };
            let Some(data) = self.tree.ghostdag(&hash).cloned() else {
                break;
            };
            let mut merged = Vec::with_capacity(data.mergeset_ordered.len());
            for m in &data.mergeset_ordered {
                let Some(summary) = self.summary(m, snapshot, data.is_merged_blue(m))? else {
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
        Ok(plan)
    }

    /// A block in the DAG: its header and (determined) body.
    pub fn block(&self, hash: &[u8; 32]) -> Option<(BlockHeader, Arc<Vec<Entry>>)> {
        let header = self.tree.header(hash)?;
        let body = self.bodies.known(hash)?;
        Some((header.clone(), Arc::clone(body)))
    }

    /// The main-chain block at that height: its header and (determined) body.
    pub fn main_block(&self, height: u64) -> Option<(BlockHeader, Arc<Vec<Entry>>)> {
        let header = self.tree.main_at(height)?;
        self.block(&header.hash())
    }
}
