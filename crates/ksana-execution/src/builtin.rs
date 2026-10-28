//! Built-in modules (native Rust implementation, not through WASM, SPEC §9.5): Account (nonce, arrears) and Asset (KSA
//! and contract assets).

use borsh::{BorshDeserialize, BorshSerialize};
use ksana_primitives::asset::{
    check_meta, CAPABILITIES, CAP_ALLOWLIST, CAP_FREEZE, CAP_MINT, CAP_PAUSE, DEFAULT_CAPABILITIES,
    STATUS_ALLOWED, STATUS_FROZEN,
};
use ksana_primitives::hash::sha256;
use ksana_vm::AssetMetaInput;

use crate::keys::{self, AccountState, AssetMeta};
use crate::state::Overlay;

/// Classification of errors during execution.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Fault {
    /// Deterministic op failure: this op is rolled back, the block remains valid.
    Failed(String),
    /// Consensus rule violation: the whole block is invalid.
    Invalid(String),
    /// Storage corruption or internal error: abort processing, unrelated to the block contents.
    Fatal(String),
}

fn fatal(e: impl std::fmt::Display) -> Fault {
    Fault::Fatal(e.to_string())
}

/// Reads and decodes a state value; a decode failure means the state is corrupted (fatal).
pub fn read<T: BorshDeserialize>(ov: &Overlay, key: &[u8]) -> Result<Option<T>, Fault> {
    match ov.get(key).map_err(Fault::Fatal)? {
        None => Ok(None),
        Some(bytes) => borsh::from_slice(&bytes)
            .map(Some)
            .map_err(|e| Fault::Fatal(format!("state value corrupted: {e}"))),
    }
}

/// Encodes and writes a state value.
pub fn write<T: BorshSerialize>(ov: &mut Overlay, key: Vec<u8>, value: &T) -> Result<(), Fault> {
    ov.set(key, borsh::to_vec(value).map_err(fatal)?);
    Ok(())
}

/// Account state; the default when absent (nonce 0, no arrears).
pub fn account_state(ov: &Overlay, account: &[u8; 32]) -> Result<AccountState, Fault> {
    Ok(read(ov, &keys::account(account))?.unwrap_or_default())
}

/// Writes the account state.
pub fn set_account_state(
    ov: &mut Overlay,
    account: &[u8; 32],
    st: &AccountState,
) -> Result<(), Fault> {
    write(ov, keys::account(account), st)
}

/// Asset balance; 0 when absent.
pub fn balance(ov: &Overlay, asset: &[u8; 32], account: &[u8; 32]) -> Result<u128, Fault> {
    Ok(read(ov, &keys::balance(asset, account))?.unwrap_or(0))
}

/// Writes a balance; deletes the key when it is 0 (canonical representation, D-019).
pub fn set_balance(
    ov: &mut Overlay,
    asset: &[u8; 32],
    account: &[u8; 32],
    amount: u128,
) -> Result<(), Fault> {
    let key = keys::balance(asset, account);
    if amount == 0 {
        ov.delete(key);
        Ok(())
    } else {
        write(ov, key, &amount)
    }
}

/// Asset metadata.
pub fn asset_meta(ov: &Overlay, asset: &[u8; 32]) -> Result<Option<AssetMeta>, Fault> {
    read(ov, &keys::asset(asset))
}

/// The status flags of `account` for `asset` (D-121); 0 when it has none.
pub fn asset_status(ov: &Overlay, asset: &[u8; 32], account: &[u8; 32]) -> Result<u8, Fault> {
    Ok(read(ov, &keys::asset_status(asset, account))?.unwrap_or(0))
}

/// Why an asset refuses an account as a sender or a recipient (D-121).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Bar {
    /// The account is frozen and the asset has `CAP_FREEZE`.
    Frozen,
    /// The asset has `CAP_ALLOWLIST` and the account is not allowed.
    NotAllowed,
}

/// Whether the asset of `meta` refuses `account`, whose status flags are `status`, as a sender or a recipient
/// (D-121). A flag counts only while the asset has the matching capability, and the issuing contract is never refused.
pub fn bar(meta: &AssetMeta, account: &[u8; 32], status: u8) -> Option<Bar> {
    if *account == meta.issuer {
        return None;
    }
    if meta.capabilities & CAP_FREEZE != 0 && status & STATUS_FROZEN != 0 {
        return Some(Bar::Frozen);
    }
    if meta.capabilities & CAP_ALLOWLIST != 0 && status & STATUS_ALLOWED == 0 {
        return Some(Bar::NotAllowed);
    }
    None
}

