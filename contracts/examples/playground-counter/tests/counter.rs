//! Native tests using the contract SDK's transactional mock host.
#![cfg(not(target_arch = "wasm32"))]
#![allow(missing_docs, clippy::unwrap_used, clippy::indexing_slicing)]

use std::collections::BTreeSet;

use ksana_contract_sdk::{decode, encode, mock, storage, Funds, KSA_ASSET_ID};
use playground_counter::{dispatch, MAX_ADD};

fn get(contract: [u8; 32]) -> u64 {
    decode(&mock::call([9; 32], contract, "get", &[]).unwrap()).unwrap()
}

#[test]
fn deploy_increment_add_and_read_real_contract_logic() {
    mock::reset();
    let owner = [1; 32];
    let contract = mock::deploy(owner, 0, b"counter", dispatch, &[]).unwrap();
    assert_eq!(get(contract), 0);
    let value: u64 = decode(&mock::call([2; 32], contract, "increment", &[]).unwrap()).unwrap();
    assert_eq!(value, 1);
    let value: u64 =
        decode(&mock::call([3; 32], contract, "add", &encode(&MAX_ADD)).unwrap()).unwrap();
    assert_eq!(value, 1 + MAX_ADD);
    assert_eq!(get(contract), 1 + MAX_ADD);
    let events: Vec<(_, _)> = mock::events()
        .into_iter()
        .map(|e| (e.contract, e.data))
        .collect();
    assert_eq!(
        events,
        vec![
            (contract, encode(&("changed", [2u8; 32], 1u64))),
            (contract, encode(&("changed", [3u8; 32], 1 + MAX_ADD))),
        ]
    );
    assert!(mock::call(owner, contract, "init", &[]).is_err());
    assert!(mock::call(owner, contract, "increment", &[0]).is_err());
    assert!(mock::call(owner, contract, "add", &encode(&0u64)).is_err());
    assert!(mock::call(owner, contract, "add", &encode(&(MAX_ADD + 1))).is_err());
    assert!(mock::call(owner, contract, "add", &[1]).is_err());
    assert!(mock::call(owner, contract, "missing", &[]).is_err());
    assert_eq!(get(contract), 1 + MAX_ADD);
}

#[test]
fn contract_instances_keep_separate_storage() {
    mock::reset();
    let owner = [1; 32];
    let first = mock::deploy(owner, 0, b"counter", dispatch, &[]).unwrap();
    let second = mock::deploy(owner, 1, b"counter", dispatch, &[]).unwrap();
    mock::call(owner, first, "increment", &[]).unwrap();
    assert_eq!(get(second), 0);
}

#[test]
fn attached_funds_and_overflow_are_rejected_without_changing_the_counter() {
    mock::reset();
    let owner = [1; 32];
    let contract = mock::deploy(owner, 0, b"counter", dispatch, &[]).unwrap();
    mock::register_asset(KSA_ASSET_ID, "KSA", 8, [0; 32]);
    mock::mint(KSA_ASSET_ID, owner, 10).unwrap();
    assert!(mock::call_with_funds(
        owner,
        contract,
        "increment",
        &[],
        &[Funds {
            asset: KSA_ASSET_ID,
            amount: 1,
        }],
    )
    .is_err());
    assert_eq!(mock::balance(&KSA_ASSET_ID, &owner), 10);
    mock::with_context(owner, contract, || storage::write(b"counter", &u64::MAX));
    assert!(mock::call(owner, contract, "increment", &[]).is_err());
    assert!(mock::call(owner, contract, "add", &encode(&1u64)).is_err());
    assert_eq!(get(contract), u64::MAX);
    assert!(mock::events().is_empty());
}

/// The methods the abi block declares are the ones `dispatch` answers.
#[test]
fn the_abi_block_matches_dispatch() {
    let source = include_str!("../src/lib.rs");
    let block: Vec<&str> = source
        .lines()
        .map(str::trim_start)
        .skip_while(|l| *l != "//! ```abi")
        .skip(1)
        .take_while(|l| *l != "//! ```")
        .map(|l| l.trim_start_matches("//!").trim())
        .collect();
    let declared: BTreeSet<&str> = block
        .iter()
        .filter_map(|l| {
            let l = l
                .strip_prefix("call ")
                .or_else(|| l.strip_prefix("view "))?;
            l.split('(').next()
        })
        .collect();
    assert_eq!(declared, BTreeSet::from(["increment", "add", "get"]));
    mock::reset();
    let contract = mock::deploy([1; 32], 0, b"counter", dispatch, &[]).unwrap();
    for method in declared {
        let args = if method == "add" {
            encode(&1u64)
        } else {
            Vec::new()
        };
        assert!(
            mock::call([1; 32], contract, method, &args).is_ok(),
            "{method}"
        );
    }
    assert!(block.iter().any(|l| l.starts_with("init()")));
    assert!(block.contains(&"event changed(by: Account, value: u64)"));
}
