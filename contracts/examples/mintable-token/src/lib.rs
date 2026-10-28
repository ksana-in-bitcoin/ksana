//! Example contract: a token issued as a VM built-in asset that its owner can mint, up to an optional cap, and its
//! holders can burn (SPEC §9.5, D-073).
//!
//! This contract extends the `asset-token` example, which issues a fixed supply once, at deployment. It issues its
//! asset with `asset_issue` under `sub_id` 0, so the asset ID is `sha256(contract ID ‖ [0; 32])` (ABI §6) and the
//! asset has the issuer capability to mint (D-121), and adds an owner who can issue more later, an optional cap on the
//! supply, burns by any holder and the hand-over of ownership. The token is an asset of the VM like KSA:
//! `ksana_getAccount` lists its balances and `ksana_getAsset` its name, symbol, decimals, total supply and
//! capabilities, holders send it with the built-in `AssetTransfer` action (there is no transfer method here) and attach
//! it to calls, and the AMM example trades it as an `Asset` side, with no approve step.
//!
//! Arguments and return values are Borsh-encoded. The arguments are a tuple in the order of the table below (a single
//! argument is just itself; no arguments is empty bytes):
//!
//! | Method | Arguments | Returns |
//! | --- | --- | --- |
//! | `init` (at deployment) | `(name: String, symbol: String, decimals: u8, cap: Option<u128>, initial_supply: u128)` | `()` |
//! | `mint` | `(to: [u8; 32], amount: u128)` | `u128` (the new supply) |
//! | `burn` | none, the token attached | `u128` (the new supply) |
//! | `set_owner` | `owner: Option<[u8; 32]>` | `()` |
//! | `info` | none | [`Info`] |
//! | `asset` | none | `[u8; 32]` (the asset ID) |
//! | `balance_of` | `account: [u8; 32]` | `u128` (the account's balance of the token) |
//!
//! Rules:
//!
//! - `init`: the name is 1 to [`MAX_NAME_BYTES`] bytes of UTF-8, the symbol 1 to [`MAX_SYMBOL_BYTES`], the decimals at
//!   most [`MAX_DECIMALS`], and a cap, when there is one, above 0 and not below `initial_supply`. The deployer (the
//!   caller of `init`) becomes the owner and receives the initial supply. An initial supply of 0 is allowed: the first
//!   issuance records the name, symbol and decimals even when it issues nothing, so the asset exists from the
//!   deployment on, with a supply of 0, and the owner mints later.
//! - `mint`: the owner only. Issues `amount` (above 0) to `to`, which is neither the zero account nor this contract
//!   (nothing here could move tokens out of it); the supply after it must stay within the cap.
//! - `burn`: any holder attaches the token, and nothing else, to the call; all of it is burned.
//! - `set_owner`: the owner only. Hands ownership to another account (neither the zero account nor this contract), or
//!   with `None` gives it up for good: the contract renounces the asset's capability to mint (`asset_renounce`,
//!   D-121), so the protocol itself refuses any further issuance, and from then on the supply can only go down.
//! - Every method but `burn` refuses attached funds, which would otherwise be stuck in this contract. Assets sent to
//!   this contract with `AssetTransfer` stay there for good too.
//!
//! Supply accounting: the contract counts `minted`, everything it ever issued (the initial supply included), and
//! `burned`, everything burned through its `burn`, and `supply = minted − burned`. A contract holding the token can also
//! burn its own holdings with the host function `asset_burn`, which this contract never sees, so the real total supply
//! is the asset's `total_supply` (`ksana_getAsset`) and `supply` is an upper bound of it: such burns only leave less
//! room under the cap. `minted` only grows, so all the mints of a token add up to at most `u128::MAX` base units,
//! whatever was burned.
//!
//! Storage: `meta` holds the name, symbol and decimals as the `AssetMetaInput` every issuance passes to the host
//! (written by `init`, so its presence means initialized), `owner` the owner (absent once ownership is given up),
//! `cap` the cap (absent without one), and `minted` and `burned` their u128 counts (absent while 0).
//!
//! Events (Borsh): `("mint", to: [u8; 32], amount: u128)` (also for the initial supply, when it is above 0),
//! `("burn", from: [u8; 32], amount: u128)` and `("owner", previous: Option<[u8; 32]>, owner: Option<[u8; 32]>)`
//! (`init` records `(None, Some(deployer))`).
//!
//! ```abi
//! struct Info { asset: AssetId, name: String, symbol: String, decimals: u8, owner: Option<Account>, cap: Option<u128>, supply: u128, minted: u128, burned: u128 }
//! init(name: String, symbol: String, decimals: u8, cap: Option<u128>, initial_supply: u128)   // issues the initial supply to the deployer, who becomes the owner
//! call mint(to: Account, amount: u128) -> u128                                // owner only; returns the new supply
//! payable call burn() -> u128                                                  // attach the token; burns all of it and returns the new supply
//! call set_owner(owner: Option<Account>)                                       // owner only; None gives up minting for good, the asset's capability too
//! view info() -> Info
//! view asset() -> AssetId
//! view balance_of(account: Account) -> u128                                    // the built-in asset balance of an account
//! event mint(to: Account, amount: u128)
//! event burn(from: Account, amount: u128)
//! event owner(previous: Option<Account>, owner: Option<Account>)
//! ```

