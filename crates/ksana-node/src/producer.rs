//! Block production loop (SPEC §6.2): anchor the BTC tip → forced inclusion → unlock → select path B entries → decide
//! whether to carry settlement transactions and assemble them → solve the PoW → sign and check the settlement
//! transactions → commit (D-034 - D-038). A block carries settlement transactions when there are bodies to carry and no
//! settlement transaction is in flight (D-077, [`crate::chain::Carry::settlement_due`]); the settlement payload also
//! carries the headers and bodies in the past that have not landed yet ([`Chain::payload_carry`]).
//!
//! The template is rebuilt when: the main-chain tip changes, the block DAG changes, the BTC snapshot changes, or the
//! template has existed for longer than [`TEMPLATE_LIFETIME`]; a change of the entry pool that can alter the packing
//! candidates (its generation, [`Mempool::generation`]) rebuilds it too, once it has been solved for
//! [`POOL_REFRESH_RATIO`] times as long as building it took (local policy, D-130). After each template is built, the
//! entry pool generation it includes is announced through [`TemplateSync`]; `ksana_submitOp` uses it to wait for a new
//! op to enter the template, guaranteeing that "the next block produced after a successful submission" has considered
//! the op.
//!
//! What other parties put on BTC or into the DAG cannot stop production (local policy, M26): a block whose merge fails
//! to execute is left out of the templates until the BTC tip changes (as are the blocks [`Chain::template`] finds a
//! merge hazard in); forced-inclusion entries are not bounded by the payload size (§7.4), so unlock and path B entries
//! are added only while this block's body still fits in one payload, and a settlement payload that would exceed
//! `MAX_BLOCK_DATA_BYTES` carries this block's data only, or is not carried at all when even that does not fit.

use std::collections::{btree_map, BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use bitcoin::hashes::Hash;
use bitcoin::secp256k1::Keypair;
use bitcoin::ScriptBuf;
use ksana_btc::client::{BtcClient, ScannedUtxo};
use ksana_btc::payload::PayloadError;
use ksana_btc::settlement::{
    draft, finalize, producer_script, validate_attached, Draft, ProducerUtxo, SettlementError,
};
use ksana_btc::tracker::BtcSnapshot;
use ksana_consensus::{median_time_past, Consensus};
use ksana_crypto::entries_root;
use ksana_execution::{
    builtin, BlockBuilder, BlockEnv, BlockError, BtcView, MergedBlock, Overlay, StateSource,
};
use ksana_mempool::{AdmitError, Mempool};
use ksana_primitives::{Block, BlockHeader, Body, BtcAnchor, Entry, Params, VmOp};
use ksana_rpc::BlockSummary;
use tokio::sync::broadcast;

use crate::btc::AnchoredPrevouts;
use crate::chain::{Chain, PAYLOAD_BODY_TRIGGER};
use crate::{monotonic_now, settlement, unix_now, NodeError};

/// Maximum time a template is used (refreshes the timestamp).
const TEMPLATE_LIFETIME: Duration = Duration::from_secs(2);
/// A change of the entry pool makes the running template stale only once it has been solved for this many times as
/// long as building it took (local policy, D-130): however fast the packing candidates change (anyone can send ops,
/// and a busy honest pool changes all the time), rebuilding for them takes at most a fifth of the time between such
/// rebuilds, and the rest goes to the PoW (A2-08). A new main-chain tip, a change of the block DAG, a new BTC tip and
/// [`TEMPLATE_LIFETIME`] make the template stale at once.
pub const POOL_REFRESH_RATIO: u32 = 4;
/// Confirmations needed for a coinbase output to mature.
const COINBASE_MATURITY: u32 = 100;
/// After a failed scan of the funding address, how long blocks go on without funding before the producer asks again
/// (D-109): they carry no settlement transactions, and their bodies wait for a later settlement (D-077).
const FUNDING_RETRY: Duration = Duration::from_secs(30);

/// Template generation announced by the block production loop: the entry pool generation the template includes.
#[derive(Default)]
pub struct TemplateSync {
    generation: Mutex<u64>,
    changed: Condvar,
}

impl TemplateSync {
    fn publish(&self, generation: u64) {
        if let Ok(mut current) = self.generation.lock() {
            *current = generation;
            self.changed.notify_all();
        }
    }

    /// Waits, at most `timeout`, for the block production loop to build a template that includes entry pool generation
    /// `generation`. Returns whether it arrived.
    pub fn wait_for(&self, generation: u64, timeout: Duration) -> bool {
        let Ok(guard) = self.generation.lock() else {
            return false;
        };
        self.changed
            .wait_timeout_while(guard, timeout, |current| *current < generation)
            .map(|(_, result)| !result.timed_out())
            .unwrap_or(false)
    }
}

/// Revenue address and hot wallet cap (D-060, local policy).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Revenue {
    /// Locking script of the revenue address (a standard output script; cannot be the funding address itself).
    pub script: ScriptBuf,
    /// Hot wallet cap (sats): when the hot wallet minus this funding is still not below it, the settlement
    /// transaction's change is paid to the revenue address.
    pub hot_cap_sat: u64,
}

/// The default highest fee rate of this node's BTC transactions (sat/vB, `--max-settlement-feerate`, D-102).
pub const DEFAULT_MAX_FEERATE: u64 = 100;
/// The default confirmation target of this node's BTC transactions (BTC blocks, `--settlement-conf-target`, D-102).
pub const DEFAULT_FEE_TARGET: u16 = 3;
/// The confirmation target while a lighter competing chain is on BTC (D-091): the next block.
pub const URGENT_FEE_TARGET: u16 = 1;
/// The most virtual bytes bitcoind's mempool holds while the next BTC block still has room for all of it (vB, D-117):
/// half of a block's 1 000 000 vB, the size up to which mempool.space suggests the minimum fee for the next block.
pub const ROOMY_MEMPOOL_VSIZE: u64 = 500_000;

