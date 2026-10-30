//! The chain: state store + block store + block DAG + consensus + executor + BTC tracking (SPEC §6, §7, §10.2; D-041,
//! D-063).
//!
//! - Block DAG (GHOSTDAG, D-063): [`Chain::accept_block`] accepts any block. When all parents are known it checks the
//!   header (GHOSTDAG parent and merge limits, version, height, `bits` by `daa_score`, PoW, anchor not below the
//!   parents'), the acceptance rules that depend on the local view (anchor, timestamp) and the body (entries root,
//!   body size, settlement transactions, forced inclusion, then the state-independent entry rules with their
//!   signatures); once passed, the block is stored and inserted into the DAG; a block with unknown parents goes into
//!   the orphan pool and is processed once they arrive. A block delivered while syncing and refused for a reason that
//!   is not objective is answered from memory while the BTC tip stays the same, for a while (local policy, M27).
//! - Main chain (virtual selected chain): the selected chain of the block with the largest fork-choice key
//!   `(chain_work, hash)` (blue work plus the block's own work, D-100) in the DAG that is eligible for the main chain. Main-chain blocks are executed as merges per [`ksana_execution`] (first
//!   the entries of the blocks it merges in lenient mode, then its own entries in strict mode) and the `state_root` is
//!   checked; on a mismatch or a strict-mode failure that block (and the blocks whose selected chain passes through it)
//!   cannot be on the main chain ([`Chain::is_disqualified`]) but stays in the DAG and can be merged by other blocks.
//! - Main-chain change (reorg): the state is rolled back to the fork point, then executed block by block along the new
//!   selected chain; when the new chain contains a block ineligible for the main chain, the original main chain is
//!   restored and the next-best tip is chosen instead.
//! - BTC rules (D-034 - D-038, D-046, D-056): anchor, forced inclusion (the window starts at the highest anchor among
//!   all parents), settlement transactions, backlog limit (judged on a main-chain block over its entire past), the
//!   execution layer's BTC view; the timestamp must not exceed the time of the anchored BTC block plus
//!   `MAX_TIMESTAMP_AHEAD_OF_ANCHOR` (D-083, also checked when syncing historical blocks).
//! - BTC finality (checkpoint rule, D-082): the block headers that appeared on BTC form a separate header DAG
//!   ([`ksana_btc::headers::HeaderDag`], the same as the indexer's), and after every BTC sync the finalized chain is
//!   decided block by block by BTC height ([`ksana_btc::finality`]). The main chain must pass through the finalized
//!   block: tips forking below it take no part in main-chain selection however heavy they are; when the main chain
//!   does not pass through it, switch to the heaviest chain that does (even if lighter); when this node lacks the
//!   finalized block, it requests it from peers and rebuilds it from the data on BTC (D-093). For the landing status
//!   of each block's data that the decision relies on, blocks this node lacks are answered from the data on BTC, with
//!   the same criteria as the indexer ([`ksana_btc::bodies`]): a block that needs a body but has no payload on BTC
//!   carrying it has not landed yet, and the decision does not wait for it.
//!
//! Write order (crash consistency): when connecting a block to the main chain, the state is committed first
//! (version = height), then the main-chain index, receipts, settlement records and the new tip are written to the
//! block store; when retreating to the fork point, the store's tip is changed first, then the state is rolled back.
//! So the state version is never below the store's tip height: on startup the block store's tip is authoritative, a
//! state version that is ahead is rolled back to the tip height, and then the state root is checked.
//!
//! When the state at the fork point has been pruned (deeper than `REORG_WINDOW`), clearing the data directory is no
//! longer needed (D-093): the forced switch to a BTC-finalized chain and crash recovery clear the state, re-run
//! genesis, re-execute along the main chain block by block up to the fork point (the blocks are all in the block
//! store), and then connect the new chain as usual. An ordinary switch by work is still bound by `REORG_WINDOW` (§6.4).
//!
//! Pruning (D-097, [`Pruning`]): memory holds only what the chains this node can still switch to need. After a BTC
//! sync, the pruning root R is the highest block decided BTC final at least [`PRUNE_BTC_MARGIN`] BTC blocks ago (a
//! BTC reorg up to that depth leaves it final, so every main chain this node can still switch to passes through it),
//! and the pruning point P is the block on R's selected chain [`GhostdagParams::min_pruning_depth`] blue score below
//! it. The block DAG keeps P and its future, the header DAG likewise, the finality decision keeps R as its root, the
//! BTC snapshot drops the BTC blocks below P's anchor, and the main chain's summaries in memory start at a window base
//! below the unlock cursor of R's state and below the settled closure at P's anchor; the main chain below the window
//! is answered from the block store's acceptance index. Every block on, or merged by, a chain through R computes
//! exactly as on a node that never pruned. The block store keeps everything: peers syncing from far behind are served
//! the main chain's acceptance order from the store ([`Chain::sync_hashes`]). The state keeps every version from R's
//! height on, so any switch the finality rules allow can still be executed; a deeper switch (a BTC reorg deeper than
//! the margin) stops this node and needs a resync. Each pruning writes a checkpoint to the data directory, from which
//! a restart continues instead of rebuilding the whole history.

use std::borrow::Borrow;
use std::collections::{btree_map, BTreeMap, BTreeSet, BinaryHeap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::time::{Duration, Instant};

use borsh::{BorshDeserialize, BorshSerialize};
use ksana_btc::bodies::{check_stale_entries, Bodies, StaleEntries};
use ksana_btc::client::BtcClient;
use ksana_btc::finality::{BtcFinal, Exclusion, Finality, Outcome, Validity};
use ksana_btc::headers::{HeaderDag, HeaderDagRecord, Insert as HeaderInsert};
use ksana_btc::payload::{bodies_in, check_body_fits, entry_data, settlement_belongs_to};
pub use ksana_btc::prune::{Pruning, PRUNE_BTC_MARGIN, PRUNE_STEP};
use ksana_btc::settlement::{validate_attached, SettlementError, SettlementPair};
use ksana_btc::verdict::{self, Judge, Verdict, Verdicts};
use ksana_consensus::{
    check_pow, BlockDag, Consensus, ConsensusError, DagCheckpoint, GhostdagData, GhostdagError,
    GhostdagParams, Hash, PowAlgorithm, PowConsensus, U256,
};
use ksana_crypto::entries_root;
use ksana_execution::{
    check_entries_static, BlockEnv, BlockError, BlockOutcome, Executor, MergedBlock, StateSource,
    UnlockItem,
};
use ksana_primitives::hash::sha256;
use ksana_primitives::{
    compact, Block, BlockHeader, Body, BtcAnchor, Entry, EntryData, Params, Payload, SettlementTxs,
};
use ksana_state::{DbMemory, StateDb, StateView, WriteSet};

use crate::btc::{
    prefix_height, settled_height, settlement_backlog, unlock_cursor, unlock_segment,
    unlock_target, AnchoredPrevouts, AnchoredView, BlockSummary, BtcSnapshot, BtcTracker,
    KnownBlocks, MainChain, MainEntry, MainHistory, SnapshotRecord, TrackerError,
};
use crate::genesis::GenesisFile;
use crate::ops::NativeAuth;
use crate::settlement;
use crate::store::{AcceptedBlock, ChainStore, SettlementRecord};
use crate::NodeError;

/// Capacity of the orphan pool (D-041).
pub const MAX_ORPHANS: usize = 256;
/// Memory budget of the orphan pool (local policy, M26), each orphan counted by its encoded length plus the in-memory
/// size of the block and of its decoded entries: together with [`MAX_ORPHANS`] it bounds what peers can make the node
/// hold for blocks whose parents may never come. A decoded entry takes more than 20 times the memory of its encoding
/// (an unlock entry is 13 bytes encoded and about 300 in memory), so a single block message at the gossip limit can
/// hold hundreds of MiB. An orphan larger than the whole budget is not kept (it is not invalid for that: sync brings it
/// again after its parents).
pub const MAX_ORPHAN_BYTES: usize = 32 * 1024 * 1024;
/// Maximum number of ancestor blocks examined when a settlement payload carries other block headers (producer's policy,
/// D-063).
const PAYLOAD_HEADER_SEARCH: usize = 1_000;
/// The compact encoding of the other block headers carried by a settlement payload takes at most `MAX_BLOCK_DATA_BYTES`
/// divided by this (producer's policy, D-075); the rest is left for this block's header, entries and other bodies.
const PAYLOAD_HEADER_SHARE: usize = 2;
/// The other block bodies carried by a settlement payload take at most `MAX_BLOCK_DATA_BYTES` divided by this; the
/// earliest bodies that do not fit are landed by republishing (producer's policy, D-077).
const PAYLOAD_BODY_SHARE: usize = 4;
/// When a settlement transaction is already in flight and the bodies waiting to be carried in total, or the body of
/// this block's entries, exceed `MAX_BLOCK_DATA_BYTES` divided by this, the new block stops waiting for it and carries
/// settlement transactions as usual (producer's policy, D-077). This threshold is half the body budget
/// (1 / [`PAYLOAD_BODY_SHARE`]): only blocks whose body does not exceed the threshold pile up, so before the trigger
/// the backlog is at most twice the threshold, which fits exactly and does not push out the earliest bodies.
pub const PAYLOAD_BODY_TRIGGER: usize = 8;
/// In-flight settlement transaction: the anchor of the block carrying it lags the BTC tip by at most this many blocks
/// and its header has not counted yet (producer's policy, D-077, D-135). One unconfirmed for longer (e.g. waiting in the
/// mempool at too low a fee rate) no longer makes the new block's body wait for it, and the new block's payload carries
/// its header and the headers of its past that have not counted ([`Chain::payload_carry`]); its producer bumps it
/// through the reveal's P2A output, and other nodes rescue it once it pays less than their fee rate or has waited
/// `CLAIM_PATIENCE` BTC blocks (D-102, [`crate::rescue`]).
const IN_FLIGHT_BTC_BLOCKS: u32 = 2;
/// Capacity of the set of known invalid blocks: the earliest are forgotten first (a forgotten invalid block is only
/// checked again).
const MAX_INVALID: usize = 65_536;
/// At most this many unknown block versions are remembered for the one-warning-per-version rule (D-076).
const MAX_UNKNOWN_VERSIONS: usize = 16;
/// How long a refusal of a block delivered while syncing is answered from memory (local policy, M27, see [`Refusals`]).
const REFUSAL_TTL: Duration = Duration::from_secs(30);
/// At most this many refusals are remembered (local policy, M27): the earliest are forgotten first.
const MAX_REFUSALS: usize = 4_096;
/// Appended to the message of a refusal of a block delivered while syncing that is answered from memory, without
/// checking the block again (local policy, M27).
pub const REMEMBERED: &str = "(remembered from an earlier delivery under the current BTC tip, M27)";
/// The pruning checkpoint in the data directory (D-097).
const CHECKPOINT_FILE: &str = "pruning.ckpt";
/// The version of the checkpoint's local format (2: the finality decision's exclusions, D-101; 3: no pending headers,
/// unresolved payloads or candidate bodies, the landed bodies kept as their transactions, D-135).
const CHECKPOINT_VERSION: u32 = 3;

/// Data the producer intends the new block's settlement payload to carry (D-063, D-075, D-077, D-135): the other block
/// headers in this block's past that have not counted on BTC yet (by blue work, lowest first), and the bodies of those
/// among them that carry no settlement transactions, need a body and whose body has not landed yet.
#[derive(Clone, Debug, Default)]
pub struct Carry {
    /// Other block headers, by blue work from lowest to highest.
    pub headers: Vec<BlockHeader>,
    /// Bodies of some of those blocks, in header order.
    pub bodies: Vec<CarriedBody>,
    /// Among the most recent blocks in the past that carry settlement transactions, one has a settlement whose header
    /// has not counted yet (an in-flight batch, from at most [`IN_FLIGHT_BTC_BLOCKS`] BTC blocks ago).
    pub in_flight: bool,
    /// Total bytes of the bodies waiting to be carried (before the earliest are dropped for the budget).
    pub pending_len: usize,
}

/// One block body in a [`Carry`].
#[derive(Clone, Debug)]
pub struct CarriedBody {
    /// Index into [`Carry::headers`].
    pub header: usize,
    /// All entries of that block.
    pub entries: Vec<Entry>,
    /// Bytes it occupies in the payload.
    pub len: usize,
}

impl Carry {
    /// Bytes the bodies occupy in the payload.
    pub fn bodies_len(&self) -> usize {
        self.bodies
            .iter()
            .fold(0usize, |sum, b| sum.saturating_add(b.len))
    }

    /// The bodies in payload form.
    pub fn payload_bodies(&self) -> Result<Vec<Body>, NodeError> {
        self.bodies
            .iter()
            .map(|b| {
                Ok(Body {
                    header: u32::try_from(b.header)
                        .map_err(|_| NodeError::Internal("body index overflow".into()))?,
                    entries: entry_data(&b.entries),
                })
            })
            .collect()
    }

    /// Whether the new block should carry settlement transactions (D-077, producer's policy): there are bodies to carry
    /// (`own_len` is the length of the body of this block's entries, 0 when none is needed) and no settlement
    /// transaction is in flight; or the backlog of bodies ([`Carry::pending_len`]) or this block's body exceeds
    /// `threshold` bytes (stop waiting for the in-flight settlement transaction, lest the backlog exceed what one
    /// settlement payload can carry). Thus with little data the block containing each op settles immediately, and with
    /// continuous ops the bodies piled up while each settlement transaction is in flight (about one BTC block) are
    /// carried all at once by the next block after it confirms.
    pub fn settlement_due(&self, own_len: usize, threshold: usize) -> bool {
        ((own_len > 0 || !self.bodies.is_empty()) && !self.in_flight)
            || self.pending_len > threshold
            || own_len > threshold
    }
}

/// Drops some headers from the start (the earliest) of `headers` so that the compact encoding (D-075) of the rest does
/// not exceed `budget` bytes, returning how many were dropped. First estimates how many to drop from each header's
/// encoded length within the whole sequence, then verifies one by one: dropped headers can no longer be referenced, so
/// the remaining headers may become longer.
fn trim_start(headers: &[BlockHeader], budget: usize) -> Result<usize, NodeError> {
    let mut encoder = compact::HeaderEncoder::default();
    let mut lens = Vec::with_capacity(headers.len());
    for header in headers {
        lens.push(encoder.encode(header).map_err(encode_err)?.len());
    }
    let mut total = lens
        .iter()
        .fold(4usize, |sum, len| sum.saturating_add(*len));
    let mut start = 0usize;
    for len in &lens {
        if total <= budget {
            break;
        }
        total = total.saturating_sub(*len);
        start = start.saturating_add(1);
    }
    fits_from(headers, start, budget)
}

/// Moves `start` forward one at a time until the compact encoding of `headers[start..]` does not exceed `budget`.
fn fits_from(headers: &[BlockHeader], mut start: usize, budget: usize) -> Result<usize, NodeError> {
    loop {
        let rest = headers.get(start..).unwrap_or_default();
        if rest.is_empty() || compact::encoded_len(rest).map_err(encode_err)? <= budget {
            return Ok(start);
        }
        start = start.saturating_add(1);
    }
}

fn encode_err(e: std::io::Error) -> NodeError {
    NodeError::Internal(format!("block header encoding failed: {e}"))
}

/// Drops the earliest headers and bodies for the budget (D-075, D-077): the compact encoding of the remaining headers
/// does not exceed `header_budget` and the bodies in total do not exceed `body_budget`. `items` is by blue work from
/// lowest to highest: (header, body when it needs to be carried).
fn trim_carry(
    items: Vec<(BlockHeader, Option<Vec<Entry>>)>,
    header_budget: usize,
    body_budget: usize,
) -> Result<Carry, NodeError> {
    let headers: Vec<BlockHeader> = items.iter().map(|(h, _)| h.clone()).collect();
    let lens = items
        .iter()
        .map(|(_, body)| match body {
            Some(entries) => settlement::body_len(entries),
            None => Ok(0),
        })
        .collect::<Result<Vec<usize>, NodeError>>()?;
    let pending_len = lens
        .iter()
        .fold(0usize, |sum, len| sum.saturating_add(*len));
    let mut start = trim_start(&headers, header_budget)?;
    loop {
        let mut bodies: usize = lens
            .iter()
            .skip(start)
            .fold(0usize, |sum, len| sum.saturating_add(*len));
        while bodies > body_budget {
            bodies = bodies.saturating_sub(lens.get(start).copied().unwrap_or(0));
            start = start.saturating_add(1);
        }
        let fitted = fits_from(&headers, start, header_budget)?;
        if fitted == start {
            break;
        }
        start = fitted;
    }
    let mut carry = Carry {
        pending_len,
        ..Carry::default()
    };
    for ((header, body), len) in items.into_iter().zip(lens).skip(start) {
        if let Some(entries) = body {
            carry.bodies.push(CarriedBody {
                header: carry.headers.len(),
                entries,
                len,
            });
        }
        carry.headers.push(header);
    }
    Ok(carry)
}

/// Result of accepting a block (D-041, D-063).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Accepted {
    /// The main chain was extended to include it (no block was disconnected).
    Extended(Tip),
    /// The main chain was reorganized to include it.
    Reorganized(Tip),
    /// Stored in the DAG, not on the main chain for now (to be merged by other blocks later).
    Side,
    /// Already known block.
    Duplicate,
    /// Parents unknown; placed in the orphan pool.
    Orphan,
}

/// Result of [`Chain::accept_block`]: the outcome for that block, and the blocks the main chain accepted and
/// disconnected as a result (including the orphans processed afterwards).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AcceptReport {
    /// Outcome for that block.
    pub outcome: Accepted,
    /// Blocks newly accepted by the main chain (in execution order: each main-chain block is preceded by the blocks it
    /// merges).
    pub connected: Vec<Block>,
    /// Blocks no longer accepted by the main chain (the disconnected main-chain blocks and the blocks they merged; some
    /// of them may have been accepted again by the new main chain afterwards).
    pub disconnected: Vec<Block>,
}

/// Local information about a block in the DAG (the data attached to [`BlockDag`]). The Borsh encoding is only the local
/// format of pruning checkpoints (D-097).
#[derive(Clone, Debug, BorshSerialize, BorshDeserialize)]
pub struct BlockInfo {
    /// Block header.
    pub header: BlockHeader,
    /// Whether the body has to land on BTC (it contains entries other than forced inclusions, D-043, D-077).
    pub settles: bool,
    /// Whether it carries settlement transactions (D-077: it may, or a later settlement may carry its body).
    pub carries: bool,
}

type Dag = BlockDag<BlockInfo>;

/// Options for opening the chain (local, not consensus).
#[derive(Clone, Default)]
pub struct ChainOptions {
    /// Replaces the mining algorithm chosen by the network params (for testing other algorithms, D-062).
    pub algorithm: Option<Arc<dyn PowAlgorithm>>,
    /// Pruning (D-097).
    pub pruning: Pruning,
}

/// Where pruning stands (D-097).
#[derive(Clone, Copy, Debug, Default)]
struct PruneState {
    /// The root of the last pruning (hash and height): a BTC-final block every main chain this node can still switch
    /// to passes through. `None` before the first pruning.
    root: Option<(Hash, u64)>,
    /// State versions from this height on are kept even when older than `REORG_WINDOW` (when pruning is enabled): the
    /// height of the highest block on the main chain decided final at least the pruning margin of BTC blocks ago, the
    /// lowest fork point a main-chain switch can still have. `None` until there is one (the genesis block aside).
    retain: Option<u64>,
}

/// The pruning checkpoint (D-097): what a restart continues from instead of rebuilding the whole history. Local
/// format, written to [`CHECKPOINT_FILE`] at every pruning ([`ksana_btc::prune::write_checkpoint`]).
#[derive(BorshSerialize, BorshDeserialize)]
struct Checkpoint {
    /// [`CHECKPOINT_VERSION`].
    version: u32,
    /// The genesis block hash (a checkpoint of another chain is ignored).
    genesis: Hash,
    /// The pruned block DAG.
    dag: DagCheckpoint<BlockInfo>,
    /// The blocks the block store inserted into the DAG with this sequence number or later are replayed on top.
    dag_seq: u64,
    /// The pruning root (hash and height).
    root: (Hash, u64),
    /// The first height of the main chain's window.
    main_base: u64,
    /// The settled prefix of the main chain below the window.
    main_base_prefix: u32,
    /// The BTC snapshot.
    snapshot: SnapshotRecord,
    /// The header DAG.
    headers: HeaderDagRecord,
    /// The finality decision.
    finality: Finality,
}

/// What a restart restores from a pruning checkpoint besides the DAG (D-097).
struct Restored {
    root: (Hash, u64),
    main_base: u64,
    main_base_prefix: u32,
    snapshot: BtcSnapshot,
    finality: FinalityState,
}

/// The main chain below the in-memory window (D-097): answered from the block store's acceptance index.
struct StoreHistory(Arc<ChainStore>);

impl MainHistory for StoreHistory {
    fn accepted(&self, hash: &[u8; 32]) -> Result<Option<(u64, BlockSummary)>, String> {
        self.0.accepted(hash).map_err(|e| e.to_string())
    }
}

/// Known invalid blocks (and their descendants), at most [`MAX_INVALID`]: the earliest are forgotten first.
#[derive(Default)]
struct Invalid {
    blocks: BTreeSet<Hash>,
    order: VecDeque<Hash>,
}

impl Invalid {
    fn insert(&mut self, hash: Hash) {
        if !self.blocks.insert(hash) {
            return;
        }
        self.order.push_back(hash);
        while self.order.len() > MAX_INVALID {
            match self.order.pop_front() {
                Some(oldest) => {
                    self.blocks.remove(&oldest);
                }
                None => break,
            }
        }
    }

    fn contains(&self, hash: &Hash) -> bool {
        self.blocks.contains(hash)
    }
}

/// Refusals of blocks delivered while syncing (`now` is `None`), remembered under the BTC tip they were made at (local
/// policy, M27). A block refused for a reason that is not objective (its anchor cannot be used yet, a forced-inclusion
/// transaction is not on the current BTC chain, its settlement transactions do not check out) is not marked invalid,
/// so a peer can deliver it again, a sync response even hundreds of times, and each delivery would cost the proof of
/// work, GHOSTDAG, BTC lookups and signature checks again. A refusal that depends only on the header and the entries
/// the header commits to is remembered for the block, as every copy that gets that far gets the same answer; one that
/// depends on the settlement transactions of the copy is remembered for that block with those transactions only, so a
/// wrong copy never stands in the way of the right one. Everything is forgotten when the BTC tip changes, and each
/// refusal after [`REFUSAL_TTL`]: what bitcoind knows besides its best chain (a stale block, a transaction) can change
/// without a new tip. Which blocks are valid does not change. A block missing a parent refused for itself is refused
/// with it, for as long as the parent's refusal is remembered (D-104 item j, [`Chain::refused_with_parent`]).
#[derive(Default)]
struct Refusals {
    /// The BTC tip the remembered refusals were made at.
    tip: Option<(u32, Hash)>,
    /// [`RefusalKey`] → when the block was refused, and why.
    refused: BTreeMap<RefusalKey, (Instant, Refusal)>,
    order: VecDeque<RefusalKey>,
}

/// What a refusal is remembered for: the block hash, and the digest of the copy's settlement transactions for a refusal
/// of that copy only ([`copy_digest`]).
type RefusalKey = (Hash, Option<Hash>);

/// A remembered refusal: the kind of error and its message.
#[derive(Clone)]
enum Refusal {
    Rejected(String),
    Invalid(String),
    Btc(String),
}

impl Refusal {
    /// The refusals worth remembering: a copy refused ([`NodeError::Rejected`], [`NodeError::InvalidBlock`] without
    /// the block being marked invalid) or the BTC view not answering ([`NodeError::Btc`]).
    fn of(error: &NodeError) -> Option<Refusal> {
        match error {
            NodeError::Rejected(m) => Some(Refusal::Rejected(m.clone())),
            NodeError::InvalidBlock(m) => Some(Refusal::Invalid(m.clone())),
            NodeError::Btc(m) => Some(Refusal::Btc(m.clone())),
            _ => None,
        }
    }

    /// The error answered from memory: the same kind, the message marked with [`REMEMBERED`].
    fn error(&self) -> NodeError {
        match self {
            Refusal::Rejected(m) => NodeError::Rejected(format!("{m} {REMEMBERED}")),
            Refusal::Invalid(m) => NodeError::InvalidBlock(format!("{m} {REMEMBERED}")),
            Refusal::Btc(m) => NodeError::Btc(format!("{m} {REMEMBERED}")),
        }
    }
}

impl Refusals {
    /// The refusal remembered for `key` under the BTC tip `tip`, unless older than [`REFUSAL_TTL`] at `now`, with the
    /// time it was made.
    fn find(
        &self,
        tip: Option<(u32, Hash)>,
        key: &RefusalKey,
        now: Instant,
    ) -> Option<(Instant, &Refusal)> {
        if self.tip != tip {
            return None;
        }
        let (at, refusal) = self.refused.get(key)?;
        (now.saturating_duration_since(*at) <= REFUSAL_TTL).then_some((*at, refusal))
    }

    /// The refusal remembered for `key` under the BTC tip `tip`, unless older than [`REFUSAL_TTL`] at `now`.
    fn get(&self, tip: Option<(u32, Hash)>, key: &RefusalKey, now: Instant) -> Option<NodeError> {
        self.find(tip, key, now).map(|(_, refusal)| refusal.error())
    }

    /// Remembers a refusal made under the BTC tip `tip`; refusals made under another tip are forgotten.
    fn insert(
        &mut self,
        tip: Option<(u32, Hash)>,
        key: RefusalKey,
        now: Instant,
        refusal: Refusal,
    ) {
        if self.tip != tip {
            self.refused.clear();
            self.order.clear();
            self.tip = tip;
        }
        if self.refused.insert(key, (now, refusal)).is_none() {
            self.order.push_back(key);
        }
        while self.order.len() > MAX_REFUSALS {
            match self.order.pop_front() {
                Some(oldest) => {
                    self.refused.remove(&oldest);
                }
                None => break,
            }
        }
    }
}

/// Digest of a block copy's settlement transactions (or of their absence): with the block hash, which commits to the
/// header and, through `entries_root`, to the entries, it tells copies of a block apart ([`Refusals`]).
fn copy_digest(block: &Block) -> Result<Hash, NodeError> {
    let encoded = borsh::to_vec(&block.settlement).map_err(|e| {
        NodeError::Internal(format!("encoding the settlement transactions failed: {e}"))
    })?;
    Ok(sha256(&encoded))
}

/// Orphan pool: blocks with unknown parents, indexed by the missing parent; the earliest are dropped while it holds
/// more than [`MAX_ORPHANS`] blocks or [`MAX_ORPHAN_BYTES`], and those waiting for a block refused for now for itself
/// when that refusal is remembered (D-104 item j, [`Chain::remember`]).
#[derive(Default)]
struct Orphans {
    /// Orphan → the block and its [`orphan_size`].
    blocks: BTreeMap<Hash, (Block, usize)>,
    /// Missing parent → the orphans waiting for it.
    waiting: BTreeMap<Hash, BTreeSet<Hash>>,
    order: VecDeque<Hash>,
    /// The sum of the sizes of the blocks in the pool.
    bytes: usize,
}

/// The memory an orphan is counted for (M26): its encoding, which covers the heap data of its header, entries and
/// settlement transactions, plus the in-memory size of the block and of each decoded entry (the pool keeps a copy,
/// whose entry list has no spare capacity).
fn orphan_size(block: &Block) -> usize {
    borsh::object_length(block)
        .unwrap_or(usize::MAX)
        .saturating_add(
            block
                .entries
                .len()
                .saturating_mul(std::mem::size_of::<Entry>()),
        )
        .saturating_add(std::mem::size_of::<Block>())
}

