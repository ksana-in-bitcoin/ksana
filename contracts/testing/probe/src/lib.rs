//! Test contract: exposes the host functions as methods for the execution-layer tests (not an example of SPEC §9.5).
//!
//! Parameters and return values are Borsh-encoded; several parameters form a tuple in the order of the table below, no
//! parameters means empty bytes.
//!
//! | Method | Parameters | Returns |
//! | --- | --- | --- |
//! | `init` (at deployment) | `tag: Vec<u8>` (reverts when it is `b"fail"`) | `()`; records the funds attached at deployment |
//! | `noop` | none | `()` |
//! | `funds` | none | `(caller: [u8; 32], funds: Vec<Funds>)`: the caller and the funds attached to this call |
//! | `init_funds` | none | `Vec<Funds>`: the funds attached at deployment |
//! | `balance` | `(asset: [u8; 32], account: [u8; 32])` | `u128` |
//! | `send` | `(asset: [u8; 32], to: [u8; 32], amount: u128)` | `()`: transfers out of this contract's account |
//! | `issue` | `(sub_id: [u8; 32], amount: u128)` | the asset ID: issues this contract's asset (`PROBE`, symbol `PRB`, 0 decimals) to the caller |
//! | `issue_meta` | `(sub_id: [u8; 32], meta: AssetMetaInput, to: [u8; 32], amount: u128)` | the asset ID: `asset_issue` with the given metadata |
//! | `create_asset` | `(sub_id: [u8; 32], meta: AssetMetaInput, capabilities: u8, to: [u8; 32], amount: u128)` | the asset ID: `asset_create` (D-121) |
//! | `freeze` | `(sub_id: [u8; 32], account: [u8; 32], frozen: bool)` | `()`: `asset_freeze` |
//! | `allow` | `(sub_id: [u8; 32], account: [u8; 32], allowed: bool)` | `()`: `asset_allow` |
//! | `pause` | `(sub_id: [u8; 32], paused: bool)` | `()`: `asset_pause` |
//! | `renounce` | `(sub_id: [u8; 32], capabilities: u8)` | `()`: `asset_renounce` |
//! | `policy` | `asset: [u8; 32]` | `Option<(u8, bool)>`: `asset_policy`, the capabilities and the pause |
//! | `status` | `(asset: [u8; 32], account: [u8; 32])` | `u8`: `asset_status` |
//! | `forward` | `(target: [u8; 32], method: String, args: Vec<u8>, funds: Vec<Funds>)` | the data in the called method's `Ok` |
//! | `forward_then_revert` | same as `forward` | reverts after the call succeeds |
//! | `try_forward` | `(target, method, args, funds, gas: u64)` | `(status: u8, data: Vec<u8>)`: the result of `try_call_with` (0 success, 1 revert, 2 other failure); afterwards writes the storage key `after` and emits the event `after` |
//! | `delegate` | `(target: [u8; 32], method: String, args: Vec<u8>)` | the result of executing the target contract's code with `delegate_call` |
//! | `set` | `(key: Vec<u8>, value: Vec<u8>)` | `()`: writes storage and emits the event `key` |
//! | `get` | `key: Vec<u8>` | `Option<Vec<u8>>` |
//! | `whoami` | none | `(caller: [u8; 32], self_id: [u8; 32])` |
//! | `env` | none | `(block_height: u64, block_timestamp: u64, chain_id: u32, gas_left: u64)` |
//! | `burn` | `(asset: [u8; 32], amount: u128)` | `()`: burns an asset held by this contract |
//! | `create` | `(code_hash: [u8; 32], salt: [u8; 32], init_args: Vec<u8>, funds: Vec<Funds>)` | the new contract's ID |
//! | `spin` | none | never returns: calls a host function in a loop until gas runs out |
//! | `recurse` | `n: u32` | `u32`: recurses n levels deep and returns n (D-073: recursion is allowed, the stack depth is metered at run time) |
//! | `format` | `(n: u64, label: String)` | `String`: the result of `format!` (`core::fmt` can be linked since D-073) |
//! | `dyn_sum` | `n: u32` | `u64`: the sum of 0..n computed through a trait object (dynamic dispatch) |
//! | `trap` | none | trap |
//! | `revert` | none | revert `b"probe revert"` |

