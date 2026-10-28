//! Determining the bodies of fast-lane blocks from BTC payloads (D-043, D-048, D-063, D-077, D-082, D-093): shared by
//! node and indexer.
//!
//! - **Body**: the entries `forced(B)` that block B must include by force are derived from BTC per §7.4 (the window
//!   starts at the highest anchor among all of B's parents, D-063; this needs the headers of B and its parents, and
//!   B's anchor must be on the best chain of the BTC snapshot, so that the BTC blocks used for the derivation are
//!   exactly the blocks of the anchor chain). When `entries_root(forced(B))` equals the header's `entries_root`, B
//!   needs no body and the body is `forced(B)`; otherwise the body is taken from the payloads carrying B's body (the
//!   entries of a settlement payload whose last header is B, or the item of a settlement / republish payload's
//!   `bodies` pointing at B, D-077): the first candidate whose converted `entries_root` matches (`EntryData::Forced`
//!   is replaced position by position with the entry of `forced(B)` with the same transaction ID). The body is
//!   uniquely determined by `entries_root`; once determined it is cached and no longer changes with the BTC snapshot.
//! - **Whether a body is needed**: `entries_root(forced(B)) ≠ B.entries_root` (D-043, D-077: empty blocks and blocks
//!   containing only forced-inclusion entries need only the header); otherwise B's body is not needed.
//! - **Anchor-invalid blocks** (the anchor block has been reorged off BTC and its height does not exceed the snapshot
//!   tip, D-056): `forced(B)` cannot be derived on the anchor chain. When `entries_root` is the root of the empty entry
//!   list the body is empty; otherwise take the candidate whose converted `entries_root` matches (`EntryData::Forced`
//!   is looked up by transaction ID as a native transaction of the current best chain over [`stale_range`]: from B's
//!   window start up to the window end of a block anchored now, D-100); when there is none, derive the
//!   forced-inclusion entries from the current best chain over B's window and adopt them if they match; when still
//!   undetermined, wait. Whether the block carries a settlement transaction is judged by the body (whether
//!   it contains entries other than forced inclusions). A node accepts a block in anchor-invalid mode only when its
//!   body could be determined this way ([`check_stale_entries`], D-100), so a block the nodes accept never leaves the
//!   indexer waiting for a body that cannot come.
//! - **Use**: the indexer rebuilds all bodies from this and executes them; the node rebuilds from this the blocks the
//!   finality decision is waiting for and the peers did not provide (D-093): the headers come from the header DAG on
//!   BTC ([`HeaderDag`]), the body is determined by the rules above, and the rebuilt block is then checked by all the
//!   rules for receiving blocks.

use std::collections::{BTreeMap, BTreeSet};
use std::ops::RangeInclusive;
use std::sync::Arc;

use bitcoin::hashes::Hash;
use borsh::{BorshDeserialize, BorshSerialize};
use ksana_crypto::entries_root;
use ksana_primitives::{BlockHeader, BtcAnchor, Entry, EntryData, Params, Payload};

use crate::finality::DataStatus;
use crate::headers::HeaderDag;
use crate::native::forced_heights;
use crate::payload::{bodies_in, needs_body};
use crate::tracker::{forced_entries, BtcSnapshot};

/// Candidate bodies and determined bodies (held separately from the block DAG; the finality decision borrows both at
/// the same time).
pub struct Bodies {
    params: Params,
    /// Log prefix (`[ksana-node]` / `[ksana-indexer]`).
    tag: &'static str,
    /// Candidate bodies: block hash → the entries in the payloads carrying its body (deduplicated, in order of
    /// appearance). Removed once the body is determined.
    candidates: BTreeMap<[u8; 32], Vec<Vec<EntryData>>>,
    /// Determined bodies.
    bodies: BTreeMap<[u8; 32], Arc<Vec<Entry>>>,
    /// Whether the block needs a body (judged by the `entries_root` of the forced-inclusion entries).
    carries: BTreeMap<[u8; 32], bool>,
    /// Anchor-invalid blocks for which "body cannot be determined yet" has already been logged (logged once per block).
    stale_waiting: BTreeSet<[u8; 32]>,
}

