//! In-memory mock host on non-wasm32 targets, for unit-testing contract logic in a native environment.
//!
//! The semantics match `docs/ABI.md`:
//!
//! - Within one "operation" ([`call`] / [`deploy`]), a failure at any level (revert, trap, panic, malformed output)
//!   fails the whole operation, and all storage, balance, asset and event changes are rolled back (ABI §2);
//! - Inter-contract calls ([`crate::call`]) are dispatched to the Rust dispatch functions registered with
//!   [`register`] / [`deploy`], with depth limit `MAX_CALL_DEPTH` (default 8, the top level being level 1) and
//!   re-entrancy allowed; the method name `init` can only be called at deployment;
//! - Length limits (storage key 256, value 65,536, event 16,384, asset meta 1,024, call input 64 KiB, output 64 KiB)
//!   trap when exceeded;
//! - `btc_header(h)` requires `h <= anchor - BTC_QUERY_DEPTH` (default 6) and the header at that height to be set;
//! - `asset_transfer` only transfers out of the current contract's account and the asset must exist; the asset ID of
//!   `asset_issue` is `sha256(self_id ‖ sub_id)`, the metadata (name 1 to 64 bytes, symbol 1 to 16 bytes, at most 18
//!   decimals) is recorded on the first issuance and must match afterwards, and a total supply overflow traps; records
//!   with a balance of 0 are deleted;
//! - The issuer capabilities of D-121 as the VM applies them: `asset_create` declares them, `asset_issue` needs
//!   [`crate::CAP_MINT`], every movement (transfers, attached funds, issuance to an account, burns) refuses a frozen
//!   account while the asset has [`crate::CAP_FREEZE`], an account not allowed while it has [`crate::CAP_ALLOWLIST`]
//!   (never the issuing contract), and while it is paused a movement out of any account but the issuing contract's;
//!   a refusal traps with a reason starting `rejected: `;
//! - Attached funds (D-073): [`call_with_funds`] / [`deploy_with_funds`] first transfer the funds from the signing
//!   account to the contract, and [`crate::call_with_funds`] transfers from the current contract to the callee; they
//!   must be in strictly ascending asset ID order, with amounts greater than 0 and at most [`crate::MAX_CALL_FUNDS`]
//!   items, and fail on insufficient balance; the callee reads them with [`crate::call_funds`];
//! - [`crate::try_call`] catches the failure of the callee's level (a revert returns the reason, other failures return
//!   `Failed`) and rolls back only that level; the mock host does not meter gas and the `gas` argument is ignored. The
//!   caller's own errors (input, method name `init`, encoding and rules of the attached funds) still fail the
//!   operation;
//! - [`crate::delegate_call`] runs the target contract's dispatch function in the context of the current call
//!   (`self_id`, `caller`, attached funds, storage and balances);
//! - [`crate::create_contract`] creates a contract from code registered via [`deploy`] or [`register_code`], at the
//!   address [`crate::created_contract_id`]; the creation is undone together with a failed operation; `asset_burn`
//!   reduces the balance and the total supply; `block_timestamp`, `chain_id`, `gas_left` return the values set by
//!   [`set_block_timestamp`] / [`set_chain_id`] / [`set_gas_left`]; [`crate::native_tx`] returns the transaction set
//!   by [`set_native_tx`] (default `None`, i.e. a path-B op).
//!
//! The state is thread-local (independent per test thread); call [`reset`] at the start of a test. A trap is
//! implemented as an unwind in the native environment, caught by [`call`] / [`deploy`] and turned into a [`Failure`].

use std::any::Any;
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::panic::{catch_unwind, resume_unwind, AssertUnwindSafe};
use std::rc::Rc;

use crate::codec::{decode_call_input, encode, CallResult};
use crate::{
    AssetMetaInput, Funds, NativeTx, CAP_ALLOWLIST, CAP_FREEZE, CAP_MINT, CAP_PAUSE,
    MAX_CALL_FUNDS, STATUS_ALLOWED, STATUS_FROZEN,
};

/// Maximum storage key length (bytes).
pub const MAX_STORAGE_KEY_BYTES: usize = 256;
/// Maximum storage value length (bytes).
pub const MAX_STORAGE_VALUE_BYTES: usize = 65_536;
/// Maximum length of a single event (bytes).
pub const MAX_EVENT_BYTES: usize = 16_384;
/// Maximum `asset_issue` meta length (bytes).
pub const MAX_ASSET_META_BYTES: usize = 1_024;
/// Maximum asset name length (bytes; at least 1, D-121).
pub const MAX_ASSET_NAME_BYTES: usize = 64;
/// Maximum asset symbol length (bytes; at least 1, D-121).
pub const MAX_ASSET_SYMBOL_BYTES: usize = 16;
/// Maximum decimals of an asset (D-121).
pub const MAX_ASSET_DECIMALS: u8 = 18;
/// The capabilities defined so far (D-121); the other bits are refused.
const CAPABILITIES: u8 = CAP_MINT | CAP_FREEZE | CAP_ALLOWLIST | CAP_PAUSE;
/// Maximum inter-contract call input length (`MAX_OP_BYTES`, 64 KiB on devnet).
pub const MAX_OP_BYTES: usize = 65_536;
/// Maximum contract output length (bytes).
pub const MAX_OUTPUT_BYTES: usize = 65_536;
/// Default contract call depth limit (devnet `MAX_CALL_DEPTH`).
pub const DEFAULT_MAX_CALL_DEPTH: usize = 8;
/// Default minimum BTC block header query depth (devnet `BTC_QUERY_DEPTH`).
pub const DEFAULT_BTC_QUERY_DEPTH: u32 = 6;
/// Default chain ID (devnet).
pub const DEFAULT_CHAIN_ID: u32 = 1001;
/// The mock host does not meter gas: the fixed value returned by `gas_left` (changeable with [`set_gas_left`]).
pub const DEFAULT_GAS_LEFT: u64 = 1_000_000_000;

