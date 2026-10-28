//! The verdict on a block the finality decision asks about (D-101): executing it as a main-chain block on top of its
//! selected parent, the same execution as when the indexer executes a main-chain block; shared by node and indexer
//! (the indexer executes every main-chain block through it).
//!
//! - **Body rules** of each block it merges (in consensus order) and of the block itself: the state-independent entry
//!   rules under the rules at the block's own DAA score (D-076), the body fitting in a payload (D-100), and forced
//!   inclusion when the block's anchor is on the BTC best chain (D-036; a block whose anchor is not is not checked
//!   against rules reproducible only on its anchor chain, D-056).
//! - **The settlement backlog limit** on the block (§6.4, D-046) when its anchor is on the best chain.
//! - **Execution**: the merged blocks leniently, then the block's own entries strictly (D-063), and its `state_root`.
//!
//! A violation is a verdict of invalid, except a failed execution or a mismatching `state_root` of a block whose
//! anchor is no longer on the best chain (anchor-invalid mode, D-056): its re-execution answers from the current chain
//! and may not reproduce the producer's, so that is no verdict at all ([`Verdict::Unjudged`]; D-100, D-101). The
//! finality decision never asks about such a block: a block anchored, or merging a block anchored, off the BTC chain is
//! excluded before any verdict is asked (D-101 item c). The indexer's execution of its main chain still meets them.
//!
//! **Basis**: the finality decision asks about a block only once the data of its closure has `BTC_FINALITY_CONFS`
//! confirmations, so its anchor is on the part of the BTC chain the decision depends on. A verdict depends on the BTC
//! chain only up to the block's anchor height: the anchor-invalid mode of the block and of each block it merges (whose
//! anchors are not above it), their forced-inclusion entries, the backlog limit and the BTC view of the execution. The
//! hash of the best chain's block at that height ([`basis`]) commits to all of it, so a verdict recorded together with
//! its basis ([`Verdicts`]) is used again only while the basis is unchanged; after a BTC reorg reaching below the
//! anchor the block is judged again. (In anchor-invalid mode the execution also reads the current chain above the
//! anchor; only a valid verdict can come out of that, and a valid verdict there contradicts nobody's verdict.)

use std::collections::BTreeMap;
use std::sync::Arc;

use ksana_consensus::Hash;
use ksana_execution::{
    check_entries_static, BlockEnv, BlockError, BlockOutcome, Executor, MergedBlock, StateSource,
    WriteSet,
};
use ksana_primitives::{BlockHeader, Entry, Params};

use crate::bodies::forced;
use crate::client::BtcClient;
use crate::finality::Validity;
use crate::headers::HeaderDag;
use crate::native::check_forced_prefix;
use crate::payload::{check_body_fits, needs_body};
use crate::tracker::{
    settlement_backlog, unlock_cursor, unlock_segment, AnchoredView, BlockSummary, BtcSnapshot,
    MainChain,
};

/// The basis of a verdict on a block anchored at `header.btc_anchor` (see the module docs): the hash of the block at
/// the anchor height on the best chain of `snapshot`; `None` when the snapshot does not hold that height.
pub fn basis(snapshot: &BtcSnapshot, header: &BlockHeader) -> Option<[u8; 32]> {
    snapshot.hash_at(header.btc_anchor.height)
}

/// Whether the block's anchor is no longer on the best chain of `snapshot` (anchor-invalid mode, D-056): the anchor
/// height is not above the tip and the block at that height is another one.
pub fn anchor_invalid(snapshot: &BtcSnapshot, header: &BlockHeader) -> bool {
    !snapshot.contains(&header.btc_anchor)
        && snapshot
            .tip()
            .is_some_and(|(tip, _)| header.btc_anchor.height <= tip)
}

/// A block's body when it can be determined (shared: the bodies are cached).
pub type SharedBody = Option<Arc<Vec<Entry>>>;

