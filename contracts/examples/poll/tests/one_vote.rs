//! A native test of the poll on the contract SDK's mock host.
#![cfg(not(target_arch = "wasm32"))]
#![allow(missing_docs, clippy::unwrap_used)]

use ksana_contract_sdk::mock::{self, Failure};
use ksana_contract_sdk::{decode, encode};

#[test]
fn every_account_votes_once() {
    mock::reset();
    let creator = [1; 32];
    let init = encode(&(
        "Which fruit?",
        vec!["apple".to_string(), "pear".to_string()],
    ));
    let contract = mock::deploy(creator, 0, b"poll", poll::dispatch, &init).unwrap();

    let votes: u64 =
        decode(&mock::call([2; 32], contract, "vote", &encode(&1u32)).unwrap()).unwrap();
    assert_eq!(votes, 1);
    assert_eq!(
        mock::call([2; 32], contract, "vote", &encode(&0u32)),
        Err(Failure::Revert {
            contract,
            reason: b"already voted".to_vec()
        })
    );
    let results: Vec<u64> =
        decode(&mock::call([9; 32], contract, "results", &[]).unwrap()).unwrap();
    assert_eq!(results, vec![0, 1]);
    assert_eq!(mock::events().len(), 1);
}