/// The first rules GHOSTDAG applies to a block's parent list ([`BlockDag::compute`], same errors), which depend neither
/// on the DAG nor on the parents themselves (D-063): at most `MAX_BLOCK_PARENTS` parents, all distinct. Checking them
/// before anything else keeps a header that names any number of hashes from being walked parent by parent, or kept as
/// an orphan waiting for all of them (M26).
fn check_parent_list(params: &Params, parents: &[Hash]) -> Result<(), NodeError> {
    if parents.len() > params.max_block_parents {
        return Err(dag_err(GhostdagError::TooManyParents {
            count: parents.len(),
            max: params.max_block_parents,
        }));
    }
    let distinct: BTreeSet<&Hash> = parents.iter().collect();
    if distinct.len() != parents.len() {
        return Err(dag_err(GhostdagError::DuplicateParent));
    }
    Ok(())
}

impl Orphans {
    /// Keeps a copy of `block`, first dropping the earliest orphans until it fits within both limits. Returns `false`,
    /// keeping nothing, when the block alone exceeds [`MAX_ORPHAN_BYTES`].
    fn insert(&mut self, block: &Block, missing: &[Hash]) -> bool {
        let hash = block.header.hash();
        if self.blocks.contains_key(&hash) {
            return true;
        }
        let size = orphan_size(block);
        if size > MAX_ORPHAN_BYTES {
            return false;
        }
        while self.order.len() >= MAX_ORPHANS || self.bytes.saturating_add(size) > MAX_ORPHAN_BYTES
        {
            let Some(oldest) = self.order.pop_front() else {
                break;
            };
            self.remove(&oldest);
        }
        for parent in missing {
            self.waiting.entry(*parent).or_default().insert(hash);
        }
        self.blocks.insert(hash, (block.clone(), size));
        self.order.push_back(hash);
        self.bytes = self.bytes.saturating_add(size);
        true
    }

    fn remove(&mut self, hash: &Hash) -> Option<Block> {
        let (block, size) = self.blocks.remove(hash)?;
        self.bytes = self.bytes.saturating_sub(size);
        for parent in &block.header.parents {
            if let Some(set) = self.waiting.get_mut(parent) {
                set.remove(hash);
                if set.is_empty() {
                    self.waiting.remove(parent);
                }
            }
        }
        self.order.retain(|h| h != hash);
        Some(block)
    }

    /// Takes the orphans waiting for `parent` (they may still lack other parents, in which case they are put back).
    fn take_waiting_on(&mut self, parent: &Hash) -> Vec<Block> {
        let hashes = self.waiting.remove(parent).unwrap_or_default();
        hashes.iter().filter_map(|h| self.remove(h)).collect()
    }

    /// Drops the orphans waiting for `parent`, and those waiting for them in turn; returns how many were dropped.
    fn drop_waiting_on(&mut self, parent: &Hash) -> usize {
        let mut dropped = 0usize;
        let mut queue = vec![*parent];
        while let Some(parent) = queue.pop() {
            for block in self.take_waiting_on(&parent) {
                dropped = dropped.saturating_add(1);
                queue.push(block.header.hash());
            }
        }
        dropped
    }

    fn len(&self) -> usize {
        self.blocks.len()
    }

    fn contains(&self, hash: &Hash) -> bool {
        self.blocks.contains_key(hash)
    }

    /// Parents the orphans are missing.
    fn missing(&self) -> Vec<Hash> {
        self.waiting.keys().copied().collect()
    }
}

/// The main-chain tip and the recent information block production needs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tip {
    /// Block header.
    pub header: BlockHeader,
    /// Block hash.
    pub hash: Hash,
    /// Chain work (D-100): the blue work (GHOSTDAG, D-063) plus the tip's own work; fork choice goes by it.
    pub chain_work: U256,
    /// Timestamps of the tip and of up to `median_time_span` blocks before it on its selected chain (old → new), for
    /// the timestamp rule (§6.3).
    pub recent_timestamps: Vec<u64>,
}

/// The DAG part of a block template (D-063): parents (the main-chain tip, i.e. the selected parent, first, the rest in
/// ascending byte order), the new block's GHOSTDAG data and the blocks it merges.
#[derive(Clone, Debug)]
pub struct Template {
    /// The main-chain tip (the new block's selected parent).
    pub tip: Tip,
    /// Parents.
    pub parents: Vec<Hash>,
    /// GHOSTDAG data of the new block computed with weight 1 (non-empty block); for an empty block (weight by the
    /// streak, D-084, D-090) only the DAA score differs, see [`Template::daa_score`].
    pub ghostdag: GhostdagData,
    /// Empty-block streak of the main-chain tip (D-090): it determines the weight of the new block when empty, see
    /// [`Template::empty_weight`].
    pub tip_streak: u64,
    /// Blocks the new block merges (the merge set without the selected parent, in consensus order).
    pub merged: Vec<Block>,
    /// The highest BTC anchor among the parents (the new block's anchor cannot be lower; the forced-inclusion window
    /// starts from it).
    pub parent_anchor: BtcAnchor,
    /// Blocks this template does not merge because of a merge hazard ([`Chain::template`]), with the reason.
    pub skipped: Vec<(Hash, String)>,
}

impl Template {
    /// DAA score of the new block when its weight is `weight` (D-084): the selected parent's score plus the merge set's
    /// weight, plus this block's weight.
    pub fn daa_score(&self, weight: u64) -> u64 {
        self.ghostdag
            .daa_score
            .saturating_sub(1)
            .saturating_add(weight)
    }

    /// Weight of the new block when it is empty (D-084, D-090): by the empty-block streak of the main-chain tip (the
    /// selected parent) plus one.
    pub fn empty_weight(&self, params: &Params) -> u64 {
        params.empty_block_weight(self.tip_streak.saturating_add(1))
    }
}

/// Result of the block header rule check ([`Chain::check_dag_header`]).
struct CheckedHeader {
    /// GHOSTDAG data (the DAA score includes this block's weight).
    data: GhostdagData,
    /// This block's weight (D-084, D-090).
    weight: u64,
    /// This block's empty-block streak (D-090).
    streak: u64,
    /// The highest anchor among the parents.
    parent_anchor: BtcAnchor,
    /// Recent timestamps on the selected chain (for the timestamp rule).
    recent: Vec<u64>,
}

/// The past of a block from its parents ([`Chain::past_of`]).
#[derive(Default)]
struct PastOf {
    /// The highest main-chain block the parents' selected chains reach: the blocks the main chain accepted at or below
    /// this height are in the past.
    main_height: Option<u64>,
    /// The parents' selected-chain blocks above the main chain, and their merge sets.
    above: BTreeSet<Hash>,
}

impl PastOf {
    /// Whether a block in the DAG is in the past, `accepted` being the height of the main-chain block that accepted it
    /// ([`Chain::accepted_height`]).
    fn contains(&self, hash: &Hash, accepted: Option<u64>) -> bool {
        self.above.contains(hash) || accepted.is_some_and(|height| Some(height) <= self.main_height)
    }
}

struct Inner {
    state: StateDb,
    tip: Tip,
}

/// The BTC tip (height and block hash) a snapshot ends at; `None` before any BTC block is scanned.
type BtcTip = Option<(u32, [u8; 32])>;

/// Funding inputs kept reserved while transactions this node broadcast may still be in the BTC mempool
/// ([`Chain::linger`], D-104).
#[derive(Clone, Debug)]
struct Lingering {
    /// The transactions (released once none of them is in the mempool).
    txids: Vec<[u8; 32]>,
    /// The funding inputs they spend.
    inputs: Vec<([u8; 32], u32)>,
}

/// BTC finality (D-082): the DAG formed by the block headers that count on BTC (D-135), and the finalized chain decided
/// block by block by BTC height; along with the decoder of the bodies landed on BTC (for rebuilding missing blocks from
/// the data on BTC, D-093), and the verdicts the decision asks for (D-101).
struct FinalityState {
    headers: HeaderDag,
    finality: Finality,
    bodies: Bodies,
    /// The verdicts on blocks the decision asked about (D-101), each on its BTC basis ([`verdict::basis`]); also kept on
    /// disk ([`ChainStore::verdict`]), so a restart need not judge them again ([`Chain::judge_for_finality`]).
    verdicts: Verdicts,
    /// The blocks this process accepted while their anchor was on the BTC best chain, so with forced inclusion checked
    /// on their anchor chain (D-036). A main-chain block connected with its anchor on the best chain gets its verdict
    /// from the connection only when it and every block it merges are here: the connection then checked everything the
    /// verdict checks (D-101). In memory only.
    normal: BTreeSet<Hash>,
    /// The BTC-final tip as this node reports it (D-114, [`Finality::btc_final`]), computed after every decision;
    /// `None` before the first one. In memory only.
    reported: Option<BtcFinal>,
}

impl FinalityState {
    fn new(headers: HeaderDag, finality: Finality, bodies: Bodies) -> Self {
        FinalityState {
            headers,
            finality,
            bodies,
            verdicts: Verdicts::default(),
            normal: BTreeSet::new(),
            reported: None,
        }
    }
}

/// The chain.
pub struct Chain {
    params: Params,
    consensus: PowConsensus,
    exec: Executor,
    store: Arc<ChainStore>,
    /// The block cache shared by the block store and the state store (D-103).
    db_memory: DbMemory,
    genesis: BlockHeader,
    /// Write set of the genesis state: for rebuilding the state from the genesis block (D-093).
    genesis_writes: WriteSet,
    btc: BtcTracker,
    /// Block DAG: the valid blocks and their GHOSTDAG data (D-063); after pruning, the pruning point and its future
    /// (D-097).
    dag: RwLock<Dag>,
    /// Summaries of the blocks on the main chain from the window base up (D-097), for the settled prefix, the backlog
    /// limit, the unlockable segment and the carrier; below the window the block store answers.
    main: RwLock<Arc<MainChain>>,
    /// Settlement records not yet confirmed on BTC (block hash → record).
    open: Mutex<BTreeMap<Hash, SettlementRecord>>,
    /// Funding inputs reserved by rescue settlements and header republishing (D-045, D-047), excluded from the
    /// producer's funding together with those reserved by settlement records.
    extra_reserved: Mutex<BTreeSet<([u8; 32], u32)>>,
    /// Funding inputs of transactions this node broadcast for blocks that then left the main chain, with the IDs of
    /// those transactions: reserved while any of them may be in the BTC mempool ([`Chain::linger`], D-104).
    lingering: Mutex<Vec<Lingering>>,
    /// Reason of each settlement record's latest broadcast failure (the same failure is logged only once).
    broadcast_errors: Mutex<BTreeMap<Hash, String>>,
    /// Accepting blocks and committing this node's blocks are mutually exclusive (the main chain does not change
    /// between validation and connection).
    process: Mutex<()>,
    /// Known invalid blocks (and their descendants), not in the DAG.
    invalid: Mutex<Invalid>,
    /// Blocks in the DAG ineligible for the main chain (state_root mismatch or strict-mode failure, D-063).
    disqualified: Mutex<BTreeSet<Hash>>,
    /// Orphan pool.
    orphans: Mutex<Orphans>,
    /// The original main-chain tip retreated from during crash recovery ([`Chain::resume`]).
    resume: Mutex<Option<Hash>>,
    /// Block header versions unknown to this node that were already warned about (D-076, one warning per version).
    unknown_versions: Mutex<BTreeSet<u32>>,
    /// Refusals of blocks delivered while syncing (local policy, M27). A leaf lock.
    refusals: Mutex<Refusals>,
    /// BTC finality (D-082). Lock order: after `process`.
    finality: Mutex<FinalityState>,
    /// Block the finality decision is waiting for (this node does not have it yet): requested from peers.
    wanted: Mutex<Option<Hash>>,
    /// The block the finality decision last waited for a verdict on without one coming out (D-101), with the BTC tip
    /// and the main-chain tip it was tried at: not judged again until one of them changes. A leaf lock.
    unjudged: Mutex<Option<(Hash, BtcTip, Hash)>>,
    /// The BTC tip at which the main chain was last re-selected after a BTC sync ([`Chain::reselect`], D-104): not
    /// again until it changes. A leaf lock.
    reselected: Mutex<Option<(u32, [u8; 32])>>,
    /// Main-chain changes that happen outside [`Chain::accept_block`] (the finality decision re-selecting the main
    /// chain, crash recovery): the newly connected blocks and the disconnected blocks, kept until
    /// [`Chain::take_chain_changes`] takes them to maintain the entry pool (D-041).
    pending_changes: Mutex<(Vec<Block>, Vec<Block>)>,
    /// The verdicts on the native ops whose nonces the entry pool counts as taken by path C ops waiting for forced
    /// inclusion ([`NativeAuth`], D-104). A leaf lock.
    native_auth: Mutex<NativeAuth>,
    /// Pruning settings (D-097).
    pruning: Pruning,
    /// Where pruning stands (D-097). A leaf lock: no other lock is taken while holding it.
    prune: Mutex<PruneState>,
    /// The data directory (for the pruning checkpoint).
    data_dir: PathBuf,
    inner: RwLock<Inner>,
}

fn poisoned<T>(_: T) -> NodeError {
    NodeError::Internal("chain state lock poisoned".into())
}

fn dag_err(e: GhostdagError) -> NodeError {
    match e {
        GhostdagError::Corrupt(m) => {
            NodeError::Internal(format!("GHOSTDAG data inconsistent: {m}"))
        }
        // A block below the pruning point (D-097): this node can no longer process it, which does not make it invalid
        GhostdagError::Pruned(hash) => NodeError::Rejected(format!(
            "the computation needs block {}, which this node has pruned from memory (D-097)",
            hex::encode(hash)
        )),
        other => NodeError::InvalidBlock(other.to_string()),
    }
}

/// Summary of a block: `rewarded` is whether it gets a block reward (a main-chain block itself, or blue in the merge
/// set of the main-chain block that merges it, D-074, D-079).
fn summary(dag: &Dag, hash: &Hash, rewarded: bool) -> Result<BlockSummary, NodeError> {
    let block = dag
        .get(hash)
        .ok_or_else(|| NodeError::Internal("block missing from the DAG".into()))?;
    Ok(BlockSummary {
        hash: *hash,
        settles: block.payload.settles,
        anchor_height: block.payload.header.btc_anchor.height,
        producer: block.payload.header.producer,
        daa_score: block.data.daa_score,
        weight: block.weight,
        rewarded,
    })
}

/// Summary of main-chain block `hash` (including the blocks it merges, in consensus order, each with its coloring).
fn main_entry(dag: &Dag, hash: &Hash) -> Result<MainEntry, NodeError> {
    let own = summary(dag, hash, true)?;
    let data = dag
        .data(hash)
        .ok_or_else(|| NodeError::Internal("block missing from the DAG".into()))?;
    let mut merged = Vec::with_capacity(data.mergeset_ordered.len());
    for m in &data.mergeset_ordered {
        merged.push(summary(dag, m, data.is_merged_blue(m))?);
    }
    Ok(MainEntry {
        hash: own.hash,
        settles: own.settles,
        anchor_height: own.anchor_height,
        producer: own.producer,
        daa_score: own.daa_score,
        weight: own.weight,
        merged,
    })
}

/// Timestamps of `hash` and of up to `n` blocks before it on its selected chain (old → new).
fn chain_timestamps(dag: &Dag, hash: &Hash, n: usize) -> Vec<u64> {
    let mut out = Vec::new();
    let mut cursor = Some(*hash);
    while let Some(current) = cursor {
        if out.len() >= n {
            break;
        }
        let Some(block) = dag.get(&current) else {
            break;
        };
        out.push(block.payload.header.timestamp);
        cursor = block.data.selected_parent;
    }
    out.reverse();
    out
}

/// The first of block `hash` and the blocks it merges whose anchor is off the BTC best chain of `snapshot` (the BTC
/// block at that height is another one): the finality determination excludes such a block and every block whose
/// selected chain passes through it while that BTC chain stands (D-101 item c, `ksana_btc::finality`). A merged block
/// pruned from the DAG is not looked at (D-097: far below any block this is asked about).
fn unanchored(dag: &Dag, snapshot: &BtcSnapshot, hash: &Hash) -> Result<Option<Hash>, NodeError> {
    let block = dag
        .get(hash)
        .ok_or_else(|| NodeError::Internal("block missing from the DAG".into()))?;
    for member in std::iter::once(hash).chain(block.data.mergeset_ordered.iter()) {
        let Some(info) = dag.payload(member) else {
            continue;
        };
        if anchored_off(snapshot, &info.header.btc_anchor) {
            return Ok(Some(*member));
        }
    }
    Ok(None)
}

/// Whether `anchor` is off the BTC best chain of `snapshot`: the BTC block at its height is another one (an anchor
/// above the BTC tip, or below the heights the snapshot holds, is not, D-104).
fn anchored_off(snapshot: &BtcSnapshot, anchor: &BtcAnchor) -> bool {
    snapshot
        .hash_at(anchor.height)
        .is_some_and(|on_chain| on_chain != anchor.hash)
}

/// The blocks a template on the main-chain tip `tip` tries to merge, and `Status` carries (D-063, D-104 item k): the DAG
/// tips within the merge depth of `tip` (blue score plus `merge_depth` not below `tip_score`, its blue score: a block
/// further down can never be merged on it), and below the tips anchored, or merging a block anchored, off the BTC best
/// chain of `snapshot`, the first blocks that are not, when all their children are and they are not in the past of
/// `tip`. After a retreat, a parallel block that a block taken back had merged is such a block: no tip, the block taken
/// back being its child, and yet nothing keeps it from being merged again, with its ops ahead of the path C ops
/// included again (§7.4) and its block reward. `tip` itself is among them when the retreat leaves it with no other
/// child. The walk goes down from the tips anchored off the BTC best chain through such blocks only, so it costs next
/// to nothing while there is none.
fn frontier(
    dag: &Dag,
    snapshot: &BtcSnapshot,
    tip: &Hash,
    tip_score: u64,
    merge_depth: u64,
) -> Result<BTreeSet<Hash>, NodeError> {
    let within = |data: &GhostdagData| data.blue_score.saturating_add(merge_depth) >= tip_score;
    let mut out = BTreeSet::new();
    let mut seen = BTreeSet::new();
    let mut walk = Vec::new();
    for hash in dag.tips() {
        let block = dag
            .get(hash)
            .ok_or_else(|| NodeError::Internal("block missing from the DAG".into()))?;
        if !within(&block.data) {
            continue;
        }
        out.insert(*hash);
        if unanchored(dag, snapshot, hash)?.is_some() {
            seen.insert(*hash);
            walk.push(*hash);
        }
    }
    while let Some(hash) = walk.pop() {
        let Some(block) = dag.get(&hash) else {
            continue;
        };
        for parent in &block.parents {
            if !seen.insert(*parent) {
                continue;
            }
            // Pruned (D-097) or beyond the merge depth: no template can merge it
            let Some(below) = dag.get(parent).filter(|b| within(&b.data)) else {
                continue;
            };
            if unanchored(dag, snapshot, parent)?.is_some() {
                walk.push(*parent);
                continue;
            }
            let mut held = true;
            for child in &below.children {
                if !dag.contains(child) || unanchored(dag, snapshot, child)?.is_none() {
                    held = false;
                    break;
                }
            }
            if held && !dag.in_past(parent, tip).map_err(dag_err)? {
                out.insert(*parent);
            }
        }
    }
    Ok(out)
}

/// The highest BTC anchor among the parents (at equal height, the earlier parent's).
fn highest_anchor(dag: &Dag, parents: &[Hash]) -> Result<BtcAnchor, NodeError> {
    let mut best: Option<BtcAnchor> = None;
    for parent in parents {
        let anchor = &dag
            .payload(parent)
            .ok_or_else(|| NodeError::Internal("parent missing from the DAG".into()))?
            .header
            .btc_anchor;
        if best.as_ref().is_none_or(|b| anchor.height > b.height) {
            best = Some(anchor.clone());
        }
    }
    best.ok_or_else(|| NodeError::Internal("no parents".into()))
}

impl Chain {
    /// Opens the data directory, initializing it from the genesis config on first use. The genesis anchor block must be
    /// on bitcoind's best chain (D-035). The mining algorithm is chosen by the network params (D-062); pruning is on
    /// (D-097).
    pub fn open(
        data_dir: &Path,
        params: Params,
        genesis_file: &GenesisFile,
        btc: Arc<dyn BtcClient>,
    ) -> Result<Self, NodeError> {
        Self::open_with(data_dir, params, genesis_file, btc, ChainOptions::default())
    }

    /// Same as [`Chain::open`]; when `algorithm` is given it replaces the mining algorithm chosen by the network params
    /// (for testing other algorithms, D-062).
    pub fn open_with_algorithm(
        data_dir: &Path,
        params: Params,
        genesis_file: &GenesisFile,
        btc: Arc<dyn BtcClient>,
        algorithm: Option<Arc<dyn PowAlgorithm>>,
    ) -> Result<Self, NodeError> {
        let options = ChainOptions {
            algorithm,
            ..ChainOptions::default()
        };
        Self::open_with(data_dir, params, genesis_file, btc, options)
    }

    /// Same as [`Chain::open`], with the given options. With pruning on, a restart continues from the pruning
    /// checkpoint when there is one (D-097); without one, or when it cannot be used, the block DAG is rebuilt from the
    /// whole history in the block store and BTC is scanned again from the genesis anchor.
    pub fn open_with(
        data_dir: &Path,
        params: Params,
        genesis_file: &GenesisFile,
        btc: Arc<dyn BtcClient>,
        options: ChainOptions,
    ) -> Result<Self, NodeError> {
        params
            .check_upgrades()
            .map_err(|e| NodeError::Config(format!("upgrade table invalid (D-076): {e}")))?;
        std::fs::create_dir_all(data_dir)?;
        let db_memory = DbMemory::new();
        let mut state = StateDb::open_with(&data_dir.join("state"), &db_memory)?;
        let store = Arc::new(ChainStore::open(&data_dir.join("chain"), &db_memory)?);
        let fingerprint = genesis_file.fingerprint()?;
        // Before any write (D-096): a placeholder or an incomplete mainnet config must not leave half a data directory
        // behind
        genesis_file.check_network(&params)?;
        let genesis_writes = genesis_file.state_writes()?;

        let genesis_hash = match store.genesis()? {
            None => {
                if state.latest_version().is_some() {
                    return Err(NodeError::Config(
                        "the data directory's state store is not empty but its block store is"
                            .into(),
                    ));
                }
                // The genesis block is built before the genesis state is written, as the indexer does: a config it
                // cannot be built from (`bits` above the PoW limit, allocations above the supply) leaves nothing behind
                let root = state.preview_root(&genesis_writes)?;
                let header = genesis_file.header(&params, root)?;
                if state.commit(0, &genesis_writes)? != root {
                    return Err(NodeError::Storage(
                        "the root of the genesis state does not match the precomputed one".into(),
                    ));
                }
                let block = Block {
                    header,
                    entries: Vec::new(),
                    settlement: None,
                };
                store.put_genesis(&block, fingerprint)?;
                block.header.hash()
            }
            Some(hash) => {
                if store.config_fingerprint()? != Some(fingerprint) {
                    return Err(NodeError::Config(
                        "data directory belongs to a different genesis config".into(),
                    ));
                }
                hash
            }
        };
        let genesis = store
            .block(&genesis_hash)?
            .ok_or_else(|| NodeError::Storage("genesis block missing".into()))?
            .header;
        let consensus = match options.algorithm {
            Some(algorithm) => PowConsensus::with_algorithm(&params, &genesis, algorithm)?,
            None => PowConsensus::new(&params, &genesis)?,
        };
        let stored_tip = store
            .tip()?
            .ok_or_else(|| NodeError::Storage("main-chain tip missing".into()))?;
        let height = store
            .block(&stored_tip)?
            .ok_or_else(|| NodeError::Storage("block of the main-chain tip is missing".into()))?
            .header
            .height;

        // Continue from the pruning checkpoint (D-097)
        let mut restored = None;
        if options.pruning.enabled {
            if let Some(checkpoint) = read_checkpoint(data_dir, &genesis_hash) {
                match restore(&store, &params, &consensus, checkpoint) {
                    Ok(r) => restored = Some(r),
                    Err(e) => eprintln!(
                        "[ksana-node] alert: the pruning checkpoint cannot be used ({e}); rebuilding the block DAG from the \
                         whole history in the block store and scanning BTC again from the genesis anchor (D-097)"
                    ),
                }
            }
        }
        let consistent = find_consistent(&store, &state, height)?;
        if let Some((_, r)) = &restored {
            // The DAG restored from the checkpoint can only continue from a state at or above the pruning root, on a
            // main chain through it whose window the block store can supply
            let usable = match consistent {
                Some((version, _, _)) if version >= r.root.1 => {
                    window_complete(&store, &r.root, r.main_base, version)?
                }
                _ => false,
            };
            if !usable {
                eprintln!(
                    "[ksana-node] alert: the pruning checkpoint does not match the block store and the state (no state \
                     version at or above the pruning root at height {} matches the main chain, or the main chain no longer \
                     passes through it); rebuilding the block DAG from the whole history in the block store (D-097)",
                    r.root.1
                );
                restored = None;
            }
        }
        let (dag, restored) = match restored {
            Some((dag, r)) => (dag, Some(r)),
            None => (load_dag(&store, &params, &consensus, &genesis)?, None),
        };
        let (tip_hash, resume) = recover(
            &store,
            &mut state,
            &dag,
            consistent,
            (stored_tip, height),
            &genesis,
            &genesis_writes,
        )?;
        let tip = load_tip(&dag, &params, &tip_hash)?;
        let (base, base_prefix) = restored
            .as_ref()
            .map_or((0, 0), |r| (r.main_base, r.main_base_prefix));
        let history: Arc<dyn MainHistory> = Arc::new(StoreHistory(Arc::clone(&store)));
        let main =
            load_main(&store, &dag, base, base_prefix, tip.header.height)?.with_history(history);
        // The operation index (D-111) and the standards index (D-120), from the stored summaries completed above when
        // they do not describe the stored main chain (a data directory from a version without them, or a build that
        // did not finish); without them the listings answer with an error until the next start
        if !store.index_current()? {
            eprintln!(
                "[ksana-node] building the operation index and the standards index of the main chain up to height {} \
                 (D-111, D-120)",
                tip.header.height
            );
            match store.rebuild_index() {
                Ok(height) => eprintln!(
                    "[ksana-node] operation index and standards index built up to height {height}"
                ),
                Err(e) => eprintln!(
                    "[ksana-node] alert: the operation index and the standards index could not be built ({e}); \
                     ksana_getAccountOps, ksana_getLatestOps and the standards' methods answer with an error until \
                     they are built at the next start (D-111, D-120)"
                ),
            }
        }
        let open: BTreeMap<Hash, SettlementRecord> = store.settlements()?.into_iter().collect();
        let (tracker, finality, prune) = match restored {
            Some(r) => {
                eprintln!(
                    "[ksana-node] continuing from the pruning checkpoint: pruning root at height {}, {} blocks in memory, \
                     main-chain window from height {}, BTC snapshot from height {} (D-097)",
                    r.root.1,
                    dag.len(),
                    base,
                    r.snapshot.base()
                );
                (
                    BtcTracker::with_snapshot(btc, params.clone(), &genesis, r.snapshot)?,
                    r.finality,
                    PruneState {
                        root: Some(r.root),
                        retain: Some(r.root.1),
                    },
                )
            }
            None => (
                BtcTracker::new(btc, params.clone(), &genesis)?,
                FinalityState::new(
                    HeaderDag::new(&params, &consensus, genesis.clone())
                        .map_err(NodeError::Internal)?,
                    Finality::new(params.btc_finality_confs, genesis_hash),
                    Bodies::new(params.clone(), genesis_hash),
                ),
                PruneState::default(),
            ),
        };
        Ok(Chain {
            exec: Executor::new(params.clone())?,
            params,
            consensus,
            store,
            db_memory,
            genesis,
            genesis_writes,
            btc: tracker,
            dag: RwLock::new(dag),
            main: RwLock::new(Arc::new(main)),
            open: Mutex::new(open),
            extra_reserved: Mutex::new(BTreeSet::new()),
            lingering: Mutex::new(Vec::new()),
            broadcast_errors: Mutex::new(BTreeMap::new()),
            process: Mutex::new(()),
            invalid: Mutex::new(Invalid::default()),
            disqualified: Mutex::new(BTreeSet::new()),
            orphans: Mutex::new(Orphans::default()),
            resume: Mutex::new(resume),
            unknown_versions: Mutex::new(BTreeSet::new()),
            refusals: Mutex::new(Refusals::default()),
            finality: Mutex::new(finality),
            wanted: Mutex::new(None),
            unjudged: Mutex::new(None),
            reselected: Mutex::new(None),
            pending_changes: Mutex::new((Vec::new(), Vec::new())),
            native_auth: Mutex::new(NativeAuth::default()),
            pruning: options.pruning,
            prune: Mutex::new(prune),
            data_dir: data_dir.to_path_buf(),
            inner: RwLock::new(Inner { state, tip }),
        })
    }