/// A block merged by the block being judged.
#[derive(Clone, Debug)]
pub struct Merged {
    /// Its header.
    pub header: BlockHeader,
    /// Its DAA score (the rules of its entries, D-076; its block reward, D-074).
    pub daa_score: u64,
    /// Its weight (D-084).
    pub weight: u64,
    /// Whether it is blue in the merge set (it gets the block reward, D-074).
    pub blue: bool,
    /// Its body.
    pub body: Arc<Vec<Entry>>,
    /// The entries it must force-include at its start; `None` when its anchor is not on the best chain (not checked,
    /// D-056).
    pub forced: Option<Vec<Entry>>,
}

/// A block prepared for judging ([`prepare`]): its header and body and those of the blocks it merges.
#[derive(Clone, Debug)]
pub struct Candidate {
    /// Its header.
    pub header: BlockHeader,
    /// Its DAA score.
    pub daa_score: u64,
    /// Its body.
    pub body: Arc<Vec<Entry>>,
    /// The entries it must force-include at its start; `None` when its anchor is not on the best chain (D-056).
    pub forced: Option<Vec<Entry>>,
    /// The blocks it merges (the merge set except the selected parent), in consensus order (D-063).
    pub merged: Vec<Merged>,
}

impl Candidate {
    /// The block hash.
    pub fn hash(&self) -> Hash {
        self.header.hash()
    }

    /// The summaries of the blocks it merges (for the backlog limit and its main-chain entry).
    pub fn merged_summaries(&self) -> Vec<BlockSummary> {
        self.merged
            .iter()
            .map(|m| BlockSummary {
                hash: m.header.hash(),
                settles: needs_body(&m.body),
                anchor_height: m.header.btc_anchor.height,
                producer: m.header.producer,
                daa_score: m.daa_score,
                weight: m.weight,
                rewarded: m.blue,
            })
            .collect()
    }
}

/// Prepares block `hash` of the header DAG for judging: its header and GHOSTDAG data from `tree`, the bodies from
/// `body` (the block's own and each merged block's), the forced-inclusion entries from `snapshot`. `Ok(None)` while a
/// body cannot be determined yet; `Err` when the block or a block it merges is missing from the DAG, or `body` fails.
pub fn prepare(
    params: &Params,
    tree: &HeaderDag,
    snapshot: &BtcSnapshot,
    hash: &Hash,
    body: &mut dyn FnMut(&Hash) -> Result<SharedBody, String>,
) -> Result<Option<Candidate>, String> {
    let missing = || format!("block {} is missing from the header DAG", hex::encode(hash));
    let header = tree.header(hash).ok_or_else(missing)?.clone();
    let data = tree.ghostdag(hash).ok_or_else(missing)?;
    let Some(own) = body(hash)? else {
        return Ok(None);
    };
    let mut merged = Vec::with_capacity(data.mergeset_ordered.len());
    for m in &data.mergeset_ordered {
        let block = tree.dag().get(m).ok_or_else(|| {
            format!(
                "block {} merged by {} is missing from the header DAG",
                hex::encode(m),
                hex::encode(hash)
            )
        })?;
        let Some(m_body) = body(m)? else {
            return Ok(None);
        };
        merged.push(Merged {
            forced: forced(params, tree, &block.payload, snapshot),
            header: block.payload.clone(),
            daa_score: block.data.daa_score,
            weight: block.weight,
            blue: data.is_merged_blue(m),
            body: m_body,
        });
    }
    Ok(Some(Candidate {
        forced: forced(params, tree, &header, snapshot),
        header,
        daa_score: data.daa_score,
        body: own,
        merged,
    }))
}

/// The verdict on a block ([`judge`]).
#[derive(Debug)]
pub enum Verdict {
    /// Valid: the outcome of executing it (committed as is when it is the next main-chain block).
    Valid(BlockOutcome),
    /// It or a block it merges breaks a body rule: the nodes reject the block itself, and every block after it.
    BadBody(String),
    /// It cannot be on the main chain: the settlement backlog limit, an execution judged invalid, a mismatching
    /// `state_root` (D-063).
    Invalid(String),
    /// No verdict: a local fault, or the failure of a block in anchor-invalid mode (D-056, D-100); judged again later.
    Unjudged(String),
}

