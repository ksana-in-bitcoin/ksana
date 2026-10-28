//! Safe wrappers of the host functions (ABI §3) and identifier computation (ABI §6).
//!
//! When a host function errors (out of bounds, storage key too long, insufficient BTC block header depth, insufficient
//! balance, etc.) the VM traps directly and the whole op fails; the contract cannot catch it, so these functions have
//! no error return value.

use alloc::string::String;
use alloc::vec::Vec;

use borsh::BorshSerialize;

use crate::backend;
use crate::codec::{decode, encode, Decode};

/// Asset ID of the project token KSA: 32 zero bytes (ABI §6).
pub const KSA_ASSET_ID: [u8; 32] = [0u8; 32];

/// Asset metadata for [`asset_issue`] and [`asset_create`] (ABI §3), passed to the host as `meta` after Borsh
/// encoding.
///
/// The name is 1 to 64 bytes, the symbol 1 to 16 bytes and the decimals at most 18 (D-121); the host traps otherwise.
/// The asset's first issuance or its creation records them; every later issuance must pass exactly the same values,
/// otherwise it traps.
#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize)]
pub struct AssetMetaInput {
    /// Asset name.
    pub name: String,
    /// Asset symbol (a ticker such as `USDX`).
    pub symbol: String,
    /// Decimals (number of decimal places).
    pub decimals: u8,
}

impl Decode for AssetMetaInput {
    fn decode(input: &mut &[u8]) -> Option<Self> {
        Some(Self {
            name: String::decode(input)?,
            symbol: String::decode(input)?,
            decimals: u8::decode(input)?,
        })
    }
}

/// Capability of an asset ([`asset_create`], D-121): its issuing contract can issue more with [`asset_issue`].
pub const CAP_MINT: u8 = 1;
/// Capability: its issuing contract can freeze an account ([`asset_freeze`]), which then can neither send, receive nor
/// burn the asset.
pub const CAP_FREEZE: u8 = 2;
/// Capability: only the accounts its issuing contract allows ([`asset_allow`]) can send, receive and burn the asset.
pub const CAP_ALLOWLIST: u8 = 4;
/// Capability: its issuing contract can pause the asset ([`asset_pause`]), which then moves out of no account but the
/// issuing contract's own.
pub const CAP_PAUSE: u8 = 8;

/// An account's status flag for an asset ([`asset_status`], D-121): frozen; it counts only while the asset has
/// [`CAP_FREEZE`].
pub const STATUS_FROZEN: u8 = 1;
/// Status flag: allowed; it counts only while the asset has [`CAP_ALLOWLIST`].
pub const STATUS_ALLOWED: u8 = 2;

/// The capabilities and pause state of an asset ([`asset_policy`], D-121).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AssetPolicy {
    /// The capabilities its issuing contract still holds (`CAP_*` bits).
    pub capabilities: u8,
    /// Whether it is paused.
    pub paused: bool,
}

/// Maximum number of fund items attached to one call (`MAX_CALL_FUNDS`, D-073).
pub const MAX_CALL_FUNDS: usize = 16;

/// One fund item attached to a call (D-073); the Borsh encoding is the same as `Funds` in VmOp.
#[derive(Clone, Copy, Debug, PartialEq, Eq, BorshSerialize)]
pub struct Funds {
    /// Asset ID ([`KSA_ASSET_ID`] for KSA).
    pub asset: [u8; 32],
    /// Amount (greater than 0).
    pub amount: u128,
}

impl Decode for Funds {
    fn decode(input: &mut &[u8]) -> Option<Self> {
        Some(Self {
            asset: <[u8; 32]>::decode(input)?,
            amount: u128::decode(input)?,
        })
    }
}

/// One transaction output returned by [`native_tx`].
#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize)]
pub struct NativeOutput {
    /// Value (sats).
    pub value: u64,
    /// Locking script.
    pub script: Vec<u8>,
}

