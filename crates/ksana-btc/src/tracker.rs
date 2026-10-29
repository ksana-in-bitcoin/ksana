//! BTC tracking (SPEC §6.3, §6.5, §7, §8.5, §10.3, §10.4; D-034-D-038, D-049, D-135): shared by the fast-lane node
//! and the standalone indexer.
//!
//! - [`BtcTracker`]: tracks bitcoind's best chain, scanning block by block from near the genesis anchor: native
//!   transactions (§7.4), the fast-lane block headers that count in KSN1 payloads, and the transactions landing each
//!   fast-lane block's data on BTC (for a block that needs a body, a settlement or republish payload carrying the same
//!   body, D-077). A settlement or republish payload counts only when every header it carries anchors below the BTC
//!   height it is confirmed at ([`payload_counts`], D-100); one that does not is ignored as a whole. On a BTC reorg it
//!   truncates to the fork point and rescans. The scan results are published as an immutable [`BtcSnapshot`]; block
//!   production, checking and RPC each hold their own snapshot without blocking each other.
//! - **Counting** (§6.5 item 1, D-135): in a payload that counts at BTC height h, a header counts at h when its anchor
//!   is on the best chain, the header DAG accepts it ([`KnownBlocks::count`]) and each of its parents has counted at a
//!   height at most h; within one BTC block this is applied until nothing more counts, so neither the order of the
//!   transactions nor that of the headers in a payload matters. An appearance that does not count leaves no trace:
//!   nothing is held for a header waiting for its parents, which counts only when a payload at or above the height its
//!   parents count at carries it again. Whether the block needs a body is settled when it counts, from the entries it
//!   must force-include (§7.4), since its parents' anchors and its own are then known.
//! - **Bodies** (§6.4, §10.3 item 3, D-135): a body lands only with its own header, in a payload at or above the height
//!   at which that header counts, when its entries match the header's `entries_root` once the `Forced` entries are
//!   replaced; it is checked when its BTC block is scanned, and kept as the transaction that carried it, one copy per
//!   transaction ([`LandedBody`], decoded when used by [`crate::bodies::Bodies`]). Nothing waits for the node or the
//!   indexer to know a block.
//! - [`AnchoredView`]: the execution layer's [`BtcView`]; all answers are relative to some block's `btc_anchor`
//!   (D-035).
//! - [`prefix_height`]: the settled prefix (D-038); in the DAG it accumulates block by block along the main chain (the
//!   virtual selected chain), each block together with the blocks it merges (the settled closure of D-063).
//! - [`unlock_segment`]: the unlockable segment (D-079); the node and the indexer compute the execution layer's
//!   `BlockEnv::unlockable` from the main-chain summaries with this same function.
//!
//! A fault of the data source (bitcoind) is always returned as a local fault and never judges a block invalid. The node
//! and the indexer each implement [`KnownBlocks`] with the same header DAG rules: for the same BTC chain both count
//! exactly the same headers, which is the premise of "the indexer and the fast-lane node agree on state" (D-049).

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, RwLock};

use bitcoin::consensus::encode::serialize;
use bitcoin::hashes::Hash;
use borsh::{BorshDeserialize, BorshSerialize};
use ksana_crypto::entries_root;
use ksana_execution::keys::{self, UnlockCursor};
use ksana_execution::{BtcView, StateSource, UnlockItem};
use ksana_primitives::{BlockHeader, BtcAnchor, Entry, OutPoint, Params, Payload, VmOp};

use crate::bodies::convert;
use crate::client::BtcClient;
use crate::data::tx_payload;
use crate::native::{native_txs, NativeTx};
use crate::payload::bodies_at;
use crate::settlement::PrevoutSource;

/// The tracker's errors. All except [`TrackerError::Config`] are local faults: retry later, and never judge a block
/// invalid because of them.
#[derive(Debug, thiserror::Error)]
pub enum TrackerError {
    /// bitcoind fault or not yet synced.
    #[error("BTC data source error: {0}")]
    Btc(String),
    /// Configuration error (for example, the genesis anchor block is not on bitcoind's best chain).
    #[error("configuration error: {0}")]
    Config(String),
    /// The [`KnownBlocks`] implementation failed to read a known block (storage fault).
    #[error("reading known blocks failed: {0}")]
    Known(String),
    /// Internal error (a lock is poisoned).
    #[error("internal error: {0}")]
    Internal(String),
    /// A BTC reorg reaches below the lowest BTC height kept (near the genesis anchor, or the range dropped by pruning,
    /// D-097): the snapshot cannot follow it.
    #[error("BTC reorg too deep: {0}")]
    TooDeep(String),
}

/// The maximum number of BTC blocks processed per sync (the snapshot is published first, the next round continues).
const SYNC_BATCH: u32 = 500;

/// A summary of a fast-lane block accepted by the main chain: for the settled prefix and the backlog limit (D-038,
/// D-046), and for the block rewards paid out on unlock (D-079). The Borsh encoding is a local storage format (D-097).
#[derive(Clone, Copy, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct BlockSummary {
    /// The block hash.
    pub hash: [u8; 32],
    /// Whether the body must land on BTC (it contains entries other than forced inclusions, D-043, D-077); unrelated to
    /// whether the block carries settlement transactions.
    pub settles: bool,
    /// The block's BTC anchor height (for the settlement backlog limit, D-046).
    pub anchor_height: u32,
    /// The producer (`header.producer`).
    pub producer: [u8; 32],
    /// The block's DAA score (the basis of the block reward, D-074).
    pub daa_score: u64,
    /// The block's weight (D-084, D-090): when rewarded, `weight` reward slots are paid out (an empty block doubles
    /// from `EMPTY_BLOCK_DIFFICULTY` by streak up to the limit, every other block is 1).
    pub weight: u64,
    /// Whether it gets the block reward (D-074): the main-chain block itself, and the blocks that are blue in the merge
    /// set of the main-chain block merging them.
    pub rewarded: bool,
}

/// A summary of a block on the main chain (the DAG's virtual selected chain, D-063): the block itself, and the blocks
/// it merges (the merge set except the selected parent, in consensus order). The main-chain blocks together with the
/// blocks they merge cover exactly the whole past of the tip, so the settled prefix and the backlog limit only need to
/// accumulate block by block along the main chain. The Borsh encoding is a local storage format (D-097).
#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct MainEntry {
    /// The block hash.
    pub hash: [u8; 32],
    /// Whether the body must land on BTC (it contains entries other than forced inclusions, D-043, D-077).
    pub settles: bool,
    /// The block's BTC anchor height (for the settlement backlog limit, D-046).
    pub anchor_height: u32,
    /// The producer (`header.producer`).
    pub producer: [u8; 32],
    /// This block's DAA score.
    pub daa_score: u64,
    /// This block's weight (D-084).
    pub weight: u64,
    /// The blocks this block merges (D-063).
    pub merged: Vec<BlockSummary>,
}

impl MainEntry {
    /// This block's summary (a main-chain block itself always gets the block reward).
    pub fn summary(&self) -> BlockSummary {
        BlockSummary {
            hash: self.hash,
            settles: self.settles,
            anchor_height: self.anchor_height,
            producer: self.producer,
            daa_score: self.daa_score,
            weight: self.weight,
            rewarded: true,
        }
    }

    /// The blocks this block merges and the block itself (in execution order).
    pub fn accepted(&self) -> impl Iterator<Item = BlockSummary> + '_ {
        self.merged.iter().copied().chain([self.summary()])
    }
}

/// The main chain below the in-memory window of [`MainChain`] (D-097): the node and the indexer answer from their
/// stores for the blocks the main chain accepted below the window.
pub trait MainHistory: Send + Sync {
    /// The acceptance height and the summary of block `hash` when the main chain accepted it (at any height); `None`
    /// when it did not. `Err` is a storage fault.
    fn accepted(&self, hash: &[u8; 32]) -> Result<Option<(u64, BlockSummary)>, String>;
}

/// The main-chain summaries held in memory (D-097): the entries from height [`MainChain::base`] up to the tip (the
/// window) and the settled prefix of the main chain below the window. A node or indexer that has never pruned holds
/// every height from the genesis block (base 0). After pruning, the blocks accepted below the window are answered by
/// the [`MainHistory`] when one is attached; everything else the consensus rules ask about stays inside the window,
/// because the window is only advanced below the unlock cursor of every state that can still be executed and below
/// the anchors of every main-chain block still to come (see the node's and the indexer's pruning).
#[derive(Clone)]
pub struct MainChain {
    /// The height of the first entry.
    base: u64,
    /// The settled prefix (D-038) of the block at `base − 1`: the highest data landing height among all the blocks the
    /// main chain accepted up to that height; 0 when `base ≤ 1` (the genesis block has no data).
    base_prefix: u32,
    /// The entries from `base` on (index = height − base); never empty.
    entries: Vec<MainEntry>,
    history: Option<Arc<dyn MainHistory>>,
}

impl std::fmt::Debug for MainChain {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MainChain")
            .field("base", &self.base)
            .field("base_prefix", &self.base_prefix)
            .field("entries", &self.entries)
            .field("history", &self.history.is_some())
            .finish()
    }
}

impl MainChain {
    /// The main chain with only the genesis block.
    pub fn new(genesis: MainEntry) -> Self {
        MainChain {
            base: 0,
            base_prefix: 0,
            entries: vec![genesis],
            history: None,
        }
    }

    /// A window starting at height `base` (the first entry is the main-chain block at that height) whose blocks below
    /// have the settled prefix `base_prefix` (0 when `base ≤ 1`). `entries` must not be empty.
    pub fn from_window(
        base: u64,
        base_prefix: u32,
        entries: Vec<MainEntry>,
    ) -> Result<Self, String> {
        if entries.is_empty() {
            return Err("a main-chain window needs at least one entry".into());
        }
        u64::try_from(entries.len())
            .ok()
            .and_then(|len| base.checked_add(len))
            .ok_or_else(|| "main-chain height overflow".to_string())?;
        Ok(MainChain {
            base,
            base_prefix: if base <= 1 { 0 } else { base_prefix },
            entries,
            history: None,
        })
    }

    /// Attaches the store that answers for the blocks accepted below the window.
    pub fn with_history(mut self, history: Arc<dyn MainHistory>) -> Self {
        self.history = Some(history);
        self
    }

    /// The height of the first entry held (0 when never pruned).
    pub fn base(&self) -> u64 {
        self.base
    }

    /// The settled prefix of the main chain below the window (0 when `base ≤ 1`).
    pub fn base_prefix(&self) -> u32 {
        self.base_prefix
    }

    /// The height of the tip.
    pub fn tip_height(&self) -> u64 {
        let len = u64::try_from(self.entries.len()).unwrap_or(u64::MAX);
        self.base.saturating_add(len.saturating_sub(1))
    }

    /// The tip's entry.
    pub fn tip(&self) -> Option<&MainEntry> {
        self.entries.last()
    }

    fn index(&self, height: u64) -> Option<usize> {
        usize::try_from(height.checked_sub(self.base)?).ok()
    }

    /// The entry at `height`; `None` outside the window.
    pub fn get(&self, height: u64) -> Option<&MainEntry> {
        self.entries.get(self.index(height)?)
    }

    /// The entries held, from [`MainChain::base`] on.
    pub fn entries(&self) -> &[MainEntry] {
        &self.entries
    }

    /// The entries held with their heights, from [`MainChain::base`] on.
    pub fn iter(
        &self,
    ) -> impl DoubleEndedIterator<Item = (u64, &MainEntry)> + ExactSizeIterator + '_ {
        let base = self.base;
        self.entries
            .iter()
            .enumerate()
            .map(move |(i, e)| (base.saturating_add(u64::try_from(i).unwrap_or(u64::MAX)), e))
    }

    /// The entries from height `from` (clamped to the window) up to the tip, with their heights.
    pub fn iter_from(
        &self,
        from: u64,
    ) -> impl DoubleEndedIterator<Item = (u64, &MainEntry)> + ExactSizeIterator + '_ {
        let skip = from.saturating_sub(self.base);
        self.iter()
            .skip(usize::try_from(skip).unwrap_or(usize::MAX))
    }

    /// The entries from height `from` up to height `to`, inclusive (both inside the window).
    fn range(&self, from: u64, to: u64) -> Option<&[MainEntry]> {
        let start = self.index(from)?;
        let end = self.index(to)?;
        self.entries.get(start..=end)
    }

    /// The store answering for the blocks accepted below the window, when attached.
    pub fn history(&self) -> Option<&Arc<dyn MainHistory>> {
        self.history.as_ref()
    }

    /// Appends the next main-chain block.
    pub fn push(&mut self, entry: MainEntry) {
        self.entries.push(entry);
    }

    /// Keeps the heights up to `height` (a main-chain rollback). The window cannot be emptied: `height` must not be
    /// below [`MainChain::base`].
    pub fn truncate(&mut self, height: u64) -> Result<(), String> {
        let keep = self
            .index(height)
            .and_then(|i| i.checked_add(1))
            .ok_or_else(|| {
                format!(
                    "cannot roll the main chain back to height {height}: the window starts at {} (D-097)",
                    self.base
                )
            })?;
        self.entries.truncate(keep);
        Ok(())
    }

    /// Advances the window to start at `base` (D-097): the settled prefix of the entries dropped is folded into
    /// [`MainChain::base_prefix`] from `snapshot`, so every dropped block must have its data landed there. `base` must
    /// not be above the tip; a `base` not above the current one changes nothing.
    pub fn prune(&mut self, base: u64, snapshot: &BtcSnapshot) -> Result<(), String> {
        if base <= self.base {
            return Ok(());
        }
        if base > self.tip_height() {
            return Err(format!(
                "cannot prune the main chain up to height {base} above its tip {}",
                self.tip_height()
            ));
        }
        let mut prefix = self.base_prefix;
        let first = self.base.max(1);
        if base > first {
            let dropped = self
                .range(first, base.saturating_sub(1))
                .ok_or_else(|| "main-chain window out of range".to_string())?;
            for entry in dropped {
                for block in entry.accepted() {
                    let landed = snapshot.data_height(&block).ok_or_else(|| {
                        format!(
                            "block {} below the new main-chain window has no data on BTC",
                            hex::encode(block.hash)
                        )
                    })?;
                    prefix = prefix.max(landed);
                }
            }
        }
        let drop = self
            .index(base)
            .ok_or_else(|| "main-chain window out of range".to_string())?;
        self.entries.drain(..drop);
        self.base = base;
        self.base_prefix = if base <= 1 { 0 } else { prefix };
        Ok(())
    }
}

/// The sender of the payload landing a block's data on BTC (D-095): who the carrier reward goes to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub enum Sender {
    /// A settlement payload: the hash of the block it belongs to (the payload's last header); the carrier is its
    /// producer, and it must already be accepted by the main chain.
    Block([u8; 32]),
    /// A republish payload: its payee account `payee` (all zeros means no reward is taken).
    Payee([u8; 32]),
}

/// The payload's sender (D-095): for a settlement payload the block it belongs to (the last header), for a republish
/// payload its payee account; a settlement payload without headers and a native transaction payload have none.
pub fn payload_sender(payload: &Payload) -> Option<Sender> {
    match payload {
        Payload::Settlement { headers, .. } => headers.last().map(|h| Sender::Block(h.hash())),
        Payload::Headers { payee, .. } => Some(Sender::Payee(*payee)),
        Payload::NativeOp { .. } => None,
    }
}

