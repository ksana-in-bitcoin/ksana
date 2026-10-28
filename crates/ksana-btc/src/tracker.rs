//! BTC tracking (SPEC §6.3, §7, §8.5, §10.3, §10.4; D-034-D-038, D-049): shared by the fast-lane node and the
//! standalone indexer.
//!
//! - [`BtcTracker`]: tracks bitcoind's best chain, scanning block by block from near the genesis anchor: native
//!   transactions (§7.4), fast-lane block headers appearing in KSN1 payloads, and the transactions landing each
//!   fast-lane block's data on BTC (for a block that needs a body, a settlement or republish payload carrying the same
//!   body, D-077). A settlement or republish payload counts only when every header it carries anchors below the BTC
//!   height it is confirmed at ([`payload_counts`], D-100); one that does not is ignored as a whole. On a BTC reorg it
//!   truncates to the fork point and rescans. The scan results are published as an immutable [`BtcSnapshot`]; block
//!   production, checking and RPC each hold their own snapshot without blocking each other.
//! - When a payload carrying a body is scanned, this node may not know the corresponding fast-lane block yet (joined
//!   late, catching up after downtime, was isolated): it is first recorded as pending, and once the node has stored
//!   the block (and before every block check) it is checked against the block contents and counted
//!   ([`BtcTracker::resolve`]); otherwise a valid block containing that block's unlock entry would be misjudged
//!   invalid.
//! - [`AnchoredView`]: the execution layer's [`BtcView`]; all answers are relative to some block's `btc_anchor`
//!   (D-035).
//! - [`prefix_height`]: the settled prefix (D-038); in the DAG it accumulates block by block along the main chain (the
//!   virtual selected chain), each block together with the blocks it merges (the settled closure of D-063).
//! - [`unlock_segment`]: the unlockable segment (D-079); the node and the indexer compute the execution layer's
//!   `BlockEnv::unlockable` from the main-chain summaries with this same function.
//!
//! A fault of the data source (bitcoind) is always returned as a local fault and never judges a block invalid. The node
//! and the indexer each implement [`KnownBlocks`]: both give exactly the same answers for the same BTC chain, which is
//! the premise of "the indexer and the fast-lane node agree on state" (D-049).

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, RwLock};

use bitcoin::consensus::encode::serialize;
use bitcoin::hashes::Hash;
use borsh::{BorshDeserialize, BorshSerialize};
use ksana_execution::keys::{self, UnlockCursor};
use ksana_execution::{BtcView, StateSource, UnlockItem};
use ksana_primitives::{BlockHeader, BtcAnchor, Entry, EntryData, OutPoint, Params, Payload, VmOp};

use crate::client::BtcClient;
use crate::data::tx_payload;
use crate::native::{native_txs, NativeTx};
use crate::payload::bodies_in;
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

/// The information needed to judge whether a payload on BTC lands the data of a fast-lane block that needs a body
/// (D-038, D-077): the header and the entries (in payload form). Any settlement or republish payload carrying the same
/// body will do (the body proves itself through the `entries_root` in the header).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SettlementContext {
    /// The block header.
    pub header: BlockHeader,
    /// All entries of the block (in payload form).
    pub entries: Vec<EntryData>,
}

/// The answer of [`KnownBlocks::block_data`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BlockData {
    /// This side does not know the block yet, or cannot determine its body yet: the payload is kept and judged later
    /// ([`BtcTracker::resolve`]).
    Unknown,
    /// The block needs no body (an empty block, or one with only forced-inclusion entries): the header landing on BTC
    /// is enough.
    NoBody,
    /// The block needs a body.
    Body(Box<SettlementContext>),
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

impl Landing {
    /// The order: lower height first, and within the same height, earlier in the BTC block first (D-095).
    fn order(&self) -> (u32, u32) {
        (self.height, self.index)
    }
}

/// Looks up fast-lane blocks known to this node by block hash (implemented by the chain).
pub trait KnownBlocks {
    /// The block's body information (D-077).
    fn block_data(&self, block_hash: &[u8; 32]) -> Result<BlockData, TrackerError>;

    /// Called when a settlement or republish payload is scanned in transaction `tx` at BTC height `btc_height` (in scan
    /// order; blocks rescanned after a BTC reorg are called for again). `snapshot` is the snapshot under construction,
    /// covering up to that BTC block (the header time of the anchor BTC block is taken from it, D-083). The indexer
    /// uses this to collect headers and bodies, and the node uses it to maintain the header DAG for the finality
    /// decision (D-082); by default it does nothing.
    fn observe_payload(
        &self,
        snapshot: &BtcSnapshot,
        btc_height: u32,
        tx: &bitcoin::Transaction,
        payload: &Payload,
    ) -> Result<(), TrackerError> {
        let _ = (snapshot, btc_height, tx, payload);
        Ok(())
    }
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
    match crate::native::forced_heights(params, parent_anchor.height, anchor.height) {
        Some(range) => crate::native::forced_entries(snapshot.native_in(range).iter()),
        None => Vec::new(),
    }
}

/// Whether a settlement or republish payload confirmed at BTC height `height` counts (D-100): every header it carries
/// anchors below `height`. An honest producer anchors to a BTC block that exists before its block, so every payload
/// carrying an honest header lands above that anchor; a payload that does not count is ignored as a whole (its headers
/// do not appear, the bodies it carries do not land), so the BTC block a header anchors to has always been scanned
/// when the header appears, and the timestamp bound of the header DAG is always checked (D-083).
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
    /// Fast-lane block hash → the earliest appearance of its header in a KSN1 settlement or republish payload (BTC
    /// height, index within the block and the payload's sender, D-095).
    headers_seen: BTreeMap<[u8; 32], Landing>,
    /// The reverse index of `headers_seen`: BTC height → the fast-lane headers that first appeared at this height (in
    /// order of appearance), for the finality decision processing height by height (D-082).
    headers_by_height: BTreeMap<u32, Vec<[u8; 32]>>,
    /// Fast-lane blocks that need a body → the transactions confirmed on BTC that land their data (height, index
    /// within the block and the payload's sender), in order of appearance (non-decreasing height): settlement or
    /// republish payloads carrying the same body (D-077, D-095).
    settlements: BTreeMap<[u8; 32], Vec<Landing>>,
    /// KSN1 settlement transactions seen (valid or not): txid → confirmation height.
    ksn1_txs: BTreeMap<[u8; 32], u32>,
    /// Fast-lane blocks this node did not know at scan time → the transactions carrying their body with their
    /// confirmation height and index within the block (in order of appearance), to be checked once the block is known.
    unresolved: BTreeMap<[u8; 32], Vec<Unresolved>>,
}

