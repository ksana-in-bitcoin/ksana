//! Block execution (SPEC §4.3, §5, §6.2, §7.4, §8.1, §8.3, §8.5, §9.2; D-018, D-035, D-036, D-038, D-078, D-079).
//!
//! # Classification of results
//!
//! - **Failed op**: the receipt is [`OpStatus::Failed`] and the block stays valid. Path B rolls back only the action
//!   itself (fee and nonce are kept); path C likewise rolls back only the action when it passed authentication and the
//!   nonce matched (the nonce increment is kept), otherwise it changes no state at all (revision of D-018).
//! - **Invalid block**: [`BlockError::Invalid`], decided only by the block contents, the parent state, the BTC data at
//!   the anchor and the block context (the merged blocks and the unlockable segment, given deterministically by the
//!   DAG).
//! - **Local fault**: [`BlockError::Fatal`] (storage, VM engine, [`BtcView`] data source failure), unrelated to the
//!   block contents; the block is never judged invalid because of it.
//!
//! # Entry rules
//!
//! Path A (merged payment with BTC) and arrears entries have been removed (D-078): paying with BTC is now done by
//! topping up KSA via path C and then using path B.
//!
//! - **Order** (§6.2): forced-inclusion entries come first, then the unlock entry, and path-B entries last.
//! - **gas** (§8.3, D-036): entries accumulate into the block total by `max_gas`. Forced-inclusion entries count as
//!   `min(max_gas, NATIVE_MAX_GAS)` and are themselves not bound by `BLOCK_GAS_LIMIT`; if adding a path-B entry pushes
//!   the total over the limit the block is invalid (unlock entries count as 0), so once the forced-inclusion entries
//!   alone exceed the limit, the block cannot contain any path-B entry.
//! - **Path C** (`Forced`, §7.4, §8.1, revision of D-018): if any of version, chain ID, encoded length, fee mode
//!   `BtcNative`, signature and nonce is not satisfied, the op is recorded as failed without changing state; when all
//!   are satisfied the nonce is incremented, and afterwards `max_gas > NATIVE_MAX_GAS` or a failing action only records
//!   a failed op (the action is rolled back, the nonce increment is kept).
//! - **Path B** (`Sponsored`, §5.2, §8.1): version, chain ID, encoded length, fee mode `Token`, signature, consecutive
//!   nonce and a KSA balance sufficient for the fee, otherwise the block is invalid. Once passed, the fee is deducted
//!   into the sponsored-fee record of this block's acceptance position (D-079), the nonce is incremented, then the
//!   action runs.
//! - **Unlock** (`Unlock { height, index }`, §8.5, D-038, D-074, D-079): at most one per block; the target position
//!   must be in [`BlockEnv::unlockable`], after the unlock cursor, and the block at that position must have entered the
//!   settled closure ([`BtcView::block_settled`]), otherwise the block is invalid. Then the segment is unlocked in
//!   order from after the cursor up to and including the target: a rewarded block is paid its block reward, then the
//!   sponsored-fee record at that position is released, both credited to its producer's available KSA balance; the
//!   cursor is set to the target. The receipt has `op_id` `None` and `gas_used` 0.
//!
//! # Acceptance positions, sponsored-fee records and unlocking (D-079)
//!
//! A main-chain block accepts the blocks that are new in its past according to the DAG: first the blocks it merges
//! ([`MergedBlock`], in consensus order), and itself last. A block's **acceptance position** is `(height, index)`:
//! `height` is the height of the main-chain block that accepts it and `index` is its ordinal in that main-chain block's
//! acceptance order (the merged blocks are 0, 1, … in turn, the main-chain block itself comes last). The acceptance
//! order is the lexicographic order of positions.
//!
//! - When a block collected path-B sponsored fees, `0x08 ‖ height ‖ index` ([`keys::locked_fee`]) is written; without
//!   sponsored fees no record is written at all.
//! - Block rewards are not recorded when the block executes but paid at unlock time by the DAA score of each block in
//!   the segment (the main-chain block itself and the blue merged blocks are rewarded, the KSA total supply is capped
//!   at `KSA_MAX_SUPPLY`); the KSA total supply grows when they are paid.
//! - `0x09` ([`keys::unlock_cursor`]) records the position unlocked up to.
//!
//! So when neither the main-chain block nor the blocks it merges have any entries, no state is written and the state
//! root equals the selected parent's (an empty block does not change the state root).
//!
//! # Merged execution (D-063)
//!
//! The state of a chain block C in the DAG = the selected parent's state → execute the entries of the other blocks in
//! the merge set in turn ([`MergedBlock`], in consensus order) (**lenient mode**) → execute C's own entries (**strict
//! mode**, i.e. the rules above). In lenient mode:
//!
//! - the environment (height, anchor, BTC view and unlockable segment) is C's;
//! - an entry that would make the block invalid in strict mode is **skipped**: the receipt is [`OpStatus::Skipped`] and
//!   no state changes (duplicate inclusion, conflicting ops with the same nonce, insufficient balance, etc.);
//! - the token fee of a path-B op is recorded at the merged block's own acceptance position and belongs to the merged
//!   block's producer (it can be unlocked only after its body has landed on BTC);
//! - unlock entries are always skipped (D-100): only a main-chain block's own unlock entry advances the cursor, so C's
//!   state never depends on how C's BTC view would judge a merged block's unlock entry (that answer can differ when C
//!   is executed again in anchor-invalid mode, D-056); an unlock entry still counts toward the merged block's own limit
//!   of one ([`check_entries_static`]);
//! - forced-inclusion entries behave as in strict mode: one naming no path-C op on C's anchor chain at or below C's
//!   anchor height makes C invalid (D-100), so that executing C again in anchor-invalid mode (D-056), which looks the
//!   transaction up on the current chain, finds what the first execution found;
//! - entry order and the gas limit are the merged block's own validity rules ([`check_entries_static`]) and are not
//!   checked again here;
//! - when C is at the settlement backlog limit ([`MergedBlock::fast_path_paused`]), all path-B entries of the merged
//!   blocks are skipped (D-046, D-063).

use std::sync::Arc;

use ksana_crypto::{verify_native_vmop, verify_vmop};
use ksana_primitives::{
    check_funds, Action, BtcAnchor, Entry, FeeMode, Funds, OutPoint, Params, Rules, VmOp,
    KSA_ASSET_ID,
};
use ksana_vm::limits::EXECUTION_THREAD_STACK;
use ksana_vm::{gas, CallOutcome, CallRequest, FailureReason, Vm, INIT_METHOD};

use crate::builtin::{self, Fault};
use crate::host::OpHost;
use crate::keys::{self, ContractMeta, LockedFee, UnlockCursor};
use crate::state::{AssetChange, ChangeKind, Event, Overlay, StateSource, WriteSet};

/// The block context needed to execute a block.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlockEnv {
    /// Height of this block.
    pub height: u64,
    /// The BTC block this block anchors to.
    pub btc_anchor: BtcAnchor,
    /// Producer (`header.producer`): this block's own sponsored fees belong to this account.
    pub producer: [u8; 32],
    /// Timestamp of this block (`header.timestamp`, seconds): the contracts' `block_timestamp` (D-073). Ops of a merged
    /// block execute as part of the main-chain block that merges it and see the main-chain block's timestamp.
    pub timestamp: u64,
    /// Rules that apply to this block (D-076, `Params::rules(DAA score of this block)`): the entries of this block and
    /// of the blocks it merges all execute under them.
    pub rules: Rules,
    /// Unlockable segment (D-079): the positions on the selected parent's main chain that come after the parent state's
    /// unlock cursor in acceptance order, at most `MAX_UNLOCK_BATCH` of them (this block and the blocks it merges are
    /// not among them: they cannot have entered the settled closure yet). Computed by the node and indexer from the DAG
    /// (same trust model as the merged blocks); unlock entries are judged and paid according to it; not read when this
    /// block has no unlock entry.
    pub unlockable: Vec<UnlockItem>,
}

