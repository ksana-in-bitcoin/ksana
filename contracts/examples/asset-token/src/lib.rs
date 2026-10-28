//! Example contract: a fixed-supply token issued as a VM built-in asset (SPEC §9.5, D-073).
//!
//! The `token` example keeps its balances in its own storage: wallets and the explorer cannot see them, and other
//! contracts move them through `approve` / `transfer_from`. This contract instead creates its asset with its whole
//! supply once, at deployment, with `asset_create`, so the token is an asset of the VM like KSA: `ksana_getAccount`
//! lists its balances and `ksana_getAsset` its name, symbol, decimals and supply, holders send it with the
//! `AssetTransfer` action and attach it to calls, and the AMM example trades it as an `Asset` side, with no approve
//! step.
//!
//! Arguments and return values are Borsh-encoded. The arguments are a tuple in the order of the table below (no
//! arguments is empty bytes):
//!
//! | Method | Arguments | Returns |
//! | --- | --- | --- |
//! | `init` (at deployment) | `(name: String, symbol: String, decimals: u8, supply: u128)` | `()` |
//! | `asset_id` | none | `[u8; 32]` |
//!
//! `init` creates the asset under `sub_id` 0 and issues `supply` base units (above 0) to the deployer, so the asset ID
//! is `sha256(contract ID ‖ [0; 32])` (ABI §6), known before the deployment since the contract ID is too. The host
//! records the name, symbol and decimals (a name of 1 to 64 bytes, a symbol of 1 to 16 bytes and at most 18 decimals,
//! or the deployment fails). The asset has none of the issuer capabilities of D-121, not even minting: the protocol
//! keeps the supply fixed, `ksana_getAsset` shows it with no capabilities, and only burns lower it.
//!
//! Event (Borsh): `("issue", asset_id: [u8; 32], to: [u8; 32], amount: u128)`.

#![cfg_attr(target_arch = "wasm32", no_std)]

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;

use ksana_contract_sdk::{self as sdk, storage, AssetMetaInput, CallResult};

sdk::entry!(dispatch);

/// The `sub_id` of the one asset this contract issues.
pub const SUB_ID: [u8; 32] = [0u8; 32];

const KEY_ASSET: &[u8] = b"asset";

const ERR_ALREADY_INITIALIZED: &[u8] = b"already initialized";
const ERR_NOT_INITIALIZED: &[u8] = b"not initialized";
const ERR_ZERO_SUPPLY: &[u8] = b"supply must be positive";

/// Dispatch function of the contract entry point.
pub fn dispatch(method: &str, args: &[u8]) -> CallResult {
    match method {
        "init" => {
            let (name, symbol, decimals, supply) = sdk::decode(args)?;
            init(name, symbol, decimals, supply)?;
            sdk::ok(&())
        }
        "asset_id" => {
            sdk::decode::<()>(args)?;
            sdk::ok(&asset_id()?)
        }
        _ => sdk::revert("unknown method"),
    }
}

/// `init(name, symbol, decimals, supply)`: creates the asset without capabilities and issues the whole supply to the
/// deployer.
pub fn init(name: String, symbol: String, decimals: u8, supply: u128) -> Result<(), Vec<u8>> {
    if storage::get(KEY_ASSET).is_some() {
        return Err(ERR_ALREADY_INITIALIZED.to_vec());
    }
    if supply == 0 {
        return Err(ERR_ZERO_SUPPLY.to_vec());
    }
    let owner = sdk::caller();
    let meta = AssetMetaInput {
        name,
        symbol,
        decimals,
    };
    let asset = sdk::asset_create(&SUB_ID, &meta, 0, &owner, supply);
    storage::write(KEY_ASSET, &asset);
    sdk::emit(&("issue", asset, owner, supply));
    Ok(())
}

/// `asset_id() -> sha256(this contract's ID ‖ SUB_ID)`.
pub fn asset_id() -> Result<[u8; 32], Vec<u8>> {
    storage::read(KEY_ASSET)?.ok_or_else(|| ERR_NOT_INITIALIZED.to_vec())
}