/// The fee rate policy of this node's BTC transactions (local policy, D-102, D-117): its settlements, rescues,
/// republishes, funding splits and CPFP bumps pay bitcoind's estimate for confirming within `target` BTC blocks, kept
/// within [`floor`, `cap`]; outside a race, its new transactions pay the floor instead while bitcoind's mempool has
/// room, and a pair that room left waiting is bumped to at least an [`escalated`](Self::escalated) rate of its own.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FeePolicy {
    /// The lowest fee rate (sat/vB, `--settlement-feerate`), also paid when bitcoind has no estimate and while its
    /// mempool has room (D-117); never below `MIN_SETTLEMENT_FEERATE`.
    pub floor: u64,
    /// The highest fee rate (sat/vB, `--max-settlement-feerate`); never below the floor.
    pub cap: u64,
    /// The confirmation target (BTC blocks, `--settlement-conf-target`).
    pub target: u16,
}

impl FeePolicy {
    /// The floor `floor` with the default cap and target.
    pub fn new(floor: u64) -> Self {
        FeePolicy {
            floor,
            cap: DEFAULT_MAX_FEERATE.max(floor),
            target: DEFAULT_FEE_TARGET,
        }
    }

    /// The fee rate (sat/vB) to pay now: [`room_rate`](Self::room_rate) while bitcoind's mempool has room, unless
    /// `urgent` (a lighter competing chain is racing this node's chain for BTC finality, D-091); otherwise
    /// [`estimated_rate`](Self::estimated_rate) (D-117).
    pub fn rate(&self, params: &Params, client: &dyn BtcClient, urgent: bool) -> u64 {
        if !urgent {
            if let Some(rate) = self.room_rate(params, client) {
                return rate;
            }
        }
        self.estimated_rate(params, client, urgent)
    }

    /// The fee rate (sat/vB) while bitcoind's mempool has room (D-117): when it holds at most
    /// [`ROOMY_MEMPOOL_VSIZE`], the next BTC block takes all of it, so the floor is enough, or the mempool's own
    /// minimum when that is higher, up to the cap. `None` when the mempool holds more, is still loading or cannot be
    /// asked.
    pub fn room_rate(&self, params: &Params, client: &dyn BtcClient) -> Option<u64> {
        let (floor, cap) = self.bounds(params);
        match client.mempool_info() {
            Ok(Some(info)) if info.vsize <= ROOMY_MEMPOOL_VSIZE => {
                Some(info.min_feerate.clamp(floor, cap))
            }
            Ok(_) => None,
            Err(e) => {
                eprintln!("[ksana-node] cannot read bitcoind's mempool ({e}); paying the estimate");
                None
            }
        }
    }

    /// bitcoind's estimate (sat/vB) for confirming within `target` BTC blocks, or within [`URGENT_FEE_TARGET`] when
    /// `urgent`, kept within [`floor`, `cap`] and never below `MIN_SETTLEMENT_FEERATE` (D-102); the floor when
    /// bitcoind has no estimate or cannot be asked.
    pub fn estimated_rate(&self, params: &Params, client: &dyn BtcClient, urgent: bool) -> u64 {
        let (floor, cap) = self.bounds(params);
        let target = if urgent {
            URGENT_FEE_TARGET
        } else {
            self.target
        };
        match client.estimate_feerate(target) {
            Ok(Some(rate)) => rate.clamp(floor, cap),
            Ok(None) => floor,
            Err(e) => {
                eprintln!(
                    "[ksana-node] cannot estimate the BTC fee rate ({e}); paying the floor of {floor} sat/vB"
                );
                floor
            }
        }
    }

    /// The fee rate (sat/vB) `rate` doubled `doublings` times, kept within [`floor`, `cap`]: what a CPFP bump lifts a
    /// pair paying `rate` to at least while bitcoind's mempool has room, whatever the estimate (D-117).
    pub fn escalated(&self, params: &Params, rate: u64, doublings: u32) -> u64 {
        let (floor, cap) = self.bounds(params);
        let factor = 1u64.checked_shl(doublings).unwrap_or(u64::MAX);
        rate.saturating_mul(factor).clamp(floor, cap)
    }

    /// The floor, never below `MIN_SETTLEMENT_FEERATE`, and the cap, never below the floor.
    fn bounds(&self, params: &Params) -> (u64, u64) {
        let floor = self.floor.max(params.min_settlement_feerate_sat_vb);
        (floor, self.cap.max(floor))
    }
}

/// The fee rate (sat/vB) of the BTC transactions this node sends now (D-102, D-117): by `funding`'s policy, urgent
/// while a lighter competing chain is on BTC ([`Chain::btc_competitor`], D-091).
pub fn current_feerate(
    chain: &Chain,
    funding: &Funding,
    snapshot: &BtcSnapshot,
) -> Result<u64, NodeError> {
    let params = chain.params();
    let urgent = chain
        .btc_competitor(snapshot, params.btc_finality_confs)?
        .is_some();
    Ok(funding
        .fees
        .rate(params, chain.btc().client().as_ref(), urgent))
}

/// The producer's BTC funding wallet (D-037): a P2TR key-path (BIP86) address. Change returns to the same address;
/// with a revenue address configured, the part above the hot wallet cap is paid to the revenue address (D-060).
#[derive(Clone)]
pub struct Funding {
    /// Funding secret key (internal key, untweaked).
    pub keypair: Keypair,
    /// Locking script of the funding address (hot wallet).
    pub script: ScriptBuf,
    /// Fee rate policy of this node's BTC transactions (D-102).
    pub fees: FeePolicy,
    /// Revenue address and hot wallet cap (D-060); when `None`, change always returns to the hot wallet.
    pub revenue: Option<Revenue>,
    /// Target size of the funding UTXO pool (D-075, local policy): when fewer funding UTXOs are available, the largest
    /// one is split; 0 means no splitting.
    pub pool_target: usize,
    /// Settlement batching (D-077, local policy, on by default): while a settlement transaction is in flight, new
    /// blocks' bodies pile up and are carried all at once by the next block after it confirms. When off, every block
    /// that needs a body carries its own settlement transactions (the practice before D-077: faster BTC finality, more
    /// settlement transactions).
    pub batching: bool,
}