/// One position in the unlockable segment (D-079): a block in acceptance order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnlockItem {
    /// Height of the main-chain block that accepts it.
    pub height: u64,
    /// Ordinal in that main-chain block's acceptance order (the merged blocks are 0, 1, … in turn, the main-chain block
    /// itself comes last).
    pub index: u32,
    /// Block hash: membership in the settled closure is judged by it ([`BtcView::block_settled`]).
    pub hash: [u8; 32],
    /// Producer (`header.producer`): the block reward (minus the block share and the carrier's share, D-089, D-095) and
    /// the sponsored fees (minus the carrier's share) belong to this account.
    pub producer: [u8; 32],
    /// DAA score of the block (D-063): the block reward is computed from it (D-074).
    pub daa_score: u64,
    /// Weight of the block (D-084, D-090): when rewarded it is paid the `weight` reward slots of DAA scores
    /// `daa_score − weight + 1 … daa_score` (empty blocks double by streak from `EMPTY_BLOCK_DIFFICULTY` up to
    /// `MAX_EMPTY_BLOCK_WEIGHT`, all others are 1).
    pub weight: u64,
    /// Whether it gets the block reward (D-074): the main-chain block itself, and blocks that are blue in the merge set
    /// of the main-chain block that merges them.
    pub rewarded: bool,
}

impl UnlockItem {
    /// Acceptance position `(height, index)`.
    pub fn position(&self) -> (u64, u32) {
        (self.height, self.index)
    }
}

/// A merged block (D-063): a block in a chain block's merge set other than the selected parent. Its ordinal in `merged`
/// is its acceptance ordinal (D-079).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MergedBlock {
    /// Block hash.
    pub hash: [u8; 32],
    /// Producer (`header.producer`): its sponsored fees belong to this account.
    pub producer: [u8; 32],
    /// All entries.
    pub entries: Vec<Entry>,
    /// The chain block merging it is at the settlement backlog limit (§6.4, D-046): all path-B entries are skipped.
    pub fast_path_paused: bool,
}

/// The BTC data needed during execution (D-035): provided by the node or indexer, it must be consistent with the chain
/// containing this block's `btc_anchor`, and the answers depend only on the part of the anchor chain at heights not
/// above the anchor height (the results do not change when syncing historical blocks). The exception is a block whose
/// anchor block has been reorged out of BTC (anchor-invalid mode, D-056): the view then answers from the current best
/// chain, which may not reproduce the first execution, and the node and indexer treat a result that differs, a verdict
/// of invalid included, as a local fault.
///
/// Any method returning `Err` means a data source failure: the execution layer returns [`BlockError::Fatal`] for it and
/// never judges the block invalid.
pub trait BtcView: Send + Sync + 'static {
    /// The 80-byte block header at height `height` on the anchor chain; `None` when unknown, `Err` means a data source
    /// failure.
    fn header(&self, height: u32) -> Result<Option<[u8; 80]>, String>;

    /// Whether, by the anchor chain, block `block_hash` has entered the settled closure (the settled prefix of the
    /// main-chain block accepting it is within the anchor height, D-038, D-063). Unlock entries are judged by this
    /// (D-074). `Err` means a data source failure.
    fn block_settled(&self, block_hash: &[u8; 32]) -> Result<bool, String>;

    /// The carrier of block `block_hash` (D-095): the payee account of the sender of the payload that first landed its
    /// data on BTC (on the anchor chain, at a height not above the anchor height) — for a settlement payload, the
    /// producer of the block it belongs to (the payload's last header), which must already be accepted by the main
    /// chain (up to this block's selected parent); for a republish payload, its `payee`, which must be non-zero, and
    /// the block's data must have waited more than `ANCHOR_DEPTH + CARRIER_REPOST_DELAY` BTC blocks after the anchor;
    /// otherwise there is no carrier (`None`, the reward goes to the producer). Only queried for blocks that have
    /// entered the settled closure. `Err` means a data source failure.
    fn carrier(&self, block_hash: &[u8; 32]) -> Result<Option<[u8; 32]>, String>;

    /// The output spent by the first input of native transaction `txid` (D-086: the signature of a path-C op is
    /// verified against it), when that transaction is confirmed in a block of the anchor chain at a height not above
    /// the anchor height and carries `op` (D-100). `Ok(None)` when the anchor chain has no such native transaction or
    /// it carries another op: the entry names no path-C op, which makes a block invalid, and a block merging it too.
    /// In anchor-invalid mode (D-056) the answer comes from the current best chain, and not finding the transaction
    /// there is a data source failure. `Err` means a data source failure.
    fn native_input(&self, txid: &[u8; 32], op: &VmOp) -> Result<Option<OutPoint>, String>;

    /// All outputs (amount, locking script) of native transaction `txid`, confirmed in a block of the anchor chain at a
    /// height not above the anchor height, in transaction order (the `native_tx` host function, D-078). Only queried
    /// for native transactions forcibly included in this block, so not finding it returns `Err` (data source failure).
    fn native_outputs(&self, txid: &[u8; 32]) -> Result<Vec<(u64, Vec<u8>)>, String>;
}

/// A view without BTC data: no block header exists, no block is settled, and querying a native transaction is a data
/// source failure.
pub struct NoBtc;

impl BtcView for NoBtc {
    fn header(&self, _height: u32) -> Result<Option<[u8; 80]>, String> {
        Ok(None)
    }

    fn block_settled(&self, _block_hash: &[u8; 32]) -> Result<bool, String> {
        Ok(false)
    }

    fn carrier(&self, _block_hash: &[u8; 32]) -> Result<Option<[u8; 32]>, String> {
        Ok(None)
    }

    fn native_input(&self, _txid: &[u8; 32], _op: &VmOp) -> Result<Option<OutPoint>, String> {
        Err("no BTC data, native transaction cannot be found".into())
    }

    fn native_outputs(&self, _txid: &[u8; 32]) -> Result<Vec<(u64, Vec<u8>)>, String> {
        Err("no BTC data, native transaction cannot be found".into())
    }
}

/// Execution result of an op.
#[derive(Clone, Debug, PartialEq, Eq, borsh::BorshSerialize, borsh::BorshDeserialize)]
pub enum OpStatus {
    /// Success.
    Success,
    /// Failed (does not affect block validity), with the reason.
    Failed(String),
    /// An entry of a merged block was skipped (lenient mode of D-063): no state changes, with the reason.
    Skipped(String),
}

/// Entry receipt (not part of the state root; the node stores it with Borsh).
#[derive(Clone, Debug, PartialEq, Eq, borsh::BorshSerialize, borsh::BorshDeserialize)]
pub struct Receipt {
    /// The `op_id` of a VmOp entry; `None` for an unlock entry.
    pub op_id: Option<[u8; 32]>,
    /// Execution result.
    pub status: OpStatus,
    /// Gas consumed (`max_gas` on failure, D-013).
    pub gas_used: u64,
    /// Contract return data (of a successful `Call` / the `init` of a `Deploy`).
    pub output: Vec<u8>,
    /// Events (empty on failure).
    pub events: Vec<Event>,
    /// The changes the op made to built-in assets, in the order it made them (empty on failure, D-122).
    pub asset_changes: Vec<AssetChange>,
}

/// Block execution result.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlockOutcome {
    /// All writes relative to the parent state; committing them yields the `state_root`.
    pub writes: WriteSet,
    /// Receipts, one per entry of this block.
    pub receipts: Vec<Receipt>,
    /// Receipts of each merged block (one list per [`MergedBlock`], one receipt per entry, D-063).
    pub merged_receipts: Vec<Vec<Receipt>>,
}