/// Whether the asset of `meta` lets `account`, whose status flags are `status`, send or burn it now (D-121): the
/// account is not barred, and the asset is not paused unless the account is its issuing contract. The same rules as
/// [`transfer`] and [`burn`] apply, for the RPC.
pub fn can_send(meta: &AssetMeta, account: &[u8; 32], status: u8) -> bool {
    bar(meta, account, status).is_none() && (!meta.paused || *account == meta.issuer)
}

/// Whether the asset of `meta` lets `account`, whose status flags are `status`, receive it now (D-121); the same rules
/// as [`transfer`] and [`issue`] apply, for the RPC.
pub fn can_receive(meta: &AssetMeta, account: &[u8; 32], status: u8) -> bool {
    bar(meta, account, status).is_none()
}

/// Refuses `account` as the `side` ("sender" / "recipient") of a movement of `asset` when the asset's rules bar it
/// (D-121). The account's status is read only for an asset with `CAP_FREEZE` or `CAP_ALLOWLIST`.
fn check_party(
    ov: &Overlay,
    asset: &[u8; 32],
    meta: &AssetMeta,
    account: &[u8; 32],
    side: &str,
) -> Result<(), Fault> {
    if meta.capabilities & (CAP_FREEZE | CAP_ALLOWLIST) == 0 || *account == meta.issuer {
        return Ok(());
    }
    match bar(meta, account, asset_status(ov, asset, account)?) {
        None => Ok(()),
        Some(Bar::Frozen) => Err(Fault::Failed(format!(
            "rejected: {side} is frozen for this asset"
        ))),
        Some(Bar::NotAllowed) => Err(Fault::Failed(format!(
            "rejected: {side} is not on this asset's allowlist"
        ))),
    }
}

/// Refuses a movement out of `from` while the asset is paused, unless `from` is its issuing contract (D-121).
fn check_paused(meta: &AssetMeta, from: &[u8; 32]) -> Result<(), Fault> {
    if meta.paused && *from != meta.issuer {
        return Err(Fault::Failed("rejected: asset is paused".into()));
    }
    Ok(())
}

/// Transfer. A failed op when the asset does not exist, its rules refuse the movement (D-121) or the balance is
/// insufficient.
pub fn transfer(
    ov: &mut Overlay,
    asset: &[u8; 32],
    from: &[u8; 32],
    to: &[u8; 32],
    amount: u128,
) -> Result<(), Fault> {
    let Some(meta) = asset_meta(ov, asset)? else {
        return Err(Fault::Failed("asset does not exist".into()));
    };
    check_paused(&meta, from)?;
    check_party(ov, asset, &meta, from, "sender")?;
    check_party(ov, asset, &meta, to, "recipient")?;
    let from_balance = balance(ov, asset, from)?;
    let rest = from_balance
        .checked_sub(amount)
        .ok_or_else(|| Fault::Failed("insufficient balance".into()))?;
    if from == to {
        return Ok(());
    }
    let to_balance = balance(ov, asset, to)?
        .checked_add(amount)
        .ok_or_else(|| Fault::Failed("balance overflow".into()))?;
    set_balance(ov, asset, from, rest)?;
    set_balance(ov, asset, to, to_balance)
}

/// The metadata of an asset the `issuer` contract creates with `capabilities` (D-121); a failed op when `meta` breaks
/// the limits of `ksana_primitives::asset::check_meta`.
fn new_meta(
    issuer: &[u8; 32],
    meta: &AssetMetaInput,
    capabilities: u8,
) -> Result<AssetMeta, Fault> {
    check_meta(&meta.name, &meta.symbol, meta.decimals).map_err(|e| Fault::Failed(e.into()))?;
    Ok(AssetMeta {
        name: meta.name.clone(),
        symbol: meta.symbol.clone(),
        decimals: meta.decimals,
        total_supply: 0,
        issuer: *issuer,
        capabilities,
        paused: false,
    })
}

/// Adds `amount` to the total supply of the asset of `meta` and to the balance of `to`, writing the metadata.
fn mint(
    ov: &mut Overlay,
    asset: &[u8; 32],
    meta: AssetMeta,
    to: &[u8; 32],
    amount: u128,
) -> Result<(), Fault> {
    let total_supply = meta
        .total_supply
        .checked_add(amount)
        .ok_or_else(|| Fault::Failed("asset total supply overflow".into()))?;
    let to_balance = balance(ov, asset, to)?
        .checked_add(amount)
        .ok_or_else(|| Fault::Failed("balance overflow".into()))?;
    write(
        ov,
        keys::asset(asset),
        &AssetMeta {
            total_supply,
            ..meta
        },
    )?;
    set_balance(ov, asset, to, to_balance)
}