/// [`Bodies`] in the local format of a pruning checkpoint (D-097).
#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct BodiesRecord {
    candidates: BTreeMap<[u8; 32], Vec<Vec<EntryData>>>,
    bodies: BTreeMap<[u8; 32], Vec<Entry>>,
    carries: BTreeMap<[u8; 32], bool>,
    stale_waiting: BTreeSet<[u8; 32]>,
}

fn root_of(entries: &[Entry]) -> Result<[u8; 32], String> {
    entries_root(entries).map_err(|e| format!("computing entries_root failed: {e}"))
}

/// Candidate entries other than forced inclusions map one to one to block entries; `Forced` returns `None`.
fn non_forced(data: &EntryData) -> Option<Entry> {
    match data {
        EntryData::Forced { .. } => None,
        EntryData::Sponsored { op } => Some(Entry::Sponsored { op: op.clone() }),
        EntryData::Unlock { height, index } => Some(Entry::Unlock {
            height: *height,
            index: *index,
        }),
    }
}

/// Converts candidate entries into block entries: `Forced { btc_txid }` must have the same transaction ID as the entry
/// at the same position in `forced` and is replaced by that entry; the other variants map one to one. Returns `None`
/// on a mismatch.
fn convert(candidate: &[EntryData], forced: &[Entry]) -> Option<Vec<Entry>> {
    candidate
        .iter()
        .enumerate()
        .map(|(index, data)| match data {
            EntryData::Forced { btc_txid } => match forced.get(index) {
                Some(entry @ Entry::Forced { btc_txid: txid, .. }) if txid == btc_txid => {
                    Some(entry.clone())
                }
                _ => None,
            },
            other => non_forced(other),
        })
        .collect()
}

/// Anchor-invalid blocks (D-056): `Forced { btc_txid }` looks up the native transaction by transaction ID among the
/// native transactions of the current best chain (the snapshot) at heights in `range` ([`stale_range`], D-100); the
/// other variants map one to one. Returns `None` when not found.
fn convert_by_txid(
    candidate: &[EntryData],
    snapshot: &BtcSnapshot,
    range: Option<&RangeInclusive<u32>>,
) -> Option<Vec<Entry>> {
    candidate
        .iter()
        .map(|data| match data {
            EntryData::Forced { btc_txid } => range
                .and_then(|range| snapshot.native_tx_in(btc_txid, range.clone()))
                .map(|tx| Entry::Forced {
                    btc_txid: tx.txid,
                    op: tx.op.clone(),
                }),
            other => non_forced(other),
        })
        .collect()
}

/// The block's anchor relative to the snapshot: `Some(true)` on the best chain; `Some(false)` no longer on the best
/// chain and its height does not exceed the snapshot tip (anchor invalid, D-056); `None` above the snapshot tip (wait
/// for BTC sync).
fn anchored(header: &BlockHeader, snapshot: &BtcSnapshot) -> Option<bool> {
    if snapshot.contains(&header.btc_anchor) {
        return Some(true);
    }
    snapshot
        .tip()
        .filter(|(tip, _)| header.btc_anchor.height <= *tip)
        .map(|_| false)
}

/// Why [`check_stale_entries`] refuses a block.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StaleEntries {
    /// The block is invalid: a forced-inclusion entry names a native transaction found over [`stale_range`] that
    /// carries another op, or the forced-inclusion entries of a block that needs no body are not the ones derived from
    /// the current best chain.
    Invalid(String),
    /// A forced-inclusion entry names a transaction that is not a native transaction of the current best chain over
    /// [`stale_range`] (not confirmed there yet, or at a height outside it): this copy is refused, which does not make
    /// the block invalid (the transaction may gain the confirmations, or a BTC reorg may confirm it in the range).
    Missing(String),
}