/// Why a block cannot be accepted.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum BlockError {
    /// A consensus rule was violated: the block is invalid.
    #[error("invalid block: {0}")]
    Invalid(String),
    /// Internal error (storage, VM engine): unrelated to the block contents; the block must not be judged invalid for
    /// it.
    #[error("internal execution error: {0}")]
    Fatal(String),
}

impl From<Fault> for BlockError {
    fn from(f: Fault) -> Self {
        match f {
            Fault::Invalid(m) => BlockError::Invalid(m),
            // An op-level failure at the block level means the caller used the wrong interface; treat it as an internal
            // error.
            Fault::Failed(m) | Fault::Fatal(m) => BlockError::Fatal(m),
        }
    }
}

/// Result of executing an action: success (return data, gas consumed) or the failure reason.
type ActionResult = Result<(Vec<u8>, u64), String>;

/// Block executor: shared by the node and indexer (SPEC §10).
pub struct Executor {
    vm: Vm,
    params: Params,
}

fn describe(reason: &FailureReason) -> String {
    match reason {
        FailureReason::OutOfGas => "out of gas".into(),
        FailureReason::Revert(data) => format!("revert: {}", String::from_utf8_lossy(data)),
        FailureReason::Trap(t) => format!("trap: {t}"),
        FailureReason::Rejected(m) => m.clone(),
    }
}

impl Executor {
    /// Creates an executor (with a new VM engine).
    pub fn new(params: Params) -> Result<Self, BlockError> {
        let vm = Vm::new(&params).map_err(|e| BlockError::Fatal(e.to_string()))?;
        Ok(Executor { vm, params })
    }

    /// Uses the given VM (e.g. an engine with parallel compilation turned off).
    pub fn with_vm(params: Params, vm: Vm) -> Self {
        Executor { vm, params }
    }

    /// Protocol parameters.
    pub fn params(&self) -> &Params {
        &self.params
    }

    /// Executes (validates) all entries of a block that merges no other blocks: if any entry makes the block invalid,
    /// `BlockError::Invalid` is returned. Equivalent to [`Executor::execute_chain_block`] with an empty merge set.
    pub fn execute_block(
        &self,
        base: Arc<dyn StateSource>,
        btc: Arc<dyn BtcView>,
        env: &BlockEnv,
        entries: &[Entry],
    ) -> Result<BlockOutcome, BlockError> {
        self.execute_chain_block(base, btc, env, &[], entries)
    }

    /// Executes (validates) a chain block (D-063): `base` is the selected parent's state; the entries of each block in
    /// `merged` are executed first in lenient mode, then this block's `entries` in strict mode: if any entry of this
    /// block makes the block invalid, `BlockError::Invalid` is returned. Shares the code path of block building by the
    /// producer ([`Executor::build_chain_block`]).
    pub fn execute_chain_block(
        &self,
        base: Arc<dyn StateSource>,
        btc: Arc<dyn BtcView>,
        env: &BlockEnv,
        merged: &[MergedBlock],
        entries: &[Entry],
    ) -> Result<BlockOutcome, BlockError> {
        self.build_chain_block(base, btc, env, merged, |builder| {
            for entry in entries {
                builder.push(entry)?;
            }
            Ok(())
        })
        .map(|(outcome, ())| outcome)
    }