/// A contract's dispatch function: `(method, args) -> Ok(data) | Err(reason)`.
pub type Dispatch = Rc<dyn Fn(&str, &[u8]) -> CallResult>;

/// Asset metadata (corresponding to `AssetMeta` under state prefix 0x03).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AssetInfo {
    /// Name.
    pub name: String,
    /// Symbol (D-121).
    pub symbol: String,
    /// Decimals.
    pub decimals: u8,
    /// Total supply.
    pub total_supply: u128,
    /// Issuing contract ID (32 zero bytes for KSA).
    pub issuer: [u8; 32],
    /// The capabilities the issuing contract still holds (`CAP_*` bits, D-121).
    pub capabilities: u8,
    /// Whether the asset is paused (D-121).
    pub paused: bool,
}

/// One event.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Event {
    /// The contract that emitted the event.
    pub contract: [u8; 32],
    /// Event data.
    pub data: Vec<u8>,
}

/// Reason an operation failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Failure {
    /// Contract `contract` returned `Err(reason)` (at any level).
    Revert {
        /// The contract that reverted.
        contract: [u8; 32],
        /// Revert reason.
        reason: Vec<u8>,
    },
    /// Trap: a host function refused (out of bounds, insufficient depth, etc.), the contract panicked, malformed
    /// output, call depth exceeded, etc.
    Trap(String),
}

#[derive(Clone, Default)]
struct Ledger {
    /// (contract, key) -> value.
    storage: BTreeMap<([u8; 32], Vec<u8>), Vec<u8>>,
    /// (asset, account) -> balance; deleted when the balance is 0.
    balances: BTreeMap<([u8; 32], [u8; 32]), u128>,
    assets: BTreeMap<[u8; 32], AssetInfo>,
    /// (asset, account) -> status flags (D-121); deleted when no flag is left.
    statuses: BTreeMap<([u8; 32], [u8; 32]), u8>,
    events: Vec<Event>,
    /// Contract ID -> dispatch function (contracts created by `create_contract` are rolled back with a failed
    /// operation).
    contracts: BTreeMap<[u8; 32], Dispatch>,
}

#[derive(Clone)]
struct Frame {
    caller: [u8; 32],
    self_id: [u8; 32],
    funds: Vec<Funds>,
}

struct Mock {
    ledger: Ledger,
    /// Code on chain: `sha256(code)` -> dispatch function (registered by [`deploy`] and [`register_code`]).
    codes: BTreeMap<[u8; 32], Dispatch>,
    frames: Vec<Frame>,
    block_height: u64,
    block_timestamp: u64,
    chain_id: u32,
    gas_left: u64,
    btc_anchor_height: u32,
    btc_headers: BTreeMap<u32, [u8; 80]>,
    btc_query_depth: u32,
    max_call_depth: usize,
    /// The BTC transaction carrying the current operation ([`set_native_tx`], D-078).
    native_tx: Option<NativeTx>,
}

impl Default for Mock {
    fn default() -> Self {
        Self {
            ledger: Ledger::default(),
            codes: BTreeMap::new(),
            frames: Vec::new(),
            block_height: 0,
            block_timestamp: 0,
            chain_id: DEFAULT_CHAIN_ID,
            gas_left: DEFAULT_GAS_LEFT,
            btc_anchor_height: 0,
            btc_headers: BTreeMap::new(),
            btc_query_depth: DEFAULT_BTC_QUERY_DEPTH,
            max_call_depth: DEFAULT_MAX_CALL_DEPTH,
            native_tx: None,
        }
    }
}

std::thread_local! {
    static MOCK: RefCell<Mock> = RefCell::new(Mock::default());
}

/// Accesses the mock state. The closure must not call contract code or other host functions (the borrow is not
/// re-entrant).
fn with<R>(f: impl FnOnce(&mut Mock) -> R) -> R {
    MOCK.with(|m| f(&mut m.borrow_mut()))
}

fn fail(failure: Failure) -> ! {
    // `resume_unwind` does not invoke the panic hook, so a trap prints no panic message in the test output.
    resume_unwind(Box::new(failure))
}

/// Rules for attached funds (same as the VM, D-073): at most [`MAX_CALL_FUNDS`] items, amounts greater than 0, asset
/// IDs strictly ascending.
fn check_funds(funds: &[Funds]) -> Result<(), &'static str> {
    if funds.len() > MAX_CALL_FUNDS {
        return Err("too many funds");
    }
    if funds.iter().any(|f| f.amount == 0) {
        return Err("zero funds amount");
    }
    if !funds
        .windows(2)
        .all(|w| matches!(w, [a, b] if a.asset < b.asset))
    {
        return Err("funds must be sorted by asset without duplicates");
    }
    Ok(())
}

/// The limits of an asset's metadata (D-121).
fn check_meta(meta: &AssetMetaInput) -> Result<(), &'static str> {
    if meta.name.is_empty() || meta.name.len() > MAX_ASSET_NAME_BYTES {
        return Err("asset name must be 1 to 64 bytes");
    }
    if meta.symbol.is_empty() || meta.symbol.len() > MAX_ASSET_SYMBOL_BYTES {
        return Err("asset symbol must be 1 to 16 bytes");
    }
    if meta.decimals > MAX_ASSET_DECIMALS {
        return Err("asset decimals must be at most 18");
    }
    Ok(())
}