/// One appearance of a block's data (header or body) on BTC (D-077, D-095).
#[derive(Clone, Copy, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct Landing {
    /// The transaction ID (internal byte order).
    pub txid: [u8; 32],
    /// The confirmation height.
    pub height: u32,
    /// The transaction's index in the BTC block: the order within the same height (D-095).
    pub index: u32,
    /// The payload's sender.
    pub sender: Sender,
}

/// A fast-lane header that counts on the BTC best chain (SPEC §6.5 item 1, D-135), with what was settled about its block
/// when it counted. The Borsh encoding is a local storage format (D-097).
#[derive(Clone, Copy, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct Counted {
    /// Its earliest appearance in the BTC block at which it counts (`seen(B)` is its height, D-095).
    pub landing: Landing,
    /// The height of its anchor (on the best chain).
    pub anchor: u32,
    /// The highest anchor height among its parents: its forced-inclusion window starts above it (§7.4, D-063).
    pub parent_anchor: u32,
    /// Whether the block needs a body: `entries_root(forced(B)) ≠ B.entries_root` (D-043, D-077).
    pub settles: bool,
}

/// The body of a fast-lane block landed on BTC (D-077, D-135): its first landing, the transaction that carried it, one
/// copy shared by every body that transaction lands, and the body's position among those its payload carries
/// ([`crate::payload::bodies_at`]). It is decoded when used ([`crate::bodies::Bodies`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LandedBody {
    /// Where it landed.
    pub landing: Landing,
    /// The transaction that carried it.
    pub tx: Arc<bitcoin::Transaction>,
    /// The body's position in the payload.
    pub slot: u32,
}

/// Counts fast-lane headers on BTC (D-135): the node and the indexer implement it with the same header DAG, so that for
/// the same BTC chain both count exactly the same headers.
pub trait KnownBlocks {
    /// Whether `header`, carried by a payload that counts, passes the header checks (SPEC §6.5 item 1: the header rules,
    /// its anchor not below its parents', the timestamp's anchor bound against the BTC block at its anchor height in
    /// `snapshot`, the snapshot under construction). The tracker asks only once the header's anchor is on the best
    /// chain and each of its parents has counted, and a header that passes joins the caller's header DAG; a header
    /// already in it passes again (its checks do not depend on the BTC chain beyond its anchor block). `Err` is a
    /// local fault (a PoW that cannot be computed, D-094, or a storage fault): the round of the sync stops and is
    /// retried, and the header is not judged.
    fn count(&self, snapshot: &BtcSnapshot, header: &BlockHeader) -> Result<bool, TrackerError>;
}

/// The native transactions block B must force-include at its start (§7.4, D-036): all native transactions confirmed at
/// BTC heights in the range `(h(parent), h(B)]`, in ascending order of height and index within the block. `snapshot`
/// must cover the range.
pub fn forced_entries(
    params: &Params,
    snapshot: &BtcSnapshot,
    parent_anchor: &BtcAnchor,
    anchor: &BtcAnchor,
) -> Vec<Entry> {
    forced_at(params, snapshot, parent_anchor.height, anchor.height)
}

/// [`forced_entries`] by anchor heights: `parent_anchor` the highest among B's parents, `anchor` B's own.
pub fn forced_at(
    params: &Params,
    snapshot: &BtcSnapshot,
    parent_anchor: u32,
    anchor: u32,
) -> Vec<Entry> {
    match crate::native::forced_heights(params, parent_anchor, anchor) {
        Some(range) => crate::native::forced_entries(snapshot.native_iter(range)),
        None => Vec::new(),
    }
}

/// Whether a settlement or republish payload confirmed at BTC height `height` counts (D-100): every header it carries
/// anchors below `height`. An honest producer anchors to a BTC block that exists before its block, so every payload
/// carrying an honest header lands above that anchor; a payload that does not count is ignored as a whole (its headers
/// do not count, the bodies it carries do not land), so the BTC block a header anchors to has always been scanned when
/// the header counts, and the timestamp bound of the header DAG is always checked (D-083).
pub fn payload_counts(headers: &[BlockHeader], height: u32) -> bool {
    headers
        .iter()
        .all(|header| header.btc_anchor.height < height)
}

/// The BTC best chain as scanned at some moment.
#[derive(Clone, Debug, Default)]
pub struct BtcSnapshot {
    /// The lowest height the snapshot covers.
    base: u32,
    /// The block hash and 80-byte header at each height from `base`.
    chain: Vec<([u8; 32], [u8; 80])>,
    /// Heights containing native transactions → the native transactions (in block order).
    native: BTreeMap<u32, Vec<NativeTx>>,
    /// Fast-lane block hash → where its header counts (D-135).
    counted: BTreeMap<[u8; 32], Counted>,
    /// The reverse index of `counted`: BTC height → the fast-lane headers that count at it, each after its parents, for
    /// the finality decision processing height by height (D-082).
    headers_by_height: BTreeMap<u32, Vec<[u8; 32]>>,
    /// Fast-lane blocks that need a body → the first landing of their body (D-077, D-095, D-135).
    bodies: BTreeMap<[u8; 32], LandedBody>,
    /// KSN1 settlement transactions seen (valid or not): txid → confirmation height.
    ksn1_txs: BTreeMap<[u8; 32], u32>,
}

impl BtcSnapshot {
    fn new(base: u32) -> Self {
        BtcSnapshot {
            base,
            ..BtcSnapshot::default()
        }
    }

    /// The scanned tip (height, hash); `None` when no block has been scanned yet.
    pub fn tip(&self) -> Option<(u32, [u8; 32])> {
        let (hash, _) = self.chain.last()?;
        let offset = u32::try_from(self.chain.len().checked_sub(1)?).ok()?;
        Some((self.base.checked_add(offset)?, *hash))
    }

    fn index(&self, height: u32) -> Option<usize> {
        usize::try_from(height.checked_sub(self.base)?).ok()
    }

    /// The block hash at this height on the best chain (only within the snapshot range).
    pub fn hash_at(&self, height: u32) -> Option<[u8; 32]> {
        self.chain.get(self.index(height)?).map(|(h, _)| *h)
    }

    /// The block header at this height on the best chain (only within the snapshot range).
    pub fn header_at(&self, height: u32) -> Option<[u8; 80]> {
        self.chain.get(self.index(height)?).map(|(_, h)| *h)
    }

    /// The time field of the block header at this height on the best chain (only within the snapshot range).
    pub fn header_time_at(&self, height: u32) -> Option<u32> {
        header_time(&self.header_at(height)?)
    }

    /// The fast-lane headers that count at BTC height `height`, each after its parents (D-082, D-135).
    pub fn headers_at(&self, height: u32) -> &[[u8; 32]] {
        self.headers_by_height
            .get(&height)
            .map_or(&[], Vec::as_slice)
    }

    /// The lowest height the snapshot covers.
    pub fn base(&self) -> u32 {
        self.base
    }

    /// Whether the anchor block is on the snapshot's best chain.
    pub fn contains(&self, anchor: &BtcAnchor) -> bool {
        self.hash_at(anchor.height) == Some(anchor.hash)
    }

    /// The native transaction with ID `txid` in a block at a height in `heights` on the best chain (within the snapshot
    /// range). The forced-inclusion entries of an anchor-invalid block are looked up by transaction ID over
    /// [`crate::bodies::stale_range`], which starts at the block's forced-inclusion window start, above the cut of a
    /// pruned snapshot: the answer does not depend on pruning (D-097, D-100).
    pub fn native_tx_in(
        &self,
        txid: &[u8; 32],
        heights: std::ops::RangeInclusive<u32>,
    ) -> Option<&NativeTx> {
        if heights.start() > heights.end() {
            return None;
        }
        self.native
            .range(heights)
            .flat_map(|(_, txs)| txs)
            .find(|tx| tx.txid == *txid)
    }

    /// The native transaction with ID `txid` in a block at height ≤ `max_height` on the current best chain (within
    /// the snapshot range).
    pub fn native_tx_at_most(&self, txid: &[u8; 32], max_height: u32) -> Option<&NativeTx> {
        self.native
            .range(..=max_height)
            .flat_map(|(_, txs)| txs)
            .find(|tx| tx.txid == *txid)
    }

    /// The native transactions in the height range, in ascending order of height and index within the block (§7.4).
    pub fn native_in(&self, heights: std::ops::RangeInclusive<u32>) -> Vec<NativeTx> {
        self.native_iter(heights).cloned().collect()
    }

    /// Same as [`BtcSnapshot::native_in`] by reference; none when the range is empty.
    pub fn native_iter(
        &self,
        heights: std::ops::RangeInclusive<u32>,
    ) -> impl Iterator<Item = &NativeTx> + '_ {
        (heights.start() <= heights.end())
            .then(|| self.native.range(heights))
            .into_iter()
            .flatten()
            .flat_map(|(_, txs)| txs)
    }

    /// Where the block's header counts on the best chain (D-135); `None` when it does not count.
    pub fn counted(&self, block_hash: &[u8; 32]) -> Option<&Counted> {
        self.counted.get(block_hash)
    }

    /// The BTC height at which the block's header counts (`seen(B)`, D-082, D-135).
    pub fn counted_at(&self, block_hash: &[u8; 32]) -> Option<u32> {
        self.counted.get(block_hash).map(|c| c.landing.height)
    }

    /// The first landing of the block's body (D-077, D-135); `None` for a block that needs none or whose body has not
    /// landed.
    pub fn landed_body(&self, block_hash: &[u8; 32]) -> Option<&LandedBody> {
        self.bodies.get(block_hash)
    }

    /// The height at which the block's data landed on BTC (D-038, D-077): [`BtcSnapshot::block_data_height`].
    pub fn data_height(&self, block: &BlockSummary) -> Option<u32> {
        self.block_data_height(&block.hash)
    }

    /// The height at which the block's data landed on BTC (D-038, D-077, D-135): for a block that needs a body, the
    /// first payload landing it; for other blocks, the height at which the header counts.
    pub fn block_data_height(&self, block_hash: &[u8; 32]) -> Option<u32> {
        self.data_landing(block_hash).map(|l| l.height)
    }

    /// The first landing of the block's data on BTC (D-095): for a block that needs a body, the first transaction
    /// landing it; for other blocks, the earliest appearance of the header at the height at which it counts. Whether a
    /// block needs a body was settled when its header counted ([`Counted::settles`]).
    pub fn data_landing(&self, block_hash: &[u8; 32]) -> Option<Landing> {
        let counted = self.counted.get(block_hash)?;
        if counted.settles {
            self.bodies.get(block_hash).map(|body| body.landing)
        } else {
            Some(counted.landing)
        }
    }

    /// The earliest confirmed transaction (txid, height) landing the block's body on BTC (D-077); within the same height,
    /// the earliest in the BTC block (D-095).
    pub fn settlement_of(&self, block_hash: &[u8; 32]) -> Option<([u8; 32], u32)> {
        self.bodies
            .get(block_hash)
            .map(|body| (body.landing.txid, body.landing.height))
    }

    /// The confirmation height of a KSN1 settlement transaction (valid or not).
    pub fn ksn1_height(&self, txid: &[u8; 32]) -> Option<u32> {
        self.ksn1_txs.get(txid).copied()
    }

    /// Keeps only the data at heights ≤ `height`; returns the number of native transactions removed (for the warning,
    /// §7.4).
    fn truncate(&mut self, height: u32) -> usize {
        let keep = match height.checked_sub(self.base) {
            Some(offset) => usize::try_from(offset)
                .ok()
                .and_then(|o| o.checked_add(1))
                .unwrap_or(usize::MAX),
            None => 0,
        };
        self.chain.truncate(keep);
        let removed_native = self
            .native
            .split_off(&height.saturating_add(1))
            .values()
            .map(Vec::len)
            .sum();
        self.counted.retain(|_, c| c.landing.height <= height);
        self.headers_by_height.split_off(&height.saturating_add(1));
        self.bodies.retain(|_, body| body.landing.height <= height);
        self.ksn1_txs.retain(|_, h| *h <= height);
        removed_native
    }

    /// Drops what a DAG pruned at a pruning point anchored at height `cut` or above can no longer ask about (D-097):
    /// the BTC blocks below `cut` (their headers, native transactions, the lists of the fast-lane headers that count
    /// there and the KSN1 settlement transactions), and the counted headers and landed bodies below `cut` of the
    /// fast-lane blocks for which `keep` is false. The tip is always kept. Returns the number of records of fast-lane
    /// blocks dropped.
    ///
    /// Every block in the future of the pruning point has an anchor at or above the pruning point's (anchors never
    /// decrease along parent edges), its data lands above its anchor and its forced-inclusion entries name native
    /// transactions from its window start on (D-100), so nothing a kept block needs is below `cut` when `cut` is at most
    /// the pruning point's anchor height less NATIVE_CONFIRMATIONS; `keep` additionally protects the blocks the caller
    /// still asks about (the header DAG and the main chain's window). A header whose parent is no longer counted here
    /// does not count, as its parent is no longer in the caller's header DAG either.
    pub fn prune(&mut self, cut: u32, keep: &dyn Fn(&[u8; 32]) -> bool) -> usize {
        let cut = match self.tip() {
            Some((tip, _)) => cut.min(tip),
            None => return 0,
        };
        if cut <= self.base {
            return 0;
        }
        let drop = usize::try_from(cut.saturating_sub(self.base)).unwrap_or(usize::MAX);
        self.chain.drain(..drop.min(self.chain.len()));
        self.base = cut;
        self.native = self.native.split_off(&cut);
        self.headers_by_height = self.headers_by_height.split_off(&cut);
        self.ksn1_txs.retain(|_, h| *h >= cut);
        let before = self.counted.len().saturating_add(self.bodies.len());
        self.counted
            .retain(|hash, c| c.landing.height >= cut || keep(hash));
        self.bodies
            .retain(|hash, body| body.landing.height >= cut || keep(hash));
        let after = self.counted.len().saturating_add(self.bodies.len());
        before.saturating_sub(after)
    }

    /// The snapshot in the local format of a pruning checkpoint (D-097). The landed bodies that share one copy of their
    /// transaction in memory share it in the record too: each transaction is written once (D-135).
    pub fn record(&self) -> SnapshotRecord {
        let mut txs = Vec::new();
        let mut written: BTreeMap<*const bitcoin::Transaction, u32> = BTreeMap::new();
        let mut bodies = BTreeMap::new();
        for (hash, body) in &self.bodies {
            let at = *written.entry(Arc::as_ptr(&body.tx)).or_insert_with(|| {
                let at = u32::try_from(txs.len()).unwrap_or(u32::MAX);
                txs.push(serialize(&*body.tx));
                at
            });
            bodies.insert(*hash, (body.landing, at, body.slot));
        }
        SnapshotRecord {
            base: self.base,
            chain: self.chain.clone(),
            native: self.native.clone(),
            counted: self.counted.clone(),
            headers_by_height: self.headers_by_height.clone(),
            txs,
            bodies,
            ksn1_txs: self.ksn1_txs.clone(),
        }
    }

    /// The snapshot a [`BtcSnapshot::record`] was taken from (the bodies one transaction landed sharing one copy of it
    /// again).
    pub fn from_record(record: SnapshotRecord) -> Result<Self, String> {
        let mut txs = Vec::with_capacity(record.txs.len());
        for bytes in &record.txs {
            let tx: bitcoin::Transaction =
                bitcoin::consensus::encode::deserialize(bytes).map_err(|e| {
                    format!("a transaction landing bodies in the checkpoint is corrupt: {e}")
                })?;
            txs.push(Arc::new(tx));
        }
        let mut bodies = BTreeMap::new();
        for (hash, (landing, at, slot)) in record.bodies {
            let tx = usize::try_from(at)
                .ok()
                .and_then(|at| txs.get(at))
                .ok_or("a landed body in the checkpoint names a transaction it lacks")?;
            bodies.insert(
                hash,
                LandedBody {
                    landing,
                    tx: Arc::clone(tx),
                    slot,
                },
            );
        }
        Ok(BtcSnapshot {
            base: record.base,
            chain: record.chain,
            native: record.native,
            counted: record.counted,
            headers_by_height: record.headers_by_height,
            bodies,
            ksn1_txs: record.ksn1_txs,
        })
    }
}