impl Decode for NativeOutput {
    fn decode(input: &mut &[u8]) -> Option<Self> {
        Some(Self {
            value: u64::decode(input)?,
            script: Vec::<u8>::decode(input)?,
        })
    }
}

/// The BTC transaction carrying this op (path C, D-078): the transaction ID and all outputs; the Borsh encoding is the
/// same as the return value of the host function `native_tx` (ABI §3).
#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize)]
pub struct NativeTx {
    /// Transaction ID (Bitcoin internal byte order, D-001).
    pub txid: [u8; 32],
    /// All outputs, in transaction order (including the KSN1 data output carrying this op).
    pub outputs: Vec<NativeOutput>,
}

impl Decode for NativeTx {
    fn decode(input: &mut &[u8]) -> Option<Self> {
        Some(Self {
            txid: <[u8; 32]>::decode(input)?,
            outputs: Vec::<NativeOutput>::decode(input)?,
        })
    }
}

/// Aborts this op immediately (wasm `unreachable`): all state changes and events of this op are rolled back.
///
/// Normal failures should return `Err(reason)` (revert); `trap` is only for internal errors that should not happen.
pub fn trap() -> ! {
    backend::trap("explicit trap")
}

/// The current caller: at the top level the VmOp's signing account, in an inter-contract call the calling contract's
/// ID.
pub fn caller() -> [u8; 32] {
    backend::caller()
}

/// The current contract ID.
pub fn self_id() -> [u8; 32] {
    backend::self_id()
}

/// Fast-lane block height.
pub fn block_height() -> u64 {
    u64::try_from(backend::block_height()).unwrap_or_else(|_| trap())
}

/// Timestamp of the main-chain block executing this op (Unix seconds, D-073). Filled in by the producer and only
/// satisfying the consensus timestamp rules (greater than the median time of the recent blocks, not too far ahead of
/// local time, SPEC §6.3); it must not be used as a precise time.
pub fn block_timestamp() -> u64 {
    u64::try_from(backend::block_timestamp()).unwrap_or_else(|_| trap())
}

/// Chain ID of this network (devnet 1001, signet 1002).
pub fn chain_id() -> u32 {
    u32::try_from(backend::chain_id()).unwrap_or_else(|_| trap())
}

/// Gas remaining for this call (after deducting this function's own cost).
pub fn gas_left() -> u64 {
    u64::try_from(backend::gas_left()).unwrap_or_else(|_| trap())
}

/// The BTC height anchored by the current block.
pub fn btc_anchor_height() -> u32 {
    u32::try_from(backend::btc_anchor_height()).unwrap_or_else(|_| trap())
}

/// Reads the 80-byte BTC block header at `height`.
///
/// Only headers with `height <= btc_anchor_height() - BTC_QUERY_DEPTH` can be read (the depth is 6 on devnet);
/// otherwise, or when the header does not exist, the host traps (ABI §3). See [`crate::btc::BlockHeader`] for parsing.
pub fn btc_header(height: u32) -> [u8; 80] {
    backend::btc_header(i64::from(height))
}

/// When this op is a path-C native op (carried by a BTC transaction and force-included), returns the transaction ID and
/// all outputs of the BTC transaction carrying it (D-078); path-B ops and read-only calls return `None`. All call
/// levels within one op read the same transaction, so the contract must itself prevent the same payment from being
/// credited twice (e.g. by recording processed payments by transaction ID).
pub fn native_tx() -> Option<NativeTx> {
    let bytes = backend::native_tx()?;
    Some(decode(&bytes).unwrap_or_else(|_| trap()))
}

/// Balance of asset `asset` held by `account`.
pub fn asset_balance(asset: &[u8; 32], account: &[u8; 32]) -> u128 {
    u128::from_le_bytes(backend::asset_balance(asset, account))
}

/// Transfers `amount` of `asset` from **this contract's account** to `to` (D-016); the host traps on insufficient
/// balance or a nonexistent asset.
pub fn asset_transfer(asset: &[u8; 32], to: &[u8; 32], amount: u128) {
    backend::asset_transfer(asset, to, &amount.to_le_bytes());
}