/// Refuses `account` as a sender (`sending`) or a recipient of `asset` when the asset's rules bar it (D-121): a frozen
/// account while it has `CAP_FREEZE`, an account not allowed while it has `CAP_ALLOWLIST`; never the issuing contract.
fn check_party(
    ledger: &Ledger,
    asset: &[u8; 32],
    account: &[u8; 32],
    sending: bool,
) -> Result<(), &'static str> {
    let Some(info) = ledger.assets.get(asset) else {
        return Err("asset does not exist");
    };
    check_party_of(ledger, asset, info, account, sending)
}

/// [`check_party`] with the asset's metadata `info` given.
fn check_party_of(
    ledger: &Ledger,
    asset: &[u8; 32],
    info: &AssetInfo,
    account: &[u8; 32],
    sending: bool,
) -> Result<(), &'static str> {
    if *account == info.issuer {
        return Ok(());
    }
    let status = ledger
        .statuses
        .get(&(*asset, *account))
        .copied()
        .unwrap_or(0);
    if info.capabilities & CAP_FREEZE != 0 && status & STATUS_FROZEN != 0 {
        return Err(if sending {
            "rejected: sender is frozen for this asset"
        } else {
            "rejected: recipient is frozen for this asset"
        });
    }
    if info.capabilities & CAP_ALLOWLIST != 0 && status & STATUS_ALLOWED == 0 {
        return Err(if sending {
            "rejected: sender is not on this asset's allowlist"
        } else {
            "rejected: recipient is not on this asset's allowlist"
        });
    }
    Ok(())
}

/// Refuses a movement of `asset` out of `from`: while it is paused, unless `from` is its issuing contract, and when its
/// rules bar `from` as a sender (D-121).
fn check_sender(ledger: &Ledger, asset: &[u8; 32], from: &[u8; 32]) -> Result<(), &'static str> {
    let Some(info) = ledger.assets.get(asset) else {
        return Err("asset does not exist");
    };
    if info.paused && *from != info.issuer {
        return Err("rejected: asset is paused");
    }
    check_party(ledger, asset, from, true)
}

/// Transfers `amount` of `asset` from `from` to `to`: the asset must exist, its rules must allow the movement (D-121)
/// and the balance must suffice.
fn transfer(
    ledger: &mut Ledger,
    asset: &[u8; 32],
    from: &[u8; 32],
    to: &[u8; 32],
    amount: u128,
) -> Result<(), &'static str> {
    if !ledger.assets.contains_key(asset) {
        return Err("asset does not exist");
    }
    check_sender(ledger, asset, from)?;
    check_party(ledger, asset, to, false)?;
    let from_balance = ledger.balances.get(&(*asset, *from)).copied().unwrap_or(0);
    let Some(rest) = from_balance.checked_sub(amount) else {
        return Err("insufficient balance");
    };
    if from == to {
        return Ok(());
    }
    let to_balance = ledger.balances.get(&(*asset, *to)).copied().unwrap_or(0);
    if to_balance.checked_add(amount).is_none() {
        return Err("balance overflow");
    }
    if rest == 0 {
        ledger.balances.remove(&(*asset, *from));
    } else {
        ledger.balances.insert((*asset, *from), rest);
    }
    credit(ledger, *asset, *to, amount).ok_or("balance overflow")
}

/// Transfers the attached funds from `from` to `to`; traps when the rules are not met or the balance is insufficient.
fn transfer_funds(from: &[u8; 32], to: &[u8; 32], funds: &[Funds]) {
    if let Err(reason) = check_funds(funds) {
        host::trap(reason);
    }
    for f in funds {
        if let Err(reason) = with(|m| transfer(&mut m.ledger, &f.asset, from, to, f.amount)) {
            host::trap(reason);
        }
    }
}

fn credit(ledger: &mut Ledger, asset: [u8; 32], account: [u8; 32], amount: u128) -> Option<()> {
    let balance = ledger.balances.get(&(asset, account)).copied().unwrap_or(0);
    let new_balance = balance.checked_add(amount)?;
    if new_balance != 0 {
        ledger.balances.insert((asset, account), new_balance);
    }
    Some(())
}

// ---------------------------------------------------------------------------
// Host functions (same names and signatures as `crate::wasm::host`).

pub(crate) mod host {
    use super::*;

    pub fn trap(reason: &str) -> ! {
        fail(Failure::Trap(reason.to_string()))
    }

    fn frame() -> Frame {
        with(|m| m.frames.last().cloned())
            .unwrap_or_else(|| trap("host function called outside of a contract call"))
    }

    fn check_len(len: usize, max: usize, what: &str) {
        if len > max {
            trap(what);
        }
    }

    pub fn storage_get(key: &[u8]) -> Option<Vec<u8>> {
        check_len(key.len(), MAX_STORAGE_KEY_BYTES, "storage key too long");
        let me = frame().self_id;
        with(|m| m.ledger.storage.get(&(me, key.to_vec())).cloned())
    }

    pub fn storage_set(key: &[u8], value: &[u8]) {
        check_len(key.len(), MAX_STORAGE_KEY_BYTES, "storage key too long");
        check_len(
            value.len(),
            MAX_STORAGE_VALUE_BYTES,
            "storage value too long",
        );
        let me = frame().self_id;
        with(|m| m.ledger.storage.insert((me, key.to_vec()), value.to_vec()));
    }