    /// Builds a block that merges no other blocks entry by entry: equivalent to [`Executor::build_chain_block`] with an
    /// empty merge set.
    pub fn build_block<R, F>(
        &self,
        base: Arc<dyn StateSource>,
        btc: Arc<dyn BtcView>,
        env: &BlockEnv,
        build: F,
    ) -> Result<(BlockOutcome, R), BlockError>
    where
        F: FnOnce(&mut BlockBuilder<'_>) -> Result<R, BlockError> + Send,
        R: Send,
    {
        self.build_chain_block(base, btc, env, &[], build)
    }

    /// Builds a chain block entry by entry (used by the producer, SPEC §6.2, D-063): the entries of each block in
    /// `merged` are executed first in lenient mode, then `build` appends this block's entries via
    /// [`BlockBuilder::push`]: an entry that would make the block invalid is rejected (`BlockError::Invalid` is
    /// returned) and the state built so far is unchanged, so the caller may skip it and continue; `BlockError::Fatal`
    /// means an internal error and the build should be abandoned. The whole process runs on a dedicated thread with a
    /// stack of `EXECUTION_THREAD_STACK` (D-014).
    pub fn build_chain_block<R, F>(
        &self,
        base: Arc<dyn StateSource>,
        btc: Arc<dyn BtcView>,
        env: &BlockEnv,
        merged: &[MergedBlock],
        build: F,
    ) -> Result<(BlockOutcome, R), BlockError>
    where
        F: FnOnce(&mut BlockBuilder<'_>) -> Result<R, BlockError> + Send,
        R: Send,
    {
        on_execution_thread(move || {
            let mut builder = BlockBuilder::new(self, base, btc, env.clone())?;
            for block in merged {
                builder.merge(block)?;
            }
            let result = build(&mut builder)?;
            Ok((builder.finish()?, result))
        })
    }

    /// Read-only contract call (RPC `ksana_call`): executes one `Call` on top of `base` (funds may be attached,
    /// transferred out of `caller`'s balance) without charging a fee or changing the nonce, and all writes are
    /// discarded. The returned receipt has the same format as one from block execution (`op_id` is `None`).
    #[allow(clippy::too_many_arguments)]
    pub fn simulate_call(
        &self,
        base: Arc<dyn StateSource>,
        btc: Arc<dyn BtcView>,
        env: &BlockEnv,
        caller: [u8; 32],
        contract: [u8; 32],
        method: String,
        args: Vec<u8>,
        funds: Vec<Funds>,
        max_gas: u64,
    ) -> Result<Receipt, BlockError> {
        on_execution_thread(move || {
            if let Err(e) = check_funds(&funds) {
                return Ok(simulated_failure(max_gas, e.into()));
            }
            let intrinsic = gas::CALL_BASE.saturating_add(funds_gas(&funds));
            let Some(remaining) = max_gas.checked_sub(intrinsic) else {
                return Ok(simulated_failure(max_gas, "out of gas".into()));
            };
            let mut ov = Overlay::new(base);
            if let Err(e) = attach_funds(&mut ov, &caller, &contract, &funds)? {
                return Ok(simulated_failure(max_gas, e));
            }
            let req = CallRequest {
                contract,
                caller,
                method,
                args,
                funds,
                allow_init: false,
            };
            let (mut ov, result) = self.call_vm(ov, &btc, env, req, remaining, None)?;
            Ok(match result {
                Ok((output, used)) => {
                    let (events, asset_changes) = ov.commit_op();
                    Receipt {
                        op_id: None,
                        status: OpStatus::Success,
                        gas_used: used.saturating_add(intrinsic),
                        output,
                        events,
                        asset_changes,
                    }
                }
                Err(reason) => simulated_failure(max_gas, reason),
            })
        })
    }

    /// Stateless checks of a VmOp: version, chain ID, encoded length.
    fn check_op_static(&self, rules: &Rules, op: &VmOp) -> Result<[u8; 32], String> {
        op_static(&self.params, rules, op)
    }

    /// Preparation part of path B: signature, consecutive nonce and sufficient token balance, otherwise the block is
    /// invalid; then the fee is deducted (into the sponsored-fee record named by `fee`) and the nonce incremented. The
    /// writes stay in the current op's buffer: the caller commits them on success and discards them on error (SPEC
    /// §4.3, §5.2, §8.1).
    fn sponsored_prepare(
        &self,
        ov: &mut Overlay,
        rules: &Rules,
        fee: &FeeOwner,
        op: &VmOp,
    ) -> Result<[u8; 32], BlockError> {
        let op_id = self
            .check_op_static(rules, op)
            .map_err(BlockError::Invalid)?;
        let FeeMode::Token { amount } = op.fee else {
            return Err(BlockError::Invalid(
                "fee mode of a Sponsored entry must be Token".into(),
            ));
        };
        verify_vmop(op).map_err(|e| BlockError::Invalid(format!("invalid signature: {e}")))?;

        let mut account = builtin::account_state(ov, &op.account)?;
        if account.nonce != op.nonce {
            return Err(BlockError::Invalid(format!(
                "nonce not consecutive: account has {}, op has {}",
                account.nonce, op.nonce
            )));
        }
        let balance = builtin::balance(ov, &KSA_ASSET_ID, &op.account)?;
        let rest = balance.checked_sub(amount).ok_or_else(|| {
            BlockError::Invalid("KSA balance insufficient to pay the sponsored fee".into())
        })?;
        builtin::set_balance(ov, &KSA_ASSET_ID, &op.account, rest)?;
        add_locked_fee(ov, fee, amount)?;
        account.nonce = account
            .nonce
            .checked_add(1)
            .ok_or_else(|| BlockError::Invalid("nonce overflow".into()))?;
        builtin::set_account_state(ov, &op.account, &account)?;
        Ok(op_id)
    }

    /// Preparation part of path C (SPEC §4.3, §7.4, §8.1, revision of D-018): the entry must name a native transaction
    /// on the anchor chain that carries this op (D-100, given by the BTC view), otherwise it stands for no path-C op
    /// and is invalid (in a merged block too: the merging block is invalid). When version, chain ID, encoded length,
    /// fee mode `BtcNative`, signature or nonce is wrong, the op did not pass authentication and is recorded as failed
    /// without changing state; when all are right the nonce is consumed (as in path B: an authenticated op consumes its
    /// nonce once included), and afterwards `max_gas` above `NATIVE_MAX_GAS` or a failing action only fails the op,
    /// keeping the nonce increment. The signature is verified against the first input of the carrying transaction
    /// `btc_txid` (D-086): an op copied into another transaction does not pass authentication. The nonce write stays in
    /// the current op's buffer and is committed by the caller. `Err` is `Invalid` for an entry naming no path-C op and
    /// `Fatal` for internal errors.
    fn forced_prepare(
        &self,
        ov: &mut Overlay,
        rules: &Rules,
        btc: &dyn BtcView,
        btc_txid: &[u8; 32],
        op: &VmOp,
    ) -> Result<ForcedPrepared, BlockError> {
        let first_input = btc
            .native_input(btc_txid, op)
            .map_err(|e| BlockError::Fatal(format!("BTC data source failure: {e}")))?
            .ok_or_else(|| {
                BlockError::Invalid(
                    "no native transaction with this ID on the anchor chain carries this op (D-100)".into(),
                )
            })?;
        if let Err(reason) = check_native_op(&self.params, rules, op, &first_input) {
            return Ok(ForcedPrepared::Rejected(reason));
        }
        let mut account = builtin::account_state(ov, &op.account)?;
        if account.nonce != op.nonce {
            return Ok(ForcedPrepared::Rejected("nonce mismatch".into()));
        }
        let Some(next_nonce) = account.nonce.checked_add(1) else {
            return Ok(ForcedPrepared::Rejected("nonce overflow".into()));
        };
        account.nonce = next_nonce;
        builtin::set_account_state(ov, &op.account, &account)?;
        if op.max_gas > rules.native_max_gas {
            return Ok(ForcedPrepared::Consumed(
                "max_gas exceeds NATIVE_MAX_GAS".into(),
            ));
        }
        Ok(ForcedPrepared::Ready)
    }

    /// Unlock entry (SPEC §8.5; D-038, D-074, D-079): cumulatively unlocks in acceptance order up to and including
    /// `target`. `target` must be in [`BlockEnv::unlockable`], after the unlock cursor, and the block at that position
    /// must have entered the settled closure, otherwise the block is invalid. Then for every position in the segment
    /// after the cursor up to `target`: a rewarded block is paid its block reward by its DAA score (KSA total supply
    /// capped at `KSA_MAX_SUPPLY`), then the sponsored-fee record at that position is released, both credited to its
    /// producer's available balance; finally the cursor is set to `target`. The writes stay in the current op's buffer.
    ///
    /// All checks that depend only on the state and the segment are done before querying the BTC view; a view failure
    /// is a local fault (`Fatal`).
    fn unlock(
        &self,
        ov: &mut Overlay,
        btc: &Arc<dyn BtcView>,
        env: &BlockEnv,
        target: (u64, u32),
    ) -> Result<(), BlockError> {
        let segment = &env.unlockable;
        // The segment is given by the node or indexer: at most MAX_UNLOCK_BATCH positions, strictly increasing in
        // acceptance order, otherwise it is their fault
        if u64::try_from(segment.len())
            .ok()
            .is_none_or(|n| n > self.params.max_unlock_batch)
        {
            return Err(BlockError::Fatal(
                "unlockable segment has more than MAX_UNLOCK_BATCH positions".into(),
            ));
        }
        if segment
            .windows(2)
            .any(|w| matches!(w, [a, b] if a.position() >= b.position()))
        {
            return Err(BlockError::Fatal(
                "positions of the unlockable segment are not strictly increasing in acceptance order".into(),
            ));
        }
        // The weight can only be 1 or the empty-block weight of some streak (D-084, D-090), otherwise it is the node's
        // / indexer's fault
        if segment
            .iter()
            .any(|item| !self.params.is_block_weight(item.weight))
        {
            return Err(BlockError::Fatal(
                "weight of a block in the unlockable segment is neither 1 nor an empty-block weight (D-084, D-090)".into(),
            ));
        }
        let cursor_key = keys::unlock_cursor();
        let cursor = builtin::read::<UnlockCursor>(ov, &cursor_key)?.map(|c| (c.height, c.index));
        if cursor.is_some_and(|c| target <= c) {
            return Err(BlockError::Invalid(
                "unlock target position was already unlocked".into(),
            ));
        }
        let after_cursor = |item: &&UnlockItem| cursor.is_none_or(|c| item.position() > c);
        let released: Vec<&UnlockItem> = segment
            .iter()
            .filter(after_cursor)
            .take_while(|item| item.position() <= target)
            .collect();
        let last = released
            .last()
            .filter(|item| item.position() == target)
            .ok_or_else(|| {
                BlockError::Invalid(format!(
                    "unlock target position is not in the unlockable segment (it does not exist, or it is more than \
                     MAX_UNLOCK_BATCH ({}) positions after the unlock cursor; unlock earlier positions first)",
                    self.params.max_unlock_batch
                ))
            })?;
        // The settled closure requires every block's data to have been carried by a transaction confirmed on BTC
        // (D-077). The settled prefix is monotonic in acceptance order: when the target has entered, so have all
        // positions before it
        let settled = btc
            .block_settled(&last.hash)
            .map_err(|e| BlockError::Fatal(format!("BTC data source failure: {e}")))?;
        if !settled {
            return Err(BlockError::Invalid(
                "the block at the unlock target position is not yet in the settled closure".into(),
            ));
        }
        // Block share account (D-089): written into the state by the genesis config; without it the whole block reward
        // goes to the producer
        let share_account = builtin::read::<[u8; 32]>(ov, &keys::block_share_account())?;
        for item in released {
            let record_key = keys::locked_fee(item.height, item.index);
            let record = builtin::read::<LockedFee>(ov, &record_key)?;
            if record.as_ref().is_some_and(|r| r.producer != item.producer) {
                return Err(BlockError::Fatal(
                    "producer of the sponsored-fee record does not match the unlockable segment"
                        .into(),
                ));
            }
            if !item.rewarded && record.is_none() {
                continue;
            }
            // Carrier (D-095): the sender of the payload that first landed the block's data on BTC; when there is none,
            // or it is the producer itself, both parts go to the producer
            let carrier = btc
                .carrier(&item.hash)
                .map_err(|e| BlockError::Fatal(format!("BTC data source failure: {e}")))?
                .filter(|c| *c != item.producer);
            // A block of weight N gets the sum of N reward slots (D-084), each slot split by the rules into three parts
            // for the producer, the block share account and the carrier (D-089, D-095); when the supply cap applies,
            // the share part is guaranteed first, then the carrier's (this can only trigger at the tail, when the share
            // has long ended)
            let (mut to_producer, mut to_carrier, share) = if item.rewarded {
                let split = self
                    .params
                    .block_rewards_split_at(item.daa_score, item.weight);
                let share_part = if share_account.is_some() {
                    split.share
                } else {
                    0
                };
                let carrier_part = if carrier.is_some() { split.carrier } else { 0 };
                let issued = issue_reward(ov, split.total(), self.params.ksa_max_supply)?;
                let share = share_part.min(issued);
                let carrier_amount = carrier_part.min(issued.saturating_sub(share));
                let producer_amount = issued.saturating_sub(share).saturating_sub(carrier_amount);
                (producer_amount, carrier_amount, share)
            } else {
                (0, 0, 0)
            };
            // The sponsored fee (D-079) is split between the carrier and the producer by the unlocked block's own rules
            // (D-095)
            if let Some(record) = record {
                let carrier_fee = if carrier.is_some() {
                    self.params.carrier_fee_at(item.daa_score, record.amount)
                } else {
                    0
                };
                to_carrier = to_carrier
                    .checked_add(carrier_fee)
                    .ok_or_else(|| BlockError::Invalid("unlock amount overflow".into()))?;
                to_producer = to_producer
                    .checked_add(record.amount.saturating_sub(carrier_fee))
                    .ok_or_else(|| BlockError::Invalid("unlock amount overflow".into()))?;
                ov.delete(record_key);
            }
            let credit = |ov: &mut Overlay,
                          account: &[u8; 32],
                          amount: u128,
                          who: &str|
             -> Result<(), BlockError> {
                if amount == 0 {
                    return Ok(());
                }
                let balance = builtin::balance(ov, &KSA_ASSET_ID, account)?
                    .checked_add(amount)
                    .ok_or_else(|| {
                        BlockError::Invalid(format!("{who} balance overflow after unlock"))
                    })?;
                builtin::set_balance(ov, &KSA_ASSET_ID, account, balance)?;
                Ok(())
            };
            credit(ov, &item.producer, to_producer, "producer")?;
            if let Some(account) = carrier {
                credit(ov, &account, to_carrier, "carrier")?;
            }
            if let Some(account) = share_account {
                credit(ov, &account, share, "block share account")?;
            }
        }
        builtin::write(
            ov,
            cursor_key,
            &UnlockCursor {
                height: target.0,
                index: target.1,
            },
        )?;
        Ok(())
    }

    /// Executes the action and produces the receipt: on success the current op's buffer is committed, on failure it is
    /// discarded (together with any uncommitted preparation writes). `Err` is only for internal errors.
    fn finish_action(
        &self,
        ov: Overlay,
        btc: &Arc<dyn BtcView>,
        env: &BlockEnv,
        op: &VmOp,
        op_id: [u8; 32],
        native: Option<[u8; 32]>,
    ) -> Result<(Overlay, Receipt), BlockError> {
        let (mut ov, result) = self.run_action(ov, btc, env, op, native)?;
        let receipt = match result {
            Ok((output, gas_used)) => {
                let (events, asset_changes) = ov.commit_op();
                Receipt {
                    op_id: Some(op_id),
                    status: OpStatus::Success,
                    gas_used,
                    output,
                    events,
                    asset_changes,
                }
            }
            Err(reason) => {
                ov.discard_op();
                failed_receipt(op_id, op.max_gas, reason)
            }
        };
        Ok((ov, receipt))
    }

    /// Executes the action; the writes stay in the current op's buffer for the caller to commit or discard. `native` is
    /// the ID of the BTC transaction carrying a path-C op (the `native_tx` host function, D-078), `None` for other
    /// paths.
    fn run_action(
        &self,
        mut ov: Overlay,
        btc: &Arc<dyn BtcView>,
        env: &BlockEnv,
        op: &VmOp,
        native: Option<[u8; 32]>,
    ) -> Result<(Overlay, ActionResult), BlockError> {
        match &op.action {
            Action::AssetTransfer { asset, to, amount } => {
                if op.max_gas < gas::ASSET_TRANSFER_ACTION {
                    return Ok((ov, Err("out of gas".into())));
                }
                let result = match builtin::transfer(&mut ov, asset, &op.account, to, *amount) {
                    Ok(()) => {
                        ov.record(
                            *asset,
                            ChangeKind::Transfer {
                                from: op.account,
                                to: *to,
                                amount: *amount,
                            },
                        );
                        Ok((Vec::new(), gas::ASSET_TRANSFER_ACTION))
                    }
                    Err(Fault::Failed(m)) => Err(m),
                    Err(other) => return Err(other.into()),
                };
                Ok((ov, result))
            }
            Action::Deploy {
                code,
                init_args,
                funds,
            } => {
                if let Err(e) = check_funds(funds) {
                    return Ok((ov, Err(e.into())));
                }
                let intrinsic = gas::cost(gas::DEPLOY_BASE, gas::DEPLOY_PER_CODE_BYTE, code.len())
                    .saturating_add(funds_gas(funds));
                let Some(remaining) = op.max_gas.checked_sub(intrinsic) else {
                    return Ok((ov, Err("out of gas".into())));
                };
                let code_hash = match self.vm.validate_code(code) {
                    Ok(h) => h,
                    Err(e) => return Ok((ov, Err(format!("deployment rejected: {e}")))),
                };
                let id = builtin::contract_id(&op.account, op.nonce, &code_hash);
                if builtin::read::<ContractMeta>(&ov, &keys::contract(&id))?.is_some() {
                    return Ok((ov, Err("contract already exists".into())));
                }
                let code_key = keys::code(&code_hash);
                if ov.get(&code_key).map_err(BlockError::Fatal)?.is_none() {
                    ov.set(code_key, code.clone());
                }
                builtin::write(
                    &mut ov,
                    keys::contract(&id),
                    &ContractMeta {
                        code_hash,
                        deployer: op.account,
                    },
                )?;
                if let Err(e) = attach_funds(&mut ov, &op.account, &id, funds)? {
                    return Ok((ov, Err(e)));
                }
                let req = CallRequest {
                    contract: id,
                    caller: op.account,
                    method: INIT_METHOD.into(),
                    args: init_args.clone(),
                    funds: funds.clone(),
                    allow_init: true,
                };
                let (ov, result) = self.call_vm(ov, btc, env, req, remaining, native)?;
                let result = result.map(|(output, used)| (output, used.saturating_add(intrinsic)));
                Ok((ov, result))
            }
            Action::Call {
                contract,
                method,
                args,
                funds,
            } => {
                if let Err(e) = check_funds(funds) {
                    return Ok((ov, Err(e.into())));
                }
                let intrinsic = gas::CALL_BASE.saturating_add(funds_gas(funds));
                let Some(remaining) = op.max_gas.checked_sub(intrinsic) else {
                    return Ok((ov, Err("out of gas".into())));
                };
                if let Err(e) = attach_funds(&mut ov, &op.account, contract, funds)? {
                    return Ok((ov, Err(e)));
                }
                let req = CallRequest {
                    contract: *contract,
                    caller: op.account,
                    method: method.clone(),
                    args: args.clone(),
                    funds: funds.clone(),
                    allow_init: false,
                };
                let (ov, result) = self.call_vm(ov, btc, env, req, remaining, native)?;
                let result = result.map(|(output, used)| (output, used.saturating_add(intrinsic)));
                Ok((ov, result))
            }
        }
    }

    fn call_vm(
        &self,
        ov: Overlay,
        btc: &Arc<dyn BtcView>,
        env: &BlockEnv,
        req: CallRequest,
        gas_limit: u64,
        native: Option<[u8; 32]>,
    ) -> Result<(Overlay, ActionResult), BlockError> {
        let host = OpHost {
            ov,
            height: env.height,
            timestamp: env.timestamp,
            anchor: env.btc_anchor.height,
            btc: Arc::clone(btc),
            native,
            native_cache: None,
        };
        let (host, outcome) = self.vm.call(host, req, gas_limit);
        let outcome = outcome.map_err(|e| BlockError::Fatal(e.to_string()))?;
        let result = match outcome {
            CallOutcome::Success { output, gas_used } => Ok((output, gas_used)),
            CallOutcome::Failure(reason) => Err(describe(&reason)),
        };
        Ok((host.ov, result))
    }
}

/// Gas of attached funds: [`gas::FUNDS_PER_ASSET`] per item (D-073).
fn funds_gas(funds: &[Funds]) -> u64 {
    gas::FUNDS_PER_ASSET.saturating_mul(u64::try_from(funds.len()).unwrap_or(u64::MAX))
}

/// Transfers the attached funds from the signing account to the contract (D-073), each recorded for the receipt
/// (D-122). Deterministic failures such as an insufficient balance return `Ok(Err(reason))` (the op fails and is rolled
/// back as a whole). A refusal by the asset's rules keeps its own `rejected:` message, as on every other path that
/// moves the asset (D-121).
fn attach_funds(
    ov: &mut Overlay,
    from: &[u8; 32],
    to: &[u8; 32],
    funds: &[Funds],
) -> Result<Result<(), String>, BlockError> {
    for f in funds {
        match builtin::transfer(ov, &f.asset, from, to, f.amount) {
            Ok(()) => ov.record(
                f.asset,
                ChangeKind::Funds {
                    from: *from,
                    to: *to,
                    amount: f.amount,
                },
            ),
            Err(Fault::Failed(m)) if m.starts_with("rejected:") => return Ok(Err(m)),
            Err(Fault::Failed(m)) => {
                return Ok(Err(format!("attached funds transfer failed: {m}")))
            }
            Err(other) => return Err(other.into()),
        }
    }
    Ok(Ok(()))
}

/// Stateless checks of a VmOp: version (the range accepted by the rules, D-076), chain ID, encoded length; returns the
/// `op_id`.
fn op_static(params: &Params, rules: &Rules, op: &VmOp) -> Result<[u8; 32], String> {
    if !rules.accepts_vmop_version(op.version) {
        return Err(format!("unsupported VmOp version {}", op.version));
    }
    if op.chain_id != params.chain_id {
        return Err(format!(
            "chain_id {} does not match this network",
            op.chain_id
        ));
    }
    let encoded = op.encode().map_err(|e| e.to_string())?;
    if encoded.len() > params.max_op_bytes {
        return Err("VmOp encoding exceeds MAX_OP_BYTES".into());
    }
    Ok(ksana_primitives::hash::sha256(&encoded))
}

/// The authentication of a path C op that needs neither state nor the BTC view (§7.4, §8.1): version (the range
/// accepted by the rules, D-076), chain ID, encoded length, fee mode `BtcNative`, and the signature over `first_input`,
/// the output spent by the first input of the carrying transaction (D-086). A forced entry whose op fails it is
/// recorded as failed without taking its nonce; the node also counts the nonce of a path C op waiting for forced
/// inclusion only when the op passes it (D-104). `Err` is the reason the op fails.
pub fn check_native_op(
    params: &Params,
    rules: &Rules,
    op: &VmOp,
    first_input: &OutPoint,
) -> Result<(), String> {
    op_static(params, rules, op)?;
    if op.fee != FeeMode::BtcNative {
        return Err("fee mode of a native op must be BtcNative".into());
    }
    verify_native_vmop(op, first_input).map_err(|e| format!("invalid signature: {e}"))
}

/// The rules on block entries that depend on neither state nor BTC data (D-063: block validity, merged blocks must
/// satisfy them too): entry order (forced inclusion → unlock → path B, §6.2), total gas (same as
/// [`BlockBuilder::push`], §8.3, D-036), and the version, chain ID, encoded length, fee mode and signature of path-B
/// VmOps. For forced-inclusion entries these problems only fail the op (§7.4) and are not checked here. `rules` are the
/// block's own rules (by its DAA score, D-076); merged blocks are also checked under their own rules.
pub fn check_entries_static(
    params: &Params,
    rules: &Rules,
    entries: &[Entry],
) -> Result<(), String> {
    let mut phase = 0;
    let mut gas: u64 = 0;
    let mut unlocked = false;
    for (index, entry) in entries.iter().enumerate() {
        let entry_phase = entry_phase(entry);
        if entry_phase < phase {
            return Err(format!(
                "entry {index}: wrong order, entries must be forced inclusion, then unlock, then path B"
            ));
        }
        phase = entry_phase;
        if matches!(entry, Entry::Unlock { .. }) {
            if unlocked {
                return Err(format!(
                    "entry {index}: at most one unlock entry per block (D-074)"
                ));
            }
            unlocked = true;
        }
        let counted = entry_gas(entry, rules);
        gas = match entry {
            Entry::Forced { .. } => gas.saturating_add(counted),
            Entry::Unlock { .. } => gas,
            Entry::Sponsored { .. } => gas
                .checked_add(counted)
                .filter(|g| *g <= rules.block_gas_limit)
                .ok_or_else(|| format!("entry {index}: block gas total exceeds BLOCK_GAS_LIMIT"))?,
        };
        let Entry::Sponsored { op } = entry else {
            continue;
        };
        op_static(params, rules, op).map_err(|e| format!("entry {index}: {e}"))?;
        if !matches!(op.fee, FeeMode::Token { .. }) {
            return Err(format!(
                "entry {index}: fee mode does not match the entry type"
            ));
        }
        verify_vmop(op).map_err(|e| format!("entry {index}: invalid signature: {e}"))?;
    }
    Ok(())
}

/// Runs on a dedicated thread with a stack of `EXECUTION_THREAD_STACK` (D-014).
fn on_execution_thread<T, F>(f: F) -> Result<T, BlockError>
where
    F: FnOnce() -> Result<T, BlockError> + Send,
    T: Send,
{
    std::thread::scope(|scope| {
        let handle = std::thread::Builder::new()
            .name("ksana-execution".into())
            .stack_size(EXECUTION_THREAD_STACK)
            .spawn_scoped(scope, f)
            .map_err(|e| BlockError::Fatal(format!("cannot spawn execution thread: {e}")))?;
        handle
            .join()
            .map_err(|_| BlockError::Fatal("execution thread exited abnormally".into()))?
    })
}

/// A block being built entry by entry (see [`Executor::build_block`]).
pub struct BlockBuilder<'a> {
    exec: &'a Executor,
    btc: Arc<dyn BtcView>,
    env: BlockEnv,
    /// `None` once an internal error occurred midway through executing an entry; every later call returns an internal
    /// error.
    ov: Option<Overlay>,
    receipts: Vec<Receipt>,
    merged_receipts: Vec<Vec<Receipt>>,
    phase: u8,
    gas: u64,
    /// This block already includes an unlock entry (at most one per block, D-074).
    unlocked: bool,
}

impl<'a> BlockBuilder<'a> {
    fn new(
        exec: &'a Executor,
        base: Arc<dyn StateSource>,
        btc: Arc<dyn BtcView>,
        env: BlockEnv,
    ) -> Result<Self, BlockError> {
        // Block rewards are paid at unlock time and sponsored fees are recorded by acceptance position when received
        // (D-079): nothing is written to the state when execution starts
        let ov = Overlay::new(base);
        Ok(BlockBuilder {
            exec,
            btc,
            env,
            ov: Some(ov),
            receipts: Vec::new(),
            merged_receipts: Vec::new(),
            phase: 0,
            gas: 0,
            unlocked: false,
        })
    }

