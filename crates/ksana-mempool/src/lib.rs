//! Ksana entry pool: VmOps the node has received but not yet included (SPEC §6.2, §8.2). Path-B ops only (path A was
//! removed, D-078).
//!
//! - **Admission**: stateless checks (version, `chain_id`, encoded length, fee mode, signature) and checks against the
//!   current state (nonce not below the account nonce; the KSA balance covers the sponsored fees of all ops queued for
//!   this account), plus this node's policy (minimum sponsored fee for relaying, capacity limits). These checks agree
//!   with the block validity rules but are only a pre-filter: at packing time the execution layer re-validates entry by
//!   entry (`Executor::build_block`).
//! - **Capacity**: the pool is limited in op count and in total encoded bytes; each account in its op count, in how far
//!   its nonces may run ahead, and in the number of its ops queued behind a nonce gap (the nonces path C ops waiting
//!   for forced inclusion take from the account nonce on are not a gap, [`Mempool::set_forced`]). A full pool evicts
//!   rather than rejects: always an account's last op (its other ops can only run before it), choosing the account
//!   whose last op stands lowest: behind a nonce gap first, then possibly unable to pay its sponsored fee after what
//!   the account's earlier ops can spend, then below this node's packing floor, then the packing candidates, within
//!   each by fee rate from low to high. A new op is rejected only when it would itself be the first to go; on a tie the
//!   op already in the pool stays. Ops that can never be packed therefore cannot keep packable ops out (Sybil accounts
//!   each queuing ops behind a gap used to fill the pool for good).
//! - **Re-check after a chain change** ([`Mempool::remove_committed`]): besides the included ops and the ops whose
//!   nonce has been used, the ops the KSA balance no longer covers, the ops the current rules no longer accept and the
//!   ops behind a nonce gap beyond the per-account limit are dropped, so nothing that can never be packed stays in the
//!   pool for good.
//! - **Replacement**: an op with the account and nonce of a queued op replaces it only when it raises the sponsored fee
//!   by a tenth of it or more, and by the relay floor or more ([`Policy::replacement_fee`], D-130): each replacement
//!   costs at least what a new op costs, and replacing one op over and over for one more unit each is not possible.
//! - **Packing candidates**: for each account, the consecutive run of ops starting at the current nonce, up to the
//!   first one that does not meet the packing floor (minimum sponsored fee, minimum fee rate); among the queue heads of
//!   the accounts, selection goes by **fee rate** from high to low (§8.2; fee rate = sponsored fee / cost, cost =
//!   max(encoded bytes, bytes equivalent to `max_gas`), [`op_cost`]: a block's gas limit is equated with a block's data
//!   limit, so an op whose `max_gas` fills the whole block cannot squeeze out other ops with a slightly higher
//!   sponsored fee), and within one account by nonce order.
//! - **Generation** ([`Mempool::generation`]): moves only when a change can alter the packing candidates, which the
//!   producer rebuilds its template for (D-130). An op behind a nonce gap or after one below the packing floor is never
//!   one, so admitting, replacing or evicting it does not count: such ops cost their sender nothing, as no block takes
//!   them, and once made them a way to keep every producer rebuilding its template (A2-08).
//! - Path C is force-included by native transactions on BTC and does not pass through the entry pool. The node tells
//!   the pool the nonces of the authenticated path C ops waiting for inclusion, in BTC order
//!   ([`Mempool::set_forced`]); a block runs its forced entries first, in that order, so those continuing an account's
//!   nonce are taken in turn and the path B ops queued above them are not behind a nonce gap: after a BTC reorg the
//!   node's retreat returns the path B ops of the blocks it takes off while the path C ops below them are included
//!   again only by the next blocks (D-104).
//!
//! The entry pool is local policy, not consensus; panics are forbidden all the same, every input comes from the
//! untrusted network.

#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unreachable,
    clippy::todo,
    clippy::unimplemented,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::float_arithmetic,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::iter_over_hash_type
)]

use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet, BinaryHeap};
use std::sync::Arc;

use ksana_crypto::verify_vmop;
use ksana_execution::{builtin, Overlay, StateSource};
use ksana_primitives::{Action, FeeMode, Params, Rules, VmOp, KSA_ASSET_ID};

/// This node's entry pool policy (not a consensus rule, §8.2). The packing floor and the relay floor are separate:
/// when the relay floor is lower, ops this node does not pack still enter the pool and are still forwarded, to be
/// packed by other producers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Policy {
    /// Minimum token amount for packing: when this node produces a block, ops whose sponsored fee is below it (and the
    /// same account's later ops) are not candidates.
    pub min_token_fee: u128,
    /// Minimum token amount for admission (relaying): ops below it neither enter the pool nor are forwarded; when it is
    /// above `min_token_fee`, `min_token_fee` applies.
    pub min_relay_token_fee: u128,
    /// Minimum fee rate for packing: token amount per KiB of cost ([`op_cost`]) ([`fee_rate`]); 0 means no limit.
    pub min_fee_per_kib: u128,
    /// Limit on the total number of ops in the pool.
    pub max_ops: usize,
    /// Limit on the total encoded bytes of the ops in the pool (the op count alone would let ops of up to
    /// `MAX_OP_BYTES` take about 640 MB).
    pub max_bytes: usize,
    /// Maximum number of ops queued per account (also limits how far a nonce may run ahead of the account's current
    /// nonce).
    pub max_ops_per_account: usize,
    /// Maximum number of an account's ops queued behind a nonce gap (not in the consecutive run starting at the
    /// account's current nonce, past the nonces path C ops waiting for forced inclusion take from it,
    /// [`Mempool::set_forced`]): they cannot be packed until the gap is filled. It leaves room for ops that arrive out
    /// of order.
    pub max_gapped_per_account: usize,
    /// How much a replacement raises the sponsored fee of the op it replaces at least, in percent of that fee
    /// ([`Policy::replacement_fee`]).
    pub replacement_bump_percent: u128,
}

