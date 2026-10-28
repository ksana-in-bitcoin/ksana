//! Playground example: a shared counter. Anyone can increase it; it takes no funds.
//!
//! The smallest contract that keeps state: one `u64` under one storage key. `init` runs once, at deployment, and
//! starts the count at 0; `increment` adds one and `add` adds up to [`MAX_ADD`] at a time; `get` reads the count
//! without changing anything. Every change emits a `changed` event with the caller and the new count.
//!
//! Arguments and return values are Borsh-encoded: `add` takes a `u64`, the other methods empty arguments; `init`
//! returns `()` and the others the count as a `u64`.
//!
//! ```abi
//! init()                          // starts the count at 0
//! call increment() -> u64         // adds one; returns the new count
//! call add(amount: u64) -> u64    // adds 1 to 1000 at a time; returns the new count
//! view get() -> u64               // the count
//! event changed(by: Account, value: u64)
//! ```
#![cfg_attr(target_arch = "wasm32", no_std)]

use ksana_contract_sdk::{self as sdk, storage, CallResult};

sdk::entry!(dispatch);

const KEY: &[u8] = b"counter";

/// The most `add` adds at once.
pub const MAX_ADD: u64 = 1_000;

/// Dispatches the playground's methods; storage belongs to this contract instance.
pub fn dispatch(method: &str, args: &[u8]) -> CallResult {
    if !sdk::call_funds().is_empty() {
        return sdk::revert("this example takes no funds");
    }
    let current = storage::read::<u64>(KEY)?;
    match (method, current) {
        ("init", None) => {
            sdk::decode::<()>(args)?;
            storage::write(KEY, &0u64);
            sdk::ok(&())
        }
        ("init", Some(_)) => sdk::revert("already initialized"),
        ("get", Some(value)) => {
            sdk::decode::<()>(args)?;
            sdk::ok(&value)
        }
        ("increment", Some(value)) => {
            sdk::decode::<()>(args)?;
            change(value, 1)
        }
        ("add", Some(value)) => {
            let amount: u64 = sdk::decode(args)?;
            if amount == 0 || amount > MAX_ADD {
                return sdk::revert("add 1 to 1000 at a time");
            }
            change(value, amount)
        }
        ("get" | "increment" | "add", None) => sdk::revert("not initialized"),
        _ => sdk::revert("unknown method"),
    }
}

/// Adds `amount` to the count `value`, keeps it and emits `changed`.
fn change(value: u64, amount: u64) -> CallResult {
    let Some(next) = value.checked_add(amount) else {
        return sdk::revert("counter overflow");
    };
    storage::write(KEY, &next);
    sdk::emit(&("changed", sdk::caller(), next));
    sdk::ok(&next)
}