impl Funding {
    /// Constructs from the funding secret key and the lowest fee rate, with the default cap and confirmation target
    /// (change always returns to the hot wallet).
    pub fn new(keypair: Keypair, feerate_sat_vb: u64) -> Self {
        let (internal, _) = keypair.x_only_public_key();
        Funding {
            keypair,
            script: producer_script(&internal),
            fees: FeePolicy::new(feerate_sat_vb),
            revenue: None,
            pool_target: 0,
            batching: true,
        }
    }

    /// Turns settlement batching on or off (D-077).
    pub fn with_batching(mut self, batching: bool) -> Self {
        self.batching = batching;
        self
    }

    /// Sets the fee rate policy (D-102).
    pub fn with_fees(mut self, fees: FeePolicy) -> Self {
        self.fees = fees;
        self
    }

    /// Sets the target size of the funding UTXO pool (D-075).
    pub fn with_pool_target(mut self, target: usize) -> Self {
        self.pool_target = target;
        self
    }

    /// Sets the revenue address and hot wallet cap (D-060).
    pub fn with_revenue(mut self, revenue: Revenue) -> Self {
        self.revenue = Some(revenue);
        self
    }

    /// Change script of a settlement transaction (D-060): with `available` sats in the hot wallet usable for funding
    /// and `used` sats chosen this time, if the remainder is still not below the hot wallet cap, the change is paid to
    /// the revenue address; otherwise it returns to the hot wallet.
    pub fn change_script(&self, available: u64, used: u64) -> ScriptBuf {
        match &self.revenue {
            Some(revenue) if available.saturating_sub(used) >= revenue.hot_cap_sat => {
                revenue.script.clone()
            }
            _ => self.script.clone(),
        }
    }
}

/// Dependencies of the block production loop.
pub struct Producer {
    /// The chain.
    pub chain: Arc<Chain>,
    /// Entry pool.
    pub mempool: Arc<Mutex<Mempool>>,
    /// Producer (the x-only public key that receives the token fees).
    pub producer: [u8; 32],
    /// Funding wallet for settlement transactions; when `None`, blocks carry no settlement transactions and their
    /// bodies are landed by other producers carrying settlement transactions or by republishing, who take the carrier
    /// reward (D-095).
    pub funding: Option<Funding>,
    /// Whether to include entries (path B ops and unlock entries, local policy): when `false`, only empty blocks or
    /// blocks containing only forced-inclusion entries are produced. Always `true` for the node; the indexer's tests
    /// use it to produce empty blocks.
    pub pack: bool,
    /// Template generation announcements.
    pub sync: Arc<TemplateSync>,
    /// New block notifications.
    pub events: broadcast::Sender<BlockSummary>,
    /// Shutdown flag.
    pub shutdown: Arc<AtomicBool>,
    /// Pauses block production (for tests: submit a batch of ops first, then let them enter the same block).
    pub paused: Arc<AtomicBool>,
    /// Minimum interval between two blocks (local policy; tests and the low-difficulty devnet use it to limit the block
    /// rate).
    pub min_interval: Duration,
    /// Notification after this node produces a new block (the node uses it to broadcast to the network).
    pub announce: Option<Announce>,
    /// Funding UTXOs and the blocks not merged for now.
    pub cache: Mutex<ProducerCache>,
}

/// Callback for new block notifications.
pub type Announce = Arc<dyn Fn(&Block) + Send + Sync>;

/// What the block production loop keeps between attempts: the funding UTXOs (rescanned only when the anchor changes),
/// the blocks it does not merge for now (local policy, M26: the list starts over when the BTC tip changes) and the
/// account nonces it read for the packing candidates.
#[derive(Default)]
pub struct ProducerCache {
    tip: Option<[u8; 32]>,
    utxos: Vec<ScannedUtxo>,
    /// The anchor and the time of the last failed scan of the funding address: no funding at that anchor until
    /// [`FUNDING_RETRY`] has passed (D-109).
    scan_failed: Option<([u8; 32], std::time::Instant)>,
    /// The BTC tip `unmerged` belongs to.
    unmerged_at: Option<[u8; 32]>,
    /// Blocks not merged for now → the reason.
    unmerged: BTreeMap<[u8; 32], String>,
    /// The main-chain tip in whose state `nonces` were read.
    nonces_at: Option<[u8; 32]>,
    /// The nonces of the accounts in the entry pool at the last template on `nonces_at` (D-130): a template rebuilt on
    /// the same tip reads only the accounts new to the pool.
    nonces: BTreeMap<[u8; 32], u64>,
}

impl ProducerCache {
    /// The blocks not merged for now at BTC tip `btc_tip`: the list is cleared when the tip changes (merge hazards and
    /// execution failures depend on the BTC chain, D-056).
    fn unmerged_at(&mut self, btc_tip: Option<[u8; 32]>) -> BTreeSet<[u8; 32]> {
        if self.unmerged_at != btc_tip {
            self.unmerged_at = btc_tip;
            self.unmerged.clear();
        }
        self.unmerged.keys().copied().collect()
    }

    /// Leaves block `hash` out of the templates until the BTC tip changes (logged once).
    fn leave_out(&mut self, hash: [u8; 32], reason: String) {
        if let btree_map::Entry::Vacant(slot) = self.unmerged.entry(hash) {
            eprintln!(
                "[ksana-node] not merging block {} for now (local policy): {reason}",
                hex::encode(hash)
            );
            slot.insert(reason);
        }
    }