    /// Second half of crash recovery (D-041): when the state lags the block store, [`Chain::open`] retreats the main
    /// chain to the height where the state is consistent, and here, after the BTC sync, the main chain is re-selected
    /// (the later blocks of the original main chain are re-executed). Does nothing when there is no block to recover;
    /// on a failure for local reasons (BTC not synced) it is kept and retried next time.
    pub fn resume(&self) -> Result<(), NodeError> {
        if self.resume.lock().map_err(poisoned)?.is_none() {
            return Ok(());
        }
        let _guard = self.process.lock().map_err(poisoned)?;
        let mut report = AcceptReport {
            outcome: Accepted::Duplicate,
            connected: Vec::new(),
            disconnected: Vec::new(),
        };
        let result = self.update_virtual(&mut report);
        if result.is_ok() {
            *self.resume.lock().map_err(poisoned)? = None;
        }
        self.stash_changes(report)?;
        result
    }

    /// Records main-chain changes that happen outside [`Chain::accept_block`] until [`Chain::take_chain_changes`] takes
    /// them.
    fn stash_changes(&self, report: AcceptReport) -> Result<(), NodeError> {
        if report.connected.is_empty() && report.disconnected.is_empty() {
            return Ok(());
        }
        let mut pending = self.pending_changes.lock().map_err(poisoned)?;
        pending.0.extend(report.connected);
        pending.1.extend(report.disconnected);
        Ok(())
    }

    /// Takes the main-chain changes caused by the BTC sync (the finality decision re-selecting the main chain, D-082)
    /// and by crash recovery (D-041): the newly connected blocks (in execution order) and the disconnected blocks. The
    /// caller uses them to maintain the entry pool and send new block notifications ([`crate::ops::chain_changed`]).
    pub fn take_chain_changes(&self) -> Result<(Vec<Block>, Vec<Block>), NodeError> {
        let mut pending = self.pending_changes.lock().map_err(poisoned)?;
        Ok(std::mem::take(&mut *pending))
    }

    /// Protocol params.
    pub fn params(&self) -> &Params {
        &self.params
    }

    /// Consensus.
    pub fn consensus(&self) -> &PowConsensus {
        &self.consensus
    }

    /// Executor.
    pub fn executor(&self) -> &Executor {
        &self.exec
    }

    /// Block store.
    pub fn store(&self) -> &ChainStore {
        &self.store
    }

    /// Genesis block header.
    pub fn genesis(&self) -> &BlockHeader {
        &self.genesis
    }

    /// BTC tracker.
    pub fn btc(&self) -> &BtcTracker {
        &self.btc
    }

