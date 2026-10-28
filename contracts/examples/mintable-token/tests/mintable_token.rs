//! Tests of the mintable-token contract on the SDK's in-memory mock host: deployment with and without a cap and an
//! initial supply, minting up to the cap, burns by holders, the hand-over and renouncement of ownership, the views
//! against the mock's ledger, the accounting under random operations, and the ABI block kept in step with `dispatch`.
#![cfg(not(target_arch = "wasm32"))]
#![allow(
    missing_docs,
    clippy::unwrap_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::cast_possible_truncation
)]

use std::collections::BTreeSet;

use ksana_contract_sdk::mock::{self, Failure};
use ksana_contract_sdk::{
    self as sdk, decode, encode, CallResult, Funds, ERR_MALFORMED_ARGS, KSA_ASSET_ID,
};
use mintable_token::{Info, MAX_DECIMALS, MAX_NAME_BYTES, SUB_ID};

const ALICE: [u8; 32] = [0xa1; 32];
const BOB: [u8; 32] = [0xb0; 32];
const CAROL: [u8; 32] = [0xc4; 32];
const ZERO: [u8; 32] = [0; 32];
/// A contract that holds or owns the token in some tests (see [`helper`]).
const HELPER: [u8; 32] = [0x5e; 32];
/// Another built-in asset.
const OTHER: [u8; 32] = [0xee; 32];

const CODE: &[u8] = b"mintable-token";
/// The KSA each of ALICE, BOB and CAROL holds.
const KSA_EACH: u128 = 1_000_000;

const ERR_NAME: &[u8] = b"name must be 1 to 32 bytes";
const ERR_DECIMALS: &[u8] = b"decimals must be at most 18";
const ERR_ABOVE_CAP: &[u8] = b"supply would exceed the cap";
const ERR_NOT_OWNER: &[u8] = b"only the owner";
const ERR_RENOUNCED: &[u8] = b"ownership has been renounced";
const ERR_UNEXPECTED_FUNDS: &[u8] = b"unexpected attached funds";
const ERR_ATTACH_TOKEN: &[u8] = b"attach only this token";
const ERR_OVERFLOW: &[u8] = b"arithmetic overflow";

#[derive(Clone, Copy, Debug)]
struct Token {
    id: [u8; 32],
    asset: [u8; 32],
}

fn deploy_as(
    deployer: [u8; 32],
    nonce: u64,
    name: &str,
    decimals: u8,
    cap: Option<u128>,
    initial_supply: u128,
) -> Result<Token, Failure> {
    let id = mock::deploy(
        deployer,
        nonce,
        CODE,
        mintable_token::dispatch,
        &encode(&(name, "KITE", decimals, cap, initial_supply)),
    )?;
    Ok(Token {
        id,
        asset: sdk::asset_id(&id, &SUB_ID),
    })
}

/// A fresh mock with KSA held by ALICE, BOB and CAROL, the helper contract, and a token "Kite" ALICE deployed.
fn setup(cap: Option<u128>, initial_supply: u128) -> Token {
    mock::reset();
    mock::register_asset(KSA_ASSET_ID, "KSA", 8, ZERO);
    for who in [ALICE, BOB, CAROL] {
        mock::mint(KSA_ASSET_ID, who, KSA_EACH).unwrap();
    }
    mock::register(HELPER, helper);
    deploy_as(ALICE, 0, "Kite", 8, cap, initial_supply).unwrap()
}

/// A contract that holds or owns the token: it forwards a mint to the token, sends what is attached to it on to an
/// account (as the built-in `AssetTransfer` would move it), and burns its own holdings through the token's `burn` or
/// directly with `asset_burn`, which the token contract never sees.
fn helper(method: &str, args: &[u8]) -> CallResult {
    match method {
        "mint" => {
            let (token, to, amount): ([u8; 32], [u8; 32], u128) = decode(args)?;
            Ok(sdk::call(&token, "mint", &encode(&(to, amount))))
        }
        "send" => {
            let to: [u8; 32] = decode(args)?;
            for funds in sdk::call_funds() {
                sdk::asset_transfer(&funds.asset, &to, funds.amount);
            }
            Ok(Vec::new())
        }
        "burn_through_token" => {
            let (token, asset, amount): ([u8; 32], [u8; 32], u128) = decode(args)?;
            Ok(sdk::call_with_funds(
                &token,
                "burn",
                &[],
                &[Funds { asset, amount }],
            ))
        }
        "burn_directly" => {
            let (asset, amount): ([u8; 32], u128) = decode(args)?;
            sdk::asset_burn(&asset, amount);
            Ok(Vec::new())
        }
        _ => sdk::revert("unknown method"),
    }
}

fn ksa(amount: u128) -> Funds {
    Funds {
        asset: KSA_ASSET_ID,
        amount,
    }
}

fn mint(token: Token, who: [u8; 32], to: [u8; 32], amount: u128) -> Result<u128, Failure> {
    mock::call(who, token.id, "mint", &encode(&(to, amount))).map(|out| decode(&out).unwrap())
}

/// `who` burns `amount` of the token, attached alone.
fn burn(token: Token, who: [u8; 32], amount: u128) -> Result<u128, Failure> {
    let funds = Funds {
        asset: token.asset,
        amount,
    };
    burn_with(token, who, &[funds])
}

fn burn_with(token: Token, who: [u8; 32], funds: &[Funds]) -> Result<u128, Failure> {
    mock::call_with_funds(who, token.id, "burn", &[], funds).map(|out| decode(&out).unwrap())
}