    /// Executes all entries of one merged block in lenient mode (D-063, see the module docs). May only be called before
    /// any entry of this block is appended. `Err` is only for internal errors.
    fn merge(&mut self, block: &MergedBlock) -> Result<(), BlockError> {
        if !self.receipts.is_empty() {
            return Err(BlockError::Fatal(
                "merged blocks must be executed before this block's entries".into(),
            ));
        }
        // The merged block's sponsored fees are recorded at its own acceptance position (D-079): the ordinal is its
        // position in the merge set
        let fee = FeeOwner {
            height: self.env.height,
            index: self.accepted_index()?,
            producer: block.producer,
        };
        let mut receipts = Vec::with_capacity(block.entries.len());
        for entry in &block.entries {
            if block.fast_path_paused && entry_phase(entry) == 2 {
                receipts.push(skipped_receipt(
                    entry,
                    "settlement backlog limit: path B paused (D-046)".into(),
                ));
                continue;
            }
            // Unlocking is left to the main-chain blocks' own entries (D-100): a merged block's unlock entry changes no
            // state, so this block's state never depends on how a merged unlock would be judged
            if matches!(entry, Entry::Unlock { .. }) {
                receipts.push(skipped_receipt(
                    entry,
                    "an unlock entry of a merged block is not executed (D-100)".into(),
                ));
                continue;
            }
            let ov = self.ov.take().ok_or_else(|| {
                BlockError::Fatal("block building was aborted by an internal error".into())
            })?;
            let receipt = match self.run_entry(ov, entry, &fee) {
                Ok((ov, receipt)) => {
                    self.ov = Some(ov);
                    receipt
                }
                // A forced-inclusion entry naming no path-C op at or below this block's anchor makes this block invalid
                // (D-100); any other entry that would make the block invalid in strict mode is skipped without changing
                // state
                Err((ov, BlockError::Invalid(reason))) if matches!(entry, Entry::Forced { .. }) => {
                    self.restore(ov);
                    return Err(BlockError::Invalid(format!(
                        "a forced-inclusion entry of a merged block names no path-C op at or below this block's \
                         anchor: {reason}"
                    )));
                }
                Err((Some(mut ov), BlockError::Invalid(reason))) => {
                    ov.discard_op();
                    self.ov = Some(ov);
                    skipped_receipt(entry, reason)
                }
                Err((ov, error)) => {
                    self.restore(ov);
                    return Err(error);
                }
            };
            receipts.push(receipt);
        }
        self.merged_receipts.push(receipts);
        Ok(())
    }