/// The BTC heights over which a forced-inclusion entry of an anchor-invalid block is looked up by transaction ID on
/// the current best chain (D-056, D-100): from the block's window start `h(parent_anchor) + 1` (`parent_anchor` being
/// the highest anchor among its parents) up to `h(tip − ANCHOR_DEPTH)`, the window end of a block a producer would
/// anchor now. `None` when the range is empty or no BTC block has been scanned.
///
/// - The start does not depend on pruning: every block a node or indexer still processes has its parents anchored at
///   or above its pruning point's anchor, so the snapshot keeps every native transaction from there on (D-097).
///   Heights below it are never looked at.
/// - The end asks for the confirmations forced inclusion asks for (`ANCHOR_DEPTH + NATIVE_CONFIRMATIONS`). It
///   follows the BTC tip rather than the block's own anchor, so an honest block whose transaction a BTC reorg
///   confirmed again a little higher is accepted once that transaction has the confirmations.
pub fn stale_range(
    params: &Params,
    snapshot: &BtcSnapshot,
    parent_anchor: &BtcAnchor,
) -> Option<RangeInclusive<u32>> {
    let (tip, _) = snapshot.tip()?;
    forced_heights(
        params,
        parent_anchor.height,
        tip.saturating_sub(params.anchor_depth),
    )
}

/// The forced-inclusion rule for a block accepted in anchor-invalid mode (D-056, D-100): the block's entries must be
/// a body the indexer can determine from BTC (see the module docs), judged on the current best chain:
///
/// - an empty block always can;
/// - a block that needs a body (it lands on BTC with its forced-inclusion entries written as transaction IDs): each
///   forced-inclusion entry names a native transaction of the current best chain over [`stale_range`] carrying the
///   same op;
/// - a block with only forced-inclusion entries (it needs no body, and no payload may carry one): they equal the
///   entries derived from the current best chain over the block's window `(h(parent), h(B)]` (§7.4).
///
/// `parent_anchor` is the highest anchor among the parents.
pub fn check_stale_entries(
    params: &Params,
    snapshot: &BtcSnapshot,
    parent_anchor: &BtcAnchor,
    header: &BlockHeader,
    entries: &[Entry],
) -> Result<(), StaleEntries> {
    if entries.is_empty() {
        return Ok(());
    }
    if needs_body(entries) {
        let range = stale_range(params, snapshot, parent_anchor);
        for (index, entry) in entries.iter().enumerate() {
            let Entry::Forced { btc_txid, op } = entry else {
                continue;
            };
            let txid = bitcoin::Txid::from_byte_array(*btc_txid);
            match range
                .as_ref()
                .and_then(|range| snapshot.native_tx_in(btc_txid, range.clone()))
            {
                Some(native) if native.op == *op => {}
                Some(_) => {
                    return Err(StaleEntries::Invalid(format!(
                        "entry {index}: native transaction {txid} carries another op (D-100)"
                    )))
                }
                None => {
                    let heights = range.as_ref().map_or_else(
                        || "an empty range".to_string(),
                        |r| format!("BTC heights {} to {}", r.start(), r.end()),
                    );
                    return Err(StaleEntries::Missing(format!(
                        "entry {index}: transaction {txid} is not a native transaction of the current BTC best chain \
                         at {heights} (D-056, D-100)"
                    )));
                }
            }
        }
        return Ok(());
    }
    let derived = forced_entries(params, snapshot, parent_anchor, &header.btc_anchor);
    if derived != entries {
        return Err(StaleEntries::Invalid(
            "the forced-inclusion entries are not the ones the current BTC best chain gives over the block's window \
             (an anchor-invalid block without a body, D-056, D-100)"
                .into(),
        ));
    }
    Ok(())
}

/// The highest anchor among the parents (the start of the forced-inclusion window, D-063); `None` when a parent's
/// header is not in the DAG.
pub fn parent_anchor(tree: &HeaderDag, header: &BlockHeader) -> Option<BtcAnchor> {
    let mut best: Option<&BtcAnchor> = None;
    for parent in &header.parents {
        let anchor = &tree.header(parent)?.btc_anchor;
        if best.is_none_or(|b| anchor.height > b.height) {
            best = Some(anchor);
        }
    }
    best.cloned()
}