    /// The packing candidates of `pool` in `state`, the state of main-chain tip `tip` (D-130): an account's nonce is
    /// read from the state only when no template on the same tip read it before. Only the nonces of the accounts in the
    /// pool are kept, so the cache never outgrows it.
    fn candidates(
        &mut self,
        pool: &Mempool,
        tip: [u8; 32],
        state: Arc<dyn StateSource>,
    ) -> Result<Vec<VmOp>, NodeError> {
        let known = if self.nonces_at == Some(tip) {
            std::mem::take(&mut self.nonces)
        } else {
            BTreeMap::new()
        };
        let ov = Overlay::new(state);
        let mut read = BTreeMap::new();
        let candidates = pool.candidates_by(|account| {
            let nonce = match known.get(account) {
                Some(nonce) => *nonce,
                None => {
                    builtin::account_state(&ov, account)
                        .map_err(|fault| AdmitError::State(format!("{fault:?}")))?
                        .nonce
                }
            };
            read.insert(*account, nonce);
            Ok(nonce)
        });
        self.nonces_at = Some(tip);
        self.nonces = read;
        candidates.map_err(|e| NodeError::Internal(e.to_string()))
    }
}

/// Result of one block production attempt.
enum Attempt {
    Block(Box<Block>),
    Stale,
}

impl Producer {
    /// Runs until the shutdown flag is set. Internal errors are only logged and retried later.
    pub fn run(self) {
        let mut last_block: Option<std::time::Instant> = None;
        while !self.shutdown.load(Ordering::Relaxed) {
            let too_soon = last_block.is_some_and(|t| t.elapsed() < self.min_interval);
            if self.paused.load(Ordering::Relaxed) || too_soon {
                std::thread::sleep(Duration::from_millis(20));
                continue;
            }
            match self.step() {
                Ok(Some(block)) => {
                    last_block = Some(monotonic_now());
                    let header = &block.header;
                    eprintln!(
                        "[ksana-node] produced block height={} hash={} entries={} weight={} anchor={} bits={:#010x}",
                        header.height,
                        hex::encode(header.hash()),
                        block.entries.len(),
                        self.chain
                            .weight_of(&header.hash())
                            .ok()
                            .flatten()
                            .unwrap_or(0),
                        header.btc_anchor.height,
                        header.bits
                    );
                }
                Ok(None) => {}
                Err(e) => {
                    eprintln!("[ksana-node] block production failed: {e}");
                    std::thread::sleep(Duration::from_millis(500));
                }
            }
        }
    }

    /// Tries to produce one block: on success returns the committed block; returns `None` when the template is stale or
    /// production is paused or stopped.
    pub fn step(&self) -> Result<Option<Block>, NodeError> {
        let attempt = {
            let mut cache = self
                .cache
                .lock()
                .map_err(|_| NodeError::Internal("funding cache lock poisoned".into()))?;
            self.produce_one(&mut cache)
        };
        match attempt {
            Ok(Attempt::Stale) => Ok(None),
            Ok(Attempt::Block(block)) => Ok(Some(*block)),
            Err(NodeError::TipChanged) => Ok(None),
            Err(e) => Err(e),
        }
    }