/// One pending payload of a fast-lane block this node did not know at scan time: the transaction carrying its body
/// with its confirmation height and index within the block. The pending payloads of one scanned transaction share a
/// single copy of it, however many unknown blocks' bodies its payload carries (a republish payload carries up to
/// `MAX_SETTLEMENT_HEADERS` of them, 500).
type Unresolved = (Arc<bitcoin::Transaction>, u32, u32);

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

    /// The fast-lane headers that first appeared at BTC height `height` (never seen at any lower height), in order of
    /// appearance (D-082).
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
        self.native
            .range(heights)
            .flat_map(|(_, txs)| txs.iter().cloned())
            .collect()
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

    /// The height at which the block's data landed on BTC (D-038, D-077): for a block that needs a body, the earliest
    /// transaction landing its data; for other blocks, the height at which the header first appeared.
    pub fn data_height(&self, block: &BlockSummary) -> Option<u32> {
        self.block_data_height(&block.hash, block.settles)
    }

    /// Same as [`BtcSnapshot::data_height`], queried by block hash and whether the body must land on BTC (`settles`).
    pub fn block_data_height(&self, block_hash: &[u8; 32], settles: bool) -> Option<u32> {
        self.data_landing(block_hash, settles).map(|l| l.height)
    }

    /// The first landing of the block's data on BTC (D-095): for a block that needs a body, the first transaction
    /// landing its data ([`BtcSnapshot::settlement_of`]); for other blocks, the first appearance of the header.
    pub fn data_landing(&self, block_hash: &[u8; 32], settles: bool) -> Option<Landing> {
        if settles {
            self.first_settlement(block_hash)
        } else {
            self.headers_seen.get(block_hash).copied()
        }
    }

    /// The BTC height at which the header first appeared in a KSN1 settlement or republish payload (whether or not that
    /// transaction is a valid settlement).
    pub fn header_seen(&self, block_hash: &[u8; 32]) -> Option<u32> {
        self.headers_seen.get(block_hash).map(|l| l.height)
    }

    /// Whether the block still has pending payloads: the block was unknown when they were scanned, so whether the
    /// payloads carry its body cannot be determined ([`BtcTracker::resolve`] / [`BtcTracker::resolve_all`] resolve them
    /// once it is known).
    pub fn has_unresolved(&self, block_hash: &[u8; 32]) -> bool {
        self.unresolved.contains_key(block_hash)
    }

    /// The earliest confirmed transaction (txid, height) landing the block's data on BTC (a payload carrying its body,
    /// D-077); within the same height, the earliest in the BTC block (D-095).
    pub fn settlement_of(&self, block_hash: &[u8; 32]) -> Option<([u8; 32], u32)> {
        self.first_settlement(block_hash)
            .map(|l| (l.txid, l.height))
    }

    fn first_settlement(&self, block_hash: &[u8; 32]) -> Option<Landing> {
        self.settlements
            .get(block_hash)?
            .iter()
            .min_by_key(|l| l.order())
            .copied()
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
        self.headers_seen.retain(|_, l| l.height <= height);
        self.headers_by_height.split_off(&height.saturating_add(1));
        for all in self.settlements.values_mut() {
            all.retain(|l| l.height <= height);
        }
        self.settlements.retain(|_, all| !all.is_empty());
        self.ksn1_txs.retain(|_, h| *h <= height);
        for all in self.unresolved.values_mut() {
            all.retain(|(_, h, _)| *h <= height);
        }
        self.unresolved.retain(|_, all| !all.is_empty());
        removed_native
    }

    /// Drops what a DAG pruned at a pruning point anchored at height `cut` or above can no longer ask about (D-097):
    /// the BTC blocks below `cut` (their headers, native transactions, the fast-lane headers first seen there and the
    /// KSN1 settlement transactions), and the landings and pending payloads of the fast-lane blocks for which `keep` is
    /// false whose data first appeared below `cut`. The tip is always kept. Returns the number of fast-lane blocks
    /// whose records were dropped.
    ///
    /// Every block in the future of the pruning point has an anchor at or above the pruning point's (anchors never
    /// decrease along parent edges), its data lands above its anchor and its forced-inclusion entries name native
    /// transactions from its window start on (D-100), so nothing a kept block needs is below `cut` when `cut` is at most
    /// the pruning point's anchor height less NATIVE_CONFIRMATIONS; `keep` additionally protects the blocks the caller
    /// still asks about (the main chain's window).
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
        let before = self
            .headers_seen
            .len()
            .saturating_add(self.settlements.len())
            .saturating_add(self.unresolved.len());
        self.headers_seen
            .retain(|hash, l| l.height >= cut || keep(hash));
        self.settlements
            .retain(|hash, all| all.iter().all(|l| l.height >= cut) || keep(hash));
        self.unresolved
            .retain(|hash, all| all.iter().all(|(_, h, _)| *h >= cut) || keep(hash));
        let after = self
            .headers_seen
            .len()
            .saturating_add(self.settlements.len())
            .saturating_add(self.unresolved.len());
        before.saturating_sub(after)
    }

    /// The snapshot in the local format of a pruning checkpoint (D-097). The pending payloads that share one copy of
    /// their transaction in memory share it in the record too: each transaction is written once (D-098).
    pub fn record(&self) -> SnapshotRecord {
        let mut pending_txs = Vec::new();
        let mut written: BTreeMap<*const bitcoin::Transaction, u32> = BTreeMap::new();
        let mut unresolved = BTreeMap::new();
        for (hash, all) in &self.unresolved {
            let mut items = Vec::with_capacity(all.len());
            for (tx, height, index) in all {
                let at = *written.entry(Arc::as_ptr(tx)).or_insert_with(|| {
                    let at = u32::try_from(pending_txs.len()).unwrap_or(u32::MAX);
                    pending_txs.push(serialize(&**tx));
                    at
                });
                items.push((at, *height, *index));
            }
            unresolved.insert(*hash, items);
        }
        SnapshotRecord {
            base: self.base,
            chain: self.chain.clone(),
            native: self.native.clone(),
            headers_seen: self.headers_seen.clone(),
            headers_by_height: self.headers_by_height.clone(),
            settlements: self.settlements.clone(),
            ksn1_txs: self.ksn1_txs.clone(),
            pending_txs,
            unresolved,
        }
    }

    /// The snapshot a [`BtcSnapshot::record`] was taken from (the pending payloads of one transaction sharing one copy
    /// of it again).
    pub fn from_record(record: SnapshotRecord) -> Result<Self, String> {
        let mut txs = Vec::with_capacity(record.pending_txs.len());
        for bytes in &record.pending_txs {
            let tx: bitcoin::Transaction = bitcoin::consensus::encode::deserialize(bytes)
                .map_err(|e| format!("a pending transaction in the checkpoint is corrupt: {e}"))?;
            txs.push(Arc::new(tx));
        }
        let mut unresolved = BTreeMap::new();
        for (hash, all) in record.unresolved {
            let mut items = Vec::with_capacity(all.len());
            for (at, height, index) in all {
                let tx = usize::try_from(at)
                    .ok()
                    .and_then(|at| txs.get(at))
                    .ok_or("a pending payload in the checkpoint names a transaction it lacks")?;
                items.push((Arc::clone(tx), height, index));
            }
            unresolved.insert(hash, items);
        }
        Ok(BtcSnapshot {
            base: record.base,
            chain: record.chain,
            native: record.native,
            headers_seen: record.headers_seen,
            headers_by_height: record.headers_by_height,
            settlements: record.settlements,
            ksn1_txs: record.ksn1_txs,
            unresolved,
        })
    }
}