/// Burns `amount` of `asset` held by **this contract's account**, reducing the asset's total supply accordingly (D-073,
/// KSA too); the host traps on insufficient balance or a nonexistent asset.
pub fn asset_burn(asset: &[u8; 32], amount: u128) {
    backend::asset_burn(asset, &amount.to_le_bytes());
}

/// Issues `amount` of this contract's asset `sub_id` to `to`, returning the asset ID
/// (= [`asset_id`]`(self_id(), sub_id)`).
///
/// The first issuance creates the asset with [`CAP_MINT`] alone and records `meta`; afterwards `meta` must match the
/// record and the asset must still have [`CAP_MINT`] (D-121). The host traps when the total supply overflows u128 or
/// the asset's rules refuse `to`.
pub fn asset_issue(
    sub_id: &[u8; 32],
    meta: &AssetMetaInput,
    to: &[u8; 32],
    amount: u128,
) -> [u8; 32] {
    backend::asset_issue(sub_id, &encode(meta), to, &amount.to_le_bytes())
}

/// Creates this contract's asset `sub_id` with `capabilities` (`CAP_*` bits) and issues `amount` of it (0 for none) to
/// `to`, returning the asset ID (= [`asset_id`]`(self_id(), sub_id)`, D-121).
///
/// The capabilities are fixed here: afterwards they can only be given up with [`asset_renounce`]. Without
/// [`CAP_MINT`] the supply stays `amount` for good. The host traps when the asset exists, `meta` breaks its limits,
/// `capabilities` has an undefined bit or the asset refuses `to` (with [`CAP_ALLOWLIST`] only this contract can
/// receive it before it allows any account).
pub fn asset_create(
    sub_id: &[u8; 32],
    meta: &AssetMetaInput,
    capabilities: u8,
    to: &[u8; 32],
    amount: u128,
) -> [u8; 32] {
    backend::asset_create(
        sub_id,
        &encode(meta),
        capabilities,
        to,
        &amount.to_le_bytes(),
    )
}

/// Freezes (`frozen`) or unfreezes `account` for this contract's asset `sub_id` (D-121): a frozen account can neither
/// send, receive nor burn it. Freezing needs [`CAP_FREEZE`]; unfreezing never does. The host traps when the asset does
/// not exist or `account` is this contract.
pub fn asset_freeze(sub_id: &[u8; 32], account: &[u8; 32], frozen: bool) {
    backend::asset_freeze(sub_id, account, frozen);
}

/// Allows (`allowed`) `account` to send, receive and burn this contract's asset `sub_id`, or stops allowing it
/// (D-121). Allowing needs [`CAP_ALLOWLIST`]; disallowing never does. The host traps when the asset does not exist or
/// `account` is this contract.
pub fn asset_allow(sub_id: &[u8; 32], account: &[u8; 32], allowed: bool) {
    backend::asset_allow(sub_id, account, allowed);
}

/// Pauses or resumes this contract's asset `sub_id` (D-121): while paused it moves out of no account but this
/// contract's. Pausing needs [`CAP_PAUSE`]; resuming never does.
pub fn asset_pause(sub_id: &[u8; 32], paused: bool) {
    backend::asset_pause(sub_id, paused);
}

/// Gives up `capabilities` (`CAP_*` bits) of this contract's asset `sub_id` for good (D-121); capabilities it no
/// longer has are ignored, and giving up [`CAP_PAUSE`] resumes a paused asset. The host traps on an undefined bit.
pub fn asset_renounce(sub_id: &[u8; 32], capabilities: u8) {
    backend::asset_renounce(sub_id, capabilities);
}