    pub fn storage_remove(key: &[u8]) {
        check_len(key.len(), MAX_STORAGE_KEY_BYTES, "storage key too long");
        let me = frame().self_id;
        with(|m| m.ledger.storage.remove(&(me, key.to_vec())));
    }

    pub fn caller() -> [u8; 32] {
        frame().caller
    }

    pub fn self_id() -> [u8; 32] {
        frame().self_id
    }

    pub fn block_height() -> i64 {
        i64::try_from(with(|m| m.block_height))
            .unwrap_or_else(|_| trap("block height out of range"))
    }

    pub fn btc_anchor_height() -> i64 {
        i64::from(with(|m| m.btc_anchor_height))
    }

    pub fn block_timestamp() -> i64 {
        i64::try_from(with(|m| m.block_timestamp))
            .unwrap_or_else(|_| trap("block timestamp out of range"))
    }

    pub fn chain_id() -> i64 {
        i64::from(with(|m| m.chain_id))
    }

    pub fn gas_left() -> i64 {
        i64::try_from(with(|m| m.gas_left)).unwrap_or(i64::MAX)
    }

    pub fn asset_burn(asset: &[u8; 32], amount: &[u8; 16]) {
        let from = frame().self_id;
        let amount = u128::from_le_bytes(*amount);
        let result = with(|m| {
            check_sender(&m.ledger, asset, &from)?;
            let Some(info) = m.ledger.assets.get(asset) else {
                return Err("asset does not exist");
            };
            let balance = m.ledger.balances.get(&(*asset, from)).copied().unwrap_or(0);
            let Some(rest) = balance.checked_sub(amount) else {
                return Err("insufficient balance");
            };
            let Some(supply) = info.total_supply.checked_sub(amount) else {
                return Err("total supply below balance");
            };
            if let Some(info) = m.ledger.assets.get_mut(asset) {
                info.total_supply = supply;
            }
            if rest == 0 {
                m.ledger.balances.remove(&(*asset, from));
            } else {
                m.ledger.balances.insert((*asset, from), rest);
            }
            Ok(())
        });
        if let Err(reason) = result {
            trap(reason);
        }
    }

    pub fn create_contract(
        code_hash: &[u8; 32],
        salt: &[u8; 32],
        args: &[u8],
        funds: &[u8],
    ) -> [u8; 32] {
        check_len(args.len(), MAX_OP_BYTES, "call input too long");
        let funds = decode_funds(funds);
        let me = frame().self_id;
        let Some(dispatch) = with(|m| m.codes.get(code_hash).cloned()) else {
            trap("unknown code")
        };
        let id = crate::created_contract_id(&me, salt, code_hash);
        if with(|m| m.ledger.contracts.contains_key(&id)) {
            trap("contract already exists");
        }
        with(|m| m.ledger.contracts.insert(id, dispatch));
        invoke(me, id, &encode(&("init", args)), true, funds);
        id
    }

    pub fn btc_header(height: i64) -> [u8; 80] {
        let result = with(|m| {
            let deepest_allowed =
                i64::from(m.btc_anchor_height).checked_sub(i64::from(m.btc_query_depth));
            if height < 0 {
                return Err("negative btc height");
            }
            if deepest_allowed.is_none_or(|max| height > max) {
                return Err("btc header is not deep enough");
            }
            u32::try_from(height)
                .ok()
                .and_then(|h| m.btc_headers.get(&h).copied())
                .ok_or("unknown btc height")
        });
        result.unwrap_or_else(|reason| trap(reason))
    }

    pub fn asset_balance(asset: &[u8; 32], account: &[u8; 32]) -> [u8; 16] {
        with(|m| m.ledger.balances.get(&(*asset, *account)).copied())
            .unwrap_or(0)
            .to_le_bytes()
    }

    pub fn asset_transfer(asset: &[u8; 32], to: &[u8; 32], amount: &[u8; 16]) {
        let from = frame().self_id;
        let amount = u128::from_le_bytes(*amount);
        if let Err(reason) = with(|m| transfer(&mut m.ledger, asset, &from, to, amount)) {
            trap(reason);
        }
    }

    pub fn asset_issue(
        sub_id: &[u8; 32],
        meta: &[u8],
        to: &[u8; 32],
        amount: &[u8; 16],
    ) -> [u8; 32] {
        check_len(meta.len(), MAX_ASSET_META_BYTES, "asset meta too long");
        let meta: AssetMetaInput =
            crate::decode(meta).unwrap_or_else(|_| trap("malformed asset meta"));
        let issuer = frame().self_id;
        let asset = crate::asset_id(&issuer, sub_id);
        let amount = u128::from_le_bytes(*amount);
        let result = with(|m| {
            let info = match m.ledger.assets.get(&asset) {
                Some(info)
                    if info.name != meta.name
                        || info.symbol != meta.symbol
                        || info.decimals != meta.decimals =>
                {
                    return Err("asset metadata does not match the recorded one");
                }
                Some(info) if info.capabilities & CAP_MINT == 0 => {
                    return Err("rejected: asset supply is fixed");
                }
                Some(info) => info.clone(),
                None => {
                    check_meta(&meta)?;
                    AssetInfo {
                        name: meta.name.clone(),
                        symbol: meta.symbol.clone(),
                        decimals: meta.decimals,
                        total_supply: 0,
                        issuer,
                        capabilities: CAP_MINT,
                        paused: false,
                    }
                }
            };
            mint_new(&mut m.ledger, asset, info, to, amount)
        });
        if let Err(reason) = result {
            trap(reason);
        }
        asset
    }