#![cfg_attr(target_arch = "wasm32", no_std)]

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;

use ksana_contract_sdk::borsh::BorshSerialize;
use ksana_contract_sdk::{self as sdk, storage, AssetMetaInput, CallResult, Decode};

sdk::entry!(dispatch);

/// The `sub_id` of the one asset this contract issues.
pub const SUB_ID: [u8; 32] = [0u8; 32];
/// The longest name, in bytes of UTF-8.
pub const MAX_NAME_BYTES: usize = 32;
/// The longest symbol, in bytes of UTF-8 (the host's limit, D-121).
pub const MAX_SYMBOL_BYTES: usize = 16;
/// The most decimals a token may have.
pub const MAX_DECIMALS: u8 = 18;

const KEY_META: &[u8] = b"meta";
const KEY_OWNER: &[u8] = b"owner";
const KEY_CAP: &[u8] = b"cap";
const KEY_MINTED: &[u8] = b"minted";
const KEY_BURNED: &[u8] = b"burned";

const ERR_ALREADY_INITIALIZED: &[u8] = b"already initialized";
const ERR_NOT_INITIALIZED: &[u8] = b"not initialized";
const ERR_UNEXPECTED_FUNDS: &[u8] = b"unexpected attached funds";
const ERR_NAME: &[u8] = b"name must be 1 to 32 bytes";
const ERR_SYMBOL: &[u8] = b"symbol must be 1 to 16 bytes";
const ERR_DECIMALS: &[u8] = b"decimals must be at most 18";
const ERR_ZERO_CAP: &[u8] = b"cap must be positive";
const ERR_ABOVE_CAP: &[u8] = b"supply would exceed the cap";
const ERR_NOT_OWNER: &[u8] = b"only the owner";
const ERR_RENOUNCED: &[u8] = b"ownership has been renounced";
const ERR_ZERO_ACCOUNT: &[u8] = b"account must not be zero";
const ERR_SELF_ACCOUNT: &[u8] = b"account must not be this contract";
const ERR_ZERO_AMOUNT: &[u8] = b"amount must be positive";
const ERR_ATTACH_TOKEN: &[u8] = b"attach only this token";
const ERR_OVERFLOW: &[u8] = b"arithmetic overflow";

/// What `info` returns.
#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize)]
#[borsh(crate = "ksana_contract_sdk::borsh")]
pub struct Info {
    /// The asset ID: `sha256(this contract's ID ‖ SUB_ID)`.
    pub asset: [u8; 32],
    /// The asset's name.
    pub name: String,
    /// The asset's symbol.
    pub symbol: String,
    /// The asset's decimals.
    pub decimals: u8,
    /// The account that can mint; `None` once ownership is given up.
    pub owner: Option<[u8; 32]>,
    /// The most the supply may reach; `None` without a cap.
    pub cap: Option<u128>,
    /// `minted − burned`, an upper bound of the asset's total supply.
    pub supply: u128,
    /// Everything this contract ever issued, the initial supply included.
    pub minted: u128,
    /// Everything burned through `burn`.
    pub burned: u128,
}

impl Decode for Info {
    fn decode(input: &mut &[u8]) -> Option<Self> {
        Some(Self {
            asset: <[u8; 32]>::decode(input)?,
            name: String::decode(input)?,
            symbol: String::decode(input)?,
            decimals: u8::decode(input)?,
            owner: Option::<[u8; 32]>::decode(input)?,
            cap: Option::<u128>::decode(input)?,
            supply: u128::decode(input)?,
            minted: u128::decode(input)?,
            burned: u128::decode(input)?,
        })
    }
}

