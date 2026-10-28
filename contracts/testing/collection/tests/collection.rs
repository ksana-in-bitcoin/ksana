//! Tests of the test collection on the SDK's in-memory mock host: it passes the KS-721 conformance kit.
#![cfg(not(target_arch = "wasm32"))]
#![allow(missing_docs, clippy::unwrap_used, clippy::panic)]

use ksana_contract_sdk::ks721::{self, Event};
use ksana_contract_sdk::mock::{self, Failure};
use ksana_contract_sdk::{conformance, decode, encode, standard, Decode};

/// The deployer and minter (the kit has accounts of its own).
const MINTER: [u8; 32] = [0x11; 32];
const HOLDER: [u8; 32] = [0x12; 32];

fn deploy() -> [u8; 32] {
    mock::deploy(
        MINTER,
        0,
        b"collection",
        collection::dispatch,
        &encode(&("Test Collection", "TC")),
    )
    .unwrap()
}

fn mint(c: [u8; 32], to: [u8; 32], serial: u128) -> Result<Vec<u8>, Failure> {
    mock::call(MINTER, c, "mint", &encode(&(to, serial, None::<String>)))
}

fn query<T: Decode>(c: [u8; 32], method: &str) -> T {
    decode(&mock::call(HOLDER, c, method, &[]).unwrap()).unwrap()
}

#[test]
fn the_collection_follows_ks721() {
    conformance::ks721(|| {
        let collection = deploy();
        for serial in 1..=3 {
            mint(collection, HOLDER, serial).unwrap();
        }
        conformance::Ks721Subject {
            collection,
            holder: HOLDER,
            tokens: (1..=3).map(ks721::id).collect(),
        }
    })
    .unwrap();
}

#[test]
fn the_deployer_mints_until_it_gives_the_role_up() {
    mock::reset();
    let c = deploy();
    let declared: Vec<Event> = mock::events()
        .iter()
        .map(|e| Event::parse(&e.data).unwrap())
        .collect();
    assert_eq!(
        declared,
        [
            Event::Standards(vec![
                ks721::ID.to_string(),
                ks721::BURNABLE.to_string(),
                ks721::ENUMERABLE.to_string(),
            ]),
            Event::Metadata {
                name: "Test Collection".to_string(),
                symbol: "TC".to_string()
            },
        ]
    );
    assert_eq!(
        query::<Vec<String>>(c, "capabilities"),
        ["mint".to_string()]
    );
    let refused = mock::call(HOLDER, c, "mint", &encode(&(HOLDER, 1u128, None::<String>)));
    assert!(
        matches!(&refused, Err(Failure::Revert { reason, .. }) if reason == standard::ERR_NOT_THE_MINTER),
        "{refused:?}"
    );
    mint(c, HOLDER, 1).unwrap();
    mock::call(MINTER, c, "end_minting", &[]).unwrap();
    assert!(query::<Vec<String>>(c, "capabilities").is_empty());
    assert!(mint(c, HOLDER, 2).is_err());
    assert_eq!(query::<u64>(c, "total_supply"), 1);
}
