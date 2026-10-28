//! The interface between the VM and the execution layer.
//!
//! The execution layer implements [`Host`]: contract storage, the built-in asset table, contract code, BTC block
//! headers and events. The VM is responsible for gas, length limits, the call stack, the `btc_header` depth rule and
//! asset ID derivation; `Host` only reads and writes state.

use borsh::{BorshDeserialize, BorshSerialize};
use ksana_primitives::Funds;

/// Error of a host operation.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum HostError {
    /// A deterministic operation failure (e.g. insufficient balance): this op fails and is rolled back, the block is
    /// processed as usual.
    #[error("operation failed: {0}")]
    Failed(String),
    /// An internal error unrelated to the op (e.g. a storage read/write failure): processing of the whole block must
    /// be aborted; it must not be recorded as an op failure.
    #[error("internal error: {0}")]
    Fatal(String),
}

/// The metadata argument of `asset_issue` and `asset_create` (Borsh, docs/ABI.md section 3): a name of 1 to 64 bytes,
/// a symbol of 1 to 16 bytes and at most 18 decimals (`ksana_primitives::asset::check_meta`, D-121).
#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct AssetMetaInput {
    /// Asset name.
    pub name: String,
    /// Asset symbol.
    pub symbol: String,
    /// Decimals.
    pub decimals: u8,
}

/// The capabilities and pause state of an asset (`asset_policy`, D-121).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AssetPolicy {
    /// The capabilities its issuing contract still holds (`ksana_primitives::asset::CAP_*`).
    pub capabilities: u8,
    /// Whether it is paused.
    pub paused: bool,
}

/// One output of the BTC transaction carrying the current op (`native_tx`, D-078).
#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct NativeOutput {
    /// Amount (sats).
    pub value: u64,
    /// Locking script.
    pub script: Vec<u8>,
}

/// The BTC transaction carrying the current op (a path-C native op, D-078): the `native_tx` host function returns its
/// Borsh encoding (docs/ABI.md section 3).
#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct NativeTxInfo {
    /// Transaction ID (Bitcoin internal byte order, D-001).
    pub txid: [u8; 32],
    /// All outputs, in transaction order.
    pub outputs: Vec<NativeOutput>,
}

/// The code of a deployed contract.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContractCode {
    /// `sha256(code)`, for the compilation cache.
    pub code_hash: [u8; 32],
    /// wasm bytecode.
    pub code: Vec<u8>,
}