/// A pending payload in a [`SnapshotRecord`]: the position of its transaction in `pending_txs`, its height and its
/// index in the BTC block.
type PendingRecord = (u32, u32, u32);

/// A [`BtcSnapshot`] in the local format of a pruning checkpoint (D-097): the same fields, with the transactions
/// carrying the pending payloads in Bitcoin's consensus encoding, each once.
#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct SnapshotRecord {
    base: u32,
    chain: Vec<([u8; 32], [u8; 80])>,
    native: BTreeMap<u32, Vec<NativeTx>>,
    headers_seen: BTreeMap<[u8; 32], Landing>,
    headers_by_height: BTreeMap<u32, Vec<[u8; 32]>>,
    settlements: BTreeMap<[u8; 32], Vec<Landing>>,
    ksn1_txs: BTreeMap<[u8; 32], u32>,
    pending_txs: Vec<Vec<u8>>,
    unresolved: BTreeMap<[u8; 32], Vec<PendingRecord>>,
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

    /// For tests: a fast-lane header appears at BTC height `height` (recorded as its first appearance when it has not
    /// appeared before); the sender is a republish taking no reward.
    pub(crate) fn see_header(&mut self, block: [u8; 32], height: u32) {
        self.see_header_from(block, height, 0, Sender::Payee([0; 32]));
    }

    /// For tests: a fast-lane header appears in transaction `index` at BTC height `height`; the payload's sender is
    /// `sender`.
    pub(crate) fn see_header_from(
        &mut self,
        block: [u8; 32],
        height: u32,
        index: u32,
        sender: Sender,
    ) {
        if let std::collections::btree_map::Entry::Vacant(seen) = self.headers_seen.entry(block) {
            seen.insert(Landing {
                txid: [0; 32],
                height,
                index,
                sender,
            });
            self.headers_by_height
                .entry(height)
                .or_default()
                .push(block);
        }
    }

    /// For tests: a payload of the unknown fast-lane block `block` confirms in `tx` at BTC height `height`.
    pub(crate) fn add_unresolved_for_test(
        &mut self,
        block: [u8; 32],
        tx: bitcoin::Transaction,
        height: u32,
    ) {
        self.unresolved
            .entry(block)
            .or_default()
            .push((Arc::new(tx), height, 0));
    }

    /// For tests: a transaction landing the block's data on BTC confirms at height `height`; the sender is a republish
    /// taking no reward.
    pub(crate) fn settle(&mut self, block: [u8; 32], txid: [u8; 32], height: u32) {
        self.settle_from(block, txid, height, 0, Sender::Payee([0; 32]));
    }

    /// For tests: transaction `txid` landing the block's data on BTC confirms as transaction `index` at BTC height
    /// `height`; the payload's sender is `sender`.
    pub(crate) fn settle_from(
        &mut self,
        block: [u8; 32],
        txid: [u8; 32],
        height: u32,
        index: u32,
        sender: Sender,
    ) {
        let all = self.settlements.entry(block).or_default();
        all.push(Landing {
            txid,
            height,
            index,
            sender,
        });
        all.sort_by_key(Landing::order);
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
                .settlements
                .get(&b.hash)
                .is_some_and(|all| all.iter().any(|l| l.height <= anchor_height))
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
    let Some(landing) = snapshot.data_landing(&block.hash, block.settles) else {
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
    genesis_anchor: BtcAnchor,
    snapshot: RwLock<Arc<BtcSnapshot>>,
    /// Snapshot updates (sync, resolving pending settlements) are mutually exclusive, so that concurrent updates do not
    /// overwrite each other.
    update: Mutex<()>,
}

fn btc_err(e: impl std::fmt::Display) -> TrackerError {
    TrackerError::Btc(e.to_string())
}

impl BtcTracker {
    /// Creates the tracker: the genesis anchor block must be on bitcoind's best chain (D-035). The snapshot starts at
    /// `min(genesis anchor height, h(genesis) + 1)` (the first forced-inclusion range of §7.4 starts at
    /// `h(genesis) + 1`).
    pub fn new(
        client: Arc<dyn BtcClient>,
        params: Params,
        genesis_anchor: BtcAnchor,
    ) -> Result<Self, TrackerError> {
        let base = initial_base(&params, &genesis_anchor);
        Self::with_snapshot(client, params, genesis_anchor, BtcSnapshot::new(base))
    }

    /// Creates the tracker continuing from `snapshot` (restored from a pruning checkpoint, D-097): the next sync
    /// continues from its tip, as if this process had scanned it. The genesis anchor block must be on bitcoind's best
    /// chain (D-035).
    pub fn with_snapshot(
        client: Arc<dyn BtcClient>,
        params: Params,
        genesis_anchor: BtcAnchor,
        snapshot: BtcSnapshot,
    ) -> Result<Self, TrackerError> {
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

    /// Prunes the snapshot (D-097, [`BtcSnapshot::prune`]) and publishes the result. Returns the number of fast-lane
    /// blocks whose records were dropped.
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
    /// snapshot changed. `known` identifies which fast-lane block a settlement transaction confirmed on BTC belongs to.
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

    /// Checks and counts the pending payloads of block `block_hash` (the block is now known). Returns whether the
    /// snapshot changed; on a data source fault the pending items are kept. The snapshot is copied only when the block
    /// turns out to be known.
    pub fn resolve(
        &self,
        known: &dyn KnownBlocks,
        block_hash: &[u8; 32],
    ) -> Result<bool, TrackerError> {
        let _guard = self.lock_update()?;
        let old = self.snapshot()?;
        if !old.unresolved.contains_key(block_hash) {
            return Ok(false);
        }
        let data = known.block_data(block_hash)?;
        if matches!(data, BlockData::Unknown) {
            return Ok(false);
        }
        let mut next = BtcSnapshot::clone(&old);
        self.resolve_in(&mut next, block_hash, data, &mut Decoded::default());
        self.publish(next)?;
        Ok(true)
    }

    /// Checks and counts the pending settlements of all known blocks. Returns whether the snapshot changed.
    ///
    /// Every pending block is looked up, but the snapshot is copied only once one of them turns out to be known: the
    /// pending payloads of blocks that stay unknown (such as made-up headers nobody produces) cost one lookup per round
    /// and nothing more.
    pub fn resolve_all(&self, known: &dyn KnownBlocks) -> Result<bool, TrackerError> {
        let _guard = self.lock_update()?;
        let old = self.snapshot()?;
        let mut next: Option<BtcSnapshot> = None;
        let mut decoded = Decoded::default();
        for block_hash in old.unresolved.keys() {
            let data = known.block_data(block_hash)?;
            if matches!(data, BlockData::Unknown) {
                continue;
            }
            let snap = next.get_or_insert_with(|| BtcSnapshot::clone(&old));
            self.resolve_in(snap, block_hash, data, &mut decoded);
        }
        let Some(next) = next else {
            return Ok(false);
        };
        self.publish(next)?;
        Ok(true)
    }

    /// Resolves the pending payloads of block `block_hash` in `snap` by the block's body information `data` (nothing
    /// changes while the block is still unknown).
    fn resolve_in(
        &self,
        snap: &mut BtcSnapshot,
        block_hash: &[u8; 32],
        data: BlockData,
        decoded: &mut Decoded,
    ) {
        let ctx = match data {
            BlockData::Unknown => return,
            BlockData::NoBody => {
                snap.unresolved.remove(block_hash);
                return;
            }
            BlockData::Body(ctx) => ctx,
        };
        let pending = snap.unresolved.remove(block_hash).unwrap_or_default();
        for (tx, height, index) in pending {
            let txid = tx.compute_txid().to_byte_array();
            match decoded.lands(&self.params, &tx, &ctx) {
                Ok(sender) => {
                    let all = snap.settlements.entry(*block_hash).or_default();
                    all.push(Landing {
                        txid,
                        height,
                        index,
                        sender,
                    });
                    all.sort_by_key(Landing::order);
                }
                Err(e) => eprintln!(
                    "[ksana-node] transaction {} at BTC height {height} cannot land the data of fast-lane block {}: {e}",
                    hex::encode(txid),
                    hex::encode(block_hash)
                ),
            }
        }
    }

    /// Scans one BTC block: the header, native transactions, KSN1 settlement and republish payloads (those that count,
    /// [`payload_counts`]).
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
            known.observe_payload(snap, height, tx, &payload)?;
            let txid = tx.compute_txid().to_byte_array();
            if matches!(payload, Payload::Settlement { .. }) {
                snap.ksn1_txs.entry(txid).or_insert(height);
            }
            // the payload's sender (D-095): a settlement payload without headers carries neither headers nor bodies
            let Some(sender) = payload_sender(&payload) else {
                continue;
            };
            let landing = Landing {
                txid,
                height,
                index,
                sender,
            };
            for h in headers {
                if let std::collections::btree_map::Entry::Vacant(seen) =
                    snap.headers_seen.entry(h.hash())
                {
                    let hash = *seen.key();
                    seen.insert(landing);
                    snap.headers_by_height.entry(height).or_default().push(hash);
                }
            }
            // each body the payload carries (the settlement payload's own block and the blocks in `bodies`, D-077):
            // whether it lands that block's data on BTC
            let carried = bodies_in(&payload);
            // the blocks not known yet share one copy of the transaction, however many bodies it carries
            let mut copy: Option<Arc<bitcoin::Transaction>> = None;
            for (block_hash, _) in &carried {
                let ctx = match known.block_data(block_hash)? {
                    BlockData::NoBody => continue,
                    BlockData::Unknown => {
                        // the block is not known yet: check once it is known (BtcTracker::resolve)
                        let copy = copy.get_or_insert_with(|| Arc::new(tx.clone()));
                        snap.unresolved.entry(*block_hash).or_default().push((
                            Arc::clone(copy),
                            height,
                            index,
                        ));
                        continue;
                    }
                    BlockData::Body(ctx) => ctx,
                };
                match carries_body(carried.iter().map(|(h, e)| (h, *e)), &ctx) {
                    Ok(()) => snap
                        .settlements
                        .entry(*block_hash)
                        .or_default()
                        .push(landing),
                    Err(e) => eprintln!(
                        "[ksana-node] transaction {} at BTC height {height} cannot land the data of fast-lane block {}: {e}",
                        hex::encode(txid),
                        hex::encode(block_hash)
                    ),
                }
            }
        }
        Ok(())
    }
}

