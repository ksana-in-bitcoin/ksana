//! Rescue settlement and republishing of block headers and bodies (SPEC §7.1, §7.3; D-045, D-047, D-077): local policy,
//! funded by this node's funding wallet. The BTC thread calls [`Rescuer::run_once`] after every round of syncing and
//! settlement broadcasting.
//!
//! - Rescue settlement: when a block accepted by the main chain with `SETTLEMENT_DEPTH` reached carries settlement
//!   transactions that cannot enter the mempool (the producer's funding inputs were spent, etc.), that confirmed in a
//!   BTC block without counting (at or below the anchor of a header they carry, after a BTC reorg, D-100;
//!   `confirmed_uncounted`), or that confirmed and counted but landed nothing because a header their payload needs
//!   from outside it had not counted, once those headers have counted, also when a later payload has counted the
//!   block's header meanwhile (D-135; `stalled`), and the data it carries has not all landed yet
//!   ([`Chain::carried_on_chain`]), rebuild them with this node's funding, payload unchanged (the
//!   path A user input/output pairs were removed, D-078), once the next BTC block is above the block's anchor so that
//!   the rebuilt payload counts. The order follows the fast-lane
//!   main chain (D-057): blocks produced by this node are rebuilt immediately; when the k-th main-chain block (k ≤
//!   [`RESCUE_ORDER_WINDOW`], counting from the main-chain block that accepted it, or from the block after it for a
//!   block on the main chain) was produced by this node, wait k BTC blocks after finding it cannot be broadcast; other
//!   nodes wait `RESCUE_ORDER_WINDOW + RESCUE_DELAY`. Before initiating, check the BTC mempool: when a transaction
//!   claiming to settle that block already exists (another node's rescue), do not initiate again. A claim counts only
//!   when it carries exactly the block's payload (it lands the same data) in a commit / reveal pair funded by confirmed
//!   outputs at a fee rate not below this node's, and only for [`CLAIM_PATIENCE`] BTC blocks after this node first saw
//!   it (M26: anyone can put a transaction naming the block in the mempool). The block's own settlement transactions
//!   waiting in the mempool hold the rescue back on the same terms (D-102): a pair paying less than this node's fee
//!   rate, or one that has waited [`CLAIM_PATIENCE`] BTC blocks, counts as stuck, and the order above starts then. The
//!   producer does not rescue its own block's waiting settlement; it bumps it (below).
//! - Republishing block headers and bodies: in the main chain's acceptance order (each main-chain block preceded by the
//!   blocks it merges, D-063), when the first block whose data has not landed carries no settlement transactions and
//!   later settlement payloads will not include it any more (the first later block carrying settlement transactions
//!   with it in its past is more than `MAX_SETTLEMENT_HEADERS − 1` away, or that block's settlement has confirmed and
//!   counted without carrying it — the producer carried fewer headers or bodies because of the payload size, D-075,
//!   D-077, and its own data may be waiting for those headers, D-135; or there is
//!   no such block yet but there already are at least `MAX_SETTLEMENT_HEADERS − 1` blocks, or its anchor has lagged the
//!   BTC tip for a long time, or a lighter competing chain recently appeared on BTC ([`Chain::btc_competitor`], D-091:
//!   this node's chain is not landing in time and the other side's blocks might be decided final first; then do not
//!   wait for `SETTLEMENT_DEPTH` to be reached), send a `Headers` transaction containing only the headers that
//!   settlement payload cannot fit, plus the bodies of those among them that need one. Blocks carrying settlement
//!   transactions that are parallel to it (possibly after it in acceptance order) will not carry it and do not count.
//!   While this node's republishes wait in the mempool, the next run (the blocks they do not carry) goes out chained on them, its
//!   commit transaction spending the change of the last one's reveal first (D-140), so BTC confirms them, reveals
//!   included, in order; at most
//!   [`MAX_CHAINED_REPUBLISHES`] are in flight, and none is added once the last one's change is spent (D-139).
//!
//! - Fee rate and CPFP bumps (D-102, D-117): every transaction this node sends pays the rate of its fee policy
//!   ([`crate::producer::FeePolicy::rate`]: bitcoind's estimate for the confirmation target, kept within the floor and
//!   the cap, or the floor while bitcoind's mempool holds at most half a block; the next-block estimate while a
//!   lighter competing chain is on BTC, D-091, whatever room the mempool has). A commit / reveal pair this node sent
//!   (the settlement of a block it produced, a rescue, a republish) that has waited in the mempool for [`BUMP_AFTER`]
//!   BTC blocks (at once while that competing chain is there) and pays less than the estimated rate
//!   ([`crate::producer::FeePolicy::estimated_rate`], whether or not the mempool has room: the floor was not enough
//!   for it) gets a child spending the reveal's change (D-140), lifting the package to that rate;
//!   once it has waited those blocks while the mempool had room, outside a race, to at least twice the pair's own
//!   rate, doubling for every further such block, up to the cap, so that the bump does not depend on the estimate
//!   being above the pair's rate ([`crate::producer::FeePolicy::escalated`], D-117). When the rate rises again, a
//!   child paying more replaces it (full RBF, Bitcoin Core 28). Only this node's key spends the change, so no other
//!   party can attach a child first and pin the pair. A pair whose change a chained republish spent is lifted through
//!   the last link's child instead, and a pair whose block's carried data has all landed is not bumped.
//!
//! - Funding UTXO pool (D-075): settlement transactions can only be funded by confirmed UTXOs and must not spend each
//!   other, so when fewer than [`Funding::pool_target`] funding UTXOs are available, the largest one is split into
//!   several equal outputs (each at least [`FUNDING_SPLIT_MIN_SAT`] sats); no further split until the split transaction
//!   confirms.
//!
//! - Revenue sweep (D-142): with a revenue address configured (D-060), once the available funding UTXOs hold at least
//!   [`REVENUE_SWEEP_MIN_SAT`] and a tenth of the hot wallet cap above the cap, one transaction moves that excess to
//!   the revenue address; no further sweep until it confirms. The change of every pair returns to the funding address,
//!   so every pair can be bumped and chained.
//!
//! Rescue settlements, header republishing, CPFP children, split and sweep transactions reserve their funding inputs with
//! [`Chain::try_reserve_inputs`] after choosing the funding and before broadcasting (mutually exclusive with the block
//! production loop); while in flight they are excluded from the producer's funding; they are released when rejected or
//! abandoned. Rescue settlements and republishing are both commit / reveal transaction pairs (D-081, the envelope using
//! this node's funding key), tracked by the reveal transaction.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use bitcoin::hashes::{sha256, Hash, HashEngine};
use bitcoin::Transaction;
use ksana_btc::client::BtcClient;
use ksana_btc::payload::{attached_payload, entry_data, headers_payload_fixed_len};
use ksana_btc::settlement::{
    cpfp_child, finalize, split_funding, sweep_revenue, validate_attached, ProducerUtxo,
    SettlementError, SettlementPair, CHANGE_OUTPUT,
};
use ksana_btc::tracker::{BlockSummary, BtcSnapshot};
use ksana_primitives::{compact, Block, BlockHeader, Body, Payload};

use crate::chain::{test_settlement, Chain};
use crate::producer::{fund, Funding};
use crate::{settlement, NodeError};

/// Window of the rescue order (D-057): when the k-th block after the block (k ≤ this value) was produced by this node,
/// this node waits k BTC blocks after finding the settlement transactions cannot be broadcast.
pub const RESCUE_ORDER_WINDOW: u32 = 6;
/// Number of further BTC blocks a node outside the rescue order window waits after [`RESCUE_ORDER_WINDOW`] (D-045,
/// D-057).
pub const RESCUE_DELAY: u32 = 2;
/// Number of BTC blocks a settlement claim in the mempool holds this node's rescue back after this node first saw it
/// (local policy, M26): as long as a node outside the rescue order waits. A claim that has not confirmed by then is
/// treated as a rescue that never came.
pub const CLAIM_PATIENCE: u32 = RESCUE_ORDER_WINDOW.saturating_add(RESCUE_DELAY);
/// Number of BTC blocks another sender's republish in the mempool holds this node's back, for the headers and bodies it
/// carries, after this node first saw it (local policy, D-143): as long as this node lets a pair of its own wait before
/// it bumps it. One that has not confirmed by then is treated as a republish that never came.
pub const REPUBLISH_PATIENCE: u32 = BUMP_AFTER;
/// A node starting a chain of republishes with a block it did not produce first waits one to [`REPUBLISH_YIELD_SLOTS`]
/// times this long, and then sends what the republishes that reached the mempool meanwhile leave (local policy, D-143):
/// nodes that find the same run due when the same BTC block arrives do not all send it.
pub const REPUBLISH_YIELD: Duration = Duration::from_secs(10);
/// The number of waits a node's turn is drawn from ([`REPUBLISH_YIELD`]).
const REPUBLISH_YIELD_SLOTS: u8 = 6;
/// Maximum number of newly seen mempool transactions fetched from bitcoind per round (D-057): the mainnet mempool is
/// large and takes several rounds to scan, during which no rescue is initiated.
const MEMPOOL_SCAN_BATCH: usize = 1_000;
/// Republish headers when no later settlement can carry them: the anchor of the first header not landed lags the BTC
/// tip by more than this multiple of `SETTLEMENT_TIMEOUT` (D-047).
const QUIET_REPUBLISH_FACTOR: u32 = 4;
/// Confirmations needed for a coinbase output to mature.
const COINBASE_MATURITY: u32 = 100;
/// Minimum amount of each output split off for the funding UTXO pool (sats, D-075).
pub const FUNDING_SPLIT_MIN_SAT: u64 = 100_000;
/// The least excess above the hot wallet cap a revenue sweep moves (sats, D-142); it also waits for a tenth of the cap.
pub const REVENUE_SWEEP_MIN_SAT: u64 = 100_000;
/// BTC blocks a pair this node sent waits unconfirmed in the mempool before it is bumped by CPFP (local policy, D-102);
/// none while a lighter competing chain is on BTC (D-091).
pub const BUMP_AFTER: u32 = 2;
/// The fee rate (sat/vB) a child replacing an earlier child pays on top of that one's fee (Bitcoin Core's
/// `-incrementalrelayfee` default).
const INCREMENTAL_RELAY_FEERATE: u64 = 1;
/// At most this many republishes of this node's are in flight at once, each chained on the change of the one before
/// (D-139): four full payloads stay below Bitcoin Core's default limit of 101 kvB on a cluster of unconfirmed
/// transactions.
pub const MAX_CHAINED_REPUBLISHES: usize = 4;