fn set_owner(token: Token, who: [u8; 32], owner: Option<[u8; 32]>) -> Result<(), Failure> {
    mock::call(who, token.id, "set_owner", &encode(&owner)).map(|out| decode(&out).unwrap())
}

fn info(token: Token) -> Info {
    decode(&mock::call(CAROL, token.id, "info", &[]).unwrap()).unwrap()
}

fn balance_of(token: Token, account: [u8; 32]) -> u128 {
    decode(&mock::call(CAROL, token.id, "balance_of", &encode(&account)).unwrap()).unwrap()
}

fn revert_reason<T: std::fmt::Debug>(result: Result<T, Failure>) -> Vec<u8> {
    match result {
        Err(Failure::Revert { reason, .. }) => reason,
        other => panic!("expected a revert, got {other:?}"),
    }
}

fn mint_event(to: [u8; 32], amount: u128) -> Vec<u8> {
    encode(&("mint", to, amount))
}

fn burn_event(from: [u8; 32], amount: u128) -> Vec<u8> {
    encode(&("burn", from, amount))
}

fn owner_event(previous: Option<[u8; 32]>, owner: Option<[u8; 32]>) -> Vec<u8> {
    encode(&("owner", previous, owner))
}

/// The data of the events the token contract emitted.
fn token_events(token: Token) -> Vec<Vec<u8>> {
    mock::events()
        .into_iter()
        .filter(|event| event.contract == token.id)
        .map(|event| event.data)
        .collect()
}

/// The token's accounting against the ledger: `supply = minted − burned` is the asset's total supply (no burns outside
/// the contract here), which the holders' balances add up to; the contract holds none of its own token, the supply
/// stays within the cap, and `balance_of` answers the ledger's balances.
fn assert_consistent(token: Token, holders: &[[u8; 32]]) {
    let state = info(token);
    assert_eq!(state.supply, state.minted - state.burned);
    assert_eq!(
        mock::asset_info(&token.asset).unwrap().total_supply,
        state.supply
    );
    let held: u128 = holders
        .iter()
        .map(|holder| mock::balance(&token.asset, holder))
        .sum();
    assert_eq!(held, state.supply);
    assert_eq!(mock::balance(&token.asset, &token.id), 0);
    assert!(state.cap.is_none_or(|cap| state.supply <= cap));
    for holder in holders {
        assert_eq!(
            balance_of(token, *holder),
            mock::balance(&token.asset, holder)
        );
    }
}

// ---------------------------------------------------------------------------
// Deployment

#[test]
fn deploys_with_and_without_a_cap_and_an_initial_supply() {
    for (cap, initial_supply) in [
        (None, 0),
        (None, 1_000),
        (Some(1_000), 0),
        (Some(1_000), 1_000),
        (Some(5_000), 1_000),
    ] {
        let token = setup(cap, initial_supply);
        assert_eq!(
            info(token),
            Info {
                asset: token.asset,
                name: "Kite".to_string(),
                symbol: "KITE".to_string(),
                decimals: 8,
                owner: Some(ALICE),
                cap,
                supply: initial_supply,
                minted: initial_supply,
                burned: 0,
            }
        );
        // The asset exists from the deployment on, also when nothing was issued
        let ledger = mock::asset_info(&token.asset).unwrap();
        assert_eq!(
            (
                ledger.name.as_str(),
                ledger.decimals,
                ledger.total_supply,
                ledger.issuer
            ),
            ("Kite", 8, initial_supply, token.id)
        );
        assert_eq!(mock::balance(&token.asset, &ALICE), initial_supply);
        let mut expected = Vec::new();
        if initial_supply > 0 {
            expected.push(mint_event(ALICE, initial_supply));
        }
        expected.push(owner_event(None, Some(ALICE)));
        assert_eq!(token_events(token), expected);
        // The storage layout: a count of 0 and a missing cap have no key
        let stored = |key: &[u8]| mock::storage_value(&token.id, key);
        assert_eq!(stored(b"meta"), Some(encode(&("Kite", "KITE", 8u8))));
        assert_eq!(stored(b"owner"), Some(ALICE.to_vec()));
        assert_eq!(stored(b"cap"), cap.map(|cap| encode(&cap)));
        assert_eq!(
            stored(b"minted"),
            (initial_supply > 0).then(|| encode(&initial_supply))
        );
        assert_eq!(stored(b"burned"), None);
        assert_consistent(token, &[ALICE]);
        // The owner mints later, under the name, symbol and decimals recorded at the deployment, as far as the cap allows
        let result = mint(token, ALICE, BOB, 1);
        if cap.is_none_or(|cap| initial_supply < cap) {
            assert_eq!(result, Ok(initial_supply + 1));
        } else {
            assert_eq!(revert_reason(result), ERR_ABOVE_CAP);
        }
        assert_consistent(token, &[ALICE, BOB]);
    }
}

#[test]
fn the_asset_id_is_the_hash_of_the_contract_and_sub_id_zero() {
    let token = setup(None, 1);
    let expected = sdk::sha256(&[token.id.as_slice(), [0u8; 32].as_slice()].concat());
    let asset: [u8; 32] = decode(&mock::call(BOB, token.id, "asset", &[]).unwrap()).unwrap();
    assert_eq!(asset, expected);
    assert_eq!(info(token).asset, expected);
    assert_eq!(mock::asset_info(&expected).unwrap().issuer, token.id);
    // Known before the deployment, from the deployer, its nonce and the code; every deployment has its own asset
    let predicted = sdk::asset_id(&sdk::contract_id(&ALICE, 1, &sdk::sha256(CODE)), &SUB_ID);
    let second = deploy_as(ALICE, 1, "Kite", 8, None, 1).unwrap();
    assert_eq!(second.asset, predicted);
    assert_ne!(second.asset, token.asset);
    assert_eq!(mock::balance(&token.asset, &ALICE), 1);
    assert_eq!(mock::balance(&second.asset, &ALICE), 1);
}

