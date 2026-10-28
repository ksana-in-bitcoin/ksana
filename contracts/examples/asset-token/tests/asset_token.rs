//! Tests of the asset-token contract on the SDK's in-memory mock host: the issuance at deployment, and the issued asset
//! traded against KSA on the AMM example, the flow of the Wizards demo on the public signet.
#![cfg(not(target_arch = "wasm32"))]
#![allow(
    missing_docs,
    clippy::unwrap_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]

use amm::math::swap_output;
use amm::AssetRef;
use asset_token::SUB_ID;
use ksana_contract_sdk::mock::{self, Failure};
use ksana_contract_sdk::{decode, encode, Funds, KSA_ASSET_ID};

const ALICE: [u8; 32] = [0xa1; 32];
const BOB: [u8; 32] = [0xb0; 32];

/// 21 million units of a token with 8 decimals.
const SUPPLY: u128 = 21_000_000 * 100_000_000;

fn deploy(nonce: u64, name: &str, decimals: u8, supply: u128) -> Result<[u8; 32], Failure> {
    deploy_as(nonce, name, "WIZ", decimals, supply)
}

fn deploy_as(
    nonce: u64,
    name: &str,
    symbol: &str,
    decimals: u8,
    supply: u128,
) -> Result<[u8; 32], Failure> {
    mock::deploy(
        ALICE,
        nonce,
        b"asset-token",
        asset_token::dispatch,
        &encode(&(name, symbol, decimals, supply)),
    )
}

fn revert_reason<T: std::fmt::Debug>(result: Result<T, Failure>) -> Vec<u8> {
    match result {
        Err(Failure::Revert { reason, .. }) => reason,
        other => panic!("expected a revert, got {other:?}"),
    }
}

#[test]
fn init_issues_the_supply_to_the_deployer() {
    mock::reset();
    let token = deploy(0, "Wizards", 8, SUPPLY).unwrap();
    let asset = ksana_contract_sdk::asset_id(&token, &SUB_ID);
    let returned: [u8; 32] = decode(&mock::call(BOB, token, "asset_id", &[]).unwrap()).unwrap();
    assert_eq!(returned, asset);

    assert_eq!(mock::balance(&asset, &ALICE), SUPPLY);
    assert_eq!(mock::balance(&asset, &token), 0);
    let info = mock::asset_info(&asset).unwrap();
    assert_eq!(
        (
            info.name.as_str(),
            info.symbol.as_str(),
            info.decimals,
            info.total_supply,
            info.issuer
        ),
        ("Wizards", "WIZ", 8, SUPPLY, token)
    );
    // No capabilities: the protocol keeps the supply fixed (D-121)
    assert_eq!((info.capabilities, info.paused), (0, false));
    let events = mock::events();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].contract, token);
    assert_eq!(events[0].data, encode(&("issue", asset, ALICE, SUPPLY)));

    // `init` runs only at deployment
    assert!(mock::call(
        ALICE,
        token,
        "init",
        &encode(&("Wizards", "WIZ", 8u8, 1u128))
    )
    .is_err());
    assert_eq!(mock::asset_info(&asset).unwrap().total_supply, SUPPLY);
}

#[test]
fn deployments_issue_separate_assets() {
    mock::reset();
    let first = deploy(0, "Wizards", 8, SUPPLY).unwrap();
    let second = deploy(1, "Wizards", 8, 5).unwrap();
    let (a, b) = (
        ksana_contract_sdk::asset_id(&first, &SUB_ID),
        ksana_contract_sdk::asset_id(&second, &SUB_ID),
    );
    assert_ne!(a, b);
    assert_eq!(mock::balance(&a, &ALICE), SUPPLY);
    assert_eq!(mock::balance(&b, &ALICE), 5);
}