    pub fn asset_create(
        sub_id: &[u8; 32],
        meta: &[u8],
        capabilities: u8,
        to: &[u8; 32],
        amount: &[u8; 16],
    ) -> [u8; 32] {
        check_len(meta.len(), MAX_ASSET_META_BYTES, "asset meta too long");
        let meta: AssetMetaInput =
            crate::decode(meta).unwrap_or_else(|_| trap("malformed asset meta"));
        let issuer = frame().self_id;
        let asset = crate::asset_id(&issuer, sub_id);
        let amount = u128::from_le_bytes(*amount);
        let result = with(|m| {
            if m.ledger.assets.contains_key(&asset) {
                return Err("asset already exists");
            }
            if capabilities & !CAPABILITIES != 0 {
                return Err("undefined capability bits");
            }
            check_meta(&meta)?;
            let info = AssetInfo {
                name: meta.name.clone(),
                symbol: meta.symbol.clone(),
                decimals: meta.decimals,
                total_supply: 0,
                issuer,
                capabilities,
                paused: false,
            };
            mint_new(&mut m.ledger, asset, info, to, amount)
        });
        if let Err(reason) = result {
            trap(reason);
        }
        asset
    }

    /// Records `info` with `amount` more supply and credits `amount` to `to`, once the asset's rules accept `to`.
    fn mint_new(
        ledger: &mut Ledger,
        asset: [u8; 32],
        info: AssetInfo,
        to: &[u8; 32],
        amount: u128,
    ) -> Result<(), &'static str> {
        check_party_of(ledger, &asset, &info, to, false)?;
        let Some(new_supply) = info.total_supply.checked_add(amount) else {
            return Err("asset total supply overflow");
        };
        // When the total supply does not overflow, no single balance overflows either (the balances sum to the
        // total supply).
        credit(ledger, asset, *to, amount).ok_or("balance overflow")?;
        ledger.assets.insert(
            asset,
            AssetInfo {
                total_supply: new_supply,
                ..info
            },
        );
        Ok(())
    }

    /// This contract's asset `sub_id`, which must exist (the asset management functions of D-121).
    fn own_asset(sub_id: &[u8; 32]) -> ([u8; 32], [u8; 32]) {
        let issuer = frame().self_id;
        let asset = crate::asset_id(&issuer, sub_id);
        if !with(|m| m.ledger.assets.contains_key(&asset)) {
            trap("asset does not exist");
        }
        (issuer, asset)
    }

    /// Sets or clears `flag`, which needs the capability `needs`, named `name` (D-121).
    fn set_status(
        sub_id: &[u8; 32],
        account: &[u8; 32],
        flag: u8,
        needs: u8,
        name: &str,
        on: bool,
    ) {
        let (issuer, asset) = own_asset(sub_id);
        if *account == issuer {
            trap("the issuing contract has no status for its own asset");
        }
        let capabilities = with(|m| m.ledger.assets.get(&asset).map_or(0, |i| i.capabilities));
        if on && capabilities & needs == 0 {
            trap(&format!("asset does not have the {name} capability"));
        }
        with(|m| {
            let old = m
                .ledger
                .statuses
                .get(&(asset, *account))
                .copied()
                .unwrap_or(0);
            let new = if on { old | flag } else { old & !flag };
            if new == 0 {
                m.ledger.statuses.remove(&(asset, *account));
            } else {
                m.ledger.statuses.insert((asset, *account), new);
            }
        });
    }

    pub fn asset_freeze(sub_id: &[u8; 32], account: &[u8; 32], frozen: bool) {
        set_status(sub_id, account, STATUS_FROZEN, CAP_FREEZE, "freeze", frozen);
    }

    pub fn asset_allow(sub_id: &[u8; 32], account: &[u8; 32], allowed: bool) {
        set_status(
            sub_id,
            account,
            STATUS_ALLOWED,
            CAP_ALLOWLIST,
            "allowlist",
            allowed,
        );
    }

    pub fn asset_pause(sub_id: &[u8; 32], paused: bool) {
        let (_, asset) = own_asset(sub_id);
        let result = with(|m| {
            let Some(info) = m.ledger.assets.get_mut(&asset) else {
                return Err("asset does not exist");
            };
            if paused && info.capabilities & CAP_PAUSE == 0 {
                return Err("asset does not have the pause capability");
            }
            info.paused = paused;
            Ok(())
        });
        if let Err(reason) = result {
            trap(reason);
        }
    }

    pub fn asset_renounce(sub_id: &[u8; 32], capabilities: u8) {
        let (_, asset) = own_asset(sub_id);
        if capabilities & !CAPABILITIES != 0 {
            trap("undefined capability bits");
        }
        with(|m| {
            if let Some(info) = m.ledger.assets.get_mut(&asset) {
                info.capabilities &= !capabilities;
                info.paused = info.paused && info.capabilities & CAP_PAUSE != 0;
            }
        });
    }

    pub fn asset_policy(asset: &[u8; 32]) -> i64 {
        with(|m| match m.ledger.assets.get(asset) {
            None => -1,
            Some(info) if info.paused => i64::from(info.capabilities) | 0x100,
            Some(info) => i64::from(info.capabilities),
        })
    }

    pub fn asset_status(asset: &[u8; 32], account: &[u8; 32]) -> i32 {
        i32::from(with(|m| {
            m.ledger
                .statuses
                .get(&(*asset, *account))
                .copied()
                .unwrap_or(0)
        }))
    }

    pub fn call_funds() -> Vec<u8> {
        encode(&frame().funds)
    }

    pub fn native_tx() -> Option<Vec<u8>> {
        with(|m| m.native_tx.as_ref().map(encode))
    }

    fn decode_funds(funds: &[u8]) -> Vec<Funds> {
        if funds.is_empty() {
            return Vec::new();
        }
        let funds: Vec<Funds> = crate::decode(funds).unwrap_or_else(|_| trap("malformed funds"));
        if let Err(reason) = check_funds(&funds) {
            trap(reason);
        }
        funds
    }

    pub fn call_contract(contract: &[u8; 32], input: &[u8], funds: &[u8]) -> Vec<u8> {
        check_len(input.len(), MAX_OP_BYTES, "call input too long");
        let funds = decode_funds(funds);
        let me = frame().self_id;
        invoke(me, *contract, input, false, funds)
    }

    pub fn try_call_contract(
        contract: &[u8; 32],
        input: &[u8],
        funds: &[u8],
        _gas: u64,
    ) -> Vec<u8> {
        // The caller's own errors: not catchable
        check_len(input.len(), MAX_OP_BYTES, "call input too long");
        let funds = decode_funds(funds);
        match decode_call_input(input) {
            Some(("init", _)) => trap("init can only be called at deployment"),
            Some(_) => {}
            None => trap("malformed call input"),
        }
        let me = frame().self_id;
        let (snapshot, depth) = with(|m| (m.ledger.clone(), m.frames.len()));
        let result = catch_unwind(AssertUnwindSafe(|| {
            invoke(me, *contract, input, false, funds)
        }));
        let (status, data) = match result {
            Ok(data) => (0u8, data),
            Err(payload) => {
                with(|m| {
                    m.ledger = snapshot;
                    m.frames.truncate(depth);
                });
                match failure_from_panic(payload) {
                    Failure::Revert { reason, .. } => (1, reason),
                    Failure::Trap(_) => (2, Vec::new()),
                }
            }
        };
        encode(&(status, data))
    }

    pub fn delegate_call(contract: &[u8; 32], input: &[u8]) -> Vec<u8> {
        check_len(input.len(), MAX_OP_BYTES, "call input too long");
        let current = frame();
        run_code(*contract, current, input, false, false)
    }

    pub fn emit_event(data: &[u8]) {
        check_len(data.len(), MAX_EVENT_BYTES, "event too long");
        let me = frame().self_id;
        with(|m| {
            m.ledger.events.push(Event {
                contract: me,
                data: data.to_vec(),
            })
        });
    }

    pub fn sha256(data: &[u8]) -> [u8; 32] {
        use sha2::Digest;
        sha2::Sha256::digest(data).into()
    }

    pub fn verify_schnorr(pubkey: &[u8; 32], msg: &[u8; 32], sig: &[u8; 64]) -> i32 {
        use secp256k1::{schnorr, Message, Secp256k1, XOnlyPublicKey};
        let (Ok(pubkey), Ok(sig)) = (
            XOnlyPublicKey::from_slice(pubkey),
            schnorr::Signature::from_slice(sig),
        ) else {
            return 0;
        };
        let valid = Secp256k1::verification_only()
            .verify_schnorr(&sig, &Message::from_digest(*msg), &pubkey)
            .is_ok();
        i32::from(valid)
    }
}