    /// The verdicts on the native ops whose nonces the entry pool counts as taken by path C ops waiting for forced
    /// inclusion ([`NativeAuth`], D-104).
    pub(crate) fn native_auth(&self) -> Result<MutexGuard<'_, NativeAuth>, NodeError> {
        self.native_auth.lock().map_err(poisoned)
    }

    fn dag(&self) -> Result<RwLockReadGuard<'_, Dag>, NodeError> {
        self.dag.read().map_err(poisoned)
    }

    /// Number of blocks in the DAG (including the genesis block).
    pub fn dag_len(&self) -> Result<usize, NodeError> {
        Ok(self.dag()?.len())
    }

    /// Tips of the DAG (blocks without children).
    pub fn dag_tips(&self) -> Result<Vec<Hash>, NodeError> {
        Ok(self.dag()?.tips().iter().copied().collect())
    }

    /// Whether the block is in the DAG.
    pub fn contains(&self, hash: &Hash) -> Result<bool, NodeError> {
        Ok(self.dag()?.contains(hash))
    }

    /// GHOSTDAG data of a block in the DAG.
    pub fn ghostdag(&self, hash: &Hash) -> Result<Option<GhostdagData>, NodeError> {
        Ok(self.dag()?.data(hash).cloned())
    }

    /// Weight of a block in the DAG (D-084, D-090).
    pub fn weight_of(&self, hash: &Hash) -> Result<Option<u64>, NodeError> {
        Ok(self.dag()?.weight(hash))
    }

    /// Empty-block streak of a block in the DAG (D-090): it determines the weight of an empty block following it.
    pub fn streak_of(&self, hash: &Hash) -> Result<Option<u64>, NodeError> {
        Ok(self.dag()?.streak(hash))
    }

    /// Whether `x` is in the past of `y` (both must be in the DAG).
    pub fn in_past(&self, x: &Hash, y: &Hash) -> Result<bool, NodeError> {
        self.dag()?.in_past(x, y).map_err(dag_err)
    }

    /// Whether this node has the block: in the DAG, or stored and pruned from memory (D-097).
    pub fn knows(&self, hash: &Hash) -> Result<bool, NodeError> {
        Ok(self.contains(hash)? || self.store.dag_seq(hash)?.is_some())
    }

    /// The pruning point of the block DAG (D-097); `None` before the first pruning.
    pub fn pruning_point(&self) -> Result<Option<Hash>, NodeError> {
        Ok(self.dag()?.pruning_point().copied())
    }

    /// The root of the last pruning (hash and height, D-097); `None` before the first pruning.
    pub fn pruning_root(&self) -> Result<Option<(Hash, u64)>, NodeError> {
        Ok(self.prune.lock().map_err(poisoned)?.root)
    }

    /// The summaries of the main chain held in memory (D-097: from [`MainChain::base`] up, which is the genesis block
    /// until the first pruning).
    pub fn main_chain(&self) -> Result<Arc<MainChain>, NodeError> {
        self.main.read().map(|m| Arc::clone(&m)).map_err(poisoned)
    }

    /// The hash of the main-chain block at `height`: from the window, below it from the block store (D-097).
    pub fn main_hash_at(&self, height: u64) -> Result<Option<Hash>, NodeError> {
        let main = self.main_chain()?;
        if height >= main.base() {
            return Ok(main.get(height).map(|e| e.hash));
        }
        self.store.hash_at(height)
    }

    /// The summary of the main-chain block at `height` and of the blocks it merges: from the window, below it from the
    /// block store (D-097).
    pub fn main_entry_at(&self, height: u64) -> Result<Option<MainEntry>, NodeError> {
        let main = self.main_chain()?;
        if height >= main.base() {
            return Ok(main.get(height).cloned());
        }
        self.store.summary_at(height)
    }

    /// Height of the main-chain block that accepted this block (its own height for a block on the main chain, the
    /// height of the merging main-chain block for a merged block); `None` when not accepted by the main chain. Answered
    /// from the block store's acceptance index (D-097).
    pub fn accepted_height(&self, hash: &Hash) -> Result<Option<u64>, NodeError> {
        Ok(self.store.accepted(hash)?.map(|(height, _)| height))
    }

    /// The height of block `hash` when it is on the main chain (from the block store's indexes, also for blocks pruned
    /// from memory, D-097); `None` otherwise.
    pub fn main_height_of(&self, hash: &Hash) -> Result<Option<u64>, NodeError> {
        let Some((height, _)) = self.store.accepted(hash)? else {
            return Ok(None);
        };
        Ok((self.store.hash_at(height)? == Some(*hash)).then_some(height))
    }

    /// The block locator of the main chain (`/ksana/sync/2`): the hashes at [`ksana_p2p::wire::locator_heights`].
    pub fn locator(&self) -> Result<Vec<Hash>, NodeError> {
        let tip = self.main_chain()?.tip_height();
        let mut out = Vec::new();
        for height in ksana_p2p::wire::locator_heights(tip) {
            if let Some(hash) = self.main_hash_at(height)? {
                out.push(hash);
            }
        }
        Ok(out)
    }

    /// Syncs one round with bitcoind (D-035), advances the BTC finality decision (D-082), re-selects the main chain
    /// under the new BTC chain once the snapshot has caught up with bitcoind ([`Chain::reselect`], D-104), then
    /// rebuilds the blocks this node lacks from the data on BTC (D-093). Returns whether the BTC snapshot, the
    /// finalized tip, the main chain or the block DAG changed.
    pub fn sync_btc(&self) -> Result<bool, NodeError> {
        let changed = self.btc.sync(self)?;
        let finalized = self.update_finality()?;
        let reselected = !changed && self.reselect()?;
        let rebuilt = self.rebuild_missing_from_btc()?;
        let pruned = self.maybe_prune()?;
        Ok(changed || finalized || reselected || rebuilt || pruned)
    }

    /// Re-selects the main chain once per BTC tip, under the `process` lock (D-104): a BTC reorg may have taken the
    /// anchor of a main-chain block above the finalized tip off the best chain, and the main chain then retreats below
    /// it ([`Chain::retreat_from_unanchored`]), or put a better candidate back on it. Main-chain changes are recorded
    /// until [`Chain::take_chain_changes`] takes them. Returns whether the main chain changed; on a local fault the BTC
    /// tip is not recorded, and the next sync tries again.
    fn reselect(&self) -> Result<bool, NodeError> {
        let Some(btc_tip) = self.btc.snapshot()?.tip() else {
            return Ok(false);
        };
        if *self.reselected.lock().map_err(poisoned)? == Some(btc_tip) {
            return Ok(false);
        }
        let _guard = self.process.lock().map_err(poisoned)?;
        let mut report = AcceptReport {
            outcome: Accepted::Duplicate,
            connected: Vec::new(),
            disconnected: Vec::new(),
        };
        // On a local fault midway the main chain may already have changed: the report still goes to the entry pool
        // maintenance
        let result = self.update_virtual(&mut report);
        let changed = !report.connected.is_empty() || !report.disconnected.is_empty();
        self.stash_changes(report)?;
        result?;
        *self.reselected.lock().map_err(poisoned)? = Some(btc_tip);
        Ok(changed)
    }

    /// Prunes when the pruning point can advance (D-097, see the module docs), after a BTC sync. The pruning root R is
    /// the highest block decided BTC final at least the pruning margin of BTC blocks below the BTC tip, and must be on
    /// this node's main chain (its height becomes the lowest state version kept); the pruning point P is the block on
    /// R's selected chain `min_pruning_depth` blue score below it, pruned at only once it is at least the pruning step
    /// above the current one. Prunes the header DAG and the finality decision (R becomes its root), then the block DAG
    /// at P, the main chain's window and the BTC snapshot, and writes the pruning checkpoint. Returns whether it pruned.
    fn maybe_prune(&self) -> Result<bool, NodeError> {
        if !self.pruning.enabled {
            return Ok(false);
        }
        let snapshot = self.btc.snapshot()?;
        let Some((btc_tip, _)) = snapshot.tip() else {
            return Ok(false);
        };
        let Some(target) = btc_tip.checked_sub(self.pruning.btc_margin) else {
            return Ok(false);
        };
        let Some((root, root_height)) = self
            .finality
            .lock()
            .map_err(poisoned)?
            .finality
            .decided_by(target)
        else {
            return Ok(false);
        };
        if root_height == 0 {
            return Ok(false);
        }
        let _guard = self.process.lock().map_err(poisoned)?;
        if !self.on_main(&root, root_height)? {
            return Ok(false);
        }
        {
            let mut prune = self.prune.lock().map_err(poisoned)?;
            prune.retain = Some(prune.retain.map_or(root_height, |r| r.max(root_height)));
        }
        let (point, point_score, point_height, point_anchor) = {
            let dag = self.dag()?;
            if !dag.contains(&root) {
                return Ok(false);
            }
            let point = match dag.chain_floor(&root, dag.params().min_pruning_depth()) {
                Ok((_, point)) => point,
                // R moved down after a BTC reorg (within the margin): its chain reaches below the current pruning point
                Err(GhostdagError::Pruned(_)) => return Ok(false),
                Err(e) => return Err(dag_err(e)),
            };
            let score = |hash: &Hash| {
                dag.data(hash)
                    .map(|d| d.blue_score)
                    .ok_or_else(|| NodeError::Internal("block missing from the DAG".into()))
            };
            let point_score = score(&point)?;
            let current = match dag.pruning_point() {
                Some(current) => score(current)?,
                None => 0,
            };
            if point_score < current.saturating_add(self.pruning.step) {
                return Ok(false);
            }
            let header = &dag
                .payload(&point)
                .ok_or_else(|| NodeError::Internal("block missing from the DAG".into()))?
                .header;
            (point, point_score, header.height, header.btc_anchor.height)
        };
        // Every block in P's future anchors at or above P's anchor, so the forced-inclusion ranges, anchor times and
        // landings it needs are above this BTC height
        let cut = point_anchor.saturating_sub(self.params.native_confirmations);
        let snapshot = self.btc.snapshot()?;
        let main = self.main_chain()?;
        // The window keeps everything still asked about: the unlock segment of any state still executable (they all
        // descend from R's state, whose unlock cursor is the lowest), the main-chain blocks still in the DAG, and the
        // blocks whose landing below the cut would otherwise be lost
        let cursor_height = {
            let inner = self.read()?;
            let Ok(view) = inner.state.view_at(root_height) else {
                // The state at R's height was already discarded (R first appeared below REORG_WINDOW): wait for a
                // later R, whose state is kept
                return Ok(false);
            };
            unlock_cursor(&view)
                .map_err(NodeError::Storage)?
                .map_or(1, |(height, _)| height.max(1))
        };
        let settled = match settled_height(&snapshot, &main, cut) {
            Ok(settled) => settled,
            Err(e) => {
                eprintln!(
                    "[ksana-node] alert: cannot prune at block {}: {e}",
                    hex::encode(point)
                );
                return Ok(false);
            }
        };
        let base = main.base().max(
            settled
                .saturating_add(1)
                .min(cursor_height)
                .min(point_height),
        );

        // The header DAG and the finality decision first: when they cannot be pruned, nothing is
        let (headers_record, finality_record, header_hashes) = {
            let (fin, _) = self.finalized()?;
            let mut state = self.finality.lock().map_err(poisoned)?;
            let FinalityState {
                headers, finality, ..
            } = &mut *state;
            let pruned = headers
                .set_floor(fin, &snapshot)
                .and_then(|()| headers.prune(&point))
                .and_then(|_| finality.prune(headers, &root, cut));
            if let Err(e) = pruned {
                eprintln!(
                    "[ksana-node] alert: cannot prune the header DAG and the finality decision at block {}: {e} (D-097)",
                    hex::encode(point)
                );
                return Ok(false);
            }
            let header_hashes: BTreeSet<Hash> = headers.dag().iter().map(|(h, _)| *h).collect();
            (
                headers.record().map_err(NodeError::Internal)?,
                finality.clone(),
                header_hashes,
            )
        };
        let dropped = self
            .dag
            .write()
            .map_err(poisoned)?
            .prune(&point)
            .map_err(dag_err)?;
        let mut window = (*main).clone();
        drop(main);
        window.prune(base, &snapshot).map_err(NodeError::Internal)?;
        let window_hashes: BTreeSet<Hash> = window
            .entries()
            .iter()
            .flat_map(|e| e.accepted().map(|b| b.hash))
            .collect();
        let main_base_prefix = window.base_prefix();
        *self.main.write().map_err(poisoned)? = Arc::new(window);
        let keep = |hash: &[u8; 32]| header_hashes.contains(hash) || window_hashes.contains(hash);
        self.btc.prune(cut, &keep)?;
        {
            let dag = self.dag()?;
            self.disqualified
                .lock()
                .map_err(poisoned)?
                .retain(|h| dag.contains(h));
            // The verdicts and acceptance modes only of the blocks still kept (D-101)
            let mut state = self.finality.lock().map_err(poisoned)?;
            let FinalityState {
                headers,
                verdicts,
                normal,
                ..
            } = &mut *state;
            verdicts.retain(|h| dag.contains(h) || headers.dag().contains(h));
            normal.retain(|h| dag.contains(h));
        }
        self.prune.lock().map_err(poisoned)?.root = Some((root, root_height));
        let (dag_checkpoint, kept) = {
            let dag = self.dag()?;
            (dag.checkpoint().map_err(dag_err)?, dag.len())
        };
        let checkpoint = Checkpoint {
            version: CHECKPOINT_VERSION,
            genesis: self.genesis.hash(),
            dag: dag_checkpoint,
            dag_seq: self.store.next_seq()?,
            root: (root, root_height),
            main_base: base,
            main_base_prefix,
            snapshot: self.btc.snapshot()?.record(),
            headers: headers_record,
            finality: finality_record,
        };
        let written = write_checkpoint(&self.data_dir, &checkpoint);
        eprintln!(
            "[ksana-node] pruned at block {} (height {point_height}, blue score {point_score}; root at height {root_height}): \
             {dropped} blocks dropped from memory, {kept} kept; main-chain window from height {base}, BTC snapshot from \
             height {cut} (D-097)",
            hex::encode(point)
        );
        match written {
            Ok(bytes) => eprintln!("[ksana-node] pruning checkpoint written ({bytes} bytes)"),
            // The pruning itself stands; a restart then continues from the previous checkpoint, or the whole history
            Err(e) => {
                eprintln!("[ksana-node] alert: cannot write the pruning checkpoint: {e} (D-097)")
            }
        }
        match self.write_buffer_bytes() {
            Ok(bytes) => eprintln!(
                "[ksana-node] RocksDB memory: write buffers {} KiB, block cache {} KiB (D-103)",
                bytes / 1024,
                self.db_memory.block_cache_bytes() / 1024
            ),
            Err(e) => eprintln!("[ksana-node] cannot read RocksDB's memory use: {e} (D-103)"),
        }
        Ok(true)
    }

    /// The bytes RocksDB's write buffers hold in the state store and the block store (D-103).
    fn write_buffer_bytes(&self) -> Result<u64, NodeError> {
        let state = self.read()?.state.memtable_bytes()?;
        Ok(state.saturating_add(self.store.memtable_bytes()?))
    }

    /// Rebuilds the blocks this node lacks from the data on BTC (D-093): among the block the finality decision is
    /// waiting for and the parents the orphans are missing, those whose header counts on BTC (D-135) and whose body is
    /// known by the rules of [`ksana_btc::bodies`] (no body needed, or a payload carrying its body has landed) are
    /// assembled into blocks without settlement transactions (D-077: settlement transactions are not part
    /// of a block's identity) and accepted under all the rules for receiving historical blocks; when the parents are
    /// missing too, it becomes an orphan first and the parents are rebuilt next round. When peers cannot supply these
    /// blocks (they are all on another chain, or this node has no peers), this node can still follow the finalized
    /// chain on BTC; invalid blocks are recorded as invalid as usual ([`Chain::follow_finality`] warns when a finalized
    /// block is invalid). A block refused for now, for instance one anchored above the local BTC tip − `ANCHOR_DEPTH`
    /// (D-101), is tried again at the next BTC sync without failing this one. Returns whether new blocks were accepted.
    pub fn rebuild_missing_from_btc(&self) -> Result<bool, NodeError> {
        let mut accepted_any = false;
        let mut tried: BTreeSet<Hash> = BTreeSet::new();
        loop {
            let mut missing = self.missing_parents()?;
            {
                let orphans = self.orphans.lock().map_err(poisoned)?;
                missing.retain(|h| !tried.contains(h) && !orphans.contains(h));
            }
            let mut blocks: Vec<Block> = Vec::new();
            if !missing.is_empty() {
                let snapshot = self.btc.snapshot()?;
                let mut state = self.finality.lock().map_err(poisoned)?;
                let FinalityState {
                    headers, bodies, ..
                } = &mut *state;
                for hash in &missing {
                    tried.insert(*hash);
                    if self.is_invalid(hash)? {
                        continue;
                    }
                    let Some(header) = headers.header(hash).cloned() else {
                        continue;
                    };
                    let Some(body) = bodies.body(&snapshot, hash).map_err(NodeError::Internal)?
                    else {
                        continue;
                    };
                    blocks.push(Block {
                        header,
                        entries: (*body).clone(),
                        settlement: None,
                    });
                }
            }
            if blocks.is_empty() {
                return Ok(accepted_any);
            }
            for block in blocks {
                let hash = block.header.hash();
                let height = block.header.height;
                match self.accept_block(block, None) {
                    Ok(report) => {
                        match &report.outcome {
                            Accepted::Orphan | Accepted::Duplicate => {}
                            outcome => {
                                accepted_any = true;
                                eprintln!(
                                    "[ksana-node] rebuilt block {} (height {height}) from the data on BTC: {} (D-093)",
                                    hex::encode(hash),
                                    match outcome {
                                        Accepted::Extended(_) => "main chain extended",
                                        Accepted::Reorganized(_) => "main chain reorganized",
                                        _ => "side chain",
                                    }
                                );
                            }
                        }
                        self.stash_changes(report)?;
                    }
                    Err(NodeError::InvalidBlock(m)) => {
                        eprintln!(
                            "[ksana-node] alert: block {} (height {height}) rebuilt from the data on BTC is invalid: {m} (D-093)",
                            hex::encode(hash)
                        );
                    }
                    // Refused for now, like an anchor above the local BTC tip − ANCHOR_DEPTH (D-101): tried again
                    // at the next BTC sync instead of failing this one, which would skip pruning until BTC advances
                    Err(NodeError::Rejected(m) | NodeError::Btc(m)) => {
                        eprintln!(
                            "[ksana-node] block {} (height {height}) rebuilt from the data on BTC cannot be accepted for now, retrying at the next BTC sync: {m} (D-093)",
                            hex::encode(hash)
                        );
                    }
                    Err(e) => return Err(e),
                }
            }
        }
    }

    /// BTC block header time the timestamp bound is based on (D-083): the time of the block at the anchor height on the
    /// current BTC best chain (the anchor block itself when it is on the best chain; when it was reorged away, the
    /// block at the same height on the current chain, consistent with D-056), taken only from the snapshot without
    /// asking bitcoind, so it is a deterministic function of the BTC chain data. `None` when the snapshot does not have
    /// that height yet (the timestamp bound is not checked; a block whose anchor is unknown is handled by the anchor
    /// check).
    pub fn anchor_time(&self, snapshot: &BtcSnapshot, anchor: &BtcAnchor) -> Option<u32> {
        snapshot.header_time_at(anchor.height)
    }

    /// The verdict the finality decision asks for on block `hash` (D-101): the one recorded on the current BTC basis
    /// ([`Verdicts`], kept on disk across restarts), from the connection of the block as a main-chain block
    /// ([`Chain::connect_chain_block`]) or from judging it ([`Chain::judge_for_finality`]); undetermined without one. A
    /// node decides final only a block it has, so that its main chain can then pass through it: a valid verdict on a
    /// block this node lacks waits until the block is rebuilt from the data on BTC or supplied by a peer, while an
    /// invalid one excludes the block at once.
    fn validity(
        &self,
        headers: &HeaderDag,
        verdicts: &Verdicts,
        snapshot: &BtcSnapshot,
        hash: &Hash,
    ) -> Result<Validity, String> {
        let stored = |hash: &Hash| self.store.verdict(hash).map_err(|e| e.to_string());
        Ok(match verdicts.validity(headers, snapshot, hash, &stored)? {
            Validity::Valid
                if self
                    .store
                    .dag_seq(hash)
                    .map_err(|e| e.to_string())?
                    .is_none() =>
            {
                Validity::Undetermined
            }
            validity => validity,
        })
    }

    /// Advances the finality decision by the current BTC snapshot (D-082); when the finalized tip advances, the main
    /// chain is re-selected and its changes are recorded until [`Chain::take_chain_changes`] takes them. When the
    /// decision waits for the verdict on a block (D-101), the block is judged on top of the finalized tip
    /// ([`Chain::judge_for_finality`]) and the decision goes on, each block judged at most once per call. Returns
    /// whether the finalized tip advanced or a block was excluded.
    fn update_finality(&self) -> Result<bool, NodeError> {
        let mut changed = false;
        let mut judged = BTreeSet::new();
        loop {
            let outcome = self.evaluate_finality()?;
            changed |= outcome.advanced || !outcome.excluded.is_empty();
            if outcome.advanced {
                self.follow_finalized()?;
            }
            match outcome.validating {
                Some(hash) if judged.insert(hash) && self.judge_for_finality(&hash)? => {}
                _ => return Ok(changed),
            }
        }
    }

    /// Re-selects the main chain after the finalized tip advanced ([`Chain::follow_finality`]) under the `process` lock.
    fn follow_finalized(&self) -> Result<(), NodeError> {
        let _guard = self.process.lock().map_err(poisoned)?;
        let mut report = AcceptReport {
            outcome: Accepted::Duplicate,
            connected: Vec::new(),
            disconnected: Vec::new(),
        };
        // On a local fault midway through the switch the main chain may already have changed: the report still goes to
        // the entry pool maintenance
        let followed = self.follow_finality(&mut report);
        self.stash_changes(report)?;
        followed
    }

    /// The finality decision itself (does not change the main chain): decides by the current BTC snapshot, whose landing
    /// heights are those of the settled closure (D-038, D-077, D-135), and the recorded verdicts ([`Chain::validity`],
    /// D-101), records the block being waited for (one valid by the verdict that this node lacks) and logs. It runs on
    /// the BTC loop, on the p2p handler and when a block arrives; the snapshot is read under the `finality` lock, and
    /// snapshots are published in time order, so each decision sees a snapshot at least as new as the one before: an
    /// older one would look like a BTC reorg to [`Finality::evaluate`] and undo the decisions made at the newer heights.
    fn evaluate_finality(&self) -> Result<Outcome, NodeError> {
        let (outcome, tip, tip_height, reported, taken_back) = {
            let mut state = self.finality.lock().map_err(poisoned)?;
            let snapshot = self.btc.snapshot()?;
            let FinalityState {
                headers,
                finality,
                verdicts,
                reported,
                ..
            } = &mut *state;
            let headers: &HeaderDag = headers;
            let verdicts: &Verdicts = verdicts;
            let outcome = finality
                .evaluate(headers, &snapshot, &mut |hash| {
                    self.validity(headers, verdicts, &snapshot, hash)
                })
                .map_err(NodeError::Internal)?;
            let now = finality.btc_final(&snapshot, reported.as_ref());
            let before = reported
                .replace(now)
                .unwrap_or_else(|| BtcFinal::from(finality.root()));
            let taken_back = finality.taken_back(&before, &now);
            (
                outcome,
                finality.tip(),
                finality.tip_height(),
                (before, now),
                taken_back,
            )
        };
        let waiting = match outcome.validating {
            Some(hash) if self.store.dag_seq(&hash)?.is_none() => Some(hash),
            _ => None,
        };
        {
            let mut wanted = self.wanted.lock().map_err(poisoned)?;
            if waiting != *wanted {
                if let Some(hash) = waiting {
                    if self.is_invalid(&hash)? {
                        eprintln!(
                            "[ksana-node] the finality decision is waiting for block {}, which this node refused as invalid: it \
                             is judged from its data on BTC, and excluded if invalid (D-101)",
                            hex::encode(hash)
                        );
                    } else {
                        eprintln!(
                            "[ksana-node] the finality decision is waiting for block {} (this node does not have it yet); \
                             requesting it from peers or rebuilding it from the data on BTC (D-082, D-093)",
                            hex::encode(hash)
                        );
                    }
                }
                *wanted = waiting;
            }
        }
        for (hash, why) in &outcome.excluded {
            match why {
                Exclusion::Invalid => eprintln!(
                    "[ksana-node] alert: block {} met the BTC finality condition but is invalid: excluded from the finality \
                     decision together with every block whose selected chain passes through it (D-101)",
                    hex::encode(hash)
                ),
                Exclusion::Unanchored(member) if member == hash => eprintln!(
                    "[ksana-node] alert: block {} is anchored on a BTC block that is not on the BTC best chain: excluded \
                     from the finality decision together with every block whose selected chain passes through it (D-101)",
                    hex::encode(hash)
                ),
                Exclusion::Unanchored(member) => eprintln!(
                    "[ksana-node] alert: block {} merges block {}, which is anchored on a BTC block that is not on the BTC \
                     best chain: excluded from the finality decision together with every block whose selected chain \
                     passes through it (D-101)",
                    hex::encode(hash),
                    hex::encode(member)
                ),
            }
        }
        let (before, now) = reported;
        if taken_back {
            eprintln!(
                "[ksana-node] alert (highest level): a BTC reorg took back BTC-final block {} at height {}: it reached the \
                 BTC block that decided it, which had BTC_FINALITY_CONFS confirmations; the BTC-final tip is now block {} \
                 at height {} (D-114)",
                hex::encode(before.hash),
                before.height,
                hex::encode(now.hash),
                now.height
            );
        } else if outcome.rolled_back {
            eprintln!(
                "[ksana-node] a BTC reorg revoked finality decisions, taking back nothing reported as BTC-final; the decided \
                 tip is at height {tip_height} (D-082, D-114)"
            );
        }
        // a rollback is reported above
        if outcome.advanced && !outcome.rolled_back {
            eprintln!(
                "[ksana-node] decided final: block {} at height {tip_height}, BTC-final once the BTC block that decided it \
                 has BTC_FINALITY_CONFS confirmations (D-082, D-114)",
                hex::encode(tip)
            );
        }
        if (now.hash, now.height) != (before.hash, before.height) && !taken_back {
            eprintln!(
                "[ksana-node] BTC final: block {} at height {} (D-114)",
                hex::encode(now.hash),
                now.height
            );
        }
        Ok(outcome)
    }

    /// Judges block `hash`, on which the finality decision waits for a verdict (D-101): the data of its closure has
    /// landed with enough confirmations and its selected parent is the finalized tip. It is executed on top of the
    /// finalized tip's state like a main-chain block, without committing anything ([`verdict::judge`], the same as the
    /// indexer), from this node's copy of each block and, for a block this node lacks or refused, from its body
    /// determined from the data on BTC; the verdict is recorded on its BTC basis. Returns whether a verdict was
    /// recorded: not when one is recorded already (a valid block this node lacks waits for the block), nor when none
    /// can be given now (the main chain does not pass through the finalized tip yet, its state is gone, a body cannot
    /// be determined, a local fault, or a failure in anchor-invalid mode after a BTC reorg since the decision asked,
    /// D-056: the decision itself never asks about a block anchored off the BTC chain), which is tried again once the
    /// BTC tip or the main-chain tip changes.
    fn judge_for_finality(&self, hash: &Hash) -> Result<bool, NodeError> {
        let _guard = self.process.lock().map_err(poisoned)?;
        let snapshot = self.btc.snapshot()?;
        let main = self.main_chain()?;
        let key = (
            *hash,
            snapshot.tip(),
            main.tip().map(|e| e.hash).unwrap_or_default(),
        );
        if self.unjudged.lock().map_err(poisoned)?.as_ref() == Some(&key) {
            return Ok(false);
        }
        let (fin, fin_height) = self.finalized()?;
        let (header, basis, candidate) = {
            let mut state = self.finality.lock().map_err(poisoned)?;
            let FinalityState {
                headers,
                bodies,
                verdicts,
                ..
            } = &mut *state;
            let headers: &HeaderDag = headers;
            let header = headers.header(hash).cloned().ok_or_else(|| {
                NodeError::Internal(
                    "the block awaiting a verdict is missing from the header DAG".into(),
                )
            })?;
            let basis = verdict::basis(&snapshot, &header);
            if verdicts.get(hash, basis).is_some() {
                return Ok(false);
            }
            // The finalized tip moved meanwhile (a block that arrived advanced the decision): the next evaluation asks
            // about the right block
            if header.selected_parent() != Some(&fin) {
                return Ok(false);
            }
            let candidate = verdict::prepare(&self.params, headers, &snapshot, hash, &mut |h| {
                if let Some(block) = self.store.block(h).map_err(|e| e.to_string())? {
                    return Ok(Some(Arc::new(block.entries)));
                }
                bodies.body(&snapshot, h)
            })
            .map_err(NodeError::Internal)?;
            (header, basis, candidate)
        };
        let unjudged = |reason: &str| -> Result<bool, NodeError> {
            eprintln!(
                "[ksana-node] no verdict on block {} (height {}) for now: {reason}; the finality decision waits, judging \
                 again when the BTC tip or the main chain changes (D-101)",
                hex::encode(hash),
                header.height
            );
            *self.unjudged.lock().map_err(poisoned)? = Some(key);
            Ok(false)
        };
        if basis.is_none() {
            return unjudged("its anchor height is not in the BTC snapshot");
        }
        let Some(candidate) = candidate else {
            return unjudged("a body in its closure cannot be determined yet");
        };
        if main.get(fin_height).map(|e| e.hash) != Some(fin) {
            return unjudged("this node's main chain does not pass through the finalized tip yet");
        }
        // The read guard must be gone before a retreat, which takes the write lock
        let view = self.read()?.state.view_at(fin_height);
        let (main, view) = match view {
            Ok(view) => (main, view),
            Err(e) => {
                // The finalized tip's state was discarded: a BTC reorg deeper than BTC_FINALITY_CONFS undid decisions above
                // it (beyond the assumption of D-082). Retreat the main chain to the finalized tip, rebuilding its state
                // from the genesis block when needed (D-093), and judge on top of it
                drop(main);
                match self.retreat_to_finalized(fin, fin_height) {
                    Ok(()) => {}
                    Err(why) => {
                        return unjudged(&format!(
                            "the finalized tip's state: {e}; retreating the main chain to it failed: {why}"
                        ))
                    }
                }
                let main = self.main_chain()?;
                let view = self.read()?.state.view_at(fin_height);
                match view {
                    Ok(view) => (main, view),
                    Err(e) => return unjudged(&format!("the finalized tip's state: {e}")),
                }
            }
        };
        let mut main = main;
        if main.tip_height() != fin_height {
            Arc::make_mut(&mut main)
                .truncate(fin_height)
                .map_err(NodeError::Internal)?;
        }
        let judge = Judge {
            params: &self.params,
            exec: &self.exec,
            snapshot: &snapshot,
            client: self.btc.client(),
        };
        let verdict = verdict::judge(&judge, &candidate, main, Arc::new(view), &mut |writes| {
            self.read()
                .map_err(|e| e.to_string())?
                .state
                .preview_root_at(fin_height, writes)
                .map_err(|e| e.to_string())
        });
        let valid = match verdict {
            Verdict::Valid(_) => {
                if self.is_invalid(hash)? {
                    eprintln!(
                        "[ksana-node] alert (highest level): block {} (height {}) is valid on top of the finalized tip by the \
                         data on BTC, but this node refused it as invalid: this node's checks and the verdict disagree (a \
                         non-determinism bug); the finality decision waits for it (D-101)",
                        hex::encode(hash),
                        header.height
                    );
                }
                true
            }
            Verdict::BadBody(reason) | Verdict::Invalid(reason) => {
                eprintln!(
                    "[ksana-node] alert: block {} (height {}) meets the BTC finality condition but is invalid ({reason}): it \
                     comes from an attacker or a faulty producer, or the implementation has a non-determinism bug (D-101)",
                    hex::encode(hash),
                    header.height
                );
                false
            }
            Verdict::Unjudged(reason) => return unjudged(&reason),
        };
        self.record_verdict(hash, valid, basis)?;
        Ok(true)
    }

    /// Records the verdict on block `hash` given on `basis` (D-101), in memory and on disk (a cache: when it cannot be
    /// written, the block is judged again after a restart).
    fn record_verdict(
        &self,
        hash: &Hash,
        valid: bool,
        basis: Option<[u8; 32]>,
    ) -> Result<(), NodeError> {
        self.finality
            .lock()
            .map_err(poisoned)?
            .verdicts
            .record(*hash, valid, basis);
        if let Some(basis) = basis {
            if let Err(e) = self.store.put_verdict(hash, valid, &basis) {
                eprintln!(
                    "[ksana-node] cannot keep the verdict on block {} on disk: {e}",
                    hex::encode(hash)
                );
            }
        }
        Ok(())
    }

    /// Retreats the main chain to the finalized tip `fin` at `fin_height` whose state was discarded (see
    /// [`Chain::judge_for_finality`]; the caller holds the `process` lock): the state is rebuilt from the genesis block
    /// ([`Chain::rewind_to`], D-093), the disconnected blocks go to the entry pool maintenance, and the main chain is
    /// selected again, which reconnects whatever chain wins above the finalized tip. Not possible after pruning: the
    /// history needed is no longer in memory (D-097).
    fn retreat_to_finalized(&self, fin: Hash, fin_height: u64) -> Result<(), String> {
        if self.main_chain().map_err(|e| e.to_string())?.base() > 0
            || self
                .dag()
                .map_err(|e| e.to_string())?
                .pruning_point()
                .is_some()
        {
            return Err("this node has pruned its history (D-097); resync it".into());
        }
        eprintln!(
            "[ksana-node] alert: the state of the finalized tip {} (height {fin_height}) was discarded after a BTC reorg undid \
             finality decisions above it; retreating the main chain to it and rebuilding the state from the genesis block \
             (D-093, D-101)",
            hex::encode(fin)
        );
        let mut report = AcceptReport {
            outcome: Accepted::Duplicate,
            connected: Vec::new(),
            disconnected: Vec::new(),
        };
        let retreated = self.rewind_to(fin, fin_height).map(|disconnected| {
            report.disconnected.extend(disconnected);
        });
        let reselected = retreated.and_then(|()| self.update_virtual(&mut report));
        // The main chain may have changed even on a failure: the report still goes to the entry pool maintenance
        self.stash_changes(report).map_err(|e| e.to_string())?;
        reselected.map_err(|e| e.to_string())
    }

    /// After the finalized tip advanced (holding the `process` lock): re-selects the main chain so that it passes
    /// through the finalized block; alerts when that is not possible.
    fn follow_finality(&self, report: &mut AcceptReport) -> Result<(), NodeError> {
        self.update_virtual(report)?;
        let (tip, tip_height) = self.finalized()?;
        if !self.on_main(&tip, tip_height)? {
            eprintln!(
                "[ksana-node] alert (highest level): the BTC-finalized block {} (height {tip_height}) is not on this node's main \
                 chain and cannot be switched to for now (blocks missing or invalid); this node's blocks and settlements above \
                 it may be voided (D-082)",
                hex::encode(tip)
            );
        }
        Ok(())
    }

    /// The tip of the finality decision (hash and height, D-082): the main chain passes through it, and the verdicts,
    /// pruning and the states kept follow it; the genesis block when there is none yet. What is reported as BTC-final
    /// waits for the BTC block that decided it to get deep enough ([`Chain::btc_final`], D-114).
    pub fn finalized(&self) -> Result<(Hash, u64), NodeError> {
        let state = self.finality.lock().map_err(poisoned)?;
        Ok((state.finality.tip(), state.finality.tip_height()))
    }

    /// Whether block `hash` at height `height` is on the decided finalized chain (D-082; see [`Chain::finalized`]).
    pub fn is_final(&self, hash: &Hash, height: u64) -> Result<bool, NodeError> {
        let (decided, root_height) = {
            let state = self.finality.lock().map_err(poisoned)?;
            (
                state.finality.is_final(hash, height),
                state.finality.root().height,
            )
        };
        // Below the finality decision's root (pruned, D-097): every main chain this node can switch to passes through
        // the root, which is final, so the main-chain blocks below it are final
        Ok(decided || (height < root_height && self.on_main(hash, height)?))
    }

    /// The BTC-final tip as this node reports it (hash and height, D-114, [`Finality::btc_final`]): the highest
    /// finalized block whose decision was made at a BTC block with `BTC_FINALITY_CONFS` confirmations, at or below
    /// [`Chain::finalized`]; the root of the finality decision before the first decision (the genesis block, or after
    /// pruning a block decided at least the pruning margin of BTC blocks ago, D-097).
    pub fn btc_final(&self) -> Result<(Hash, u64), NodeError> {
        let state = self.finality.lock().map_err(poisoned)?;
        let root = state.finality.root();
        Ok(state
            .reported
            .map_or((root.hash, root.height), |r| (r.hash, r.height)))
    }

    /// Whether block `hash` at height `height` is BTC-final as this node reports it (D-114, see [`Chain::btc_final`]).
    pub fn is_btc_final(&self, hash: &Hash, height: u64) -> Result<bool, NodeError> {
        let (reported, root_height) = {
            let state = self.finality.lock().map_err(poisoned)?;
            let root = state.finality.root();
            let tip = state.reported.map_or(root.height, |r| r.height);
            (
                height <= tip && state.finality.is_final(hash, height),
                root.height,
            )
        };
        // Below the root, as for `is_final`: the root was decided at least the pruning margin of BTC blocks ago (D-097)
        Ok(reported || (height < root_height && self.on_main(hash, height)?))
    }

    /// How many BTC blocks must pass after a competing chain's block appears on BTC before it counts (D-091): a block
    /// parallel to this node's landing first is normal with multiple producers, and this node's next block carrying
    /// settlement transactions usually carries its own headers right away, so no republish is needed; republish only
    /// when there is still no carrier after one BTC block (typically an idle chain), with margin left before the other
    /// side reaches `BTC_FINALITY_CONFS` confirmations.
    pub const COMPETITOR_GRACE: u32 = 1;

    /// Competing chain on BTC (local policy, D-091): among the tips of the header DAG (the headers that appeared in BTC
    /// payloads, D-082), a block that first appeared on the current best chain at least [`Chain::COMPETITOR_GRACE`] and
    /// fewer than `within` BTC blocks ago, is not on this node's main chain, and whose fork-choice key
    /// `(chain_work, hash)` is below this node's main-chain tip yet above the highest connected block on this node's
    /// main chain (one whose header and all of its past's headers have landed, D-082) — someone put a chain onto BTC
    /// that is heavier than this node's chain already on BTC but lighter than this node's chain; if this node's chain
    /// does not land in time, the other side's blocks may reach `BTC_FINALITY_CONFS` confirmations first and be decided
    /// final. Returns the one with the largest key among them (hash and height). Blocks whose key is not below this
    /// node's main-chain tip do not count (this node will switch to them, or is waiting for them); nor do blocks not
    /// above this node's connected part (the finality candidate is the connected block with the largest key, so they
    /// can never be candidates); nor does what the finality decision can never finalize (D-104): a chain forking below
    /// the finalized tip, and on a tip's selected chain the lowest block anchored, or merging a block anchored, off the
    /// BTC best chain, or excluded by the decision, with everything above it. The blocks below such a block count in
    /// the tip's place: after a BTC reorg the settlement transactions of a chain this node retreated from confirm
    /// again on the new branch, and while its blocks anchored on the old branch can never be final, the blocks below
    /// them still race this node's chain.
    pub fn btc_competitor(
        &self,
        snapshot: &BtcSnapshot,
        within: u32,
    ) -> Result<Option<(Hash, u64)>, NodeError> {
        let Some((btc_tip, _)) = snapshot.tip() else {
            return Ok(None);
        };
        let ours = {
            let tip = self.tip()?;
            (tip.chain_work, tip.hash)
        };
        let main = self.main_chain()?;
        let on_main = |hash: &Hash, height: u64| main.get(height).is_some_and(|e| e.hash == *hash);
        let state = self.finality.lock().map_err(poisoned)?;
        // The highest connected block on this node's main chain (the genesis block counts as always on chain): this
        // node's chain already on BTC
        let on_chain = main
            .entries()
            .iter()
            .rev()
            .find(|entry| state.finality.connected_at(&entry.hash).is_some())
            .and_then(|entry| state.headers.dag().chain_key(&entry.hash).ok())
            .unwrap_or((U256::ZERO, [0; 32]));
        if on_chain >= ours {
            return Ok(None);
        }
        let headers = &state.headers;
        let dag = headers.dag();
        let fin_height = state.finality.tip_height();
        // The highest block of a tip's selected chain the finality decision can still finalize (D-104): none when the
        // chain forks below the finalized tip; below the lowest block above the finalized tip and off this node's main
        // chain that is anchored, or merges a block anchored, off the BTC best chain, or that the decision excluded
        let decidable = |tip: &Hash| {
            let mut candidate = Some(*tip);
            let mut cursor = *tip;
            while let Some(block) = dag.get(&cursor) {
                let height = block.payload.height;
                if height <= fin_height {
                    return candidate.filter(|_| state.finality.is_final(&cursor, height));
                }
                if on_main(&cursor, height) {
                    return candidate;
                }
                let parent = block.data.selected_parent;
                if headers.unanchored(snapshot, &cursor).is_some()
                    || state.finality.excluded_at(&cursor).is_some()
                {
                    candidate = parent;
                }
                match parent {
                    Some(sp) => cursor = sp,
                    None => return candidate,
                }
            }
            None
        };
        let best = dag
            .tips()
            .iter()
            .filter_map(|tip| {
                let candidate = decidable(tip)?;
                let age = btc_tip.saturating_sub(snapshot.counted_at(&candidate)?);
                if age < Self::COMPETITOR_GRACE || age >= within {
                    return None;
                }
                let block = dag.get(&candidate)?;
                let key = dag.chain_key(&candidate).ok()?;
                (key > on_chain && key < ours && !on_main(&candidate, block.payload.height))
                    .then_some((key, block.payload.height))
            })
            .max();
        Ok(best.map(|((_, hash), height)| (hash, height)))
    }

    /// BTC height up to which the finality decision has been made (D-082).
    pub fn finality_evaluated_height(&self) -> Result<Option<u32>, NodeError> {
        Ok(self
            .finality
            .lock()
            .map_err(poisoned)?
            .finality
            .evaluated_height())
    }

    /// Whether the block is at `height` on the main chain.
    fn on_main(&self, hash: &Hash, height: u64) -> Result<bool, NodeError> {
        Ok(self.main_hash_at(height)? == Some(*hash))
    }

    /// The block at height `height` on the selected chain of `from`; `None` when the chain does not reach that height
    /// in the DAG (after pruning, D-097: the chain goes below the pruning point first).
    fn chain_at(dag: &Dag, from: &Hash, height: u64) -> Result<Option<Hash>, NodeError> {
        let mut cursor = *from;
        loop {
            let Some(block) = dag.get(&cursor) else {
                if dag.pruning_point().is_some() {
                    return Ok(None);
                }
                return Err(NodeError::Internal("block missing from the DAG".into()));
            };
            if block.payload.header.height == height {
                return Ok(Some(cursor));
            }
            match block.data.selected_parent {
                Some(sp) if block.payload.header.height > height => cursor = sp,
                _ => return Ok(None),
            }
        }
    }

    /// The execution layer's BTC view relative to `anchor` (the parent is the current main-chain tip). With `history`
    /// true it is for syncing historical blocks: when the anchor block was reorged away on BTC, it enters
    /// anchor-invalid mode ([`AnchoredView::history`], D-056).
    pub fn btc_view(
        &self,
        anchor: BtcAnchor,
        snapshot: Arc<BtcSnapshot>,
        history: bool,
    ) -> Result<AnchoredView, NodeError> {
        let client = Arc::clone(self.btc.client());
        let main = self.main_chain()?;
        let params = self.params.clone();
        Ok(if history {
            AnchoredView::history(anchor, snapshot, client, main, params)
        } else {
            AnchoredView::new(anchor, snapshot, client, main, params)
        })
    }

    /// Settled prefix height of the main-chain block at height `height` (D-038, D-063); `None` also below the main
    /// chain's window (D-097, see [`prefix_height`]).
    pub fn prefix_height(
        &self,
        snapshot: &BtcSnapshot,
        height: u64,
    ) -> Result<Option<u32>, NodeError> {
        Ok(prefix_height(snapshot, &*self.main_chain()?, height))
    }

    /// Native transactions the block must forcibly include at its start (§7.4, D-036, D-063): the window starts at the
    /// highest anchor among the parents.
    pub fn forced_entries(
        &self,
        snapshot: &BtcSnapshot,
        parent_anchor: &BtcAnchor,
        anchor: &BtcAnchor,
    ) -> Vec<Entry> {
        crate::btc::forced_entries(&self.params, snapshot, parent_anchor, anchor)
    }

    /// Whether a new block merging `merged` and anchored at `anchor_height` triggers the settlement backlog limit
    /// (§6.4, D-046, D-063): the earliest block in the new block's past (the main chain and the blocks it merges, plus
    /// `merged`) whose settlement has timed out.
    pub fn backlog(
        &self,
        snapshot: &BtcSnapshot,
        merged: &[BlockSummary],
        anchor_height: u32,
    ) -> Result<Option<BlockSummary>, NodeError> {
        settlement_backlog(
            &self.params,
            snapshot,
            &*self.main_chain()?,
            merged,
            anchor_height,
        )
        .map_err(NodeError::Internal)
    }

    /// Anchor rules that depend on the local BTC view (§6.3, D-035, D-056). `parent_anchor_height` is the highest
    /// anchor height among the parents. Returns [`NodeError::Btc`] (retry later) when the anchor block is above the
    /// locally synced tip.
    ///
    /// - Accepting a new block (`now` given): the anchor block is on the local BTC best chain; the anchor height does
    ///   not exceed `max(parents' anchor height, tip − ANCHOR_DEPTH)`, otherwise the local BTC is considered behind
    ///   ([`NodeError::Btc`], retry later); it does not lag the tip by more than `MAX_ANCHOR_LAG + ANCHOR_DEPTH`.
    ///   Returns [`NodeError::Rejected`] when not satisfied.
    /// - Syncing historical blocks (`now` is `None`): the anchor height does not exceed `max(parents' anchor height,
    ///   tip − ANCHOR_DEPTH)` either ([`NodeError::Btc`], retry later; D-101: a BTC reorg of at most `ANCHOR_DEPTH`
    ///   blocks must not orphan the anchor of a block that can still join the main chain). The anchor block may no
    ///   longer be on the best chain (reorged away on BTC) and is then handled in anchor-invalid mode (D-056), but only
    ///   for stale blocks bitcoind knows, or anchors lagging the tip by more than `MAX_ANCHOR_LAG + ANCHOR_DEPTH`,
    ///   whose hash is not looked at; otherwise returns [`NodeError::Rejected`] (retry later). A freshly produced
    ///   block's anchor is usually close to the tip, but on an idle chain it can lag as far as its parent's, and then a
    ///   made-up hash passes here. A block taken here with its anchor off the BTC best chain stays a side block: the
    ///   finality determination excludes it and every block merging it or built on it (D-101 item c), so no template
    ///   merges it (`Chain::merge_hazard`) and the main-chain selection never takes it (`Chain::best_candidate`).
    pub fn check_anchor(
        &self,
        snapshot: &BtcSnapshot,
        parent_anchor_height: u32,
        header: &BlockHeader,
        now: Option<u64>,
    ) -> Result<(), NodeError> {
        let anchor = &header.btc_anchor;
        let (tip_height, _) = snapshot
            .tip()
            .ok_or_else(|| NodeError::Btc("not yet synced with bitcoind".into()))?;
        if anchor.height > tip_height {
            return Err(NodeError::Btc(format!(
                "anchor height {} is above the locally synced BTC tip {tip_height}",
                anchor.height
            )));
        }
        let max_lag = self
            .params
            .max_anchor_lag
            .saturating_add(self.params.anchor_depth);
        let highest = tip_height
            .saturating_sub(self.params.anchor_depth)
            .max(parent_anchor_height);
        if now.is_none() {
            // Not above what a new block may anchor on either (D-101): a BTC reorg of at most ANCHOR_DEPTH blocks must
            // not be able to orphan the anchor of a block that can still become part of the main chain
            if anchor.height > highest {
                return Err(NodeError::Btc(format!(
                    "anchor height {} is above the local BTC tip {tip_height} − ANCHOR_DEPTH {} (the local BTC is considered behind, retry later)",
                    anchor.height, self.params.anchor_depth
                )));
            }
            if snapshot.contains(anchor)
                || tip_height.saturating_sub(anchor.height) > max_lag
                || self.known_stale(anchor)?
            {
                return Ok(());
            }
            return Err(NodeError::Rejected(format!(
                "btc_anchor (height {}) is not on the local BTC best chain, is not a stale block bitcoind knows, and does not lag \
                 the tip {tip_height} by more than MAX_ANCHOR_LAG + ANCHOR_DEPTH: not handled in anchor-invalid mode for now (D-056), retry later",
                anchor.height
            )));
        }
        if !snapshot.contains(anchor) {
            return Err(NodeError::Rejected(
                "btc_anchor is not on the local BTC best chain".into(),
            ));
        }
        if anchor.height > highest {
            return Err(NodeError::Btc(format!(
                "anchor height {} is above the local BTC tip {tip_height} − ANCHOR_DEPTH {} (the local BTC is considered behind, retry later)",
                anchor.height, self.params.anchor_depth
            )));
        }
        if tip_height.saturating_sub(anchor.height) > max_lag {
            return Err(NodeError::Rejected(format!(
                "btc_anchor lags the local BTC tip by more than MAX_ANCHOR_LAG + ANCHOR_DEPTH ({} < {tip_height} − {max_lag})",
                anchor.height
            )));
        }
        Ok(())
    }

    /// Whether the anchor block is a stale block bitcoind knows: bitcoind has the header, the height matches, and it is
    /// not on the best chain (D-056).
    fn known_stale(&self, anchor: &BtcAnchor) -> Result<bool, NodeError> {
        Ok(self
            .btc
            .client()
            .header(&anchor.hash)
            .map_err(|e| NodeError::Btc(e.to_string()))?
            .is_some_and(|info| !info.in_best_chain && info.height == anchor.height))
    }

    /// Extra checks on the settlement transactions when accepting a new block (and before the producer commits its own
    /// block) (D-034, D-081): all inputs of the commit transaction are unspent in bitcoind's UTXO set, and
    /// `testmempoolaccept` accepts both transactions ([`test_settlement`]).
    pub fn check_settlement_now(&self, pair: &SettlementPair) -> Result<(), NodeError> {
        let client = self.btc.client();
        for input in &pair.commit.input {
            let utxo = client
                .utxo(&input.previous_output, false)
                .map_err(|e| NodeError::Btc(e.to_string()))?;
            if utxo.is_none() {
                return Err(NodeError::Rejected(format!(
                    "settlement transaction input {}:{} is spent or does not exist",
                    input.previous_output.txid, input.previous_output.vout
                )));
            }
        }
        match test_settlement(client.as_ref(), pair).map_err(|e| NodeError::Btc(e.to_string()))? {
            Ok(()) => Ok(()),
            Err(reason) => Err(NodeError::Rejected(format!(
                "testmempoolaccept rejected the settlement transactions: {reason}"
            ))),
        }
    }

    /// Unlock entries written when producing a block (§8.5, D-038, D-074, D-079): relative to `anchor`, the last
    /// position in the unlockable segment `segment` (given by [`Chain::block_env`]) whose acceptance height does not
    /// exceed the settled height — one entry unlocks cumulatively up to it. Empty when no position can be unlocked.
    pub fn unlock_entries(
        &self,
        segment: &[UnlockItem],
        snapshot: &BtcSnapshot,
        anchor: &BtcAnchor,
    ) -> Result<Vec<Entry>, NodeError> {
        if !snapshot.contains(anchor) {
            return Ok(Vec::new());
        }
        let main = self.main_chain()?;
        let settled =
            settled_height(snapshot, &main, anchor.height).map_err(NodeError::Internal)?;
        Ok(unlock_target(segment, settled)
            .into_iter()
            .map(|(height, index)| Entry::Unlock { height, index })
            .collect())
    }

    /// Unconfirmed settlement records (block hash → record).
    pub fn open_settlements(&self) -> Result<Vec<([u8; 32], SettlementRecord)>, NodeError> {
        Ok(self
            .open
            .lock()
            .map_err(poisoned)?
            .iter()
            .map(|(h, r)| (*h, r.clone()))
            .collect())
    }

    /// Funding inputs reserved by settlement transactions not yet confirmed (D-037), by rescue settlements, header
    /// republishing and CPFP children in flight, and by transactions of blocks that left the main chain while they may
    /// still be in the BTC mempool (D-104).
    pub fn reserved_inputs(&self) -> Result<BTreeSet<([u8; 32], u32)>, NodeError> {
        let mut reserved: BTreeSet<([u8; 32], u32)> = self
            .open
            .lock()
            .map_err(poisoned)?
            .values()
            .flat_map(|r| r.funding_inputs.iter().copied())
            .collect();
        reserved.extend(
            self.extra_reserved
                .lock()
                .map_err(poisoned)?
                .iter()
                .copied(),
        );
        reserved.extend(
            self.lingering
                .lock()
                .map_err(poisoned)?
                .iter()
                .flat_map(|l| l.inputs.iter().copied()),
        );
        Ok(reserved)
    }

    /// Atomically reserves funding inputs: when any of them is already reserved (an open settlement transaction, a
    /// rescue settlement or republish transaction in flight, a block template being solved), reserves nothing and
    /// returns false. Called by the block production loop and the rescue thread after each has chosen its funding and
    /// before committing the block or broadcasting, guaranteeing that two of this node's transactions in flight at the
    /// same time never spend the same funding output (D-045, D-047).
    pub fn try_reserve_inputs(&self, inputs: &[([u8; 32], u32)]) -> Result<bool, NodeError> {
        // Same lock order as reserved_inputs: open first, then extra_reserved, then lingering
        let open = self.open.lock().map_err(poisoned)?;
        let mut reserved = self.extra_reserved.lock().map_err(poisoned)?;
        let lingering = self.lingering.lock().map_err(poisoned)?;
        let taken = inputs.iter().any(|input| {
            reserved.contains(input)
                || open.values().any(|r| r.funding_inputs.contains(input))
                || lingering.iter().any(|l| l.inputs.contains(input))
        });
        if !taken {
            reserved.extend(inputs.iter().copied());
        }
        Ok(!taken)
    }

    /// Releases the funding inputs reserved by [`Chain::try_reserve_inputs`].
    pub fn release_inputs(&self, inputs: &[([u8; 32], u32)]) -> Result<(), NodeError> {
        let mut reserved = self.extra_reserved.lock().map_err(poisoned)?;
        for input in inputs {
            reserved.remove(input);
        }
        Ok(())
    }

    /// Keeps the funding inputs of transactions `txids` this node broadcast reserved while any of them may be in the
    /// BTC mempool, after what they were sent for no longer needs them (D-104): the block left the main chain, or its
    /// data landed by another transaction. A BTC reorg puts the transactions of the blocks it orphaned back into the
    /// mempool, where spending the same inputs again would conflict with them. The inputs move out of those reserved
    /// by [`Chain::try_reserve_inputs`] at once, and [`Chain::process_settlements`] releases them once none of the
    /// transactions is in the mempool (confirmed, which spends the inputs, or gone).
    pub fn linger(
        &self,
        txids: Vec<[u8; 32]>,
        inputs: &[([u8; 32], u32)],
    ) -> Result<(), NodeError> {
        let mut reserved = self.extra_reserved.lock().map_err(poisoned)?;
        let mut lingering = self.lingering.lock().map_err(poisoned)?;
        for input in inputs {
            reserved.remove(input);
        }
        lingering.push(Lingering {
            txids,
            inputs: inputs.to_vec(),
        });
        Ok(())
    }

    /// Releases the lingering funding inputs ([`Chain::linger`]) of transactions none of which is in the BTC mempool.
    fn release_lingering(&self) -> Result<(), NodeError> {
        let lingering = self.lingering.lock().map_err(poisoned)?.clone();
        if lingering.is_empty() {
            return Ok(());
        }
        let client = self.btc.client();
        let mut gone = Vec::new();
        for entry in &lingering {
            let mut waiting = false;
            for txid in &entry.txids {
                if client
                    .in_mempool(txid)
                    .map_err(|e| NodeError::Btc(e.to_string()))?
                {
                    waiting = true;
                    break;
                }
            }
            if !waiting {
                gone.push(entry.txids.clone());
            }
        }
        self.lingering
            .lock()
            .map_err(poisoned)?
            .retain(|l| !gone.contains(&l.txids));
        Ok(())
    }

    /// Delayed broadcast and rebroadcast of settlement transactions (D-037, D-063): broadcast when the main-chain block
    /// that accepted the block has `SETTLEMENT_DEPTH` confirmations; rebroadcast those already broadcast but neither in
    /// the mempool nor confirmed (e.g. evicted after a BTC reorg); close the record once all the data the settlement
    /// carries has landed with `BTC_FINALITY_CONFS` confirmations. A settlement that confirmed and counted without
    /// landing it (D-135) keeps its record open for the rescuer, which sends its payload again. First releases the
    /// lingering funding inputs of transactions no longer in the mempool ([`Chain::linger`], D-104).
    pub fn process_settlements(&self) -> Result<(), NodeError> {
        self.release_lingering()?;
        let snapshot = self.btc.snapshot()?;
        let btc_tip = snapshot.tip().map(|(h, _)| h).unwrap_or(0);
        let tip_height = self.tip()?.header.height;
        let client = self.btc.client();
        let records = self.open_settlements()?;
        for (block_hash, mut record) in records {
            if self.accepted_height(&block_hash)? != Some(record.height) {
                // The block is no longer accepted by the main chain (e.g. cleanup unfinished before a crash): stop
                // broadcasting
                self.open.lock().map_err(poisoned)?.remove(&block_hash);
                continue;
            }
            let landed = match self.store.block(&block_hash)? {
                Some(block) => self.carried_on_chain(&block, &snapshot)?,
                None => snapshot.ksn1_height(&record.txid),
            };
            if let Some(height) = landed {
                let confs = btc_tip.saturating_sub(height).saturating_add(1);
                if confs >= self.params.btc_finality_confs {
                    self.open.lock().map_err(poisoned)?.remove(&block_hash);
                }
                continue;
            }
            // Confirmed and counted, but a header it needs from outside its payload had not counted, so it landed
            // nothing: not broadcast again, the rescuer sends its payload again once those headers have counted (D-135)
            if snapshot.ksn1_height(&record.txid).is_some() {
                continue;
            }
            if tip_height < record.height.saturating_add(self.params.settlement_depth) {
                continue;
            }
            if record.broadcast
                && client
                    .in_mempool(&record.txid)
                    .map_err(|e| NodeError::Btc(e.to_string()))?
            {
                continue;
            }
            let Some(block) = self.store.block(&block_hash)? else {
                continue;
            };
            let Some(txs) = &block.settlement else {
                continue;
            };
            let pair = settlement::decode(txs)?;
            match client
                .broadcast_all(&pair.txs())
                .map_err(|e| NodeError::Btc(e.to_string()))?
            {
                Ok(()) => {
                    if !record.broadcast {
                        eprintln!(
                            "[ksana-node] broadcast settlement transactions height={} block={} commit txid={} reveal txid={}",
                            record.height,
                            hex::encode(block_hash),
                            pair.commit.compute_txid(),
                            pair.reveal.compute_txid()
                        );
                        record.broadcast = true;
                        self.store.put_settlement(&block_hash, &record)?;
                        if let Some(open) = self.open.lock().map_err(poisoned)?.get_mut(&block_hash)
                        {
                            open.broadcast = true;
                        }
                    }
                }
                Err(reason) => {
                    let mut errors = self.broadcast_errors.lock().map_err(poisoned)?;
                    if errors.get(&block_hash) != Some(&reason) {
                        eprintln!(
                            "[ksana-node] alert: broadcasting the settlement transactions (height {}) failed: {reason}",
                            record.height
                        );
                        errors.insert(block_hash, reason);
                    }
                }
            }
        }
        // The failure reasons of records no longer open are not needed (memory stays bounded, D-097)
        let open: BTreeSet<Hash> = self
            .open
            .lock()
            .map_err(poisoned)?
            .keys()
            .copied()
            .collect();
        self.broadcast_errors
            .lock()
            .map_err(poisoned)?
            .retain(|hash, _| open.contains(hash));
        Ok(())
    }

    /// Whether all the data carried by the block's settlement transactions has landed (D-077): this block's data, the
    /// other headers and the bodies of the blocks in `bodies`. Returns the highest landing height among them when all
    /// have landed; `None` when the block carries no settlement transactions.
    pub fn carried_on_chain(
        &self,
        block: &Block,
        snapshot: &BtcSnapshot,
    ) -> Result<Option<u32>, NodeError> {
        let Some(txs) = &block.settlement else {
            return Ok(None);
        };
        let pair = settlement::decode(txs)?;
        let payload = settlement::attached_payload(&self.params, &pair).ok_or_else(|| {
            NodeError::Internal("accepted settlement transactions have no KSN1 envelope".into())
        })?;
        let Payload::Settlement {
            headers, bodies, ..
        } = Payload::decode(&payload, &self.params).map_err(|e| {
            NodeError::Internal(format!(
                "accepted settlement payload cannot be decoded: {e}"
            ))
        })?
        else {
            return Err(NodeError::Internal(
                "the payload of the accepted settlement transactions is not a settlement payload"
                    .into(),
            ));
        };
        let mut highest = match snapshot.block_data_height(&block.header.hash()) {
            Some(h) => h,
            None => return Ok(None),
        };
        for header in &headers {
            match snapshot.counted_at(&header.hash()) {
                Some(h) => highest = highest.max(h),
                None => return Ok(None),
            }
        }
        for body in &bodies {
            let hash = usize::try_from(body.header)
                .ok()
                .and_then(|i| headers.get(i))
                .map(BlockHeader::hash)
                .ok_or_else(|| {
                    NodeError::Internal("invalid body index in the settlement payload".into())
                })?;
            match snapshot.settlement_of(&hash) {
                Some((_, h)) => highest = highest.max(h),
                None => return Ok(None),
            }
        }
        Ok(Some(highest))
    }

    /// Whether the block carries settlement transactions (D-077; a block in the DAG).
    pub fn carries_settlement(&self, hash: &Hash) -> Result<bool, NodeError> {
        Ok(self.dag()?.payload(hash).is_some_and(|info| info.carries))
    }

    /// Whether `x` is `y` itself or in the past of `y` (D-063; both must be in the DAG).
    pub fn in_closed_past(&self, x: &Hash, y: &Hash) -> Result<bool, NodeError> {
        self.dag()?.in_closed_past(x, y).map_err(dag_err)
    }

    /// Which block's settlement payload carries the body of block `hash`, which carries no settlement transactions
    /// (D-077): in the main chain's acceptance order, starting from the main-chain block that accepted it, look for a
    /// block carrying settlement transactions whose payload `bodies` contains that block, examining at most
    /// [`PAYLOAD_HEADER_SEARCH`] main-chain blocks. `None` when not found.
    pub fn body_carrier(&self, hash: &Hash) -> Result<Option<Block>, NodeError> {
        let Some(accepted) = self.accepted_height(hash)? else {
            return Ok(None);
        };
        let main = self.main_chain()?;
        if accepted < main.base() {
            // Accepted below the main chain's window (D-097): its body landed long ago
            return Ok(None);
        }
        for (_, entry) in main.iter_from(accepted).take(PAYLOAD_HEADER_SEARCH) {
            for candidate in entry.accepted() {
                if candidate.hash == *hash || !self.carries_settlement(&candidate.hash)? {
                    continue;
                }
                let Some(block) = self.store.block(&candidate.hash)? else {
                    continue;
                };
                let Some(txs) = &block.settlement else {
                    continue;
                };
                let pair = settlement::decode(txs)?;
                let Some(payload) = settlement::attached_payload(&self.params, &pair) else {
                    continue;
                };
                let Ok(decoded) = Payload::decode(&payload, &self.params) else {
                    continue;
                };
                if bodies_in(&decoded).iter().any(|(h, _)| h == hash) {
                    return Ok(Some(block));
                }
            }
        }
        Ok(None)
    }

    fn read(&self) -> Result<RwLockReadGuard<'_, Inner>, NodeError> {
        self.inner.read().map_err(poisoned)
    }

    fn write(&self) -> Result<RwLockWriteGuard<'_, Inner>, NodeError> {
        self.inner.write().map_err(poisoned)
    }

    /// The main-chain tip.
    pub fn tip(&self) -> Result<Tip, NodeError> {
        Ok(self.read()?.tip.clone())
    }

    /// The main-chain tip and a read-only view of its state (a consistent snapshot of the same moment).
    pub fn tip_and_view(&self) -> Result<(Tip, StateView), NodeError> {
        let inner = self.read()?;
        Ok((inner.tip.clone(), inner.state.view()?))
    }

    /// Accesses the state store and the tip under the read lock (for RPC queries; no new block is committed meanwhile).
    pub fn with_state<R>(&self, f: impl FnOnce(&StateDb, &Tip) -> R) -> Result<R, NodeError> {
        let inner = self.read()?;
        Ok(f(&inner.state, &inner.tip))
    }

    /// The state root after committing `writes` on top of tip `tip_hash` (without writing).
    pub fn preview_root(
        &self,
        tip_hash: &[u8; 32],
        writes: &WriteSet,
    ) -> Result<[u8; 32], NodeError> {
        let inner = self.read()?;
        if inner.tip.hash != *tip_hash {
            return Err(NodeError::TipChanged);
        }
        Ok(inner.state.preview_root(writes)?)
    }

    /// Block context for executing a main-chain block: the block's selected parent must be the current main-chain tip
    /// (otherwise returns [`NodeError::TipChanged`]) and `parent` is its state; `daa_score` comes from the block's
    /// GHOSTDAG data and determines the applicable rules (D-076). The unlockable segment is computed from the current
    /// main chain and the parent state's unlock cursor ([`unlock_segment`], D-079), the same as the indexer.
    pub fn block_env(
        &self,
        header: &BlockHeader,
        daa_score: u64,
        parent: &dyn StateSource,
    ) -> Result<BlockEnv, NodeError> {
        let main = self.main_chain()?;
        if main.tip().map(|e| &e.hash) != header.selected_parent() {
            return Err(NodeError::TipChanged);
        }
        let cursor = unlock_cursor(parent).map_err(NodeError::Storage)?;
        Ok(BlockEnv {
            height: header.height,
            btc_anchor: header.btc_anchor.clone(),
            producer: header.producer,
            timestamp: header.timestamp,
            rules: self.params.rules(daa_score),
            unlockable: unlock_segment(&main, cursor, self.params.max_unlock_batch)
                .map_err(NodeError::Internal)?,
        })
    }

    /// The DAG part of a block template (D-063): the main-chain tip is the selected parent, then the other tips, and the
    /// blocks below the tips anchored off the BTC best chain whose children all are (a parallel block a retreat left
    /// behind, D-104 item k, see `frontier`), are added by fork-choice key from highest to lowest as long as the merge
    /// limits allow, the number of parents stays within the limit, and the selected parent does not change (a tip
    /// ineligible for the main chain may have more chain work and must not become the selected parent). A block beyond
    /// the merge depth of the main-chain tip is not tried, and the trials share one merge-depth root (D-097), so that
    /// tips nobody can merge cost a template next to nothing (second audit, P4a-2).
    ///
    /// Which tips a block merges is the producer's choice (local policy, M26): a tip is not added when it would bring
    /// into the merge set a block in `unmerged` (the producer found that executing it fails, see the block production
    /// loop) or a block with a merge hazard by the BTC chain `snapshot` (see `Chain::merge_hazard`); the latter are
    /// returned in [`Template::skipped`]. One side block that cannot be merged then cannot stop the production of a
    /// node that would otherwise merge it every time.
    pub fn template(
        &self,
        snapshot: &BtcSnapshot,
        unmerged: &BTreeSet<Hash>,
    ) -> Result<Template, NodeError> {
        let tip = self.tip()?;
        let dag = self.dag()?;
        let missing = || NodeError::Internal("block missing from the DAG".into());
        let tip_key = dag.chain_key(&tip.hash).map_err(dag_err)?;
        let tip_score = dag.data(&tip.hash).ok_or_else(missing)?.blue_score;
        let mut others = Vec::new();
        // A block beyond the merge depth of the main-chain tip can never be merged on it (a merged block needs
        // blue_score + merge_depth ≥ blue_score(SP)), so it is not tried at all: such tips are cheap to create on an
        // old main-chain block and stay tips (second audit, P4a-2)
        for t in frontier(
            &dag,
            snapshot,
            &tip.hash,
            tip_score,
            self.params.merge_depth,
        )? {
            if t == tip.hash {
                continue;
            }
            let key = dag.chain_key(&t).map_err(dag_err)?;
            if key < tip_key {
                others.push(key);
            }
        }
        others.sort_unstable_by(|a, b| b.cmp(a));
        // Every trial below has the main-chain tip as its selected parent: one merge-depth root for all of them, whose
        // walks are then done once per template (D-097)
        let mut on_tip = dag.merge_root_cache(&tip.hash).map_err(dag_err)?;
        let mut parents = vec![tip.hash];
        // The merge set so far (checked), and the blocks left out with the reason
        let mut merging: BTreeSet<Hash> = BTreeSet::new();
        let mut skipped: BTreeMap<Hash, String> = BTreeMap::new();
        'tips: for (_, candidate) in others {
            if parents.len() >= self.params.max_block_parents {
                break;
            }
            let mut trial = parents.clone();
            trial.push(candidate);
            // The weight only affects the DAA score (D-084): the template is computed as a non-empty block (weight 1);
            // an empty block's score is converted by Template::daa_score
            let data = match on_tip.compute(&trial, 1) {
                Ok(data) if data.selected_parent == Some(tip.hash) => data,
                Ok(_) => continue,
                Err(GhostdagError::Corrupt(m)) => {
                    return Err(NodeError::Internal(format!(
                        "GHOSTDAG data inconsistent: {m}"
                    )))
                }
                // Merge limit exceeded, etc.: do not merge this tip
                Err(_) => continue,
            };
            let added: Vec<&Hash> = data
                .mergeset_ordered
                .iter()
                .filter(|h| !merging.contains(*h))
                .collect();
            if added
                .iter()
                .any(|h| unmerged.contains(*h) || skipped.contains_key(*h))
            {
                continue;
            }
            for hash in added {
                if let Some(reason) = self.merge_hazard(&dag, snapshot, hash)? {
                    skipped.insert(*hash, reason);
                    continue 'tips;
                }
            }
            merging = data.mergeset_ordered.into_iter().collect();
            parents = trial;
        }
        let ghostdag = on_tip.compute(&parents, 1).map_err(dag_err)?;
        if ghostdag.selected_parent != Some(tip.hash) {
            return Err(NodeError::TipChanged);
        }
        let mut sorted = parents.split_off(1);
        sorted.sort_unstable();
        parents.extend(sorted);
        let parent_anchor = highest_anchor(&dag, &parents)?;
        let tip_streak = dag
            .streak(&tip.hash)
            .ok_or_else(|| NodeError::Internal("main-chain tip missing from the DAG".into()))?;
        drop(dag);
        let mut merged = Vec::with_capacity(ghostdag.mergeset_ordered.len());
        for hash in &ghostdag.mergeset_ordered {
            merged.push(
                self.store
                    .block(hash)?
                    .ok_or_else(|| NodeError::Storage("merged block missing".into()))?,
            );
        }
        Ok(Template {
            tip,
            parents,
            ghostdag,
            tip_streak,
            merged,
            parent_anchor,
            skipped: skipped.into_iter().collect(),
        })
    }

    /// Why this producer does not merge block `hash` for now (local policy, M26), judged by the BTC chain `snapshot`;
    /// `None` when nothing stands in the way. A block's forced-inclusion entries were checked against the BTC chain
    /// this node had when it accepted the block (its window, or in anchor-invalid mode the rule of D-100), and BTC may
    /// have reorganized since:
    ///
    /// - a block anchored on the current chain must start with the forced-inclusion entries of its window (§7.4; only a
    ///   block accepted while its anchor was off the chain can miss them);
    /// - a block anchored above the local BTC tip waits for BTC sync;
    /// - a block anchored off the BTC best chain (an anchor-invalid block, D-056: the BTC block at its anchor height is
    ///   another one) is never merged: the finality determination excludes every block merging it, together with the
    ///   blocks whose selected chain passes through that one (D-101 item c), and such a block is cheap to make (one
    ///   taken from a peer while its anchor lags, `Chain::check_anchor`, names no BTC block at all);
    /// - an anchor below the snapshot's base is merged only while the indexer can determine its body from the current
    ///   chain ([`check_stale_entries`], as at acceptance): otherwise the indexers would wait for that body before they
    ///   could execute the merging block.
    ///
    /// The block must be in `dag` (the caller holds its read lock).
    fn merge_hazard(
        &self,
        dag: &Dag,
        snapshot: &BtcSnapshot,
        hash: &Hash,
    ) -> Result<Option<String>, NodeError> {
        let block = self.store.block(hash)?.ok_or_else(|| {
            NodeError::Storage("block in the DAG is missing from the block store".into())
        })?;
        let header = &block.header;
        let anchor = &header.btc_anchor;
        let parent_anchor = highest_anchor(dag, &header.parents)?;
        if snapshot.contains(anchor) {
            let forced = self.forced_entries(snapshot, &parent_anchor, anchor);
            return Ok(ksana_btc::native::check_forced_prefix(&block.entries, &forced).err());
        }
        if snapshot
            .tip()
            .is_none_or(|(height, _)| anchor.height > height)
        {
            return Ok(Some(format!(
                "its anchor (BTC height {}) is above the local BTC tip",
                anchor.height
            )));
        }
        if snapshot
            .hash_at(anchor.height)
            .is_some_and(|on_chain| on_chain != anchor.hash)
        {
            return Ok(Some(format!(
                "its anchor (BTC height {}) is not on the BTC best chain: no block merging it could become BTC-final (D-101)",
                anchor.height
            )));
        }
        Ok(check_stale_entries(
            &self.params,
            snapshot,
            &parent_anchor,
            header,
            &block.entries,
        )
        .err()
        .map(|(StaleEntries::Invalid(m) | StaleEntries::Missing(m))| {
            format!(
                "its anchor (BTC height {}) is no longer on the BTC best chain: {m}",
                anchor.height
            )
        }))
    }

    /// Summaries of the merged blocks: `data` is the GHOSTDAG data of the block merging them, which decides whether
    /// each is blue or red (D-074).
    pub fn summaries(
        &self,
        blocks: &[Block],
        data: &GhostdagData,
    ) -> Result<Vec<BlockSummary>, NodeError> {
        let dag = self.dag()?;
        blocks
            .iter()
            .map(|b| {
                let hash = b.header.hash();
                let block = dag.get(&hash).ok_or_else(|| {
                    NodeError::Internal("merged block missing from the DAG".into())
                })?;
                Ok(BlockSummary {
                    hash,
                    settles: settlement::needs_body(&b.entries),
                    anchor_height: b.header.btc_anchor.height,
                    producer: b.header.producer,
                    daa_score: block.data.daa_score,
                    weight: block.weight,
                    rewarded: data.is_merged_blue(&hash),
                })
            })
            .collect()
    }

    /// Execution inputs of the merged blocks (D-063): in the merge set's consensus order, the index being their
    /// acceptance index (D-079).
    pub fn merged_blocks(blocks: &[Block], fast_path_paused: bool) -> Vec<MergedBlock> {
        blocks
            .iter()
            .map(|b| MergedBlock {
                hash: b.header.hash(),
                producer: b.header.producer,
                entries: b.entries.clone(),
                fast_path_paused,
            })
            .collect()
    }

    /// Data the new block's settlement payload is to carry (producer's policy, D-063, D-075, D-077, D-135): starting from
    /// the parents, examine the blocks in the past by blue work from highest to lowest and collect the blocks whose
    /// header has not counted on BTC yet, plus the bodies (together with the headers) of blocks that need a body whose
    /// body has not landed yet, at most `MAX_SETTLEMENT_HEADERS − 1` of them, ordered by blue work from lowest to
    /// highest. A block that carries settlement transactions stops the search when its header has counted, or while its
    /// settlement may still confirm (its anchor at most [`IN_FLIGHT_BTC_BLOCKS`] behind the BTC tip): its own settlement
    /// carries its data and that of its past not yet landed. Otherwise its settlement has not confirmed in time, or
    /// confirmed while a header it needs had not counted: its header is carried, not its body, and its past is searched
    /// for headers only, since a header counts only once its parents have (D-135). When the compact encoding of the
    /// headers exceeds 1 / [`PAYLOAD_HEADER_SHARE`] of `MAX_BLOCK_DATA_BYTES`, or the bodies exceed 1 /
    /// [`PAYLOAD_BODY_SHARE`], the earliest are dropped (they are landed by republishing). Bodies are carried only for
    /// blocks in the future of the new block's merge-depth root (or the root itself, D-097, see
    /// [`Chain::check_carried_bodies`]); `parents[0]` is the selected parent.
    pub fn payload_carry(
        &self,
        parents: &[Hash],
        snapshot: &BtcSnapshot,
    ) -> Result<Carry, NodeError> {
        let keep = self.params.max_settlement_headers.saturating_sub(1);
        let dag = self.dag()?;
        let genesis = *dag.genesis();
        let root = match parents.first() {
            Some(sp) => Some(
                dag.chain_floor(sp, self.params.merge_depth)
                    .map_err(dag_err)?
                    .1,
            ),
            None => None,
        };
        let mut heap = BinaryHeap::new();
        let mut seen = BTreeSet::new();
        // The blocks reached other than only through a settlement that did not confirm in time: their bodies are
        // carried too. A block's children have more blue work, so all of them are examined before it is
        let mut full = BTreeSet::new();
        for parent in parents {
            if seen.insert(*parent) {
                heap.push(dag.sort_key(parent).map_err(dag_err)?);
            }
            full.insert(*parent);
        }
        let mut found = Vec::new();
        let mut in_flight = false;
        let btc_tip = snapshot.tip().map_or(0, |(h, _)| h);
        while let Some((work, hash)) = heap.pop() {
            if found.len() >= keep || seen.len() > PAYLOAD_HEADER_SEARCH {
                break;
            }
            if hash == genesis {
                continue;
            }
            let block = dag
                .get(&hash)
                .ok_or_else(|| NodeError::Internal("block missing from the DAG".into()))?;
            let counted = snapshot.counted_at(&hash).is_some();
            let mut bodies = full.contains(&hash);
            if block.payload.carries {
                if counted {
                    continue;
                }
                // Its own header not having counted means that settlement transaction is still in flight, or did not
                // confirm in time, or confirmed before a header it needs counted
                let recent = btc_tip.saturating_sub(block.payload.header.btc_anchor.height)
                    <= IN_FLIGHT_BTC_BLOCKS;
                if recent {
                    in_flight = true;
                    continue;
                }
                found.push((work, block.payload.header.clone(), None));
                bodies = false;
            } else {
                let body_needed =
                    bodies && block.payload.settles && snapshot.settlement_of(&hash).is_none();
                if body_needed || !counted {
                    let carried = match &root {
                        Some(root) => {
                            body_needed && dag.in_closed_past(root, &hash).map_err(dag_err)?
                        }
                        None => body_needed,
                    };
                    let body = if carried {
                        let stored = self.store.block(&hash)?.ok_or_else(|| {
                            NodeError::Storage(
                                "block in the DAG is missing from the block store".into(),
                            )
                        })?;
                        Some(stored.entries)
                    } else {
                        None
                    };
                    found.push((work, block.payload.header.clone(), body));
                }
            }
            for parent in &block.parents {
                // A parent pruned from memory (D-097) landed long ago
                if !dag.contains(parent) {
                    continue;
                }
                if seen.insert(*parent) {
                    heap.push(dag.sort_key(parent).map_err(dag_err)?);
                }
                if bodies {
                    full.insert(*parent);
                }
            }
        }
        found.sort_by_key(|(work, _, _)| *work);
        let items = found.into_iter().map(|(_, h, b)| (h, b)).collect();
        let max = self.params.max_block_data_bytes;
        let mut carry = trim_carry(items, max / PAYLOAD_HEADER_SHARE, max / PAYLOAD_BODY_SHARE)?;
        carry.in_flight = in_flight;
        Ok(carry)
    }

    /// Accepts a block (D-041, D-063): `now` as the local time means a newly received block (checked by the acceptance
    /// rules), `None` means syncing a historical block. Returns the outcome for that block, and the blocks the main
    /// chain accepted and disconnected (including the orphans with it as parent processed afterwards). Returns
    /// `NodeError::InvalidBlock` when the block is invalid (objectively invalid ones are remembered); a local fault
    /// (storage, bitcoind) returns another error and does not judge the block invalid. The block may be passed by value
    /// or by reference (the caller need not copy it to retry, M26).
    pub fn accept_block(
        &self,
        block: impl Borrow<Block>,
        now: Option<u64>,
    ) -> Result<AcceptReport, NodeError> {
        let block = block.borrow();
        let _guard = self.process.lock().map_err(poisoned)?;
        let mut report = AcceptReport {
            outcome: Accepted::Duplicate,
            connected: Vec::new(),
            disconnected: Vec::new(),
        };
        report.outcome = self.accept_one(block, now, &mut report)?;
        if matches!(report.outcome, Accepted::Orphan | Accepted::Duplicate) {
            return Ok(report);
        }
        let mut queue = vec![block.header.hash()];
        while let Some(parent) = queue.pop() {
            let children = self
                .orphans
                .lock()
                .map_err(poisoned)?
                .take_waiting_on(&parent);
            for child in children {
                let hash = child.header.hash();
                match self.accept_one(&child, None, &mut report) {
                    Ok(Accepted::Orphan | Accepted::Duplicate) => {}
                    Ok(_) => queue.push(hash),
                    Err(NodeError::InvalidBlock(m)) => {
                        eprintln!("[ksana-node] orphan {} is invalid: {m}", hex::encode(hash));
                    }
                    // Refused for now (remembered under the BTC tip, `Chain::refused`): the block that was waited on is
                    // accepted, and the report of the main-chain changes must not be lost
                    Err(e @ (NodeError::Rejected(_) | NodeError::Btc(_))) => {
                        eprintln!(
                            "[ksana-node] orphan {} not accepted for now: {e}",
                            hex::encode(hash)
                        );
                    }
                    Err(e) => return Err(e),
                }
            }
        }
        // The block the finality decision was waiting for has arrived: advance the decision immediately and make the
        // main chain pass through it (D-082); the main-chain changes are recorded in this report
        let wanted = *self.wanted.lock().map_err(poisoned)?;
        if let Some(wanted) = wanted {
            if self.contains(&wanted)? && self.evaluate_finality()?.advanced {
                if let Err(e) = self.follow_finality(&mut report) {
                    // Local fault midway through the switch: the main chain may already have changed; the report goes
                    // to the entry pool maintenance
                    self.stash_changes(report)?;
                    return Err(e);
                }
                if matches!(report.outcome, Accepted::Side)
                    && self.on_main(&block.header.hash(), block.header.height)?
                {
                    let tip = self.tip()?;
                    report.outcome = if report.disconnected.is_empty() {
                        Accepted::Extended(tip)
                    } else {
                        Accepted::Reorganized(tip)
                    };
                }
            }
        }
        Ok(report)
    }

    /// Validates and accepts a block (for tests and import tools): returns the new tip when the main chain comes to
    /// include it, the current tip for a known block, and an error for a block not on the main chain or an orphan.
    pub fn import_block(&self, block: &Block, now: Option<u64>) -> Result<Tip, NodeError> {
        match self.accept_block(block.clone(), now)?.outcome {
            Accepted::Extended(tip) | Accepted::Reorganized(tip) => Ok(tip),
            Accepted::Duplicate => self.tip(),
            Accepted::Side => Err(NodeError::InvalidBlock(
                "block is not on the main chain".into(),
            )),
            Accepted::Orphan => Err(NodeError::InvalidBlock("parents unknown".into())),
        }
    }

    /// Blocks the peer may lack during sync (GetBlocks of `/ksana/sync/2`, D-063): take F, the first block in `locator`
    /// that is on the main chain (the genesis block when none is), and return the blocks not in `past(F) ∪ {F}` in
    /// ascending `(blue_work, hash)` order (topological order); when `after` is given, only those with a key greater
    /// than it, at most `max` of them. Blocks below the point on F's selected chain whose blue score is below
    /// `blue_score(F) − 2 × MERGE_DEPTH` are left out (they can no longer be merged). Returns empty when `after` is not
    /// a block this node has.
    ///
    /// After pruning (D-097), a peer whose F is below the pruning root R is far behind: it is served the main chain's
    /// acceptance order from F up to R from the block store (each main-chain block preceded by the blocks it merges,
    /// `after` being a position in that order), which is exactly `(past(R) ∪ {R}) − (past(F) ∪ {F})`: every block a
    /// chain through R needs that is not in R's future. Once its locator reaches R, the next rounds continue in the
    /// order above from R, whose future is all in memory.
    pub fn sync_hashes(
        &self,
        locator: &[Hash],
        after: Option<&Hash>,
        max: usize,
    ) -> Result<Vec<Hash>, NodeError> {
        let mut fork = None;
        for hash in locator {
            if let Some(height) = self.main_height_of(hash)? {
                fork = Some((*hash, height));
                break;
            }
        }
        let (fork, fork_height) = fork.unwrap_or((self.genesis.hash(), 0));
        let root = self.pruning_root()?;
        if let Some((_, root_height)) = root {
            if fork_height < root_height {
                return self.sync_from_store(fork_height, root_height, after, max);
            }
        }
        // A position in the acceptance order served from the block store (at or below the pruning root): the order
        // above starts over
        let from_store = match (root, after) {
            (Some((_, root_height)), Some(a)) => self
                .store
                .accepted(a)?
                .is_some_and(|(height, _)| height <= root_height),
            _ => false,
        };
        let after_stored = match after {
            Some(a) => self.store.dag_seq(a)?.is_some(),
            None => false,
        };
        let dag = self.dag()?;
        if !dag.contains(&fork) {
            return Ok(Vec::new());
        }
        let floor = dag
            .chain_floor(&fork, self.params.merge_depth.saturating_mul(2))
            .map_err(dag_err)?;
        let start = match after {
            None => floor,
            Some(a) if dag.contains(a) && !from_store => {
                dag.sort_key(a).map_err(dag_err)?.max(floor)
            }
            Some(_) if from_store || after_stored => floor,
            Some(_) => return Ok(Vec::new()),
        };
        let past = dag.closed_past_above(&fork, &start).map_err(dag_err)?;
        Ok(dag
            .after(Some(&start))
            .filter(|h| !past.contains(*h))
            .take(max)
            .copied()
            .collect())
    }

    /// The main chain's acceptance order from the block store (D-097, see [`Chain::sync_hashes`]): the blocks accepted
    /// at heights `fork_height + 1 ..= root_height`, after `after` when it is in that range, at most `max` of them.
    fn sync_from_store(
        &self,
        fork_height: u64,
        root_height: u64,
        after: Option<&Hash>,
        max: usize,
    ) -> Result<Vec<Hash>, NodeError> {
        let mut height = fork_height.saturating_add(1);
        let mut skip = 0usize;
        if let Some(a) = after {
            if let Some((h, _)) = self.store.accepted(a)? {
                if h >= height && h <= root_height {
                    let index = self
                        .main_entry_at(h)?
                        .and_then(|entry| entry.accepted().position(|b| b.hash == *a));
                    if let Some(index) = index {
                        height = h;
                        skip = index.saturating_add(1);
                    }
                }
            }
        }
        let mut out = Vec::new();
        while height <= root_height && out.len() < max {
            let entry = self.main_entry_at(height)?.ok_or_else(|| {
                NodeError::Storage(format!("main chain is missing height {height}"))
            })?;
            for block in entry.accepted().skip(skip) {
                if out.len() >= max {
                    break;
                }
                out.push(block.hash);
            }
            skip = 0;
            match height.checked_add(1) {
                Some(next) => height = next,
                None => break,
            }
        }
        Ok(out)
    }

    /// DAG tips carried by `Status` (D-063): those still mergeable (blue score + MERGE_DEPTH not below the main-chain
    /// tip's blue score), by fork-choice key `(chain_work, hash)` from largest to smallest, at most `max` of them.
    /// A tip anchored, or merging a block anchored, off the BTC best chain is left out (local policy, D-104 item i),
    /// such as a block a retreat left behind: no block merging it could become BTC-final while that BTC chain stands,
    /// and a peer that refused it would fetch it again at every `Status` until it falls out of the merge depth. The
    /// blocks below it that templates merge in its stead are carried instead (D-104 item k, see `frontier`): the
    /// main-chain tip the retreat went back to, and a parallel block the retreat left behind, which a peer lacking it
    /// fetches and merges too.
    pub fn status_tips(&self, max: usize) -> Result<Vec<Hash>, NodeError> {
        let tip = self.tip()?;
        let snapshot = self.btc.snapshot()?;
        let dag = self.dag()?;
        let missing = || NodeError::Internal("block missing from the DAG".into());
        let tip_score = dag.data(&tip.hash).ok_or_else(missing)?.blue_score;
        let mut tips = Vec::new();
        for hash in frontier(
            &dag,
            &snapshot,
            &tip.hash,
            tip_score,
            self.params.merge_depth,
        )? {
            if unanchored(&dag, &snapshot, &hash)?.is_none() {
                tips.push(dag.chain_key(&hash).map_err(dag_err)?);
            }
        }
        tips.sort_unstable_by(|a, b| b.cmp(a));
        tips.truncate(max);
        Ok(tips.into_iter().map(|(_, hash)| hash).collect())
    }

    /// Number of blocks in the orphan pool.
    pub fn orphan_count(&self) -> Result<usize, NodeError> {
        Ok(self.orphans.lock().map_err(poisoned)?.len())
    }

    /// Parents the orphans are missing (fetched from peers).
    pub fn missing_parents(&self) -> Result<Vec<Hash>, NodeError> {
        let mut missing = self.orphans.lock().map_err(poisoned)?.missing();
        // The block the finality decision is waiting for (D-082)
        if let Some(wanted) = *self.wanted.lock().map_err(poisoned)? {
            if !missing.contains(&wanted) {
                missing.push(wanted);
            }
        }
        let missing: Vec<Hash> = {
            let dag = self.dag()?;
            missing.into_iter().filter(|h| !dag.contains(h)).collect()
        };
        let mut out = Vec::with_capacity(missing.len());
        for hash in missing {
            // Stored and pruned from memory (D-097): not missing
            if self.store.dag_seq(&hash)?.is_none() {
                out.push(hash);
            }
        }
        Ok(out)
    }

    /// Whether it is a known invalid block.
    pub fn is_invalid(&self, hash: &[u8; 32]) -> Result<bool, NodeError> {
        Ok(self.invalid.lock().map_err(poisoned)?.contains(hash))
    }

    /// Whether it is a block ineligible for the main chain (D-063: in the DAG, can be merged).
    pub fn is_disqualified(&self, hash: &[u8; 32]) -> Result<bool, NodeError> {
        Ok(self.disqualified.lock().map_err(poisoned)?.contains(hash))
    }

    fn mark_invalid(&self, hash: [u8; 32]) -> Result<(), NodeError> {
        self.invalid.lock().map_err(poisoned)?.insert(hash);
        Ok(())
    }

    /// Records a block header version above the highest this node knows (D-076); true when it was not recorded yet, so
    /// that the alert is printed once per version. At most [`MAX_UNKNOWN_VERSIONS`] versions are recorded: the version
    /// is checked before the proof of work, so anyone could otherwise grow the set without bound.
    fn warn_unknown_version(&self, version: u32) -> Result<bool, NodeError> {
        let mut seen = self.unknown_versions.lock().map_err(poisoned)?;
        if seen.len() >= MAX_UNKNOWN_VERSIONS {
            return Ok(false);
        }
        Ok(seen.insert(version))
    }

    /// Processes one block (the caller holds the `process` lock).
    fn accept_one(
        &self,
        block: &Block,
        now: Option<u64>,
        report: &mut AcceptReport,
    ) -> Result<Accepted, NodeError> {
        let header = &block.header;
        let hash = header.hash();
        if self.is_invalid(&hash)? {
            return Err(NodeError::InvalidBlock("known invalid block".into()));
        }
        if self.contains(&hash)? {
            // Known block: an earlier main-chain switch may have been interrupted for local reasons (BTC sync behind,
            // etc.); try again
            self.update_virtual(report)?;
            return Ok(Accepted::Duplicate);
        }
        // Version above the highest this node knows: an upgrade this node does not recognize (D-076), validity cannot
        // be judged — reject, but do not mark invalid or penalize the peer; after upgrading the software and
        // restarting, this chain syncs as usual
        let max_known = self.params.max_known_version();
        if header.version > max_known {
            if self.warn_unknown_version(header.version)? {
                eprintln!(
                    "[ksana-node] alert (highest level): received a block of protocol version {}, this node only supports up \
                     to {max_known}: the network may have upgraded, the node software needs upgrading (D-076)",
                    header.version
                );
            }
            return Err(NodeError::Rejected(format!(
                "block header version {} is above the highest version {max_known} this node supports: the node software needs upgrading (D-076)",
                header.version
            )));
        }
        if header.parents.is_empty() {
            self.mark_invalid(hash)?;
            return Err(NodeError::InvalidBlock(
                "a non-genesis block must have parents".into(),
            ));
        }
        if let Err(e) = check_parent_list(&self.params, &header.parents) {
            self.mark_invalid(hash)?;
            return Err(e);
        }
        for parent in &header.parents {
            if self.is_invalid(parent)? {
                self.mark_invalid(hash)?;
                return Err(NodeError::InvalidBlock("parent is invalid".into()));
            }
        }
        let (missing, point_anchor) = {
            let dag = self.dag()?;
            let missing: Vec<Hash> = header
                .parents
                .iter()
                .filter(|p| !dag.contains(p))
                .copied()
                .collect();
            let point_anchor = dag
                .pruning_point()
                .and_then(|point| dag.payload(point))
                .map(|info| info.header.btc_anchor.height);
            (missing, point_anchor)
        };
        if let Some(point_anchor) = point_anchor {
            // After pruning (D-097): a block anchored below the pruning point, or with a parent pruned from memory, is
            // not in the pruning point's future, so it can be neither on nor merged by a chain through the BTC-final
            // blocks; this node no longer processes it, and does not fetch its ancestors either
            let mut needs_pruned = header.btc_anchor.height < point_anchor;
            for parent in &missing {
                if needs_pruned {
                    break;
                }
                needs_pruned = self.store.dag_seq(parent)?.is_some();
            }
            if needs_pruned {
                if self.store.dag_seq(&hash)?.is_some() {
                    return Ok(Accepted::Duplicate);
                }
                return Err(NodeError::Rejected(
                    "the block is not in the future of the pruning point: it cannot be on, or merged by, a chain through \
                     the BTC-final blocks, and this node no longer processes it (D-097)"
                        .into(),
                ));
            }
        }
        // A block refused while syncing under the current BTC tip is answered from memory (local policy, M27), also one
        // refused for a missing parent (`Chain::refused_with_parent`)
        if now.is_none() {
            let tip = self.btc.snapshot()?.tip();
            let at = crate::monotonic_now();
            let refusals = self.refusals.lock().map_err(poisoned)?;
            for key in [(hash, None), (hash, Some(copy_digest(block)?))] {
                if let Some(refusal) = refusals.get(tip, &key, at) {
                    return Err(refusal);
                }
            }
        }
        if !missing.is_empty() {
            self.check_orphan(block)?;
            if let Some(error) = self.refused_with_parent(now, &missing, hash)? {
                return Err(error);
            }
            if !self
                .orphans
                .lock()
                .map_err(poisoned)?
                .insert(block, &missing)
            {
                return Err(NodeError::Rejected(format!(
                    "orphan of about {} bytes in memory exceeds the orphan pool's budget of {MAX_ORPHAN_BYTES} bytes",
                    orphan_size(block)
                )));
            }
            return Ok(Accepted::Orphan);
        }

        // Rules of the header itself: a failure makes that hash permanently invalid
        let CheckedHeader {
            data,
            weight,
            streak,
            parent_anchor,
            recent,
        } = match self.check_dag_header(header) {
            Ok(checked) => checked,
            Err(NodeError::InvalidBlock(m)) => {
                self.mark_invalid(hash)?;
                return Err(NodeError::InvalidBlock(m));
            }
            Err(e) => return Err(e),
        };
        // Acceptance rules that depend on the local time and the BTC view
        let snapshot = self.btc.snapshot()?;
        if let Err(e) = self.check_anchor(&snapshot, parent_anchor.height, header, now) {
            return Err(self.refused(now, &snapshot, (hash, None), e)?);
        }
        // The timestamp must not exceed the anchored BTC block's time plus MAX_TIMESTAMP_AHEAD_OF_ANCHOR (D-083,
        // consensus rule)
        if let Some(anchor_time) = self.anchor_time(&snapshot, &header.btc_anchor) {
            if let Err(e) = ksana_consensus::check_anchor_time(&self.params, header, anchor_time) {
                self.mark_invalid(hash)?;
                return Err(NodeError::InvalidBlock(e.to_string()));
            }
        }
        if let Some(now) = now {
            ksana_consensus::check_timestamp(&self.params, header, &recent, now)
                .map_err(|e| NodeError::Rejected(e.to_string()))?;
        }
        let normal = self.check_body(block, &snapshot, &parent_anchor, data.daa_score, now)?;

        // Store into the DAG
        self.store.put_dag_block(block)?;
        let work = self.consensus.work(header, weight)?;
        {
            let mut dag = self.dag.write().map_err(poisoned)?;
            let inserted = dag
                .insert(
                    hash,
                    header.parents.clone(),
                    work,
                    weight,
                    streak,
                    BlockInfo {
                        header: header.clone(),
                        settles: settlement::needs_body(&block.entries),
                        carries: block.settlement.is_some(),
                    },
                )
                .map_err(dag_err)?;
            if *inserted != data {
                return Err(NodeError::Internal(
                    "GHOSTDAG data differs from the one checked".into(),
                ));
            }
        }
        // Whether forced inclusion was checked on its anchor chain is remembered for its verdict (D-101)
        if normal {
            self.finality.lock().map_err(poisoned)?.normal.insert(hash);
        }
        let disconnected_before = report.disconnected.len();
        self.update_virtual(report)?;
        if self.accepted_height(&hash)?.is_none() {
            return Ok(Accepted::Side);
        }
        let tip = self.tip()?;
        Ok(if report.disconnected.len() > disconnected_before {
            Accepted::Reorganized(tip)
        } else {
            Accepted::Extended(tip)
        })
    }

    /// What an orphan must pass before it is kept (local policy, M26): rules that do not need its parents and that every
    /// valid block meets. The parents after the first are in strictly ascending byte order and none is the first (the
    /// header check, whichever parent turns out to be the selected one); the PoW meets the block's own `bits` (a valid
    /// target not above the PoW limit) at the lowest weight the block can have — 1 for a non-empty block, that of the
    /// first empty block of a streak for an empty one, as the weight only grows with the streak (D-084, D-090) — the
    /// entries match `entries_root` and the body fits in a payload (D-100). A failure rejects this copy without marking
    /// the hash invalid: the block is judged by all the rules once its parents are present.
    fn check_orphan(&self, block: &Block) -> Result<(), NodeError> {
        let header = &block.header;
        if let Some((first, rest)) = header.parents.split_first() {
            let ascending = rest.windows(2).all(|w| matches!(w, [a, b] if a < b));
            if !ascending || rest.contains(first) {
                return Err(ConsensusError::BadParentOrder.into());
            }
        }
        check_pow(
            self.consensus.algorithm(),
            header,
            self.params.pow_limit_bits,
            self.consensus.weight(header, 0),
        )?;
        if entries_root(&block.entries)? != header.entries_root {
            return Err(NodeError::InvalidBlock(
                "entries_root does not match the entries".into(),
            ));
        }
        check_body_fits(&self.params, header, &block.entries).map_err(NodeError::InvalidBlock)
    }

    /// Header rules (D-063, depending only on the header and the DAG): GHOSTDAG (number of parents, no parent an
    /// ancestor of another, merge set size and merge depth), version, `parents[0]` being the selected parent with the
    /// rest in ascending order, height, `bits` by `daa_score`, PoW (an empty block by the effective target, the weight
    /// by the selected parent's empty-block streak, D-084, D-090), and the anchor not below the highest anchor among
    /// the parents (D-035). Returns the GHOSTDAG data (the DAA score includes this block's weight), the weight and
    /// streak, the highest anchor among the parents and the recent timestamps on the selected chain (for the timestamp
    /// rule).
    ///
    /// The PoW is checked first, against the header's own `bits` (`bits` is compared with the value GHOSTDAG gives
    /// afterwards): GHOSTDAG costs more the longer the merged chains, and a header without valid PoW must not make
    /// the node pay for it (second audit, P1-1).
    fn check_dag_header(&self, header: &BlockHeader) -> Result<CheckedHeader, NodeError> {
        let dag = self.dag()?;
        let sp_streak = dag.parent_streak(&header.parents).map_err(dag_err)?;
        let weight = self.consensus.weight(header, sp_streak);
        let streak = self.consensus.streak(header, sp_streak);
        self.consensus.check_header_pow(header, weight)?;
        let data = dag.compute(&header.parents, weight).map_err(dag_err)?;
        let sp = data
            .selected_parent
            .ok_or_else(|| NodeError::Internal("no selected parent".into()))?;
        let sp_header = &dag
            .payload(&sp)
            .ok_or_else(|| NodeError::Internal("selected parent missing from the DAG".into()))?
            .header;
        self.consensus
            .check_header_rules(sp_header, data.daa_score, weight, header)?;
        let parent_anchor = highest_anchor(&dag, &header.parents)?;
        if header.btc_anchor.height < parent_anchor.height {
            return Err(NodeError::InvalidBlock(
                "btc_anchor height is below the parents' anchor".into(),
            ));
        }
        let recent = chain_timestamps(&dag, &sp, self.params.median_time_span);
        Ok(CheckedHeader {
            data,
            weight,
            streak,
            parent_anchor,
            recent,
        })
    }

    /// Body rules (D-041, D-063): entries root, the body fitting in a payload (D-100), presence and content of the
    /// settlement transactions (D-034, D-043, with their acceptance checks when `now` is given), forced inclusion
    /// (§7.4), then the state-independent entry rules (by the rules at the block's own DAA score, D-076). A wrong
    /// entries root or settlement transaction only means this copy is wrong (the block hash is not marked); when the
    /// entries root matches but the body size, forced inclusion or the entry rules do not, the block is invalid
    /// (marked). A historical block with an invalid anchor (D-056) is checked against the current BTC best chain
    /// instead of its window, the way the indexer determines its body (D-100): a forced-inclusion entry naming a
    /// transaction the current chain does not have only refuses this copy. The checks against the BTC view and the
    /// settlement transactions come before the signature checks of the entry rules, so that a copy they refuse costs
    /// no signature checks; while syncing such a refusal is remembered ([`Refusals`], local policy, M27). Returns
    /// whether the block's anchor is on the BTC best chain (forced inclusion checked on its anchor chain).
    fn check_body(
        &self,
        block: &Block,
        snapshot: &Arc<BtcSnapshot>,
        parent_anchor: &BtcAnchor,
        daa_score: u64,
        now: Option<u64>,
    ) -> Result<bool, NodeError> {
        let header = &block.header;
        let hash = header.hash();
        if entries_root(&block.entries)? != header.entries_root {
            return Err(NodeError::InvalidBlock(
                "entries_root does not match the entries".into(),
            ));
        }
        if let Err(m) = check_body_fits(&self.params, header, &block.entries) {
            self.mark_invalid(hash)?;
            return Err(NodeError::InvalidBlock(m));
        }
        let btc_view = self.btc_view(
            header.btc_anchor.clone(),
            Arc::clone(snapshot),
            now.is_none(),
        )?;
        if let Some(txs) = &block.settlement {
            if let Err(e) = self.check_attached(header, &block.entries, txs, &btc_view, now) {
                let key = (hash, Some(copy_digest(block)?));
                return Err(self.refused(now, snapshot, key, e)?);
            }
        }
        if btc_view.is_stale() {
            eprintln!(
                "[ksana-node] block {} anchors a BTC block (height {}) that is no longer on the local BTC best chain; handling in anchor-invalid mode (D-056)",
                hex::encode(hash),
                header.btc_anchor.height
            );
            match check_stale_entries(
                &self.params,
                snapshot,
                parent_anchor,
                header,
                &block.entries,
            ) {
                Ok(()) => {}
                Err(StaleEntries::Invalid(m)) => {
                    self.mark_invalid(hash)?;
                    return Err(NodeError::InvalidBlock(m));
                }
                Err(StaleEntries::Missing(m)) => {
                    return Err(self.refused(
                        now,
                        snapshot,
                        (hash, None),
                        NodeError::Rejected(m),
                    )?);
                }
            }
        } else {
            let forced = self.forced_entries(snapshot, parent_anchor, &header.btc_anchor);
            if let Err(m) = ksana_btc::native::check_forced_prefix(&block.entries, &forced) {
                self.mark_invalid(hash)?;
                return Err(NodeError::InvalidBlock(m));
            }
        }
        let rules = self.params.rules(daa_score);
        if let Err(m) = check_entries_static(&self.params, &rules, &block.entries) {
            self.mark_invalid(hash)?;
            return Err(NodeError::InvalidBlock(m));
        }
        Ok(!btc_view.is_stale())
    }

    /// The settlement transactions a block carries (D-034, D-043, D-081): a well-formed pair whose payload belongs to
    /// the block (D-100), the other bodies it carries in the block's past ([`Chain::check_carried_bodies`]), valid
    /// against the block's BTC view, and accepted by bitcoind when `now` is given. A failure refuses only this copy.
    fn check_attached(
        &self,
        header: &BlockHeader,
        entries: &[Entry],
        txs: &SettlementTxs,
        btc_view: &AnchoredView,
        now: Option<u64>,
    ) -> Result<(), NodeError> {
        let pair = settlement::decode(txs)?;
        let payload = settlement::attached_payload(&self.params, &pair).ok_or_else(|| {
            NodeError::InvalidBlock(
                "the reveal transaction has no unique, well-formed KSN1 envelope".into(),
            )
        })?;
        let bodies = settlement_belongs_to(&self.params, &payload, header, &entry_data(entries))
            .map_err(NodeError::InvalidBlock)?;
        self.check_carried_bodies(&header.parents, &bodies)?;
        validate_attached(&self.params, &pair, &payload, &AnchoredPrevouts(btc_view))
            .map_err(settlement_err)?;
        if now.is_some() {
            self.check_settlement_now(&pair)?;
        }
        Ok(())
    }

    /// Remembers a refusal of a block delivered while syncing (`now` is `None`) under the tip of `snapshot`, unless the
    /// block was marked invalid ([`Refusals`]), and gives the error back.
    fn refused(
        &self,
        now: Option<u64>,
        snapshot: &BtcSnapshot,
        key: RefusalKey,
        error: NodeError,
    ) -> Result<NodeError, NodeError> {
        self.remember(now, snapshot.tip(), key, crate::monotonic_now(), error)
    }

    /// Remembers a refusal made at `at` of a block delivered while syncing (`now` is `None`) under the BTC tip `tip`,
    /// unless the block was marked invalid ([`Refusals`]), and gives the error back. A refusal of the block itself
    /// (whatever the copy) that is not the local BTC view lagging ([`NodeError::Btc`]) also drops the orphans waiting
    /// for the block, and those waiting for them (local policy, D-104 item j): while it is remembered no block missing
    /// this one is kept as an orphan ([`Chain::refused_with_parent`]), and they come back with it in a later sync once
    /// it can be taken.
    fn remember(
        &self,
        now: Option<u64>,
        tip: Option<(u32, Hash)>,
        key: RefusalKey,
        at: Instant,
        error: NodeError,
    ) -> Result<NodeError, NodeError> {
        if now.is_none() && !self.is_invalid(&key.0)? {
            if let Some(refusal) = Refusal::of(&error) {
                let drops = key.1.is_none() && !matches!(refusal, Refusal::Btc(_));
                self.refusals
                    .lock()
                    .map_err(poisoned)?
                    .insert(tip, key, at, refusal);
                if drops {
                    let dropped = self
                        .orphans
                        .lock()
                        .map_err(poisoned)?
                        .drop_waiting_on(&key.0);
                    if dropped > 0 {
                        eprintln!(
                            "[ksana-node] dropped {dropped} orphans waiting for block {}, which is refused for now under the current BTC tip (local policy, D-104)",
                            hex::encode(key.0)
                        );
                    }
                }
            }
        }
        Ok(error)
    }

    /// The refusal of a block with a missing parent whose refusal for itself, not for the local BTC view lagging, is
    /// remembered under the current BTC tip (local policy, D-104 item j): that parent cannot be taken before the BTC
    /// tip changes or its refusal is forgotten, so the block is refused for now with it instead of being kept as an
    /// orphan, whose missing parent would be fetched from peers at every sync and checked again after every BTC block
    /// for as long as that BTC chain stands. Delivered while syncing, the refusal is remembered for as long as the
    /// parent's, so the blocks built on this one in a sync response are refused the same way. `None` when no missing
    /// parent has such a refusal.
    fn refused_with_parent(
        &self,
        now: Option<u64>,
        missing: &[Hash],
        hash: Hash,
    ) -> Result<Option<NodeError>, NodeError> {
        let tip = self.btc.snapshot()?.tip();
        let found = {
            let refusals = self.refusals.lock().map_err(poisoned)?;
            let at = crate::monotonic_now();
            missing.iter().find_map(|parent| {
                refusals
                    .find(tip, &(*parent, None), at)
                    .filter(|(_, refusal)| !matches!(refusal, Refusal::Btc(_)))
                    .map(|(since, _)| (*parent, since))
            })
        };
        let Some((parent, since)) = found else {
            return Ok(None);
        };
        let error = NodeError::Rejected(format!(
            "its parent {} is refused for now under the current BTC tip (local policy, D-104)",
            hex::encode(parent)
        ));
        Ok(Some(self.remember(now, tip, (hash, None), since, error)?))
    }

    /// Other bodies carried by the settlement payload (D-077, D-097): each must belong to a block in this block's past
    /// (`parents` are this block's parents) that is in the future of this block's merge-depth root or is the root
    /// itself (the highest block on the selected parent's chain whose blue score is at most the selected parent's minus
    /// `MERGE_DEPTH`), and equal all the entries of that block. The bound by the root keeps the rule exact on a pruned
    /// node (the root and its future are in memory whenever the block can be processed at all). The past is taken once
    /// for all the bodies ([`Chain::past_of`]) and each carried block is looked up once: a payload may carry the same
    /// body hundreds of times, and a walk down the selected chain per body would cost as much as the depth of the
    /// carried block each time (M26).
    fn check_carried_bodies(
        &self,
        parents: &[Hash],
        bodies: &[(BlockHeader, Vec<EntryData>)],
    ) -> Result<(), NodeError> {
        if bodies.is_empty() {
            return Ok(());
        }
        let past = self.past_of(parents)?;
        let root = {
            let dag = self.dag()?;
            let sp = dag.selected_parent(parents).map_err(dag_err)?;
            dag.chain_floor(&sp, self.params.merge_depth)
                .map_err(dag_err)?
                .1
        };
        // The entries of each carried block, checked and read once
        let mut stored: BTreeMap<Hash, Vec<EntryData>> = BTreeMap::new();
        for (carried, entries) in bodies {
            let hash = carried.hash();
            let expected = match stored.entry(hash) {
                btree_map::Entry::Occupied(found) => found.into_mut(),
                btree_map::Entry::Vacant(slot) => {
                    let in_past = self.dag()?.contains(&hash)
                        && past.contains(&hash, self.accepted_height(&hash)?);
                    if !in_past {
                        return Err(NodeError::InvalidBlock(format!(
                            "block {} whose body the settlement payload carries is not in this block's past",
                            hex::encode(hash)
                        )));
                    }
                    if !self.dag()?.in_closed_past(&root, &hash).map_err(dag_err)? {
                        return Err(NodeError::InvalidBlock(format!(
                            "block {} whose body the settlement payload carries is not in the future of this block's \
                             merge-depth root (D-097)",
                            hex::encode(hash)
                        )));
                    }
                    let block = self.store.block(&hash)?.ok_or_else(|| {
                        NodeError::Storage(
                            "block in the DAG is missing from the block store".into(),
                        )
                    })?;
                    slot.insert(entry_data(&block.entries))
                }
            };
            if *expected != *entries {
                return Err(NodeError::InvalidBlock(format!(
                    "body carried by the settlement payload for block {} does not match that block's entries",
                    hex::encode(hash)
                )));
            }
        }
        Ok(())
    }

    /// The past of a block with parents `parents` (all in the DAG), for many membership queries (M26), with the same
    /// answers as [`BlockDag::in_closed_past`] against each parent. Each parent's selected chain is walked once, down
    /// to the first main-chain block (or a pruned block, D-097), collecting the chain blocks above it and their merge
    /// sets (`past(C)` is the merge set of C and the closed past of its selected parent); the closed past of the
    /// main-chain block at height j is exactly the blocks the main chain accepted at heights up to j (`accepted`). A
    /// walk stops early at a block already collected: its closed past is covered by the parent walked before. The
    /// caller holds the `process` lock, so the main chain does not change in between.
    fn past_of(&self, parents: &[Hash]) -> Result<PastOf, NodeError> {
        let dag = self.dag()?;
        let main = self.main_chain()?;
        let mut past = PastOf::default();
        for parent in parents {
            let mut current = *parent;
            loop {
                let Some(block) = dag.get(&current) else {
                    // Pruned (D-097): no kept block is in the past of a pruned one (a kept block is the pruning point
                    // or in its future), so this walk has already collected every kept block in the parent's past
                    if dag.pruning_point().is_some() {
                        break;
                    }
                    return Err(NodeError::Internal("block missing from the DAG".into()));
                };
                let height = block.payload.header.height;
                // The blocks walked are in the DAG, so at or above the pruning point, whose height is in the main
                // chain's window (D-097)
                if main.get(height).is_some_and(|e| e.hash == current) {
                    past.main_height = past.main_height.max(Some(height));
                    break;
                }
                if !past.above.insert(current) {
                    break;
                }
                past.above
                    .extend(block.data.mergeset_ordered.iter().copied());
                match block.data.selected_parent {
                    Some(sp) => current = sp,
                    None => break,
                }
            }
        }
        Ok(past)
    }

    /// The best candidate for the main-chain tip (D-063), and whether the switch is forced by finality (D-082).
    ///
    /// Candidates are taken from the DAG's selected-chain tips (blocks that are no block's selected parent: tips
    /// without children, and blocks only merged by other blocks — exactly what the successors of a finalized block are
    /// when a heavier chain merges them, D-082): the one with the largest fork-choice key `(chain_work, hash)` (D-100)
    /// and no block ineligible for the main chain on its selected chain; when a tip's selected chain has such blocks,
    /// take the selected parent of the ineligible block closest to the main chain between it and the main chain (if
    /// that is not on the main chain yet). The candidate is on the tip's selected chain and its key is not above the
    /// tip's. A block is ineligible when it is disqualified (D-063), or when the finality determination would exclude
    /// it under the current BTC chain: its own anchor, or the anchor of a block it merges, is off the BTC best chain
    /// (D-101 item c). The main chain itself has no ineligible block above the finalized tip: [`Chain::update_virtual`]
    /// first retreats below one ([`Chain::retreat_from_unanchored`], D-104).
    ///
    /// When the main chain passes through the finalized block: the candidate must be strictly better than the current
    /// tip (tips whose key does not exceed the current main-chain tip's, e.g. abandoned branches, need not be examined
    /// along their selected chain), and its selected chain must pass through the finalized block (its junction with the
    /// main chain is not below it). When the main chain does not pass through the finalized block (this node chose
    /// another, heavier chain before it was finalized): the candidate is the best among the tips whose selected chain
    /// passes through the finalized block, whether or not it is better than the current tip; `None` when there is no
    /// such tip (this node does not have the finalized block yet).
    fn best_candidate(&self) -> Result<Option<(Hash, bool)>, NodeError> {
        let snapshot = self.btc.snapshot()?;
        let tip_hash = self.tip()?.hash;
        let (fin, fin_height) = self.finalized()?;
        let main_final = self.on_main(&fin, fin_height)?;
        let main = self.main_chain()?;
        let dag = self.dag()?;
        let disqualified = self.disqualified.lock().map_err(poisoned)?;
        // The blocks walked are in the DAG, so at or above the pruning point, whose height is in the main chain's
        // window (D-097)
        let on_main = |hash: &Hash, height: u64| main.get(height).is_some_and(|e| e.hash == *hash);
        let tip_key = dag.chain_key(&tip_hash).map_err(dag_err)?;
        let mut best: Option<(U256, Hash)> = None;
        let above = if main_final { Some(&tip_key) } else { None };
        'tips: for tip_key_desc in dag.selected_chain_tips_desc(above) {
            // A candidate never has a larger fork-choice key than the tip it comes from: the tips are visited from the
            // heaviest down, so none of the rest can beat the best candidate found
            if best.as_ref().is_some_and(|b| tip_key_desc <= b) {
                break;
            }
            let tip = &tip_key_desc.1;
            // The tip's selected chain, up to (excluding) the main chain
            let mut path = Vec::new();
            let mut cursor = *tip;
            loop {
                let Some(block) = dag.get(&cursor) else {
                    // Pruned (D-097): this selected chain leaves the pruning point's future before meeting the main
                    // chain, so it passes neither through the pruning point nor through the finalized blocks above it
                    if dag.pruning_point().is_some() {
                        continue 'tips;
                    }
                    return Err(NodeError::Internal("block missing from the DAG".into()));
                };
                if on_main(&cursor, block.payload.header.height) {
                    break;
                }
                path.push(cursor);
                match block.data.selected_parent {
                    Some(sp) => cursor = sp,
                    None => break,
                }
            }
            if main_final {
                // The junction with the main chain is below the finalized block: it forks below it and does not take
                // part (D-082)
                let join_height = dag
                    .payload(&cursor)
                    .ok_or_else(|| NodeError::Internal("block missing from the DAG".into()))?
                    .header
                    .height;
                if join_height < fin_height {
                    continue;
                }
            } else if Self::chain_at(&dag, tip, fin_height)? != Some(fin) {
                continue;
            }
            // The lowest block of the path ineligible for the main chain: disqualified (D-063), or excluded by the
            // finality determination under the current BTC chain, for its own anchor or that of a block it merges
            // (D-101 item c), so that neither it nor any block on top of it could ever become BTC-final
            let mut lowest = None;
            for (i, h) in path.iter().enumerate().rev() {
                if disqualified.contains(h) || unanchored(&dag, &snapshot, h)?.is_some() {
                    lowest = Some(i);
                    break;
                }
            }
            let candidate = match lowest {
                Some(lowest) => path.get(lowest.saturating_add(1)).copied(),
                None => path.first().copied(),
            };
            let Some(candidate) = candidate else { continue };
            if !main_final {
                // The candidate (possibly moved lower because of ineligible blocks) must still pass through the
                // finalized block
                let height = dag
                    .payload(&candidate)
                    .ok_or_else(|| NodeError::Internal("block missing from the DAG".into()))?
                    .header
                    .height;
                if height < fin_height {
                    continue;
                }
            }
            let key = dag.chain_key(&candidate).map_err(dag_err)?;
            if best.as_ref().is_none_or(|b| key > *b) {
                best = Some(key);
            }
        }
        Ok(best.map(|(_, h)| (h, !main_final)))
    }

    /// Switches the main chain to the best candidate (D-063): when the candidate's fork-choice key is strictly greater
    /// than the current tip's, retreat to the fork point, then execute and connect block by block along the
    /// candidate's selected chain; when a block in it is ineligible for the main chain, restore the original main
    /// chain, mark it ineligible and choose the next-best candidate instead. On a local fault, restore the original
    /// main chain and return the error. First the main chain retreats below a block a BTC reorg made ineligible
    /// ([`Chain::retreat_from_unanchored`], D-104), so the original main chain restored on a failure is eligible.
    fn update_virtual(&self, report: &mut AcceptReport) -> Result<(), NodeError> {
        self.retreat_from_unanchored(report)?;
        loop {
            let Some((candidate, forced)) = self.best_candidate()? else {
                return Ok(());
            };
            let tip = self.tip()?;
            if candidate == tip.hash {
                return Ok(());
            }
            let (candidate_key, tip_key) = {
                let dag = self.dag()?;
                (
                    dag.chain_key(&candidate).map_err(dag_err)?,
                    dag.chain_key(&tip.hash).map_err(dag_err)?,
                )
            };
            if !forced && candidate_key <= tip_key {
                return Ok(());
            }
            // The candidate's selected chain: walk back from the candidate to the fork point on the main chain
            let mut branch = Vec::new();
            let mut cursor = candidate;
            let (fork_hash, fork_height) = loop {
                let (height, sp) = {
                    let dag = self.dag()?;
                    let block = dag
                        .get(&cursor)
                        .ok_or_else(|| NodeError::Internal("block missing from the DAG".into()))?;
                    (block.payload.header.height, block.data.selected_parent)
                };
                if self.store.hash_at(height)? == Some(cursor) {
                    break (cursor, height);
                }
                branch.push(cursor);
                cursor = sp.ok_or_else(|| {
                    NodeError::Internal("selected chain does not reach the main chain".into())
                })?;
            };
            branch.reverse();
            let depth = tip.header.height.saturating_sub(fork_height);
            if depth > self.params.reorg_window && !forced {
                eprintln!(
                    "[ksana-node] alert: refusing a main-chain switch of depth {depth} (exceeds REORG_WINDOW {})",
                    self.params.reorg_window
                );
                return Ok(());
            }
            if forced {
                eprintln!(
                    "[ksana-node] alert: the main chain does not pass through the BTC-finalized block; switching to a chain that does (depth {depth}, D-082)"
                );
                if depth > self.params.reorg_window {
                    // A switch to the finalized chain is not bound by REORG_WINDOW: when the state at the fork point
                    // has been pruned, rebuild from the genesis block (D-093)
                    eprintln!(
                        "[ksana-node] alert (highest level): switch depth {depth} exceeds REORG_WINDOW {}; when the state at the \
                         fork point has been pruned, the state is rebuilt from the genesis block before switching, and this node \
                         processes no new blocks meanwhile (D-093)",
                        self.params.reorg_window
                    );
                }
            }
            if depth > self.params.settlement_depth {
                eprintln!(
                    "[ksana-node] alert (highest level): main-chain switch depth {depth} exceeds SETTLEMENT_DEPTH {}; settlement transactions already broadcast may be voided",
                    self.params.settlement_depth
                );
            }
            let old_chain: Vec<Hash> = self
                .main_chain()?
                .iter_from(fork_height.saturating_add(1))
                .map(|(_, e)| e.hash)
                .collect();
            if depth > 0 {
                eprintln!(
                    "[ksana-node] main-chain switch: fork point height {fork_height}, disconnecting {} main-chain blocks, connecting {}",
                    old_chain.len(),
                    branch.len()
                );
            }
            let disconnected = self.rewind_to(fork_hash, fork_height)?;
            let mut connected = Vec::new();
            let mut failure = None;
            for hash in &branch {
                match self.connect_chain_block(hash) {
                    Ok(blocks) => connected.extend(blocks),
                    Err(e) => {
                        failure = Some((*hash, e));
                        break;
                    }
                }
            }
            let Some((failed, error)) = failure else {
                report.disconnected.extend(disconnected);
                report.connected.extend(connected);
                continue;
            };
            // Restore the original main chain
            eprintln!(
                "[ksana-node] block {} of the new main chain could not be connected ({error}); restoring the original main chain",
                hex::encode(failed)
            );
            self.rewind_to(fork_hash, fork_height)?;
            for hash in &old_chain {
                self.connect_chain_block(hash)?;
            }
            match error {
                NodeError::InvalidBlock(_) => {
                    let height = self
                        .dag()?
                        .payload(&failed)
                        .map(|b| b.header.height)
                        .unwrap_or(u64::MAX);
                    if self.is_final(&failed, height)? {
                        eprintln!(
                            "[ksana-node] alert (highest level): the BTC-finalized block {} is ineligible for the main chain, \
                             although only a block judged valid becomes final (D-101): the implementation has a non-determinism \
                             bug; this node stays before it (D-082)",
                            hex::encode(failed)
                        );
                    }
                    self.disqualified.lock().map_err(poisoned)?.insert(failed);
                }
                other => return Err(other),
            }
        }
    }

    /// Keeps the main chain eligible above the finalized tip (D-104; the caller holds the `process` lock): when a BTC
    /// reorg took the anchor of a main-chain block above the finalized tip, or of a block it merges, off the BTC best
    /// chain (it needs a reorg of at least `ANCHOR_DEPTH` + 1 blocks in this node's bitcoind), neither that block nor any
    /// block on top of it can ever be BTC final (D-101 item c), so the main chain retreats to the selected parent of the
    /// lowest such block, whatever the depth (the state is rebuilt from the genesis block when the parent's was
    /// discarded, D-093). [`Chain::best_candidate`] then picks the best eligible chain as usual: the same chain the
    /// finality decision goes on with, and back to the original one when BTC reorganizes back. The finalized tip is
    /// read before the snapshot, so the snapshot is at least as new as the one it was decided on. Nothing happens
    /// while the main chain does not pass through the finalized tip: the switch to it is forced anyway (D-082), and
    /// its candidates are checked the same way. Returns whether the main chain retreated, with the disconnected blocks
    /// in `report`.
    fn retreat_from_unanchored(&self, report: &mut AcceptReport) -> Result<bool, NodeError> {
        let (fin, fin_height) = self.finalized()?;
        let snapshot = self.btc.snapshot()?;
        let lowest = {
            let main = self.main_chain()?;
            if main.get(fin_height).map(|e| e.hash) != Some(fin) {
                return Ok(false);
            }
            let dag = self.dag()?;
            let mut lowest = None;
            for (height, entry) in main.iter_from(fin_height.saturating_add(1)) {
                if let Some(member) = unanchored(&dag, &snapshot, &entry.hash)? {
                    lowest = Some((height, entry.hash, member));
                    break;
                }
            }
            lowest
        };
        let Some((height, hash, member)) = lowest else {
            return Ok(false);
        };
        let parent_height = height.saturating_sub(1);
        let parent = self
            .main_hash_at(parent_height)?
            .ok_or_else(|| NodeError::Internal("main-chain block missing".into()))?;
        let depth = self.tip()?.header.height.saturating_sub(parent_height);
        if member == hash {
            eprintln!(
                "[ksana-node] alert: main-chain block {} (height {height}) is anchored on a BTC block a BTC reorg took off the \
                 best chain: neither it nor any block on top of it can become BTC final; retreating the main chain by {depth} \
                 blocks to height {parent_height} and choosing it again (D-104)",
                hex::encode(hash)
            );
        } else {
            eprintln!(
                "[ksana-node] alert: main-chain block {} (height {height}) merges block {}, which is anchored on a BTC block a \
                 BTC reorg took off the best chain: neither it nor any block on top of it can become BTC final; retreating \
                 the main chain by {depth} blocks to height {parent_height} and choosing it again (D-104)",
                hex::encode(hash),
                hex::encode(member)
            );
        }
        let disconnected = self.rewind_to(parent, parent_height)?;
        report.disconnected.extend(disconnected);
        Ok(true)
    }

    /// Executes and connects main-chain block `hash` (its selected parent must be the current main-chain tip): executes
    /// the blocks it merges in lenient mode and its own entries in strict mode (settlement backlog limit on a
    /// main-chain block: when triggered, its own fast path entries make it ineligible for the main chain, and the
    /// merged blocks' fast path entries are skipped, D-046, D-063), checks the state root, then commits. Returns the
    /// blocks the main chain thereby accepted (the merged blocks first, itself last).
    fn connect_chain_block(&self, hash: &Hash) -> Result<Vec<Block>, NodeError> {
        let block = self
            .store
            .block(hash)?
            .ok_or_else(|| NodeError::Storage("block missing".into()))?;
        let data = self
            .ghostdag(hash)?
            .ok_or_else(|| NodeError::Internal("block missing from the DAG".into()))?;
        let mut merged = Vec::with_capacity(data.mergeset_ordered.len());
        for m in &data.mergeset_ordered {
            merged.push(
                self.store
                    .block(m)?
                    .ok_or_else(|| NodeError::Storage("merged block missing".into()))?,
            );
        }
        let (tip, view) = self.tip_and_view()?;
        if block.header.selected_parent() != Some(&tip.hash) {
            return Err(NodeError::Internal(
                "the main-chain block's selected parent is not the main-chain tip".into(),
            ));
        }
        let snapshot = self.btc.snapshot()?;
        let btc_view = Arc::new(self.btc_view(
            block.header.btc_anchor.clone(),
            Arc::clone(&snapshot),
            true,
        )?);
        let stale = btc_view.is_stale();
        let header = &block.header;
        let mut paused = false;
        if !stale {
            if let Some(stuck) = self.backlog(
                &snapshot,
                &self.summaries(&merged, &data)?,
                header.btc_anchor.height,
            )? {
                paused = true;
                if block
                    .entries
                    .iter()
                    .any(|e| !matches!(e, Entry::Forced { .. }))
                {
                    return Err(NodeError::InvalidBlock(format!(
                        "settlement backlog limit: the settlement of block {} has not landed within SETTLEMENT_TIMEOUT; this block may only contain forced-inclusion entries (D-046)",
                        hex::encode(stuck.hash)
                    )));
                }
            }
        }
        let env = self.block_env(header, data.daa_score, &view)?;
        let outcome = match self.exec.execute_chain_block(
            Arc::new(view),
            btc_view,
            &env,
            &Chain::merged_blocks(&merged, paused),
            &block.entries,
        ) {
            Ok(outcome) => outcome,
            // Re-executed in anchor-invalid mode the BTC view answers from the current chain, which may not reproduce
            // the first execution: a verdict of invalid is then not trusted, like a state_root that does not match
            // below (D-056, D-100)
            Err(BlockError::Invalid(m)) if stale => {
                return Err(NodeError::Btc(format!(
                    "alert: re-executing a block with an invalid anchor (height {}) judged it invalid ({m}): possibly a \
                     BTC reorg that cannot be reproduced on the current BTC chain; not judged for now (D-056)",
                    header.height
                )))
            }
            Err(e) => return Err(e.into()),
        };
        if self.preview_root(&tip.hash, &outcome.writes)? != header.state_root {
            if stale {
                return Err(NodeError::Btc(format!(
                    "the state_root from re-executing a block with an invalid anchor (height {}) does not match the header: \
                     possibly a BTC reorg deeper than BTC_QUERY_DEPTH that cannot be reproduced on the current BTC chain; not judged for now (D-056)",
                    header.height
                )));
            }
            return Err(NodeError::InvalidBlock(
                "state_root does not match the result of re-execution".into(),
            ));
        }
        self.commit_chain(&block, &merged, outcome)?;
        if !stale {
            // Connected with its anchor on the best chain: when it and every block it merges were also accepted that
            // way, the connection checked everything the verdict of the finality decision checks (D-101)
            let trusted = {
                let state = self.finality.lock().map_err(poisoned)?;
                state.normal.contains(hash)
                    && merged
                        .iter()
                        .all(|m| state.normal.contains(&m.header.hash()))
            };
            if trusted {
                self.record_verdict(hash, true, verdict::basis(&snapshot, header))?;
            }
        }
        merged.push(block);
        Ok(merged)
    }

    /// Commits a block produced by this node (already executed by the block production loop per [`Chain::template`]):
    /// mutually exclusive with accepting blocks; returns [`NodeError::TipChanged`] when its selected parent is no
    /// longer the main-chain tip (also after the main chain retreated below a block a BTC reorg made ineligible,
    /// D-104) or when a BTC reorg synced since the template was built took its anchor, or the anchor of a block it
    /// merges, off the BTC best chain (D-104), and the caller discards the block.
    pub fn commit_block(&self, block: &Block, outcome: BlockOutcome) -> Result<Tip, NodeError> {
        let _guard = self.process.lock().map_err(poisoned)?;
        // A BTC reorg may have made the main chain ineligible since the template was built (D-104): the main chain
        // retreats first, and a block on top of what it left is not committed
        let mut report = AcceptReport {
            outcome: Accepted::Duplicate,
            connected: Vec::new(),
            disconnected: Vec::new(),
        };
        let retreated = self.retreat_from_unanchored(&mut report);
        self.stash_changes(report)?;
        retreated?;
        let header = &block.header;
        let hash = header.hash();
        if header.selected_parent() != Some(&self.tip()?.hash) {
            return Err(NodeError::TipChanged);
        }
        // Our own block is bound by the timestamp anchor bound as well (D-083): the producer already clamped the
        // timestamp within the bound; this is only the last gate
        let snapshot = self.btc.snapshot()?;
        if let Some(anchor_time) = self.anchor_time(&snapshot, &header.btc_anchor) {
            ksana_consensus::check_anchor_time(&self.params, header, anchor_time)
                .map_err(|e| NodeError::InvalidBlock(e.to_string()))?;
        }
        let (weight, streak, data) = {
            let dag = self.dag()?;
            let sp_streak = dag.parent_streak(&header.parents).map_err(dag_err)?;
            let weight = self.consensus.weight(header, sp_streak);
            let streak = self.consensus.streak(header, sp_streak);
            let data = dag.compute(&header.parents, weight).map_err(dag_err)?;
            // The template was built on an older snapshot: a BTC reorg synced since then may have taken the anchor of
            // this block, or of a block it merges, off the BTC best chain, and such a block can never become final
            // (D-104)
            let off_chain = anchored_off(&snapshot, &header.btc_anchor)
                || data.mergeset_ordered.iter().any(|member| {
                    dag.payload(member)
                        .is_some_and(|info| anchored_off(&snapshot, &info.header.btc_anchor))
                });
            if off_chain {
                return Err(NodeError::TipChanged);
            }
            (weight, streak, data)
        };
        let mut merged = Vec::with_capacity(data.mergeset_ordered.len());
        for m in &data.mergeset_ordered {
            merged.push(
                self.store
                    .block(m)?
                    .ok_or_else(|| NodeError::Storage("merged block missing".into()))?,
            );
        }
        if outcome.merged_receipts.len() != merged.len() {
            return Err(NodeError::Internal(
                "receipts of the merged blocks do not match the merge set".into(),
            ));
        }
        self.store.put_dag_block(block)?;
        let work = self.consensus.work(header, weight)?;
        self.dag
            .write()
            .map_err(poisoned)?
            .insert(
                hash,
                header.parents.clone(),
                work,
                weight,
                streak,
                BlockInfo {
                    header: header.clone(),
                    settles: settlement::needs_body(&block.entries),
                    carries: block.settlement.is_some(),
                },
            )
            .map_err(dag_err)?;
        self.commit_chain(block, &merged, outcome)
    }

    /// Commits an executed main-chain block: state → block store (main-chain index, receipts, op index, settlement
    /// records, tip) → the in-memory main chain and tip, then prunes old state. The caller holds the `process` lock.
    fn commit_chain(
        &self,
        block: &Block,
        merged: &[Block],
        outcome: BlockOutcome,
    ) -> Result<Tip, NodeError> {
        let header = &block.header;
        let hash = header.hash();
        let (entry, chain_work) = {
            let dag = self.dag()?;
            let entry = main_entry(&dag, &hash)?;
            let chain_work = dag.chain_work(&hash).map_err(dag_err)?;
            (entry, chain_work)
        };
        let (_, finalized_height) = self.finalized()?;
        let mut records = Vec::new();
        for accepted in merged.iter().chain([block]) {
            if let Some(record) = self.settlement_record(accepted, header.height)? {
                records.push((accepted.header.hash(), record));
            }
        }
        let mut accepted_blocks = Vec::with_capacity(merged.len().saturating_add(1));
        for (m, receipts) in merged.iter().zip(&outcome.merged_receipts) {
            accepted_blocks.push(AcceptedBlock { block: m, receipts });
        }
        accepted_blocks.push(AcceptedBlock {
            block,
            receipts: &outcome.receipts,
        });

        let mut inner = self.write()?;
        if header.selected_parent() != Some(&inner.tip.hash) {
            return Err(NodeError::TipChanged);
        }
        let parent_height = inner.tip.header.height;
        let root = inner.state.commit(header.height, &outcome.writes)?;
        if root != header.state_root {
            inner.state.rollback_to(parent_height)?;
            return Err(NodeError::InvalidBlock(
                "state_root after the commit does not match the header".into(),
            ));
        }
        if let Err(e) = self
            .store
            .connect_main(block, &entry, &accepted_blocks, &records)
        {
            inner.state.rollback_to(parent_height)?;
            return Err(e);
        }
        {
            let mut main = self.main.write().map_err(poisoned)?;
            Arc::make_mut(&mut main).push(entry);
        }
        {
            let mut open = self.open.lock().map_err(poisoned)?;
            for (block_hash, record) in records {
                open.insert(block_hash, record);
            }
        }
        let tip = &mut inner.tip;
        tip.recent_timestamps.push(header.timestamp);
        let excess = tip
            .recent_timestamps
            .len()
            .saturating_sub(self.params.median_time_span);
        tip.recent_timestamps.drain(..excess);
        tip.header = header.clone();
        tip.hash = hash;
        tip.chain_work = chain_work;
        let new_tip = tip.clone();

        // With pruning, the states a main-chain switch can still fork from are kept as well (D-097): every main chain
        // this node can switch to passes through the pruning root, which is BTC final by the pruning margin
        // The finalized tip's state is kept as well: the finality decision judges blocks on top of it (D-101)
        let mut keep_from = header
            .height
            .saturating_sub(self.params.reorg_window)
            .min(finalized_height);
        if self.pruning.enabled {
            if let Some(retain) = self.prune.lock().map_err(poisoned)?.retain {
                keep_from = keep_from.min(retain);
            }
        }
        if inner
            .state
            .oldest_version()
            .is_some_and(|oldest| keep_from > oldest)
        {
            inner.state.prune(keep_from)?;
        }
        Ok(new_tip)
    }

    /// Retreats the main chain to the fork point: first changes the store (tip, height index, and the receipts, op
    /// index and settlement records of the disconnected blocks), then rolls back the state and recomputes the tip
    /// information; the settlement records of the disconnected blocks are no longer broadcast (an alert for records
    /// already broadcast). Returns the blocks no longer accepted by the main chain (in their original execution order).
    fn rewind_to(&self, fork_hash: Hash, fork_height: u64) -> Result<Vec<Block>, NodeError> {
        let (disconnected, old_height) = {
            let main = self.main_chain()?;
            if fork_height < main.base() {
                return Err(NodeError::Internal(format!(
                    "cannot retreat the main chain to height {fork_height}, below its window starting at {} (D-097)",
                    main.base()
                )));
            }
            let mut disconnected = Vec::new();
            for (_, entry) in main.iter_from(fork_height.saturating_add(1)) {
                for b in entry.accepted() {
                    disconnected.push(self.store.block(&b.hash)?.ok_or_else(|| {
                        NodeError::Storage("block accepted by the main chain is missing".into())
                    })?);
                }
            }
            (disconnected, main.tip_height())
        };
        if disconnected.is_empty() {
            return Ok(disconnected);
        }
        let pruned = !self
            .read()?
            .state
            .oldest_version()
            .is_some_and(|oldest| fork_height >= oldest);
        if pruned {
            return self.rewind_deep(fork_hash, fork_height, old_height, disconnected);
        }
        let tip = load_tip(&*self.dag()?, &self.params, &fork_hash)?;
        let mut inner = self.write()?;
        self.store.rewind_main(
            &fork_hash,
            fork_height.saturating_add(1)..=old_height,
            &disconnected,
        )?;
        inner.state.rollback_to(fork_height)?;
        inner.tip = tip;
        {
            let mut main = self.main.write().map_err(poisoned)?;
            Arc::make_mut(&mut main)
                .truncate(fork_height)
                .map_err(NodeError::Internal)?;
        }
        let mut open = self.open.lock().map_err(poisoned)?;
        self.drop_records(&mut open, &disconnected)?;
        Ok(disconnected)
    }

    /// Drops the settlement records of the blocks the main chain no longer accepts (the caller holds `open`), with an
    /// alert for those already broadcast: their transactions may be in the BTC mempool, or come back to it with a BTC
    /// reorg, so their funding inputs stay reserved meanwhile ([`Chain::linger`], D-104).
    fn drop_records(
        &self,
        open: &mut BTreeMap<Hash, SettlementRecord>,
        disconnected: &[Block],
    ) -> Result<(), NodeError> {
        for block in disconnected {
            let Some(record) = open.remove(&block.header.hash()) else {
                continue;
            };
            if !record.broadcast {
                continue;
            }
            eprintln!(
                "[ksana-node] alert (highest level): a block whose settlement transactions were broadcast (acceptance height {}) is no longer accepted by the main chain",
                record.height
            );
            let mut txids = vec![record.txid];
            if let Some(pair) = block
                .settlement
                .as_ref()
                .and_then(|txs| settlement::decode(txs).ok())
            {
                txids.push(pair.commit_txid());
            }
            self.linger(txids, &record.funding_inputs)?;
        }
        Ok(())
    }

    /// Retreat when the state at the fork point has been pruned (D-093): both the block store and the state retreat to
    /// the genesis block (the state is cleared and genesis re-run), then the original main chain is re-executed up to
    /// the fork point. Re-execution uses the same path as connecting a new block ([`Chain::connect_chain_block`]), and
    /// the receipts, op index and settlement records in the block store are rewritten along the way. On a failure
    /// midway (a local fault) the main chain stops at the height re-executed so far, with the block store, the state
    /// and the in-memory main chain consistent, and the caller continues from there when retrying later. Returns the
    /// blocks no longer accepted by the main chain (those above the fork point).
    fn rewind_deep(
        &self,
        fork_hash: Hash,
        fork_height: u64,
        old_height: u64,
        disconnected: Vec<Block>,
    ) -> Result<Vec<Block>, NodeError> {
        let genesis_hash = self.genesis.hash();
        let main = self.main_chain()?;
        if main.base() > 0 || self.dag()?.pruning_point().is_some() {
            // After pruning (D-097) the history needed to re-execute from the genesis block is no longer in memory: the
            // switch can only come from a BTC reorg deeper than the pruning margin
            eprintln!(
                "[ksana-node] alert (highest level): a main-chain switch needs the state at fork point height {fork_height}, \
                 which has been discarded, and this node has pruned its history (D-097): it cannot re-execute the main \
                 chain from the genesis block. This takes a BTC reorg deeper than the pruning margin; resync this node \
                 from an empty data directory"
            );
            return Err(NodeError::Internal(format!(
                "the state at fork point height {fork_height} has been discarded and the history is pruned (D-097)"
            )));
        }
        let prefix: Vec<Hash> = main
            .iter_from(1)
            .take_while(|(height, _)| *height <= fork_height)
            .map(|(_, e)| e.hash)
            .collect();
        drop(main);
        {
            let inner = self.read()?;
            eprintln!(
                "[ksana-node] the state at fork point height {fork_height} has been pruned (oldest version {}): clearing the \
                 state store and re-executing {} main-chain blocks from the genesis block up to the fork point, then connecting \
                 the new chain (D-093)",
                inner.state.oldest_version().unwrap_or(0),
                prefix.len()
            );
        }
        let tip = load_tip(&*self.dag()?, &self.params, &genesis_hash)?;
        {
            let mut inner = self.write()?;
            // Change the store first: the batch above the fork point, then those below it in batches (to limit memory)
            self.store.rewind_main(
                &fork_hash,
                fork_height.saturating_add(1)..=old_height,
                &disconnected,
            )?;
            rewind_store(&self.store, &*self.dag()?, 0, fork_height)?;
            reset_state_to_genesis(&mut inner.state, &self.genesis, &self.genesis_writes)?;
            inner.tip = tip;
            let history: Arc<dyn MainHistory> = Arc::new(StoreHistory(Arc::clone(&self.store)));
            *self.main.write().map_err(poisoned)? = Arc::new(
                MainChain::new(main_entry(&*self.dag()?, &genesis_hash)?).with_history(history),
            );
            let mut open = self.open.lock().map_err(poisoned)?;
            self.drop_records(&mut open, &disconnected)?;
            // The settlement records below the fork point are rebuilt during re-execution
            open.clear();
        }
        for hash in &prefix {
            self.connect_chain_block(hash)?;
        }
        Ok(disconnected)
    }

    /// Broadcast record of the settlement transactions the block carries (`height` is the height of the main-chain
    /// block that accepted it).
    fn settlement_record(
        &self,
        block: &Block,
        height: u64,
    ) -> Result<Option<SettlementRecord>, NodeError> {
        let Some(txs) = &block.settlement else {
            return Ok(None);
        };
        let pair = settlement::decode(txs)?;
        Ok(Some(SettlementRecord {
            height,
            txid: pair.txid(),
            funding_inputs: settlement::funding_inputs(&pair.commit),
            broadcast: false,
        }))
    }
}