/// A landed body in a [`SnapshotRecord`]: its landing, the position of its transaction in `txs` and its slot.
type BodyRecord = (Landing, u32, u32);

/// A [`BtcSnapshot`] in the local format of a pruning checkpoint (D-097): the same fields, with the transactions
/// carrying the landed bodies in Bitcoin's consensus encoding, each once.
#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct SnapshotRecord {
    base: u32,
    chain: Vec<([u8; 32], [u8; 80])>,
    native: BTreeMap<u32, Vec<NativeTx>>,
    counted: BTreeMap<[u8; 32], Counted>,
    headers_by_height: BTreeMap<u32, Vec<[u8; 32]>>,
    txs: Vec<Vec<u8>>,
    bodies: BTreeMap<[u8; 32], BodyRecord>,
    ksn1_txs: BTreeMap<[u8; 32], u32>,
}

#[cfg(test)]
impl BtcSnapshot {
    /// For tests: an empty snapshot starting at `base`.
    pub(crate) fn testing(base: u32) -> Self {
        BtcSnapshot::new(base)
    }

    /// For tests: appends a BTC block above the tip (only the time field of the header is filled in).
    pub(crate) fn push_block(&mut self, hash: [u8; 32], time: u32) {
        let mut header = [0u8; 80];
        header[68..72].copy_from_slice(&time.to_le_bytes());
        self.chain.push((hash, header));
    }

    /// For tests: a native transaction confirmed at height `height` (after those already there).
    pub(crate) fn push_native(&mut self, height: u32, tx: NativeTx) {
        self.native.entry(height).or_default().push(tx);
    }

    /// For tests: a fast-lane header counts at BTC height `height` (unless it already counts); the sender is a
    /// republish taking no reward. The block needs a body when one has been landed for it with [`BtcSnapshot::settle`].
    pub(crate) fn see_header(&mut self, block: [u8; 32], height: u32) {
        self.see_header_from(block, height, 0, Sender::Payee([0; 32]));
    }

    /// For tests: a fast-lane header counts in transaction `index` at BTC height `height`; the payload's sender is
    /// `sender`.
    pub(crate) fn see_header_from(
        &mut self,
        block: [u8; 32],
        height: u32,
        index: u32,
        sender: Sender,
    ) {
        let settles = self.bodies.contains_key(&block);
        if let std::collections::btree_map::Entry::Vacant(seen) = self.counted.entry(block) {
            seen.insert(Counted {
                landing: Landing {
                    txid: [0; 32],
                    height,
                    index,
                    sender,
                },
                anchor: 0,
                parent_anchor: 0,
                settles,
            });
            self.headers_by_height
                .entry(height)
                .or_default()
                .push(block);
        }
    }

    /// For tests: a transaction landing the block's body on BTC confirms at height `height`; the sender is a republish
    /// taking no reward.
    pub(crate) fn settle(&mut self, block: [u8; 32], txid: [u8; 32], height: u32) {
        self.settle_from(block, txid, height, 0, Sender::Payee([0; 32]));
    }

    /// For tests: transaction `txid` landing the block's body on BTC confirms as transaction `index` at BTC height
    /// `height`; the payload's sender is `sender`. The block needs a body from then on, and its header counts there
    /// unless it already counts; only the first landing is kept.
    pub(crate) fn settle_from(
        &mut self,
        block: [u8; 32],
        txid: [u8; 32],
        height: u32,
        index: u32,
        sender: Sender,
    ) {
        let landing = Landing {
            txid,
            height,
            index,
            sender,
        };
        let earlier = self
            .bodies
            .get(&block)
            .is_some_and(|body| (body.landing.height, body.landing.index) <= (height, index));
        if !earlier {
            let tx = Arc::new(bitcoin::Transaction {
                version: bitcoin::transaction::Version::TWO,
                lock_time: bitcoin::absolute::LockTime::ZERO,
                input: Vec::new(),
                output: Vec::new(),
            });
            self.bodies.insert(
                block,
                LandedBody {
                    landing,
                    tx,
                    slot: 0,
                },
            );
        }
        self.see_header_from(block, height, index, sender);
        if let Some(counted) = self.counted.get_mut(&block) {
            counted.settles = true;
        }
    }

    /// For tests: the counted block needs a body (none has landed unless [`BtcSnapshot::settle`] lands one).
    pub(crate) fn needs_body_for_test(&mut self, block: [u8; 32]) {
        if let Some(counted) = self.counted.get_mut(&block) {
            counted.settles = true;
        }
    }

    /// For tests: a BTC reorg back to height `height` (removes all data above it).
    pub(crate) fn truncate_for_test(&mut self, height: u32) {
        self.truncate(height);
    }
}

/// The settled prefix (D-038, D-063): the maximum of the data landing heights of the blocks in the closure (the block
/// and its whole past) of the main-chain block at height `height`; `None` when some block has no data, or when
/// `height` is outside the window of `main` (D-097). Index 0 is the genesis block, not counted; the main-chain blocks
/// together with the blocks they merge cover the whole past.
pub fn prefix_height(snapshot: &BtcSnapshot, main: &MainChain, height: u64) -> Option<u32> {
    let mut prefix = main.base_prefix;
    let first = main.base.max(1);
    if height < first {
        return (height >= main.base).then_some(prefix);
    }
    for entry in main.range(first, height)? {
        for block in entry.accepted() {
            prefix = prefix.max(snapshot.data_height(&block)?);
        }
    }
    Some(prefix)
}

/// The settlement backlog limit (§6.4, D-046, D-063): for a new block with anchor height `anchor_height`, the blocks
/// in its past (the blocks `main` accepts and the blocks they merge, plus the `extra` blocks the new block itself
/// merges) that carry settlement transactions, have no valid settlement confirmed at or before the anchor height, and
/// have an anchor height ≤ `anchor_height − SETTLEMENT_TIMEOUT`. When one exists (the new block can only be an
/// empty block or one with only forced-inclusion entries), the earliest of them is returned. `snapshot` must cover up
/// to `anchor_height`; the answer depends only on the anchor chain. The blocks below the window of `main` have all
/// landed by its settled prefix (D-097); an anchor height below that prefix cannot be answered here (`Err`).
pub fn settlement_backlog(
    params: &Params,
    snapshot: &BtcSnapshot,
    main: &MainChain,
    extra: &[BlockSummary],
    anchor_height: u32,
) -> Result<Option<BlockSummary>, String> {
    if main.base_prefix > anchor_height {
        return Err(format!(
            "the settlement backlog at anchor height {anchor_height} reaches below the pruned main chain (its settled \
             prefix is {}, D-097)",
            main.base_prefix
        ));
    }
    Ok(main
        .iter_from(1)
        .flat_map(|(_, entry)| entry.accepted())
        .chain(extra.iter().copied())
        .filter(|b| b.settles)
        .filter(|b| {
            !snapshot
                .settlement_of(&b.hash)
                .is_some_and(|(_, height)| height <= anchor_height)
        })
        .find(|b| {
            u64::from(b.anchor_height)
                .checked_add(u64::from(params.settlement_timeout))
                .is_some_and(|limit| u64::from(anchor_height) >= limit)
        }))
}

/// The height at which block `block_hash` is accepted on the main chain (for a main-chain block its own height, for a
/// merged block the height of the main-chain block merging it, D-063), within the window of `main`. Every block is
/// accepted exactly once along the main chain; the search goes back from the tip (the block asked about is usually a
/// recent one).
pub fn accepted_height(main: &MainChain, block_hash: &[u8; 32]) -> Option<u64> {
    main.iter()
        .rev()
        .find(|(_, e)| e.hash == *block_hash || e.merged.iter().any(|m| m.hash == *block_hash))
        .map(|(height, _)| height)
}

/// The summary of block `block_hash` accepted by the main chain (a main-chain block itself or a merged block, D-063);
/// the search goes back from the tip, and below the window it asks the attached [`MainHistory`] (D-097). `Err` is a
/// storage fault of the history.
pub fn accepted_summary(
    main: &MainChain,
    block_hash: &[u8; 32],
) -> Result<Option<BlockSummary>, String> {
    let found = main.entries.iter().rev().find_map(|e| {
        if e.hash == *block_hash {
            return Some(e.summary());
        }
        e.merged.iter().find(|m| m.hash == *block_hash).copied()
    });
    if found.is_some() {
        return Ok(found);
    }
    match &main.history {
        Some(history) => Ok(history
            .accepted(block_hash)?
            .filter(|(height, _)| *height < main.base)
            .map(|(_, summary)| summary)),
        None => Ok(None),
    }
}

/// The carrier of block `block_hash` (D-095): the block must be accepted by the main chain (`main`, up to the selected
/// parent at execution time); the carrier is the sender of the first landing of its data
/// ([`BtcSnapshot::data_landing`]): for a settlement payload the producer of the block it belongs to (that block must
/// also be accepted by `main`, otherwise there is no carrier), for a republish payload its payee account (must be
/// non-zero, and the block's data must have waited more than `ANCHOR_DEPTH + CARRIER_REPOST_DELAY` BTC blocks after
/// the anchor: `landing height − anchor height > ANCHOR_DEPTH + CARRIER_REPOST_DELAY`). When `limit` is given, only
/// landings at heights ≤ `limit` count (the anchor height: the answer depends only on the part of the anchor chain up
/// to the anchor height). `Err` is a storage fault of the main chain's history (D-097).
pub fn carrier_of(
    params: &Params,
    snapshot: &BtcSnapshot,
    main: &MainChain,
    limit: Option<u32>,
    block_hash: &[u8; 32],
) -> Result<Option<[u8; 32]>, String> {
    let Some(block) = accepted_summary(main, block_hash)? else {
        return Ok(None);
    };
    let Some(landing) = snapshot.data_landing(&block.hash) else {
        return Ok(None);
    };
    if limit.is_some_and(|limit| landing.height > limit) {
        return Ok(None);
    }
    match landing.sender {
        Sender::Block(carrier_block) => {
            Ok(accepted_summary(main, &carrier_block)?.map(|c| c.producer))
        }
        Sender::Payee(payee) => {
            let waited = u64::from(landing.height).saturating_sub(u64::from(block.anchor_height));
            let bound = u64::from(params.anchor_depth)
                .saturating_add(u64::from(params.carrier_repost_delay));
            Ok((payee != [0; 32] && waited > bound).then_some(payee))
        }
    }
}

/// The highest main-chain height that has entered the settled closure at anchor height `anchor_height` (D-038, D-063,
/// D-074): the largest `h` with [`prefix_height`]`(h) ≤ anchor_height` (the settled prefix is non-decreasing in
/// height); 0 when not even height 1 qualifies. The heights below the window of `main` count as settled when its
/// settled prefix is within the anchor height; otherwise the answer lies below the window (`Err`, D-097).
pub fn settled_height(
    snapshot: &BtcSnapshot,
    main: &MainChain,
    anchor_height: u32,
) -> Result<u64, String> {
    if main.base_prefix > anchor_height {
        return Err(format!(
            "the settled height at anchor height {anchor_height} lies below the pruned main chain (its settled prefix \
             is {}, D-097)",
            main.base_prefix
        ));
    }
    let mut settled = main.base.saturating_sub(1);
    let mut prefix = main.base_prefix;
    for (height, entry) in main.iter_from(1) {
        for block in entry.accepted() {
            match snapshot.data_height(&block) {
                Some(h) => prefix = prefix.max(h),
                None => return Ok(settled),
            }
        }
        if prefix > anchor_height {
            return Ok(settled);
        }
        settled = height;
    }
    Ok(settled)
}

/// The unlockable segment (D-079): the positions on the main chain (index = height; index 0 is the genesis block, which
/// accepts no blocks) after the unlock cursor `cursor` in acceptance order, at most `max` (`MAX_UNLOCK_BATCH`) of them.
/// For a main-chain block, acceptance order is first the blocks it merges (indices 0, 1, ...), and last the block
/// itself.
///
/// When executing a main-chain block, the node and the indexer call this with the selected parent and its ancestors
/// (`main` up to the parent) and the cursor of the parent state; the result is `BlockEnv::unlockable`, and both give
/// the same segment for the same block. The window of `main` must reach down to the cursor (D-097: it is never pruned
/// above the cursor of a state that can still be executed); otherwise `Err`.
pub fn unlock_segment(
    main: &MainChain,
    cursor: Option<(u64, u32)>,
    max: u64,
) -> Result<Vec<UnlockItem>, String> {
    let max = usize::try_from(max).unwrap_or(usize::MAX);
    let start = cursor.map_or(1, |(height, _)| height.max(1));
    if start < main.base {
        return Err(format!(
            "the unlock cursor at height {start} is below the main-chain window starting at {} (D-097)",
            main.base
        ));
    }
    let mut items = Vec::new();
    for (height, entry) in main.iter_from(start) {
        for (index, block) in entry.accepted().enumerate() {
            let Ok(index) = u32::try_from(index) else {
                break;
            };
            if cursor.is_some_and(|c| (height, index) <= c) {
                continue;
            }
            if items.len() >= max {
                return Ok(items);
            }
            items.push(UnlockItem {
                height,
                index,
                hash: block.hash,
                producer: block.producer,
                daa_score: block.daa_score,
                weight: block.weight,
                rewarded: block.rewarded,
            });
        }
    }
    Ok(items)
}

/// The unlock cursor in the state (`0x09`, D-079); `None` when nothing has been unlocked yet. `Err` is a storage fault
/// or a corrupt value.
pub fn unlock_cursor(state: &dyn StateSource) -> Result<Option<(u64, u32)>, String> {
    match state.get(&keys::unlock_cursor())? {
        None => Ok(None),
        Some(bytes) => {
            let cursor: UnlockCursor =
                borsh::from_slice(&bytes).map_err(|e| format!("unlock cursor corrupt: {e}"))?;
            Ok(Some((cursor.height, cursor.index)))
        }
    }
}

/// The unlock target the producer writes (D-079): the last position in the segment whose acceptance height is ≤
/// `settled` (the highest main-chain height that has entered the settled closure at the anchor, [`settled_height`]).
/// The settled closure advances as a whole by main-chain height, and the segment is in acceptance order.
pub fn unlock_target(segment: &[UnlockItem], settled: u64) -> Option<(u64, u32)> {
    segment
        .iter()
        .take_while(|item| item.height <= settled)
        .last()
        .map(UnlockItem::position)
}