#[test]
fn init_checks_its_arguments_and_a_failed_deployment_leaves_nothing() {
    mock::reset();
    mock::register_asset(KSA_ASSET_ID, "KSA", 8, ZERO);
    mock::mint(KSA_ASSET_ID, ALICE, KSA_EACH).unwrap();
    let id = sdk::contract_id(&ALICE, 0, &sdk::sha256(CODE));
    let nothing_left = || {
        assert!(!mock::contract_exists(&id));
        assert!(mock::asset_info(&sdk::asset_id(&id, &SUB_ID)).is_none());
        assert!(mock::events().is_empty());
    };
    let too_long = "k".repeat(MAX_NAME_BYTES + 1);
    // 33 bytes in 17 characters, and in 11
    let umlauts = format!("{}k", "ü".repeat(16));
    let bitcoins = "₿".repeat(11);
    for (name, decimals, cap, initial_supply, reason) in [
        ("", 8, None, 1, ERR_NAME),
        (too_long.as_str(), 8, None, 1, ERR_NAME),
        (umlauts.as_str(), 8, None, 1, ERR_NAME),
        (bitcoins.as_str(), 8, None, 1, ERR_NAME),
        ("Kite", MAX_DECIMALS + 1, None, 1, ERR_DECIMALS),
        ("Kite", u8::MAX, None, 1, ERR_DECIMALS),
        ("Kite", 8, Some(0), 0, b"cap must be positive"),
        ("Kite", 8, Some(1_000), 1_001, ERR_ABOVE_CAP),
        ("Kite", 8, Some(u128::MAX - 1), u128::MAX, ERR_ABOVE_CAP),
    ] {
        let result = deploy_as(ALICE, 0, name, decimals, cap, initial_supply);
        assert_eq!(revert_reason(result), reason, "{name} {decimals} {cap:?}");
        nothing_left();
    }
    // Funds attached at the deployment are refused, and come back with it
    let result = mock::deploy_with_funds(
        ALICE,
        0,
        CODE,
        mintable_token::dispatch,
        &encode(&("Kite", "KITE", 8u8, None::<u128>, 1u128)),
        &[ksa(5)],
    );
    assert_eq!(revert_reason(result), ERR_UNEXPECTED_FUNDS);
    assert_eq!(mock::balance(&KSA_ASSET_ID, &ALICE), KSA_EACH);
    nothing_left();
    // Malformed arguments: a missing and an extra field, an option tag of 2, a name that is not UTF-8, the symbol
    // left out as before D-121
    for args in [
        encode(&("Kite", "KITE", 8u8, None::<u128>)),
        encode(&("Kite", "KITE", 8u8, None::<u128>, 1u128, 0u8)),
        [encode("Kite"), encode("KITE"), vec![8, 2], encode(&1u128)].concat(),
        encode(&(vec![0xffu8, 0xfe], "KITE", 8u8, None::<u128>, 1u128)),
        encode(&("Kite", 8u8, None::<u128>, 1u128)),
    ] {
        let result = mock::deploy(ALICE, 0, CODE, mintable_token::dispatch, &args);
        assert_eq!(revert_reason(result), ERR_MALFORMED_ARGS);
        nothing_left();
    }
    // The symbol is 1 to 16 bytes of UTF-8
    for symbol in ["", "KITEKITEKITEKITEK", "₿₿₿₿₿₿"] {
        let args = encode(&("Kite", symbol, 8u8, None::<u128>, 1u128));
        let result = mock::deploy(ALICE, 0, CODE, mintable_token::dispatch, &args);
        assert_eq!(
            revert_reason(result),
            b"symbol must be 1 to 16 bytes",
            "{symbol}"
        );
        nothing_left();
    }
    // The zero account never becomes the owner (nobody can sign as it, but the mock can call as it)
    let result = deploy_as(ZERO, 0, "Kite", 8, None, 1);
    assert_eq!(revert_reason(result), b"account must not be zero");
    let zero_id = sdk::contract_id(&ZERO, 0, &sdk::sha256(CODE));
    assert!(!mock::contract_exists(&zero_id));
    assert!(mock::events().is_empty());
}

#[test]
fn init_accepts_the_edges() {
    mock::reset();
    let umlauts = "ü".repeat(16);
    let kites = "🪁".repeat(8);
    for (nonce, (name, decimals, cap, initial_supply)) in [
        ("K", 0, None, 0),
        (umlauts.as_str(), MAX_DECIMALS, None, 1),
        (kites.as_str(), 8, Some(1), 1),
        ("Kite", 8, Some(u128::MAX), u128::MAX),
        ("Kite", 8, None, u128::MAX),
    ]
    .into_iter()
    .enumerate()
    {
        let nonce = u64::try_from(nonce).unwrap();
        let token = deploy_as(ALICE, nonce, name, decimals, cap, initial_supply).unwrap();
        let state = info(token);
        assert_eq!(
            (state.name.as_str(), state.decimals, state.cap, state.supply),
            (name, decimals, cap, initial_supply)
        );
        assert_eq!(mock::asset_info(&token.asset).unwrap().name, name);
        assert_eq!(mock::balance(&token.asset, &ALICE), initial_supply);
    }
    // 32 bytes each, in 16 and in 8 characters
    assert_eq!(
        (umlauts.len(), kites.len()),
        (MAX_NAME_BYTES, MAX_NAME_BYTES)
    );
}

