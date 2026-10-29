//! The bodies of fast-lane blocks from BTC payloads (D-043, D-048, D-063, D-077, D-093, D-135): shared by node and
//! indexer.
//!
//! - **Body** of a block whose header counts on the BTC best chain ([`crate::tracker`]): the entries `forced(B)` that
//!   block B must include by force are derived from BTC per §7.4 over its window (from above the highest anchor among
//!   its parents up to its own anchor, D-063), which the tracker recorded when B's header counted. When
//!   `entries_root(forced(B))` equals the header's `entries_root`, B needs no body (D-043, D-077: empty blocks and blocks
//!   containing only forced-inclusion entries need only the header) and its body is `forced(B)`. Otherwise its body is
//!   the first one that landed: the tracker checked it when its BTC block was scanned (`EntryData::Forced` replaced
//!   position by position with the entry of `forced(B)` with the same transaction ID, the result matching
//!   `entries_root`) and kept the transaction that carried it, which [`Bodies::body`] decodes again. The body is uniquely
//!   determined by `entries_root`, so it does not depend on which payload landed it.
//! - **Anchor-invalid blocks** (the anchor block has been reorged off BTC and its height does not exceed the snapshot
//!   tip, D-056): such a block does not count (D-135), so no body of it is derived from BTC. A node still accepts one
//!   from a peer when syncing history only when its entries could be determined on the current best chain
//!   ([`check_stale_entries`], D-100).
//! - **Use**: the indexer executes the bodies it gets from this; the node rebuilds from this the blocks the finality
//!   decision is waiting for and the peers did not provide (D-093): the headers come from the header DAG on BTC
//!   ([`HeaderDag`]), the body from this, and the rebuilt block is then checked by all the rules for receiving blocks.

use std::ops::RangeInclusive;
use std::sync::Arc;

use bitcoin::hashes::Hash;
use ksana_primitives::{BlockHeader, BtcAnchor, Entry, EntryData, Params, Payload};

use crate::data::tx_payload;
use crate::headers::HeaderDag;
use crate::native::forced_heights;
use crate::payload::{bodies_at, needs_body};
use crate::tracker::{forced_at, forced_entries, BtcSnapshot};

/// Decodes the bodies of the blocks whose headers count from the BTC snapshot (see the module docs). It keeps only the
/// payload it decoded last, so that the bodies one payload carries decode it once.
pub struct Bodies {
    params: Params,
    /// The genesis block's hash (its body is empty).
    genesis: [u8; 32],
    /// The payload decoded last and the transaction it came from.
    last: Option<([u8; 32], Arc<Payload>)>,
}

/// Entries other than forced inclusions map one to one to block entries; `Forced` returns `None`.
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

/// Converts the entries of a body as a payload carries them into block entries: `Forced { btc_txid }` must have the
/// same transaction ID as the entry at the same position in `forced` and is replaced by that entry; the other variants
/// map one to one. Returns `None` on a mismatch.
pub(crate) fn convert(candidate: &[EntryData], forced: &[Entry]) -> Option<Vec<Entry>> {
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
    /// A decoder for the network `params` whose genesis block is `genesis`.
    pub fn new(params: Params, genesis: [u8; 32]) -> Self {
        Bodies {
            params,
            genesis,
            last: None,
        }
    }

    /// The body of block `hash` on the best chain of `snapshot` (see the module docs): the genesis block's is empty, a
    /// block that needs no body has the entries it must force-include, a block that needs one has the body that landed
    /// first. `Ok(None)` while its header does not count or its body has not landed; `Err` when a landed body cannot be
    /// decoded again, or when its forced-inclusion window lies below a pruned snapshot (local faults: the body was
    /// checked when its BTC block was scanned).
    pub fn body(
        &mut self,
        snapshot: &BtcSnapshot,
        hash: &[u8; 32],
    ) -> Result<Option<Arc<Vec<Entry>>>, String> {
        if *hash == self.genesis {
            return Ok(Some(Arc::new(Vec::new())));
        }
        let Some(counted) = snapshot.counted(hash) else {
            return Ok(None);
        };
        let block = hex::encode(hash);
        if forced_heights(&self.params, counted.parent_anchor, counted.anchor)
            .is_some_and(|window| *window.start() < snapshot.base())
        {
            return Err(format!(
                "the forced-inclusion window of block {block} starts below the pruned BTC snapshot (D-097)"
            ));
        }
        let forced = forced_at(
            &self.params,
            snapshot,
            counted.parent_anchor,
            counted.anchor,
        );
        if !counted.settles {
            return Ok(Some(Arc::new(forced)));
        }
        let Some(landed) = snapshot.landed_body(hash) else {
            return Ok(None);
        };
        let txid = landed.landing.txid;
        let payload = match &self.last {
            Some((last, payload)) if *last == txid => Arc::clone(payload),
            _ => {
                let (_, payload) = tx_payload(&self.params, &landed.tx).ok_or_else(|| {
                    format!(
                        "the transaction {} that landed the body of block {block} no longer decodes",
                        bitcoin::Txid::from_byte_array(txid)
                    )
                })?;
                let payload = Arc::new(payload);
                self.last = Some((txid, Arc::clone(&payload)));
                payload
            }
        };
        let headers = match &*payload {
            Payload::Settlement { headers, .. } | Payload::Headers { headers, .. } => headers,
            Payload::NativeOp { .. } => &Vec::new(),
        };
        let slot = usize::try_from(landed.slot).unwrap_or(usize::MAX);
        let entries = bodies_at(&payload)
            .get(slot)
            .filter(|(index, _)| headers.get(*index).is_some_and(|h| h.hash() == *hash))
            .map(|(_, entries)| *entries)
            .ok_or_else(|| {
                format!(
                    "the body of block {block} is not at slot {slot} of transaction {}",
                    bitcoin::Txid::from_byte_array(txid)
                )
            })?;
        let body = convert(entries, &forced).ok_or_else(|| {
            format!(
                "the landed body of block {block} no longer matches its forced-inclusion entries"
            )
        })?;
        Ok(Some(Arc::new(body)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ksana_primitives::{Action, FeeMode, SigScheme, VmOp};

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

    /// D-100: a block accepted in anchor-invalid mode must have entries that can be determined on the current best
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
        // at most height 3 (NATIVE_CONFIRMATIONS 1): the verdicts stay the same
        let mut pruned = snapshot.clone();
        pruned.prune(3, &|_| false);
        assert!(pruned.native_tx_in(&[5; 32], 0..=20).is_none());
        assert_eq!(
            stale_range(&DEVNET, &snapshot, &parent_anchor),
            stale_range(&DEVNET, &pruned, &parent_anchor)
        );
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
        }
    }
}
