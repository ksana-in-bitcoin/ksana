//! A KS-20 token on the contract SDK's reference implementation: `init` mints the whole supply to the deployer, and
//! holders can burn their tokens. Built on the KS-20 token page of the playground, which has the `standards` feature.
//!
//! ```abi
//! init(name: String, symbol: String, decimals: u8, supply: u128)   // the deployer gets the supply
//! view standards() -> Vec<String>                                  // ["KS-20", "KS-20-burnable"]
//! view metadata() -> (String, String, u8)                          // (name, symbol, decimals)
//! view capabilities() -> Vec<String>
//! view total_supply() -> u128
//! view balance_of(account: Account) -> u128
//! view allowance(owner: Account, spender: Account) -> u128         // 0 once it expired
//! view allowance_info(owner: Account, spender: Account) -> (u128, Option<u64>)
//! call transfer(to: Account, amount: u128)
//! call transfer_from(from: Account, to: Account, amount: u128)     // with the allowance from gave the caller
//! call approve(spender: Account, amount: u128, expires_at: Option<u64>)   // sets the allowance; 0 revokes
//! payable call transfer_and_call(to: ContractId, amount: u128, data: Bytes) -> Bytes
//! call burn(amount: u128)
//! call burn_from(from: Account, amount: u128)                      // with the allowance from gave the caller
//! event standards(ids: Vec<String>)
//! event metadata(name: String, symbol: String, decimals: u8)
//! event transfer(from: Account, to: Account, amount: u128)
//! event approve(owner: Account, spender: Account, amount: u128, expires_at: Option<u64>)
//! ```
#![cfg_attr(target_arch = "wasm32", no_std)]
extern crate alloc;

use alloc::string::String;
use ksana_contract_sdk::{self as sdk, ks20::Ks20, standard, CallResult};

sdk::entry!(dispatch);

const TOKEN: Ks20 = Ks20::new().burnable();

pub fn dispatch(method: &str, args: &[u8]) -> CallResult {
    if method == "init" {
        standard::no_funds()?;
        let (name, symbol, decimals, supply): (String, String, u8, u128) = sdk::decode(args)?;
        TOKEN.init(&name, &symbol, decimals)?;
        TOKEN.mint(&sdk::caller(), supply)?;
        return sdk::ok(&());
    }
    TOKEN
        .dispatch(method, args)
        .unwrap_or_else(|| sdk::revert("unknown method"))
}