#![cfg_attr(target_arch = "wasm32", no_std)]

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;

use ksana_contract_sdk::{self as sdk, storage, AssetMetaInput, CallError, CallResult, Funds};

sdk::entry!(dispatch);

const KEY_INIT_FUNDS: &[u8] = b"init_funds";

/// Dispatch function of the contract entry point.
pub fn dispatch(method: &str, args: &[u8]) -> CallResult {
    match method {
        "init" => {
            let tag: Vec<u8> = sdk::decode(args)?;
            if tag == b"fail" {
                return sdk::revert("init failed");
            }
            storage::set(KEY_INIT_FUNDS, &sdk::encode(&sdk::call_funds()));
            sdk::ok(&())
        }
        "noop" => sdk::ok(&()),
        "funds" => sdk::ok(&(sdk::caller(), sdk::call_funds())),
        "native_tx" => sdk::ok(&sdk::native_tx()),
        "init_funds" => {
            let stored = storage::get(KEY_INIT_FUNDS).unwrap_or_default();
            let funds: Vec<Funds> = sdk::decode(&stored)?;
            sdk::ok(&funds)
        }
        "balance" => {
            let (asset, account): ([u8; 32], [u8; 32]) = sdk::decode(args)?;
            sdk::ok(&sdk::asset_balance(&asset, &account))
        }
        "send" => {
            let (asset, to, amount): ([u8; 32], [u8; 32], u128) = sdk::decode(args)?;
            sdk::asset_transfer(&asset, &to, amount);
            sdk::ok(&())
        }
        "issue" => {
            let (sub_id, amount): ([u8; 32], u128) = sdk::decode(args)?;
            let meta = AssetMetaInput {
                name: String::from("PROBE"),
                symbol: String::from("PRB"),
                decimals: 0,
            };
            sdk::ok(&sdk::asset_issue(&sub_id, &meta, &sdk::caller(), amount))
        }
        "issue_meta" => {
            let (sub_id, meta, to, amount): ([u8; 32], AssetMetaInput, [u8; 32], u128) =
                sdk::decode(args)?;
            sdk::ok(&sdk::asset_issue(&sub_id, &meta, &to, amount))
        }
        "create_asset" => {
            let (sub_id, meta, caps, to, amount): ([u8; 32], AssetMetaInput, u8, [u8; 32], u128) =
                sdk::decode(args)?;
            sdk::ok(&sdk::asset_create(&sub_id, &meta, caps, &to, amount))
        }
        "freeze" => {
            let (sub_id, account, on): ([u8; 32], [u8; 32], bool) = sdk::decode(args)?;
            sdk::asset_freeze(&sub_id, &account, on);
            sdk::ok(&())
        }
        "allow" => {
            let (sub_id, account, on): ([u8; 32], [u8; 32], bool) = sdk::decode(args)?;
            sdk::asset_allow(&sub_id, &account, on);
            sdk::ok(&())
        }
        "pause" => {
            let (sub_id, on): ([u8; 32], bool) = sdk::decode(args)?;
            sdk::asset_pause(&sub_id, on);
            sdk::ok(&())
        }
        "renounce" => {
            let (sub_id, caps): ([u8; 32], u8) = sdk::decode(args)?;
            sdk::asset_renounce(&sub_id, caps);
            sdk::ok(&())
        }
        "policy" => {
            let asset: [u8; 32] = sdk::decode(args)?;
            sdk::ok(&sdk::asset_policy(&asset).map(|p| (p.capabilities, p.paused)))
        }
        "status" => {
            let (asset, account): ([u8; 32], [u8; 32]) = sdk::decode(args)?;
            sdk::ok(&sdk::asset_status(&asset, &account))
        }
        "forward" => {
            let (target, method, inner, funds): ([u8; 32], String, Vec<u8>, Vec<Funds>) =
                sdk::decode(args)?;
            Ok(sdk::call_with_funds(&target, &method, &inner, &funds))
        }
        "forward_then_revert" => {
            let (target, method, inner, funds): ([u8; 32], String, Vec<u8>, Vec<Funds>) =
                sdk::decode(args)?;
            sdk::call_with_funds(&target, &method, &inner, &funds);
            sdk::revert("probe revert")
        }
        "try_forward" => {
            let (target, method, inner, funds, gas): ([u8; 32], String, Vec<u8>, Vec<Funds>, u64) =
                sdk::decode(args)?;
            let result = sdk::try_call_with(&target, &method, &inner, &funds, gas);
            storage::set(b"after", b"1");
            sdk::emit_event(b"after");
            sdk::ok(&match result {
                Ok(data) => (0u8, data),
                Err(CallError::Revert(reason)) => (1, reason),
                Err(CallError::Failed) => (2, Vec::new()),
            })
        }
        "delegate" => {
            let (target, method, inner): ([u8; 32], String, Vec<u8>) = sdk::decode(args)?;
            Ok(sdk::delegate_call(&target, &method, &inner))
        }
        "set" => {
            let (key, value): (Vec<u8>, Vec<u8>) = sdk::decode(args)?;
            storage::set(&key, &value);
            sdk::emit_event(&key);
            sdk::ok(&())
        }
        "get" => {
            let key: Vec<u8> = sdk::decode(args)?;
            sdk::ok(&storage::get(&key))
        }
        "whoami" => sdk::ok(&(sdk::caller(), sdk::self_id())),
        "env" => sdk::ok(&(
            sdk::block_height(),
            sdk::block_timestamp(),
            sdk::chain_id(),
            sdk::gas_left(),
        )),
        "burn" => {
            let (asset, amount): ([u8; 32], u128) = sdk::decode(args)?;
            sdk::asset_burn(&asset, amount);
            sdk::ok(&())
        }
        "create" => {
            let (code_hash, salt, init_args, funds): ([u8; 32], [u8; 32], Vec<u8>, Vec<Funds>) =
                sdk::decode(args)?;
            sdk::ok(&sdk::create_contract(&code_hash, &salt, &init_args, &funds))
        }
        "spin" => loop {
            sdk::block_height();
        },
        "trap" => sdk::trap(),
        "recurse" => {
            let n: u32 = sdk::decode(args)?;
            sdk::ok(&recurse(n))
        }
        "format" => {
            let (n, label): (u64, String) = sdk::decode(args)?;
            sdk::ok(&alloc::format!("{label}={n:>5}|{:x}", n))
        }
        "dyn_sum" => {
            let n: u32 = sdk::decode(args)?;
            let adders: [&dyn Add; 2] = [&Plain, &Doubled];
            let mut sum = 0u64;
            for i in 0..n {
                let pick = usize::try_from(i % 2).unwrap_or(0);
                if let Some(adder) = adders.get(pick) {
                    sum = adder.add(sum, u64::from(i));
                }
            }
            sdk::ok(&sum)
        }
        "revert" => sdk::revert("probe revert"),
        _ => sdk::revert("unknown method"),
    }
}

/// Direct recursion: returns n after n levels.
fn recurse(n: u32) -> u32 {
    match n.checked_sub(1) {
        None => 0,
        Some(m) => recurse(m).saturating_add(1),
    }
}

/// Addition called through a trait object (dynamic dispatch, `call_indirect`).
trait Add {
    fn add(&self, acc: u64, x: u64) -> u64;
}

struct Plain;
struct Doubled;

impl Add for Plain {
    fn add(&self, acc: u64, x: u64) -> u64 {
        acc.saturating_add(x)
    }
}

impl Add for Doubled {
    fn add(&self, acc: u64, x: u64) -> u64 {
        acc.saturating_add(x.saturating_mul(2))
    }
}