    fn mempool(&self) -> Result<std::sync::MutexGuard<'_, Mempool>, NodeError> {
        self.mempool
            .lock()
            .map_err(|_| NodeError::Internal("entry pool lock poisoned".into()))
    }

    /// Available funding UTXOs (D-037): confirmed before the anchor height, not reserved by an unconfirmed settlement
    /// transaction, not an immature coinbase; by amount from largest to smallest. None for a while after a failed scan
    /// (D-109).
    fn available_utxos(
        &self,
        cache: &mut ProducerCache,
        anchor: &BtcAnchor,
    ) -> Result<Vec<ProducerUtxo>, NodeError> {
        let Some(funding) = &self.funding else {
            return Ok(Vec::new());
        };
        if cache.tip != Some(anchor.hash) {
            if cache
                .scan_failed
                .is_some_and(|(at, failed)| at == anchor.hash && failed.elapsed() < FUNDING_RETRY)
            {
                return Ok(Vec::new());
            }
            // A failed scan (bitcoind busy with another process's scan, or slower than the RPC timeout) costs this
            // node its settlements until the next attempt, not its blocks (D-109)
            match self.chain.btc().client().scan_utxos(&funding.script) {
                Ok(utxos) => {
                    cache.utxos = utxos;
                    cache.tip = Some(anchor.hash);
                    cache.scan_failed = None;
                }
                Err(e) => {
                    eprintln!(
                        "[ksana-node] scanning the funding address failed, producing without settlement transactions \
                         for {} s or until the next anchor: {e}",
                        FUNDING_RETRY.as_secs()
                    );
                    cache.scan_failed = Some((anchor.hash, monotonic_now()));
                    return Ok(Vec::new());
                }
            }
        }
        let reserved = self.chain.reserved_inputs()?;
        let mut utxos: Vec<ProducerUtxo> = cache
            .utxos
            .iter()
            .filter(|u| u.height <= anchor.height)
            .filter(|u| {
                !u.coinbase
                    || anchor.height.saturating_sub(u.height).saturating_add(1) >= COINBASE_MATURITY
            })
            .filter(|u| !reserved.contains(&(u.outpoint.txid.to_byte_array(), u.outpoint.vout)))
            .map(|u| ProducerUtxo {
                outpoint: u.outpoint,
                txout: u.txout.clone(),
            })
            .collect();
        utxos.sort_by_key(|u| std::cmp::Reverse(u.txout.value));
        Ok(utxos)
    }

    /// Builds a template and solves it; when a solution is found, signs the settlement transactions, checks and
    /// commits.
    fn produce_one(&self, cache: &mut ProducerCache) -> Result<Attempt, NodeError> {
        let building = monotonic_now();
        let chain = &self.chain;
        let params = chain.params();
        let snapshot = chain.btc().snapshot()?;
        // Parents: the main-chain tip plus the other tips that can be merged (D-063), without the blocks not merged for
        // now (local policy, M26)
        let unmerged = cache.unmerged_at(snapshot.tip().map(|(_, hash)| hash));
        let template = chain.template(&snapshot, &unmerged)?;
        for (hash, reason) in &template.skipped {
            cache.leave_out(*hash, reason.clone());
        }
        let (tip, view) = chain.tip_and_view()?;
        if tip.hash != template.tip.hash {
            return Ok(Attempt::Stale);
        }
        let dag_size = chain.dag_len()?;
        let view = Arc::new(view);
        let Some((btc_height, _)) = snapshot.tip() else {
            std::thread::sleep(Duration::from_millis(200));
            return Ok(Attempt::Stale);
        };
        if btc_height < template.parent_anchor.height {
            // The BTC chain got shorter (reorg): the anchor height cannot be below the parents'; wait for the BTC chain
            // to grow back
            std::thread::sleep(Duration::from_millis(200));
            return Ok(Attempt::Stale);
        }
        // Anchor the block at max(parents' anchor height, tip − ANCHOR_DEPTH) (D-056, D-063)
        let height = btc_height
            .saturating_sub(params.anchor_depth)
            .max(template.parent_anchor.height);
        let Some(hash) = snapshot.hash_at(height) else {
            std::thread::sleep(Duration::from_millis(200));
            return Ok(Attempt::Stale);
        };
        let anchor = BtcAnchor { height, hash };
        let btc_view = Arc::new(chain.btc_view(anchor.clone(), Arc::clone(&snapshot), false)?);
        let forced = chain.forced_entries(&snapshot, &template.parent_anchor, &anchor);
        // Settlement backlog limit (§6.4, D-046, D-063): when triggered, only empty blocks or blocks containing only
        // forced-inclusion entries are produced (path B entries of merged blocks are skipped), and a highest-level
        // alert is logged
        let backlog = chain.backlog(
            &snapshot,
            &chain.summaries(&template.merged, &template.ghostdag)?,
            anchor.height,
        )?;
        if let Some(stuck) = &backlog {
            eprintln!(
                "[ksana-node] alert (highest level): the settlement of block {} has not landed within SETTLEMENT_TIMEOUT, fast path paused (D-046)",
                hex::encode(stuck.hash)
            );
        }
        // Under backlog only forced-inclusion entries are allowed, but settlement transactions may still be carried,
        // with the piled-up bodies (D-077). A producer without a funding wallet also packs path B: the bodies are
        // landed by other producers carrying settlement transactions or by republishing, who take the carrier reward
        // (D-095)
        let fast = backlog.is_none() && self.pack;
        let funding_utxos = self.available_utxos(cache, &anchor)?;
        let can_fund = self.funding.is_some() && !funding_utxos.is_empty();

        let (generation, candidates) = {
            let pool = self.mempool()?;
            let candidates = if fast {
                cache.candidates(&pool, tip.hash, view.clone())?
            } else {
                Vec::new()
            };
            (pool.generation(), candidates)
        };

        let height = tip
            .header
            .height
            .checked_add(1)
            .ok_or_else(|| NodeError::Internal("height overflow".into()))?;
        let median = median_time_past(&tip.recent_timestamps).unwrap_or(0);
        let timestamp = unix_now().max(median.saturating_add(1));
        // The timestamp must not exceed the anchored BTC block's time plus MAX_TIMESTAMP_AHEAD_OF_ANCHOR (D-083): when
        // the local clock is ahead or BTC has had no new block for a long time, clamp to the bound; when the bound is
        // already not above the median time, this node cannot produce a valid block and waits for BTC to advance (the
        // fast lane pauses along with a stalled BTC instead of producing a block everyone rejects)
        let timestamp = match chain.anchor_time(&snapshot, &anchor) {
            Some(anchor_time) => {
                let bound = u64::from(anchor_time)
                    .saturating_add(params.max_timestamp_ahead_of_anchor_secs);
                if median.saturating_add(1) > bound {
                    std::thread::sleep(Duration::from_millis(200));
                    return Ok(Attempt::Stale);
                }
                timestamp.min(bound)
            }
            None => timestamp,
        };
        // Two-speed difficulty (D-084): bits do not depend on this block's weight; they follow the selected parent and
        // the DAA score without this block's weight. Version and rules follow the DAA score including this block's
        // weight (D-076), which is known only once the entries are determined, so prepare as a non-empty block
        // (weight 1) first
        let bits = chain
            .consensus()
            .next_bits(&tip.header, template.ghostdag.daa_score, 1)?;
        let mut header = BlockHeader {
            version: 0,
            height,
            parents: template.parents.clone(),
            btc_anchor: anchor.clone(),
            entries_root: [0; 32],
            state_root: [0; 32],
            producer: self.producer,
            timestamp,
            bits,
            nonce: 0,
        };
        let prepare = |header: &mut BlockHeader, weight: u64| -> Result<BlockEnv, NodeError> {
            let daa_score = template.daa_score(weight);
            header.version = params.protocol_version(daa_score);
            chain.block_env(header, daa_score, view.as_ref())
        };
        let mut env = prepare(&mut header, 1)?;
        // Room for this block's entries in a payload carrying only its header and body (§7.2): the forced-inclusion
        // entries come first and are not bounded by it (§7.4); unlock and path B entries are added only while the body
        // still fits, otherwise no payload could ever carry the body (M26)
        let mut room = ksana_btc::payload::body_room(params, &header);
        for entry in &forced {
            room = room.saturating_sub(settlement::entry_data_len(entry)?);
        }
        // Unlock entries (D-079): by the unlockable segment and the settled height at the anchor; the body may be
        // carried by a later settlement (D-077), so this block is not required to be able to fund
        let mut unlocks = if fast {
            chain.unlock_entries(&env.unlockable, &snapshot, &anchor)?
        } else {
            Vec::new()
        };
        let mut unlocks_len = 0usize;
        for entry in &unlocks {
            unlocks_len = unlocks_len.saturating_add(settlement::entry_data_len(entry)?);
        }
        if unlocks_len > room {
            eprintln!(
                "[ksana-node] the unlock entry waits for a later block: the forced-inclusion entries of this block leave no room for it in a payload"
            );
            unlocks.clear();
        } else {
            room = room.saturating_sub(unlocks_len);
        }
        // The settlement payload additionally carries the headers and bodies in the past that have not landed yet
        // (D-063, D-077); the budget for this block's entries assumes settlement transactions are carried
        let carry = chain.payload_carry(&template.parents, &snapshot)?;
        let mut budget = params
            .max_block_data_bytes
            .saturating_sub(settlement::fixed_payload_len(
                carry.headers.iter().chain([&header]),
            ))
            .saturating_sub(carry.bodies_len());
        for entry in forced.iter().chain(unlocks.iter()) {
            budget = budget.saturating_sub(settlement::entry_data_len(entry)?);
        }
        let budget = budget.min(room);
        let btc: Arc<dyn BtcView> = btc_view.clone();
        let merged = Chain::merged_blocks(&template.merged, backlog.is_some());
        // Produce an empty block when there are no entries to include (weight increasing with the main-chain tip's
        // empty-block streak, D-084, D-090); when there are still no entries after including with weight 1, redo as an
        // empty block too (the rules may differ); an empty block no longer tries to include entries, so the weight
        // after the redo is certain to match the plan
        let empty_weight = template.empty_weight(params);
        let build = |env: &BlockEnv, weight: u64| {
            chain
                .executor()
                .build_chain_block(view.clone(), btc.clone(), env, &merged, |builder| {
                    let mut entries = Vec::new();
                    if weight != 1 {
                        return Ok(entries);
                    }
                    for entry in &forced {
                        builder.push(entry)?;
                        entries.push(entry.clone());
                    }
                    for entry in &unlocks {
                        match builder.push(entry) {
                            Ok(_) => entries.push(entry.clone()),
                            Err(BlockError::Invalid(m)) => {
                                eprintln!("[ksana-node] skipping unlock entry: {m}");
                            }
                            Err(fatal) => return Err(fatal),
                        }
                    }
                    select(builder, &candidates, budget, &mut entries)?;
                    Ok(entries)
                })
        };
        let mut weight = if forced.is_empty() && unlocks.is_empty() && candidates.is_empty() {
            empty_weight
        } else {
            1
        };
        if weight != 1 {
            env = prepare(&mut header, weight)?;
        }
        let (mut outcome, mut entries) = match build(&env, weight) {
            Ok(built) => built,
            Err(e) => return self.merge_failure(cache, view.clone(), &btc, &env, &merged, e),
        };
        if entries.is_empty() && weight != empty_weight {
            weight = empty_weight;
            env = prepare(&mut header, weight)?;
            (outcome, entries) = match build(&env, weight) {
                Ok(built) => built,
                Err(e) => return self.merge_failure(cache, view.clone(), &btc, &env, &merged, e),
            };
        }
        self.sync.publish(generation);

        header.entries_root = entries_root(&entries)?;
        header.state_root = chain.preview_root(&tip.hash, &outcome.writes)?;
        if chain.consensus().weight(&header, template.tip_streak) != weight {
            return Err(NodeError::Internal(
                "block weight differs from the one prepared".into(),
            ));
        }

        // Whether to carry settlement transactions (D-077): by the producer's policy ([`Carry::settlement_due`])
        let own_len = if settlement::needs_body(&entries) {
            settlement::body_len(&entries)?
        } else {
            0
        };
        let batching = self.funding.as_ref().is_some_and(|f| f.batching);
        let due = if batching {
            carry.settlement_due(own_len, params.max_block_data_bytes / PAYLOAD_BODY_TRIGGER)
        } else {
            own_len > 0 || !carry.bodies.is_empty()
        };
        let carries = can_fund && due;
        let mut payload_headers: Vec<BlockHeader> =
            carry.headers.iter().chain([&header]).cloned().collect();
        let mut payload_bodies = carry.payload_bodies()?;

        // Draft of the settlement transactions (inputs, outputs and fee fixed before solving; the data output has the
        // same length as the final one). Its funding inputs stay reserved until this attempt ends: after the commit the
        // block's settlement record takes them over; they are released when the attempt is abandoned
        let mut reserved = None;
        let draft = if carries {
            let funded = match &self.funding {
                Some(funding) => {
                    match own_payload(params, &mut payload_headers, &mut payload_bodies, &entries)?
                    {
                        Some(payload) => match fund(
                            params,
                            &funding_utxos,
                            funding,
                            &payload,
                            current_feerate(chain, funding, &snapshot)?,
                        )? {
                            Ok(draft) => Some(draft),
                            Err(Unfunded::Insufficient) => None,
                            Err(Unfunded::Failed(reason)) => {
                                eprintln!(
                                    "[ksana-node] cannot assemble the settlement transactions: {reason}"
                                );
                                None
                            }
                        },
                        None => None,
                    }
                }
                None => None,
            };
            match funded {
                Some(d) => {
                    // Reserve the chosen funding before solving: the rescue thread may pick the same funding output at
                    // the same time (D-045, D-047)
                    let inputs = settlement::funding_inputs(&d.commit);
                    if !chain.try_reserve_inputs(&inputs)? {
                        return Ok(Attempt::Stale);
                    }
                    reserved = Some(Reserved { chain, inputs });
                    Some(d)
                }
                // Insufficient funds (or no payload fits): this block carries no settlement transactions; a later
                // settlement carries the body
                None => None,
            }
        } else {
            None
        };

        let started = monotonic_now();
        // A change of the entry pool makes the template stale only once it has been solved for POOL_REFRESH_RATIO times
        // as long as building it took (D-130)
        let refresh_after = started
            .saturating_duration_since(building)
            .saturating_mul(POOL_REFRESH_RATIO);
        let mut start = 0u64;
        // The number of nonces per attempt is given by the algorithm (about 1 ms per attempt for Argon2id, D-094);
        // between attempts check whether the template is stale
        let chunk = chain.consensus().solve_chunk();
        loop {
            if self.shutdown.load(Ordering::Relaxed) || self.paused.load(Ordering::Relaxed) {
                return Ok(Attempt::Stale);
            }
            // A computation failure is a local fault (`PowUnavailable` → `NodeError::Internal`): this round of block
            // production fails, without affecting the validity of any block
            if chain.consensus().solve(&mut header, weight, start, chunk)? {
                break;
            }
            let stale = (started.elapsed() >= refresh_after
                && self.mempool()?.generation() != generation)
                || chain.tip()?.hash != tip.hash
                || chain.dag_len()? != dag_size
                || chain.btc().snapshot()?.tip() != snapshot.tip()
                || started.elapsed() > TEMPLATE_LIFETIME;
            match start.checked_add(chunk) {
                Some(next) if !stale => start = next,
                _ => return Ok(Attempt::Stale),
            }
        }

        let settlement = match (draft, &self.funding) {
            (Some(draft), Some(funding)) => {
                let mut payload_headers = payload_headers;
                if let Some(last) = payload_headers.last_mut() {
                    *last = header.clone();
                }
                let payload =
                    settlement::payload(params, payload_headers, &entries, payload_bodies)?;
                let pair = finalize(params, draft, &payload, &funding.keypair).map_err(|e| {
                    NodeError::Internal(format!("settlement transaction signing failed: {e}"))
                })?;
                // Self-check: the same checks other nodes make on receipt (D-034, D-037, D-081)
                validate_attached(params, &pair, &payload, &AnchoredPrevouts(&btc_view)).map_err(
                    |e| {
                        NodeError::Internal(format!(
                            "own settlement transaction failed validation: {e}"
                        ))
                    },
                )?;
                if let Err(e) = chain.check_settlement_now(&pair) {
                    eprintln!("[ksana-node] settlement transaction failed the bitcoind check, discarding this block: {e}");
                    if let Some(reserved) = &reserved {
                        drop_spent_utxos(chain, cache, &reserved.inputs)?;
                    }
                    return Ok(Attempt::Stale);
                }
                Some(pair.encode())
            }
            _ => None,
        };
        let block = Block {
            header,
            entries,
            settlement,
        };
        chain.commit_block(&block, outcome)?;
        drop(reserved);
        self.after_commit(template.merged, &block)?;
        if let Some(announce) = &self.announce {
            announce(&block);
        }
        Ok(Attempt::Block(Box::new(block)))
    }

    /// Maintains the entry pool after the commit: the main chain accepted the merged blocks and this block.
    fn after_commit(&self, mut merged: Vec<Block>, block: &Block) -> Result<(), NodeError> {
        merged.push(block.clone());
        crate::ops::chain_changed(&self.chain, &self.mempool, &self.events, &merged, &[])
    }

    /// Building the block failed with `error` (local policy, M26): when executing one of the `merged` blocks fails, it
    /// is left out of the templates until the BTC tip changes and this attempt is abandoned; any other failure is
    /// returned (a local fault in this node's own state or BTC data). The block at fault ends the shortest prefix of
    /// `merged` whose execution fails without this block's own entries (found by bisection); when executing no merged
    /// block fails too, when all of them succeed, or when bitcoind is unreachable (every BTC lookup then fails), the
    /// failure cannot be attributed to one block.
    fn merge_failure(
        &self,
        cache: &mut ProducerCache,
        base: Arc<dyn StateSource>,
        btc: &Arc<dyn BtcView>,
        env: &BlockEnv,
        merged: &[MergedBlock],
        error: BlockError,
    ) -> Result<Attempt, NodeError> {
        let executor = self.chain.executor();
        let fails = |count: usize| {
            let prefix = merged.get(..count).unwrap_or(merged);
            executor
                .build_chain_block(Arc::clone(&base), Arc::clone(btc), env, prefix, |_| Ok(()))
                .is_err()
        };
        if merged.is_empty() || fails(0) || !fails(merged.len()) {
            return Err(error.into());
        }
        // Executing the first `low` merged blocks succeeds, the first `high` fails
        let (mut low, mut high) = (0, merged.len());
        while high.saturating_sub(low) > 1 {
            let middle = low.saturating_add(high.saturating_sub(low) / 2);
            if fails(middle) {
                high = middle;
            } else {
                low = middle;
            }
        }
        let Some(culprit) = merged.get(high.saturating_sub(1)) else {
            return Err(error.into());
        };
        if self.chain.btc().client().tip().is_err() {
            return Err(error.into());
        }
        cache.leave_out(
            culprit.hash,
            format!("executing it in a block fails: {error}"),
        );
        Ok(Attempt::Stale)
    }
}