/// The entries the block must include by force at its start (§7.4, D-063): determinable only when the block's anchor
/// is on the snapshot's best chain and the headers of all parents are in the DAG.
pub fn forced(
    params: &Params,
    tree: &HeaderDag,
    header: &BlockHeader,
    snapshot: &BtcSnapshot,
) -> Option<Vec<Entry>> {
    if !snapshot.contains(&header.btc_anchor) {
        return None;
    }
    let parent_anchor = parent_anchor(tree, header)?;
    Some(forced_entries(
        params,
        snapshot,
        &parent_anchor,
        &header.btc_anchor,
    ))
}

impl Bodies {
    /// Knows only the genesis block (whose body is empty). `tag` is the log prefix.
    pub fn new(params: Params, genesis: [u8; 32], tag: &'static str) -> Self {
        Bodies {
            params,
            tag,
            candidates: BTreeMap::new(),
            bodies: BTreeMap::from([(genesis, Arc::new(Vec::new()))]),
            carries: BTreeMap::from([(genesis, false)]),
            stale_waiting: BTreeSet::new(),
        }
    }

    /// A scanned settlement or republish payload: the bodies it carries (the entries of a settlement payload belong to
    /// the block its last header points at, the items of `bodies` belong to the headers they point at, D-077) become
    /// candidate bodies of those blocks; only those of blocks for which `wanted` is true are collected (the node
    /// collects only blocks it does not have yet, D-093; the indexer collects all). Blocks whose body is already
    /// determined and duplicate payloads (rescanned after a BTC reorg) are ignored.
    pub fn observe(&mut self, payload: &Payload, wanted: &dyn Fn(&[u8; 32]) -> bool) {
        for (hash, entries) in bodies_in(payload) {
            if self.bodies.contains_key(&hash) || !wanted(&hash) {
                continue;
            }
            let list = self.candidates.entry(hash).or_default();
            if !list.iter().any(|c| c.as_slice() == entries) {
                list.push(entries.to_vec());
            }
        }
    }

    /// The determined body.
    pub fn known(&self, hash: &[u8; 32]) -> Option<&Arc<Vec<Entry>>> {
        self.bodies.get(hash)
    }

    /// The candidates and conclusion for this block are no longer needed (the node obtained the block elsewhere,
    /// D-093).
    pub fn forget(&mut self, hash: &[u8; 32]) {
        self.candidates.remove(hash);
        self.bodies.remove(hash);
        self.carries.remove(hash);
        self.stale_waiting.remove(hash);
    }

    /// The number of blocks that have candidate bodies (for tests and statistics).
    pub fn candidate_count(&self) -> usize {
        self.candidates.len()
    }

    /// The number of blocks whose body or whether it needs one is held (for tests and statistics).
    pub fn known_count(&self) -> usize {
        self.bodies.len().max(self.carries.len())
    }

    /// Drops everything held for the blocks for which `keep` is false (D-097: the blocks no longer kept after
    /// pruning). Returns the number of blocks dropped.
    pub fn prune(&mut self, keep: &dyn Fn(&[u8; 32]) -> bool) -> usize {
        let before = self.candidates.len().saturating_add(self.carries.len());
        self.candidates.retain(|hash, _| keep(hash));
        self.bodies.retain(|hash, _| keep(hash));
        self.carries.retain(|hash, _| keep(hash));
        self.stale_waiting.retain(|hash| keep(hash));
        before.saturating_sub(self.candidates.len().saturating_add(self.carries.len()))
    }

    /// Drops everything held for the blocks whose headers `headers` no longer holds ([`HeaderDag::holds`]) after it was
    /// pruned (D-097). A pending header keeps its candidates: they cannot be collected again once its payload has been
    /// scanned, and the header can still join the DAG. Returns the number of blocks dropped.
    pub fn prune_to(&mut self, headers: &HeaderDag) -> usize {
        self.prune(&|hash| headers.holds(hash))
    }

