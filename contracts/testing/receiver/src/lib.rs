//! Test contract: takes tokens by `transfer_and_call` (KS-20's `on_token_received`, KS-721's `on_nft_received`,
//! `docs/STANDARDS.md`) for the execution-layer tests (not an example of SPEC §9.5).
//!
//! Parameters and return values are Borsh-encoded; several parameters form a tuple in the order of the table below, no
//! parameters means empty bytes.
//!
//! | Method | Parameters | Returns |
//! | --- | --- | --- |
//! | `init` (at deployment) | none | `()` |
//! | `on_token_received` | `(from: [u8; 32], amount: u128, data: Vec<u8>)` | `Vec<u8>` (KS-20's `Bytes`): `(caller: [u8; 32], amount: u128)`; reverts with `refused` when `data` is `refuse` |
//! | `on_nft_received` | `(from: [u8; 32], token_id: [u8; 32], data: Vec<u8>)` | `Vec<u8>` (KS-721's `Bytes`): `(caller: [u8; 32], token_id: [u8; 32])`; the same refusal |
//! | `last` | none | `Option<(caller: [u8; 32], args: Vec<u8>, funds: Vec<Funds>)>`: what the last callback saw |
//!
//! A callback records its caller, its arguments and its attached funds (which stay in this contract) before it
//! answers.

#![cfg_attr(target_arch = "wasm32", no_std)]

extern crate alloc;

use alloc::vec::Vec;

use ksana_contract_sdk::{self as sdk, ks20, ks721, standard, storage, CallResult, Funds};

sdk::entry!(dispatch);

const KEY_LAST: &[u8] = b"last";

/// Dispatch function of the contract entry point.
pub fn dispatch(method: &str, args: &[u8]) -> CallResult {
    match method {
        "init" => sdk::ok(&()),
        "on_token_received" => {
            let received = ks20::received(args)?;
            record(args, &received.data)?;
            standard::reply(&(received.token, received.amount))
        }
        "on_nft_received" => {
            let received = ks721::received(args)?;
            record(args, &received.data)?;
            standard::reply(&(received.collection, received.token_id))
        }
        "last" => {
            sdk::decode::<()>(args)?;
            let last: Option<([u8; 32], Vec<u8>, Vec<Funds>)> = storage::read(KEY_LAST)?;
            sdk::ok(&last)
        }
        _ => sdk::revert("unknown method"),
    }
}

/// Records the callback, or refuses it when the sender's data says so.
fn record(args: &[u8], data: &[u8]) -> Result<(), Vec<u8>> {
    if data == b"refuse" {
        return Err(b"refused".to_vec());
    }
    storage::write(KEY_LAST, &(sdk::caller(), args, sdk::call_funds()));
    Ok(())
}