/// After bitcoind rejected the settlement transactions: removes the funding outputs already spent (including in the
/// mempool) from this anchor's funding cache. The cache comes from a scan that only looks at the UTXO set; outputs
/// spent by transactions no longer tracked (e.g. republish or rescue transactions in flight before a restart, or other
/// spending from the funding wallet) would otherwise be picked again and again, and this node could not include fast
/// path entries until they confirm.
fn drop_spent_utxos(
    chain: &Chain,
    cache: &mut ProducerCache,
    inputs: &[([u8; 32], u32)],
) -> Result<(), NodeError> {
    let client = chain.btc().client();
    for (txid, vout) in inputs {
        let outpoint = bitcoin::OutPoint::new(bitcoin::Txid::from_byte_array(*txid), *vout);
        let unspent = client
            .utxo(&outpoint, true)
            .map_err(|e| NodeError::Btc(e.to_string()))?;
        if unspent.is_none() {
            cache.utxos.retain(|u| u.outpoint != outpoint);
        }
    }
    Ok(())
}

/// Funding inputs reserved by one block production attempt ([`Chain::try_reserve_inputs`]): released when it goes out
/// of scope. Once the block is committed, the block's settlement record holds them instead; when the attempt is
/// abandoned (stale template, pause, error) they are returned to the funding wallet.
struct Reserved<'a> {
    chain: &'a Chain,
    inputs: Vec<([u8; 32], u32)>,
}