/// Transactions sent by this node and not yet confirmed: commit and reveal for a rescue settlement or a republish
/// (D-081), a single one for a split.
struct Sent {
    /// ID of the last one (the reveal or the split transaction): when it is in the mempool or confirmed, so are the
    /// ones before it.
    txid: [u8; 32],
    /// In broadcast order (parent transactions first).
    txs: Vec<Transaction>,
    /// Reserved funding inputs.
    inputs: Vec<([u8; 32], u32)>,
}

impl Sent {
    fn pair(pair: SettlementPair) -> Self {
        Sent {
            txid: pair.txid(),
            inputs: settlement::funding_inputs(&pair.commit),
            txs: vec![pair.commit, pair.reveal],
        }
    }

    fn refs(&self) -> Vec<&Transaction> {
        self.txs.iter().collect()
    }

    /// The IDs of all its transactions.
    fn txids(&self) -> Vec<[u8; 32]> {
        self.txs
            .iter()
            .map(|tx| tx.compute_txid().to_byte_array())
            .collect()
    }

    /// Display ID of the last transaction.
    fn shown(&self) -> String {
        self.txs
            .last()
            .map(|tx| tx.compute_txid().to_string())
            .unwrap_or_default()
    }
}

/// Whether a settlement transaction (`txid`, the reveal) confirmed on the BTC best chain in a block this snapshot has
/// scanned without counting (D-100: a payload counts only in a BTC block above every header it carries, see
/// [`ksana_btc::tracker::payload_counts`]; second audit P4b-1). It then never lands the data it carries, although
/// bitcoind knows it and a rebroadcast reports success: an honest block's settlement can end up so when a BTC reorg
/// re-mines it at or below the block's anchor, and only a rescue, with the payload confirmed higher, lands it. A
/// transaction confirmed in a block the snapshot has not scanned yet counts as not (yet) known to be uncounted.
fn confirmed_uncounted(
    client: &dyn BtcClient,
    snapshot: &BtcSnapshot,
    txid: &[u8; 32],
) -> Result<bool, NodeError> {
    if snapshot.ksn1_height(txid).is_some() {
        return Ok(false);
    }
    let Some(block) = client.transaction(txid).map_err(btc)?.and_then(|t| t.block) else {
        return Ok(false);
    };
    let Some(info) = client.header(&block).map_err(btc)? else {
        return Ok(false);
    };
    Ok(info.in_best_chain && snapshot.hash_at(info.height) == Some(block))
}

/// Whether a settlement transaction (`txid`, the reveal) of block `block`, whose data has not landed, with the payload
/// headers `headers`, confirmed and counted but landed nothing, and lands it if sent again (D-135): a header the
/// payload needs from outside it counted only after it confirmed, or the block's own header did (through a later
/// payload: a body lands only at or above the height at which its header counted), and every header it needs from
/// outside has counted since. The payload then goes out again unchanged; while one of them has not counted, it waits for
/// later payloads to carry them. One that confirmed after all of them had counted would land no more if sent again.
fn stalled(
    snapshot: &BtcSnapshot,
    genesis: &[u8; 32],
    txid: &[u8; 32],
    block: &[u8; 32],
    headers: &[BlockHeader],
) -> bool {
    let Some(confirmed) = snapshot.ksn1_height(txid) else {
        return false;
    };
    let inside: BTreeSet<[u8; 32]> = headers.iter().map(BlockHeader::hash).collect();
    let mut latest = snapshot.counted_at(block);
    for parent in headers.iter().flat_map(|header| &header.parents) {
        if inside.contains(parent) || parent == genesis {
            continue;
        }
        match snapshot.counted_at(parent) {
            Some(height) => latest = latest.max(Some(height)),
            None => return false,
        }
    }
    latest.is_some_and(|height| height > confirmed)
}

/// A transaction seen in the BTC mempool (D-057).
struct Seen {
    /// The fast-lane block it claims to settle (hash of the last header of its settlement payload); `None` when it is
    /// not a settlement transaction.
    block: Option<[u8; 32]>,
    /// The blocks whose headers its republish payload carries, each with whether the payload also carries the block's
    /// body as this node stores it (D-143); empty when it is not a republish.
    republished: Vec<([u8; 32], bool)>,
    /// The parents of those headers that the payload does not carry: nothing of it lands before they have counted
    /// (D-135).
    needs: Vec<[u8; 32]>,
    /// The BTC tip height when this node first saw it.
    since: u32,
    /// Whether it has left the mempool: a republish is then kept while bitcoind is ahead of the scanned tip (D-143).
    left: bool,
    /// Whether the claim holds this node's rescue back ([`Rescuer::honors`]), or the republish pays this node's fee rate
    /// ([`Rescuer::republish_pays`]), once checked.
    honored: Option<bool>,
}

/// A pair this node sent (the settlement of a block it produced, a rescue or a republish) waiting in the BTC mempool
/// (D-102).
struct Bump {
    /// The BTC tip height when this node first saw it waiting.
    since: u32,
    /// The BTC tip height since which every round of this node has seen it waiting while bitcoind's mempool had room
    /// (D-117); `None` after a round without room.
    room_since: Option<u32>,
    /// The CPFP child sent for it, and the child's fee (sats).
    child: Option<(Sent, u64)>,
}

/// What a pair this node sent lands.
#[derive(Clone, Copy)]
enum Lands {
    /// The data of a block: the block's own settlement, or a rescue of it ([`State::rescues`]).
    Block([u8; 32]),
    /// A republish ([`State::headers`]).
    Headers,
}

#[derive(Default)]
struct State {
    /// Block hash → BTC tip height when its settlement transactions were first found unable to enter the mempool.
    stuck_since: BTreeMap<[u8; 32], u32>,
    /// Block hash → BTC tip height when this node first saw the block's own settlement transactions waiting in the
    /// mempool (D-102).
    waiting: BTreeMap<[u8; 32], u32>,
    /// Reveal txid of a pair this node sent → its CPFP bump (D-102).
    bumps: BTreeMap<[u8; 32], Bump>,
    /// Block hash → rescue settlement sent by this node.
    rescues: BTreeMap<[u8; 32], Sent>,
    /// Competing-chain blocks already republished for (or at least logged) (D-091), so the log is not repeated every
    /// round.
    competitors: BTreeSet<[u8; 32]>,
    /// Hash of the first republished header → `Headers` transaction sent by this node.
    headers: BTreeMap<[u8; 32], Sent>,
    /// Seen mempool transactions (D-057).
    mempool: BTreeMap<[u8; 32], Seen>,
    /// Funding UTXO split transaction sent by this node and not yet confirmed (D-075).
    split: Option<Sent>,
    /// Revenue sweep transaction sent by this node and not yet confirmed (D-142).
    sweep: Option<Sent>,
    /// The first header of the republish this node is waiting its turn to send, and since when (D-143).
    yielding: Option<([u8; 32], Instant)>,
    /// This node's fee rate (sat/vB) the republishes of [`State::mempool`] were last checked against
    /// ([`Rescuer::republish_pays`], D-143).
    republish_feerate: u64,
}

/// Rescue settlement and header republishing.
pub struct Rescuer {
    chain: Arc<Chain>,
    funding: Funding,
    producer: Option<[u8; 32]>,
    /// Payee account of republish payloads (D-095): defaults to the producer account; all zeros for a non-producing
    /// node (no carrier reward collected).
    payee: [u8; 32],
    /// The unit of the wait before a chain of republishes this node is not the first to send ([`REPUBLISH_YIELD`]).
    republish_yield: Duration,
    state: Mutex<State>,
}

fn btc(e: impl std::fmt::Display) -> NodeError {
    NodeError::Btc(e.to_string())
}

impl Rescuer {
    /// `producer` is the producer public key when this node produces blocks (its blocks are rescued immediately);
    /// `payee` is the payee account of republish payloads (D-095), the producer account when `None`, and all zeros when
    /// there is neither (no carrier reward collected).
    pub fn new(
        chain: Arc<Chain>,
        funding: Funding,
        producer: Option<[u8; 32]>,
        payee: Option<[u8; 32]>,
    ) -> Self {
        Rescuer {
            chain,
            funding,
            producer,
            payee: payee.or(producer).unwrap_or([0; 32]),
            republish_yield: REPUBLISH_YIELD,
            state: Mutex::new(State::default()),
        }
    }

    /// Replaces [`REPUBLISH_YIELD`] (zero: this node never waits its turn to republish).
    pub fn with_republish_yield(mut self, unit: Duration) -> Self {
        self.republish_yield = unit;
        self
    }