impl Default for Policy {
    fn default() -> Self {
        Policy {
            min_token_fee: 1,
            min_relay_token_fee: 1,
            min_fee_per_kib: 0,
            max_ops: 10_000,
            max_bytes: 32 * 1024 * 1024,
            max_ops_per_account: 64,
            max_gapped_per_account: 8,
            replacement_bump_percent: 10,
        }
    }
}

impl Policy {
    /// Admission (relay) floor: the smaller of the minimum token amounts for relaying and for packing.
    pub fn relay_floor(&self) -> u128 {
        self.min_relay_token_fee.min(self.min_token_fee)
    }

    /// Whether an op with sponsored fee `fee` and fee rate `rate` meets the packing floor.
    pub fn packable(&self, fee: u128, rate: u128) -> bool {
        fee >= self.min_token_fee && rate >= self.min_fee_per_kib
    }

    /// The least sponsored fee of an op replacing a queued op that pays `fee` (D-130): `fee` raised by
    /// [`Policy::replacement_bump_percent`] of it (rounded up), and by no less than the relay floor or 1 (saturating).
    /// The raise pays for relaying the replacement as a new op would; the percentage makes each further replacement
    /// cost more, so one op cannot be replaced over and over for one more unit each (A2-08).
    pub fn replacement_fee(&self, fee: u128) -> u128 {
        let share = fee
            .saturating_mul(self.replacement_bump_percent)
            .saturating_add(99)
            .checked_div(100)
            .unwrap_or(0);
        fee.saturating_add(share.max(self.relay_floor()).max(1))
    }
}

/// An op's cost in the packing budget (local policy): the larger of the encoded bytes (`encoded_len`) and the bytes
/// equivalent to `max_gas`, at least 1. A block's gas limit is equated with a block's data limit:
/// `max_gas × MAX_BLOCK_DATA_BYTES / BLOCK_GAS_LIMIT` (rounded down).
pub fn op_cost(params: &Params, rules: &Rules, encoded_len: usize, max_gas: u64) -> u64 {
    let bytes = u64::try_from(encoded_len).unwrap_or(u64::MAX);
    let data = u128::try_from(params.max_block_data_bytes).unwrap_or(u128::MAX);
    let by_gas = match rules.block_gas_limit {
        0 => 0,
        limit => u128::from(max_gas)
            .saturating_mul(data)
            .checked_div(u128::from(limit))
            .unwrap_or(0),
    };
    let by_gas = u64::try_from(by_gas).unwrap_or(u64::MAX);
    bytes.max(by_gas).max(1)
}

/// Fee rate: sponsored fee per KiB of cost, `fee × 1024 / cost` (rounded down, saturating).
pub fn fee_rate(fee: u128, cost: u64) -> u128 {
    fee.saturating_mul(1024)
        .checked_div(u128::from(cost.max(1)))
        .unwrap_or(0)
}

/// Why an op was rejected.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum AdmitError {
    /// Invalid under the consensus rules (signature, nonce, balance, etc.).
    #[error("invalid op: {0}")]
    Invalid(String),
    /// Valid, but does not meet this node's policy (fee too low, pool full, replacement fee not high enough).
    #[error("does not meet this node's policy: {0}")]
    Policy(String),
    /// Reading the state failed (local fault).
    #[error("reading state failed: {0}")]
    State(String),
}

#[derive(Clone, Debug)]
struct PoolOp {
    op: VmOp,
    op_id: [u8; 32],
    /// Sponsored fee (tokens).
    fee: u128,
    /// Fee rate ([`fee_rate`]), for ordering.
    rate: u128,
    /// Encoded length (bytes), counted against [`Policy::max_bytes`].
    size: usize,
    /// The most KSA the action can move out of the account besides the sponsored fee ([`ksa_outflow`]).
    outflow: u128,
}

/// Where an op stands in the eviction order (local policy), from the first to go to the last. Following an account's
/// consecutive run from its current nonce, an op stands no higher than the ops before it: it can only be packed after
/// them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Standing {
    /// Behind a nonce gap (or below the account nonce, or at a nonce a path C op waiting for forced inclusion takes
    /// first, [`Mempool::set_forced`]): cannot be packed until the gap is filled.
    Gapped,
    /// The KSA balance, less what the account's earlier ops can spend at most (sponsored fees, KSA transfers, KSA
    /// attached to calls and deployments), does not cover its sponsored fee: unless KSA comes in first, it fails at
    /// packing.
    Unfunded,
    /// Below this node's packing floor ([`Policy::packable`]), or after such an op: kept to be forwarded to other
    /// producers.
    Unpackable,
    /// A packing candidate of this node.
    Packable,
}