#[test]
fn init_runs_only_once() {
    let token = setup(Some(1_000), 100);
    let before = info(token);
    let args = encode(&("Kite", "KITE", 8u8, None::<u128>, 5u128));
    // The VM refuses `init` after the deployment
    assert!(matches!(
        mock::call(ALICE, token.id, "init", &args),
        Err(Failure::Trap(_))
    ));
    // and so does the contract, were it called again
    let answer = mock::with_context(ALICE, token.id, || mintable_token::dispatch("init", &args));
    assert_eq!(answer, Err(b"already initialized".to_vec()));
    assert_eq!(info(token), before);
    assert_eq!(mock::asset_info(&token.asset).unwrap().total_supply, 100);
}

#[test]
fn every_method_waits_for_init() {
    mock::reset();
    mock::register_asset(KSA_ASSET_ID, "KSA", 8, ZERO);
    mock::mint(KSA_ASSET_ID, ALICE, KSA_EACH).unwrap();
    // The code, registered without running its `init`
    let id = [0x70; 32];
    mock::register(id, mintable_token::dispatch);
    for (method, args) in [
        ("mint", encode(&(BOB, 1u128))),
        ("burn", Vec::new()),
        ("set_owner", encode(&Some(BOB))),
        ("info", Vec::new()),
        ("asset", Vec::new()),
        ("balance_of", encode(&ALICE)),
    ] {
        assert_eq!(
            revert_reason(mock::call(ALICE, id, method, &args)),
            b"not initialized",
            "{method}"
        );
        let result = mock::call_with_funds(ALICE, id, method, &args, &[ksa(1)]);
        assert_eq!(revert_reason(result), b"not initialized", "{method}");
    }
    assert_eq!(mock::balance(&KSA_ASSET_ID, &ALICE), KSA_EACH);
    assert!(mock::events().is_empty());
}

#[test]
fn unknown_methods_revert() {
    let token = setup(None, 1);
    for method in ["transfer", "approve", "total_supply", "Mint", ""] {
        assert_eq!(
            revert_reason(mock::call(ALICE, token.id, method, &[])),
            b"unknown method"
        );
    }
}

// ---------------------------------------------------------------------------
// Minting

#[test]
fn the_owner_mints_up_to_exactly_the_cap() {
    let token = setup(Some(1_000), 400);
    mock::clear_events();
    assert_eq!(revert_reason(mint(token, ALICE, BOB, 601)), ERR_ABOVE_CAP);
    assert_eq!(mint(token, ALICE, BOB, 599), Ok(999));
    assert_eq!(revert_reason(mint(token, ALICE, CAROL, 2)), ERR_ABOVE_CAP);
    assert_eq!(mint(token, ALICE, CAROL, 1), Ok(1_000));
    // One past the cap, and far past it
    for amount in [1, u128::MAX] {
        assert_eq!(
            revert_reason(mint(token, ALICE, BOB, amount)),
            ERR_ABOVE_CAP
        );
    }
    assert_eq!(
        token_events(token),
        vec![mint_event(BOB, 599), mint_event(CAROL, 1)]
    );
    let state = info(token);
    assert_eq!(
        (state.supply, state.minted, state.burned),
        (1_000, 1_000, 0)
    );
    assert_eq!(mock::balance(&token.asset, &BOB), 599);
    assert_eq!(mock::balance(&token.asset, &CAROL), 1);
    assert_consistent(token, &[ALICE, BOB, CAROL]);
}

#[test]
fn only_the_owner_mints() {
    let token = setup(None, 100);
    mock::clear_events();
    for who in [BOB, CAROL, ZERO, token.id] {
        assert_eq!(revert_reason(mint(token, who, BOB, 1)), ERR_NOT_OWNER);
    }
    // A contract that is not the owner is refused too
    assert_eq!(
        mock::call(BOB, HELPER, "mint", &encode(&(token.id, BOB, 1u128))),
        Err(Failure::Revert {
            contract: token.id,
            reason: ERR_NOT_OWNER.to_vec()
        })
    );
    assert!(token_events(token).is_empty());
    assert_eq!(info(token).supply, 100);
    assert_consistent(token, &[ALICE, BOB]);
}

#[test]
fn mint_refuses_bad_receivers_amounts_and_funds() {
    let token = setup(None, 100);
    mock::clear_events();
    assert_eq!(
        revert_reason(mint(token, ALICE, ZERO, 1)),
        b"account must not be zero"
    );
    assert_eq!(
        revert_reason(mint(token, ALICE, token.id, 1)),
        b"account must not be this contract"
    );
    assert_eq!(
        revert_reason(mint(token, ALICE, BOB, 0)),
        b"amount must be positive"
    );
    // Funds are refused, the token itself too, and come back with the failed operation
    let own = Funds {
        asset: token.asset,
        amount: 1,
    };
    for funds in [ksa(1), own] {
        let args = encode(&(BOB, 1u128));
        let result = mock::call_with_funds(ALICE, token.id, "mint", &args, &[funds]);
        assert_eq!(revert_reason(result), ERR_UNEXPECTED_FUNDS);
    }
    assert_eq!(mock::balance(&KSA_ASSET_ID, &ALICE), KSA_EACH);
    assert_eq!(mock::balance(&token.asset, &ALICE), 100);
    for args in [
        encode(&BOB),
        encode(&(BOB, 1u64)),
        encode(&(BOB, 1u128, 0u8)),
    ] {
        let result = mock::call(ALICE, token.id, "mint", &args);
        assert_eq!(revert_reason(result), ERR_MALFORMED_ARGS);
    }
    assert!(token_events(token).is_empty());
    assert_consistent(token, &[ALICE, BOB]);
}