impl Verdict {
    /// The verdict for the finality decision (D-101).
    pub fn validity(&self) -> Validity {
        match self {
            Verdict::Valid(_) => Validity::Valid,
            Verdict::BadBody(_) | Verdict::Invalid(_) => Validity::Invalid,
            Verdict::Unjudged(_) => Validity::Undetermined,
        }
    }
}

/// What judging reads besides the block and its parent state.
pub struct Judge<'a> {
    /// The network parameters.
    pub params: &'a Params,
    /// The executor.
    pub exec: &'a Executor,
    /// The current BTC snapshot.
    pub snapshot: &'a Arc<BtcSnapshot>,
    /// bitcoind (the BTC view of the execution answers below the snapshot from it).
    pub client: &'a Arc<dyn BtcClient>,
}

/// The body rules of one block (see the module docs).
fn check_body(
    params: &Params,
    header: &BlockHeader,
    daa_score: u64,
    body: &[Entry],
    forced: Option<&[Entry]>,
) -> Result<(), String> {
    let hash = hex::encode(header.hash());
    check_entries_static(params, &params.rules(daa_score), body)
        .and_then(|()| check_body_fits(params, header, body))
        .map_err(|m| format!("block {hash}: {m}"))?;
    if let Some(forced) = forced {
        check_forced_prefix(body, forced).map_err(|m| format!("block {hash}: {m}"))?;
    }
    Ok(())
}

/// Judges `candidate` as a main-chain block on top of its selected parent (see the module docs): `main` is the main
/// chain up to the selected parent (its tip), `state` the selected parent's state, `root` gives the state root after
/// committing writes on top of that state.
pub fn judge(
    j: &Judge<'_>,
    candidate: &Candidate,
    main: Arc<MainChain>,
    state: Arc<dyn StateSource>,
    root: &mut dyn FnMut(&WriteSet) -> Result<[u8; 32], String>,
) -> Verdict {
    let header = &candidate.header;
    let height = header.height;
    for m in &candidate.merged {
        if let Err(reason) = check_body(
            j.params,
            &m.header,
            m.daa_score,
            &m.body,
            m.forced.as_deref(),
        ) {
            return Verdict::BadBody(reason);
        }
    }
    if let Err(reason) = check_body(
        j.params,
        header,
        candidate.daa_score,
        &candidate.body,
        candidate.forced.as_deref(),
    ) {
        return Verdict::BadBody(reason);
    }
    if main.tip().map(|e| &e.hash) != header.selected_parent() {
        return Verdict::Unjudged(format!(
            "the main chain given for judging the block at height {height} does not end at its selected parent"
        ));
    }
    let btc = AnchoredView::history(
        header.btc_anchor.clone(),
        Arc::clone(j.snapshot),
        Arc::clone(j.client),
        Arc::clone(&main),
        j.params.clone(),
    );
    let stale = btc.is_stale();
    // Settlement backlog limit (§6.4, D-046, D-063): judged on a main-chain block over its whole past, depending only
    // on the anchor chain
    let mut paused = false;
    if !stale {
        match settlement_backlog(
            j.params,
            j.snapshot,
            &main,
            &candidate.merged_summaries(),
            header.btc_anchor.height,
        ) {
            Err(e) => return Verdict::Unjudged(e),
            Ok(None) => {}
            Ok(Some(backlog)) => {
                paused = true;
                if candidate
                    .body
                    .iter()
                    .any(|e| !matches!(e, Entry::Forced { .. }))
                {
                    return Verdict::Invalid(format!(
                        "settlement backlog limit triggered (block {} had no valid settlement land before the anchor \
                         height), the block may only contain forced-inclusion entries (D-046)",
                        hex::encode(backlog.hash)
                    ));
                }
            }
        }
    }
    let merged: Vec<MergedBlock> = candidate
        .merged
        .iter()
        .map(|m| MergedBlock {
            hash: m.header.hash(),
            producer: m.header.producer,
            entries: m.body.to_vec(),
            fast_path_paused: paused,
        })
        .collect();
    // Unlockable segment (D-079): past the parent state's unlock cursor on the main chain up to the parent
    let unlockable = match unlock_cursor(state.as_ref())
        .and_then(|cursor| unlock_segment(&main, cursor, j.params.max_unlock_batch))
    {
        Ok(unlockable) => unlockable,
        Err(e) => return Verdict::Unjudged(e),
    };
    let env = BlockEnv {
        height,
        btc_anchor: header.btc_anchor.clone(),
        producer: header.producer,
        timestamp: header.timestamp,
        rules: j.params.rules(candidate.daa_score),
        unlockable,
    };
    let outcome = match j.exec.execute_chain_block(
        state,
        Arc::new(btc),
        &env,
        &merged,
        &candidate.body,
    ) {
        Ok(outcome) => outcome,
        // Re-executed in anchor-invalid mode the BTC view answers from the current chain, which may not reproduce the
        // producer's execution: a verdict of invalid is then no verdict, like a mismatching state_root below (D-056,
        // D-100)
        Err(BlockError::Invalid(m)) if stale => {
            return Verdict::Unjudged(format!(
                "re-executing the block at height {height}, whose anchor is invalid, judged it invalid ({m}); not \
                 reproducible on the current BTC chain, not judged (D-056)"
            ))
        }
        Err(BlockError::Invalid(m)) => return Verdict::Invalid(m),
        Err(BlockError::Fatal(m)) => {
            return Verdict::Unjudged(format!("executing the block at height {height}: {m}"))
        }
    };
    let computed = match root(&outcome.writes) {
        Ok(computed) => computed,
        Err(e) => return Verdict::Unjudged(e),
    };
    if computed != header.state_root {
        let mismatch = format!(
            "state_root does not match the re-execution result (header {}, re-execution gave {})",
            hex::encode(header.state_root),
            hex::encode(computed)
        );
        if stale {
            // Possibly a BTC reorg deeper than BTC_QUERY_DEPTH, not reproducible on the current chain (D-056)
            return Verdict::Unjudged(format!(
                "block at height {height}, whose anchor is invalid: {mismatch}; not reproducible on the current BTC \
                 chain, not judged (D-056)"
            ));
        }
        return Verdict::Invalid(mismatch);
    }
    Verdict::Valid(outcome)
}