/// An account's place in the eviction order: that of its last op, the first of its ops to go. Ordered from the first
/// to go: by standing, fee rate and sponsored fee from low to high; on a tie a newcomer goes before the ops already in
/// the pool, and among those the one that would be packed last (the higher `op_id`, see [`Head`]) goes first.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct EvictKey {
    standing: Standing,
    rate: u128,
    fee: u128,
    incumbent: bool,
    op_id: Reverse<[u8; 32]>,
    account: [u8; 32],
    nonce: u64,
}

/// One account's queued ops.
#[derive(Debug, Default)]
struct Queue {
    /// The account's nonce in the state last read for it (on admission, and on the re-check after a chain change).
    nonce: u64,
    /// The account's KSA balance in the same state.
    balance: u128,
    /// The ops by nonce.
    ops: BTreeMap<u64, PoolOp>,
    /// The account's entry in [`Mempool::evict`].
    key: Option<EvictKey>,
}

impl Queue {
    /// The first nonce left to the account's path B ops: the account nonce, past the nonces the account's path C ops
    /// waiting for forced inclusion take in turn. `forced` holds their nonces in BTC order ([`Mempool::set_forced`]),
    /// the order a block runs its forced entries in, before its path B ops: going through them from the account nonce,
    /// one equal to the next nonce takes it, and one that is not fails out of turn and takes none. `None` when they
    /// reach the largest nonce.
    fn start(&self, forced: &[u64]) -> Option<u64> {
        let mut nonce = self.nonce;
        for &next in forced {
            if next == nonce {
                nonce = nonce.checked_add(1)?;
            }
        }
        Some(nonce)
    }

    /// The end of the consecutive run starting at [`Queue::start`]: the first nonce from there that is not queued
    /// (`None` when the run reaches the largest nonce).
    fn run_end(&self, forced: &[u64]) -> Option<u64> {
        let mut expected = self.start(forced)?;
        while self.ops.contains_key(&expected) {
            expected = expected.checked_add(1)?;
        }
        Some(expected)
    }

    /// Number of ops queued behind a nonce gap (`forced` as for [`Queue::start`]).
    fn gapped(&self, forced: &[u64]) -> usize {
        self.run_end(forced)
            .map_or(0, |end| self.ops.range(end..).count())
    }

    /// The first nonce whose op the KSA balance no longer covers: the balance is below the sponsored fees of the
    /// queued ops up to and including it (the admission rule). The account's later ops are not covered either.
    fn unpaid_from(&self) -> Option<u64> {
        let mut needed: u128 = 0;
        for (nonce, pool_op) in self.ops.range(self.nonce..) {
            needed = needed.saturating_add(pool_op.fee);
            if needed > self.balance {
                return Some(*nonce);
            }
        }
        None
    }

    /// The standing of the last op ([`Standing`]; `forced` as for [`Queue::start`]).
    fn standing(&self, policy: &Policy, forced: &[u64]) -> Standing {
        let Some(start) = self.start(forced) else {
            return Standing::Gapped;
        };
        if self
            .ops
            .last_key_value()
            .is_none_or(|(nonce, _)| *nonce < start)
        {
            return Standing::Gapped;
        }
        let mut standing = Standing::Packable;
        let mut expected = Some(start);
        let mut available = self.balance;
        for (nonce, pool_op) in self.ops.range(start..) {
            if expected != Some(*nonce) {
                return Standing::Gapped;
            }
            expected = nonce.checked_add(1);
            match available.checked_sub(pool_op.fee) {
                // At most what the action can move out; an action that would overdraw fails and moves nothing
                Some(rest) => available = rest.checked_sub(pool_op.outflow).unwrap_or(rest),
                None => standing = standing.min(Standing::Unfunded),
            }
            if !policy.packable(pool_op.fee, pool_op.rate) {
                standing = standing.min(Standing::Unpackable);
            }
        }
        standing
    }

    /// The `op_id`s of the account's packing candidates in a state where its nonce is the one last read for it
    /// ([`packing_run`]): a change of the pool that leaves them, and every other account's, as they were cannot alter
    /// the packing candidates (D-130).
    fn live(&self, policy: &Policy) -> Vec<[u8; 32]> {
        packing_run(&self.ops, self.nonce, policy)
            .map(|pool_op| pool_op.op_id)
            .collect()
    }

    /// Whether the op at `nonce` is one of the account's packing candidates ([`Queue::live`]).
    fn is_live(&self, nonce: u64, policy: &Policy) -> bool {
        packing_run(&self.ops, self.nonce, policy).any(|pool_op| pool_op.op.nonce == nonce)
    }

    /// The account's place in the eviction order (`forced` as for [`Queue::start`]); `newcomer` is the op being
    /// admitted, if any.
    fn eviction_key(
        &self,
        account: &[u8; 32],
        policy: &Policy,
        forced: &[u64],
        newcomer: Option<&[u8; 32]>,
    ) -> Option<EvictKey> {
        let (nonce, last) = self.ops.last_key_value()?;
        Some(EvictKey {
            standing: self.standing(policy, forced),
            rate: last.rate,
            fee: last.fee,
            incumbent: newcomer != Some(&last.op_id),
            op_id: Reverse(last.op_id),
            account: *account,
            nonce: *nonce,
        })
    }
}

/// The ops taken out while admitting an op, with their accounts, to be put back if it is rejected.
type Undo = Vec<([u8; 32], PoolOp)>;