impl KnownBlocks for Chain {
    /// Adds a header that may count on BTC to the header DAG of the finality decision (D-082, D-135), with the same
    /// rules as the indexer: it counts when the DAG takes it, or holds it already.
    fn count(&self, snapshot: &BtcSnapshot, header: &BlockHeader) -> Result<bool, TrackerError> {
        let internal = |m: &str| TrackerError::Internal(m.into());
        let max_known = self.params.max_known_version();
        if header.version > max_known {
            // An upgrade this node does not recognize (D-076): the header cannot be judged and does not count here;
            // warn once, as when accepting blocks
            if self
                .warn_unknown_version(header.version)
                .map_err(|_| internal("version warning lock poisoned"))?
            {
                eprintln!(
                    "[ksana-node] alert (highest level): a BTC payload has a block header of protocol version {}, this node \
                     only supports up to {max_known}: the network may have upgraded, the node software needs upgrading (D-076)",
                    header.version
                );
            }
            return Ok(false);
        }
        let mut state = self
            .finality
            .lock()
            .map_err(|_| internal("finality state lock poisoned"))?;
        // The timestamp bound takes the time of the block at the anchor height on the BTC chain being scanned (D-083,
        // the same as `Chain::anchor_time`), and the main chain of the header DAG is chosen under it (D-104)
        match state
            .headers
            .insert(&self.consensus, header.clone(), snapshot)
        {
            HeaderInsert::Connected(_) | HeaderInsert::Duplicate => Ok(true),
            HeaderInsert::Invalid(reason) => {
                eprintln!(
                    "[ksana-node] block header {} (height {}) in a BTC payload is invalid and does not count: {reason}",
                    hex::encode(header.hash()),
                    header.height
                );
                Ok(false)
            }
            // Local fault (e.g. the PoW value cannot be computed, D-094): this round of BTC sync fails with an alert;
            // the header is not treated as invalid
            HeaderInsert::Unavailable(reason) => {
                eprintln!(
                    "[ksana-node] alert (highest level): cannot check block header {} (height {}) in a BTC payload: {reason}",
                    hex::encode(header.hash()),
                    header.height
                );
                Err(internal(&format!("cannot check block header: {reason}")))
            }
            // Below the pruning point, or on a block pruned away (D-097): it can no longer take part in the finality
            // decision
            HeaderInsert::Pruned | HeaderInsert::MissingParent => Ok(false),
        }
    }
}