/// A verdict kept on disk (D-101): valid or not, and the basis it was given on.
pub type StoredVerdict = (bool, [u8; 32]);

/// The version of the verdict rules, kept with each verdict on disk ([`encode_stored`]): a verdict kept under another
/// version is not used, and the block is judged again. Raised with any change to what [`judge`] checks. (Version 1 was
/// the unversioned format of the first implementation, never released.)
pub const VERDICT_VERSION: u8 = 2;

/// The bytes a verdict is kept as on disk: [`VERDICT_VERSION`], then Borsh(`(valid, basis)`).
pub fn encode_stored(valid: bool, basis: &[u8; 32]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(34);
    bytes.push(VERDICT_VERSION);
    bytes.push(u8::from(valid));
    bytes.extend_from_slice(basis);
    bytes
}

/// The verdict kept on disk as `bytes` ([`encode_stored`]); `None` when it was kept under another version of the rules
/// or cannot be read: the verdicts on disk are a cache, and the block is judged again.
pub fn decode_stored(bytes: &[u8]) -> Option<StoredVerdict> {
    match bytes.split_first() {
        Some((&VERDICT_VERSION, rest)) => borsh::from_slice(rest).ok(),
        _ => None,
    }
}

/// The lookup of the verdicts kept on disk (D-101); `Err` is a storage failure.
pub type StoredVerdicts<'a> = dyn Fn(&Hash) -> Result<Option<StoredVerdict>, String> + 'a;

/// Recorded verdicts (D-101), each with its basis (see the module docs): one is used again only while the block at
/// the anchor height on the best chain is the one it was given on. Local, in memory; the node and the indexer also
/// keep them on disk, so that a restart does not have to judge the blocks again (asked through
/// [`Verdicts::validity`]).
#[derive(Clone, Debug, Default)]
pub struct Verdicts {
    /// Block → (valid, basis).
    map: BTreeMap<Hash, (bool, [u8; 32])>,
}