/// Dispatch function of the contract entry point.
pub fn dispatch(method: &str, args: &[u8]) -> CallResult {
    match method {
        "init" => {
            let (name, symbol, decimals, cap, initial_supply) = sdk::decode(args)?;
            init(name, symbol, decimals, cap, initial_supply)?;
            sdk::ok(&())
        }
        "mint" => {
            let (to, amount) = sdk::decode(args)?;
            sdk::ok(&mint(&to, amount)?)
        }
        "burn" => {
            sdk::decode::<()>(args)?;
            sdk::ok(&burn()?)
        }
        "set_owner" => {
            let owner = sdk::decode(args)?;
            set_owner(owner)?;
            sdk::ok(&())
        }
        "info" => {
            sdk::decode::<()>(args)?;
            sdk::ok(&info()?)
        }
        "asset" => {
            sdk::decode::<()>(args)?;
            sdk::ok(&asset()?)
        }
        "balance_of" => {
            let account = sdk::decode(args)?;
            sdk::ok(&balance_of(&account)?)
        }
        _ => sdk::revert("unknown method"),
    }
}

fn overflow() -> Vec<u8> {
    ERR_OVERFLOW.to_vec()
}

/// Reverts with `not initialized` before `init`.
fn check_initialized() -> Result<(), Vec<u8>> {
    match storage::get(KEY_META) {
        Some(_) => Ok(()),
        None => Err(ERR_NOT_INITIALIZED.to_vec()),
    }
}

/// The name, symbol and decimals recorded by `init`, as every issuance passes them to the host.
fn meta() -> Result<AssetMetaInput, Vec<u8>> {
    storage::read(KEY_META)?.ok_or_else(|| ERR_NOT_INITIALIZED.to_vec())
}

/// Refuses attached funds: a method that is not payable would leave them stuck in this contract.
fn no_funds() -> Result<(), Vec<u8>> {
    if sdk::call_funds().is_empty() {
        Ok(())
    } else {
        Err(ERR_UNEXPECTED_FUNDS.to_vec())
    }
}

/// This contract's asset ID.
fn own_asset() -> [u8; 32] {
    sdk::asset_id(&sdk::self_id(), &SUB_ID)
}

/// Reads a u128 count; 0 when absent.
fn read_amount(key: &[u8]) -> Result<u128, Vec<u8>> {
    Ok(storage::read::<u128>(key)?.unwrap_or(0))
}

/// `minted − burned`; every burn burned issued tokens, so this never goes below 0.
fn supply_of(minted: u128, burned: u128) -> Result<u128, Vec<u8>> {
    minted.checked_sub(burned).ok_or_else(overflow)
}

/// Checks that the caller is the owner, and returns it.
fn only_owner() -> Result<[u8; 32], Vec<u8>> {
    let owner: [u8; 32] = storage::read(KEY_OWNER)?.ok_or_else(|| ERR_RENOUNCED.to_vec())?;
    if sdk::caller() == owner {
        Ok(owner)
    } else {
        Err(ERR_NOT_OWNER.to_vec())
    }
}

/// An account that receives tokens or the owner role is neither the zero account nor this contract.
fn check_account(account: &[u8; 32]) -> Result<(), Vec<u8>> {
    if *account == [0u8; 32] {
        return Err(ERR_ZERO_ACCOUNT.to_vec());
    }
    if *account == sdk::self_id() {
        return Err(ERR_SELF_ACCOUNT.to_vec());
    }
    Ok(())
}

/// `init(name, symbol, decimals, cap, initial_supply)`: records the token, makes the deployer its owner and issues the
/// initial supply to the deployer.
pub fn init(
    name: String,
    symbol: String,
    decimals: u8,
    cap: Option<u128>,
    initial_supply: u128,
) -> Result<(), Vec<u8>> {
    if storage::get(KEY_META).is_some() {
        return Err(ERR_ALREADY_INITIALIZED.to_vec());
    }
    no_funds()?;
    if name.is_empty() || name.len() > MAX_NAME_BYTES {
        return Err(ERR_NAME.to_vec());
    }
    if symbol.is_empty() || symbol.len() > MAX_SYMBOL_BYTES {
        return Err(ERR_SYMBOL.to_vec());
    }
    if decimals > MAX_DECIMALS {
        return Err(ERR_DECIMALS.to_vec());
    }
    match cap {
        Some(0) => return Err(ERR_ZERO_CAP.to_vec()),
        Some(cap) if initial_supply > cap => return Err(ERR_ABOVE_CAP.to_vec()),
        _ => {}
    }
    let deployer = sdk::caller();
    check_account(&deployer)?;
    let meta = AssetMetaInput {
        name,
        symbol,
        decimals,
    };
    storage::write(KEY_META, &meta);
    storage::write(KEY_OWNER, &deployer);
    if let Some(cap) = cap {
        storage::write(KEY_CAP, &cap);
    }
    if initial_supply > 0 {
        storage::write(KEY_MINTED, &initial_supply);
    }
    // Issuing 0 still records the name, symbol and decimals: the asset exists from now on
    sdk::asset_issue(&SUB_ID, &meta, &deployer, initial_supply);
    if initial_supply > 0 {
        sdk::emit(&("mint", deployer, initial_supply));
    }
    sdk::emit(&("owner", None::<[u8; 32]>, Some(deployer)));
    Ok(())
}