    /// Acceptance ordinal of the next accepted block (D-079): the number of blocks merged so far. This block's own
    /// entries run after all merged blocks, so its ordinal is the number of merged blocks.
    fn accepted_index(&self) -> Result<u32, BlockError> {
        u32::try_from(self.merged_receipts.len())
            .map_err(|_| BlockError::Fatal("too many merged blocks".into()))
    }

    /// Total gas counted for the entries included so far (SPEC §8.3, D-036): forced-inclusion entries as `min(max_gas,
    /// NATIVE_MAX_GAS)`, path-B entries as `max_gas`, unlock entries as 0. Forced-inclusion entries are not bound by
    /// `BLOCK_GAS_LIMIT` and may push it over the limit. Entries of merged blocks are not counted (D-063).
    pub fn gas(&self) -> u64 {
        self.gas
    }

    /// Receipts of the entries included in this block so far.
    pub fn receipts(&self) -> &[Receipt] {
        &self.receipts
    }

    /// Appends one entry and executes it. When the entry would make the block invalid, `BlockError::Invalid` is
    /// returned, the entry is not included and the state built so far is unchanged; `BlockError::Fatal` means an
    /// internal error (including a BTC view failure) and the caller should abandon the build. Check order: entry order
    /// (forced inclusion → unlock → path B, SPEC §6.2), total gas (forced-inclusion entries are not bound by the limit,
    /// unlock entries count as 0, and adding a path-B entry must not exceed `BLOCK_GAS_LIMIT`, §8.3, D-036), then
    /// execution by entry type (rules in the module docs).
    pub fn push(&mut self, entry: &Entry) -> Result<&Receipt, BlockError> {
        let phase = entry_phase(entry);
        if phase < self.phase {
            return Err(BlockError::Invalid(
                "wrong entry order: entries must be forced inclusion, then unlock, then path B"
                    .into(),
            ));
        }
        let is_unlock = matches!(entry, Entry::Unlock { .. });
        if is_unlock && self.unlocked {
            return Err(BlockError::Invalid(
                "at most one unlock entry per block (D-074)".into(),
            ));
        }
        let counted = entry_gas(entry, &self.env.rules);
        let gas = match entry {
            // Forced-inclusion entries are not bound by BLOCK_GAS_LIMIT (D-036); the total saturates and never
            // overflows
            Entry::Forced { .. } => self.gas.saturating_add(counted),
            // An unlock entry counts 0 gas, does not increase the total and is therefore not bound by the limit (D-036)
            Entry::Unlock { .. } => self.gas,
            Entry::Sponsored { .. } => self
                .gas
                .checked_add(counted)
                .filter(|g| *g <= self.env.rules.block_gas_limit)
                .ok_or_else(|| {
                    BlockError::Invalid("block gas total exceeds BLOCK_GAS_LIMIT".into())
                })?,
        };
        // This block's sponsored fees are recorded at its own acceptance position: after the blocks it merges (D-079)
        let fee = FeeOwner {
            height: self.env.height,
            index: self.accepted_index()?,
            producer: self.env.producer,
        };
        let ov = self.ov.take().ok_or_else(|| {
            BlockError::Fatal("block building was aborted by an internal error".into())
        })?;
        match self.run_entry(ov, entry, &fee) {
            Ok((ov, receipt)) => {
                self.ov = Some(ov);
                self.phase = phase;
                self.gas = gas;
                self.unlocked |= is_unlock;
                self.receipts.push(receipt);
                self.receipts
                    .last()
                    .ok_or_else(|| BlockError::Fatal("receipt missing".into()))
            }
            Err((ov, error)) => {
                self.restore(ov);
                Err(error)
            }
        }
    }