/// The entry pool.
#[derive(Debug)]
pub struct Mempool {
    params: Params,
    /// The rules admission goes by (D-076): the rules of the next block after the tip, updated by the node on admission
    /// and on main-chain changes ([`Mempool::set_rules`]).
    rules: Rules,
    policy: Policy,
    by_account: BTreeMap<[u8; 32], Queue>,
    by_id: BTreeMap<[u8; 32], ([u8; 32], u64)>,
    /// Each account's last op in eviction order ([`EvictKey`]): the first entry goes first when the pool is full.
    evict: BTreeSet<EvictKey>,
    /// The nonces of the path C ops waiting for forced inclusion, by account in BTC order ([`Mempool::set_forced`]).
    forced: BTreeMap<[u8; 32], Vec<u64>>,
    /// Total encoded bytes of the ops in the pool.
    bytes: usize,
    generation: u64,
}

impl Mempool {
    /// The local policy.
    pub fn policy(&self) -> &Policy {
        &self.policy
    }

    /// An empty entry pool (admission goes by the rules after genesis until [`Mempool::set_rules`]).
    pub fn new(params: Params, policy: Policy) -> Self {
        Mempool {
            rules: params.rules(0),
            params,
            policy,
            by_account: BTreeMap::new(),
            by_id: BTreeMap::new(),
            evict: BTreeSet::new(),
            forced: BTreeMap::new(),
            bytes: 0,
            generation: 0,
        }
    }

    /// The rules admission goes by (D-076).
    pub fn rules(&self) -> &Rules {
        &self.rules
    }

    /// Update the rules admission goes by (D-076): the rules of the next block after the tip. Ops already in the pool
    /// are re-checked against them by the next [`Mempool::remove_committed`].
    pub fn set_rules(&mut self, rules: Rules) {
        self.rules = rules;
    }

    /// Set the nonces of the path C ops waiting for forced inclusion, by account in BTC order (§7.4): the node passes
    /// in those of the native transactions on its BTC best chain that its main chain has not included yet and whose op
    /// passes the authentication a block forcing it applies, after every main-chain change and BTC sync. A block runs
    /// its forced entries before its path B ops, in BTC order, so those taking an account's nonces in turn from its
    /// current nonce count as filled, and the path B ops queued above them are not behind a nonce gap (D-104: after a
    /// BTC reorg the retreat returns the path B ops of the blocks it takes off while the path C ops below them are
    /// included again only by the next blocks; counted as behind a gap, those beyond
    /// [`Policy::max_gapped_per_account`] were dropped and the ops signed above them refused). One out of turn there
    /// fails and takes none, and so does a path C op above a path B op still in the pool, which it runs before: their
    /// nonces stay gaps. Admission, eviction and the re-check after a chain change go by them, the packing candidates
    /// do not: the path B ops above a path C op follow it in the next block.
    pub fn set_forced(&mut self, forced: BTreeMap<[u8; 32], Vec<u64>>) {
        let accounts: BTreeSet<[u8; 32]> =
            self.forced.keys().chain(forced.keys()).copied().collect();
        self.forced = forced;
        for account in &accounts {
            self.rekey(account, None);
        }
    }

    /// Number of ops in the pool.
    pub fn len(&self) -> usize {
        self.by_id.len()
    }

    /// Whether the pool is empty.
    pub fn is_empty(&self) -> bool {
        self.by_id.is_empty()
    }

    /// Total encoded bytes of the ops in the pool (limited by [`Policy::max_bytes`]).
    pub fn bytes(&self) -> usize {
        self.bytes
    }

    /// Incremented every time a change of the pool can alter the packing candidates ([`Mempool::candidates`]): an op
    /// admitted, replaced or evicted in its account's run of candidates in the state last read for that account, an op
    /// removed ([`Mempool::remove`], [`Mempool::remove_committed`]), or [`Mempool::mark_changed`]. The producer uses it
    /// to tell whether a packing template is stale; ops behind a nonce gap, or after one below the packing floor, do not
    /// move it (D-130).
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Moves the generation without changing the pool (D-130). [`Mempool::insert`] judges whether an op alters the
    /// packing candidates in the state it is given; the node calls this when the main-chain tip has moved since it read
    /// that state, so that a template built on the new tip is checked again.
    pub fn mark_changed(&mut self) {
        self.bump();
    }

    /// Look up by `op_id`.
    pub fn get(&self, op_id: &[u8; 32]) -> Option<&VmOp> {
        let (account, nonce) = self.by_id.get(op_id)?;
        self.by_account
            .get(account)
            .and_then(|queue| queue.ops.get(nonce))
            .map(|p| &p.op)
    }

    /// Whether the pool contains this op.
    pub fn contains(&self, op_id: &[u8; 32]) -> bool {
        self.by_id.contains_key(op_id)
    }