/// The first BTC height a fresh snapshot covers: `min(genesis anchor height, h(genesis) + 1)` (the first
/// forced-inclusion range of §7.4 starts at `h(genesis) + 1`).
fn initial_base(params: &Params, genesis_anchor: &BtcAnchor) -> u32 {
    let first_forced = crate::native::inclusion_height(params, genesis_anchor.height)
        .saturating_add(1)
        .max(0);
    u32::try_from(first_forced)
        .unwrap_or(u32::MAX)
        .min(genesis_anchor.height)
}

/// Tracks bitcoind's best chain.
pub struct BtcTracker {
    client: Arc<dyn BtcClient>,
    params: Params,
    /// The genesis block's hash: it always counts (D-135).
    genesis: [u8; 32],
    genesis_anchor: BtcAnchor,
    snapshot: RwLock<Arc<BtcSnapshot>>,
    /// Snapshot updates (sync, pruning) are mutually exclusive, so that concurrent updates do not overwrite each other.
    update: Mutex<()>,
}

fn btc_err(e: impl std::fmt::Display) -> TrackerError {
    TrackerError::Btc(e.to_string())
}

/// A settlement or republish payload of the BTC block being scanned that counts ([`payload_counts`]) and has a sender.
struct Scanned<'a> {
    tx: &'a bitcoin::Transaction,
    payload: Payload,
    /// The hashes of its headers, in order.
    hashes: Vec<[u8; 32]>,
    landing: Landing,
}

impl Scanned<'_> {
    fn headers(&self) -> &[BlockHeader] {
        match &self.payload {
            Payload::Settlement { headers, .. } | Payload::Headers { headers, .. } => headers,
            Payload::NativeOp { .. } => &[],
        }
    }
}

/// A header of the BTC block being scanned that does not count yet, at its earliest appearance in that block.
struct Appearance<'a> {
    header: &'a BlockHeader,
    landing: Landing,
    /// Its position: the transaction's index in the BTC block and the header's index in the payload.
    order: (u32, u32),
}

impl BtcTracker {
    /// Creates the tracker: the anchor block of the genesis block `genesis` must be on bitcoind's best chain (D-035).
    /// The snapshot starts at `min(genesis anchor height, h(genesis) + 1)` (the first forced-inclusion range of §7.4
    /// starts at `h(genesis) + 1`).
    pub fn new(
        client: Arc<dyn BtcClient>,
        params: Params,
        genesis: &BlockHeader,
    ) -> Result<Self, TrackerError> {
        let base = initial_base(&params, &genesis.btc_anchor);
        Self::with_snapshot(client, params, genesis, BtcSnapshot::new(base))
    }

    /// Creates the tracker continuing from `snapshot` (restored from a pruning checkpoint, D-097): the next sync
    /// continues from its tip, as if this process had scanned it. The genesis anchor block must be on bitcoind's best
    /// chain (D-035).
    pub fn with_snapshot(
        client: Arc<dyn BtcClient>,
        params: Params,
        genesis: &BlockHeader,
        snapshot: BtcSnapshot,
    ) -> Result<Self, TrackerError> {
        let genesis_anchor = genesis.btc_anchor.clone();
        let info = client.header(&genesis_anchor.hash).map_err(btc_err)?;
        match info {
            Some(info) if info.in_best_chain && info.height == genesis_anchor.height => {}
            _ => {
                return Err(TrackerError::Config(format!(
                    "the genesis anchor block (height {}) is not on bitcoind's best chain",
                    genesis_anchor.height
                )))
            }
        }
        Ok(BtcTracker {
            client,
            params,
            genesis: genesis.hash(),
            genesis_anchor,
            snapshot: RwLock::new(Arc::new(snapshot)),
            update: Mutex::new(()),
        })
    }

    /// Starts over from the genesis anchor (D-097: the indexer rebuilding from the genesis block after a BTC reorg
    /// deeper than what pruning kept): the next sync scans again from the start.
    pub fn reset(&self) -> Result<(), TrackerError> {
        let _guard = self.lock_update()?;
        self.publish(BtcSnapshot::new(initial_base(
            &self.params,
            &self.genesis_anchor,
        )))
    }

    /// Prunes the snapshot (D-097, [`BtcSnapshot::prune`]) and publishes the result. Returns the number of records of
    /// fast-lane blocks dropped.
    pub fn prune(&self, cut: u32, keep: &dyn Fn(&[u8; 32]) -> bool) -> Result<usize, TrackerError> {
        let _guard = self.lock_update()?;
        let mut next = (*self.snapshot()?).clone();
        let dropped = next.prune(cut, keep);
        self.publish(next)?;
        Ok(dropped)
    }

    /// The bitcoind client.
    pub fn client(&self) -> &Arc<dyn BtcClient> {
        &self.client
    }

    /// The genesis block's BTC anchor.
    pub fn genesis_anchor(&self) -> &BtcAnchor {
        &self.genesis_anchor
    }

    /// The current snapshot.
    pub fn snapshot(&self) -> Result<Arc<BtcSnapshot>, TrackerError> {
        self.snapshot
            .read()
            .map(|s| Arc::clone(&s))
            .map_err(|_| TrackerError::Internal("the BTC snapshot lock is poisoned".into()))
    }

    fn publish(&self, snapshot: BtcSnapshot) -> Result<(), TrackerError> {
        let mut guard = self
            .snapshot
            .write()
            .map_err(|_| TrackerError::Internal("the BTC snapshot lock is poisoned".into()))?;
        *guard = Arc::new(snapshot);
        Ok(())
    }

    /// Runs one round of syncing with bitcoind's best chain (at most [`SYNC_BATCH`] new blocks). Returns whether the
    /// snapshot changed. `known` checks the headers that may count (D-135).
    pub fn sync(&self, known: &dyn KnownBlocks) -> Result<bool, TrackerError> {
        let _guard = self.lock_update()?;
        let (tip_height, tip_hash) = self.client.tip().map_err(btc_err)?;
        let old = self.snapshot()?;
        if old.tip().map(|(_, h)| h) == Some(tip_hash) {
            return Ok(false);
        }
        let mut next = (*old).clone();

        // the fork point: the highest block common to the snapshot and bitcoind's best chain
        let mut fork = next.tip().map(|(height, _)| height.min(tip_height));
        while let Some(height) = fork {
            let ours = next.hash_at(height);
            let theirs = self.client.block_hash(height).map_err(btc_err)?;
            if ours.is_some() && ours == theirs {
                break;
            }
            fork = height.checked_sub(1).filter(|h| *h >= next.base);
        }
        let start = match fork {
            Some(height) => {
                let removed = next.truncate(height);
                if next.tip().map(|(h, _)| h) != old.tip().map(|(h, _)| h) {
                    eprintln!(
                        "[ksana-node] warning: BTC reorg, rolled back to height {height} ({removed} native transactions removed; the fast-lane blocks anchored above this height leave the main chain, D-104)"
                    );
                }
                height.saturating_add(1)
            }
            None => {
                if next.tip().is_some() {
                    return Err(TrackerError::TooDeep(format!(
                        "the BTC reorg went below height {}, the lowest height kept (near the genesis anchor, or the \
                         pruned range, D-097)",
                        next.base
                    )));
                }
                next = BtcSnapshot::new(next.base);
                next.base
            }
        };

        let end = tip_height.min(start.saturating_add(SYNC_BATCH.saturating_sub(1)));
        let mut prev = next.tip().map(|(_, h)| h);
        let mut height = start;
        while height <= end {
            let Some(hash) = self.client.block_hash(height).map_err(btc_err)? else {
                break;
            };
            let Some(block) = self.client.block(&hash).map_err(btc_err)? else {
                break;
            };
            if let Some(prev) = prev {
                if block.header.prev_blockhash.to_byte_array() != prev {
                    // the best chain changed again during the sync: publish the processed part first and find the fork
                    // point again next round
                    break;
                }
            }
            self.scan(&mut next, height, hash, &block, known)?;
            prev = Some(hash);
            let Some(h) = height.checked_add(1) else {
                break;
            };
            height = h;
        }
        // after pruning (D-097) the genesis anchor is below the snapshot; a reorg that deep was refused above
        let anchor_kept = self.genesis_anchor.height >= next.base;
        if next.tip().is_none() || (anchor_kept && !next.contains(&self.genesis_anchor)) {
            return Err(TrackerError::Btc(
                "bitcoind's best chain no longer contains the genesis anchor block".into(),
            ));
        }
        self.publish(next)?;
        Ok(true)
    }

