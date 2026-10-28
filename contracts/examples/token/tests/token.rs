//! Tests of the token contract on the SDK's in-memory mock host (via the same entry-point encode/decode path as wasm).
#![cfg(not(target_arch = "wasm32"))]
#![allow(
    missing_docs,
    clippy::unwrap_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]

use ksana_contract_sdk::ks20::{self, Event};
use ksana_contract_sdk::mock::{self, Failure};
use ksana_contract_sdk::{conformance, decode, encode, ERR_MALFORMED_ARGS};

const ALICE: [u8; 32] = [0xa1; 32];
const BOB: [u8; 32] = [0xb0; 32];
const CAROL: [u8; 32] = [0xc4; 32];

fn deploy_with(args: &[u8]) -> Result<[u8; 32], Failure> {
    mock::reset();
    mock::deploy(ALICE, 0, b"token", token::dispatch, args)
}

fn deploy(supply: u128) -> [u8; 32] {
    deploy_with(&encode(&("Test Token", "TT", 8u8, supply))).unwrap()
}

fn call(caller: [u8; 32], token: [u8; 32], method: &str, args: &[u8]) -> Result<Vec<u8>, Failure> {
    mock::call(caller, token, method, args)
}

fn query<T: ksana_contract_sdk::Decode>(token: [u8; 32], method: &str, args: &[u8]) -> T {
    decode(&call(CAROL, token, method, args).unwrap()).unwrap()
}

fn balance(token: [u8; 32], account: [u8; 32]) -> u128 {
    query(token, "balance_of", &encode(&account))
}

fn allowance(token: [u8; 32], owner: [u8; 32], spender: [u8; 32]) -> u128 {
    query(token, "allowance", &encode(&(owner, spender)))
}

fn revert_reason(result: Result<Vec<u8>, Failure>) -> Vec<u8> {
    match result {
        Err(Failure::Revert { reason, .. }) => reason,
        other => panic!("expected a revert, got {other:?}"),
    }
}

fn events(token: [u8; 32]) -> Vec<Event> {
    mock::events()
        .into_iter()
        .filter(|e| e.contract == token)
        .map(|e| Event::parse(&e.data).unwrap())
        .collect()
}

#[test]
fn the_token_follows_ks20() {
    // the kit has accounts of its own (`conformance::ALICE` is this file's ALICE)
    let holder = [0x11; 32];
    let with_minter = encode(&(
        "Test Token",
        "TT",
        8u8,
        1_000_000u128,
        Some(holder),
        Some(2_000_000u128),
    ));
    for init in [
        encode(&("Test Token", "TT", 8u8, 1_000_000u128)),
        with_minter,
    ] {
        conformance::ks20(|| conformance::Ks20Subject {
            token: mock::deploy(holder, 0, b"token", token::dispatch, &init).unwrap(),
            holder,
        })
        .unwrap();
    }
}

#[test]
fn init_declares_and_mints_supply_to_deployer() {
    let token = deploy(1_000_000);
    assert_eq!(balance(token, ALICE), 1_000_000);
    assert_eq!(balance(token, BOB), 0);
    assert_eq!(query::<u128>(token, "total_supply", &[]), 1_000_000);
    let meta: (String, String, u8) = query(token, "metadata", &[]);
    assert_eq!(meta, ("Test Token".to_string(), "TT".to_string(), 8));
    let standards = vec![
        ks20::ID.to_string(),
        ks20::BURNABLE.to_string(),
        ks20::MINTABLE.to_string(),
    ];
    assert_eq!(query::<Vec<String>>(token, "standards", &[]), standards);
    assert_eq!(
        events(token),
        [
            Event::Standards(standards),
            Event::Metadata {
                name: "Test Token".to_string(),
                symbol: "TT".to_string(),
                decimals: 8
            },
            Event::Minter {
                previous: None,
                minter: None
            },
            Event::Transfer {
                from: [0; 32],
                to: ALICE,
                amount: 1_000_000
            },
        ]
    );
    // without a minter nobody mints, and nothing is reported
    assert_eq!(query::<Option<[u8; 32]>>(token, "minter", &[]), None);
    assert!(query::<Vec<String>>(token, "capabilities", &[]).is_empty());
    assert_eq!(
        revert_reason(call(ALICE, token, "mint", &encode(&(ALICE, 1u128)))),
        b"minting has ended".to_vec()
    );
}