    /// The state in the local format of a pruning checkpoint (D-097).
    pub fn record(&self) -> BodiesRecord {
        BodiesRecord {
            candidates: self.candidates.clone(),
            bodies: self
                .bodies
                .iter()
                .map(|(hash, body)| (*hash, body.as_ref().clone()))
                .collect(),
            carries: self.carries.clone(),
            stale_waiting: self.stale_waiting.clone(),
        }
    }

    /// The state a [`Bodies::record`] was taken from.
    pub fn from_record(params: Params, tag: &'static str, record: BodiesRecord) -> Self {
        Bodies {
            params,
            tag,
            candidates: record.candidates,
            bodies: record
                .bodies
                .into_iter()
                .map(|(hash, body)| (hash, Arc::new(body)))
                .collect(),
            carries: record.carries,
            stale_waiting: record.stale_waiting,
        }
    }

    /// Whether the block carries a settlement transaction (D-043): `entries_root(forced(B)) ≠ B.entries_root`;
    /// anchor-invalid blocks are judged by the body (D-056). `None` when it cannot be determined yet.
    pub fn carries(
        &mut self,
        tree: &HeaderDag,
        hash: &[u8; 32],
        snapshot: &BtcSnapshot,
    ) -> Result<Option<bool>, String> {
        if let Some(carries) = self.carries.get(hash) {
            return Ok(Some(*carries));
        }
        let Some(header) = tree.header(hash) else {
            return Ok(None);
        };
        if anchored(header, snapshot) == Some(false) {
            return Ok(self
                .body(tree, hash, snapshot)?
                .map(|body| needs_body(&body)));
        }
        let Some(forced) = forced(&self.params, tree, header, snapshot) else {
            return Ok(None);
        };
        let carries = root_of(&forced)? != header.entries_root;
        self.carries.insert(*hash, carries);
        Ok(Some(carries))
    }

    /// The block's body (see the module docs); `None` when it cannot be determined yet.
    pub fn body(
        &mut self,
        tree: &HeaderDag,
        hash: &[u8; 32],
        snapshot: &BtcSnapshot,
    ) -> Result<Option<Arc<Vec<Entry>>>, String> {
        if let Some(body) = self.bodies.get(hash) {
            return Ok(Some(Arc::clone(body)));
        }
        let Some(header) = tree.header(hash) else {
            return Ok(None);
        };
        if anchored(header, snapshot) == Some(false) {
            let Some(body) = self.stale_body(tree, hash, header, snapshot)? else {
                return Ok(None);
            };
            self.carries.insert(*hash, needs_body(&body));
            let body = Arc::new(body);
            self.bodies.insert(*hash, Arc::clone(&body));
            self.candidates.remove(hash);
            self.stale_waiting.remove(hash);
            return Ok(Some(body));
        }
        let Some(forced) = forced(&self.params, tree, header, snapshot) else {
            return Ok(None);
        };
        let target = header.entries_root;
        let body = if root_of(&forced)? == target {
            self.carries.insert(*hash, false);
            forced
        } else {
            self.carries.insert(*hash, true);
            let mut found = None;
            for candidate in self.candidates.get(hash).into_iter().flatten() {
                let Some(entries) = convert(candidate, &forced) else {
                    continue;
                };
                if root_of(&entries)? == target {
                    found = Some(entries);
                    break;
                }
            }
            match found {
                Some(entries) => entries,
                None => return Ok(None),
            }
        };
        let body = Arc::new(body);
        self.bodies.insert(*hash, Arc::clone(&body));
        self.candidates.remove(hash);
        Ok(Some(body))
    }