/// `testmempoolaccept` for settlement transactions (D-081): when the commit transaction is already in the mempool or
/// confirmed, only the reveal transaction is submitted for checking, otherwise both as one package; when the reveal
/// transaction is already in the mempool or confirmed, accept directly (bitcoind reports `txn-already-in-mempool` /
/// `txn-already-known` for known transactions).
pub fn test_settlement(
    client: &dyn ksana_btc::client::BtcClient,
    pair: &SettlementPair,
) -> Result<Result<(), String>, ksana_btc::client::BtcError> {
    if client.transaction(&pair.txid())?.is_some() {
        return Ok(Ok(()));
    }
    if client.transaction(&pair.commit_txid())?.is_some() {
        client.test_accept(&[&pair.reveal])
    } else {
        client.test_accept(&pair.txs())
    }
}

fn settlement_err(e: SettlementError) -> NodeError {
    match e {
        SettlementError::Fatal(m) => NodeError::Btc(m),
        other => NodeError::InvalidBlock(other.to_string()),
    }
}

/// A DAG holding only the genesis block.
fn genesis_dag(
    params: &Params,
    consensus: &PowConsensus,
    genesis: &BlockHeader,
) -> Result<Dag, NodeError> {
    Ok(BlockDag::new(
        GhostdagParams::from_params(params),
        genesis.hash(),
        consensus.genesis_work(genesis)?,
        BlockInfo {
            header: genesis.clone(),
            settles: false,
            carries: false,
        },
    ))
}