    /// Accept a path-B op and return its `op_id`. The same op already in the pool returns success directly; another op
    /// with the same account and nonce replaces it only when its sponsored fee reaches [`Policy::replacement_fee`].
    /// Admission goes by the relay floor ([`Policy::relay_floor`]); ops below the packing floor enter the pool but are
    /// not this node's packing candidates. When the op would take the account over its limit of ops behind a nonce gap,
    /// or the pool over its op count or byte limit, ops that stand lower are evicted to make room (module docs); the op
    /// is rejected when it would itself be the first to go. The generation moves only when the packing candidates may
    /// have changed ([`Mempool::generation`]).
    pub fn insert(
        &mut self,
        op: VmOp,
        state: &Arc<dyn StateSource>,
    ) -> Result<[u8; 32], AdmitError> {
        let (op_id, fee, cost, size) = self.check_static(&op)?;
        if self.by_id.contains_key(&op_id) {
            return Ok(op_id);
        }
        let floor = self.policy.relay_floor();
        if fee < floor {
            return Err(AdmitError::Policy(format!(
                "sponsored fee {fee} is below this node's minimum {floor}"
            )));
        }

        let ov = Overlay::new(Arc::clone(state));
        let account = builtin::account_state(&ov, &op.account).map_err(state_err)?;
        if op.nonce < account.nonce {
            return Err(AdmitError::Invalid(format!(
                "nonce {} already used (account nonce is {})",
                op.nonce, account.nonce
            )));
        }
        let ahead = op.nonce.saturating_sub(account.nonce);
        if ahead >= u64::try_from(self.policy.max_ops_per_account).unwrap_or(u64::MAX) {
            return Err(AdmitError::Policy(format!(
                "nonce {} runs too far ahead of account nonce {}",
                op.nonce, account.nonce
            )));
        }

        let queued = self.by_account.get(&op.account).map(|queue| &queue.ops);
        let replaced = queued.and_then(|ops| ops.get(&op.nonce));
        if let Some(existing) = replaced {
            let least = self.policy.replacement_fee(existing.fee);
            if fee < least {
                return Err(AdmitError::Policy(format!(
                    "an op with the same nonce already exists; replacing it needs a sponsored fee of at least {least}"
                )));
            }
        } else if queued.is_some_and(|ops| ops.len() >= self.policy.max_ops_per_account) {
            return Err(AdmitError::Policy(
                "too many ops queued for this account".into(),
            ));
        }

        // The balance must cover the sponsored fees of all queued ops of this account with nonce not above this op's
        // (excluding the one being replaced)
        let mut needed = fee;
        if let Some(ops) = queued {
            for (nonce, queued_op) in ops.range(..=op.nonce) {
                if *nonce != op.nonce {
                    needed = needed.saturating_add(queued_op.fee);
                }
            }
        }
        let balance = builtin::balance(&ov, &KSA_ASSET_ID, &op.account).map_err(state_err)?;
        if balance < needed {
            return Err(AdmitError::Invalid(format!(
                "KSA balance {balance} does not cover the sponsored fees {needed} of the queued ops"
            )));
        }

        let (account_id, nonce) = (op.account, op.nonce);
        let queue = self.by_account.entry(account_id).or_default();
        queue.nonce = account.nonce;
        queue.balance = balance;
        // The account's packing candidates before the change, in the state just read (D-130)
        let live = queue.live(&self.policy);
        let pool_op = PoolOp {
            outflow: ksa_outflow(&op.action),
            op,
            op_id,
            fee,
            rate: fee_rate(fee, cost),
            size,
        };
        let mut undo = Undo::new();
        if let Some(old) = self.put(account_id, pool_op) {
            undo.push((account_id, old));
        }
        let made_room = self.make_room(&account_id, &op_id, nonce, &mut undo);
        let mut touched: BTreeSet<[u8; 32]> = undo.iter().map(|(account, _)| *account).collect();
        touched.insert(account_id);
        if made_room.is_err() {
            // Put the pool back as it was
            if let Some((account, nonce)) = self.by_id.get(&op_id).copied() {
                self.take(&account, nonce);
            }
            for (account, old) in undo.into_iter().rev() {
                self.put(account, old);
            }
        }
        for account in &touched {
            self.settle(account);
        }
        let evicted_live = made_room?;
        let changed = evicted_live
            || self
                .by_account
                .get(&account_id)
                .map_or_else(Vec::new, |queue| queue.live(&self.policy))
                != live;
        if changed {
            self.bump();
        }
        Ok(op_id)
    }

