//! Op admission and entry pool maintenance (shared by RPC submission and network forwarding).
//!
//! - [`admit`]: entry pool admission of path-B ops (path A has been removed, D-078). Rejection reasons are split into
//!   objectively invalid (format, signature; the forwarder should lose score) and invalid under the local view (nonce,
//!   balance).
//! - [`chain_changed`]: after the main chain connects or disconnects blocks (this node produced a block, a block from
//!   another node arrived, a reorg), maintains the entry pool and emits new-block notifications (D-041).
//! - The nonces of the path C ops waiting for forced inclusion (the ops passing authentication, in BTC order): the entry
//!   pool counts those taking an account's nonces in turn as filled ([`Mempool::set_forced`], D-104), updated after
//!   every main-chain change and BTC sync.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard};

use ksana_btc::native::inclusion_height;
use ksana_execution::{check_native_op, StateSource};
use ksana_mempool::{AdmitError, Mempool};
use ksana_primitives::{Block, Entry, Rules, VmOp};
use ksana_rpc::BlockSummary;
use tokio::sync::broadcast;

use crate::chain::{Chain, Tip};
use crate::NodeError;

/// Callback that forwards a newly accepted op to the network.
pub type Relay = Arc<dyn Fn(&VmOp) + Send + Sync>;

/// Why an op was rejected.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Rejection {
    /// Objectively invalid: format, version, `chain_id`, fee mode, signature.
    Invalid(String),
    /// Invalid under this node's current state: nonce, balance.
    Stale(String),
    /// Valid, but does not meet this node's policy (fee, capacity, replacement).
    Policy(String),
    /// Local fault (storage, bitcoind).
    Local(String),
}

impl std::fmt::Display for Rejection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Rejection::Invalid(m) | Rejection::Stale(m) => write!(f, "invalid op: {m}"),
            Rejection::Policy(m) => write!(f, "does not meet this node's policy: {m}"),
            Rejection::Local(m) => write!(f, "local fault: {m}"),
        }
    }
}

fn pool(mempool: &Mutex<Mempool>) -> Result<MutexGuard<'_, Mempool>, NodeError> {
    mempool
        .lock()
        .map_err(|_| NodeError::Internal("entry pool lock is poisoned".into()))
}

/// The rules that apply to the next block after the tip (D-076): entry pool admission follows them. The DAA score of
/// the next block is at least the tip's plus 1 (larger when merging parallel blocks, may differ by a few near an
/// activation point; block production checks against the actual rules).
fn next_rules(chain: &Chain) -> Result<Rules, NodeError> {
    let tip = chain.tip()?;
    let daa_score = chain
        .ghostdag(&tip.hash)?
        .map_or(0, |d| d.daa_score.saturating_add(1));
    Ok(chain.params().rules(daa_score))
}

/// The verdicts of the authentication a block forcing a native op applies to it ([`check_native_op`]), by txid with the
/// rules each was reached under, for the native transactions of the last scan ([`forced_nonces`]): each is checked
/// once, its signature verification being the costly part.
#[derive(Debug, Default)]
pub(crate) struct NativeAuth {
    verdicts: BTreeMap<[u8; 32], (Rules, bool)>,
}

/// The nonces of the path C ops waiting for forced inclusion, by account in BTC order (§7.4, D-104): those of the
/// native transactions on the BTC best chain above the inclusion height of the main-chain tip's anchor, which the next
/// blocks force in that order as their anchors reach them. The entry pool counts those taking an account's nonces in
/// turn as filled ([`Mempool::set_forced`]).
///
/// - Only an op passing the authentication a block forcing it applies counts ([`check_native_op`] under `rules`, the
///   next block's): one failing it takes no nonce, and anyone could otherwise make the pool count any account's nonce
///   as filled for the price of a BTC transaction. The verdicts are kept by txid ([`NativeAuth`]).
/// - The scan covers the first `NATIVE_CONFIRMATIONS + ANCHOR_DEPTH + BTC_FINALITY_CONFS` heights from there, which
///   the next blocks force first, after a retreat from a BTC reorg as deep as BTC finality allows too; leaving out the
///   later ones only leaves their nonces gaps. A node far behind BTC does not go through the whole backlog of native
///   transactions for every block it accepts.
fn forced_nonces(
    chain: &Chain,
    tip: &Tip,
    rules: &Rules,
) -> Result<BTreeMap<[u8; 32], Vec<u64>>, NodeError> {
    let params = chain.params();
    let snapshot = chain.btc().snapshot()?;
    let mut forced: BTreeMap<[u8; 32], Vec<u64>> = BTreeMap::new();
    let Some((btc_height, _)) = snapshot.tip() else {
        return Ok(forced);
    };
    let from = inclusion_height(params, tip.header.btc_anchor.height).saturating_add(1);
    let Ok(from) = u32::try_from(from.max(0)) else {
        return Ok(forced);
    };
    let span = params
        .native_confirmations
        .saturating_add(params.anchor_depth)
        .saturating_add(params.btc_finality_confs);
    let to = btc_height.min(from.saturating_add(span.saturating_sub(1)));
    let mut auth = chain.native_auth()?;
    let mut verdicts = BTreeMap::new();
    for tx in snapshot.native_iter(from..=to) {
        let authentic = match auth.verdicts.get(&tx.txid) {
            Some((checked, authentic)) if checked == rules => *authentic,
            _ => check_native_op(params, rules, &tx.op, &tx.first_input).is_ok(),
        };
        verdicts.insert(tx.txid, (*rules, authentic));
        if authentic {
            forced.entry(tx.op.account).or_default().push(tx.op.nonce);
        }
    }
    auth.verdicts = verdicts;
    Ok(forced)
}