/// Rebuilds the DAG from the whole history in the block store, in insertion order (D-063).
fn load_dag(
    store: &ChainStore,
    params: &Params,
    consensus: &PowConsensus,
    genesis: &BlockHeader,
) -> Result<Dag, NodeError> {
    let mut dag = genesis_dag(params, consensus, genesis)?;
    replay_dag(&mut dag, store, consensus, 0)?;
    Ok(dag)
}

/// Inserts into `dag` the blocks the block store inserted into the DAG with sequence number `from` or later, in
/// insertion order (a block is always inserted after its parents), reading them in batches to limit memory; blocks
/// already in `dag` are skipped.
fn replay_dag(
    dag: &mut Dag,
    store: &ChainStore,
    consensus: &PowConsensus,
    from: u64,
) -> Result<(), NodeError> {
    const BATCH: usize = 4_096;
    let failed = |e: GhostdagError| NodeError::Storage(format!("rebuilding the DAG failed: {e}"));
    let mut next = from;
    loop {
        let batch = store.dag_from(next, BATCH)?;
        let Some(&(last, _)) = batch.last() else {
            return Ok(());
        };
        for (_, hash) in batch {
            if dag.contains(&hash) {
                continue;
            }
            let block = store.block(&hash)?.ok_or_else(|| {
                NodeError::Storage("block referenced by the DAG order does not exist".into())
            })?;
            let sp_streak = dag.parent_streak(&block.header.parents).map_err(failed)?;
            let weight = consensus.weight(&block.header, sp_streak);
            let streak = consensus.streak(&block.header, sp_streak);
            let work = consensus.work(&block.header, weight)?;
            let settles = settlement::needs_body(&block.entries);
            let carries = block.settlement.is_some();
            dag.insert(
                hash,
                block.header.parents.clone(),
                work,
                weight,
                streak,
                BlockInfo {
                    header: block.header,
                    settles,
                    carries,
                },
            )
            .map_err(failed)?;
        }
        next = last
            .checked_add(1)
            .ok_or_else(|| NodeError::Storage("DAG sequence number overflow".into()))?;
    }
}