    /// After the new op `op_id` (at `op_nonce`) of `account` has been put in: drops the account's later ops the balance
    /// no longer covers (a replacement can raise an earlier sponsored fee); when the new op is behind a nonce gap,
    /// brings the account back within its limit of such ops (from its last op; an op in the consecutive run never costs
    /// the account its other ops, so ops returned to the pool after a reorg keep the ones queued after them); then
    /// brings the pool within its op count and byte limits, always taking the last op of the account that stands
    /// lowest. The ops taken out go to `undo`; an error means the new op itself would have had to go, and the caller
    /// puts everything back. Returns whether an op evicted for the op count or byte limits was one of its account's
    /// packing candidates ([`Queue::is_live`]); the caller compares `account`'s own candidates before and after.
    fn make_room(
        &mut self,
        account: &[u8; 32],
        op_id: &[u8; 32],
        op_nonce: u64,
        undo: &mut Undo,
    ) -> Result<bool, AdmitError> {
        let unpaid: Vec<(u64, [u8; 32])> = self
            .by_account
            .get(account)
            .and_then(|queue| {
                let from = queue.unpaid_from()?;
                Some(
                    queue
                        .ops
                        .range(from..)
                        .map(|(n, p)| (*n, p.op_id))
                        .collect(),
                )
            })
            .unwrap_or_default();
        for (nonce, id) in unpaid {
            if id == *op_id {
                return Err(AdmitError::Invalid(
                    "KSA balance does not cover the sponsored fees of the queued ops".into(),
                ));
            }
            if let Some(old) = self.take(account, nonce) {
                undo.push((*account, old));
            }
        }
        let gapped = self
            .by_account
            .get(account)
            .and_then(|queue| queue.run_end(forced_of(&self.forced, account)))
            .is_some_and(|end| op_nonce > end);
        loop {
            let last = self
                .by_account
                .get(account)
                .filter(|queue| {
                    gapped
                        && queue.gapped(forced_of(&self.forced, account))
                            > self.policy.max_gapped_per_account
                })
                .and_then(|queue| queue.ops.last_key_value())
                .map(|(n, p)| (*n, p.op_id));
            let Some((nonce, id)) = last else {
                break;
            };
            if id == *op_id {
                return Err(AdmitError::Policy(
                    "too many ops queued behind a nonce gap for this account".into(),
                ));
            }
            match self.take(account, nonce) {
                Some(old) => undo.push((*account, old)),
                None => break,
            }
        }
        self.rekey(account, Some(op_id));
        let mut evicted_live = false;
        while self.by_id.len() > self.policy.max_ops || self.bytes > self.policy.max_bytes {
            let Some(key) = self.evict.first().copied() else {
                break;
            };
            if key.op_id.0 == *op_id {
                return Err(AdmitError::Policy(
                    "entry pool is full and this op stands below the ops it would have to evict"
                        .into(),
                ));
            }
            let live = self
                .by_account
                .get(&key.account)
                .is_some_and(|queue| queue.is_live(key.nonce, &self.policy));
            match self.take(&key.account, key.nonce) {
                Some(old) => {
                    evicted_live |= live;
                    undo.push((key.account, old));
                }
                // A stale entry (never expected): drop it so the loop moves on
                None => {
                    self.evict.remove(&key);
                }
            }
            self.rekey(&key.account, Some(op_id));
        }
        Ok(evicted_live)
    }

    /// Remove an op. Returns whether it existed.
    pub fn remove(&mut self, op_id: &[u8; 32]) -> bool {
        let existed = self.remove_quiet(op_id);
        if existed {
            self.bump();
        }
        existed
    }

    /// Put an op into its account's queue and the indexes; returns the op it replaced. The account's eviction entry is
    /// updated by the caller ([`Mempool::rekey`]).
    fn put(&mut self, account: [u8; 32], pool_op: PoolOp) -> Option<PoolOp> {
        let (op_id, nonce, size) = (pool_op.op_id, pool_op.op.nonce, pool_op.size);
        let old = self
            .by_account
            .entry(account)
            .or_default()
            .ops
            .insert(nonce, pool_op);
        if let Some(old) = &old {
            self.by_id.remove(&old.op_id);
            self.bytes = self.bytes.saturating_sub(old.size);
        }
        self.by_id.insert(op_id, (account, nonce));
        self.bytes = self.bytes.saturating_add(size);
        old
    }

    /// Take the op at `nonce` out of the account's queue and the indexes. The account's eviction entry is updated by
    /// the caller.
    fn take(&mut self, account: &[u8; 32], nonce: u64) -> Option<PoolOp> {
        let old = self.by_account.get_mut(account)?.ops.remove(&nonce)?;
        self.by_id.remove(&old.op_id);
        self.bytes = self.bytes.saturating_sub(old.size);
        Some(old)
    }

    /// Recompute the account's eviction entry after its queue, nonce or balance changed.
    fn rekey(&mut self, account: &[u8; 32], newcomer: Option<&[u8; 32]>) {
        let Some(queue) = self.by_account.get_mut(account) else {
            return;
        };
        if let Some(old) = queue.key.take() {
            self.evict.remove(&old);
        }
        queue.key = queue.eviction_key(
            account,
            &self.policy,
            forced_of(&self.forced, account),
            newcomer,
        );
        if let Some(key) = queue.key {
            self.evict.insert(key);
        }
    }

    /// Recompute the account's eviction entry, and forget the account when it has no ops left.
    fn settle(&mut self, account: &[u8; 32]) {
        self.rekey(account, None);
        if self
            .by_account
            .get(account)
            .is_some_and(|queue| queue.ops.is_empty())
        {
            self.by_account.remove(account);
        }
    }

    /// Stateless checks only (version, `chain_id`, encoded length, fee mode, signature); returns the `op_id`.
    /// An op that passes may still be rejected by [`Mempool::insert`] because of state (nonce, balance) or this node's
    /// policy.
    pub fn check_stateless(&self, op: &VmOp) -> Result<[u8; 32], AdmitError> {
        self.check_static(op).map(|(op_id, _, _, _)| op_id)
    }

    /// At most `max` ops from the pool (ordered by account, then nonce), for newly connected peers to sync the entry
    /// pool.
    pub fn pending(&self, max: usize) -> Vec<VmOp> {
        self.by_account
            .values()
            .flat_map(|queue| queue.ops.values())
            .take(max)
            .map(|pool_op| pool_op.op.clone())
            .collect()
    }