    fn state(&self) -> Result<MutexGuard<'_, State>, NodeError> {
        self.state
            .lock()
            .map_err(|_| NodeError::Internal("rescue state lock poisoned".into()))
    }

    /// Handles one round: rescue settlements, header and body republishing, CPFP bumps of this node's waiting pairs,
    /// then maintenance of the funding UTXO pool; all at the fee rate of this node's policy now (D-102, D-117), urgent
    /// while a lighter competing chain is on BTC (D-091), the bumps at the estimated rate, or while the mempool has
    /// room at least an escalation of a pair's own rate (D-117).
    pub fn run_once(&self) -> Result<(), NodeError> {
        let snapshot = self.chain.btc().snapshot()?;
        let Some((btc_tip, _)) = snapshot.tip() else {
            return Ok(());
        };
        let params = self.chain.params();
        let client = self.chain.btc().client().as_ref();
        let fees = &self.funding.fees;
        let competitor = self
            .chain
            .btc_competitor(&snapshot, params.btc_finality_confs)?;
        let urgent = competitor.is_some();
        // A pair the room rate left waiting for BUMP_AFTER BTC blocks is bumped to the estimate, or past its own rate
        // while the room is still there (D-117)
        let estimated = fees.estimated_rate(params, client, urgent);
        let room = if urgent {
            None
        } else {
            fees.room_rate(params, client)
        };
        let feerate = room.unwrap_or(estimated);
        self.rescue_settlements(&snapshot, btc_tip, feerate)?;
        self.republish(&snapshot, btc_tip, feerate, competitor)?;
        self.bump(&snapshot, btc_tip, estimated, urgent, room.is_some())?;
        // A funding split does not race: it keeps the policy's own target while a competing chain is on BTC
        let split_rate = match competitor {
            Some(_) => fees.rate(params, client, false),
            None => feerate,
        };
        self.maintain_pool(btc_tip, split_rate)?;
        self.maintain_revenue(btc_tip, split_rate)
    }

    /// Revenue sweep (D-142, local policy): once the available funding UTXOs exceed the hot wallet cap of D-060 by at
    /// least [`REVENUE_SWEEP_MIN_SAT`] and a tenth of the cap, one transaction moves the excess, less its fee, to the
    /// revenue address, spending the largest UTXOs that cover it and returning the rest to the funding address; no
    /// further sweep until it confirms.
    fn maintain_revenue(&self, btc_tip: u32, feerate: u64) -> Result<(), NodeError> {
        let Some(revenue) = &self.funding.revenue else {
            return Ok(());
        };
        let chain = &self.chain;
        let client = chain.btc().client();
        let mut state = self.state()?;
        if !self.settled(client.as_ref(), &mut state.sweep)? {
            return Ok(());
        }
        let utxos = self.available_utxos(btc_tip)?;
        let total = utxos
            .iter()
            .map(|u| u.txout.value.to_sat())
            .fold(0u64, u64::saturating_add);
        let excess = total.saturating_sub(revenue.hot_cap_sat);
        let least = REVENUE_SWEEP_MIN_SAT.max(revenue.hot_cap_sat.checked_div(10).unwrap_or(0));
        if excess < least {
            return Ok(());
        }
        let mut inputs = Vec::new();
        let mut covered = 0u64;
        for u in &utxos {
            if covered >= excess {
                break;
            }
            covered = covered.saturating_add(u.txout.value.to_sat());
            inputs.push(u.clone());
        }
        let swept = sweep_revenue(
            &inputs,
            excess,
            revenue.script.clone(),
            feerate,
            &self.funding.keypair,
        )
        .map_err(|e| {
            NodeError::Internal(format!(
                "assembling the revenue sweep transaction failed: {e}"
            ))
        })?;
        let Some(tx) = swept else {
            return Ok(());
        };
        if let Err(reason) = client.test_accept(&[&tx]).map_err(btc)? {
            eprintln!(
                "[ksana-node] testmempoolaccept rejected the revenue sweep transaction: {reason}"
            );
            return Ok(());
        }
        let sent = Sent {
            txid: tx.compute_txid().to_byte_array(),
            inputs: inputs
                .iter()
                .map(|u| (u.outpoint.txid.to_byte_array(), u.outpoint.vout))
                .collect(),
            txs: vec![tx],
        };
        if !chain.try_reserve_inputs(&sent.inputs)? {
            return Ok(());
        }
        match client.broadcast_all(&sent.refs()) {
            Ok(Ok(())) => {
                eprintln!(
                    "[ksana-node] revenue sweep: {excess} sats above the hot wallet cap to the revenue address txid={}",
                    sent.shown()
                );
                state.sweep = Some(sent);
            }
            Ok(Err(reason)) => {
                chain.release_inputs(&sent.inputs)?;
                eprintln!(
                    "[ksana-node] broadcasting the revenue sweep transaction failed: {reason}"
                );
            }
            Err(e) => {
                // Outcome unknown: track as usual and verify next round
                state.sweep = Some(sent);
                return Err(btc(e));
            }
        }
        Ok(())
    }

    /// Funding UTXO pool (D-075, local policy): when fewer than [`Funding::pool_target`] funding UTXOs are available,
    /// split the largest one into several equal outputs to make up the target (fewer parts when the amount is
    /// insufficient, each at least [`FUNDING_SPLIT_MIN_SAT`] sats); no further split until the split transaction
    /// confirms.
    fn maintain_pool(&self, btc_tip: u32, feerate: u64) -> Result<(), NodeError> {
        let target = self.funding.pool_target;
        if target == 0 {
            return Ok(());
        }
        let chain = &self.chain;
        let client = chain.btc().client();
        let mut state = self.state()?;
        if !self.settled(client.as_ref(), &mut state.split)? {
            return Ok(());
        }
        let utxos = self.available_utxos(btc_tip)?;
        if utxos.len() >= target {
            return Ok(());
        }
        let Some(largest) = utxos.first() else {
            return Ok(());
        };
        let mut count = target.saturating_sub(utxos.len()).saturating_add(1);
        let tx = loop {
            if count < 2 {
                return Ok(());
            }
            let split = split_funding(
                largest,
                count,
                FUNDING_SPLIT_MIN_SAT,
                feerate,
                &self.funding.keypair,
            )
            .map_err(|e| {
                NodeError::Internal(format!(
                    "assembling the funding split transaction failed: {e}"
                ))
            })?;
            match split {
                Some(tx) => break tx,
                None => count = count.saturating_sub(1),
            }
        };
        if let Err(reason) = client.test_accept(&[&tx]).map_err(btc)? {
            eprintln!(
                "[ksana-node] testmempoolaccept rejected the funding split transaction: {reason}"
            );
            return Ok(());
        }
        let sent = Sent {
            txid: tx.compute_txid().to_byte_array(),
            inputs: vec![(largest.outpoint.txid.to_byte_array(), largest.outpoint.vout)],
            txs: vec![tx],
        };
        if !chain.try_reserve_inputs(&sent.inputs)? {
            return Ok(());
        }
        match client.broadcast_all(&sent.refs()) {
            Ok(Ok(())) => {
                eprintln!(
                    "[ksana-node] funding UTXO pool: {} available, splitting into {count} txid={}",
                    utxos.len(),
                    sent.shown()
                );
                state.split = Some(sent);
            }
            Ok(Err(reason)) => {
                chain.release_inputs(&sent.inputs)?;
                eprintln!(
                    "[ksana-node] broadcasting the funding split transaction failed: {reason}"
                );
            }
            Err(e) => {
                // Outcome unknown: track as usual and verify next round
                state.split = Some(sent);
                return Err(btc(e));
            }
        }
        Ok(())
    }

    /// Available funding UTXOs: confirmed, not an immature coinbase, not reserved, still unspent (including the
    /// mempool); by amount from largest to smallest.
    fn available_utxos(&self, btc_tip: u32) -> Result<Vec<ProducerUtxo>, NodeError> {
        let client = self.chain.btc().client();
        let reserved = self.chain.reserved_inputs()?;
        let mut utxos = Vec::new();
        for u in client.scan_utxos(&self.funding.script).map_err(btc)? {
            if u.coinbase && btc_tip.saturating_sub(u.height).saturating_add(1) < COINBASE_MATURITY
            {
                continue;
            }
            if reserved.contains(&(u.outpoint.txid.to_byte_array(), u.outpoint.vout)) {
                continue;
            }
            if client.utxo(&u.outpoint, true).map_err(btc)?.is_none() {
                continue;
            }
            utxos.push(ProducerUtxo {
                outpoint: u.outpoint,
                txout: u.txout,
            });
        }
        utxos.sort_by_key(|u| std::cmp::Reverse(u.txout.value));
        Ok(utxos)
    }

    /// Whether nothing is left of the split or sweep transaction tracked in `slot`: none is tracked, or it has
    /// confirmed (its inputs are spent, no longer reserved). False while it is unconfirmed and in the round that
    /// abandons it. The record is dropped only with its reservation, so an RPC error leaves both for the next round.
    fn settled(&self, client: &dyn BtcClient, slot: &mut Option<Sent>) -> Result<bool, NodeError> {
        let Some(sent) = slot.as_ref() else {
            return Ok(true);
        };
        let confirmed = client
            .transaction(&sent.txid)
            .map_err(btc)?
            .is_some_and(|info| info.block.is_some());
        if confirmed {
            self.chain.release_inputs(&sent.inputs)?;
        } else if self.keep_alive(client, sent)? {
            return Ok(false);
        }
        *slot = None;
        Ok(confirmed)
    }

    /// Keeps in-flight transactions alive: rebroadcasts them (in order) when not in the mempool, gives up when rejected
    /// (returns false and releases the reserved inputs).
    fn keep_alive(&self, client: &dyn BtcClient, sent: &Sent) -> Result<bool, NodeError> {
        if client.in_mempool(&sent.txid).map_err(btc)? {
            return Ok(true);
        }
        match client.broadcast_all(&sent.refs()).map_err(btc)? {
            Ok(()) => Ok(true),
            Err(reason) => {
                eprintln!(
                    "[ksana-node] abandoning transaction {} sent by this node: {reason}",
                    sent.shown()
                );
                self.chain.release_inputs(&sent.inputs)?;
                Ok(false)
            }
        }
    }

    fn rescue_settlements(
        &self,
        snapshot: &BtcSnapshot,
        btc_tip: u32,
        feerate: u64,
    ) -> Result<(), NodeError> {
        let chain = &self.chain;
        let params = chain.params();
        let client = chain.btc().client();
        let tip_height = chain.tip()?.header.height;
        let open: BTreeMap<[u8; 32], _> = chain.open_settlements()?.into_iter().collect();
        let mut state = self.state()?;
        state.stuck_since.retain(|h, _| open.contains_key(h));
        state.waiting.retain(|h, _| open.contains_key(h));
        let closed: Vec<[u8; 32]> = state
            .rescues
            .keys()
            .filter(|h| !open.contains_key(*h))
            .copied()
            .collect();
        // A closed rescue may still be in the BTC mempool (its record closed because another transaction landed the
        // data, or its block left the main chain and a BTC reorg put it back there): its inputs stay reserved until it
        // is not (D-104)
        for hash in closed {
            if let Some(sent) = state.rescues.remove(&hash) {
                chain.linger(sent.txids(), &sent.inputs)?;
            }
        }

        for (block_hash, record) in open {
            if chain.accepted_height(&block_hash)? != Some(record.height)
                || tip_height < record.height.saturating_add(params.settlement_depth)
            {
                continue;
            }
            let Some(block) = chain.store().block(&block_hash)? else {
                continue;
            };
            if chain.carried_on_chain(&block, snapshot)?.is_some() {
                // All carried data has landed (this node's or someone else's rescue, the original settlement, or
                // another payload, D-077)
                state.stuck_since.remove(&block_hash);
                continue;
            }
            let Some(txs) = &block.settlement else {
                continue;
            };
            let attached = settlement::decode(txs)?;
            let attached_txid = attached.txid();
            let Some(payload) = settlement::attached_payload(params, &attached) else {
                continue;
            };
            let Ok(Payload::Settlement { headers, .. }) = Payload::decode(&payload, params) else {
                continue;
            };
            let genesis = chain.genesis().hash();
            if let Some(sent) = state.rescues.get(&block_hash) {
                let again = if confirmed_uncounted(client.as_ref(), snapshot, &sent.txid)? {
                    Some("confirmed without counting (D-100)")
                } else if stalled(snapshot, &genesis, &sent.txid, &block_hash, &headers) {
                    Some("counted before a header it needs did, and landed nothing (D-135)")
                } else {
                    None
                };
                if let Some(why) = again {
                    eprintln!(
                        "[ksana-node] the rescue settlement {} (height {}) {why}; rescuing again",
                        sent.shown(),
                        record.height
                    );
                    chain.release_inputs(&sent.inputs)?;
                } else if snapshot.ksn1_height(&sent.txid).is_some() {
                    // Confirmed and counted without landing the data: it goes out again once the headers it waits for
                    // have counted, and not at all when it confirmed after them (D-135)
                    continue;
                } else if self.keep_alive(client.as_ref(), sent)? {
                    continue;
                }
                state.rescues.remove(&block_hash);
            }
            // Confirmed and counted without landing the data it carries: it waits for the headers it needs from outside
            // its payload, then goes out again (D-135)
            let stalled = stalled(snapshot, &genesis, &attached_txid, &block_hash, &headers);
            if snapshot.ksn1_height(&attached_txid).is_some() && !stalled {
                state.stuck_since.remove(&block_hash);
                state.waiting.remove(&block_hash);
                continue;
            }
            let waiting = client.in_mempool(&attached_txid).map_err(btc)?;
            if waiting {
                // The block's own settlement waiting in the mempool holds the rescue back like another party's claim
                // (D-102): only while it pays this node's fee rate, and for CLAIM_PATIENCE BTC blocks after this node
                // first saw it waiting. This node bumps the settlements of its own blocks by CPFP instead
                // ([`Rescuer::bump`])
                let since = *state.waiting.entry(block_hash).or_insert(btc_tip);
                let holds = self.producer == Some(block.header.producer)
                    || (btc_tip < since.saturating_add(CLAIM_PATIENCE)
                        && self.pays(&attached, &payload, feerate)?);
                if holds {
                    state.stuck_since.remove(&block_hash);
                    continue;
                }
            } else {
                state.waiting.remove(&block_hash);
            }
            // Able to enter the mempool; bitcoind knowing the transactions is not enough when they confirmed without
            // counting: those never land the data, and only a rescue does (second audit P4b-1)
            let uncounted =
                !waiting && confirmed_uncounted(client.as_ref(), snapshot, &attached_txid)?;
            if !waiting
                && !uncounted
                && !stalled
                && test_settlement(client.as_ref(), &attached)
                    .map_err(btc)?
                    .is_ok()
            {
                state.stuck_since.remove(&block_hash);
                continue;
            }
            // The settlement transactions carried by the block cannot enter the mempool, wait there paying too little
            // or for too long, confirmed without counting, or counted and landed nothing: wait in the order set by the
            // fast-lane main chain (D-057)
            let since = *state.stuck_since.entry(block_hash).or_insert(btc_tip);
            if btc_tip < since.saturating_add(self.rescue_delay(&block, record.height)?) {
                continue;
            }
            // The rebuilt payload counts only in a BTC block above every header it carries, which anchor at or below
            // the block's own anchor (D-100): when BTC has not passed the anchor (a reorg to a shorter chain), wait
            if block.header.btc_anchor.height > btc_tip {
                continue;
            }
            // A node ahead in the order has already rescued (its transaction is in the mempool), or the mempool scan is
            // not finished: do not initiate this round. The block's own settlement was weighed above
            if self.mempool_settles(
                &mut state,
                &block_hash,
                &payload,
                btc_tip,
                feerate,
                &attached_txid,
            )? != Some(false)
            {
                continue;
            }
            let what = format!("the rescue settlement (height {})", block.header.height);
            let Some(sent) = self.build_pair(&payload, btc_tip, feerate, &what)? else {
                continue;
            };
            // Reserve the funding before broadcasting: the block production loop may pick the same funding output at
            // the same time; when it is taken, choose again next round
            if !chain.try_reserve_inputs(&sent.inputs)? {
                continue;
            }
            match client.broadcast_all(&sent.refs()) {
                Ok(Ok(())) => {
                    let why = if uncounted {
                        "confirmed without counting, D-100"
                    } else if stalled {
                        "counted before a header they need did, and landed nothing, D-135"
                    } else if waiting {
                        "wait in the BTC mempool paying less than this node's fee rate, or for too long, D-102"
                    } else {
                        "cannot be broadcast"
                    };
                    eprintln!(
                        "[ksana-node] rescue settlement height={} reveal txid={} (the original settlement transactions {why})",
                        record.height,
                        sent.shown()
                    );
                    state.rescues.insert(block_hash, sent);
                }
                Ok(Err(reason)) => {
                    chain.release_inputs(&sent.inputs)?;
                    eprintln!(
                        "[ksana-node] broadcasting the rescue settlement (height {}) failed: {reason}",
                        record.height
                    );
                }
                Err(e) => {
                    // Outcome unknown (the transactions may have entered the mempool): track as usual; keep_alive
                    // verifies next round
                    state.rescues.insert(block_hash, sent);
                    return Err(btc(e));
                }
            }
        }
        Ok(())
    }

    /// Number of BTC blocks this node waits after finding the block's settlement transactions cannot be broadcast
    /// before rescuing it (D-057, D-063): 0 for the block's producer; k when the k-th main-chain block (k ≤
    /// [`RESCUE_ORDER_WINDOW`]) counting from the main-chain block that accepted it (height `accepted`; from the block
    /// after it when the block itself is on the main chain) was produced by this node; otherwise
    /// `RESCUE_ORDER_WINDOW + RESCUE_DELAY`.
    fn rescue_delay(&self, block: &Block, accepted: u64) -> Result<u32, NodeError> {
        let last = RESCUE_ORDER_WINDOW.saturating_add(RESCUE_DELAY);
        let Some(me) = self.producer else {
            return Ok(last);
        };
        if block.header.producer == me {
            return Ok(0);
        }
        let store = self.chain.store();
        let first = if store.hash_at(accepted)? == Some(block.header.hash()) {
            accepted.saturating_add(1)
        } else {
            accepted
        };
        for k in 1..=RESCUE_ORDER_WINDOW {
            let height = first.saturating_add(u64::from(k.saturating_sub(1)));
            let Some(hash) = store.hash_at(height)? else {
                break;
            };
            let Some(next) = store.block(&hash)? else {
                break;
            };
            if next.header.producer == me {
                return Ok(k);
            }
        }
        Ok(last)
    }

    /// Whether the BTC mempool already has a KSN1 transaction claiming to settle that block (the last header of the
    /// payload is that block, D-057) that this node honors ([`Rescuer::honors`]: `payload` is the block's settlement
    /// payload) and first saw fewer than [`CLAIM_PATIENCE`] BTC blocks before `btc_tip`; `None` while the mempool scan
    /// is not finished. The block's own settlement (reveal `own`) is not a claim here: the caller weighs it with its
    /// own patience (D-102). Called only when there is a block awaiting rescue: at most [`MEMPOOL_SCAN_BATCH`] newly
    /// seen transactions are fetched per round, the conclusions are cached, and transactions that left the mempool are
    /// removed from the cache.
    fn mempool_settles(
        &self,
        state: &mut State,
        block_hash: &[u8; 32],
        payload: &[u8],
        btc_tip: u32,
        feerate: u64,
        own: &[u8; 32],
    ) -> Result<Option<bool>, NodeError> {
        if !self.scan_mempool(state, btc_tip)? {
            return Ok(None);
        }
        for (txid, seen) in state.mempool.iter_mut() {
            if txid == own
                || seen.block.as_ref() != Some(block_hash)
                || btc_tip >= seen.since.saturating_add(CLAIM_PATIENCE)
            {
                continue;
            }
            let honored = match seen.honored {
                Some(honored) => honored,
                None => *seen.honored.insert(self.honors(txid, payload, feerate)?),
            };
            if honored {
                return Ok(Some(true));
            }
        }
        Ok(Some(false))
    }

    /// Brings [`State::mempool`] up to date with the BTC mempool (D-057, D-143): the transactions that left it are
    /// forgotten (a republish only once the scanned tip has caught up with bitcoind's), and at most
    /// [`MEMPOOL_SCAN_BATCH`] newly seen ones are fetched and read. `false` while some are left for the next round.
    fn scan_mempool(&self, state: &mut State, btc_tip: u32) -> Result<bool, NodeError> {
        let client = self.chain.btc().client();
        let params = self.chain.params();
        let txids: BTreeSet<[u8; 32]> = client.mempool_txids().map_err(btc)?.into_iter().collect();
        // bitcoind may be a block ahead of the scanned tip: a republish that left the mempool has then most likely
        // confirmed in a block this node has not read yet, and is kept until it has (D-143)
        let ahead = client.tip().map_err(btc)?.0 > btc_tip;
        state.mempool.retain(|txid, seen| {
            seen.left = !txids.contains(txid);
            !seen.left || (ahead && !seen.republished.is_empty())
        });
        let mut fetched = 0usize;
        for txid in &txids {
            if state.mempool.contains_key(txid) {
                continue;
            }
            if fetched >= MEMPOOL_SCAN_BATCH {
                return Ok(false);
            }
            fetched = fetched.saturating_add(1);
            let mut seen = Seen {
                block: None,
                republished: Vec::new(),
                needs: Vec::new(),
                since: btc_tip,
                left: false,
                honored: None,
            };
            let payload = client
                .transaction(txid)
                .map_err(btc)?
                .and_then(|info| ksana_btc::data::tx_payload(params, &info.tx));
            match payload {
                Some((_, Payload::Settlement { headers, .. })) => {
                    seen.block = headers.last().map(BlockHeader::hash);
                }
                Some((
                    _,
                    Payload::Headers {
                        headers, bodies, ..
                    },
                )) => {
                    let hashes: Vec<[u8; 32]> = headers.iter().map(BlockHeader::hash).collect();
                    let genesis = self.chain.genesis().hash();
                    let needs: BTreeSet<[u8; 32]> = headers
                        .iter()
                        .flat_map(|header| header.parents.iter().copied())
                        .filter(|parent| *parent != genesis && !hashes.contains(parent))
                        .collect();
                    seen.needs = needs.into_iter().collect();
                    // A body counts only when it is the one this node stores for that block: any other lands nothing
                    let mut with_body = BTreeSet::new();
                    for body in &bodies {
                        let Some(hash) = usize::try_from(body.header)
                            .ok()
                            .and_then(|index| hashes.get(index))
                        else {
                            continue;
                        };
                        let stored = self.chain.store().block(hash)?;
                        if stored.is_some_and(|block| entry_data(&block.entries) == body.entries) {
                            with_body.insert(*hash);
                        }
                    }
                    seen.republished = hashes
                        .into_iter()
                        .map(|hash| (hash, with_body.contains(&hash)))
                        .collect();
                }
                Some((_, Payload::NativeOp { .. })) | None => {}
            }
            state.mempool.insert(*txid, seen);
        }
        Ok(true)
    }

    /// The blocks other senders' republishes in the BTC mempool carry (local policy, D-143), each with whether its body
    /// is carried too: this node does not carry them again. A republish holds this node's back when this node first saw
    /// it fewer than [`REPUBLISH_PATIENCE`] BTC blocks before `btc_tip`, it pays this node's fee rate `feerate`
    /// ([`Rescuer::republish_pays`]), and every header it needs from outside its payload has counted or is carried by a
    /// republish of this node's in flight (`covered`) or by another one that holds this node's back: it lands what it
    /// carries when it confirms. One that left the mempool holds this node's back only while it is in a BTC block the
    /// snapshot has not read ([`Rescuer::read_on_btc`]). Anyone
    /// can put such a transaction into the mempool: one that does not confirm delays this node by
    /// [`REPUBLISH_PATIENCE`] BTC blocks, and one that confirms has landed the data at its sender's cost. `None` while
    /// the mempool scan is not finished.
    fn republished_by_others(
        &self,
        state: &mut State,
        snapshot: &BtcSnapshot,
        btc_tip: u32,
        feerate: u64,
        covered: &BTreeSet<[u8; 32]>,
    ) -> Result<Option<BTreeMap<[u8; 32], bool>>, NodeError> {
        if !self.scan_mempool(state, btc_tip)? {
            return Ok(None);
        }
        // Whether a republish pays this node's rate is checked again when that rate has changed
        if state.republish_feerate != feerate {
            state.republish_feerate = feerate;
            for seen in state.mempool.values_mut() {
                if !seen.republished.is_empty() {
                    seen.honored = None;
                }
            }
        }
        let own: BTreeSet<[u8; 32]> = state.headers.values().map(|sent| sent.txid).collect();
        let mut pays: BTreeMap<[u8; 32], bool> = state
            .mempool
            .iter()
            .filter(|(_, seen)| !seen.republished.is_empty())
            .filter_map(|(txid, seen)| Some((*txid, seen.honored?)))
            .collect();
        let mut waiting = Vec::new();
        for (txid, seen) in &state.mempool {
            if seen.republished.is_empty()
                || own.contains(txid)
                || btc_tip >= seen.since.saturating_add(REPUBLISH_PATIENCE)
            {
                continue;
            }
            // One that left the mempool is waited for only while it is in a BTC block the snapshot has not read: what
            // one the snapshot has read landed is in the snapshot, and one that did not confirm lands nothing
            if seen.left && self.read_on_btc(snapshot, txid)? != Some(false) {
                continue;
            }
            if self.republish_pays(state, &mut pays, txid, feerate, MAX_CHAINED_REPUBLISHES)? {
                waiting.push(seen);
            }
        }
        // Until nothing more is added: a republish may need the headers another one carries
        let mut others: BTreeMap<[u8; 32], bool> = BTreeMap::new();
        loop {
            let before = waiting.len();
            waiting.retain(|seen| {
                let lands = seen.needs.iter().all(|parent| {
                    snapshot.counted_at(parent).is_some()
                        || covered.contains(parent)
                        || others.contains_key(parent)
                });
                if lands {
                    for (hash, body) in &seen.republished {
                        *others.entry(*hash).or_default() |= *body;
                    }
                }
                !lands
            });
            if waiting.len() == before {
                break;
            }
        }
        for (txid, pays) in pays {
            if let Some(seen) = state.mempool.get_mut(&txid) {
                seen.honored = Some(pays);
            }
        }
        Ok(Some(others))
    }

    /// Whether the transaction has confirmed in a BTC block the snapshot has read (`Some(true)`) or in one it has not,
    /// bitcoind being ahead of it (`Some(false)`); `None` when it has not confirmed on the best chain. A transaction
    /// bitcoind does not know while its txindex is behind its tip may be in a block the index has not reached, which
    /// the snapshot has not read either: `Some(false)`.
    fn read_on_btc(
        &self,
        snapshot: &BtcSnapshot,
        txid: &[u8; 32],
    ) -> Result<Option<bool>, NodeError> {
        let client = self.chain.btc().client();
        let info = match client.transaction(txid).map_err(btc)? {
            Some(info) => info,
            None => {
                // txindex follows the tip after a block connects: "unknown" says nothing before it has caught up
                let tip = client.tip().map_err(btc)?.0;
                if client
                    .txindex_height()
                    .map_err(btc)?
                    .is_some_and(|indexed| indexed < tip)
                {
                    return Ok(Some(false));
                }
                // It had caught up with that tip: asked again, the answer covers every block up to it
                match client.transaction(txid).map_err(btc)? {
                    Some(info) => info,
                    None => return Ok(None),
                }
            }
        };
        let Some(block) = info.block else {
            return Ok(None);
        };
        Ok(client
            .header(&block)
            .map_err(btc)?
            .filter(|header| header.in_best_chain)
            .map(|header| snapshot.hash_at(header.height) == Some(block)))
    }

    /// Whether another sender's republish in the mempool (reveal transaction `txid`) pays for what it carries (D-143): a
    /// commit / reveal pair paying a fee rate not below `feerate`, this node's (a cheaper one may confirm much later, or
    /// never), whose commit transaction spends confirmed outputs, or outputs of the reveal of another republish that
    /// pays (a chain, D-139; `depth` links at most). `pays` holds the answers so far.
    fn republish_pays(
        &self,
        state: &State,
        pays: &mut BTreeMap<[u8; 32], bool>,
        txid: &[u8; 32],
        feerate: u64,
        depth: usize,
    ) -> Result<bool, NodeError> {
        if let Some(known) = pays.get(txid) {
            return Ok(*known);
        }
        pays.insert(*txid, false);
        let client = self.chain.btc().client();
        let Some(reveal) = client.transaction(txid).map_err(btc)?.map(|info| info.tx) else {
            return Ok(false);
        };
        let Some(commit_txid) = reveal.input.first().map(|txin| txin.previous_output.txid) else {
            return Ok(false);
        };
        if reveal
            .input
            .iter()
            .any(|txin| txin.previous_output.txid != commit_txid)
        {
            return Ok(false);
        }
        let Some(commit) = client
            .transaction(&commit_txid.to_byte_array())
            .map_err(btc)?
            .map(|info| info.tx)
        else {
            return Ok(false);
        };
        for txin in &commit.input {
            let parent = txin.previous_output.txid.to_byte_array();
            let Some(info) = client.transaction(&parent).map_err(btc)? else {
                return Ok(false);
            };
            if info.block.is_some() {
                continue;
            }
            let chained = depth > 0
                && state
                    .mempool
                    .get(&parent)
                    .is_some_and(|seen| !seen.republished.is_empty())
                && self.republish_pays(state, pays, &parent, feerate, depth.saturating_sub(1))?;
            if !chained {
                return Ok(false);
            }
        }
        let paid = match self.pair_fee(&SettlementPair { commit, reveal })? {
            Some((fee, vsize)) => fee >= feerate.saturating_mul(vsize),
            None => false,
        };
        pays.insert(*txid, paid);
        Ok(paid)
    }

    /// Whether a settlement claim in the mempool (reveal transaction `txid`) holds this node's rescue back (D-057, M26):
    /// a commit / reveal pair that lands the block's data paying this node's fee rate `feerate` ([`Rescuer::pays`]).
    fn honors(&self, txid: &[u8; 32], payload: &[u8], feerate: u64) -> Result<bool, NodeError> {
        let client = self.chain.btc().client();
        let Some(reveal) = client.transaction(txid).map_err(btc)?.map(|info| info.tx) else {
            return Ok(false);
        };
        let Some(input) = reveal.input.first() else {
            return Ok(false);
        };
        let commit_txid = input.previous_output.txid.to_byte_array();
        let Some(commit) = client
            .transaction(&commit_txid)
            .map_err(btc)?
            .map(|info| info.tx)
        else {
            return Ok(false);
        };
        self.pays(&SettlementPair { commit, reveal }, payload, feerate)
    }

    /// Whether a commit / reveal pair (D-081) lands a block's data and pays for it: its envelope carries exactly the
    /// block's settlement payload `payload` (so it lands the same data), its commit transaction spends only confirmed
    /// outputs, and together they pay a fee rate not below `feerate`, this node's (a cheaper one may confirm much
    /// later, or never; D-102).
    fn pays(&self, pair: &SettlementPair, payload: &[u8], feerate: u64) -> Result<bool, NodeError> {
        let client = self.chain.btc().client();
        let params = self.chain.params();
        let mut prevouts = BTreeMap::new();
        for txin in &pair.commit.input {
            let outpoint = txin.previous_output;
            let confirmed = client
                .transaction(&outpoint.txid.to_byte_array())
                .map_err(btc)?
                .filter(|info| info.block.is_some())
                .and_then(|info| {
                    let vout = usize::try_from(outpoint.vout).ok()?;
                    info.tx.output.get(vout).cloned()
                });
            let Some(txout) = confirmed else {
                return Ok(false);
            };
            prevouts.insert(outpoint, txout);
        }
        let checked = match validate_attached(params, pair, payload, &prevouts) {
            Ok(checked) => checked,
            Err(SettlementError::Fatal(m)) => return Err(NodeError::Internal(m)),
            Err(_) => return Ok(false),
        };
        Ok(checked.fee >= feerate.saturating_mul(checked.vsize))
    }

    /// A commit / reveal pair carrying `payload`, funded by this node at `feerate` (§7.3, D-045; the envelope using this
    /// node's funding key, D-081): a rescue rebuilds a block's settlement transactions with the payload unchanged. It
    /// passes the checks of the
    /// settlement transactions a block carries, by which other nodes judge a claim in the mempool (D-057), and
    /// `testmempoolaccept`; `None` when it cannot be assembled or is refused (logged, `what` naming it).
    fn build_pair(
        &self,
        payload: &[u8],
        btc_tip: u32,
        feerate: u64,
        what: &str,
    ) -> Result<Option<Sent>, NodeError> {
        let params = self.chain.params();
        let client = self.chain.btc().client();
        let utxos = self.available_utxos(btc_tip)?;
        let draft = match fund(params, &utxos, &self.funding, payload, feerate)? {
            Ok(draft) => draft,
            Err(why) => {
                eprintln!("[ksana-node] cannot assemble {what}: {why}");
                return Ok(None);
            }
        };
        let prevouts: BTreeMap<bitcoin::OutPoint, bitcoin::TxOut> = draft
            .commit
            .input
            .iter()
            .map(|input| input.previous_output)
            .zip(draft.prevouts.iter().cloned())
            .collect();
        let pair = finalize(params, draft, payload, &self.funding.keypair)
            .map_err(|e| NodeError::Internal(format!("signing {what} failed: {e}")))?;
        if let Err(e) = validate_attached(params, &pair, payload, &prevouts) {
            eprintln!("[ksana-node] {what} failed the self-check: {e}");
            return Ok(None);
        }
        if let Err(reason) = client.test_accept(&pair.txs()).map_err(btc)? {
            eprintln!("[ksana-node] testmempoolaccept rejected {what}: {reason}");
            return Ok(None);
        }
        Ok(Some(Sent::pair(pair)))
    }

    /// CPFP bumps of the pairs this node sent (D-102): the settlement transactions of the blocks it produced, its
    /// rescues and its republishes. One waiting in the BTC mempool for [`BUMP_AFTER`] BTC blocks (at once while a
    /// lighter competing chain is on BTC, D-091) whose package pays less than `feerate`, the estimated rate (D-117),
    /// gets a child spending the reveal's change (D-140), lifting the package to
    /// `feerate`. Once the pair has waited [`BUMP_AFTER`] BTC blocks while bitcoind's mempool had `room` (outside a
    /// race), the target is at least the pair's own rate doubled for every such block from the `BUMP_AFTER`-th on, up
    /// to the cap: those blocks had space for the pair, so the miners want more than it pays, whatever bitcoind
    /// estimates (D-117). The child is replaced by a richer one when the target rises again. The child's funding input
    /// stays reserved until the child confirms or is gone.
    ///
    /// The pairs of a block whose carried data has all landed are not bumped, nor a pair whose change a chained
    /// republish spent (D-139), nor a block's own settlement once its payload went out again in a rescue of this
    /// node's: that one is bumped instead.
    fn bump(
        &self,
        snapshot: &BtcSnapshot,
        btc_tip: u32,
        feerate: u64,
        urgent: bool,
        room: bool,
    ) -> Result<(), NodeError> {
        let chain = &self.chain;
        let params = chain.params();
        let client = chain.btc().client();
        let mut state = self.state()?;
        let mut pairs: BTreeMap<[u8; 32], (SettlementPair, Lands)> = BTreeMap::new();
        if let Some(me) = self.producer {
            for (hash, record) in chain.open_settlements()? {
                if !record.broadcast || state.rescues.contains_key(&hash) {
                    continue;
                }
                let Some(block) = chain.store().block(&hash)? else {
                    continue;
                };
                let Some(txs) = block
                    .settlement
                    .as_ref()
                    .filter(|_| block.header.producer == me)
                else {
                    continue;
                };
                let pair = settlement::decode(txs)?;
                pairs.insert(pair.txid(), (pair, Lands::Block(hash)));
            }
        }
        let ours = state
            .rescues
            .iter()
            .map(|(hash, sent)| (Lands::Block(*hash), sent))
            .chain(state.headers.values().map(|sent| (Lands::Headers, sent)));
        for (lands, sent) in ours {
            if let [commit, reveal] = sent.txs.as_slice() {
                let pair = SettlementPair {
                    commit: commit.clone(),
                    reveal: reveal.clone(),
                };
                pairs.insert(sent.txid, (pair, lands));
            }
        }

        // The children sent: a confirmed one is done, one gone from the mempool is sent again while its pair still
        // waits (given up when that is rejected: its parents are gone) and given up once the pair no longer waits,
        // since it would only pay the package's deficit, or is no longer one this node settles
        // (its block left the main chain, D-104); a bump is kept while its pair still waits or its child is
        // unconfirmed
        let tracked: Vec<[u8; 32]> = state.bumps.keys().copied().collect();
        for reveal in tracked {
            let Some(mut bump) = state.bumps.remove(&reveal) else {
                continue;
            };
            if let Some((child, fee)) = bump.child.take() {
                let confirmed = client
                    .transaction(&child.txid)
                    .map_err(btc)?
                    .is_some_and(|info| info.block.is_some());
                if confirmed {
                    chain.release_inputs(&child.inputs)?;
                } else if !client.in_mempool(&child.txid).map_err(btc)?
                    && (!pairs.contains_key(&reveal) || !client.in_mempool(&reveal).map_err(btc)?)
                {
                    eprintln!(
                        "[ksana-node] abandoning the CPFP child {} sent by this node: it left the BTC mempool, and its pair \
                         no longer waits there or is no longer one this node settles (D-104)",
                        child.shown()
                    );
                    chain.release_inputs(&child.inputs)?;
                } else if self.keep_alive(client.as_ref(), &child)? {
                    bump.child = Some((child, fee));
                }
            }
            if bump.child.is_some()
                || (pairs.contains_key(&reveal) && client.in_mempool(&reveal).map_err(btc)?)
            {
                state.bumps.insert(reveal, bump);
            }
        }

        let wait = if urgent { 0 } else { BUMP_AFTER };
        for (reveal, (pair, lands)) in &pairs {
            if !client.in_mempool(reveal).map_err(btc)? {
                continue;
            }
            let bump = state.bumps.entry(*reveal).or_insert(Bump {
                since: btc_tip,
                room_since: None,
                child: None,
            });
            // Only the BTC blocks it has waited through while the mempool had room count against the pair's rate: a
            // round without room starts them again (D-117)
            if room {
                bump.room_since.get_or_insert(btc_tip);
            } else {
                bump.room_since = None;
            }
            if btc_tip < bump.since.saturating_add(wait) {
                continue;
            }
            let Some((pair_fee, pair_vsize)) = self.pair_fee(pair)? else {
                continue;
            };
            // With room the pair has missed blocks that had space for it: twice its own rate once it has waited
            // BUMP_AFTER BTC blocks with room, twice as much again for every further one, so that a bump comes even when
            // bitcoind has no estimate or estimates no more than the floor the pair pays (D-117)
            let target = match bump.room_since {
                Some(room_since) if btc_tip >= room_since.saturating_add(wait) => {
                    let doublings = btc_tip
                        .saturating_sub(room_since)
                        .saturating_sub(wait)
                        .saturating_add(1);
                    let own = pair_fee.checked_div(pair_vsize).unwrap_or(0);
                    feerate.max(self.funding.fees.escalated(params, own, doublings))
                }
                _ => feerate,
            };
            // Nothing more is spent on a pair whose block's carried data has all landed through other transactions
            if let Lands::Block(hash) = lands {
                if self.landed(hash, snapshot)? {
                    continue;
                }
            }
            // The least fee of the child: a replacement pays more than the child it replaces by the relay increment
            let min_fee = match &bump.child {
                Some((child, fee)) => {
                    let Some(tx) = child.txs.first() else {
                        continue;
                    };
                    let size = tx.weight().to_vbytes_ceil();
                    let paid = pair_fee.saturating_add(*fee);
                    if paid >= target.saturating_mul(pair_vsize.saturating_add(size)) {
                        continue;
                    }
                    fee.saturating_add(size.saturating_mul(INCREMENTAL_RELAY_FEERATE))
                }
                None => {
                    if pair_fee >= target.saturating_mul(pair_vsize) {
                        continue;
                    }
                    0
                }
            };
            // The child spends the reveal's change (D-140); once a republish chained on the pair spends it (D-139),
            // that republish waits with it and is the one bumped. Every pair pays its change to the funding address
            // (D-142); one that does not is not this key's to spend
            if pair
                .reveal
                .output
                .first()
                .is_none_or(|change| change.script_pubkey != self.funding.script)
            {
                continue;
            }
            let outpoint = bitcoin::OutPoint {
                txid: pair.reveal.compute_txid(),
                vout: CHANGE_OUTPUT,
            };
            if bump.child.is_none() && client.utxo(&outpoint, true).map_err(btc)?.is_none() {
                continue;
            }
            let child = cpfp_child(
                params,
                &pair.reveal,
                pair_fee,
                pair_vsize,
                target,
                min_fee,
                &self.funding.keypair,
            )
            .map_err(|e| NodeError::Internal(format!("building the CPFP child failed: {e}")))?;
            let Some((tx, fee)) = child else {
                eprintln!(
                    "[ksana-node] cannot bump the transaction {} waiting in the BTC mempool: its change does not pay for it",
                    bitcoin::Txid::from_byte_array(*reveal)
                );
                continue;
            };
            if let Err(reason) = client.test_accept(&[&tx]).map_err(btc)? {
                eprintln!(
                    "[ksana-node] testmempoolaccept rejected the CPFP child of {}: {reason}",
                    bitcoin::Txid::from_byte_array(*reveal)
                );
                continue;
            }
            let sent = Sent {
                txid: tx.compute_txid().to_byte_array(),
                inputs: vec![(outpoint.txid.to_byte_array(), outpoint.vout)],
                txs: vec![tx],
            };
            // A first child reserves its funding input; a replacement keeps the reservation of the one it replaces
            let replacing = bump.child.is_some();
            if !replacing && !chain.try_reserve_inputs(&sent.inputs)? {
                continue;
            }
            match client.broadcast_all(&sent.refs()) {
                Ok(Ok(())) => {
                    eprintln!(
                        "[ksana-node] bumped {} waiting in the BTC mempool to {target} sat/vB by CPFP (D-102): child txid={} fee={fee}",
                        bitcoin::Txid::from_byte_array(*reveal),
                        sent.shown()
                    );
                    bump.child = Some((sent, fee));
                }
                Ok(Err(reason)) => {
                    if !replacing {
                        chain.release_inputs(&sent.inputs)?;
                    }
                    eprintln!(
                        "[ksana-node] broadcasting the CPFP child of {} failed: {reason}",
                        bitcoin::Txid::from_byte_array(*reveal)
                    );
                }
                Err(e) => {
                    // Outcome unknown: track as usual, verified next round
                    bump.child = Some((sent, fee));
                    return Err(btc(e));
                }
            }
        }
        Ok(())
    }

    /// Whether all the data block `hash`'s own settlement carries has landed (D-077), through whatever transactions.
    fn landed(&self, hash: &[u8; 32], snapshot: &BtcSnapshot) -> Result<bool, NodeError> {
        Ok(match self.chain.store().block(hash)? {
            Some(block) => self.chain.carried_on_chain(&block, snapshot)?.is_some(),
            None => false,
        })
    }

    /// The fees (sats) and virtual size (vB) of a commit / reveal pair together; `None` when the previous outputs of the
    /// commit transaction cannot be found.
    fn pair_fee(&self, pair: &SettlementPair) -> Result<Option<(u64, u64)>, NodeError> {
        let client = self.chain.btc().client();
        let mut total_in = 0u64;
        for txin in &pair.commit.input {
            let outpoint = txin.previous_output;
            let value = client
                .transaction(&outpoint.txid.to_byte_array())
                .map_err(btc)?
                .and_then(|info| {
                    let vout = usize::try_from(outpoint.vout).ok()?;
                    info.tx.output.get(vout).map(|out| out.value.to_sat())
                });
            let Some(value) = value else {
                return Ok(None);
            };
            total_in = total_in.saturating_add(value);
        }
        let out = |tx: &Transaction| {
            tx.output
                .iter()
                .map(|o| o.value.to_sat())
                .fold(0u64, u64::saturating_add)
        };
        // The reveal spends the commit's commitment output, so the pair's fee is what the commit's inputs give minus
        // what neither transaction spends: the commit's change and the reveal's outputs
        let committed = pair.commit.output.first().map_or(0, |o| o.value.to_sat());
        let fee = total_in
            .saturating_sub(out(&pair.commit).saturating_sub(committed))
            .saturating_sub(out(&pair.reveal));
        let vsize = pair
            .commit
            .weight()
            .to_vbytes_ceil()
            .saturating_add(pair.reveal.weight().to_vbytes_ceil());
        Ok(Some((fee, vsize)))
    }

    /// Whether the data of block `block`, which carries settlement transactions, has landed, or its settlement (its own
    /// pair or this node's rescue of it) has confirmed and counted on BTC (D-135).
    fn settlement_counted(
        &self,
        state: &State,
        snapshot: &BtcSnapshot,
        block: &BlockSummary,
    ) -> Result<bool, NodeError> {
        if snapshot.data_height(block).is_some()
            || state
                .rescues
                .get(&block.hash)
                .is_some_and(|sent| snapshot.ksn1_height(&sent.txid).is_some())
        {
            return Ok(true);
        }
        let Some(stored) = self.chain.store().block(&block.hash)? else {
            return Ok(false);
        };
        let Some(txs) = &stored.settlement else {
            return Ok(false);
        };
        Ok(snapshot
            .ksn1_height(&settlement::decode(txs)?.txid())
            .is_some())
    }

    /// This node's turn among the nodes waiting to republish the run that starts with block `first` (D-143): from the
    /// block and this node's funding script, so the nodes' turns differ from run to run.
    fn yield_slot(&self, first: &[u8; 32]) -> u8 {
        let mut engine = sha256::Hash::engine();
        engine.input(first);
        engine.input(self.funding.script.as_bytes());
        let digest = sha256::Hash::from_engine(engine).to_byte_array();
        digest.first().copied().unwrap_or(0) % REPUBLISH_YIELD_SLOTS
    }

    fn republish(
        &self,
        snapshot: &BtcSnapshot,
        btc_tip: u32,
        feerate: u64,
        competitor: Option<([u8; 32], u64)>,
    ) -> Result<(), NodeError> {
        let chain = &self.chain;
        let params = chain.params();
        let client = chain.btc().client();
        let main = chain.main_chain()?;
        // Blocks accepted by the main chain, in acceptance order (each main-chain block preceded by the blocks it
        // merges, D-063), with the main-chain height that accepted each; from the window held in memory (D-097: the
        // blocks below it have all landed)
        let accepted: Vec<(u64, BlockSummary)> = main
            .iter_from(1)
            .flat_map(|(height, e)| e.accepted().map(move |b| (height, b)))
            .collect();
        let mut state = self.state()?;
        // Republishes that have landed (or are no longer accepted by the main chain) are no longer tracked
        let done: Vec<[u8; 32]> = state
            .headers
            .keys()
            .filter(|h| {
                accepted
                    .iter()
                    .find(|(_, b)| b.hash == **h)
                    .is_none_or(|(_, b)| snapshot.data_height(b).is_some())
            })
            .copied()
            .collect();
        // As for closed rescues, the inputs stay reserved while the transactions may be in the BTC mempool (D-104)
        for first in done {
            if let Some(sent) = state.headers.remove(&first) {
                chain.linger(sent.txids(), &sent.inputs)?;
            }
        }

        let Some((first, (_, block))) = accepted
            .iter()
            .enumerate()
            .find(|(_, (_, b))| snapshot.data_height(b).is_none())
        else {
            return Ok(());
        };
        if chain.carries_settlement(&block.hash)? {
            // A block carrying settlement transactions is handled by rescue
            return Ok(());
        }
        let tip = main.tip_height();
        let keep = params.max_settlement_headers.saturating_sub(1);
        let depth = params.settlement_depth;
        // Range to republish [first, end): the payload of c, the first later block carrying settlement transactions
        // with first in its past, carries the most recent keep headers (and bodies) in its past that have not landed,
        // so only the earlier part needs republishing; when c does not exist yet, take all headers whose accepting
        // main-chain block has reached depth SETTLEMENT_DEPTH. A block carrying settlement transactions that comes
        // after first in acceptance order yet is parallel to it (a merged block in the DAG, D-063) will not carry it
        // and cannot be the basis for republishing
        let mut next_carrier = None;
        for (index, (_, b)) in accepted.iter().enumerate().skip(first.saturating_add(1)) {
            if chain.carries_settlement(&b.hash)? && chain.in_closed_past(&block.hash, &b.hash)? {
                next_carrier = Some((index, *b));
                break;
            }
        }
        let end = match next_carrier {
            // c's settlement has confirmed and counted without carrying first (the producer carried fewer headers or
            // bodies because of the payload size, D-075, D-077): everything before c that has not landed needs
            // republishing, and c's own data may be waiting for those headers (D-135)
            // The later carriers whose settlements have counted too left their earliest bodies to republishing in the
            // same way (D-138): everything before the last of them that has not landed goes out in one payload, rather
            // than one run per BTC block
            Some((c, settling)) if self.settlement_counted(&state, snapshot, &settling)? => {
                let mut end = c;
                for (index, (_, b)) in accepted.iter().enumerate().skip(c.saturating_add(1)) {
                    if !chain.carries_settlement(&b.hash)? {
                        continue;
                    }
                    if !self.settlement_counted(&state, snapshot, b)? {
                        break;
                    }
                    end = index;
                }
                end
            }
            Some((c, _)) => c.saturating_sub(keep),
            None => {
                let gap = accepted.len().saturating_sub(first) >= keep;
                let quiet = btc_tip.saturating_sub(block.anchor_height)
                    > params
                        .settlement_timeout
                        .saturating_mul(QUIET_REPUBLISH_FACTOR);
                // A lighter competing chain recently appeared on BTC (D-091): do not wait for the two conditions above,
                // republish immediately so that this node's heavier chain lands before the other side reaches
                // BTC_FINALITY_CONFS confirmations; and do not wait for depth SETTLEMENT_DEPTH (the other side is
                // racing for finality, so this node must get a stretch of chain heavier than theirs onto BTC as soon as
                // possible; even if the tip blocks are reorged later, landing their headers does no harm)
                if !gap && !quiet && competitor.is_none() {
                    return Ok(());
                }
                if let Some((hash, height)) = competitor {
                    if !state.competitors.contains(&hash) {
                        state.competitors.insert(hash);
                        eprintln!(
                            "[ksana-node] a lighter competing chain appeared on BTC: block {} (height {height}), republishing this node's headers immediately (D-091)",
                            hex::encode(hash)
                        );
                    }
                }
                let deep = if competitor.is_some() {
                    tip
                } else {
                    tip.saturating_sub(depth)
                };
                accepted
                    .iter()
                    .rposition(|(height, _)| *height <= deep)
                    .map_or(0, |p| p.saturating_add(1))
            }
        };
        if end <= first {
            return Ok(());
        }
        // This node's republishes in flight, in acceptance order (D-139): each is kept alive, and the next one chains on
        // the change of the last one's commit transaction, so BTC confirms them in order; the blocks they carry are not
        // carried again
        let position = |hash: &[u8; 32]| accepted.iter().position(|(_, b)| b.hash == *hash);
        let mut flying: Vec<[u8; 32]> = state.headers.keys().copied().collect();
        flying.sort_by_key(|hash| position(hash));
        let mut covered = BTreeSet::new();
        let mut tail = None;
        let mut in_flight = 0usize;
        for hash in flying {
            let Some(sent) = state.headers.get(&hash) else {
                continue;
            };
            // Confirmed in a BTC block the snapshot has read: no longer in flight, and what it did not land goes out
            // again (D-143; it lands nothing when a header it needed, carried by another sender's republish, had not
            // counted)
            if self.read_on_btc(snapshot, &sent.txid)? == Some(true) {
                if let Some(sent) = state.headers.remove(&hash) {
                    chain.linger(sent.txids(), &sent.inputs)?;
                }
                continue;
            }
            if !self.keep_alive(client.as_ref(), sent)? {
                state.headers.remove(&hash);
                continue;
            }
            in_flight = in_flight.saturating_add(1);
            tail = None;
            if let [commit, reveal] = sent.txs.as_slice() {
                let pair = SettlementPair {
                    commit: commit.clone(),
                    reveal: reveal.clone(),
                };
                if let Some(Payload::Headers { headers, .. }) = attached_payload(params, &pair)
                    .and_then(|payload| Payload::decode(&payload, params).ok())
                {
                    covered.extend(headers.iter().map(BlockHeader::hash));
                }
                let change = bitcoin::OutPoint {
                    txid: reveal.compute_txid(),
                    vout: CHANGE_OUTPUT,
                };
                // Only change the funding key signs links the chain (every pair pays it there, D-142)
                if let Some(txout) = reveal
                    .output
                    .first()
                    .filter(|txout| txout.script_pubkey == self.funding.script)
                {
                    if client.utxo(&change, true).map_err(btc)?.is_some() {
                        tail = Some(ProducerUtxo {
                            outpoint: change,
                            txout: txout.clone(),
                        });
                    }
                }
            }
        }
        // A full chain waits for its first pairs to confirm; so does one whose last change is spent (it can no longer
        // be ordered after it)
        if in_flight >= MAX_CHAINED_REPUBLISHES || (in_flight > 0 && tail.is_none()) {
            return Ok(());
        }
        // What other senders' republishes in the mempool carry is not carried again (D-143)
        let Some(others) =
            self.republished_by_others(&mut state, snapshot, btc_tip, feerate, &covered)?
        else {
            return Ok(());
        };

        // From first (up to before end), the blocks whose data has not landed: the header of each that has not counted,
        // and the header and body of each that carries no settlement transactions and needs a body (D-138); the blocks
        // skipped have counted, so every parent of a header in the payload has counted or is in it (D-135). A carrier
        // whose header has counted is left to the rescue. At most MAX_SETTLEMENT_HEADERS headers (§7.1) and the payload
        // within the limit: accumulate the lengths of the compact encoding and the bodies one by one (an upper bound of
        // the payload's length, the payee included, D-095)
        let mut headers = Vec::new();
        let mut bodies = Vec::new();
        let mut encoder = compact::HeaderEncoder::default();
        let mut len = headers_payload_fixed_len(std::iter::empty());
        for (_, b) in accepted.iter().take(end).skip(first) {
            if headers.len() >= params.max_settlement_headers {
                break;
            }
            if snapshot.data_height(b).is_some() || covered.contains(&b.hash) {
                continue;
            }
            let body = b.settles && !chain.carries_settlement(&b.hash)?;
            if !body && snapshot.counted_at(&b.hash).is_some() {
                continue;
            }
            if others
                .get(&b.hash)
                .is_some_and(|with_body| *with_body || !body)
            {
                continue;
            }
            let block = chain.store().block(&b.hash)?.ok_or_else(|| {
                NodeError::Storage("block accepted by the main chain is missing".into())
            })?;
            let encoded = encoder
                .encode(&block.header)
                .map_err(|e| NodeError::Internal(format!("block header encoding failed: {e}")))?;
            len = len.saturating_add(encoded.len());
            if body {
                len = len.saturating_add(settlement::body_len(&block.entries)?);
            }
            if len > params.max_block_data_bytes {
                break;
            }
            if body {
                bodies.push(Body {
                    header: u32::try_from(headers.len())
                        .map_err(|_| NodeError::Internal("body index overflow".into()))?,
                    entries: entry_data(&block.entries),
                });
            }
            headers.push(block.header);
        }
        // The payload by its exact encoding: should the estimate fall short, drop trailing headers (and a body pointing
        // at one) until it fits
        let payload = loop {
            let payload = Payload::Headers {
                headers: headers.clone(),
                bodies: bodies.clone(),
                payee: self.payee,
            }
            .encode(params)
            .map_err(|e| NodeError::Internal(e.to_string()))?;
            if payload.len() <= params.max_block_data_bytes || headers.is_empty() {
                break payload;
            }
            headers.pop();
            // The popped header's index is now the number of headers
            if bodies.last().map(|b| b.header) == u32::try_from(headers.len()).ok() {
                bodies.pop();
            }
        };
        if headers.is_empty() {
            return Ok(());
        }
        let count = headers.len();
        let body_count = bodies.len();
        let key = headers.first().map(BlockHeader::hash).unwrap_or(block.hash);
        // A node starting a chain with a block it did not produce waits its turn (D-143), unless a lighter competing
        // chain is on BTC (D-091); the republishes sent meanwhile change what is left, and the wait starts again
        let first_producer = headers.first().map(|header| header.producer);
        if in_flight == 0
            && !self.republish_yield.is_zero()
            && competitor.is_none()
            && self.producer != first_producer
        {
            let wait = self
                .republish_yield
                .saturating_mul(u32::from(self.yield_slot(&key)).saturating_add(1));
            match state.yielding {
                Some((waited, since)) if waited == key => {
                    if since.elapsed() < wait {
                        return Ok(());
                    }
                }
                _ => {
                    state.yielding = Some((key, crate::monotonic_now()));
                    return Ok(());
                }
            }
        }
        state.yielding = None;
        // The last republish's change first: the funding takes a prefix of the list
        let utxos: Vec<ProducerUtxo> = tail
            .into_iter()
            .chain(self.available_utxos(btc_tip)?)
            .collect();
        let draft = match fund(params, &utxos, &self.funding, &payload, feerate)? {
            Ok(draft) => draft,
            Err(why) => {
                eprintln!(
                    "[ksana-node] cannot assemble the header republish transaction ({count} headers, {body_count} bodies, {} bytes): {why}",
                    payload.len()
                );
                return Ok(());
            }
        };
        let pair = finalize(params, draft, &payload, &self.funding.keypair).map_err(|e| {
            NodeError::Internal(format!("header republish transaction signing failed: {e}"))
        })?;
        if let Err(reason) = client.test_accept(&pair.txs()).map_err(btc)? {
            eprintln!("[ksana-node] testmempoolaccept rejected the header republish transaction: {reason}");
            return Ok(());
        }
        let sent = Sent::pair(pair);
        // Reserve the funding before broadcasting (as for rescue settlements)
        if !chain.try_reserve_inputs(&sent.inputs)? {
            return Ok(());
        }
        match client.broadcast_all(&sent.refs()) {
            Ok(Ok(())) => {
                eprintln!(
                    "[ksana-node] republishing {count} headers and {body_count} bodies (from block {}) reveal txid={}",
                    hex::encode(key),
                    sent.shown()
                );
                state.headers.insert(key, sent);
            }
            Ok(Err(reason)) => {
                chain.release_inputs(&sent.inputs)?;
                eprintln!(
                    "[ksana-node] broadcasting the header republish transaction failed: {reason}"
                );
            }
            Err(e) => {
                // Outcome unknown: track as usual; keep_alive verifies next round
                state.headers.insert(key, sent);
                return Err(btc(e));
            }
        }
        Ok(())
    }
}