/// Reads the pruning checkpoint (D-097); `None` when there is none, or when it is unreadable, corrupt, of another
/// format version or of another chain (with an alert).
fn read_checkpoint(data_dir: &Path, genesis: &Hash) -> Option<Checkpoint> {
    let path = data_dir.join(CHECKPOINT_FILE);
    let unusable = |why: &str| {
        eprintln!(
            "[ksana-node] alert: the pruning checkpoint {} is {why}; rebuilding the block DAG from the whole history in the \
             block store and scanning BTC again from the genesis anchor (D-097)",
            path.display()
        );
    };
    let checkpoint: Checkpoint = match ksana_btc::prune::read_checkpoint(&path) {
        Ok(Some(checkpoint)) => checkpoint,
        Ok(None) => return None,
        Err(why) => {
            unusable(&why);
            return None;
        }
    };
    if checkpoint.version != CHECKPOINT_VERSION {
        unusable(&format!("of format version {}", checkpoint.version));
        return None;
    }
    if checkpoint.genesis != *genesis {
        unusable("of another chain");
        return None;
    }
    Some(checkpoint)
}

/// Writes the pruning checkpoint (D-097, [`ksana_btc::prune::write_checkpoint`]: a crash leaves either the old or the
/// new checkpoint).
fn write_checkpoint(data_dir: &Path, checkpoint: &Checkpoint) -> Result<usize, NodeError> {
    Ok(ksana_btc::prune::write_checkpoint(
        &data_dir.join(CHECKPOINT_FILE),
        checkpoint,
    )?)
}

/// Restores the block DAG and the BTC side from a pruning checkpoint (D-097): the checkpoint's DAG plus the blocks the
/// block store inserted after it was written, the header DAG, the finality decision and the BTC snapshot. `Err` when it
/// is inconsistent (the caller rebuilds from the whole history instead).
fn restore(
    store: &ChainStore,
    params: &Params,
    consensus: &PowConsensus,
    checkpoint: Checkpoint,
) -> Result<(Dag, Restored), String> {
    let Checkpoint {
        genesis,
        dag,
        dag_seq,
        root,
        main_base,
        main_base_prefix,
        snapshot,
        headers,
        finality,
        ..
    } = checkpoint;
    let mut dag =
        BlockDag::restore(GhostdagParams::from_params(params), dag).map_err(|e| e.to_string())?;
    if !dag.contains(&root.0) {
        return Err("the pruning root is not in the checkpoint's DAG".into());
    }
    replay_dag(&mut dag, store, consensus, dag_seq).map_err(|e| e.to_string())?;
    let finality = FinalityState::new(
        HeaderDag::from_record(params, headers)?,
        finality,
        Bodies::new(params.clone(), genesis),
    );
    Ok((
        dag,
        Restored {
            root,
            main_base,
            main_base_prefix,
            snapshot: BtcSnapshot::from_record(snapshot)?,
            finality,
        },
    ))
}

/// Whether the block store can back the main chain of a pruning checkpoint (D-097): its main chain still passes
/// through the pruning root, and every height of the window from `base` up to `version` has its summary.
fn window_complete(
    store: &ChainStore,
    root: &(Hash, u64),
    base: u64,
    version: u64,
) -> Result<bool, NodeError> {
    if store.hash_at(root.1)? != Some(root.0) {
        return Ok(false);
    }
    for height in base..=version {
        if store.summary_at(height)?.is_none() {
            return Ok(false);
        }
    }
    Ok(true)
}

/// The highest state version not above the block store's main-chain height `height` whose state root matches the
/// main-chain block at that height, searched down to the oldest version kept: the version, the block hash and the
/// latest version; `None` when there is none (the state store is empty, corrupted or belongs to another chain).
fn find_consistent(
    store: &ChainStore,
    state: &StateDb,
    height: u64,
) -> Result<Option<(u64, Hash, u64)>, NodeError> {
    let (Some(latest), Some(oldest)) = (state.latest_version(), state.oldest_version()) else {
        return Ok(None);
    };
    let mut version = latest.min(height);
    while version >= oldest {
        let hash = store
            .hash_at(version)?
            .ok_or_else(|| NodeError::Storage(format!("main chain is missing height {version}")))?;
        let root = store
            .block(&hash)?
            .ok_or_else(|| NodeError::Storage("block on the main chain is missing".into()))?
            .header
            .state_root;
        if state.root(version).ok() == Some(root) {
            return Ok(Some((version, hash, latest)));
        }
        let Some(lower) = version.checked_sub(1) else {
            break;
        };
        version = lower;
    }
    Ok(None)
}

/// Crash recovery (D-041): the state store and the block store are written separately, so after a crash their heights
/// may differ. `consistent` is the highest state version matching the main chain ([`find_consistent`]); the state is
/// rolled back to it, and when the block store's main chain (`stored`: its tip and height) is above it, the main chain
/// retreats to it (together with the records of the merged blocks) and the original tip is returned for
/// [`Chain::resume`] to re-select the main chain after the BTC sync. When there is no consistent version, rebuild from
/// the genesis block (D-093): the state is cleared and genesis re-run, the block store's main chain retreats to the
/// genesis block, and the original tip is likewise left for [`Chain::resume`] to re-execute (only with the whole
/// history in `dag`: a DAG restored from a pruning checkpoint always comes with a consistent version at or above its
/// root, D-097). Returns the main-chain tip and the original tip to recover.
fn recover(
    store: &ChainStore,
    state: &mut StateDb,
    dag: &Dag,
    consistent: Option<(u64, Hash, u64)>,
    stored: (Hash, u64),
    genesis: &BlockHeader,
    genesis_writes: &WriteSet,
) -> Result<(Hash, Option<Hash>), NodeError> {
    let (stored_tip, height) = stored;
    let Some((version, hash, latest)) = consistent else {
        if dag.pruning_point().is_some() {
            return Err(NodeError::Internal(
                "no consistent state for a pruned block DAG (D-097)".into(),
            ));
        }
        eprintln!(
            "[ksana-node] alert: the state store (latest version {}) matches the main chain nowhere within the versions it \
             keeps; rebuilding the state from the genesis block; the main chain (height {height}) is re-executed after the \
             BTC sync (D-093)",
            state
                .latest_version()
                .map_or_else(|| "empty".to_owned(), |v| v.to_string())
        );
        reset_state_to_genesis(state, genesis, genesis_writes)?;
        rewind_store(store, dag, 0, height)?;
        return Ok((genesis.hash(), Some(stored_tip)));
    };
    if version < latest {
        state.rollback_to(version)?;
    }
    if version == height {
        return Ok((hash, None));
    }
    eprintln!("[ksana-node] crash recovery: the state and the main chain agree at height {version}, main-chain height {height}; the later blocks are re-executed shortly");
    rewind_store(store, dag, version, height)?;
    Ok((hash, Some(stored_tip)))
}

/// Retreats the block store's main chain from height `old_height` to `to` (the block at height `to` becomes the tip),
/// together with the receipts, op index, acceptance index and settlement records of the disconnected blocks
/// (main-chain blocks and the blocks they merged, from the stored summaries, or from the DAG for heights without
/// one); the disconnected blocks are read in batches to limit memory (crash recovery and rebuilding from genesis may
/// retreat very deep, D-093).
fn rewind_store(store: &ChainStore, dag: &Dag, to: u64, old_height: u64) -> Result<(), NodeError> {
    const CHUNK: u64 = 256;
    let mut high = old_height;
    while high > to {
        let low = high
            .saturating_sub(CHUNK.saturating_sub(1))
            .max(to.saturating_add(1));
        let mut disconnected = Vec::new();
        for h in low..=high {
            let entry = match store.summary_at(h)? {
                Some(entry) => entry,
                None => {
                    let chain_hash = store.hash_at(h)?.ok_or_else(|| {
                        NodeError::Storage(format!("main chain is missing height {h}"))
                    })?;
                    main_entry(dag, &chain_hash)?
                }
            };
            for b in entry.accepted() {
                disconnected.push(store.block(&b.hash)?.ok_or_else(|| {
                    NodeError::Storage("block accepted by the main chain is missing".into())
                })?);
            }
        }
        let fork_height = low.saturating_sub(1);
        let fork = store.hash_at(fork_height)?.ok_or_else(|| {
            NodeError::Storage(format!("main chain is missing height {fork_height}"))
        })?;
        store.rewind_main(&fork, low..=high, &disconnected)?;
        high = fork_height;
    }
    Ok(())
}

/// Clears the state store and re-runs genesis (D-093): the root of the genesis state must equal the genesis block's
/// `state_root`.
fn reset_state_to_genesis(
    state: &mut StateDb,
    genesis: &BlockHeader,
    writes: &WriteSet,
) -> Result<(), NodeError> {
    state.reset()?;
    let root = state.commit(0, writes)?;
    if root != genesis.state_root {
        return Err(NodeError::Storage(
            "rebuilt genesis state does not match the genesis block's state_root".into(),
        ));
    }
    Ok(())
}

/// Loads the main chain's window from height `base` (0 until the first pruning, D-097) up to `tip_height`: the summaries
/// of the main-chain blocks and the blocks they merge, from the block store; heights without one (the genesis block,
/// or a main chain connected by an older version) are computed from the DAG and written back.
fn load_main(
    store: &ChainStore,
    dag: &Dag,
    base: u64,
    base_prefix: u32,
    tip_height: u64,
) -> Result<MainChain, NodeError> {
    let mut entries = Vec::new();
    for height in base..=tip_height {
        let entry = match store.summary_at(height)? {
            Some(entry) => entry,
            None => {
                let hash = store.hash_at(height)?.ok_or_else(|| {
                    NodeError::Storage(format!("main chain is missing height {height}"))
                })?;
                let entry = main_entry(dag, &hash)?;
                store.put_main_index(height, &entry)?;
                entry
            }
        };
        entries.push(entry);
    }
    MainChain::from_window(base, base_prefix, entries).map_err(NodeError::Storage)
}

/// Tip information: the header, the chain work and the recent timestamps on the selected chain.
fn load_tip(dag: &Dag, params: &Params, hash: &Hash) -> Result<Tip, NodeError> {
    let block = dag
        .get(hash)
        .ok_or_else(|| NodeError::Storage("main-chain tip missing from the DAG".into()))?;
    Ok(Tip {
        header: block.payload.header.clone(),
        hash: *hash,
        chain_work: dag.chain_work(hash).map_err(dag_err)?,
        recent_timestamps: chain_timestamps(dag, hash, params.median_time_span),
    })
}