    fn lock_update(&self) -> Result<std::sync::MutexGuard<'_, ()>, TrackerError> {
        self.update
            .lock()
            .map_err(|_| TrackerError::Internal("the BTC snapshot update lock is poisoned".into()))
    }

    /// Scans one BTC block: the header, native transactions, and the KSN1 settlement and republish payloads that count
    /// ([`payload_counts`]): the headers that count at this height, then the bodies that land at it (D-135, see the
    /// module docs).
    fn scan(
        &self,
        snap: &mut BtcSnapshot,
        height: u32,
        hash: [u8; 32],
        block: &bitcoin::Block,
        known: &dyn KnownBlocks,
    ) -> Result<(), TrackerError> {
        let header: [u8; 80] = serialize(&block.header)
            .try_into()
            .map_err(|_| TrackerError::Btc("the block header is not 80 bytes".into()))?;
        snap.chain.push((hash, header));
        let natives = native_txs(&self.params, block);
        if !natives.is_empty() {
            snap.native.insert(height, natives);
        }
        let mut scanned = Vec::new();
        for (index, tx) in block.txdata.iter().enumerate() {
            let Some((_, payload)) = tx_payload(&self.params, tx) else {
                continue;
            };
            let headers = match &payload {
                Payload::Settlement { headers, .. } | Payload::Headers { headers, .. } => headers,
                Payload::NativeOp { .. } => continue,
            };
            if !payload_counts(headers, height) {
                continue;
            }
            let index = u32::try_from(index).map_err(|_| {
                TrackerError::Btc("the transaction index in the BTC block overflows".into())
            })?;
            let txid = tx.compute_txid().to_byte_array();
            if matches!(payload, Payload::Settlement { .. }) {
                snap.ksn1_txs.entry(txid).or_insert(height);
            }
            // the payload's sender (D-095): a settlement payload without headers carries neither headers nor bodies
            let Some(sender) = payload_sender(&payload) else {
                continue;
            };
            let hashes = headers.iter().map(BlockHeader::hash).collect();
            scanned.push(Scanned {
                tx,
                payload,
                hashes,
                landing: Landing {
                    txid,
                    height,
                    index,
                    sender,
                },
            });
        }
        self.count_headers(snap, height, &scanned, known)?;
        self.land_bodies(snap, height, &scanned)
    }

    /// Counts the headers carried by this BTC block's payloads (SPEC §6.5 item 1, D-135): a header counts once its
    /// anchor is on the best chain, each of its parents has counted (below, or at this height) and `known` accepts it.
    /// A header is tried once its last parent counts, the ready ones in order of first appearance; whether a header
    /// counts depends only on it, its past and the BTC chain, so the headers that count are the same in any order of the
    /// transactions and of the headers in their payloads. A header with a parent that neither counts nor appears in
    /// this block is not even tried.
    fn count_headers(
        &self,
        snap: &mut BtcSnapshot,
        height: u32,
        scanned: &[Scanned<'_>],
        known: &dyn KnownBlocks,
    ) -> Result<(), TrackerError> {
        // each header not counted yet, at its earliest appearance in this block
        let mut waiting: BTreeMap<[u8; 32], Appearance<'_>> = BTreeMap::new();
        for s in scanned {
            for (position, (header, hash)) in s.headers().iter().zip(&s.hashes).enumerate() {
                if *hash == self.genesis || snap.counted.contains_key(hash) {
                    continue;
                }
                let position = u32::try_from(position).unwrap_or(u32::MAX);
                waiting.entry(*hash).or_insert(Appearance {
                    header,
                    landing: s.landing,
                    order: (s.landing.index, position),
                });
            }
        }
        // how many parents each header still waits for, and the headers waiting for each parent: a parent that has not
        // counted must be among them, or the header cannot count at this height
        let mut missing: BTreeMap<[u8; 32], usize> = BTreeMap::new();
        let mut children: BTreeMap<[u8; 32], Vec<[u8; 32]>> = BTreeMap::new();
        let mut ready: BTreeSet<((u32, u32), [u8; 32])> = BTreeSet::new();
        for (hash, appearance) in &waiting {
            let parents: Vec<&[u8; 32]> = appearance
                .header
                .parents
                .iter()
                .filter(|p| **p != self.genesis && !snap.counted.contains_key(*p))
                .collect();
            if parents.iter().any(|p| !waiting.contains_key(*p)) {
                continue;
            }
            if parents.is_empty() {
                ready.insert((appearance.order, *hash));
                continue;
            }
            missing.insert(*hash, parents.len());
            for parent in parents {
                children.entry(*parent).or_default().push(*hash);
            }
        }
        while let Some((_, hash)) = ready.pop_first() {
            let Some(appearance) = waiting.get(&hash) else {
                continue;
            };
            let Some(counted) =
                self.try_count(snap, appearance.header, appearance.landing, known)?
            else {
                continue;
            };
            snap.counted.insert(hash, counted);
            snap.headers_by_height.entry(height).or_default().push(hash);
            for child in children.remove(&hash).unwrap_or_default() {
                let Some(left) = missing.get_mut(&child) else {
                    continue;
                };
                *left = left.saturating_sub(1);
                if *left == 0 {
                    missing.remove(&child);
                    if let Some(next) = waiting.get(&child) {
                        ready.insert((next.order, child));
                    }
                }
            }
        }
        Ok(())
    }

    /// Whether `header`, whose parents have all counted, counts (D-135): its anchor is on the best chain and `known`
    /// accepts it. When it does, also settles whether its block needs a body, from the entries it must force-include
    /// (D-043, D-077).
    fn try_count(
        &self,
        snap: &BtcSnapshot,
        header: &BlockHeader,
        landing: Landing,
        known: &dyn KnownBlocks,
    ) -> Result<Option<Counted>, TrackerError> {
        if !snap.contains(&header.btc_anchor) {
            return Ok(None);
        }
        let mut parent_anchor = 0u32;
        for parent in &header.parents {
            let anchor = if *parent == self.genesis {
                self.genesis_anchor.height
            } else {
                match snap.counted.get(parent) {
                    Some(counted) => counted.anchor,
                    None => return Ok(None),
                }
            };
            parent_anchor = parent_anchor.max(anchor);
        }
        if !known.count(snap, header)? {
            return Ok(None);
        }
        let anchor = header.btc_anchor.height;
        let forced = forced_at(&self.params, snap, parent_anchor, anchor);
        let root = entries_root(&forced)
            .map_err(|e| TrackerError::Internal(format!("computing entries_root failed: {e}")))?;
        Ok(Some(Counted {
            landing,
            anchor,
            parent_anchor,
            settles: root != header.entries_root,
        }))
    }

    /// Lands the bodies carried by this BTC block's payloads (D-077, D-135): a body lands for a block whose header
    /// counts and that needs one, when its entries match the header's `entries_root` once the entries the block must
    /// force-include replace its `Forced` entries. Only the first landing is kept, the earliest in the BTC block; the
    /// bodies one transaction lands share one copy of it.
    fn land_bodies(
        &self,
        snap: &mut BtcSnapshot,
        height: u32,
        scanned: &[Scanned<'_>],
    ) -> Result<(), TrackerError> {
        let mut forced: BTreeMap<[u8; 32], Vec<Entry>> = BTreeMap::new();
        for s in scanned {
            let mut copy: Option<Arc<bitcoin::Transaction>> = None;
            let mut refused = 0usize;
            for (slot, (index, entries)) in bodies_at(&s.payload).into_iter().enumerate() {
                let (Some(hash), Some(header)) = (s.hashes.get(index), s.headers().get(index))
                else {
                    continue;
                };
                if snap.bodies.contains_key(hash) {
                    continue;
                }
                let Some(counted) = snap.counted.get(hash).filter(|c| c.settles) else {
                    continue;
                };
                let forced = forced.entry(*hash).or_insert_with(|| {
                    forced_at(&self.params, snap, counted.parent_anchor, counted.anchor)
                });
                let matches = match convert(entries, forced) {
                    Some(body) => {
                        entries_root(&body).map_err(|e| {
                            TrackerError::Internal(format!("computing entries_root failed: {e}"))
                        })? == header.entries_root
                    }
                    None => false,
                };
                if !matches {
                    refused = refused.saturating_add(1);
                    continue;
                }
                let tx = copy.get_or_insert_with(|| Arc::new(s.tx.clone()));
                snap.bodies.insert(
                    *hash,
                    LandedBody {
                        landing: s.landing,
                        tx: Arc::clone(tx),
                        slot: u32::try_from(slot).unwrap_or(u32::MAX),
                    },
                );
            }
            if refused > 0 {
                eprintln!(
                    "[ksana-btc] transaction {} at BTC height {height} carries {refused} bodies that do not match their \
                     headers (D-135)",
                    hex::encode(s.landing.txid)
                );
            }
        }
        Ok(())
    }
}

/// The execution layer's BTC view: relative to `anchor` (D-035).
///
/// Syncing historical blocks uses [`AnchoredView::history`]: when the anchor block is no longer on the local BTC best
/// chain (reorged out by BTC) but its height is not above the snapshot tip, the view enters anchor-invalid mode
/// (D-056) and answers by the current best chain: confirmed outputs are looked up by transaction ID at any height (the
/// contents of the same transaction ID do not change, so the execution result is the same as on the anchor chain), and
/// when not found `Err` is returned (a local fault, retry later), never "does not exist"; the settled closure is judged
/// by the current best chain; for headers see [`AnchoredView::stale_header`].
pub struct AnchoredView {
    anchor: BtcAnchor,
    snapshot: Arc<BtcSnapshot>,
    client: Arc<dyn BtcClient>,
    /// The parent block and its ancestors, for the settled prefix and the carrier (D-095).
    main: Arc<MainChain>,
    /// The network parameters (the carrier's republish wait, D-095).
    params: Params,
    /// Anchor-invalid mode (D-056).
    stale: bool,
}

/// The time field of an 80-byte BTC block header (bytes 68-71, little-endian).
pub fn header_time(header: &[u8; 80]) -> Option<u32> {
    let bytes: [u8; 4] = header.get(68..72)?.try_into().ok()?;
    Some(u32::from_le_bytes(bytes))
}

fn header_bytes(header: &bitcoin::block::Header) -> Result<[u8; 80], String> {
    serialize(header)
        .try_into()
        .map_err(|_| "the block header is not 80 bytes".to_string())
}

impl AnchoredView {
    /// Creates the view (for block production and receiving new blocks): the anchor block must be on the snapshot's
    /// best chain, otherwise the methods return `Err`.
    pub fn new(
        anchor: BtcAnchor,
        snapshot: Arc<BtcSnapshot>,
        client: Arc<dyn BtcClient>,
        main: Arc<MainChain>,
        params: Params,
    ) -> Self {
        AnchoredView {
            anchor,
            snapshot,
            client,
            main,
            params,
            stale: false,
        }
    }

    /// The view for syncing historical blocks (P2P sync, connecting a side chain on reorg, crash recovery, the indexer)
    /// (D-056): when the anchor block is not on the snapshot's best chain but its height is not above the snapshot tip,
    /// the view enters anchor-invalid mode; otherwise the same as [`AnchoredView::new`].
    pub fn history(
        anchor: BtcAnchor,
        snapshot: Arc<BtcSnapshot>,
        client: Arc<dyn BtcClient>,
        main: Arc<MainChain>,
        params: Params,
    ) -> Self {
        let stale = !snapshot.contains(&anchor)
            && snapshot.tip().is_some_and(|(tip, _)| anchor.height <= tip);
        AnchoredView {
            anchor,
            snapshot,
            client,
            main,
            params,
            stale,
        }
    }

    /// Whether the view is in anchor-invalid mode (D-056).
    pub fn is_stale(&self) -> bool {
        self.stale
    }

    fn check_snapshot(&self) -> Result<(), String> {
        if self.snapshot.contains(&self.anchor) {
            Ok(())
        } else {
            Err(format!(
                "the anchor block (height {}) is not on the best chain of the local BTC snapshot",
                self.anchor.height
            ))
        }
    }

    /// The anchor block is still on bitcoind's best chain (called after querying bitcoind, guaranteeing that the answer
    /// corresponds to the anchor chain).
    fn check_bitcoind(&self) -> Result<(), String> {
        match self.client.header(&self.anchor.hash) {
            Ok(Some(info)) if info.in_best_chain && info.height == self.anchor.height => Ok(()),
            Ok(_) => Err("the anchor block is no longer on bitcoind's best chain".into()),
            Err(e) => Err(e.to_string()),
        }
    }

    /// The header at height `height` on the current best chain: from the snapshot within its range, from bitcoind below
    /// it.
    fn current_header(&self, height: u32) -> Result<[u8; 80], String> {
        if let Some(header) = self.snapshot.header_at(height) {
            return Ok(header);
        }
        let Some(hash) = self.client.block_hash(height).map_err(|e| e.to_string())? else {
            return Err(format!("bitcoind has no block at height {height}"));
        };
        let info = self
            .client
            .header(&hash)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("bitcoind has no header {}", hex::encode(hash)))?;
        header_bytes(&info.header)
    }

    /// In anchor-invalid mode, the header at height `height` (not above the anchor height) on the anchor chain: when
    /// bitcoind knows the anchor block (it saw the stale branch before the reorg), walk back along the parent hashes to
    /// the current best chain, taking the blocks on the stale branch passed through directly and the lower ones from
    /// the current best chain (they are ancestors of the anchor block). When bitcoind does not know the anchor block,
    /// answer by the current best chain: contracts can only read headers below anchor height − `BTC_QUERY_DEPTH`, so
    /// only a deeper BTC reorg gives a wrong answer, in which case the state root of the re-execution does not match
    /// and the caller treats it as a local fault (D-056).
    fn stale_header(&self, height: u32) -> Result<[u8; 80], String> {
        let mut hash = self.anchor.hash;
        let mut at = self.anchor.height;
        while let Some(info) = self.client.header(&hash).map_err(|e| e.to_string())? {
            if info.in_best_chain {
                break;
            }
            if info.height != at {
                return Err(format!(
                    "block {} on the stale branch in bitcoind has height {}, expected {at}",
                    hex::encode(hash),
                    info.height
                ));
            }
            if at == height {
                return header_bytes(&info.header);
            }
            hash = info.header.prev_blockhash.to_byte_array();
            at = at
                .checked_sub(1)
                .ok_or_else(|| "the stale branch walked back below height 0".to_string())?;
        }
        self.current_header(height)
    }

    /// Anchor-invalid mode: the transaction is confirmed on the current best chain (at any height); when it is not on
    /// the current best chain, a local fault (D-056).
    fn stale_confirmed(&self, txid: &[u8; 32]) -> Result<bitcoin::Transaction, String> {
        let missing = || {
            format!(
                "anchor-invalid historical block (D-056): transaction {} is not on the current BTC best chain; retry later",
                bitcoin::Txid::from_byte_array(*txid)
            )
        };
        let info = self
            .client
            .transaction(txid)
            .map_err(|e| e.to_string())?
            .ok_or_else(missing)?;
        let block = info.block.ok_or_else(missing)?;
        match self.client.header(&block).map_err(|e| e.to_string())? {
            Some(header) if header.in_best_chain => Ok(info.tx),
            _ => Err(missing()),
        }
    }

    /// A transaction output confirmed in a block at height ≤ the anchor height on the anchor chain (whether or not it
    /// was spent later); otherwise returns `None` (the previous outputs of the settlement transactions a block carries,
    /// D-034). In anchor-invalid mode, looked up by transaction ID on the current best chain (D-056). `Err` means a
    /// data source fault.
    pub fn confirmed_output(
        &self,
        outpoint: &bitcoin::OutPoint,
    ) -> Result<Option<bitcoin::TxOut>, String> {
        let txid = outpoint.txid.to_byte_array();
        let output = |tx: &bitcoin::Transaction| {
            usize::try_from(outpoint.vout)
                .ok()
                .and_then(|v| tx.output.get(v))
                .cloned()
        };
        if self.stale {
            return Ok(output(&self.stale_confirmed(&txid)?));
        }
        self.check_snapshot()?;
        let found = match self.client.transaction(&txid).map_err(|e| e.to_string())? {
            Some(info) => match info.block {
                Some(block) => {
                    let header = self.client.header(&block).map_err(|e| e.to_string())?;
                    match header {
                        Some(h) if h.in_best_chain && h.height <= self.anchor.height => {
                            output(&info.tx)
                        }
                        _ => None,
                    }
                }
                None => None,
            },
            None => None,
        };
        if found.is_none() {
            // "does not exist" is trustworthy only once txindex has synced to the anchor height (txindex updates
            // asynchronously after a new block connects)
            let indexed = self.client.txindex_height().map_err(|e| e.to_string())?;
            if indexed.is_none_or(|h| h < self.anchor.height) {
                return Err("bitcoind's txindex has not synced to the anchor height yet".into());
            }
        }
        self.check_bitcoind()?;
        Ok(found)
    }
}

impl BtcView for AnchoredView {
    fn header(&self, height: u32) -> Result<Option<[u8; 80]>, String> {
        if height > self.anchor.height {
            return Ok(None);
        }
        if self.stale {
            return self.stale_header(height).map(Some);
        }
        self.check_snapshot()?;
        if let Some(header) = self.snapshot.header_at(height) {
            return Ok(Some(header));
        }
        // below the snapshot range: query bitcoind, then confirm the anchor block is still on its best chain
        let bytes = self.current_header(height)?;
        self.check_bitcoind()?;
        Ok(Some(bytes))
    }

    fn block_settled(&self, block_hash: &[u8; 32]) -> Result<bool, String> {
        let Some(height) = accepted_height(&self.main, block_hash) else {
            return self.settled_below_window(block_hash);
        };
        if self.stale {
            // whether it is a block on this chain does not depend on BTC; the settled closure is judged by the current
            // best chain (at any height), a local fault when it does not hold (D-056)
            return match prefix_height(&self.snapshot, &self.main, height) {
                Some(_) => Ok(true),
                None => Err(format!(
                    "anchor-invalid historical block (D-056): the settled prefix of block {} is incomplete on the current BTC best chain; retry later",
                    hex::encode(block_hash)
                )),
            };
        }
        self.check_snapshot()?;
        let prefix = prefix_height(&self.snapshot, &self.main, height);
        Ok(prefix.is_some_and(|p| p <= self.anchor.height))
    }

    fn carrier(&self, block_hash: &[u8; 32]) -> Result<Option<[u8; 32]>, String> {
        if self.stale {
            // anchor-invalid (D-056): answer by the current best chain, at any height
            return carrier_of(&self.params, &self.snapshot, &self.main, None, block_hash);
        }
        self.check_snapshot()?;
        carrier_of(
            &self.params,
            &self.snapshot,
            &self.main,
            Some(self.anchor.height),
            block_hash,
        )
    }

    fn native_input(&self, txid: &[u8; 32], op: &VmOp) -> Result<Option<OutPoint>, String> {
        if self.stale {
            // the contents of the same transaction ID do not change: take it from the current best chain (at any
            // height), a local fault when it is not on the current best chain (D-056)
            let tx = self.stale_confirmed(txid)?;
            return Ok(crate::native::native_op(&self.params, &tx)
                .filter(|carried| carried == op)
                .and_then(|_| crate::native::first_input(&tx)));
        }
        // On the anchor chain the snapshot answers alone: every forced-inclusion entry of a block a node or indexer
        // still processes names a transaction from that block's window start on (derived over its window, or for a
        // block accepted while its anchor was invalid bounded by `bodies::stale_range`, D-100), and the window start
        // is above the cut of a pruned snapshot (D-097). A transaction not there names no path-C op, whatever bitcoind
        // may hold at other heights
        self.check_snapshot()?;
        Ok(self
            .snapshot
            .native_tx_at_most(txid, self.anchor.height)
            .filter(|native| native.op == *op)
            .map(|native| native.first_input.clone()))
    }

    fn native_outputs(&self, txid: &[u8; 32]) -> Result<Vec<(u64, Vec<u8>)>, String> {
        self.native_record(txid).map(|(_, outputs)| outputs)
    }
}

/// The first input and all outputs (amount, locking script) of a native transaction.
type NativeRecord = (OutPoint, Vec<(u64, Vec<u8>)>);

impl AnchoredView {
    /// [`BtcView::block_settled`] for a block not accepted within the main chain's window (D-097): a block the main
    /// chain accepted below the window has landed by the window's settled prefix, so it is settled whenever that prefix
    /// is within the anchor height; below that prefix the answer cannot be given here (a local fault). A block the
    /// main chain did not accept is not settled.
    fn settled_below_window(&self, block_hash: &[u8; 32]) -> Result<bool, String> {
        let Some(history) = self.main.history() else {
            return Ok(false);
        };
        let below = history
            .accepted(block_hash)?
            .is_some_and(|(height, _)| height < self.main.base());
        if !below {
            return Ok(false);
        }
        if self.stale || self.main.base_prefix() <= self.anchor.height {
            return Ok(true);
        }
        Err(format!(
            "block {} was accepted below the pruned main chain, whose settled prefix {} is above the anchor height {} (D-097)",
            hex::encode(block_hash),
            self.main.base_prefix(),
            self.anchor.height
        ))
    }

    /// The first input and all outputs of native transaction `txid` confirmed in a block at height ≤ the anchor
    /// height on the anchor chain (shared by the two native transaction queries of [`BtcView`]).
    fn native_record(&self, txid: &[u8; 32]) -> Result<NativeRecord, String> {
        let record = |tx: &bitcoin::Transaction| {
            let first_input = crate::native::first_input(tx).ok_or_else(|| {
                format!(
                    "transaction {} is a coinbase, not a native transaction",
                    bitcoin::Txid::from_byte_array(*txid)
                )
            })?;
            Ok((first_input, crate::native::tx_outputs(tx)))
        };
        if self.stale {
            // the contents of the same transaction ID do not change: take it from the current best chain (at any
            // height), a local fault when it is not on the current best chain (D-056)
            return record(&self.stale_confirmed(txid)?);
        }
        self.check_snapshot()?;
        if let Some(native) = self.snapshot.native_tx_at_most(txid, self.anchor.height) {
            return Ok((native.first_input.clone(), native.outputs.clone()));
        }
        // below the snapshot range: query bitcoind by transaction ID; it must be on the anchor chain at height ≤ the
        // anchor height, then confirm the anchor block is still on its best chain
        let missing = || {
            format!(
                "native transaction {} not found on the anchor chain",
                bitcoin::Txid::from_byte_array(*txid)
            )
        };
        let info = self
            .client
            .transaction(txid)
            .map_err(|e| e.to_string())?
            .ok_or_else(missing)?;
        let block = info.block.ok_or_else(missing)?;
        match self.client.header(&block).map_err(|e| e.to_string())? {
            Some(h) if h.in_best_chain && h.height <= self.anchor.height => {}
            _ => return Err(missing()),
        }
        self.check_bitcoind()?;
        record(&info.tx)
    }
}