#[test]
fn a_contract_can_own_the_token() {
    let token = setup(Some(1_000), 0);
    set_owner(token, ALICE, Some(HELPER)).unwrap();
    assert_eq!(revert_reason(mint(token, ALICE, ALICE, 1)), ERR_NOT_OWNER);
    // Whoever calls the helper, the token sees the helper as its caller
    let args = encode(&(token.id, BOB, 700u128));
    let supply: u128 = decode(&mock::call(CAROL, HELPER, "mint", &args).unwrap()).unwrap();
    assert_eq!(supply, 700);
    assert_eq!(mock::balance(&token.asset, &BOB), 700);
    assert_eq!(info(token).owner, Some(HELPER));
    assert_consistent(token, &[ALICE, BOB]);
}

#[test]
fn the_supply_cannot_overflow_without_a_cap() {
    let token = setup(None, u128::MAX - 1);
    assert_eq!(mint(token, ALICE, BOB, 1), Ok(u128::MAX));
    for amount in [1, u128::MAX] {
        assert_eq!(revert_reason(mint(token, ALICE, BOB, amount)), ERR_OVERFLOW);
    }
    assert_eq!(
        mock::asset_info(&token.asset).unwrap().total_supply,
        u128::MAX
    );
    assert_consistent(token, &[ALICE, BOB]);

    // From nothing: all of u128 in one mint, then not one unit more
    let token = deploy_as(ALICE, 1, "Kite", 8, None, 0).unwrap();
    assert_eq!(mint(token, ALICE, BOB, u128::MAX), Ok(u128::MAX));
    assert_eq!(revert_reason(mint(token, ALICE, CAROL, 1)), ERR_OVERFLOW);
    assert_consistent(token, &[BOB, CAROL]);

    // With a cap of u128::MAX, the cap answers first
    let token = deploy_as(ALICE, 2, "Kite", 8, Some(u128::MAX), u128::MAX).unwrap();
    assert_eq!(revert_reason(mint(token, ALICE, BOB, 1)), ERR_ABOVE_CAP);
}

#[test]
fn minted_keeps_counting_after_burns() {
    let token = setup(None, u128::MAX);
    assert_eq!(burn(token, ALICE, 1_000), Ok(u128::MAX - 1_000));
    // The supply has room again, but `minted` cannot pass u128::MAX
    assert_eq!(revert_reason(mint(token, ALICE, BOB, 1)), ERR_OVERFLOW);
    let state = info(token);
    assert_eq!(
        (state.minted, state.burned, state.supply),
        (u128::MAX, 1_000, u128::MAX - 1_000)
    );
    assert_consistent(token, &[ALICE, BOB]);
}

// ---------------------------------------------------------------------------
// Burning

#[test]
fn holders_burn_by_attaching_the_token() {
    let token = setup(None, 1_000);
    mint(token, ALICE, BOB, 500).unwrap();
    mock::clear_events();
    // BOB holds the token without owning the contract
    assert_eq!(burn(token, BOB, 200), Ok(1_300));
    assert_eq!(mock::balance(&token.asset, &BOB), 300);
    assert_eq!(mock::asset_info(&token.asset).unwrap().total_supply, 1_300);
    // All of a balance
    assert_eq!(burn(token, BOB, 300), Ok(1_000));
    assert_eq!(mock::balance(&token.asset, &BOB), 0);
    // The owner burns like any holder; one unit
    assert_eq!(burn(token, ALICE, 1), Ok(999));
    assert_eq!(
        token_events(token),
        vec![
            burn_event(BOB, 200),
            burn_event(BOB, 300),
            burn_event(ALICE, 1)
        ]
    );
    let state = info(token);
    assert_eq!(
        (state.minted, state.burned, state.supply),
        (1_500, 501, 999)
    );
    assert_consistent(token, &[ALICE, BOB]);
    // A contract that holds the token burns through `burn` too, and the event names the contract
    mint(token, ALICE, HELPER, 50).unwrap();
    mock::clear_events();
    let args = encode(&(token.id, token.asset, 20u128));
    let out = mock::call(CAROL, HELPER, "burn_through_token", &args).unwrap();
    assert_eq!(decode::<u128>(&out), Ok(1_029));
    assert_eq!(token_events(token), vec![burn_event(HELPER, 20)]);
    assert_consistent(token, &[ALICE, BOB, HELPER]);
}