    /// Packing candidates: for each account, the consecutive ops starting at the current nonce, up to the first one
    /// below the packing floor ([`Policy::packable`]); across accounts by the fee rate of the queue head from high to
    /// low (ties by sponsored fee, then by `op_id`). The caller tries to include them in order; when the execution
    /// layer rejects an op, all later ops of the same account should be skipped.
    pub fn candidates(&self, state: &Arc<dyn StateSource>) -> Result<Vec<VmOp>, AdmitError> {
        let ov = Overlay::new(Arc::clone(state));
        self.candidates_by(|account| {
            Ok(builtin::account_state(&ov, account)
                .map_err(state_err)?
                .nonce)
        })
    }

    /// [`Mempool::candidates`] with each account's current nonce from `nonce_of`, called once for every account in the
    /// pool, in order: the producer answers from the nonces it read in the same state for an earlier template, so that
    /// a template rebuilt on the same main-chain tip reads the state only for the accounts new to the pool (D-130).
    pub fn candidates_by(
        &self,
        mut nonce_of: impl FnMut(&[u8; 32]) -> Result<u64, AdmitError>,
    ) -> Result<Vec<VmOp>, AdmitError> {
        let mut chains: Vec<Vec<&PoolOp>> = Vec::new();
        for (account, queue) in &self.by_account {
            let chain: Vec<&PoolOp> =
                packing_run(&queue.ops, nonce_of(account)?, &self.policy).collect();
            if !chain.is_empty() {
                chains.push(chain);
            }
        }

        let mut heap = BinaryHeap::new();
        for (index, chain) in chains.iter().enumerate() {
            if let Some(head) = chain.first() {
                heap.push(Head::new(head, index, 0));
            }
        }
        let mut out = Vec::new();
        while let Some(head) = heap.pop() {
            let Some(chain) = chains.get(head.chain) else {
                continue;
            };
            if let Some(pool_op) = chain.get(head.position) {
                out.push(pool_op.op.clone());
            }
            let next = head.position.saturating_add(1);
            if let Some(pool_op) = chain.get(next) {
                heap.push(Head::new(pool_op, head.chain, next));
            }
        }
        Ok(out)
    }

    /// After a main-chain change (the node calls it with the new tip state, after [`Mempool::set_rules`]): remove the
    /// included ops and the ops whose nonce has been used, then re-check every account's remaining ops against its
    /// current KSA balance and the current rules: the first op the balance no longer covers (the admission rule) goes
    /// with all of the account's later ops, ops whose version or `max_gas` the rules no longer accept go, and so do the
    /// account's ops behind a nonce gap beyond [`Policy::max_gapped_per_account`] (from the last one). None of these
    /// could ever be packed.
    pub fn remove_committed(
        &mut self,
        included: &[[u8; 32]],
        state: &Arc<dyn StateSource>,
    ) -> Result<(), AdmitError> {
        // Only removals happen here: the op count tells whether the contents changed, also when reading the state
        // fails halfway
        let before = self.by_id.len();
        let result = self.recheck_all(included, state);
        if self.by_id.len() != before {
            self.bump();
        }
        result
    }

    /// The work of [`Mempool::remove_committed`].
    fn recheck_all(
        &mut self,
        included: &[[u8; 32]],
        state: &Arc<dyn StateSource>,
    ) -> Result<(), AdmitError> {
        for op_id in included {
            self.remove_quiet(op_id);
        }
        let ov = Overlay::new(Arc::clone(state));
        let accounts: Vec<[u8; 32]> = self.by_account.keys().copied().collect();
        for account in &accounts {
            let nonce = builtin::account_state(&ov, account)
                .map_err(state_err)?
                .nonce;
            let balance = builtin::balance(&ov, &KSA_ASSET_ID, account).map_err(state_err)?;
            self.recheck(account, nonce, balance);
        }
        Ok(())
    }

    /// Re-check one account's queue against its current nonce and KSA balance and the current rules
    /// ([`Mempool::remove_committed`]).
    fn recheck(&mut self, account: &[u8; 32], nonce: u64, balance: u128) {
        let rules = self.rules;
        let max_gapped = self.policy.max_gapped_per_account;
        let Some(queue) = self.by_account.get_mut(account) else {
            return;
        };
        queue.nonce = nonce;
        queue.balance = balance;
        // Nonces already used, and ops the rules no longer accept
        let mut gone: BTreeSet<u64> = queue.ops.range(..nonce).map(|(n, _)| *n).collect();
        gone.extend(
            queue
                .ops
                .iter()
                .filter(|(_, p)| !accepted_by(&rules, &p.op))
                .map(|(n, _)| *n),
        );
        for n in gone {
            self.take(account, n);
        }
        // From the first op the balance no longer covers
        let unpaid: Vec<u64> = self
            .by_account
            .get(account)
            .and_then(|queue| {
                let from = queue.unpaid_from()?;
                Some(queue.ops.range(from..).map(|(n, _)| *n).collect())
            })
            .unwrap_or_default();
        for n in unpaid {
            self.take(account, n);
        }
        // Ops behind a nonce gap beyond the limit, from the last one
        loop {
            let last = self
                .by_account
                .get(account)
                .filter(|queue| queue.gapped(forced_of(&self.forced, account)) > max_gapped)
                .and_then(|queue| queue.ops.last_key_value())
                .map(|(n, _)| *n);
            let Some(last) = last else {
                break;
            };
            if self.take(account, last).is_none() {
                break;
            }
        }
        self.settle(account);
    }