/// A contract issues an asset (docs/ABI.md section 3 `asset_issue`). The first issuance creates the asset with
/// `CAP_MINT` alone; afterwards the name, symbol and decimals must match and the asset must still have `CAP_MINT`
/// (D-121). The recipient must pass the asset's rules; a pause does not stop the issuing contract.
pub fn issue(
    ov: &mut Overlay,
    asset: &[u8; 32],
    issuer: &[u8; 32],
    meta: &AssetMetaInput,
    to: &[u8; 32],
    amount: u128,
) -> Result<(), Fault> {
    let recorded = match asset_meta(ov, asset)? {
        None => new_meta(issuer, meta, DEFAULT_CAPABILITIES)?,
        Some(m) => {
            if m.name != meta.name
                || m.symbol != meta.symbol
                || m.decimals != meta.decimals
                || m.issuer != *issuer
            {
                return Err(Fault::Failed(
                    "asset metadata does not match the recorded one".into(),
                ));
            }
            if m.capabilities & CAP_MINT == 0 {
                return Err(Fault::Failed("rejected: asset supply is fixed".into()));
            }
            m
        }
    };
    check_party(ov, asset, &recorded, to, "recipient")?;
    mint(ov, asset, recorded, to, amount)
}

/// A contract creates an asset with `capabilities` and issues `amount` of it to `to` (`asset_create`, D-121). A failed
/// op when the asset exists, `meta` breaks the limits, `capabilities` has a reserved bit or the asset's rules refuse
/// `to` (with `CAP_ALLOWLIST` only the issuing contract can receive it before any account is allowed).
pub fn create(
    ov: &mut Overlay,
    asset: &[u8; 32],
    issuer: &[u8; 32],
    meta: &AssetMetaInput,
    capabilities: u8,
    to: &[u8; 32],
    amount: u128,
) -> Result<(), Fault> {
    if asset_meta(ov, asset)?.is_some() {
        return Err(Fault::Failed("asset already exists".into()));
    }
    if capabilities & !CAPABILITIES != 0 {
        return Err(Fault::Failed("undefined capability bits".into()));
    }
    let recorded = new_meta(issuer, meta, capabilities)?;
    check_party(ov, asset, &recorded, to, "recipient")?;
    mint(ov, asset, recorded, to, amount)
}

/// The metadata of `asset`, which must exist and be issued by `issuer` (the asset management functions of D-121).
fn issued_by(ov: &Overlay, asset: &[u8; 32], issuer: &[u8; 32]) -> Result<AssetMeta, Fault> {
    match asset_meta(ov, asset)? {
        None => Err(Fault::Failed("asset does not exist".into())),
        Some(m) if m.issuer != *issuer => Err(Fault::Failed(
            "only the issuing contract manages an asset".into(),
        )),
        Some(m) => Ok(m),
    }
}

/// The issuing contract sets (`on`) or clears the status flag `flag` (`STATUS_FROZEN` / `STATUS_ALLOWED`) of `account`
/// for its asset (`asset_freeze` / `asset_allow`, D-121). Setting a flag needs the matching capability, clearing one
/// never does; the issuing contract itself has no status. The key is deleted when no flag is left (D-019).
pub fn set_status(
    ov: &mut Overlay,
    asset: &[u8; 32],
    issuer: &[u8; 32],
    account: &[u8; 32],
    flag: u8,
    on: bool,
) -> Result<(), Fault> {
    let meta = issued_by(ov, asset, issuer)?;
    let (needs, name) = match flag {
        STATUS_FROZEN => (CAP_FREEZE, "freeze"),
        STATUS_ALLOWED => (CAP_ALLOWLIST, "allowlist"),
        _ => return Err(Fault::Failed("undefined status flag".into())),
    };
    if *account == meta.issuer {
        return Err(Fault::Failed(
            "the issuing contract has no status for its own asset".into(),
        ));
    }
    if on && meta.capabilities & needs == 0 {
        return Err(Fault::Failed(format!(
            "asset does not have the {name} capability"
        )));
    }
    let old = asset_status(ov, asset, account)?;
    let new = if on { old | flag } else { old & !flag };
    let key = keys::asset_status(asset, account);
    if new == 0 {
        ov.delete(key);
        Ok(())
    } else {
        write(ov, key, &new)
    }
}