// ---------------------------------------------------------------------------
// Calls and operations.

/// Pops the call frame when leaving the contract (including on unwind).
struct FrameGuard;

impl Drop for FrameGuard {
    fn drop(&mut self) {
        with(|m| m.frames.pop());
    }
}

/// Calls contract `target` as `caller` (raw input `input`), first transferring the attached funds from `caller` to
/// `target`; returns the data in `Ok`, unwinds on failure.
fn invoke(
    caller: [u8; 32],
    target: [u8; 32],
    input: &[u8],
    allow_init: bool,
    funds: Vec<Funds>,
) -> Vec<u8> {
    let frame = Frame {
        caller,
        self_id: target,
        funds,
    };
    run_code(target, frame, input, allow_init, true)
}

/// Runs the dispatch function of contract `code_of` in call frame `frame` (the same contract for a normal call,
/// different for a delegate call); with `transfer`, the attached funds are transferred from `frame.caller` to
/// `frame.self_id` before entering (a delegate call keeps the current frame, and the funds arrived long ago).
fn run_code(
    code_of: [u8; 32],
    frame: Frame,
    input: &[u8],
    allow_init: bool,
    transfer: bool,
) -> Vec<u8> {
    let Some((method, _)) = decode_call_input(input) else {
        host::trap("malformed call input")
    };
    if method == "init" && !allow_init {
        host::trap("init can only be called at deployment");
    }
    let (dispatch, depth_ok) = with(|m| {
        (
            m.ledger.contracts.get(&code_of).cloned(),
            m.frames.len() < m.max_call_depth,
        )
    });
    let Some(dispatch) = dispatch else {
        host::trap("no contract with this id")
    };
    if !depth_ok {
        host::trap("call depth exceeded");
    }
    if transfer {
        transfer_funds(&frame.caller, &frame.self_id, &frame.funds);
    }
    let target = frame.self_id;
    with(|m| m.frames.push(frame));
    let guard = FrameGuard;
    let output = crate::entry::run(input, |method, args| dispatch(method, args));
    drop(guard);
    // The VM's checks on the output (ABI §2 item 4).
    if output.len() > MAX_OUTPUT_BYTES {
        host::trap("output too long");
    }
    match borsh::from_slice::<Result<Vec<u8>, Vec<u8>>>(&output) {
        Ok(Ok(data)) => data,
        Ok(Err(reason)) => fail(Failure::Revert {
            contract: target,
            reason,
        }),
        Err(_) => host::trap("malformed output"),
    }
}

