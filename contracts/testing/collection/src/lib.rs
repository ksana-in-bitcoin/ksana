//! Test contract: a KS-721 collection on the contract SDK's reference implementation ([`ksana_contract_sdk::ks721`],
//! `docs/STANDARDS.md`) with the extensions `KS-721-burnable` and `KS-721-enumerable`, for the execution-layer tests
//! (not an example of SPEC §9.5). It is the collection of the `ks721` module's docs, reporting `mint` among its
//! capabilities while the deployer can mint.
//!
//! Parameters and return values are Borsh-encoded; several parameters form a tuple in the order of the table below, no
//! parameters means empty bytes. Every method of KS-721 and of the two extensions is the reference implementation's.
//!
//! | Method | Parameters | Returns |
//! | --- | --- | --- |
//! | `init` (at deployment) | `(name: String, symbol: String)` | `()`; the deployer becomes the minter |
//! | `mint` | `(to: [u8; 32], serial: u128, uri: Option<String>)` | `()`: mints the token `ks721::id(serial)`; the minter only |
//! | `end_minting` | none | `()`: the minter gives the role up for good |

#![cfg_attr(target_arch = "wasm32", no_std)]

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;

use ksana_contract_sdk::ks721::{self, Ks721, Policy};
use ksana_contract_sdk::{self as sdk, standard, storage, CallResult};

sdk::entry!(dispatch);

const KEY_MINTER: &[u8] = b"minter";

/// The collection's rules: none but reporting `mint` while there is a minter.
#[derive(Clone, Copy, Debug)]
pub struct Minting;

impl Policy for Minting {
    fn capabilities(&self) -> Result<Vec<&'static str>, Vec<u8>> {
        Ok(match storage::get(KEY_MINTER) {
            Some(_) => Vec::from([standard::CAP_MINT]),
            None => Vec::new(),
        })
    }
}

/// The collection: KS-721 with burning and the lists.
pub const NFT: Ks721<Minting> = Ks721::new().burnable().enumerable().with_policy(Minting);

/// Dispatch function of the contract entry point.
pub fn dispatch(method: &str, args: &[u8]) -> CallResult {
    match method {
        "init" => {
            standard::no_funds()?;
            let (name, symbol): (String, String) = sdk::decode(args)?;
            NFT.init(&name, &symbol)?;
            storage::write(KEY_MINTER, &sdk::caller());
            sdk::ok(&())
        }
        "mint" => {
            standard::no_funds()?;
            let (to, serial, uri): ([u8; 32], u128, Option<String>) = sdk::decode(args)?;
            only_the_minter()?;
            NFT.mint(&to, &ks721::id(serial), uri.as_deref())?;
            sdk::ok(&())
        }
        "end_minting" => {
            standard::no_funds()?;
            sdk::decode::<()>(args)?;
            only_the_minter()?;
            storage::remove(KEY_MINTER);
            sdk::ok(&())
        }
        _ => NFT
            .dispatch(method, args)
            .unwrap_or_else(|| sdk::revert("unknown method")),
    }
}

fn only_the_minter() -> Result<(), Vec<u8>> {
    if storage::read::<[u8; 32]>(KEY_MINTER)? == Some(sdk::caller()) {
        Ok(())
    } else {
        Err(standard::ERR_NOT_THE_MINTER.to_vec())
    }
}