/// `mint(to, amount) -> supply`: the owner issues `amount` more to `to`.
pub fn mint(to: &[u8; 32], amount: u128) -> Result<u128, Vec<u8>> {
    let meta = meta()?;
    no_funds()?;
    only_owner()?;
    check_account(to)?;
    if amount == 0 {
        return Err(ERR_ZERO_AMOUNT.to_vec());
    }
    let minted = read_amount(KEY_MINTED)?;
    let supply = supply_of(minted, read_amount(KEY_BURNED)?)?;
    let cap: Option<u128> = storage::read(KEY_CAP)?;
    // The supply never exceeds the cap, so the room left under it is never negative
    if cap.is_some_and(|cap| amount > cap.saturating_sub(supply)) {
        return Err(ERR_ABOVE_CAP.to_vec());
    }
    let minted = minted.checked_add(amount).ok_or_else(overflow)?;
    let supply = supply.checked_add(amount).ok_or_else(overflow)?;
    storage::write(KEY_MINTED, &minted);
    sdk::asset_issue(&SUB_ID, &meta, to, amount);
    sdk::emit(&("mint", *to, amount));
    Ok(supply)
}

/// `burn() -> supply`: burns all of this token attached to the call.
pub fn burn() -> Result<u128, Vec<u8>> {
    check_initialized()?;
    let asset = own_asset();
    let amount = match sdk::call_funds().as_slice() {
        [funds] if funds.asset == asset && funds.amount > 0 => funds.amount,
        _ => return Err(ERR_ATTACH_TOKEN.to_vec()),
    };
    let burned = read_amount(KEY_BURNED)?
        .checked_add(amount)
        .ok_or_else(overflow)?;
    let supply = supply_of(read_amount(KEY_MINTED)?, burned)?;
    storage::write(KEY_BURNED, &burned);
    sdk::asset_burn(&asset, amount);
    sdk::emit(&("burn", sdk::caller(), amount));
    Ok(supply)
}

/// `set_owner(owner)`: the owner hands ownership to `owner`, or gives it up for good with `None`, renouncing the
/// asset's capability to mint with it.
pub fn set_owner(owner: Option<[u8; 32]>) -> Result<(), Vec<u8>> {
    check_initialized()?;
    no_funds()?;
    let previous = only_owner()?;
    match &owner {
        Some(next) => {
            check_account(next)?;
            storage::write(KEY_OWNER, next);
        }
        None => {
            storage::remove(KEY_OWNER);
            sdk::asset_renounce(&SUB_ID, sdk::CAP_MINT);
        }
    }
    sdk::emit(&("owner", Some(previous), owner));
    Ok(())
}

/// `info() -> Info`.
pub fn info() -> Result<Info, Vec<u8>> {
    let AssetMetaInput {
        name,
        symbol,
        decimals,
    } = meta()?;
    no_funds()?;
    let minted = read_amount(KEY_MINTED)?;
    let burned = read_amount(KEY_BURNED)?;
    Ok(Info {
        asset: own_asset(),
        name,
        symbol,
        decimals,
        owner: storage::read(KEY_OWNER)?,
        cap: storage::read(KEY_CAP)?,
        supply: supply_of(minted, burned)?,
        minted,
        burned,
    })
}

/// `asset() -> sha256(this contract's ID ‖ SUB_ID)`.
pub fn asset() -> Result<[u8; 32], Vec<u8>> {
    check_initialized()?;
    no_funds()?;
    Ok(own_asset())
}

/// `balance_of(account)`: the account's balance of this token, as the VM keeps it.
pub fn balance_of(account: &[u8; 32]) -> Result<u128, Vec<u8>> {
    check_initialized()?;
    no_funds()?;
    Ok(sdk::asset_balance(&own_asset(), account))
}