fn failure_from_panic(payload: Box<dyn Any + Send>) -> Failure {
    let payload = match payload.downcast::<Failure>() {
        Ok(failure) => return *failure,
        Err(payload) => payload,
    };
    let message = if let Some(s) = payload.downcast_ref::<&'static str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        String::from("unknown panic payload")
    };
    Failure::Trap(format!("panic: {message}"))
}

/// Executes one operation: on failure rolls back the ledger and returns the failure reason.
fn operation(
    caller: [u8; 32],
    target: [u8; 32],
    input: &[u8],
    allow_init: bool,
    funds: &[Funds],
) -> Result<Vec<u8>, Failure> {
    let (snapshot, base_depth) = with(|m| (m.ledger.clone(), m.frames.len()));
    let result = catch_unwind(AssertUnwindSafe(|| {
        invoke(caller, target, input, allow_init, funds.to_vec())
    }));
    with(|m| m.frames.truncate(base_depth));
    result.map_err(|payload| {
        with(|m| m.ledger = snapshot);
        failure_from_panic(payload)
    })
}

/// Clears all mock state and restores the default parameters.
pub fn reset() {
    with(|m| *m = Mock::default());
}

/// Sets the fast-lane block height.
pub fn set_block_height(height: u64) {
    with(|m| m.block_height = height);
}

/// Sets the block timestamp (`block_timestamp`).
pub fn set_block_timestamp(timestamp: u64) {
    with(|m| m.block_timestamp = timestamp);
}

/// Sets the chain ID (default 1001, devnet).
pub fn set_chain_id(chain_id: u32) {
    with(|m| m.chain_id = chain_id);
}

/// Sets the return value of `gas_left` (the mock host does not meter gas).
pub fn set_gas_left(gas: u64) {
    with(|m| m.gas_left = gas);
}

/// Registers on-chain code `code` (keyed by `sha256(code)`, with dispatch function `dispatch`) for use by
/// [`crate::create_contract`]; returns the code hash. [`deploy`] registers too.
pub fn register_code(
    code: &[u8],
    dispatch: impl Fn(&str, &[u8]) -> CallResult + 'static,
) -> [u8; 32] {
    let code_hash = crate::sha256(code);
    let dispatch: Dispatch = Rc::new(dispatch);
    with(|m| m.codes.insert(code_hash, dispatch));
    code_hash
}

/// Sets the BTC height anchored by the current block.
pub fn set_btc_anchor_height(height: u32) {
    with(|m| m.btc_anchor_height = height);
}

/// Sets the BTC block header at `height`.
pub fn set_btc_header(height: u32, header: [u8; 80]) {
    with(|m| m.btc_headers.insert(height, header));
}

/// Sets `BTC_QUERY_DEPTH` (default 6).
pub fn set_btc_query_depth(depth: u32) {
    with(|m| m.btc_query_depth = depth);
}

/// Sets `MAX_CALL_DEPTH` (default 8).
pub fn set_max_call_depth(depth: usize) {
    with(|m| m.max_call_depth = depth);
}

/// Sets the [`crate::native_tx`] read by subsequent operations: `Some` simulates a path-C native op (carried by this
/// BTC transaction), `None` (the default) simulates a path-B op (D-078).
pub fn set_native_tx(tx: Option<NativeTx>) {
    with(|m| m.native_tx = tx);
}

/// Registers an existing asset (e.g. KSA written at genesis) with a total supply of 0, its name as its symbol, not
/// paused, and no capabilities when its issuer is 32 zero bytes (like KSA), otherwise `CAP_MINT` (like an asset first
/// issued by `asset_issue`). When it already exists, the name, symbol, decimals and issuer are overwritten.
pub fn register_asset(asset: [u8; 32], name: &str, decimals: u8, issuer: [u8; 32]) {
    with(|m| {
        let total_supply = m
            .ledger
            .assets
            .get(&asset)
            .map_or(0, |info| info.total_supply);
        m.ledger.assets.insert(
            asset,
            AssetInfo {
                name: name.to_string(),
                symbol: name.to_string(),
                decimals,
                total_supply,
                issuer,
                capabilities: if issuer == [0u8; 32] { 0 } else { CAP_MINT },
                paused: false,
            },
        );
    });
}

/// Sets the capabilities and pause state of the registered asset `asset` (D-121), e.g. to test how a contract handles
/// an asset that can freeze it; traps when the asset is unknown.
pub fn set_asset_policy(asset: [u8; 32], capabilities: u8, paused: bool) {
    let known = with(|m| match m.ledger.assets.get_mut(&asset) {
        Some(info) => {
            info.capabilities = capabilities;
            info.paused = paused;
            true
        }
        None => false,
    });
    if !known {
        host::trap("unknown asset");
    }
}

/// Sets the status flags (`STATUS_*` bits) of `account` for `asset` (D-121) as its issuing contract would; 0 clears
/// them.
pub fn set_account_status(asset: [u8; 32], account: [u8; 32], status: u8) {
    with(|m| {
        if status == 0 {
            m.ledger.statuses.remove(&(asset, account));
        } else {
            m.ledger.statuses.insert((asset, account), status);
        }
    });
}