    /// Executes by entry type (rules in the module docs). `Ok` means included (the writes are committed to the block
    /// buffer); `Err((state, error))` means not included: the entry's writes in the state are not yet committed (the
    /// state is `None` when it was lost to an internal error midway through executing the action).
    fn run_entry(
        &self,
        mut ov: Overlay,
        entry: &Entry,
        fee: &FeeOwner,
    ) -> Result<(Overlay, Receipt), (Option<Overlay>, BlockError)> {
        let exec = self.exec;
        let executed = match entry {
            Entry::Forced { btc_txid, op } => {
                let op_id = match op.op_id() {
                    Ok(id) => id,
                    Err(e) => return Err((Some(ov), BlockError::Fatal(e.to_string()))),
                };
                match exec.forced_prepare(&mut ov, &self.env.rules, &*self.btc, btc_txid, op) {
                    Ok(ForcedPrepared::Ready) => {
                        // The nonce is committed first and kept even if the action fails (revision of D-018)
                        ov.commit_op();
                        exec.finish_action(ov, &self.btc, &self.env, op, op_id, Some(*btc_txid))
                    }
                    Ok(ForcedPrepared::Consumed(reason)) => {
                        ov.commit_op();
                        Ok((ov, failed_receipt(op_id, op.max_gas, reason)))
                    }
                    Ok(ForcedPrepared::Rejected(reason)) => {
                        ov.discard_op();
                        Ok((ov, failed_receipt(op_id, op.max_gas, reason)))
                    }
                    Err(e) => return Err((Some(ov), e)),
                }
            }
            Entry::Sponsored { op } => {
                match exec.sponsored_prepare(&mut ov, &self.env.rules, fee, op) {
                    Ok(op_id) => {
                        // The preparation writes (fee deduction, nonce) are committed first and kept even if the action
                        // fails
                        ov.commit_op();
                        exec.finish_action(ov, &self.btc, &self.env, op, op_id, None)
                    }
                    Err(e) => return Err((Some(ov), e)),
                }
            }
            Entry::Unlock { height, index } => {
                match exec.unlock(&mut ov, &self.btc, &self.env, (*height, *index)) {
                    Ok(()) => Ok(bookkeeping_receipt(ov)),
                    Err(e) => return Err((Some(ov), e)),
                }
            }
        };
        executed.map_err(|e| (None, e))
    }