/// Whether one of the bodies a payload carries ([`bodies_in`]: the settlement payload's own block entries and the
/// items in `bodies`, as block hash and entries) is the body of the block described by `ctx` (D-077): both the header
/// and the entries the same.
fn carries_body<'a>(
    mut bodies: impl Iterator<Item = (&'a [u8; 32], &'a [EntryData])>,
    ctx: &SettlementContext,
) -> Result<(), String> {
    let hash = ctx.header.hash();
    if bodies.any(|(h, entries)| *h == hash && entries == ctx.entries.as_slice()) {
        Ok(())
    } else {
        Err("the payload does not carry the block's body".into())
    }
}

/// What resolving needs from a pending transaction: the bodies its payload carries ([`bodies_in`]) and the payload's
/// sender ([`payload_sender`]); `None` when it has no decodable KSN1 payload.
type Carried = Option<(Vec<([u8; 32], Vec<EntryData>)>, Option<Sender>)>;

/// The pending transaction decoded last in a resolution round and what it carries: a run of pending payloads sharing
/// one transaction (a payload can repeat a block's header and body up to `MAX_SETTLEMENT_HEADERS` times) decodes it and
/// hashes its headers once, not once per payload. Holds one transaction at a time.
#[derive(Default)]
struct Decoded(Option<(Arc<bitcoin::Transaction>, Carried)>);