#[test]
fn burn_takes_this_token_alone() {
    let token = setup(None, 1_000);
    mock::register_asset(OTHER, "Other", 0, ZERO);
    mock::mint(OTHER, ALICE, 50).unwrap();
    mock::clear_events();
    let own = |amount| Funds {
        asset: token.asset,
        amount,
    };
    let other = |amount| Funds {
        asset: OTHER,
        amount,
    };
    let held = || [KSA_ASSET_ID, OTHER, token.asset].map(|asset| mock::balance(&asset, &ALICE));
    let before = held();
    for mut funds in [
        vec![],
        vec![ksa(10)],
        vec![other(10)],
        vec![ksa(10), own(10)],
        vec![own(10), other(10)],
        vec![ksa(10), own(10), other(10)],
    ] {
        // Attached funds come in ascending order of asset ID
        funds.sort_by_key(|f| f.asset);
        let result = burn_with(token, ALICE, &funds);
        assert_eq!(revert_reason(result), ERR_ATTACH_TOKEN, "{funds:?}");
        // and come back with the failed operation
        assert_eq!(held(), before);
    }
    // Any arguments
    let result = mock::call_with_funds(ALICE, token.id, "burn", &[0], &[own(1)]);
    assert_eq!(revert_reason(result), ERR_MALFORMED_ARGS);
    // More than the caller holds: the VM cannot move the attached funds, and the contract never runs
    assert!(matches!(burn(token, ALICE, 1_001), Err(Failure::Trap(_))));
    assert!(matches!(burn(token, BOB, 1), Err(Failure::Trap(_))));
    assert_eq!(held(), before);
    assert!(token_events(token).is_empty());
    assert_eq!(info(token).burned, 0);
    assert_consistent(token, &[ALICE]);
}

#[test]
fn burns_free_room_under_the_cap() {
    let token = setup(Some(1_000), 1_000);
    assert_eq!(revert_reason(mint(token, ALICE, BOB, 1)), ERR_ABOVE_CAP);
    assert_eq!(burn(token, ALICE, 250), Ok(750));
    assert_eq!(revert_reason(mint(token, ALICE, BOB, 251)), ERR_ABOVE_CAP);
    assert_eq!(mint(token, ALICE, BOB, 250), Ok(1_000));
    assert_eq!(revert_reason(mint(token, ALICE, BOB, 1)), ERR_ABOVE_CAP);
    let state = info(token);
    assert_eq!(
        (state.minted, state.burned, state.supply),
        (1_250, 250, 1_000)
    );
    assert_consistent(token, &[ALICE, BOB]);
}

#[test]
fn burns_outside_the_contract_leave_its_supply_an_upper_bound() {
    let token = setup(Some(1_000), 0);
    mint(token, ALICE, HELPER, 1_000).unwrap();
    // The helper burns 100 of its holdings directly: the token contract does not see it
    let args = encode(&(token.asset, 100u128));
    mock::call(CAROL, HELPER, "burn_directly", &args).unwrap();
    assert_eq!(mock::asset_info(&token.asset).unwrap().total_supply, 900);
    let state = info(token);
    assert_eq!(
        (state.supply, state.minted, state.burned),
        (1_000, 1_000, 0)
    );
    // It only leaves less room under the cap: 900 exist, yet nothing more can be minted
    assert_eq!(revert_reason(mint(token, ALICE, BOB, 1)), ERR_ABOVE_CAP);
    // A burn through the token's `burn` is counted, and frees room
    let args = encode(&(token.id, token.asset, 300u128));
    let out = mock::call(CAROL, HELPER, "burn_through_token", &args).unwrap();
    assert_eq!(decode::<u128>(&out), Ok(700));
    assert_eq!(mint(token, ALICE, BOB, 300), Ok(1_000));
    // The supply stays the real total plus what was burned outside
    let total = mock::asset_info(&token.asset).unwrap().total_supply;
    assert_eq!((info(token).supply, total), (1_000, 900));
    assert_eq!(
        mock::balance(&token.asset, &HELPER) + mock::balance(&token.asset, &BOB),
        total
    );
}

// ---------------------------------------------------------------------------
// Ownership

#[test]
fn ownership_changes_hands_and_can_be_given_up() {
    let token = setup(Some(10_000), 100);
    mock::clear_events();
    assert_eq!(set_owner(token, ALICE, Some(BOB)), Ok(()));
    assert_eq!(info(token).owner, Some(BOB));
    // The old owner can neither mint nor hand ownership on
    assert_eq!(revert_reason(mint(token, ALICE, ALICE, 1)), ERR_NOT_OWNER);
    assert_eq!(
        revert_reason(set_owner(token, ALICE, Some(ALICE))),
        ERR_NOT_OWNER
    );
    assert_eq!(mint(token, BOB, CAROL, 10), Ok(110));
    // Handing ownership to the owner itself changes nothing, and is recorded
    assert_eq!(set_owner(token, BOB, Some(BOB)), Ok(()));
    assert_eq!(set_owner(token, BOB, None), Ok(()));
    assert_eq!(
        token_events(token),
        vec![
            owner_event(Some(ALICE), Some(BOB)),
            mint_event(CAROL, 10),
            owner_event(Some(BOB), Some(BOB)),
            owner_event(Some(BOB), None),
        ]
    );
    assert_eq!(info(token).owner, None);
    assert_eq!(mock::storage_value(&token.id, b"owner"), None);
    // The asset's capability to mint is given up with it, so the protocol refuses any issuance too (D-121)
    assert_eq!(mock::asset_info(&token.asset).unwrap().capabilities, 0);
    // Nobody can mint again, or take ownership back
    for who in [ALICE, BOB, CAROL] {
        assert_eq!(revert_reason(mint(token, who, who, 1)), ERR_RENOUNCED);
        assert_eq!(
            revert_reason(set_owner(token, who, Some(who))),
            ERR_RENOUNCED
        );
        assert_eq!(revert_reason(set_owner(token, who, None)), ERR_RENOUNCED);
    }
    // Burns still work: from now on the supply only goes down
    assert_eq!(burn(token, CAROL, 10), Ok(100));
    assert_eq!(info(token).supply, 100);
    assert_consistent(token, &[ALICE, BOB, CAROL]);
}