/// The previous outputs of the settlement transactions a block carries (D-034): only outputs confirmed on the anchor
/// chain at heights ≤ the anchor height are accepted.
pub struct AnchoredPrevouts<'a>(pub &'a AnchoredView);

impl PrevoutSource for AnchoredPrevouts<'_> {
    fn prevout(&self, outpoint: &bitcoin::OutPoint) -> Result<Option<bitcoin::TxOut>, String> {
        self.0.confirmed_output(outpoint)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ksana_primitives::DEVNET;

    fn summary(seed: u8, settles: bool) -> BlockSummary {
        BlockSummary {
            hash: [seed; 32],
            settles,
            anchor_height: 0,
            producer: [seed.wrapping_add(100); 32],
            daa_score: u64::from(seed),
            // blocks with an odd seed count as empty blocks (weight 12, D-084)
            weight: if seed.is_multiple_of(2) { 1 } else { 12 },
            rewarded: seed.is_multiple_of(2),
        }
    }

    /// A main chain from the genesis block (never pruned).
    fn chain(entries: Vec<MainEntry>) -> MainChain {
        MainChain::from_window(0, 0, entries).unwrap()
    }

    fn entry(seed: u8, settles: bool, merged: Vec<BlockSummary>) -> MainEntry {
        MainEntry {
            hash: [seed; 32],
            settles,
            anchor_height: 0,
            producer: [seed.wrapping_add(100); 32],
            daa_score: u64::from(seed),
            weight: if seed.is_multiple_of(2) { 1 } else { 12 },
            merged,
        }
    }

    /// The unlockable segment lists the positions after the cursor in acceptance order (each main-chain block is
    /// preceded by the blocks it merges), at most `max` of them; the producer's target is the last position among them
    /// whose acceptance height is ≤ the settled height (D-079).
    #[test]
    fn unlock_segment_lists_positions_after_the_cursor() {
        let entries = vec![
            entry(0, false, vec![]),
            entry(1, true, vec![]),
            entry(2, false, vec![summary(20, false), summary(21, true)]),
            entry(3, false, vec![]),
        ];
        let main = chain(entries.clone());
        let positions = |segment: &[UnlockItem]| -> Vec<(u64, u32)> {
            segment.iter().map(UnlockItem::position).collect()
        };
        let segment = |main: &MainChain, cursor, max| unlock_segment(main, cursor, max).unwrap();
        let all = segment(&main, None, 100);
        assert_eq!(
            positions(&all),
            vec![(1, 0), (2, 0), (2, 1), (2, 2), (3, 0)]
        );
        // a merged block carries its own producer, DAA score, weight (D-084) and color; a main-chain block itself
        // always gets the reward
        assert_eq!(all[1].hash, [20; 32]);
        assert_eq!(all[1].producer, [120; 32]);
        assert_eq!(all[1].daa_score, 20);
        assert_eq!(all[1].weight, 1);
        assert!(all[1].rewarded);
        assert_eq!(all[2].weight, 12);
        assert!(!all[2].rewarded);
        assert_eq!(all[3].hash, [2; 32]);
        assert!(all[3].rewarded);
        assert_eq!(all[4].weight, 12, "the main-chain block's own weight");
        assert!(all[4].rewarded);
        assert_eq!(
            positions(&segment(&main, Some((2, 0)), 100)),
            vec![(2, 1), (2, 2), (3, 0)]
        );
        assert_eq!(positions(&segment(&main, Some((2, 2)), 100)), vec![(3, 0)]);
        assert!(segment(&main, Some((3, 0)), 100).is_empty());
        assert_eq!(positions(&segment(&main, None, 2)), vec![(1, 0), (2, 0)]);
        assert!(segment(&chain(entries[..1].to_vec()), None, 100).is_empty());

        assert_eq!(unlock_target(&all, 0), None);
        assert_eq!(unlock_target(&all, 1), Some((1, 0)));
        assert_eq!(unlock_target(&all, 2), Some((2, 2)));
        assert_eq!(unlock_target(&all, 9), Some((3, 0)));
    }

    /// `settled_height` is the largest `h` with `prefix_height(h) ≤ anchor height`: the settled prefix is monotonic
    /// in height, and nothing after a block without data counts (D-038, D-074); `accepted_height` gives the
    /// acceptance height for main-chain blocks and merged blocks alike.
    #[test]
    fn settled_height_is_the_last_height_with_a_settled_prefix() {
        let entries = vec![
            entry(0, false, vec![]),
            // 1: the settlement confirms at BTC height 10
            entry(1, true, vec![]),
            // 2: the header appears at 11, the header of the block it merges appears at 12
            entry(2, false, vec![summary(20, false)]),
            // 3: the header appears at 13
            entry(3, false, vec![]),
            // 4: no data
            entry(4, false, vec![]),
            // 5: the settlement confirms at 11, but 4 has no data, so nothing from it on is in the settled prefix
            entry(5, true, vec![]),
        ];
        let main = chain(entries.clone());
        let mut snapshot = BtcSnapshot::default();
        snapshot.settle([1; 32], [0xa1; 32], 10);
        snapshot.see_header([2; 32], 11);
        snapshot.see_header([20; 32], 12);
        snapshot.see_header([3; 32], 13);
        snapshot.settle([5; 32], [0xa5; 32], 11);
        for (anchor, expected) in [(9, 0), (10, 1), (11, 1), (12, 2), (13, 3), (1_000, 3)] {
            let settled = settled_height(&snapshot, &main, anchor).unwrap();
            assert_eq!(settled, expected, "anchor height {anchor}");
            for height in 1..=5u64 {
                let in_prefix =
                    prefix_height(&snapshot, &main, height).is_some_and(|p| p <= anchor);
                assert_eq!(
                    in_prefix,
                    height <= settled,
                    "anchor height {anchor}, height {height}"
                );
            }
        }
        assert_eq!(accepted_height(&main, &[20; 32]), Some(2));
        assert_eq!(accepted_height(&main, &[5; 32]), Some(5));
        assert_eq!(accepted_height(&main, &[9; 32]), None);
        assert_eq!(
            settled_height(&snapshot, &chain(entries[..1].to_vec()), 1_000),
            Ok(0)
        );
    }

    /// The carrier (D-095): the sender of the payload that first landed the block's data: a settlement payload goes to
    /// the producer of the block it belongs to (that block must be accepted by the main chain), a republish payload to
    /// the payee account, but only once the block's data has waited more than `ANCHOR_DEPTH + CARRIER_REPOST_DELAY` BTC
    /// blocks after the anchor; an all-zero payee account, a block not accepted by the main chain and a block without
    /// data have no carrier; within the same height, the order in the BTC block decides; only landings within the
    /// anchor height count.
    #[test]
    fn carrier_is_the_sender_of_the_first_landing() {
        let params = Params {
            anchor_depth: 2,
            carrier_repost_delay: 2,
            ..DEVNET
        };
        let mut e1 = entry(1, true, vec![]);
        e1.anchor_height = 10;
        let mut e2 = entry(2, false, vec![summary(20, false), summary(21, true)]);
        e2.anchor_height = 10;
        let mut e3 = entry(3, false, vec![]);
        e3.anchor_height = 12;
        let mut entries = vec![entry(0, false, vec![]), e1, e2, e3];
        let main = chain(entries.clone());
        let mut snapshot = BtcSnapshot::default();
        // the body of 1 is landed by the settlement payload of block 3 (height 12); an earlier but invalid settlement
        // does not count; the producer of block 3 is [103; 32]
        snapshot.settle_from([1; 32], [0xa1; 32], 13, 0, Sender::Payee([0x55; 32]));
        snapshot.settle_from([1; 32], [0xb1; 32], 12, 2, Sender::Block([3; 32]));
        // the header of 2 first appears in transaction 1 at height 11 (a republish, payee account [0x66; 32], waited 1
        // block: not enough); transaction 0 is the settlement of block 9 (the main chain did not accept block 9)
        snapshot.see_header_from([2; 32], 11, 1, Sender::Payee([0x66; 32]));
        // the header of 20: a republish that waited 5 blocks (15 − 10 > 2 + 2)
        snapshot.see_header_from([20; 32], 15, 0, Sender::Payee([0x66; 32]));
        // the body of 21: two transactions at height 15; transaction 0 is the settlement of block 9, which the main
        // chain did not accept, and transaction 1 is a republish
        snapshot.settle_from([21; 32], [0xc1; 32], 15, 1, Sender::Payee([0x77; 32]));
        snapshot.settle_from([21; 32], [0xc0; 32], 15, 0, Sender::Block([9; 32]));
        // the header of 3: its own settlement
        snapshot.see_header_from([3; 32], 12, 2, Sender::Block([3; 32]));
        let carrier = |limit: Option<u32>, block: u8| {
            carrier_of(&params, &snapshot, &main, limit, &[block; 32]).unwrap()
        };
        assert_eq!(
            carrier(None, 1),
            Some([103; 32]),
            "a settlement payload goes to the producer of the block it belongs to"
        );
        assert_eq!(carrier(Some(12), 1), Some([103; 32]));
        assert_eq!(
            carrier(Some(11), 1),
            None,
            "not landed within the anchor height yet"
        );
        assert_eq!(
            carrier(None, 2),
            None,
            "the republish did not wait long enough"
        );
        assert_eq!(
            carrier(None, 20),
            Some([0x66; 32]),
            "the republish waited long enough"
        );
        assert_eq!(carrier(Some(14), 20), None);
        assert_eq!(
            carrier(None, 21),
            None,
            "the earlier transaction at the same height counts, and its block was not accepted by the main chain"
        );
        assert_eq!(
            carrier(None, 3),
            Some([103; 32]),
            "carrying itself: goes to itself"
        );
        assert_eq!(
            carrier(None, 9),
            None,
            "a block the main chain did not accept"
        );
        assert_eq!(carrier(None, 0), None, "the genesis block has no data");
        // the wait boundary: exactly ANCHOR_DEPTH + CARRIER_REPOST_DELAY does not count, one more does; an all-zero
        // payee account does not count
        let mut e4 = entry(4, false, vec![]);
        e4.anchor_height = 20;
        let mut e5 = entry(5, false, vec![]);
        e5.anchor_height = 20;
        let mut e6 = entry(6, false, vec![]);
        e6.anchor_height = 20;
        entries.extend([e4, e5, e6]);
        let main = chain(entries);
        snapshot.see_header_from([4; 32], 24, 0, Sender::Payee([0x66; 32]));
        snapshot.see_header_from([5; 32], 25, 0, Sender::Payee([0x66; 32]));
        snapshot.see_header_from([6; 32], 25, 0, Sender::Payee([0; 32]));
        assert_eq!(
            carrier_of(&params, &snapshot, &main, None, &[4; 32]),
            Ok(None)
        );
        assert_eq!(
            carrier_of(&params, &snapshot, &main, None, &[5; 32]),
            Ok(Some([0x66; 32]))
        );
        assert_eq!(
            carrier_of(&params, &snapshot, &main, None, &[6; 32]),
            Ok(None)
        );
        assert_eq!(
            accepted_summary(&main, &[21; 32]).map(|b| b.map(|b| b.producer)),
            Ok(Some([121; 32]))
        );
        assert_eq!(accepted_summary(&main, &[9; 32]), Ok(None));
    }

    /// A main-chain history answering from a full list of entries (index = height).
    struct FullHistory(Vec<MainEntry>);

    impl MainHistory for FullHistory {
        fn accepted(&self, hash: &[u8; 32]) -> Result<Option<(u64, BlockSummary)>, String> {
            Ok(self.0.iter().enumerate().find_map(|(height, entry)| {
                entry
                    .accepted()
                    .find(|b| b.hash == *hash)
                    .map(|b| (u64::try_from(height).unwrap(), b))
            }))
        }
    }

    /// A pruned main-chain window (D-097) answers the settled prefix, the settled height, the settlement backlog, the
    /// unlockable segment and the accepted blocks exactly like the full chain wherever the question stays at or above
    /// the window, and refuses (`Err`, or no prefix) below it; pruning past a block without data is refused.
    #[test]
    fn a_pruned_main_chain_window_answers_like_the_full_chain() {
        let params = Params {
            settlement_timeout: 2,
            ..DEVNET
        };
        // heights 1..=8: block i (odd ones need a body) is anchored at 8 + i and its data lands at 10 + i; every third
        // block merges another block, landed one BTC block later; block 7 needs a body that never lands
        let mut entries = vec![entry(0, false, vec![])];
        let mut snapshot = BtcSnapshot::default();
        let land = |snapshot: &mut BtcSnapshot, block: &BlockSummary, height: u32| {
            if block.settles {
                snapshot.settle(block.hash, [block.hash[0] ^ 0xff; 32], height);
            } else {
                snapshot.see_header(block.hash, height);
            }
        };
        for i in 1..=8u8 {
            let merged = if i % 3 == 0 {
                vec![summary(100 + i, i % 2 == 0)]
            } else {
                vec![]
            };
            let mut e = entry(i, i % 2 == 1, merged);
            e.anchor_height = 8 + u32::from(i);
            for m in &e.merged {
                land(&mut snapshot, m, 11 + u32::from(i));
            }
            if i != 7 {
                land(&mut snapshot, &e.summary(), 10 + u32::from(i));
            }
            entries.push(e);
        }
        let full = chain(entries.clone());
        let history: Arc<dyn MainHistory> = Arc::new(FullHistory(entries.clone()));
        assert!(MainChain::from_window(3, 0, vec![]).is_err());
        assert!(full.clone().prune(9, &snapshot).is_err(), "above the tip");
        assert!(
            full.clone().prune(8, &snapshot).is_err(),
            "block 7 has no data"
        );
        for base in 0..=7u64 {
            let mut window = full.clone();
            window.prune(base, &snapshot).unwrap();
            let window = window.with_history(Arc::clone(&history));
            assert_eq!(window.base(), base);
            assert_eq!(window.tip_height(), 8);
            assert_eq!(window.tip(), full.tip());
            let base_prefix = window.base_prefix();
            let expected = match base {
                0 | 1 => 0,
                _ => prefix_height(&snapshot, &full, base - 1).unwrap(),
            };
            assert_eq!(base_prefix, expected, "base {base}");
            for height in 0..=8u64 {
                let got = prefix_height(&snapshot, &window, height);
                if height >= base {
                    assert_eq!(got, prefix_height(&snapshot, &full, height));
                } else {
                    assert_eq!(got, None);
                }
            }
            for anchor in 0..25u32 {
                let settled = settled_height(&snapshot, &window, anchor);
                let backlog = settlement_backlog(&params, &snapshot, &window, &[], anchor);
                if anchor >= base_prefix {
                    assert_eq!(settled, settled_height(&snapshot, &full, anchor));
                    assert_eq!(
                        backlog,
                        settlement_backlog(&params, &snapshot, &full, &[], anchor)
                    );
                } else {
                    assert!(
                        settled.is_err() && backlog.is_err(),
                        "base {base} anchor {anchor}"
                    );
                }
            }
            for height in 1..=8u64 {
                for index in 0..2u32 {
                    let cursor = Some((height, index));
                    let got = unlock_segment(&window, cursor, 100);
                    if height >= base {
                        assert_eq!(got, unlock_segment(&full, cursor, 100));
                    } else {
                        assert!(got.is_err());
                    }
                }
            }
            assert_eq!(unlock_segment(&window, None, 100).is_ok(), base <= 1);
            for seed in [1u8, 3, 103, 6, 106, 8, 42] {
                let hash = [seed; 32];
                assert_eq!(
                    accepted_summary(&window, &hash),
                    accepted_summary(&full, &hash),
                    "block {seed} with the history"
                );
                let below = accepted_height(&full, &hash).is_some_and(|h| h < base);
                let without = window.clone();
                let without = MainChain {
                    history: None,
                    ..without
                };
                assert_eq!(
                    accepted_summary(&without, &hash).unwrap().is_none(),
                    below || accepted_height(&full, &hash).is_none(),
                    "block {seed} without the history"
                );
                assert_eq!(
                    accepted_height(&window, &hash),
                    accepted_height(&full, &hash).filter(|h| *h >= base)
                );
            }
            let mut truncated = window.clone();
            if base > 0 {
                assert!(truncated.truncate(base - 1).is_err());
            }
            truncated.truncate(base).unwrap();
            assert_eq!(truncated.tip_height(), base);
            assert_eq!(truncated.entries().len(), 1);
        }
    }

    /// Pruning the snapshot (D-097) drops the BTC blocks below the cut and the records of the fast-lane blocks not kept
    /// that counted or landed below it, keeps everything at or above the cut and whatever `keep` protects, and the record
    /// of a snapshot restores the same snapshot.
    #[test]
    fn a_pruned_snapshot_keeps_the_cut_and_up_and_its_record_round_trips() {
        let mut snapshot = BtcSnapshot::testing(10);
        for height in 10..=30u32 {
            snapshot.push_block([u8::try_from(height).unwrap(); 32], 1_790_000_000 + height);
        }
        // fast-lane block b lands at BTC height b (headers of even blocks, bodies of odd ones)
        for b in 11..=29u8 {
            if b % 2 == 0 {
                snapshot.see_header([b; 32], u32::from(b));
            } else {
                snapshot.settle([b; 32], [b ^ 0xff; 32], u32::from(b));
            }
        }
        let record = snapshot.record();
        let restored =
            BtcSnapshot::from_record(borsh::from_slice(&borsh::to_vec(&record).unwrap()).unwrap())
                .unwrap();
        assert_eq!(restored.record(), record);

        let mut pruned = snapshot.clone();
        // block 13 is protected (e.g. accepted within the main chain's window)
        let dropped = pruned.prune(20, &|hash| hash[0] == 13);
        assert_eq!(pruned.base(), 20);
        assert_eq!(pruned.tip(), snapshot.tip());
        assert_eq!(pruned.hash_at(19), None);
        assert_eq!(pruned.hash_at(20), snapshot.hash_at(20));
        for b in 11..=29u8 {
            let kept = b >= 20 || b == 13;
            assert_eq!(
                pruned.block_data_height(&[b; 32]),
                kept.then_some(u32::from(b)),
                "block {b}"
            );
            assert_eq!(pruned.counted_at(&[b; 32]).is_some(), kept, "block {b}");
        }
        assert!(pruned.headers_at(18).is_empty());
        assert_eq!(pruned.headers_at(20), snapshot.headers_at(20));
        // below the cut and not protected: the counted headers of 11, 12 and 14 to 19 and the bodies of 11, 15, 17 and
        // 19
        assert_eq!(dropped, 12);
        // a cut not above the base, or above the tip, changes nothing beyond the tip
        let mut again = pruned.clone();
        assert_eq!(again.prune(15, &|_| false), 0);
        assert_eq!(again.record(), pruned.record());
        let mut beyond = pruned.clone();
        beyond.prune(1_000, &|_| false);
        assert_eq!(beyond.base(), 30);
        assert_eq!(beyond.tip(), snapshot.tip());
        let record = pruned.record();
        assert_eq!(
            BtcSnapshot::from_record(record.clone()).unwrap().record(),
            record
        );
    }
}