    /// The body of an anchor-invalid block (see the module docs, D-056); `None` when it cannot be determined yet (the
    /// reason is logged once per block).
    fn stale_body(
        &mut self,
        tree: &HeaderDag,
        hash: &[u8; 32],
        header: &BlockHeader,
        snapshot: &BtcSnapshot,
    ) -> Result<Option<Vec<Entry>>, String> {
        let target = header.entries_root;
        if root_of(&[])? == target {
            return Ok(Some(Vec::new()));
        }
        let reason = match parent_anchor(tree, header) {
            Some(parent_anchor) => {
                let range = stale_range(&self.params, snapshot, &parent_anchor);
                for candidate in self.candidates.get(hash).into_iter().flatten() {
                    let Some(entries) = convert_by_txid(candidate, snapshot, range.as_ref()) else {
                        continue;
                    };
                    if root_of(&entries)? == target {
                        return Ok(Some(entries));
                    }
                }
                let derived =
                    forced_entries(&self.params, snapshot, &parent_anchor, &header.btc_anchor);
                if root_of(&derived)? == target {
                    return Ok(Some(derived));
                }
                "no settlement payload matches after conversion (the forced-inclusion native transactions must be \
                 found on the current chain from the block's window start on, with the confirmations forced inclusion \
                 asks for, D-100), and the forced-inclusion entries derived from the current chain do not match either"
            }
            None => {
                "a parent's header is not known, so neither is the block's forced-inclusion window"
            }
        };
        if self.stale_waiting.insert(*hash) {
            eprintln!(
                "{} the block at height {} with hash {} anchors a BTC block (height {}) that is no longer on the best chain; \
                 its body cannot be determined yet: {reason}; waiting (D-056)",
                self.tag,
                header.height,
                hex::encode(hash),
                header.btc_anchor.height
            );
        }
        Ok(None)
    }