#[test]
fn set_owner_refuses_bad_owners_callers_and_funds() {
    let token = setup(None, 100);
    mock::clear_events();
    assert_eq!(
        revert_reason(set_owner(token, ALICE, Some(ZERO))),
        b"account must not be zero"
    );
    assert_eq!(
        revert_reason(set_owner(token, ALICE, Some(token.id))),
        b"account must not be this contract"
    );
    for owner in [Some(BOB), Some(CAROL), None] {
        assert_eq!(revert_reason(set_owner(token, BOB, owner)), ERR_NOT_OWNER);
    }
    let args = encode(&Some(BOB));
    let result = mock::call_with_funds(ALICE, token.id, "set_owner", &args, &[ksa(1)]);
    assert_eq!(revert_reason(result), ERR_UNEXPECTED_FUNDS);
    assert_eq!(mock::balance(&KSA_ASSET_ID, &ALICE), KSA_EACH);
    // Malformed: an option tag of 2, a short account, trailing bytes
    for args in [
        vec![2],
        [vec![1], vec![7; 31]].concat(),
        [encode(&Some(BOB)), vec![0]].concat(),
    ] {
        let result = mock::call(ALICE, token.id, "set_owner", &args);
        assert_eq!(revert_reason(result), ERR_MALFORMED_ARGS);
    }
    assert_eq!(info(token).owner, Some(ALICE));
    assert!(token_events(token).is_empty());
}

// ---------------------------------------------------------------------------
// Views

#[test]
fn views_answer_from_the_ledger_and_write_nothing() {
    let token = setup(Some(1_000_000), 600);
    mint(token, ALICE, BOB, 300).unwrap();
    burn(token, ALICE, 100).unwrap();
    let ledger = mock::asset_info(&token.asset).unwrap();
    let raw = mock::call(BOB, token.id, "info", &[]).unwrap();
    let answer: Info = decode(&raw).unwrap();
    assert_eq!(
        answer,
        Info {
            asset: token.asset,
            name: ledger.name,
            symbol: ledger.symbol,
            decimals: ledger.decimals,
            owner: Some(ALICE),
            cap: Some(1_000_000),
            supply: ledger.total_supply,
            minted: 900,
            burned: 100,
        }
    );
    // `Info` encodes as `info` returns it
    assert_eq!(encode(&answer), raw);
    let asset: [u8; 32] = decode(&mock::call(BOB, token.id, "asset", &[]).unwrap()).unwrap();
    assert_eq!(asset, token.asset);
    for account in [ALICE, BOB, CAROL, ZERO, token.id] {
        assert_eq!(
            balance_of(token, account),
            mock::balance(&token.asset, &account)
        );
    }
    assert_eq!(
        (balance_of(token, ALICE), balance_of(token, BOB)),
        (500, 300)
    );

    // Views write nothing and emit nothing
    let stored = || {
        ["meta", "owner", "cap", "minted", "burned"]
            .map(|key| mock::storage_value(&token.id, key.as_bytes()))
    };
    let before = stored();
    mock::clear_events();
    let views = [
        ("info", Vec::new()),
        ("asset", Vec::new()),
        ("balance_of", encode(&BOB)),
    ];
    for (method, args) in &views {
        mock::call(BOB, token.id, method, args).unwrap();
    }
    assert_eq!(stored(), before);
    assert!(mock::events().is_empty());
    // They refuse attached funds, and any arguments but their own
    for (method, args) in &views {
        let result = mock::call_with_funds(BOB, token.id, method, args, &[ksa(1)]);
        assert_eq!(revert_reason(result), ERR_UNEXPECTED_FUNDS, "{method}");
        let mut longer = args.clone();
        longer.push(0);
        let result = mock::call(BOB, token.id, method, &longer);
        assert_eq!(revert_reason(result), ERR_MALFORMED_ARGS, "{method}");
    }
    assert_eq!(mock::balance(&KSA_ASSET_ID, &BOB), KSA_EACH);
}

// ---------------------------------------------------------------------------
// Invariants

/// Random mints, burns, sends between holders and hand-overs of ownership, each checked against a model of the counts
/// and the owner, and the contract's accounting checked against the ledger after every step.
#[test]
fn random_operations_keep_the_accounting() {
    const CAP: u128 = 1_000_000;
    let token = setup(Some(CAP), 10_000);
    let accounts = [ALICE, BOB, CAROL];
    let (mut minted, mut burned, mut owner) = (10_000u128, 0u128, ALICE);
    let (mut mints, mut burns, mut sends, mut handovers) = (0u32, 0u32, 0u32, 0u32);
    let mut seed: u64 = 11;
    let mut next = |bound: u64| {
        seed = seed
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (seed >> 11) % bound
    };
    for _ in 0..400 {
        let who = accounts[usize::try_from(next(3)).unwrap()];
        let to = accounts[usize::try_from(next(3)).unwrap()];
        let held = mock::balance(&token.asset, &who);
        match next(4) {
            0 => {
                let amount = u128::from(next(100_000));
                let result = mint(token, who, to, amount);
                if who != owner {
                    assert_eq!(revert_reason(result), ERR_NOT_OWNER);
                } else if amount == 0 {
                    assert_eq!(revert_reason(result), b"amount must be positive");
                } else if minted - burned + amount > CAP {
                    assert_eq!(revert_reason(result), ERR_ABOVE_CAP);
                } else {
                    minted += amount;
                    mints += 1;
                    assert_eq!(result, Ok(minted - burned));
                }
            }
            1 if held > 0 => {
                let amount = u128::from(next(u64::try_from(held).unwrap())) + 1;
                burned += amount;
                burns += 1;
                assert_eq!(burn(token, who, amount), Ok(minted - burned));
            }
            2 if held > 0 => {
                // `who` sends some of its tokens to `to`, as the built-in `AssetTransfer` would
                let amount = u128::from(next(u64::try_from(held).unwrap())) + 1;
                let funds = [Funds {
                    asset: token.asset,
                    amount,
                }];
                mock::call_with_funds(who, HELPER, "send", &encode(&to), &funds).unwrap();
                sends += 1;
            }
            3 => {
                let result = set_owner(token, who, Some(to));
                if who == owner {
                    assert_eq!(result, Ok(()));
                    owner = to;
                    handovers += 1;
                } else {
                    assert_eq!(revert_reason(result), ERR_NOT_OWNER);
                }
            }
            _ => {}
        }
        let state = info(token);
        assert_eq!(
            (state.minted, state.burned, state.owner),
            (minted, burned, Some(owner))
        );
        assert_consistent(token, &accounts);
    }
    // Every kind of operation happened
    assert!(mints > 0 && burns > 0 && sends > 0 && handovers > 0);
}