/// The status flags recorded for `account` and `asset` (D-121); 0 when it has none.
pub fn account_status(asset: &[u8; 32], account: &[u8; 32]) -> u8 {
    with(|m| m.ledger.statuses.get(&(*asset, *account)).copied()).unwrap_or(0)
}

/// Test funding: adds `amount` of the registered asset `asset` to `account` (the total supply increases accordingly).
pub fn mint(asset: [u8; 32], account: [u8; 32], amount: u128) -> Result<(), Failure> {
    with(|m| {
        let info = m
            .ledger
            .assets
            .get_mut(&asset)
            .ok_or_else(|| Failure::Trap("unknown asset".to_string()))?;
        let new_supply = info
            .total_supply
            .checked_add(amount)
            .ok_or_else(|| Failure::Trap("total supply overflow".to_string()))?;
        info.total_supply = new_supply;
        credit(&mut m.ledger, asset, account, amount)
            .ok_or_else(|| Failure::Trap("balance overflow".to_string()))
    })
}

/// The `asset` balance of `account`.
pub fn balance(asset: &[u8; 32], account: &[u8; 32]) -> u128 {
    with(|m| m.ledger.balances.get(&(*asset, *account)).copied()).unwrap_or(0)
}

/// Asset metadata.
pub fn asset_info(asset: &[u8; 32]) -> Option<AssetInfo> {
    with(|m| m.ledger.assets.get(asset).cloned())
}

/// The value of `key` in the storage of contract `contract`.
pub fn storage_value(contract: &[u8; 32], key: &[u8]) -> Option<Vec<u8>> {
    with(|m| m.ledger.storage.get(&(*contract, key.to_vec())).cloned())
}

/// All events recorded so far (by successful operations).
pub fn events() -> Vec<Event> {
    with(|m| m.ledger.events.clone())
}

/// Clears the event log.
pub fn clear_events() {
    with(|m| m.ledger.events.clear());
}

/// Whether a contract is registered at `contract`.
pub fn contract_exists(contract: &[u8; 32]) -> bool {
    with(|m| m.ledger.contracts.contains_key(contract))
}

/// Registers a dispatch function at `contract` (without calling `init`), usable for stub contracts. Replaces an
/// existing one.
pub fn register(contract: [u8; 32], dispatch: impl Fn(&str, &[u8]) -> CallResult + 'static) {
    let dispatch: Dispatch = Rc::new(dispatch);
    with(|m| m.ledger.contracts.insert(contract, dispatch));
}

/// Deploys: the contract ID is computed per ABI §6 from `deployer`, `nonce` and `sha256(code)`; after registering the
/// dispatch function, `init(init_args)` runs with `deployer` as the caller; when `init` fails the deployment is undone
/// and rolled back.
pub fn deploy(
    deployer: [u8; 32],
    nonce: u64,
    code: &[u8],
    dispatch: impl Fn(&str, &[u8]) -> CallResult + 'static,
    init_args: &[u8],
) -> Result<[u8; 32], Failure> {
    deploy_with_funds(deployer, nonce, code, dispatch, init_args, &[])
}

/// Same as [`deploy`], and transfers `funds` from `deployer` to the new contract before running `init` (D-073).
pub fn deploy_with_funds(
    deployer: [u8; 32],
    nonce: u64,
    code: &[u8],
    dispatch: impl Fn(&str, &[u8]) -> CallResult + 'static,
    init_args: &[u8],
    funds: &[Funds],
) -> Result<[u8; 32], Failure> {
    let id = crate::contract_id(&deployer, nonce, &crate::sha256(code));
    if contract_exists(&id) {
        return Err(Failure::Trap("contract already exists".to_string()));
    }
    let dispatch: Dispatch = Rc::new(dispatch);
    let code_hash = crate::sha256(code);
    with(|m| m.codes.insert(code_hash, Rc::clone(&dispatch)));
    with(|m| m.ledger.contracts.insert(id, dispatch));
    match operation(deployer, id, &encode(&("init", init_args)), true, funds) {
        Ok(_) => Ok(id),
        Err(failure) => {
            with(|m| m.ledger.contracts.remove(&id));
            Err(failure)
        }
    }
}

/// A top-level call operation: `caller` (the VmOp's signing account) calls `contract.method(args)`.
///
/// On success returns the data in `Ok`; when any level fails, rolls back all state changes and events and returns the
/// failure reason. The method name `init` can only be called at deployment and always fails here.
pub fn call(
    caller: [u8; 32],
    contract: [u8; 32],
    method: &str,
    args: &[u8],
) -> Result<Vec<u8>, Failure> {
    call_with_funds(caller, contract, method, args, &[])
}

/// Same as [`call`], and first transfers `funds` from `caller` to the contract (the attached funds of a top-level
/// `Call`, D-073); rolled back together on failure.
pub fn call_with_funds(
    caller: [u8; 32],
    contract: [u8; 32],
    method: &str,
    args: &[u8],
    funds: &[Funds],
) -> Result<Vec<u8>, Failure> {
    operation(caller, contract, &encode(&(method, args)), false, funds)
}

/// Runs `f` directly in the context of `caller` calling `self_id` (no rollback, traps not caught), for testing
/// contract internals directly.
pub fn with_context<R>(caller: [u8; 32], self_id: [u8; 32], f: impl FnOnce() -> R) -> R {
    with(|m| {
        m.frames.push(Frame {
            caller,
            self_id,
            funds: Vec::new(),
        })
    });
    let guard = FrameGuard;
    let result = f();
    drop(guard);
    result
}