impl Verdicts {
    /// Records a verdict given on `basis`; a block without a basis is not recorded.
    pub fn record(&mut self, hash: Hash, valid: bool, basis: Option<[u8; 32]>) {
        match basis {
            Some(basis) => {
                self.map.insert(hash, (valid, basis));
            }
            None => {
                self.map.remove(&hash);
            }
        }
    }

    /// The recorded verdict on the block when it was given on `basis` (the current one): `Some(valid)`.
    pub fn get(&self, hash: &Hash, basis: Option<[u8; 32]>) -> Option<bool> {
        let (valid, recorded) = self.map.get(hash)?;
        (Some(*recorded) == basis).then_some(*valid)
    }

    /// The answer for the finality decision from the recorded verdicts (D-101): the verdict on the block recorded on
    /// the current basis here or, when none is, in `stored` (the verdicts kept on disk), otherwise
    /// [`Validity::Undetermined`] (the node or indexer then judges it). `Err` only when `stored` fails.
    pub fn validity(
        &self,
        tree: &HeaderDag,
        snapshot: &BtcSnapshot,
        hash: &Hash,
        stored: &StoredVerdicts,
    ) -> Result<Validity, String> {
        let Some(on) = tree.header(hash).and_then(|h| basis(snapshot, h)) else {
            return Ok(Validity::Undetermined);
        };
        let valid = match self.get(hash, Some(on)) {
            Some(valid) => Some(valid),
            None => stored(hash)?.and_then(|(valid, given_on)| (given_on == on).then_some(valid)),
        };
        Ok(match valid {
            Some(true) => Validity::Valid,
            Some(false) => Validity::Invalid,
            None => Validity::Undetermined,
        })
    }

    /// Forgets the verdicts on the blocks for which `keep` is false (pruning, D-097).
    pub fn retain(&mut self, keep: impl Fn(&Hash) -> bool) {
        self.map.retain(|hash, _| keep(hash));
    }

    /// Forgets everything.
    pub fn clear(&mut self) {
        self.map.clear();
    }

    /// The number of verdicts recorded.
    pub fn len(&self) -> usize {
        self.map.len()
    }

    /// Whether no verdict is recorded.
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_verdict_holds_only_on_its_basis() {
        let mut verdicts = Verdicts::default();
        verdicts.record([1; 32], true, Some([7; 32]));
        verdicts.record([2; 32], false, Some([7; 32]));
        verdicts.record([3; 32], true, None);
        assert_eq!(verdicts.get(&[1; 32], Some([7; 32])), Some(true));
        assert_eq!(verdicts.get(&[2; 32], Some([7; 32])), Some(false));
        assert_eq!(verdicts.get(&[1; 32], Some([8; 32])), None);
        assert_eq!(verdicts.get(&[1; 32], None), None);
        assert_eq!(verdicts.get(&[3; 32], None), None);
        assert_eq!(verdicts.len(), 2);
        // a later verdict without a basis drops the earlier one
        verdicts.record([1; 32], true, None);
        assert_eq!(verdicts.get(&[1; 32], Some([7; 32])), None);
        verdicts.retain(|h| *h != [2; 32]);
        assert!(verdicts.is_empty());
    }

    /// A verdict kept on disk is used only under the version of the rules it was kept under; anything else on disk is
    /// judged again.
    #[test]
    fn a_verdict_on_disk_is_read_only_under_its_version() {
        let bytes = encode_stored(true, &[7; 32]);
        assert_eq!(bytes.len(), 34);
        assert_eq!(decode_stored(&bytes), Some((true, [7; 32])));
        assert_eq!(
            decode_stored(&encode_stored(false, &[8; 32])),
            Some((false, [8; 32]))
        );
        let mut other = bytes.clone();
        other[0] = VERDICT_VERSION.wrapping_add(1);
        assert_eq!(decode_stored(&other), None);
        // the unversioned format of the first implementation: Borsh(`(bool, [u8; 32])`)
        assert_eq!(decode_stored(&bytes[1..]), None);
        assert_eq!(decode_stored(&[]), None);
        assert_eq!(decode_stored(&bytes[..33]), None);
        let mut longer = bytes;
        longer.push(0);
        assert_eq!(decode_stored(&longer), None);
    }
}
