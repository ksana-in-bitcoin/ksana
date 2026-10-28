//! Example contract: a fungible token, the reference KS-20 token (`docs/STANDARDS.md`, D-120; SPEC §9.5).
//!
//! The token is the contract SDK's reference implementation of KS-20 ([`ksana_contract_sdk::ks20::Ks20`]) with the
//! extensions `KS-20-burnable` and `KS-20-mintable` and no rules of its own: no fee, no freeze, no pause. The deployer
//! receives the initial supply; a minter, when `init` names one, can create more up to the cap, hand the role over or
//! end minting for good.
//!
//! Arguments and return values are Borsh-encoded. The arguments are a tuple in the order of the block below (a single
//! argument is just itself; no arguments is empty bytes). An account is 32 bytes: a user's Taproot output key or a
//! contract ID.
//!
//! Rules (KS-20, `docs/STANDARDS.md`):
//!
//! - `init`: `name` is 1 to 64 bytes and `symbol` 1 to 16 bytes (UTF-8, counted in bytes), `decimals` at most 18;
//!   `minter` and `cap` may be left out (as callers written before them send `init`), and then nobody can mint. A cap
//!   below `supply` is refused. `init` emits `standards`, `metadata` and `minter`, then the `transfer` of the supply
//!   from the zero account.
//! - Moving tokens: amounts are above 0; the recipient is neither the zero account nor this contract.
//!   `transfer_from` by any account but `from` takes the allowance `from` gave the caller.
//! - `approve(spender, amount, expires_at)` sets the allowance (it does not add to it); 0 revokes it, `u128::MAX` is
//!   unlimited and never goes down; `expires_at` is the block timestamp (seconds) from which it no longer counts, `None`
//!   never. The two-argument form `approve(spender, amount)` of the token before KS-20 is taken as `expires_at = None`.
//! - `transfer_and_call(to, amount, data)` moves the tokens to the contract `to` and then calls its
//!   `on_token_received(from, amount, data)`, passing on the funds attached to the call; it returns `to`'s answer, and
//!   when `to` refuses the whole op fails.
//! - Every method but `transfer_and_call` refuses attached funds. Every change of a balance emits `transfer` (a mint
//!   from the zero account, a burn to it); `transfer_from` emits no `approve`.
//!
//! Storage keys start with `KS20:` (the reference implementation's).
//!
//! The interface, as the playground reads it:
//!
//! ```abi
//! init(name: String, symbol: String, decimals: u8, supply: u128, minter: Option<Account>, cap: Option<u128>)   // the deployer gets the supply
//! view standards() -> Vec<String>                                  // ["KS-20", "KS-20-burnable", "KS-20-mintable"]
//! view metadata() -> (String, String, u8)                          // (name, symbol, decimals)
//! view capabilities() -> Vec<String>                               // ["mint"] while there is a minter
//! view total_supply() -> u128
//! view balance_of(account: Account) -> u128
//! view allowance(owner: Account, spender: Account) -> u128         // 0 once it expired
//! view allowance_info(owner: Account, spender: Account) -> (u128, Option<u64>)   // (amount, expires_at) as set
//! view minter() -> Option<Account>
//! view cap() -> Option<u128>
//! call transfer(to: Account, amount: u128)
//! call transfer_from(from: Account, to: Account, amount: u128)     // with the allowance from gave the caller
//! call approve(spender: Account, amount: u128, expires_at: Option<u64>)   // sets the allowance; 0 revokes
//! payable call transfer_and_call(to: ContractId, amount: u128, data: Bytes) -> Bytes   // returns the recipient's answer
//! call burn(amount: u128)
//! call burn_from(from: Account, amount: u128)                      // with the allowance from gave the caller
//! call mint(to: Account, amount: u128)                             // minter only, up to the cap
//! call set_minter(minter: Option<Account>)                         // minter only; None ends minting for good
//! event standards(ids: Vec<String>)
//! event metadata(name: String, symbol: String, decimals: u8)
//! event transfer(from: Account, to: Account, amount: u128)
//! event approve(owner: Account, spender: Account, amount: u128, expires_at: Option<u64>)
//! event minter(previous: Option<Account>, minter: Option<Account>)
//! ```

#![cfg_attr(target_arch = "wasm32", no_std)]

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;

use ksana_contract_sdk::{self as sdk, ks20::Ks20, standard, CallResult};

sdk::entry!(dispatch);

/// The token: KS-20 with burning and minting, and no rules of its own.
pub const TOKEN: Ks20 = Ks20::new().burnable().mintable();

/// The arguments of `init`: `(name, symbol, decimals, supply, minter, cap)`.
type Init = (String, String, u8, u128, Option<[u8; 32]>, Option<u128>);

/// Dispatch function of the contract entry point.
pub fn dispatch(method: &str, args: &[u8]) -> CallResult {
    if method == "init" {
        standard::no_funds()?;
        let (name, symbol, decimals, supply, minter, cap) = init_args(args)?;
        TOKEN.init(&name, &symbol, decimals)?;
        TOKEN.init_minter(minter, cap)?;
        if supply > 0 {
            TOKEN.mint(&sdk::caller(), supply)?;
        }
        return sdk::ok(&());
    }
    TOKEN
        .dispatch(method, args)
        .unwrap_or_else(|| sdk::revert("unknown method"))
}

fn init_args(args: &[u8]) -> Result<Init, Vec<u8>> {
    match sdk::decode(args) {
        Ok(all) => Ok(all),
        // Without the minter and the cap, as callers written before them send them
        Err(_) => {
            let (name, symbol, decimals, supply) = sdk::decode(args)?;
            Ok((name, symbol, decimals, supply, None, None))
        }
    }
}