    /// Remove an op without incrementing the generation; returns whether it existed.
    fn remove_quiet(&mut self, op_id: &[u8; 32]) -> bool {
        let Some((account, nonce)) = self.by_id.get(op_id).copied() else {
            return false;
        };
        if self.take(&account, nonce).is_none() {
            self.by_id.remove(op_id);
        }
        self.settle(&account);
        true
    }

    /// Stateless checks: version, chain ID, encoded length, fee mode, signature. Returns `(op_id, sponsored fee,
    /// cost, encoded length)`.
    fn check_static(&self, op: &VmOp) -> Result<([u8; 32], u128, u64, usize), AdmitError> {
        if !self.rules.accepts_vmop_version(op.version) {
            return Err(AdmitError::Invalid(format!(
                "unsupported VmOp version {}",
                op.version
            )));
        }
        if op.chain_id != self.params.chain_id {
            return Err(AdmitError::Invalid(format!(
                "chain_id {} does not match this network",
                op.chain_id
            )));
        }
        let encoded = op
            .encode()
            .map_err(|e| AdmitError::Invalid(e.to_string()))?;
        if encoded.len() > self.params.max_op_bytes {
            return Err(AdmitError::Invalid(
                "VmOp encoding exceeds MAX_OP_BYTES".into(),
            ));
        }
        let fee = match op.fee {
            FeeMode::Token { amount } => amount,
            FeeMode::BtcNative => {
                return Err(AdmitError::Invalid(
                    "path C must be submitted through a BTC native transaction".into(),
                ));
            }
        };
        if op.max_gas > self.rules.block_gas_limit {
            return Err(AdmitError::Invalid(
                "max_gas exceeds BLOCK_GAS_LIMIT".into(),
            ));
        }
        verify_vmop(op).map_err(|e| AdmitError::Invalid(format!("invalid signature: {e}")))?;
        let cost = op_cost(&self.params, &self.rules, encoded.len(), op.max_gas);
        Ok((
            ksana_primitives::hash::sha256(&encoded),
            fee,
            cost,
            encoded.len(),
        ))
    }

    fn bump(&mut self) {
        self.generation = self.generation.wrapping_add(1);
    }
}

fn state_err(fault: builtin::Fault) -> AdmitError {
    AdmitError::State(format!("{fault:?}"))
}

/// The nonces of the account's path C ops waiting for forced inclusion, in BTC order ([`Mempool::set_forced`]).
fn forced_of<'a>(forced: &'a BTreeMap<[u8; 32], Vec<u64>>, account: &[u8; 32]) -> &'a [u64] {
    forced.get(account).map_or(&[], Vec::as_slice)
}

/// An account's packing candidates in a state where its nonce is `nonce`: its queued ops (`ops`, by nonce) in the
/// consecutive run from `nonce`, up to the first one below the packing floor ([`Policy::packable`]).
fn packing_run<'a>(
    ops: &'a BTreeMap<u64, PoolOp>,
    nonce: u64,
    policy: &'a Policy,
) -> impl Iterator<Item = &'a PoolOp> + 'a {
    ops.range(nonce..)
        .scan(Some(nonce), |expected, (queued, pool_op)| {
            if *expected != Some(*queued) || !policy.packable(pool_op.fee, pool_op.rate) {
                return None;
            }
            *expected = queued.checked_add(1);
            Some(pool_op)
        })
}

/// Whether the rules still accept an op that passed [`Mempool::check_static`] under earlier rules: the parts of that
/// check that depend on the rules (VmOp version, `max_gas` within `BLOCK_GAS_LIMIT`).
fn accepted_by(rules: &Rules, op: &VmOp) -> bool {
    rules.accepts_vmop_version(op.version) && op.max_gas <= rules.block_gas_limit
}

/// The most KSA an op's action can move out of the signing account besides the sponsored fee: a KSA transfer, or KSA
/// attached to a call or deployment. Nothing else can: contracts only move their own balances (`docs/ABI.md` §3).
fn ksa_outflow(action: &Action) -> u128 {
    match action {
        Action::AssetTransfer { asset, amount, .. } if *asset == KSA_ASSET_ID => *amount,
        Action::AssetTransfer { .. } => 0,
        Action::Call { funds, .. } | Action::Deploy { funds, .. } => funds
            .iter()
            .filter(|f| f.asset == KSA_ASSET_ID)
            .fold(0, |sum: u128, f| sum.saturating_add(f.amount)),
    }
}

/// An account's queue head in the heap: by fee rate from high to low, ties by sponsored fee from high to low, then by
/// `op_id` from low to high (a deterministic order).
#[derive(PartialEq, Eq)]
struct Head {
    rate: u128,
    fee: u128,
    op_id: [u8; 32],
    chain: usize,
    position: usize,
}

impl Head {
    fn new(pool_op: &PoolOp, chain: usize, position: usize) -> Self {
        Head {
            rate: pool_op.rate,
            fee: pool_op.fee,
            op_id: pool_op.op_id,
            chain,
            position,
        }
    }
}

impl Ord for Head {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.rate
            .cmp(&other.rate)
            .then_with(|| self.fee.cmp(&other.fee))
            .then_with(|| other.op_id.cmp(&self.op_id))
            .then_with(|| other.chain.cmp(&self.chain))
            .then_with(|| other.position.cmp(&self.position))
    }
}

impl PartialOrd for Head {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