/// Counting headers and landing bodies on the mock BTC chain (D-135): a header counts once its parents have, in any
/// order within a BTC block, a body lands only with its counted header, and nothing is held for what does not count.
#[cfg(test)]
#[cfg(feature = "mock")]
mod counting_tests {
    use std::slice;

    use bitcoin::key::{Keypair, Secp256k1};
    use bitcoin::secp256k1::SecretKey;
    use bitcoin::{
        absolute, transaction, Amount, ScriptBuf, Sequence, Transaction, TxIn, TxOut, WPubkeyHash,
        Witness,
    };
    use ksana_primitives::{compact, Body, EntryData, DEVNET};
    use proptest::prelude::*;

    use super::*;
    use crate::bodies::Bodies;
    use crate::envelope::envelope_script;
    use crate::mock::MockBtc;

    /// The payee account of the republish payloads.
    const PAYEE: [u8; 32] = [0x66; 32];

    /// The headers and the bodies (by header index) one payload carries.
    type Carried = (Vec<BlockHeader>, Vec<(usize, Vec<EntryData>)>);

    /// Accepts every header but those in `rejected`, and records each header it is asked about.
    #[derive(Default)]
    struct Accepts {
        rejected: BTreeSet<[u8; 32]>,
        asked: Mutex<Vec<[u8; 32]>>,
    }

    impl Accepts {
        fn asked(&self) -> Vec<[u8; 32]> {
            self.asked.lock().unwrap().clone()
        }
    }

    impl KnownBlocks for Accepts {
        fn count(&self, _: &BtcSnapshot, header: &BlockHeader) -> Result<bool, TrackerError> {
            let hash = header.hash();
            self.asked.lock().unwrap().push(hash);
            Ok(!self.rejected.contains(&hash))
        }
    }

    /// The body of block `i` in a payload: one unlock entry.
    fn unlock(i: u64) -> Vec<EntryData> {
        vec![EntryData::Unlock {
            height: i + 1,
            index: 0,
        }]
    }

    /// The block entries of a body without forced-inclusion entries.
    fn entries(body: &[EntryData]) -> Vec<Entry> {
        crate::bodies::convert(body, &[]).unwrap()
    }

    /// A transaction spending `inputs` with the KSN1 envelope carrying `payload` in the first input's witness
    /// (signatures and control blocks are placeholders: the mock chain checks neither).
    fn carrier(inputs: &[bitcoin::OutPoint], payload: &Payload) -> Transaction {
        let key =
            Keypair::from_secret_key(&Secp256k1::new(), &SecretKey::from_slice(&[7; 32]).unwrap())
                .x_only_public_key()
                .0;
        let leaf = envelope_script(&key, &DEVNET.magic, &payload.encode(&DEVNET).unwrap()).unwrap();
        Transaction {
            version: transaction::Version::TWO,
            lock_time: absolute::LockTime::ZERO,
            input: inputs
                .iter()
                .map(|previous_output| TxIn {
                    previous_output: *previous_output,
                    script_sig: ScriptBuf::new(),
                    sequence: Sequence::MAX,
                    witness: Witness::from_slice(&[vec![1; 64], leaf.to_bytes(), vec![0xc0; 33]]),
                })
                .collect(),
            output: vec![TxOut {
                value: Amount::from_sat(1_000),
                script_pubkey: ScriptBuf::new_p2wpkh(&WPubkeyHash::from_byte_array([9; 20])),
            }],
        }
    }

    fn txid(tx: &Transaction) -> [u8; 32] {
        tx.compute_txid().to_byte_array()
    }

    /// A mock BTC chain: block 1 funds `coins` outputs, blocks 2 and 3 are empty (anchors for the headers), and the
    /// genesis block of a fast-lane chain anchors at its block 0.
    struct Btc {
        btc: Arc<MockBtc>,
        coins: Vec<bitcoin::OutPoint>,
        genesis: BlockHeader,
    }

    impl Btc {
        fn new(coins: usize) -> Self {
            let btc = Arc::new(MockBtc::new());
            let funding = ScriptBuf::new_p2wpkh(&WPubkeyHash::from_byte_array([8; 20]));
            let coins = btc.fund(&funding, &vec![50_000; coins]);
            btc.mine(2);
            let mut chain = Btc {
                btc,
                coins,
                genesis: BlockHeader {
                    version: 1,
                    height: 0,
                    parents: Vec::new(),
                    btc_anchor: BtcAnchor {
                        height: 0,
                        hash: [0; 32],
                    },
                    entries_root: compact::empty_entries_root(),
                    state_root: [0x44; 32],
                    producer: [0; 32],
                    timestamp: 1_700_000_000,
                    bits: 0x207f_ffff,
                    nonce: 0,
                },
            };
            chain.genesis.btc_anchor = chain.anchor(0);
            chain
        }

        /// The BTC block at `height` on the best chain.
        fn anchor(&self, height: u32) -> BtcAnchor {
            BtcAnchor {
                height,
                hash: self.btc.block_hash(height).unwrap().unwrap(),
            }
        }

        fn tracker(&self) -> BtcTracker {
            BtcTracker::new(self.btc.clone(), DEVNET, &self.genesis).unwrap()
        }

        /// A transaction carrying `payload`, spending a coin of its own.
        fn carry(&mut self, payload: &Payload) -> Transaction {
            let coin = self.coins.pop().unwrap();
            carrier(&[coin], payload)
        }

        /// Mines a block holding a transaction for each payload, in order; returns them.
        fn mine(&mut self, payloads: &[Payload]) -> Vec<Transaction> {
            let txs: Vec<Transaction> = payloads.iter().map(|p| self.carry(p)).collect();
            self.btc.mine_block(txs.clone()).unwrap();
            txs
        }
    }

    /// A header of a block on `parents`, anchored at `anchor`, with the body `body` (the empty entries root without
    /// one: the block needs no body).
    fn header(
        parents: &[&BlockHeader],
        anchor: &BtcAnchor,
        tag: u64,
        body: Option<&[EntryData]>,
    ) -> BlockHeader {
        BlockHeader {
            version: 1,
            height: parents.iter().map(|p| p.height).max().unwrap_or(0) + 1,
            parents: parents.iter().map(|p| p.hash()).collect(),
            btc_anchor: anchor.clone(),
            entries_root: match body {
                Some(body) => entries_root(&entries(body)).unwrap(),
                None => compact::empty_entries_root(),
            },
            state_root: [0x44; 32],
            producer: [0x55; 32],
            timestamp: 1_700_000_000 + tag,
            bits: 0x207f_ffff,
            nonce: tag,
        }
    }

    /// `n` headers in a chain on `first`, each with the body `unlock(tag)`.
    fn chain_on(first: &BlockHeader, anchor: &BtcAnchor, n: u64) -> Vec<BlockHeader> {
        let mut out: Vec<BlockHeader> = Vec::new();
        for tag in 0..n {
            let parent = out.last().unwrap_or(first).clone();
            out.push(header(&[&parent], anchor, tag, Some(&unlock(tag))));
        }
        out
    }

    /// A republish payload carrying `headers`, with the bodies of those at the positions in `bodies` (ascending).
    fn republish(headers: &[BlockHeader], bodies: &[(usize, Vec<EntryData>)]) -> Payload {
        Payload::Headers {
            headers: headers.to_vec(),
            bodies: bodies
                .iter()
                .map(|(at, entries)| Body {
                    header: u32::try_from(*at).unwrap(),
                    entries: entries.clone(),
                })
                .collect(),
            payee: PAYEE,
        }
    }

    /// Each header of `headers` with its body `unlock(position)`.
    fn with_bodies(headers: &[BlockHeader]) -> Payload {
        let bodies: Vec<(usize, Vec<EntryData>)> = (0..headers.len())
            .map(|i| (i, unlock(headers[i].nonce)))
            .collect();
        republish(headers, &bodies)
    }

    /// The second internal audit's A2-10 (D-135): a payload carrying a full republish of headers whose parent never
    /// counted, each with a body, adds nothing: no header counts, none is even checked, and the snapshot and its record
    /// hold nothing of them. Carried once their parents count, they count in full, their bodies land, and a
    /// checkpoint shares one copy of the transaction among the bodies it landed.
    #[test]
    fn headers_whose_parents_did_not_count_leave_no_trace() {
        let mut btc = Btc::new(4);
        let n = u64::try_from(DEVNET.max_settlement_headers).unwrap();
        let anchor = btc.anchor(2);
        let stray = header(&[], &anchor, 9_999, None);
        let stray = BlockHeader {
            parents: vec![[0x11; 32]],
            ..stray
        };
        let orphans = chain_on(&stray, &anchor, n);
        btc.mine(&[with_bodies(&orphans)]);
        let tracker = btc.tracker();
        let empty = tracker.snapshot().unwrap().record();
        let known = Accepts::default();
        assert!(tracker.sync(&known).unwrap());
        let snap = tracker.snapshot().unwrap();
        assert!(known.asked().is_empty());
        assert!(orphans.iter().all(|h| snap.counted(&h.hash()).is_none()));
        assert!(snap.bodies.is_empty() && snap.counted.is_empty());
        assert!(snap.headers_at(4).is_empty());
        let record = snap.record();
        assert!(record.txs.is_empty());
        assert_eq!(
            borsh::to_vec(&record).unwrap().len(),
            borsh::to_vec(&empty).unwrap().len() + 5 * (32 + 80),
            "only the BTC blocks 0 to 4"
        );

        // the same number of headers on the genesis block counts in full, its bodies land, the bodies share one copy
        // of their transaction, also in a checkpoint
        let honest = chain_on(&btc.genesis, &anchor, n);
        let txs = btc.mine(&[with_bodies(&honest)]);
        assert!(tracker.sync(&known).unwrap());
        let snap = tracker.snapshot().unwrap();
        assert_eq!(
            snap.headers_at(5),
            honest.iter().map(BlockHeader::hash).collect::<Vec<_>>()
        );
        let landed = snap.landed_body(&honest[0].hash()).unwrap();
        assert_eq!(
            (landed.landing.txid, landed.landing.height),
            (txid(&txs[0]), 5)
        );
        assert_eq!(Arc::strong_count(&landed.tx), honest.len());
        let record = snap.record();
        assert_eq!(record.txs.len(), 1);
        let restored = BtcSnapshot::from_record(record.clone()).unwrap();
        assert_eq!(restored.record(), record);
        let last = restored
            .landed_body(&honest[honest.len() - 1].hash())
            .unwrap();
        assert_eq!(Arc::strong_count(&last.tx), honest.len());
        // the bodies decode again from the transaction
        let mut bodies = Bodies::new(DEVNET, btc.genesis.hash());
        for (tag, h) in (0u64..).zip(&honest) {
            let body = bodies.body(&restored, &h.hash()).unwrap().unwrap();
            assert_eq!(*body, entries(&unlock(tag)));
        }
        assert_eq!(
            *bodies
                .body(&restored, &btc.genesis.hash())
                .unwrap()
                .unwrap(),
            Vec::<Entry>::new()
        );
        assert_eq!(bodies.body(&restored, &stray.hash()).unwrap(), None);
    }