/// The capabilities and pause state of `asset`, any asset (D-121); `None` when it does not exist. KSA has no
/// capabilities.
pub fn asset_policy(asset: &[u8; 32]) -> Option<AssetPolicy> {
    let v = backend::asset_policy(asset);
    if v < 0 {
        return None;
    }
    Some(AssetPolicy {
        capabilities: u8::try_from(v & 0xFF).unwrap_or(0),
        paused: v & 0x100 != 0,
    })
}

/// The status flags (`STATUS_*` bits) recorded for `account` and `asset` (D-121); 0 when it has none. A flag counts
/// only while the asset has the matching capability ([`asset_policy`]), and the issuing contract has none.
pub fn asset_status(asset: &[u8; 32], account: &[u8; 32]) -> u8 {
    u8::try_from(backend::asset_status(asset, account)).unwrap_or(0)
}

/// Asset ID of a contract-issued asset: `sha256(issuing contract ID ‖ sub_id)` (ABI §6).
pub fn asset_id(issuer: &[u8; 32], sub_id: &[u8; 32]) -> [u8; 32] {
    sha256(&[issuer.as_slice(), sub_id.as_slice()].concat())
}

/// Contract ID: `sha256(deployer ‖ nonce as u64 little-endian ‖ code_hash)`, where `code_hash = sha256(code)` (ABI §6).
pub fn contract_id(deployer: &[u8; 32], nonce: u64, code_hash: &[u8; 32]) -> [u8; 32] {
    sha256(
        &[
            deployer.as_slice(),
            nonce.to_le_bytes().as_slice(),
            code_hash.as_slice(),
        ]
        .concat(),
    )
}

/// Funds attached to this call, in ascending asset ID order (D-073). The caller (at the top level the VmOp's signing
/// account, in an inter-contract call the calling contract) has already transferred them into this contract's account
/// before entering this contract; empty when nothing is attached.
pub fn call_funds() -> Vec<Funds> {
    decode(&backend::call_funds()).unwrap_or_else(|_| trap())
}

/// Amount of `asset` attached to this call; 0 when that asset is not attached.
pub fn received(asset: &[u8; 32]) -> u128 {
    call_funds()
        .iter()
        .find(|f| f.asset == *asset)
        .map_or(0, |f| f.amount)
}

/// ID of a contract created by a contract: `sha256(creator ‖ salt ‖ code_hash)` (D-073, similar to CREATE2).
pub fn created_contract_id(creator: &[u8; 32], salt: &[u8; 32], code_hash: &[u8; 32]) -> [u8; 32] {
    sha256(&[creator.as_slice(), salt.as_slice(), code_hash.as_slice()].concat())
}

/// Creates a contract from code already on chain (stored by some deployment, `code_hash = sha256(code)`), returning its
/// ID ([`created_contract_id`]`(self_id(), salt, code_hash)`, D-073). The new contract's `init(init_args)` runs with
/// this contract as the caller; before it runs, `funds` (same rules as [`call_with_funds`]) are transferred from this
/// contract's account to the new contract. The whole op fails when the code does not exist, the address is already
/// taken or `init` fails.
pub fn create_contract(
    code_hash: &[u8; 32],
    salt: &[u8; 32],
    init_args: &[u8],
    funds: &[Funds],
) -> [u8; 32] {
    let funds = if funds.is_empty() {
        Vec::new()
    } else {
        encode(funds)
    };
    backend::create_contract(code_hash, salt, init_args, &funds)
}

/// Calls method `method` of contract `contract`, with `args` as that method's argument bytes (usually Borsh-encoded),
/// and returns the data in the callee's `Ok`. Attaches no funds; see [`call_with_funds`].
///
/// When the callee fails (revert, trap, out of gas, etc.) the whole op fails and the caller cannot catch it (ABI §2).
/// The method name `init` can only be called at deployment; using it here fails.
pub fn call(contract: &[u8; 32], method: &str, args: &[u8]) -> Vec<u8> {
    call_with_funds(contract, method, args, &[])
}