#[test]
fn bad_deployments_fail_and_leave_nothing() {
    mock::reset();
    assert_eq!(
        revert_reason(deploy(0, "Wizards", 8, 0)),
        b"supply must be positive"
    );
    // Metadata over the host's limits traps: a name over 64 bytes, a symbol over 16, more than 18 decimals
    let long = "w".repeat(65);
    assert!(matches!(deploy(1, &long, 8, SUPPLY), Err(Failure::Trap(_))));
    assert!(matches!(
        deploy_as(1, "Wizards", "WIZARDSWIZARDSWIZ", 8, SUPPLY),
        Err(Failure::Trap(_))
    ));
    assert!(matches!(
        deploy(1, "Wizards", 19, SUPPLY),
        Err(Failure::Trap(_))
    ));
    // Malformed init arguments: the symbol missing
    let result = mock::deploy(
        ALICE,
        2,
        b"asset-token",
        asset_token::dispatch,
        &encode(&("Wizards", 8u8, SUPPLY)),
    );
    assert!(result.is_err());
    for nonce in 0..3 {
        let id = ksana_contract_sdk::contract_id(
            &ALICE,
            nonce,
            &ksana_contract_sdk::sha256(b"asset-token"),
        );
        assert!(!mock::contract_exists(&id));
        assert!(mock::asset_info(&ksana_contract_sdk::asset_id(&id, &SUB_ID)).is_none());
    }
    assert!(mock::events().is_empty());
}

#[test]
fn the_asset_trades_against_ksa_on_the_amm() {
    mock::reset();
    mock::register_asset(KSA_ASSET_ID, "KSA", 8, [0; 32]);
    mock::mint(KSA_ASSET_ID, ALICE, 100 * 100_000_000).unwrap();
    mock::mint(KSA_ASSET_ID, BOB, 100 * 100_000_000).unwrap();
    let token = deploy(0, "Wizards", 8, SUPPLY).unwrap();
    let wiz = ksana_contract_sdk::asset_id(&token, &SUB_ID);
    let pool = mock::deploy(
        ALICE,
        1,
        b"amm",
        amm::dispatch,
        &encode(&(AssetRef::Asset(wiz), AssetRef::Asset(KSA_ASSET_ID))),
    )
    .unwrap();
    let funds = |asset, amount| Funds { asset, amount };

    // Liquidity: 1 000 000 WIZ and 50 KSA, both attached (KSA's ID is the smallest, so it goes first)
    let (wiz_in, ksa_in) = (1_000_000 * 100_000_000u128, 50 * 100_000_000u128);
    let shares: u128 = decode(
        &mock::call_with_funds(
            ALICE,
            pool,
            "add_liquidity",
            &encode(&(wiz_in, ksa_in)),
            &[funds(KSA_ASSET_ID, ksa_in), funds(wiz, wiz_in)],
        )
        .unwrap(),
    )
    .unwrap();
    assert!(shares > 0);
    assert_eq!(mock::balance(&wiz, &ALICE), SUPPLY - wiz_in);
    assert_eq!(mock::balance(&wiz, &pool), wiz_in);
    assert_eq!(mock::balance(&KSA_ASSET_ID, &pool), ksa_in);

    // Bob swaps 5 KSA for WIZ: the output is a plain balance of the built-in asset
    let amount = 5 * 100_000_000u128;
    let expected = swap_output(amount, ksa_in, wiz_in).unwrap();
    let out: u128 = decode(
        &mock::call_with_funds(
            BOB,
            pool,
            "swap",
            &encode(&(KSA_ASSET_ID, amount, expected)),
            &[funds(KSA_ASSET_ID, amount)],
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(out, expected);
    assert_eq!(mock::balance(&wiz, &BOB), expected);
    assert_eq!(mock::balance(&KSA_ASSET_ID, &BOB), 95 * 100_000_000);
    let reserves: (u128, u128) = decode(&mock::call(BOB, pool, "reserves", &[]).unwrap()).unwrap();
    assert_eq!(reserves, (wiz_in - expected, ksa_in + amount));
    assert_eq!(mock::balance(&wiz, &pool), wiz_in - expected);
    // The supply is unchanged: the swap moved the asset, nothing was issued
    assert_eq!(mock::asset_info(&wiz).unwrap().total_supply, SUPPLY);
}