    /// Within one BTC block the headers count whatever the order (D-135): children in an earlier transaction than their
    /// parents, or before them in a payload, count at that height, each after its parents and landing at its earliest
    /// appearance. A header carried in a BTC block below the one its parent counts in does not count, nor does its
    /// body land, until a payload carries them again.
    #[test]
    fn headers_count_in_any_order_within_a_block_and_only_after_their_parents() {
        let mut btc = Btc::new(8);
        let anchor = btc.anchor(2);
        let g = btc.genesis.clone();
        let a1 = header(&[&g], &anchor, 1, Some(&unlock(1)));
        let a2 = header(&[&a1], &anchor, 2, None);
        let b1 = header(&[&g], &anchor, 3, Some(&unlock(3)));
        let m3 = header(&[&a2, &b1], &anchor, 4, Some(&unlock(4)));
        // BTC block 4: m3 and a2 first, then a1 and b1 in a second transaction, the children before their parents
        let txs = btc.mine(&[
            republish(&[m3.clone(), a2.clone()], &[(0, unlock(4))]),
            republish(&[b1.clone(), m3.clone(), a1.clone()], &[(2, unlock(1))]),
        ]);
        let tracker = btc.tracker();
        let known = Accepts::default();
        assert!(tracker.sync(&known).unwrap());
        let snap = tracker.snapshot().unwrap();
        let order = snap.headers_at(4).to_vec();
        assert_eq!(order.len(), 4);
        let at = |h: &BlockHeader| order.iter().position(|x| *x == h.hash()).unwrap();
        assert!(at(&a1) < at(&a2) && at(&a2) < at(&m3) && at(&b1) < at(&m3));
        let landing = |h: &BlockHeader| snap.counted(&h.hash()).unwrap().landing;
        assert_eq!((landing(&m3).txid, landing(&m3).index), (txid(&txs[0]), 1));
        assert_eq!(landing(&a1).txid, txid(&txs[1]));
        assert!(snap.counted(&a1.hash()).unwrap().settles);
        assert!(!snap.counted(&a2.hash()).unwrap().settles);
        // a2 needs no body: its data lands with its header; m3's and a1's bodies landed, b1's did not
        assert_eq!(snap.data_landing(&a2.hash()), Some(landing(&a2)));
        assert_eq!(snap.settlement_of(&m3.hash()), Some((txid(&txs[0]), 4)));
        assert_eq!(snap.settlement_of(&a1.hash()), Some((txid(&txs[1]), 4)));
        assert_eq!(snap.block_data_height(&b1.hash()), None);
        assert_eq!(known.asked().len(), 4);

        // c2 on a2 and its child c3 carried at 5, c2's parent's sibling d1 only at 6: c4 on c3 and d1 counts at 6 only
        // when carried there again; its body carried at 5 does not land
        let c2 = header(&[&a2], &anchor, 5, None);
        let d1 = header(&[&g], &anchor, 6, None);
        let c4 = header(&[&c2, &d1], &anchor, 7, Some(&unlock(7)));
        btc.mine(&[republish(&[c2.clone(), c4.clone()], &[(1, unlock(7))])]);
        btc.mine(&[republish(slice::from_ref(&d1), &[])]);
        assert!(tracker.sync(&known).unwrap());
        let snap = tracker.snapshot().unwrap();
        assert_eq!(snap.counted_at(&c2.hash()), Some(5));
        assert_eq!(snap.counted_at(&d1.hash()), Some(6));
        assert_eq!(snap.counted_at(&c4.hash()), None);
        let txs = btc.mine(&[republish(slice::from_ref(&c4), &[(0, unlock(7))])]);
        assert!(tracker.sync(&known).unwrap());
        let snap = tracker.snapshot().unwrap();
        assert_eq!(snap.counted_at(&c4.hash()), Some(7));
        assert_eq!(snap.settlement_of(&c4.hash()), Some((txid(&txs[0]), 7)));
        // a BTC reorg of block 7 takes c4 back; the same transactions mined again at 7 count it again
        btc.btc.reorg(1);
        tracker.sync(&known).unwrap();
        assert_eq!(tracker.snapshot().unwrap().counted_at(&c4.hash()), None);
        btc.btc.mine(1);
        assert!(tracker.sync(&known).unwrap());
        let snap = tracker.snapshot().unwrap();
        assert_eq!(snap.counted_at(&c4.hash()), Some(7));
        assert_eq!(snap.settlement_of(&c4.hash()), Some((txid(&txs[0]), 7)));
    }

    /// A header the header DAG refuses does not count, and neither does any header on it, which is never even checked;
    /// nor does a header anchored off the best chain (D-135).
    #[test]
    fn a_refused_or_unanchored_header_and_its_descendants_do_not_count() {
        let mut btc = Btc::new(4);
        let anchor = btc.anchor(2);
        let g = btc.genesis.clone();
        let a1 = header(&[&g], &anchor, 1, None);
        let bad = header(&[&a1], &anchor, 2, None);
        let child = header(&[&bad], &anchor, 3, None);
        let off = header(
            &[&a1],
            &BtcAnchor {
                height: 2,
                hash: [0xab; 32],
            },
            4,
            None,
        );
        let above = header(&[&off], &anchor, 5, None);
        btc.mine(&[republish(
            &[
                child.clone(),
                bad.clone(),
                a1.clone(),
                off.clone(),
                above.clone(),
            ],
            &[],
        )]);
        let tracker = btc.tracker();
        let known = Accepts {
            rejected: BTreeSet::from([bad.hash()]),
            ..Accepts::default()
        };
        assert!(tracker.sync(&known).unwrap());
        let snap = tracker.snapshot().unwrap();
        assert_eq!(snap.headers_at(4), &[a1.hash()]);
        assert_eq!(known.asked(), vec![a1.hash(), bad.hash()]);
    }

    /// A body lands only when it matches its counted header's `entries_root`; only its first landing is kept, the
    /// earliest transaction of the earliest BTC block; a body repeated in a payload lands once (D-135).
    #[test]
    fn a_body_lands_once_and_only_when_it_matches() {
        let mut btc = Btc::new(8);
        let anchor = btc.anchor(2);
        let g = btc.genesis.clone();
        let a1 = header(&[&g], &anchor, 1, Some(&unlock(1)));
        // BTC block 4: a junk body, then the body twice in one payload, then the body again
        let txs = btc.mine(&[
            republish(slice::from_ref(&a1), &[(0, unlock(9))]),
            republish(&[a1.clone(), a1.clone()], &[(0, unlock(1)), (1, unlock(1))]),
            republish(slice::from_ref(&a1), &[(0, unlock(1))]),
        ]);
        btc.mine(&[republish(slice::from_ref(&a1), &[(0, unlock(1))])]);
        let tracker = btc.tracker();
        assert!(tracker.sync(&Accepts::default()).unwrap());
        let snap = tracker.snapshot().unwrap();
        assert_eq!(
            snap.counted(&a1.hash()).unwrap().landing.txid,
            txid(&txs[0])
        );
        let landed = snap.landed_body(&a1.hash()).unwrap();
        assert_eq!(
            (
                landed.landing.txid,
                landed.landing.height,
                landed.landing.index,
                landed.slot
            ),
            (txid(&txs[1]), 4, 2, 0)
        );
        assert_eq!(landed.landing.sender, Sender::Payee(PAYEE));
        assert_eq!(snap.block_data_height(&a1.hash()), Some(4));
    }

    /// A republish payload with more headers than `MAX_SETTLEMENT_HEADERS` does not decode (D-100): nothing of it
    /// counts, while the payload next to it is scanned as usual.
    #[test]
    fn a_payload_over_the_header_limit_is_not_scanned() {
        let mut btc = Btc::new(4);
        let anchor = btc.anchor(2);
        let n = u64::try_from(DEVNET.max_settlement_headers).unwrap() + 1;
        let over = chain_on(&btc.genesis, &anchor, n);
        let a1 = header(&[&btc.genesis], &anchor, 7_777, None);
        btc.mine(&[with_bodies(&over), republish(slice::from_ref(&a1), &[])]);
        let tracker = btc.tracker();
        assert!(tracker.sync(&Accepts::default()).unwrap());
        let snap = tracker.snapshot().unwrap();
        assert_eq!(snap.headers_at(4), &[a1.hash()]);
        assert!(snap.bodies.is_empty());
    }

    /// D-100: a payload carrying a header anchored at or above the BTC height it is confirmed at does not count as a
    /// whole: none of its headers counts, not even those anchored below, and none of its bodies lands; the same payload
    /// one BTC block higher counts, its headers counting as far as their anchors are on the chain.
    #[test]
    fn a_payload_with_a_header_anchored_at_its_own_height_is_not_scanned() {
        let mut btc = Btc::new(4);
        let low = btc.anchor(2);
        let mut headers = chain_on(&btc.genesis, &low, 3);
        // the last header anchors at BTC height 4, where the payload confirms (no hash can be that block's own)
        let own = BtcAnchor {
            height: 4,
            hash: [0xab; 32],
        };
        headers[2] = header(&[&headers[1]], &own, 2, Some(&unlock(2)));
        assert!(!payload_counts(&headers, 4));
        assert!(payload_counts(&headers, 5));
        let payload = with_bodies(&headers);
        let tracker = btc.tracker();
        let known = Accepts::default();
        btc.mine(slice::from_ref(&payload));
        assert!(tracker.sync(&known).unwrap());
        let snap = tracker.snapshot().unwrap();
        assert!(snap.counted.is_empty() && snap.bodies.is_empty());
        assert!(known.asked().is_empty());
        btc.mine(&[payload]);
        assert!(tracker.sync(&known).unwrap());
        let snap = tracker.snapshot().unwrap();
        for h in &headers[..2] {
            assert_eq!(snap.counted_at(&h.hash()), Some(5));
            assert_eq!(snap.block_data_height(&h.hash()), Some(5));
        }
        assert_eq!(snap.counted_at(&headers[2].hash()), None);
        assert_eq!(known.asked(), vec![headers[0].hash(), headers[1].hash()]);
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(48))]

        /// Counting depends only on the current BTC chain (D-135): whatever the order in which the headers and bodies
        /// are carried, how they are split into transactions and how BTC reorgs on the way (also below an anchor, and
        /// confirming the orphaned transactions again higher), a tracker that followed every step holds exactly what
        /// a fresh tracker scanning the final chain once holds. Every header counted has its parents counted at or
        /// below its height and was accepted; every body landed matches its header and lands at or above it.
        #[test]
        fn counting_depends_only_on_the_current_chain(
            shape in proptest::collection::vec((0usize..12, 0usize..12, any::<bool>(), 0u8..2, 0u8..8), 12),
            plan in proptest::collection::vec(
                proptest::collection::vec((0usize..12, any::<bool>(), 0u8..6), 1..7),
                2..9,
            ),
            splits in proptest::collection::vec((1usize..4, any::<bool>()), 9),
            reorgs in proptest::collection::vec((0u8..5, 1u32..4, any::<bool>()), 9),
        ) {
            let mut btc = Btc::new(64);
            let anchors = [btc.anchor(2), btc.anchor(3)];
            let g = btc.genesis.clone();
            // the DAG: each header on one or two earlier headers or the genesis block, anchored at BTC block 2 or 3,
            // with a body or not; a few are refused
            let mut headers: Vec<BlockHeader> = Vec::new();
            let mut rejected = BTreeSet::new();
            for (i, (first, second, merge, anchor, refuse)) in shape.iter().enumerate() {
                let pick = |at: usize| if at % (i + 1) == i { &g } else { &headers[at % (i + 1)] };
                let mut parents = vec![pick(*first).clone()];
                let other = pick(*second).clone();
                if *merge && other.hash() != parents[0].hash() {
                    parents.push(other);
                }
                let parents: Vec<&BlockHeader> = parents.iter().collect();
                let tag = u64::try_from(i).unwrap();
                let body = (i % 3 != 0).then(|| unlock(tag));
                let h = header(&parents, &anchors[usize::from(*anchor)], tag, body.as_deref());
                if *refuse == 0 {
                    rejected.insert(h.hash());
                }
                headers.push(h);
            }
            let known = Accepts {
                rejected,
                ..Accepts::default()
            };
            let tracker = btc.tracker();
            prop_assert!(tracker.sync(&known).is_ok());
            for (step, block) in plan.iter().enumerate() {
                // the payloads of this BTC block
                let (parts, reversed) = splits[step];
                let mut payloads: Vec<Carried> = vec![(Vec::new(), Vec::new()); parts];
                for (k, (index, with_body, junk)) in block.iter().enumerate() {
                    let (carried, bodies) = &mut payloads[k % parts];
                    let h = headers[*index].clone();
                    if *with_body {
                        let tag = h.nonce + if *junk == 0 { 100 } else { 0 };
                        bodies.push((carried.len(), unlock(tag)));
                    }
                    carried.push(h);
                }
                if reversed {
                    payloads.reverse();
                }
                let payloads: Vec<Payload> = payloads
                    .into_iter()
                    .filter(|(carried, _)| !carried.is_empty())
                    .map(|(carried, bodies)| republish(&carried, &bodies))
                    .collect();
                btc.mine(&payloads);
                prop_assert!(tracker.sync(&known).is_ok());
                let (kind, depth, again) = reorgs[step];
                if kind == 0 {
                    // never below BTC block 2: block 3, an anchor, may go
                    let depth = depth.min(btc.btc.tip_height() - 2);
                    btc.btc.reorg(depth);
                    if again {
                        // the orphaned transactions confirm again in one block
                        btc.btc.mine(1);
                    } else {
                        btc.btc.mine_block(Vec::new()).unwrap();
                    }
                    prop_assert!(tracker.sync(&known).is_ok());
                }
            }
            let fresh = btc.tracker();
            prop_assert!(fresh.sync(&known).is_ok());
            let snap = tracker.snapshot().unwrap();
            prop_assert_eq!(snap.record(), fresh.snapshot().unwrap().record());
            for (hash, counted) in &snap.counted {
                let h = headers.iter().find(|h| h.hash() == *hash).unwrap();
                prop_assert!(!known.rejected.contains(hash));
                prop_assert!(snap.contains(&h.btc_anchor));
                for parent in &h.parents {
                    prop_assert!(
                        *parent == g.hash()
                            || snap.counted_at(parent).is_some_and(|p| p <= counted.landing.height)
                    );
                }
                prop_assert_eq!(counted.settles, h.nonce % 3 != 0);
            }
            let mut bodies = Bodies::new(DEVNET, g.hash());
            for (hash, landed) in &snap.bodies {
                let h = headers.iter().find(|h| h.hash() == *hash).unwrap();
                prop_assert!(snap.counted_at(hash).is_some_and(|c| c <= landed.landing.height));
                let body = bodies.body(&snap, hash).unwrap().unwrap();
                prop_assert_eq!(&*body, &entries(&unlock(h.nonce)));
            }
        }
    }
}