impl Drop for Reserved<'_> {
    fn drop(&mut self) {
        if let Err(e) = self.chain.release_inputs(&self.inputs) {
            eprintln!("[ksana-node] failed to release funding inputs: {e}");
        }
    }
}

/// Why [`fund`] assembled no settlement transactions.
#[derive(Debug)]
pub(crate) enum Unfunded {
    /// Even all the funding UTXOs are not enough.
    Insufficient,
    /// Another reason (the payload exceeds `MAX_BLOCK_DATA_BYTES`, etc.).
    Failed(String),
}

impl std::fmt::Display for Unfunded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Unfunded::Insufficient => f.write_str("insufficient funds"),
            Unfunded::Failed(reason) => f.write_str(reason),
        }
    }
}

/// Assembles a settlement transaction draft (commit and reveal, the envelope using the funding key, D-081) paying
/// `feerate` sat/vB ([`current_feerate`]; never below `MIN_SETTLEMENT_FEERATE`) with the fewest funding UTXOs (added by
/// amount from largest to smallest); [`Unfunded`] when there is none (the caller logs it), an error on a local fault.
pub(crate) fn fund(
    params: &Params,
    utxos: &[ProducerUtxo],
    funding: &Funding,
    payload: &[u8],
    feerate: u64,
) -> Result<Result<Draft, Unfunded>, NodeError> {
    let feerate = feerate.max(params.min_settlement_feerate_sat_vb);
    let sum = |utxos: &[ProducerUtxo]| {
        utxos
            .iter()
            .map(|u| u.txout.value.to_sat())
            .fold(0u64, u64::saturating_add)
    };
    let available = sum(utxos);
    for count in 1..=utxos.len() {
        let Some(inputs) = utxos.get(..count) else {
            break;
        };
        match draft(
            params,
            inputs,
            funding.change_script(available, sum(inputs)),
            &funding.keypair.x_only_public_key().0,
            payload,
            feerate,
        ) {
            Ok(d) => return Ok(Ok(d)),
            Err(SettlementError::InsufficientFunds(_)) => continue,
            Err(SettlementError::Fatal(m)) => {
                return Err(NodeError::Internal(format!(
                    "assembling the settlement transactions failed: {m}"
                )))
            }
            // E.g. the payload exceeds the limit
            Err(e) => return Ok(Err(Unfunded::Failed(e.to_string()))),
        }
    }
    Ok(Err(Unfunded::Insufficient))
}