/// Admit a path-B op: stateless checks, then entry pool admission against the main-chain tip state. Returns the
/// `op_id` and the entry pool generation after admission.
pub fn admit(
    chain: &Chain,
    mempool: &Mutex<Mempool>,
    op: VmOp,
) -> Result<([u8; 32], u64), Rejection> {
    let local = |e: NodeError| Rejection::Local(e.to_string());
    let rules = next_rules(chain).map_err(local)?;
    let mut stateless = pool(mempool).map_err(local)?;
    stateless.set_rules(rules);
    stateless.check_stateless(&op).map_err(|e| match e {
        AdmitError::Invalid(m) => Rejection::Invalid(m),
        AdmitError::Policy(m) => Rejection::Policy(m),
        AdmitError::State(m) => Rejection::Local(m),
    })?;
    drop(stateless);
    let (tip, view) = chain.tip_and_view().map_err(local)?;
    let state: Arc<dyn StateSource> = Arc::new(view);
    let mut pool = pool(mempool).map_err(local)?;
    let op_id = pool.insert(op, &state).map_err(|e| match e {
        AdmitError::Invalid(m) => Rejection::Stale(m),
        AdmitError::Policy(m) => Rejection::Policy(m),
        AdmitError::State(m) => Rejection::Local(m),
    })?;
    // The pool judged in the state of `tip` whether the op alters the packing candidates (D-130): when the main-chain
    // tip has moved since, a template built on the new tip is checked again
    if chain.tip().map_err(local)?.hash != tip.hash {
        pool.mark_changed();
    }
    Ok((op_id, pool.generation()))
}

/// Take the main-chain changes that happened outside [`Chain::accept_block`] (the finality decision re-selecting the
/// main chain, crash recovery; [`Chain::take_chain_changes`]) and maintain the entry pool per [`chain_changed`];
/// when there are no changes, only updates the nonces of the path C ops waiting for forced inclusion (it runs after
/// every BTC sync, which may have changed them).
pub fn apply_chain_changes(
    chain: &Chain,
    mempool: &Mutex<Mempool>,
    events: &broadcast::Sender<BlockSummary>,
) -> Result<(), NodeError> {
    let (connected, disconnected) = chain.take_chain_changes()?;
    if connected.is_empty() && disconnected.is_empty() {
        let rules = next_rules(chain)?;
        let forced = forced_nonces(chain, &chain.tip()?, &rules)?;
        pool(mempool)?.set_forced(forced);
        return Ok(());
    }
    chain_changed(chain, mempool, events, &connected, &disconnected)
}

/// After a main-chain change: the path-B ops of the disconnected blocks, and the skipped ones of the merged blocks
/// (D-063), go back into the entry pool while still valid (D-041), after the pool has learned the nonces of the path C
/// ops waiting for forced inclusion under the new tip (D-104); executed ops and ops whose nonce has been used are
/// removed, then new-block notifications are emitted for the connected blocks in order.
pub fn chain_changed(
    chain: &Chain,
    mempool: &Mutex<Mempool>,
    events: &broadcast::Sender<BlockSummary>,
    connected: &[Block],
    disconnected: &[Block],
) -> Result<(), NodeError> {
    let mut included = Vec::new();
    let mut retry = Vec::new();
    // A block connected and then disconnected in the same report (for example a connected orphan immediately
    // disconnected by the forced switch of finality): its ops go back into the entry pool, not counted as included
    let disconnected_hashes: std::collections::BTreeSet<[u8; 32]> =
        disconnected.iter().map(|b| b.header.hash()).collect();
    for block in connected
        .iter()
        .filter(|b| !disconnected_hashes.contains(&b.header.hash()))
    {
        for entry in &block.entries {
            if let Entry::Sponsored { op } | Entry::Forced { op, .. } = entry {
                let op_id = op.op_id()?;
                // Only appeared at a skipped position (the op index keeps the executed position): not executed
                let skipped = chain
                    .store()
                    .op_location(&op_id)?
                    .is_some_and(|location| location.skipped);
                if skipped {
                    retry.push(entry);
                } else {
                    included.push(op_id);
                }
            }
        }
    }
    let (tip, view) = chain.tip_and_view()?;
    let state: Arc<dyn StateSource> = Arc::new(view);
    let rules = next_rules(chain)?;
    let forced = forced_nonces(chain, &tip, &rules)?;
    {
        let mut pool = pool(mempool)?;
        pool.set_rules(rules);
        // First: the ops a retreat returns above a path C op included again are not behind a nonce gap
        pool.set_forced(forced);
        let returned = disconnected
            .iter()
            .flat_map(|block| block.entries.iter())
            .chain(retry);
        for entry in returned {
            let Entry::Sponsored { op } = entry else {
                continue;
            };
            // Dropped when no longer valid on the new main chain (nonce already used etc.)
            let _ = pool.insert(op.clone(), &state);
        }
        pool.remove_committed(&included, &state)
            .map_err(|e| NodeError::Internal(e.to_string()))?;
    }
    for block in connected {
        // Sending fails when there are no subscribers; ignored
        let _ = events.send(BlockSummary {
            height: block.header.height,
            hash: block.header.hash(),
            entries: u32::try_from(block.entries.len()).unwrap_or(u32::MAX),
            timestamp: block.header.timestamp,
        });
    }
    Ok(())
}