#[test]
fn a_minter_mints_up_to_the_cap() {
    let args = encode(&("Test Token", "TT", 0u8, 100u128, Some(BOB), Some(150u128)));
    let token = deploy_with(&args).unwrap();
    assert_eq!(query::<Option<[u8; 32]>>(token, "minter", &[]), Some(BOB));
    assert_eq!(query::<Option<u128>>(token, "cap", &[]), Some(150));
    assert_eq!(
        query::<Vec<String>>(token, "capabilities", &[]),
        ["mint".to_string()]
    );
    assert_eq!(
        revert_reason(call(ALICE, token, "mint", &encode(&(ALICE, 1u128)))),
        b"not the minter".to_vec()
    );
    call(BOB, token, "mint", &encode(&(CAROL, 50u128))).unwrap();
    assert_eq!(balance(token, CAROL), 50);
    assert_eq!(
        revert_reason(call(BOB, token, "mint", &encode(&(CAROL, 1u128)))),
        b"cap exceeded".to_vec()
    );
    // a cap below the supply is refused at deployment
    let args = encode(&("Test Token", "TT", 0u8, 100u128, Some(BOB), Some(99u128)));
    assert_eq!(
        revert_reason(deploy_with(&args).map(|_| Vec::new())),
        b"cap exceeded".to_vec()
    );
}

#[test]
fn transfer_moves_balance_and_emits_event() {
    let token = deploy(1_000);
    mock::clear_events();
    call(ALICE, token, "transfer", &encode(&(BOB, 250u128))).unwrap();
    assert_eq!(balance(token, ALICE), 750);
    assert_eq!(balance(token, BOB), 250);
    assert_eq!(
        events(token),
        [Event::Transfer {
            from: ALICE,
            to: BOB,
            amount: 250
        }]
    );
    // the transfer event keeps the layout of the token before KS-20
    let raw = mock::events().pop().unwrap().data;
    assert_eq!(raw, encode(&("transfer", ALICE, BOB, 250u128)));
}

#[test]
fn insufficient_balance_reverts_without_changes() {
    let token = deploy(100);
    let result = call(BOB, token, "transfer", &encode(&(ALICE, 1u128)));
    assert_eq!(revert_reason(result), b"insufficient balance".to_vec());
    let result = call(ALICE, token, "transfer", &encode(&(BOB, 101u128)));
    assert_eq!(revert_reason(result), b"insufficient balance".to_vec());
    assert_eq!(balance(token, ALICE), 100);
    assert_eq!(balance(token, BOB), 0);
}

#[test]
fn approvals_expire_and_the_old_form_never_does() {
    let token = deploy(1_000);
    mock::set_block_timestamp(1_000);
    call(
        ALICE,
        token,
        "approve",
        &encode(&(BOB, 300u128, Some(2_000u64))),
    )
    .unwrap();
    call(
        BOB,
        token,
        "transfer_from",
        &encode(&(ALICE, CAROL, 100u128)),
    )
    .unwrap();
    assert_eq!(allowance(token, ALICE, BOB), 200);
    mock::set_block_timestamp(2_000);
    assert_eq!(allowance(token, ALICE, BOB), 0);
    assert_eq!(
        query::<(u128, Option<u64>)>(token, "allowance_info", &encode(&(ALICE, BOB))),
        (200, Some(2_000))
    );
    assert_eq!(
        revert_reason(call(
            BOB,
            token,
            "transfer_from",
            &encode(&(ALICE, CAROL, 1u128))
        )),
        b"allowance expired".to_vec()
    );
    // approve(spender, amount), as the token took it before KS-20
    call(ALICE, token, "approve", &encode(&(BOB, 50u128))).unwrap();
    assert_eq!(
        query::<(u128, Option<u64>)>(token, "allowance_info", &encode(&(ALICE, BOB))),
        (50, None)
    );
    call(
        BOB,
        token,
        "transfer_from",
        &encode(&(ALICE, CAROL, 50u128)),
    )
    .unwrap();
    assert_eq!(balance(token, CAROL), 150);
    assert_eq!(allowance(token, ALICE, BOB), 0);
}