impl Decoded {
    /// Whether pending transaction `tx` confirmed on BTC lands the data of the block described by `ctx` (D-077): its
    /// payload carries the same body as that block (whether or not the transaction is the settlement transaction some
    /// block carries). When it does, returns the payload's sender (D-095).
    fn lands(
        &mut self,
        params: &Params,
        tx: &Arc<bitcoin::Transaction>,
        ctx: &SettlementContext,
    ) -> Result<Sender, String> {
        if !self
            .0
            .as_ref()
            .is_some_and(|(last, _)| Arc::ptr_eq(last, tx))
        {
            self.0 = None;
        }
        let (_, carried) = self
            .0
            .get_or_insert_with(|| (Arc::clone(tx), Decoded::carried(params, tx)));
        let (bodies, sender) = carried.as_ref().ok_or("no decodable KSN1 payload")?;
        carries_body(bodies.iter().map(|(h, e)| (h, e.as_slice())), ctx)?;
        sender.ok_or_else(|| "the payload has no sender".to_string())
    }

    /// Decodes what transaction `tx` carries.
    fn carried(params: &Params, tx: &bitcoin::Transaction) -> Carried {
        let (_, payload) = tx_payload(params, tx)?;
        let bodies = bodies_in(&payload)
            .into_iter()
            .map(|(hash, entries)| (hash, entries.to_vec()))
            .collect();
        Some((bodies, payload_sender(&payload)))
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
    /// whose data appeared below it, keeps everything at or above the cut and whatever `keep` protects, and the record
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
        let tx = |seed: u8| bitcoin::Transaction {
            version: bitcoin::transaction::Version::TWO,
            lock_time: bitcoin::absolute::LockTime::ZERO,
            input: vec![bitcoin::TxIn {
                previous_output: bitcoin::OutPoint {
                    txid: bitcoin::Txid::from_byte_array([seed; 32]),
                    vout: 0,
                },
                ..Default::default()
            }],
            output: vec![],
        };
        snapshot.add_unresolved_for_test([0xe1; 32], tx(1), 12);
        snapshot.add_unresolved_for_test([0xe2; 32], tx(2), 25);
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
            let settles = b % 2 == 1;
            let kept = b >= 20 || b == 13;
            assert_eq!(
                pruned.block_data_height(&[b; 32], settles),
                kept.then_some(u32::from(b)),
                "block {b}"
            );
        }
        assert!(pruned.headers_at(18).is_empty());
        assert_eq!(pruned.headers_at(20), snapshot.headers_at(20));
        assert!(!pruned.has_unresolved(&[0xe1; 32]));
        assert!(pruned.has_unresolved(&[0xe2; 32]));
        // 8 blocks below the cut not protected (11, 12, 14..=19 minus 13) and one pending payload
        assert_eq!(dropped, 9);
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

/// Pending payloads on the mock BTC chain: bodies carried by a payload whose blocks this side did not know when it was
/// scanned (joined late, catching up, or made-up headers nobody will ever produce).
#[cfg(test)]
#[cfg(feature = "mock")]
mod pending_tests {
    use std::collections::BTreeSet;
    use std::sync::Weak;

    use bitcoin::key::{Keypair, Secp256k1};
    use bitcoin::secp256k1::SecretKey;
    use bitcoin::{
        absolute, transaction, Amount, ScriptBuf, Sequence, Transaction, TxIn, TxOut, WPubkeyHash,
        Witness,
    };
    use ksana_primitives::{compact, Body, DEVNET};

    use super::*;
    use crate::envelope::envelope_script;
    use crate::mock::MockBtc;

    /// The payee account of the republish payload.
    const PAYEE: [u8; 32] = [0x66; 32];

    /// Answers `block_data` from a table (blocks not in it are unknown). While a pending transaction is watched, every
    /// lookup records how many references to it exist at that moment.
    #[derive(Default)]
    struct Table {
        answers: Mutex<BTreeMap<[u8; 32], BlockData>>,
        watched: Mutex<Option<Weak<Transaction>>>,
        seen: Mutex<Vec<usize>>,
    }

    impl Table {
        fn new(answers: BTreeMap<[u8; 32], BlockData>) -> Self {
            Table {
                answers: Mutex::new(answers),
                ..Table::default()
            }
        }

        fn set(&self, hash: [u8; 32], data: BlockData) {
            self.answers.lock().unwrap().insert(hash, data);
        }
    }

    impl KnownBlocks for Table {
        fn block_data(&self, block_hash: &[u8; 32]) -> Result<BlockData, TrackerError> {
            if let Some(watched) = &*self.watched.lock().unwrap() {
                self.seen.lock().unwrap().push(watched.strong_count());
            }
            let answers = self.answers.lock().unwrap();
            Ok(answers
                .get(block_hash)
                .cloned()
                .unwrap_or(BlockData::Unknown))
        }
    }

    fn body(header: &BlockHeader, entries: Vec<EntryData>) -> BlockData {
        BlockData::Body(Box::new(SettlementContext {
            header: header.clone(),
            entries,
        }))
    }

    /// The body of made-up block `i`: one unlock entry.
    fn unlock(i: usize) -> Vec<EntryData> {
        vec![EntryData::Unlock {
            height: u64::try_from(i).unwrap() + 1,
            index: 0,
        }]
    }

    /// The entries of the block the settlement payload belongs to.
    fn own_entries() -> Vec<EntryData> {
        vec![EntryData::Unlock {
            height: 77,
            index: 1,
        }]
    }

    /// `n` made-up headers in a chain; with the empty entries root each one after the first takes 14 bytes in a payload
    /// (D-075). No such blocks exist, so they stay unknown unless a test says otherwise.
    fn made_up_headers(n: usize) -> Vec<BlockHeader> {
        let mut parent = [0x11; 32];
        (0..n)
            .map(|i| {
                let i = u64::try_from(i).unwrap();
                let header = BlockHeader {
                    version: 1,
                    height: i + 1,
                    parents: vec![parent],
                    btc_anchor: BtcAnchor {
                        height: 1,
                        hash: [0x22; 32],
                    },
                    entries_root: compact::empty_entries_root(),
                    state_root: [0x44; 32],
                    producer: [0x55; 32],
                    timestamp: 1_700_000_000 + i,
                    bits: 0x207f_ffff,
                    nonce: i,
                };
                parent = header.hash();
                header
            })
            .collect()
    }