/// This block's settlement payload within `MAX_BLOCK_DATA_BYTES` (§7.2). `headers` and `bodies` are the headers and
/// bodies of other blocks waiting to be carried (D-063, D-077) followed by this block's header; when they do not fit
/// together with this block's entries, they are cut down to this block's header (later payloads or a republish carry
/// the rest, D-047). `None` when even that does not fit: only forced-inclusion entries can take that much room (§7.4;
/// the producer adds other entries only while they fit), so the block needs no body and carries no settlement
/// transactions (M26).
fn own_payload(
    params: &Params,
    headers: &mut Vec<BlockHeader>,
    bodies: &mut Vec<Body>,
    entries: &[Entry],
) -> Result<Option<Vec<u8>>, NodeError> {
    loop {
        match ksana_btc::payload::payload(params, headers.clone(), entries, bodies.clone()) {
            Ok(payload) => return Ok(Some(payload)),
            Err(PayloadError::TooLarge(len)) if headers.len() > 1 => {
                eprintln!(
                    "[ksana-node] with the {} headers and {} bodies waiting to be carried the settlement payload would take {len} bytes, more than MAX_BLOCK_DATA_BYTES: carrying this block's data only",
                    headers.len().saturating_sub(1),
                    bodies.len()
                );
                *headers = headers.split_off(headers.len().saturating_sub(1));
                bodies.clear();
            }
            Err(PayloadError::TooLarge(len)) if !settlement::needs_body(entries) => {
                eprintln!(
                    "[ksana-node] the settlement payload of this block would take {len} bytes, more than MAX_BLOCK_DATA_BYTES (forced-inclusion entries, §7.4): it carries no settlement transactions"
                );
                return Ok(None);
            }
            Err(e) => return Err(NodeError::Internal(e.to_string())),
        }
    }
}

/// Includes path B entries in candidate order: an op rejected by the execution layer (it would make the block invalid)
/// is skipped along with all later ops of the same account; the total data of the settlement payload stays within the
/// budget (§7.2 `MAX_BLOCK_DATA_BYTES`).
fn select(
    builder: &mut BlockBuilder<'_>,
    candidates: &[VmOp],
    mut budget: usize,
    entries: &mut Vec<Entry>,
) -> Result<(), BlockError> {
    let mut blocked = BTreeSet::new();
    for op in candidates {
        if blocked.contains(&op.account) {
            continue;
        }
        let account = op.account;
        let entry = Entry::Sponsored { op: op.clone() };
        let size =
            settlement::entry_data_len(&entry).map_err(|e| BlockError::Fatal(e.to_string()))?;
        if size > budget {
            blocked.insert(account);
            continue;
        }
        match builder.push(&entry) {
            Ok(_) => {
                budget = budget.saturating_sub(size);
                entries.push(entry);
            }
            Err(BlockError::Invalid(_)) => {
                blocked.insert(account);
            }
            Err(fatal) => return Err(fatal),
        }
    }
    Ok(())
}