/// The issuing contract pauses or resumes its asset (`asset_pause`, D-121); pausing needs `CAP_PAUSE`, resuming never
/// does.
pub fn set_paused(
    ov: &mut Overlay,
    asset: &[u8; 32],
    issuer: &[u8; 32],
    paused: bool,
) -> Result<(), Fault> {
    let meta = issued_by(ov, asset, issuer)?;
    if paused && meta.capabilities & CAP_PAUSE == 0 {
        return Err(Fault::Failed(
            "asset does not have the pause capability".into(),
        ));
    }
    write(ov, keys::asset(asset), &AssetMeta { paused, ..meta })
}

/// The issuing contract gives up `capabilities` of its asset for good (`asset_renounce`, D-121): capabilities it no
/// longer has are ignored, and giving up `CAP_PAUSE` resumes a paused asset.
pub fn renounce(
    ov: &mut Overlay,
    asset: &[u8; 32],
    issuer: &[u8; 32],
    capabilities: u8,
) -> Result<(), Fault> {
    let meta = issued_by(ov, asset, issuer)?;
    if capabilities & !CAPABILITIES != 0 {
        return Err(Fault::Failed("undefined capability bits".into()));
    }
    let left = meta.capabilities & !capabilities;
    let paused = meta.paused && left & CAP_PAUSE != 0;
    write(
        ov,
        keys::asset(asset),
        &AssetMeta {
            capabilities: left,
            paused,
            ..meta
        },
    )
}

/// Burns `amount` of `asset` held by `from`; the total supply decreases accordingly (`asset_burn`, D-073). A failed op
/// when the asset does not exist, its rules refuse `from` as a sender (D-121) or the balance is insufficient.
pub fn burn(
    ov: &mut Overlay,
    asset: &[u8; 32],
    from: &[u8; 32],
    amount: u128,
) -> Result<(), Fault> {
    let Some(meta) = asset_meta(ov, asset)? else {
        return Err(Fault::Failed("asset does not exist".into()));
    };
    check_paused(&meta, from)?;
    check_party(ov, asset, &meta, from, "sender")?;
    let rest = balance(ov, asset, from)?
        .checked_sub(amount)
        .ok_or_else(|| Fault::Failed("insufficient balance".into()))?;
    // the sum of the balances equals the total supply, so the total supply is not less than any single balance;
    // otherwise the state is corrupted
    let total_supply = meta
        .total_supply
        .checked_sub(amount)
        .ok_or_else(|| Fault::Fatal("asset total supply is less than a balance".into()))?;
    write(
        ov,
        keys::asset(asset),
        &AssetMeta {
            total_supply,
            ..meta
        },
    )?;
    set_balance(ov, asset, from, rest)
}

/// A contract created by a contract (`create_contract`, D-073): `sha256(creator ‖ salt ‖ code_hash)`. The preimage is
/// 96 bytes, a different length from the user-deployed [`contract_id`] (72 bytes), so the two can never coincide.
pub fn created_contract_id(creator: &[u8; 32], salt: &[u8; 32], code_hash: &[u8; 32]) -> [u8; 32] {
    let mut preimage = Vec::with_capacity(96);
    preimage.extend_from_slice(creator);
    preimage.extend_from_slice(salt);
    preimage.extend_from_slice(code_hash);
    sha256(&preimage)
}

/// `contract_id = sha256(deployer ‖ nonce (u64 little-endian) ‖ code_hash)` (SPEC §9.2, D-015).
pub fn contract_id(deployer: &[u8; 32], nonce: u64, code_hash: &[u8; 32]) -> [u8; 32] {
    let mut preimage = Vec::with_capacity(72);
    preimage.extend_from_slice(deployer);
    preimage.extend_from_slice(&nonce.to_le_bytes());
    preimage.extend_from_slice(code_hash);
    sha256(&preimage)
}

/// Contract asset ID: `sha256(issuing contract ‖ sub_id)` (D-015).
pub fn asset_id(issuer: &[u8; 32], sub_id: &[u8; 32]) -> [u8; 32] {
    let mut preimage = Vec::with_capacity(64);
    preimage.extend_from_slice(issuer);
    preimage.extend_from_slice(sub_id);
    sha256(&preimage)
}