    /// A transaction spending `inputs` with the KSN1 envelope carrying `payload` in the first input's witness and a
    /// `junk`-byte leaf that is not an envelope in the second one (signatures and control blocks are placeholders: the
    /// mock chain checks neither).
    fn carrier(inputs: &[bitcoin::OutPoint], payload: &Payload, junk: usize) -> Transaction {
        let key =
            Keypair::from_secret_key(&Secp256k1::new(), &SecretKey::from_slice(&[7; 32]).unwrap())
                .x_only_public_key()
                .0;
        let leaf = envelope_script(&key, &DEVNET.magic, &payload.encode(&DEVNET).unwrap()).unwrap();
        let witness = |leaf: Vec<u8>| Witness::from_slice(&[vec![1; 64], leaf, vec![0xc0; 33]]);
        let witnesses = [witness(leaf.to_bytes()), witness(vec![0x51; junk])];
        Transaction {
            version: transaction::Version::TWO,
            lock_time: absolute::LockTime::ZERO,
            input: inputs
                .iter()
                .zip(witnesses)
                .map(|(previous_output, witness)| TxIn {
                    previous_output: *previous_output,
                    script_sig: ScriptBuf::new(),
                    sequence: Sequence::MAX,
                    witness,
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

    /// BTC block 2 holds two transactions carrying bodies: transaction 1 a republish payload with the headers and
    /// bodies of `n` made-up blocks (plus a `junk`-byte leaf in a second input), transaction 2 the settlement payload of
    /// block `own`, which also carries the bodies of the first two made-up blocks again.
    struct Scenario {
        btc: Arc<MockBtc>,
        headers: Vec<BlockHeader>,
        own: BlockHeader,
        republish: Transaction,
        settlement: Transaction,
    }

    impl Scenario {
        fn new(n: usize, junk: usize) -> Self {
            let btc = Arc::new(MockBtc::new());
            let funding = ScriptBuf::new_p2wpkh(&WPubkeyHash::from_byte_array([8; 20]));
            let funded = btc.fund(&funding, &[50_000, 50_000, 50_000]);
            let headers = made_up_headers(n);
            let republish = Payload::Headers {
                headers: headers.clone(),
                bodies: (0..n)
                    .map(|i| Body {
                        header: u32::try_from(i).unwrap(),
                        entries: unlock(i),
                    })
                    .collect(),
                payee: PAYEE,
            };
            let republish = carrier(&funded[..2], &republish, junk);
            let own = BlockHeader {
                height: 9_999,
                nonce: 9_999,
                ..headers[1].clone()
            };
            let settlement = Payload::Settlement {
                headers: vec![headers[0].clone(), headers[1].clone(), own.clone()],
                entries: own_entries(),
                bodies: vec![
                    Body {
                        header: 0,
                        entries: unlock(0),
                    },
                    Body {
                        header: 1,
                        entries: unlock(1),
                    },
                ],
            };
            let settlement = carrier(&funded[2..], &settlement, 0);
            btc.mine_block(vec![republish.clone(), settlement.clone()])
                .unwrap();
            Scenario {
                btc,
                headers,
                own,
                republish,
                settlement,
            }
        }

        fn tracker(&self) -> BtcTracker {
            tracker_on(&self.btc)
        }

        fn hash(&self, i: usize) -> [u8; 32] {
            self.headers[i].hash()
        }
    }

    /// A tracker following `btc` from its genesis block.
    fn tracker_on(btc: &Arc<MockBtc>) -> BtcTracker {
        let client: Arc<dyn BtcClient> = btc.clone();
        let genesis = BtcAnchor {
            height: 0,
            hash: client.block_hash(0).unwrap().unwrap(),
        };
        BtcTracker::new(client, DEVNET, genesis).unwrap()
    }

    /// BTC block 2 holds a republish payload (transaction 1) repeating the header and body of one made-up block
    /// `copies` times: every copy has the same block hash.
    fn repeated(copies: usize) -> (Arc<MockBtc>, BlockHeader, Transaction) {
        let btc = Arc::new(MockBtc::new());
        let funding = ScriptBuf::new_p2wpkh(&WPubkeyHash::from_byte_array([8; 20]));
        let funded = btc.fund(&funding, &[50_000, 50_000]);
        let header = made_up_headers(1).remove(0);
        let payload = Payload::Headers {
            headers: vec![header.clone(); copies],
            bodies: (0..copies)
                .map(|i| Body {
                    header: u32::try_from(i).unwrap(),
                    entries: unlock(0),
                })
                .collect(),
            payee: PAYEE,
        };
        let tx = carrier(&funded, &payload, 0);
        assert_eq!(tx_payload(&DEVNET, &tx).map(|(_, p)| p), Some(payload));
        btc.mine_block(vec![tx.clone()]).unwrap();
        (btc, header, tx)
    }

    /// The transactions held for the pending payloads of transaction `index` in BTC block 2 (one per pending body).
    fn pending_of(snap: &BtcSnapshot, index: u32) -> Vec<&Arc<Transaction>> {
        snap.unresolved
            .values()
            .flatten()
            .filter(|(_, height, at)| (*height, *at) == (2, index))
            .map(|(tx, _, _)| tx)
            .collect()
    }

    /// A payload carrying the bodies of many blocks unknown when it is scanned keeps one copy of its transaction for
    /// all of them, however many bodies it carries (a republish payload carries up to `MAX_SETTLEMENT_HEADERS`); the
    /// copy is released once they are all resolved.
    #[test]
    fn pending_bodies_of_one_transaction_share_one_copy() {
        let n = DEVNET.max_settlement_headers;
        let s = Scenario::new(n, 16 * 1024);
        let tracker = s.tracker();
        let table = Table::default();
        assert!(tracker.sync(&table).unwrap());
        let copies = {
            let snap = tracker.snapshot().unwrap();
            assert!((0..n).all(|i| snap.has_unresolved(&s.hash(i))));
            assert!(snap.has_unresolved(&s.own.hash()));
            let (republish, settlement) = (pending_of(&snap, 1), pending_of(&snap, 2));
            assert_eq!((republish.len(), settlement.len()), (n, 3));
            assert!(republish.iter().all(|tx| ***tx == s.republish));
            assert!(settlement.iter().all(|tx| ***tx == s.settlement));
            let copies: BTreeSet<*const Transaction> =
                republish.iter().map(|tx| Arc::as_ptr(tx)).collect();
            let size = serialize(&s.republish).len();
            assert_eq!(
                copies.len(),
                1,
                "{n} pending bodies hold {} copies of the {size}-byte transaction carrying them ({} bytes)",
                copies.len(),
                copies.len() * size
            );
            assert_eq!(Arc::strong_count(republish[0]), n);
            let distinct: BTreeSet<*const Transaction> =
                settlement.iter().map(|tx| Arc::as_ptr(tx)).collect();
            assert_eq!(distinct.len(), 1);
            assert_eq!(Arc::strong_count(settlement[0]), 3);
            [Arc::downgrade(republish[0]), Arc::downgrade(settlement[0])]
        };

        // resolving every block releases both copies
        for (i, header) in s.headers.iter().enumerate() {
            table.set(header.hash(), body(header, unlock(i)));
        }
        table.set(s.own.hash(), body(&s.own, own_entries()));
        assert!(tracker.resolve_all(&table).unwrap());
        let snap = tracker.snapshot().unwrap();
        assert!(snap.unresolved.is_empty());
        assert_eq!(
            snap.settlement_of(&s.hash(n - 1)),
            Some((txid(&s.republish), 2))
        );
        assert!(copies.iter().all(|copy| copy.upgrade().is_none()));
    }

    /// A pruning checkpoint keeps the sharing (D-097): its record holds each pending transaction once, and the snapshot
    /// restored from it shares one copy among the pending payloads of each transaction again.
    #[test]
    fn a_checkpoint_keeps_one_copy_per_pending_transaction() {
        let n = DEVNET.max_settlement_headers;
        let s = Scenario::new(n, 16 * 1024);
        let tracker = s.tracker();
        assert!(tracker.sync(&Table::default()).unwrap());
        let snap = tracker.snapshot().unwrap();
        let record = snap.record();
        let size = borsh::to_vec(&record).unwrap().len();
        let txs = serialize(&s.republish).len() + serialize(&s.settlement).len();
        assert!(
            size < txs + 1_000 * (n + 3),
            "a record of {size} bytes for {txs} bytes of pending transactions"
        );
        let restored = BtcSnapshot::from_record(record.clone()).unwrap();
        assert_eq!(restored.unresolved, snap.unresolved);
        assert_eq!(restored.record(), record);
        let (republish, settlement) = (pending_of(&restored, 1), pending_of(&restored, 2));
        assert_eq!((republish.len(), settlement.len()), (n, 3));
        assert_eq!(Arc::strong_count(republish[0]), n);
        assert_eq!(Arc::strong_count(settlement[0]), 3);
    }

    /// A republish payload with more headers than `MAX_SETTLEMENT_HEADERS` does not decode (D-100): none of its bodies
    /// is pending, while the settlement payload next to it is scanned as usual.
    #[test]
    fn a_payload_over_the_header_limit_is_not_scanned() {
        let n = DEVNET.max_settlement_headers + 1;
        let s = Scenario::new(n, 0);
        let tracker = s.tracker();
        assert!(tracker.sync(&Table::default()).unwrap());
        let snap = tracker.snapshot().unwrap();
        assert!(pending_of(&snap, 1).is_empty());
        assert_eq!(pending_of(&snap, 2).len(), 3);
        assert!((2..n).all(|i| !snap.has_unresolved(&s.hash(i))));
    }

    /// D-100: a payload carrying a header anchored at or above the BTC height it is confirmed at does not count: none of
    /// its headers appears and none of its bodies is pending or lands, while the same payload confirmed one BTC block
    /// higher counts.
    #[test]
    fn a_payload_with_a_header_anchored_at_its_own_height_is_not_scanned() {
        let mut headers = made_up_headers(3);
        assert!(payload_counts(&headers, 2));
        assert!(!payload_counts(&headers, 1));
        headers[1].btc_anchor.height = 2;
        assert!(!payload_counts(&headers, 2));
        assert!(payload_counts(&headers, 3));
        let payload = Payload::Headers {
            headers: headers.clone(),
            bodies: (0..3)
                .map(|i| Body {
                    header: u32::try_from(i).unwrap(),
                    entries: unlock(i),
                })
                .collect(),
            payee: PAYEE,
        };
        let btc = Arc::new(MockBtc::new());
        let funding = ScriptBuf::new_p2wpkh(&WPubkeyHash::from_byte_array([8; 20]));
        let funded = btc.fund(&funding, &[50_000, 50_000]);
        let tx = carrier(&funded[..1], &payload, 0);
        let again = carrier(&funded[1..], &payload, 0);
        btc.mine_block(vec![tx]).unwrap();
        let tracker = tracker_on(&btc);
        assert!(tracker.sync(&Table::default()).unwrap());
        {
            let snap = tracker.snapshot().unwrap();
            assert_eq!(snap.tip().map(|(height, _)| height), Some(2));
            assert!(headers
                .iter()
                .all(|h| snap.header_seen(&h.hash()).is_none() && !snap.has_unresolved(&h.hash())));
        }
        btc.mine_block(vec![again]).unwrap();
        assert!(tracker.sync(&Table::default()).unwrap());
        let snap = tracker.snapshot().unwrap();
        assert!(headers
            .iter()
            .all(|h| snap.header_seen(&h.hash()) == Some(3) && snap.has_unresolved(&h.hash())));
    }

    /// Resolving pending payloads once their blocks are known records exactly what scanning them records when the
    /// blocks are already known: a payload carrying the block's body lands its data (a republish payload for its payee,
    /// a settlement payload for the block it belongs to, D-095), one carrying other entries does not, a block needing
    /// no body keeps only its header landing, and a block still unknown stays pending. A block known later is resolved
    /// on its own, a round finding nothing new changes nothing, and a BTC reorg drops landings and pending payloads.
    #[test]
    fn resolving_later_records_what_a_scan_knowing_the_blocks_records() {
        let n = 300;
        let s = Scenario::new(n, 1_024);
        let mut answers = BTreeMap::new();
        for (i, header) in s.headers.iter().enumerate() {
            let data = match i {
                0..100 => body(header, unlock(i)),
                100..150 => body(header, unlock(i + 1)),
                150..200 => BlockData::NoBody,
                _ => continue,
            };
            answers.insert(header.hash(), data);
        }
        answers.insert(s.own.hash(), body(&s.own, own_entries()));

        let early = s.tracker();
        assert!(early.sync(&Table::new(answers.clone())).unwrap());
        let late = s.tracker();
        let table = Table::default();
        assert!(late.sync(&table).unwrap());
        let scanned = late.snapshot().unwrap();
        assert!((0..n).all(|i| scanned.has_unresolved(&s.hash(i))));
        assert!(scanned.settlements.is_empty());
        *table.answers.lock().unwrap() = answers;
        assert!(late.resolve_all(&table).unwrap());

        let (a, b) = (early.snapshot().unwrap(), late.snapshot().unwrap());
        assert_eq!(b.settlements, a.settlements);
        assert_eq!(b.unresolved, a.unresolved);
        assert_eq!(b.headers_seen, a.headers_seen);
        assert_eq!(b.headers_by_height, a.headers_by_height);
        assert_eq!(b.ksn1_txs, a.ksn1_txs);
        assert_eq!(b.chain, a.chain);

        let by_payee = Landing {
            txid: txid(&s.republish),
            height: 2,
            index: 1,
            sender: Sender::Payee(PAYEE),
        };
        let by_own = Landing {
            txid: txid(&s.settlement),
            height: 2,
            index: 2,
            sender: Sender::Block(s.own.hash()),
        };
        for i in [0, 1] {
            assert_eq!(b.settlements.get(&s.hash(i)), Some(&vec![by_payee, by_own]));
        }
        assert_eq!(b.settlements.get(&s.own.hash()), Some(&vec![by_own]));
        for i in 2..100 {
            assert_eq!(
                b.data_landing(&s.hash(i), true),
                Some(by_payee),
                "block {i}"
            );
            assert!(!b.has_unresolved(&s.hash(i)));
        }
        for i in 100..200 {
            assert_eq!(b.settlement_of(&s.hash(i)), None, "block {i}");
            assert_eq!(b.data_landing(&s.hash(i), false), Some(by_payee));
            assert!(!b.has_unresolved(&s.hash(i)));
        }
        for i in 200..n {
            assert_eq!(b.settlement_of(&s.hash(i)), None, "block {i}");
            assert!(b.has_unresolved(&s.hash(i)));
        }

        // nothing new is known: no change, the same snapshot stays published
        assert!(!late.resolve_all(&table).unwrap());
        assert!(Arc::ptr_eq(&b, &late.snapshot().unwrap()));
        // one more block becomes known: resolved on its own
        table.set(s.hash(250), body(&s.headers[250], unlock(250)));
        assert!(late.resolve(&table, &s.hash(250)).unwrap());
        let c = late.snapshot().unwrap();
        assert_eq!(c.data_landing(&s.hash(250), true), Some(by_payee));
        assert!(!c.has_unresolved(&s.hash(250)));
        assert!(c.has_unresolved(&s.hash(251)));
        assert!(!late.resolve(&table, &s.hash(251)).unwrap());

        // a BTC reorg dropping block 2 drops its landings and its pending payloads
        s.btc.reorg(1);
        s.btc.clear_mempool();
        s.btc.mine(2);
        assert!(late.sync(&table).unwrap());
        let d = late.snapshot().unwrap();
        assert!(d.settlements.is_empty());
        assert!(d.unresolved.is_empty());
        assert_eq!(d.header_seen(&s.hash(0)), None);
    }

    /// While no pending block is known, a resolution round looks the pending blocks up without copying the snapshot:
    /// during every lookup the published snapshot holds the only references to a pending transaction. Once a block is
    /// known, its pending payloads are counted.
    #[test]
    fn resolving_copies_the_snapshot_only_once_a_pending_block_is_known() {
        let n = 50;
        let s = Scenario::new(n, 0);
        let tracker = s.tracker();
        let table = Table::default();
        assert!(tracker.sync(&table).unwrap());
        let first = s.hash(0);
        let watched = Arc::downgrade(&tracker.snapshot().unwrap().unresolved[&first][0].0);
        let held = watched.strong_count();
        *table.watched.lock().unwrap() = Some(watched);
        assert!(!tracker.resolve_all(&table).unwrap());
        assert!(!tracker.resolve(&table, &first).unwrap());
        let seen = table.seen.lock().unwrap().clone();
        assert_eq!(
            seen.len(),
            n + 2,
            "the made-up blocks and the own block (resolve_all), then the first block (resolve)"
        );
        assert!(
            seen.iter().all(|count| *count == held),
            "references to a pending transaction during the lookups: {seen:?}; the published snapshot holds {held}"
        );

        table.set(first, body(&s.headers[0], unlock(0)));
        assert!(tracker.resolve(&table, &first).unwrap());
        let snap = tracker.snapshot().unwrap();
        assert_eq!(snap.settlement_of(&first), Some((txid(&s.republish), 2)));
        assert!(!snap.has_unresolved(&first));
        assert!(snap.has_unresolved(&s.hash(1)));
    }

    /// A payload repeating one block's header and body (every copy has the same block hash): scanning it with the block
    /// known records one landing per copy, and so does resolving its pending payloads once the block is known; until
    /// then all the copies' pending payloads share one copy of the transaction.
    #[test]
    fn a_body_repeated_in_one_payload_resolves_like_a_scan_knowing_it() {
        let copies = 400;
        let (btc, header, tx) = repeated(copies);
        let known = Table::new(BTreeMap::from([(header.hash(), body(&header, unlock(0)))]));
        let early = tracker_on(&btc);
        assert!(early.sync(&known).unwrap());
        let late = tracker_on(&btc);
        assert!(late.sync(&Table::default()).unwrap());
        {
            let snap = late.snapshot().unwrap();
            let pending = pending_of(&snap, 1);
            assert_eq!(pending.len(), copies);
            assert_eq!(Arc::strong_count(pending[0]), copies);
            assert!(snap.settlements.is_empty());
        }
        assert!(late.resolve_all(&known).unwrap());

        let (a, b) = (early.snapshot().unwrap(), late.snapshot().unwrap());
        let landing = Landing {
            txid: txid(&tx),
            height: 2,
            index: 1,
            sender: Sender::Payee(PAYEE),
        };
        assert_eq!(
            a.settlements.get(&header.hash()),
            Some(&vec![landing; copies])
        );
        assert_eq!(b.settlements, a.settlements);
        assert!(a.unresolved.is_empty() && b.unresolved.is_empty());
    }
}