    /// The entry is not included: discard its uncommitted writes and restore the state built so far (once the state is
    /// lost, every later call returns an internal error).
    fn restore(&mut self, ov: Option<Overlay>) {
        self.ov = ov.map(|mut ov| {
            ov.discard_op();
            ov
        });
    }

    fn finish(self) -> Result<BlockOutcome, BlockError> {
        let ov = self.ov.ok_or_else(|| {
            BlockError::Fatal("block building was aborted by an internal error".into())
        })?;
        Ok(BlockOutcome {
            writes: ov.into_block_writes(),
            receipts: self.receipts,
            merged_receipts: self.merged_receipts,
        })
    }
}

/// The phase an entry belongs to: forced inclusion 0, unlock 1, path B 2 (SPEC §6.2).
fn entry_phase(entry: &Entry) -> u8 {
    match entry {
        Entry::Forced { .. } => 0,
        Entry::Unlock { .. } => 1,
        Entry::Sponsored { .. } => 2,
    }
}

/// The amount an entry counts towards the block gas (SPEC §8.3, D-036): `min(max_gas, NATIVE_MAX_GAS)` for a
/// forced-inclusion entry (an op above it is not executed), `max_gas` for a path-B entry, 0 for an unlock entry.
fn entry_gas(entry: &Entry, rules: &Rules) -> u64 {
    match entry {
        Entry::Forced { op, .. } => op.max_gas.min(rules.native_max_gas),
        Entry::Sponsored { op } => op.max_gas,
        Entry::Unlock { .. } => 0,
    }
}

/// An unlock entry executed successfully: commit the writes. The receipt has `op_id` `None`, `gas_used` 0 and no return
/// data (D-038).
fn bookkeeping_receipt(mut ov: Overlay) -> (Overlay, Receipt) {
    let (events, asset_changes) = ov.commit_op();
    let receipt = Receipt {
        op_id: None,
        status: OpStatus::Success,
        gas_used: 0,
        output: Vec::new(),
        events,
        asset_changes,
    };
    (ov, receipt)
}

fn simulated_failure(max_gas: u64, reason: String) -> Receipt {
    Receipt {
        op_id: None,
        status: OpStatus::Failed(reason),
        gas_used: max_gas,
        output: Vec::new(),
        events: Vec::new(),
        asset_changes: Vec::new(),
    }
}

/// A skipped entry of a merged block (D-063): no state change, `gas_used` 0. A VmOp entry carries its `op_id` (`None`
/// when computing it failed).
fn skipped_receipt(entry: &Entry, reason: String) -> Receipt {
    let op_id = match entry {
        Entry::Forced { op, .. } | Entry::Sponsored { op } => op.op_id().ok(),
        Entry::Unlock { .. } => None,
    };
    Receipt {
        op_id,
        status: OpStatus::Skipped(reason),
        gas_used: 0,
        output: Vec::new(),
        events: Vec::new(),
        asset_changes: Vec::new(),
    }
}

/// Result of the path-C preparation checks (revision of D-018).
enum ForcedPrepared {
    /// Did not pass authentication (version, chain ID, length, fee mode, signature) or the nonce mismatched: failed op,
    /// no state change.
    Rejected(String),
    /// The nonce was consumed but the op cannot execute (`max_gas` exceeds `NATIVE_MAX_GAS`): failed op, only the nonce
    /// increment is kept.
    Consumed(String),
    /// The nonce was consumed; execute the action.
    Ready,
}

fn failed_receipt(op_id: [u8; 32], max_gas: u64, reason: String) -> Receipt {
    Receipt {
        op_id: Some(op_id),
        status: OpStatus::Failed(reason),
        gas_used: max_gas,
        output: Vec::new(),
        events: Vec::new(),
        asset_changes: Vec::new(),
    }
}

/// Issues a block reward (D-074, D-079, called at unlock time): the KSA total supply grows by `reward` but not beyond
/// `max_supply` (`KSA_MAX_SUPPLY`; the excess is not issued). Returns the amount actually issued, which the caller
/// credits to the producer's balance.
fn issue_reward(ov: &mut Overlay, reward: u128, max_supply: u128) -> Result<u128, BlockError> {
    if reward == 0 {
        return Ok(0);
    }
    let key = keys::asset(&KSA_ASSET_ID);
    let meta = builtin::read::<keys::AssetMeta>(ov, &key)?
        .ok_or_else(|| BlockError::Fatal("asset metadata of KSA is missing".into()))?;
    // The total supply is capped at KSA_MAX_SUPPLY (D-074): parallel blue blocks may have the same DAA score, so paying
    // per block can slightly exceed the bound obtained by summing over scores
    let reward = reward.min(max_supply.saturating_sub(meta.total_supply));
    if reward == 0 {
        return Ok(0);
    }
    let total_supply = meta
        .total_supply
        .checked_add(reward)
        .ok_or_else(|| BlockError::Invalid("KSA total supply overflow".into()))?;
    builtin::write(
        ov,
        key,
        &keys::AssetMeta {
            total_supply,
            ..meta
        },
    )?;
    Ok(reward)
}

/// Which record a path-B sponsored fee goes to (D-063, D-079): the acceptance position and producer of the block
/// collecting the fee.
#[derive(Clone, Copy, Debug)]
struct FeeOwner {
    /// Height of the main-chain block accepting it (i.e. the height of the main-chain block being executed).
    height: u64,
    /// Acceptance ordinal: for a merged block its ordinal in the merge set, for the main-chain block itself the number
    /// of merged blocks.
    index: u32,
    /// Producer.
    producer: [u8; 32],
}

/// Accumulates a sponsored fee into the `0x08 ‖ height ‖ index` record (D-079).
fn add_locked_fee(ov: &mut Overlay, fee: &FeeOwner, amount: u128) -> Result<(), BlockError> {
    let key = keys::locked_fee(fee.height, fee.index);
    let record = match builtin::read::<LockedFee>(ov, &key)? {
        None => LockedFee {
            producer: fee.producer,
            amount,
        },
        Some(r) => {
            if r.producer != fee.producer {
                return Err(BlockError::Fatal(
                    "producer of the sponsored-fee record does not match the block".into(),
                ));
            }
            LockedFee {
                amount: r
                    .amount
                    .checked_add(amount)
                    .ok_or_else(|| BlockError::Invalid("sponsored fee total overflow".into()))?,
                ..r
            }
        }
    };
    builtin::write(ov, key, &record)?;
    Ok(())
}