    /// The landing status of the block's data (for the finality decision, D-082): the same rules as the settled
    /// closure. Undetermined while it cannot be determined whether the block needs a body (anchor not on the snapshot,
    /// parents unknown); when a body is needed and has not been determined yet, the payload carrying it has not landed
    /// (landed payloads were converted into bodies when the tracker parsed the settlement), i.e. not landed yet.
    pub fn data_status(
        &mut self,
        tree: &HeaderDag,
        hash: &[u8; 32],
        snapshot: &BtcSnapshot,
    ) -> Result<DataStatus, String> {
        let Some(settles) = self.carries(tree, hash, snapshot)? else {
            return Ok(DataStatus::Undetermined);
        };
        Ok(match snapshot.block_data_height(hash, settles) {
            Some(height) => DataStatus::Landed(height),
            None => DataStatus::NotLanded,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ksana_primitives::{Action, Body, FeeMode, SigScheme, VmOp, BLOCK_VERSION, DEVNET};

    fn op(nonce: u64, fee: FeeMode) -> VmOp {
        VmOp {
            version: 1,
            chain_id: 1001,
            account: [1; 32],
            nonce,
            fee,
            max_gas: 1,
            action: Action::AssetTransfer {
                asset: [0; 32],
                to: [2; 32],
                amount: 1,
            },
            sig: SigScheme::Bip340([0; 64]),
        }
    }

    fn forced(txid: u8, nonce: u64) -> Entry {
        Entry::Forced {
            btc_txid: [txid; 32],
            op: op(nonce, FeeMode::BtcNative),
        }
    }

    #[test]
    fn candidate_entries_map_back_to_block_entries() {
        let entries = vec![
            forced(1, 0),
            forced(2, 1),
            Entry::Unlock {
                height: 7,
                index: 1,
            },
            Entry::Sponsored {
                op: op(3, FeeMode::Token { amount: 2 }),
            },
        ];
        let data: Vec<EntryData> = entries.iter().map(EntryData::from).collect();
        let derived = vec![forced(1, 0), forced(2, 1)];
        assert_eq!(convert(&data, &derived), Some(entries.clone()));
        // forced-inclusion entries correspond by position: a different transaction ID, a different order or being
        // outside the derived result all fail
        assert_eq!(convert(&data, &[forced(1, 0), forced(9, 1)]), None);
        assert_eq!(convert(&data, &[forced(2, 1), forced(1, 0)]), None);
        assert_eq!(convert(&data, &[forced(1, 0)]), None);
        assert_eq!(convert(&data[1..], &derived), None);
        // candidates without forced-inclusion entries are converted as usual (validity is judged by entries_root and
        // execution)
        assert_eq!(convert(&data[2..], &derived), Some(entries[2..].to_vec()));
    }

    /// Pruning (D-097) drops everything held for the blocks not kept; the record restores the same state.
    #[test]
    fn pruning_drops_the_blocks_not_kept_and_the_record_round_trips() {
        let header = |tag: u8| BlockHeader {
            version: BLOCK_VERSION,
            height: u64::from(tag),
            parents: vec![[0; 32]],
            btc_anchor: BtcAnchor {
                height: 1,
                hash: [0; 32],
            },
            entries_root: [tag; 32],
            state_root: [tag; 32],
            producer: [tag; 32],
            timestamp: 0,
            bits: 0,
            nonce: 0,
        };
        let (h1, h2) = (header(1), header(2));
        let entries = vec![EntryData::Unlock {
            height: 7,
            index: 1,
        }];
        let payload = Payload::Settlement {
            headers: vec![h1.clone(), h2.clone()],
            entries: entries.clone(),
            bodies: vec![Body { header: 0, entries }],
        };
        let mut bodies = Bodies::new(DEVNET, [9; 32], "[test]");
        bodies.observe(&payload, &|_| true);
        assert_eq!(bodies.candidate_count(), 2);
        let record = bodies.record();
        let bytes = borsh::to_vec(&record).unwrap();
        let restored = Bodies::from_record(DEVNET, "[test]", borsh::from_slice(&bytes).unwrap());
        assert_eq!(restored.record(), record);
        // h1's candidates and the genesis block's conclusion go
        assert_eq!(bodies.prune(&|hash| *hash == h2.hash()), 2);
        assert_eq!(bodies.candidate_count(), 1);
        assert!(bodies.known(&[9; 32]).is_none());
        assert_eq!(bodies.prune(&|hash| *hash == h2.hash()), 0);
    }

    /// D-100: a block accepted in anchor-invalid mode must have a body the indexer can determine on the current best
    /// chain. Empty: always. Needing a body: each forced-inclusion entry names a native transaction carrying the same
    /// op over the stale range, from the block's window start up to the window end of a block anchored now (another
    /// op makes the block invalid; a transaction not found there only refuses this copy). Only forced-inclusion
    /// entries: exactly the ones derived over the block's window. Pruning the snapshot below the window start changes
    /// none of these verdicts (D-097).
    #[test]
    fn anchor_invalid_entries_must_resolve_on_the_current_chain() {
        use ksana_primitives::{OutPoint, DEVNET};
        let mut snapshot = BtcSnapshot::testing(0);
        for height in 0..=20u8 {
            snapshot.push_block([height; 32], 0);
        }
        let native = |txid: u8, nonce: u64| crate::native::NativeTx {
            txid: [txid; 32],
            op: op(nonce, FeeMode::BtcNative),
            first_input: OutPoint {
                txid: [txid ^ 0xff; 32],
                vout: 0,
            },
            outputs: Vec::new(),
        };
        snapshot.push_native(2, native(5, 3));
        snapshot.push_native(4, native(6, 4));
        snapshot.push_native(5, native(1, 0));
        snapshot.push_native(6, native(2, 1));
        snapshot.push_native(15, native(3, 2));
        snapshot.push_native(20, native(7, 5));
        // the anchor (height 10) is no longer on the snapshot's chain; the parents' highest anchor is at height 4, so
        // the window is heights 5 to 10 (NATIVE_CONFIRMATIONS 1 on devnet)
        let header = BlockHeader {
            version: 1,
            height: 3,
            parents: vec![[7; 32]],
            btc_anchor: BtcAnchor {
                height: 10,
                hash: [0xee; 32],
            },
            entries_root: [0; 32],
            state_root: [0; 32],
            producer: [0; 32],
            timestamp: 0,
            bits: 0,
            nonce: 0,
        };
        assert!(!snapshot.contains(&header.btc_anchor));
        let parent_anchor = BtcAnchor {
            height: 4,
            hash: [4; 32],
        };
        let check = |entries: &[Entry]| {
            check_stale_entries(&DEVNET, &snapshot, &parent_anchor, &header, entries)
        };
        assert_eq!(check(&[]), Ok(()));
        let unlock = Entry::Unlock {
            height: 1,
            index: 0,
        };
        // needs a body: forced-inclusion entries are looked up by transaction ID over the stale range, heights 5 to 20
        // (ANCHOR_DEPTH 0 on devnet)
        assert_eq!(
            stale_range(&DEVNET, &snapshot, &parent_anchor),
            Some(5..=20)
        );
        assert_eq!(check(&[forced(3, 2), unlock.clone()]), Ok(()));
        assert_eq!(check(&[forced(7, 5), unlock.clone()]), Ok(()));
        assert_eq!(check(&[forced(1, 0), forced(2, 1), unlock.clone()]), Ok(()));
        assert!(matches!(
            check(&[forced(1, 9), unlock.clone()]),
            Err(StaleEntries::Invalid(_))
        ));
        // a transaction the chain does not have, or a native transaction below the window start (whatever its op):
        // not found
        for missing in [forced(4, 0), forced(5, 3), forced(6, 4), forced(6, 9)] {
            assert!(
                matches!(
                    check(&[missing.clone(), unlock.clone()]),
                    Err(StaleEntries::Missing(_))
                ),
                "{missing:?}"
            );
        }
        // the end asks for the confirmations forced inclusion asks for: with ANCHOR_DEPTH 2 the range ends at height 18
        let mut deep = DEVNET.clone();
        deep.anchor_depth = 2;
        assert_eq!(stale_range(&deep, &snapshot, &parent_anchor), Some(5..=18));
        let check_deep = |entries: &[Entry]| {
            check_stale_entries(&deep, &snapshot, &parent_anchor, &header, entries)
        };
        assert_eq!(check_deep(&[forced(3, 2), unlock.clone()]), Ok(()));
        assert!(matches!(
            check_deep(&[forced(7, 5), unlock.clone()]),
            Err(StaleEntries::Missing(_))
        ));
        // only forced-inclusion entries: the window's native transactions, in order, nothing else
        assert_eq!(check(&[forced(1, 0), forced(2, 1)]), Ok(()));
        for wrong in [
            vec![forced(1, 0)],
            vec![forced(2, 1), forced(1, 0)],
            vec![forced(1, 0), forced(2, 1), forced(3, 2)],
            vec![forced(1, 0), forced(4, 1)],
        ] {
            assert!(
                matches!(check(&wrong), Err(StaleEntries::Invalid(_))),
                "{wrong:?}"
            );
        }

        // A pruning point the block descends from anchors at or below its parents' anchor (height 4), so the cut is
        // at most height 3 (NATIVE_CONFIRMATIONS 1): the verdicts and the converted candidates stay the same
        let mut pruned = snapshot.clone();
        pruned.prune(3, &|_| false);
        assert!(pruned.native_tx_in(&[5; 32], 0..=20).is_none());
        let range = stale_range(&DEVNET, &snapshot, &parent_anchor);
        assert_eq!(range, stale_range(&DEVNET, &pruned, &parent_anchor));
        for entries in [
            vec![forced(3, 2), unlock.clone()],
            vec![forced(1, 9), unlock.clone()],
            vec![forced(5, 3), unlock.clone()],
            vec![forced(6, 4), unlock.clone()],
            vec![forced(1, 0), forced(2, 1)],
            vec![forced(5, 3), forced(6, 4), forced(1, 0), forced(2, 1)],
        ] {
            assert_eq!(
                check(&entries),
                check_stale_entries(&DEVNET, &pruned, &parent_anchor, &header, &entries),
                "{entries:?}"
            );
            let data: Vec<EntryData> = entries.iter().map(EntryData::from).collect();
            assert_eq!(
                convert_by_txid(&data, &snapshot, range.as_ref()),
                convert_by_txid(&data, &pruned, range.as_ref()),
                "{entries:?}"
            );
        }
        assert_eq!(
            convert_by_txid(
                &[EntryData::Forced { btc_txid: [5; 32] }],
                &snapshot,
                range.as_ref()
            ),
            None
        );
    }
}