/// State access provided by the execution layer to the VM. All methods operate on the current op's write buffer, which
/// the execution layer discards as a whole when the op fails.
pub trait Host: 'static {
    /// Reads contract storage.
    fn storage_get(
        &mut self,
        contract: &[u8; 32],
        key: &[u8],
    ) -> Result<Option<Vec<u8>>, HostError>;
    /// Writes contract storage.
    fn storage_set(
        &mut self,
        contract: &[u8; 32],
        key: &[u8],
        value: &[u8],
    ) -> Result<(), HostError>;
    /// Removes a key from contract storage.
    fn storage_remove(&mut self, contract: &[u8; 32], key: &[u8]) -> Result<(), HostError>;
    /// The fast-lane block height.
    fn block_height(&self) -> u64;
    /// The timestamp of the main-chain block executing this op (seconds, D-073).
    fn block_timestamp(&self) -> u64;
    /// The BTC height anchored by the current block.
    fn btc_anchor_height(&self) -> u32;
    /// The 80-byte BTC block header at height `height`; the VM has already checked the depth rule. Returns `None`
    /// when unknown.
    fn btc_header(&mut self, height: u32) -> Result<Option<[u8; 80]>, HostError>;
    /// The BTC transaction carrying the current op (D-078): a path-C native op returns `Some`, other ops (including
    /// read-only calls) return `None`. All call levels within one op see the same transaction.
    fn native_tx(&mut self) -> Result<Option<NativeTxInfo>, HostError>;
    /// Built-in asset balance.
    fn asset_balance(&mut self, asset: &[u8; 32], account: &[u8; 32]) -> Result<u128, HostError>;
    /// Built-in asset transfer; returns `Failed` on insufficient balance or when the asset does not exist.
    fn asset_transfer(
        &mut self,
        asset: &[u8; 32],
        from: &[u8; 32],
        to: &[u8; 32],
        amount: u128,
    ) -> Result<(), HostError>;
    /// Moves funds a contract attaches to `call_contract`, `try_call_contract` or `create_contract` from the calling
    /// contract to the callee, one asset at a time (D-073): the same transfer as [`Host::asset_transfer`], told apart
    /// so that the execution layer can record it as attached funds in the receipt (D-122).
    fn transfer_attached(
        &mut self,
        asset: &[u8; 32],
        from: &[u8; 32],
        to: &[u8; 32],
        amount: u128,
    ) -> Result<(), HostError> {
        self.asset_transfer(asset, from, to, amount)
    }
    /// Issues `asset` by the `issuer` contract (the VM has already derived it as `sha256(issuer ‖ sub_id)`) and
    /// credits it to `to`. The first issuance writes the metadata; afterwards `meta` must match the record and the
    /// asset must still have `CAP_MINT` (D-121), and `Failed` is returned when the total supply overflows.
    fn asset_issue(
        &mut self,
        asset: &[u8; 32],
        issuer: &[u8; 32],
        meta: &AssetMetaInput,
        to: &[u8; 32],
        amount: u128,
    ) -> Result<(), HostError>;
    /// Creates `asset` for the `issuer` contract (the VM has already derived it as `sha256(issuer ‖ sub_id)`) with
    /// `capabilities` and credits `amount` of it to `to` (D-121); returns `Failed` when the asset exists, the metadata
    /// breaks the limits, `capabilities` has an undefined bit or the asset's rules refuse `to`.
    fn asset_create(
        &mut self,
        asset: &[u8; 32],
        issuer: &[u8; 32],
        meta: &AssetMetaInput,
        capabilities: u8,
        to: &[u8; 32],
        amount: u128,
    ) -> Result<(), HostError>;
    /// Sets (`on`) or clears the status flag `flag` (`STATUS_FROZEN` / `STATUS_ALLOWED`) of `account` for the asset
    /// of the `issuer` contract (D-121); setting a flag needs the matching capability.
    fn asset_set_status(
        &mut self,
        asset: &[u8; 32],
        issuer: &[u8; 32],
        account: &[u8; 32],
        flag: u8,
        on: bool,
    ) -> Result<(), HostError>;
    /// Pauses or resumes the asset of the `issuer` contract (D-121); pausing needs `CAP_PAUSE`.
    fn asset_pause(
        &mut self,
        asset: &[u8; 32],
        issuer: &[u8; 32],
        paused: bool,
    ) -> Result<(), HostError>;
    /// Removes `capabilities` from the asset of the `issuer` contract for good (D-121).
    fn asset_renounce(
        &mut self,
        asset: &[u8; 32],
        issuer: &[u8; 32],
        capabilities: u8,
    ) -> Result<(), HostError>;
    /// The capabilities and pause state of `asset`; `None` when it does not exist (D-121).
    fn asset_policy(&mut self, asset: &[u8; 32]) -> Result<Option<AssetPolicy>, HostError>;
    /// The status flags of `account` for `asset` as recorded; 0 when it has none (D-121).
    fn asset_status(&mut self, asset: &[u8; 32], account: &[u8; 32]) -> Result<u8, HostError>;
    /// Burns `amount` of `asset` held by `from`, reducing the total supply accordingly; returns `Failed` when the
    /// asset does not exist or the balance is insufficient (D-073).
    fn asset_burn(
        &mut self,
        asset: &[u8; 32],
        from: &[u8; 32],
        amount: u128,
    ) -> Result<(), HostError>;
    /// Code already on chain (stored by some deployment); returns `None` when it does not exist.
    fn code(&mut self, code_hash: &[u8; 32]) -> Result<Option<Vec<u8>>, HostError>;
    /// Registers contract `contract` (code `code_hash`, creator `creator`); the VM has already confirmed that the
    /// address is unoccupied and the code exists (D-073).
    fn create_contract(
        &mut self,
        contract: &[u8; 32],
        code_hash: &[u8; 32],
        creator: &[u8; 32],
    ) -> Result<(), HostError>;
    /// Contract code; returns `None` when the contract does not exist.
    fn contract_code(&mut self, contract: &[u8; 32]) -> Result<Option<ContractCode>, HostError>;
    /// Records an event.
    fn emit_event(&mut self, contract: &[u8; 32], data: &[u8]) -> Result<(), HostError>;
    /// Sets a checkpoint (before `try_call_contract` enters the callee, D-073): the state writes and events after it
    /// can be undone by [`Host::revert_checkpoint`]. Checkpoints can be nested.
    fn checkpoint(&mut self) -> Result<(), HostError>;
    /// The callee failed: undoes all state writes and events after the most recent checkpoint and removes that
    /// checkpoint.
    fn revert_checkpoint(&mut self) -> Result<(), HostError>;
    /// The callee succeeded: keeps the writes and events after the most recent checkpoint (merging them into the outer
    /// level) and removes that checkpoint.
    fn release_checkpoint(&mut self) -> Result<(), HostError>;
}

/// A top-level contract call.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CallRequest {
    /// The callee contract.
    pub contract: [u8; 32],
    /// The caller: the VmOp's signing account.
    pub caller: [u8; 32],
    /// Method name.
    pub method: String,
    /// Arguments.
    pub args: Vec<u8>,
    /// The funds attached to this call (the execution layer has already transferred them from the caller to the
    /// contract, D-073; the VM only records them for `call_funds` to read).
    pub funds: Vec<Funds>,
    /// Whether calling `init` is allowed (true only at deployment).
    pub allow_init: bool,
}

/// The reason a call failed (deterministic).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FailureReason {
    /// Out of gas.
    OutOfGas,
    /// The contract reverted explicitly (`Err(reason)`).
    Revert(Vec<u8>),
    /// A wasm trap (unreachable, out of bounds, division by zero, etc.).
    Trap(String),
    /// A violation of the ABI or host function rules (length limits, depth, illegal output, etc.), or a host operation
    /// failure.
    Rejected(String),
}

/// The result of a call.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CallOutcome {
    /// Success: the data in `Ok` and the gas consumed.
    Success {
        /// Returned data.
        output: Vec<u8>,
        /// Gas consumed.
        gas_used: u64,
    },
    /// Failure (gas is recorded as the full limit, D-013).
    Failure(FailureReason),
}

/// Errors that cannot be recorded as an op failure and must abort block processing.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum VmError {
    /// An internal error reported by `Host`.
    #[error("host internal error: {0}")]
    Host(String),
    /// A wasmtime internal error (unrelated to the contract's behavior).
    #[error("VM internal error: {0}")]
    Internal(String),
}