/// Same as [`call`], and transfers `funds` from **this contract's account** to the callee (which reads them with
/// [`call_funds`], D-073).
///
/// `funds` must be in strictly ascending asset ID order (no duplicates), each amount greater than 0, at most
/// [`MAX_CALL_FUNDS`] items; the host traps when this is not satisfied or the balance is insufficient.
pub fn call_with_funds(contract: &[u8; 32], method: &str, args: &[u8], funds: &[Funds]) -> Vec<u8> {
    let funds = if funds.is_empty() {
        Vec::new()
    } else {
        encode(funds)
    };
    backend::call_contract(contract, &encode(&(method, args)), &funds)
}

/// Failure of [`try_call`] / [`try_call_with`]: all state changes, transfers and events of the callee's level have been
/// rolled back, and the caller continues.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CallError {
    /// The callee explicitly reverted (returned `Err(reason)`), with the reason.
    Revert(Vec<u8>),
    /// Other failures: trap, out of gas, rule violation, nonexistent contract, call depth exceeded, attached funds
    /// transfer failed, etc.
    Failed,
}

/// Catchable inter-contract call: same as [`call`], but when the callee fails only that level is rolled back and a
/// [`CallError`] is returned (D-073).
///
/// The callee uses at most 63/64 of the remaining gas; the caller keeps at least 1/64 to handle the failure. The
/// caller's own errors (method name `init`, input too long, etc.) still fail the whole op.
pub fn try_call(contract: &[u8; 32], method: &str, args: &[u8]) -> Result<Vec<u8>, CallError> {
    try_call_with(contract, method, args, &[], 0)
}

/// Same as [`try_call`], with attached funds (same rules as [`call_with_funds`]; a failed transfer returns
/// [`CallError::Failed`]) and the callee's gas limited to `gas` (0 means no extra limit, i.e. 63/64 of the remaining
/// gas; a larger value is likewise capped at 63/64).
pub fn try_call_with(
    contract: &[u8; 32],
    method: &str,
    args: &[u8],
    funds: &[Funds],
    gas: u64,
) -> Result<Vec<u8>, CallError> {
    let funds = if funds.is_empty() {
        Vec::new()
    } else {
        encode(funds)
    };
    let raw = backend::try_call_contract(contract, &encode(&(method, args)), &funds, gas);
    let (status, data): (u8, Vec<u8>) = decode(&raw).unwrap_or_else(|_| trap());
    match status {
        0 => Ok(data),
        1 => Err(CallError::Revert(data)),
        _ => Err(CallError::Failed),
    }
}

/// Delegate call: executes the code of contract `contract` in the context of the **current contract** (storage,
/// balances, [`self_id`], [`caller`] and [`call_funds`] all unchanged, D-073), returning the data in its method's `Ok`;
/// on failure the whole op fails (same as [`call`]). For proxy and upgradeable contracts: the executed code can rewrite
/// this contract's storage and transfer out this contract's assets, so only delegate to trusted code.
pub fn delegate_call(contract: &[u8; 32], method: &str, args: &[u8]) -> Vec<u8> {
    backend::delegate_call(contract, &encode(&(method, args)))
}

/// Records an event (not part of the state root; discarded when the op fails).
pub fn emit_event(data: &[u8]) {
    backend::emit_event(data);
}

/// Records a Borsh-encoded event.
pub fn emit<T: BorshSerialize + ?Sized>(event: &T) {
    emit_event(&encode(event));
}

/// SHA-256.
pub fn sha256(data: &[u8]) -> [u8; 32] {
    backend::sha256(data)
}

/// Double SHA-256: `sha256(sha256(data))` (Bitcoin's block hashes, transaction IDs, Merkle nodes).
pub fn sha256d(data: &[u8]) -> [u8; 32] {
    sha256(&sha256(data))
}

/// BIP340 signature verification: 32-byte x-only public key, 32-byte message digest, 64-byte signature.
pub fn verify_schnorr(pubkey: &[u8; 32], msg: &[u8; 32], sig: &[u8; 64]) -> bool {
    backend::verify_schnorr(pubkey, msg, sig) == 1
}