// ---------------------------------------------------------------------------
// The ABI block

const SOURCE: &str = include_str!("../src/lib.rs");

/// The declarations of the ABI block in the module docs, without the `//!` prefix and their descriptions.
fn abi_declarations() -> Vec<&'static str> {
    let opening = "//! ```abi\n";
    let start = SOURCE.find(opening).unwrap() + opening.len();
    let block = &SOURCE[start..];
    let block = &block[..block.find("//! ```\n").unwrap()];
    block
        .lines()
        .map(|line| {
            let line = line.strip_prefix("//!").unwrap();
            line.split("//").next().unwrap().trim()
        })
        .collect()
}

/// The names of the methods the ABI block declares: its `init`, `call` and `view` lines.
fn declared_methods() -> BTreeSet<String> {
    abi_declarations()
        .into_iter()
        .filter_map(|line| {
            let line = line.strip_prefix("payable ").unwrap_or(line);
            let line = line
                .strip_prefix("call ")
                .or_else(|| line.strip_prefix("view "))
                .or_else(|| line.starts_with("init(").then_some(line))?;
            Some(line.split('(').next().unwrap().to_string())
        })
        .collect()
}

/// The names of the events the ABI block declares.
fn declared_events() -> BTreeSet<String> {
    abi_declarations()
        .into_iter()
        .filter_map(|line| Some(line.strip_prefix("event ")?.split('(').next()?.to_string()))
        .collect()
}

/// The names of the `"name" =>` arms of `dispatch`.
fn dispatched_methods() -> BTreeSet<String> {
    let body = &SOURCE[SOURCE.find("pub fn dispatch(").unwrap()..];
    let body = &body[..body.find("\n}\n").unwrap()];
    body.lines()
        .filter_map(|line| {
            let (name, rest) = line.trim().strip_prefix('"')?.split_once('"')?;
            rest.trim_start()
                .starts_with("=>")
                .then(|| name.to_string())
        })
        .collect()
}

/// The names of the events the source emits: the first field of every `sdk::emit(&("name", ...))`.
fn emitted_events() -> BTreeSet<String> {
    SOURCE
        .split("sdk::emit(&(")
        .skip(1)
        .map(|rest| {
            let rest = rest.trim_start().strip_prefix('"').unwrap();
            rest.split('"').next().unwrap().to_string()
        })
        .collect()
}

/// The field names of `struct Info` in the ABI block, in order.
fn declared_info_fields() -> Vec<String> {
    let line = abi_declarations()
        .into_iter()
        .find(|line| line.starts_with("struct Info {"))
        .unwrap();
    let fields = line
        .strip_prefix("struct Info {")
        .unwrap()
        .strip_suffix('}')
        .unwrap();
    fields
        .split(',')
        .map(|field| field.split(':').next().unwrap().trim().to_string())
        .collect()
}

/// The field names of the Rust `Info`, in order.
fn rust_info_fields() -> Vec<String> {
    let body = &SOURCE[SOURCE.find("pub struct Info {").unwrap()..];
    let body = &body[..body.find("\n}\n").unwrap()];
    body.lines()
        .skip(1)
        .filter_map(|line| {
            Some(
                line.trim()
                    .strip_prefix("pub ")?
                    .split(':')
                    .next()?
                    .to_string(),
            )
        })
        .collect()
}

#[test]
fn the_abi_block_matches_dispatch() {
    let methods = declared_methods();
    let expected: BTreeSet<String> = [
        "init",
        "mint",
        "burn",
        "set_owner",
        "info",
        "asset",
        "balance_of",
    ]
    .map(String::from)
    .into();
    assert_eq!(methods, expected);
    assert_eq!(dispatched_methods(), methods);
    let events: BTreeSet<String> = ["mint", "burn", "owner"].map(String::from).into();
    assert_eq!(declared_events(), events);
    assert_eq!(emitted_events(), events);
    assert_eq!(declared_info_fields(), rust_info_fields());
    assert_eq!(rust_info_fields().len(), 9);
    // `dispatch` knows every declared name: with empty arguments it reverts, but never with `unknown method`
    mock::reset();
    let unknown = Err(b"unknown method".to_vec());
    let answer = |method: &str| {
        mock::with_context(ALICE, [0x70; 32], || mintable_token::dispatch(method, &[]))
    };
    for method in &methods {
        assert_ne!(answer(method), unknown, "{method}");
    }
    assert_eq!(answer("transfer"), unknown);
}