#[test]
fn burning_lowers_the_supply() {
    let token = deploy(1_000);
    call(ALICE, token, "burn", &encode(&100u128)).unwrap();
    call(
        ALICE,
        token,
        "approve",
        &encode(&(BOB, 50u128, None::<u64>)),
    )
    .unwrap();
    call(BOB, token, "burn_from", &encode(&(ALICE, 50u128))).unwrap();
    assert_eq!(balance(token, ALICE), 850);
    assert_eq!(query::<u128>(token, "total_supply", &[]), 850);
}

#[test]
fn zero_balances_are_removed_from_storage() {
    let token = deploy(100);
    call(ALICE, token, "transfer", &encode(&(BOB, 100u128))).unwrap();
    let alice_key = [b"KS20:b".as_slice(), &ALICE].concat();
    let bob_key = [b"KS20:b".as_slice(), &BOB].concat();
    assert_eq!(mock::storage_value(&token, &alice_key), None);
    assert_eq!(
        mock::storage_value(&token, &bob_key),
        Some(100u128.to_le_bytes().to_vec())
    );
}

#[test]
fn init_cannot_be_called_again_and_bad_calls_revert() {
    let token = deploy(100);
    let result = call(ALICE, token, "init", &encode(&("X", "X", 0u8, 1u128)));
    assert!(matches!(result, Err(Failure::Trap(_))), "{result:?}");
    assert_eq!(balance(token, ALICE), 100);

    let result = call(ALICE, token, "no_such_method", &[]);
    assert_eq!(revert_reason(result), b"unknown method".to_vec());
    let result = call(ALICE, token, "transfer", &[1, 2, 3]);
    assert_eq!(revert_reason(result), ERR_MALFORMED_ARGS.to_vec());
    // Trailing argument bytes are rejected too.
    let mut args = encode(&(BOB, 1u128));
    args.push(0);
    assert_eq!(
        revert_reason(call(ALICE, token, "transfer", &args)),
        ERR_MALFORMED_ARGS.to_vec()
    );
    // the metadata is checked
    let result = deploy_with(&encode(&("", "TT", 8u8, 1u128)));
    assert_eq!(
        revert_reason(result.map(|_| Vec::new())),
        b"name must be 1 to 64 bytes".to_vec()
    );
    let result = deploy_with(&encode(&("Test", "TT", 19u8, 1u128)));
    assert_eq!(
        revert_reason(result.map(|_| Vec::new())),
        b"decimals must be at most 18".to_vec()
    );
}

#[test]
fn supply_is_conserved_under_random_transfers() {
    let accounts = [ALICE, BOB, CAROL, [0xd0; 32], [0xe0; 32]];
    let token = deploy(1_000_000);
    let mut seed: u64 = 0x1234_5678;
    let mut next = || {
        seed = seed
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        seed >> 33
    };
    for _ in 0..300 {
        let from = accounts[(next() % 5) as usize];
        let to = accounts[(next() % 5) as usize];
        let amount = u128::from(next() % 50_000);
        let before = balance(token, from);
        let result = call(from, token, "transfer", &encode(&(to, amount)));
        assert_eq!(result.is_ok(), amount > 0 && amount <= before);
        let total: u128 = accounts.iter().map(|a| balance(token, *a)).sum();
        assert_eq!(total, 1_000_000);
    }
}
