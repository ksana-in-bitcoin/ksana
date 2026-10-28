//! Tests of the factory contract on the SDK's in-memory mock host: creating an AMM pair with `create_contract` and
//! trading on it.
#![cfg(not(target_arch = "wasm32"))]
#![allow(missing_docs, clippy::unwrap_used, clippy::panic)]

use amm_factory::{canonical, pair_salt, Side};
use ksana_contract_sdk::mock::{self, Failure};
use ksana_contract_sdk::{created_contract_id, decode, encode, Funds, KSA_ASSET_ID};

const ALICE: [u8; 32] = [0xa1; 32];
const SUPPLY: u128 = 1_000_000_000;

fn revert_reason(result: Result<Vec<u8>, Failure>) -> Vec<u8> {
    match result {
        Err(Failure::Revert { reason, .. }) => reason,
        other => panic!("expected a revert, got {other:?}"),
    }
}

#[test]
fn factory_creates_each_pair_once_at_a_predictable_address() {
    mock::reset();
    mock::register_asset(KSA_ASSET_ID, "KSA", 8, [0; 32]);
    mock::mint(KSA_ASSET_ID, ALICE, SUPPLY).unwrap();
    let amm_code = mock::register_code(b"amm", amm::dispatch);
    let token = mock::deploy(
        ALICE,
        0,
        b"token",
        token::dispatch,
        &encode(&("Token", "T", 8u8, SUPPLY)),
    )
    .unwrap();
    let factory = mock::deploy(
        ALICE,
        1,
        b"factory",
        amm_factory::dispatch,
        &encode(&amm_code),
    )
    .unwrap();

    let (a, b) = (Side::Token(token), Side::Asset(KSA_ASSET_ID));
    let pair: [u8; 32] =
        decode(&mock::call(ALICE, factory, "create_pair", &encode(&(a, b))).unwrap()).unwrap();
    assert_eq!(
        pair,
        created_contract_id(&factory, &pair_salt(a, b), &amm_code)
    );
    // Found with the sides in either order; KSA (ID 0) sorts first
    for (x, y) in [(a, b), (b, a)] {
        let found: Option<[u8; 32]> =
            decode(&mock::call(ALICE, factory, "get_pair", &encode(&(x, y))).unwrap()).unwrap();
        assert_eq!(found, Some(pair));
    }
    assert_eq!(canonical(a, b), (b, a));
    let sides: (amm::AssetRef, amm::AssetRef) =
        decode(&mock::call(ALICE, pair, "tokens", &[]).unwrap()).unwrap();
    assert_eq!(
        sides,
        (
            amm::AssetRef::Asset(KSA_ASSET_ID),
            amm::AssetRef::Token(token)
        )
    );
    let count: u64 = decode(&mock::call(ALICE, factory, "pair_count", &[]).unwrap()).unwrap();
    assert_eq!(count, 1);
    let first: Option<[u8; 32]> =
        decode(&mock::call(ALICE, factory, "pair_at", &encode(&0u64)).unwrap()).unwrap();
    assert_eq!(first, Some(pair));

    // The same pair (in either order) cannot be created again; identical sides are rejected
    assert_eq!(
        revert_reason(mock::call(ALICE, factory, "create_pair", &encode(&(b, a)))),
        b"pair already exists"
    );
    assert_eq!(
        revert_reason(mock::call(ALICE, factory, "create_pair", &encode(&(a, a)))),
        b"side_a and side_b must differ"
    );

    // The created pair can be used directly: the KSA side takes attached funds, the token side an allowance
    mock::call(ALICE, token, "approve", &encode(&(pair, u128::MAX))).unwrap();
    let shares: u128 = decode(
        &mock::call_with_funds(
            ALICE,
            pair,
            "add_liquidity",
            &encode(&(1_000u128, 4_000u128)),
            &[Funds {
                asset: KSA_ASSET_ID,
                amount: 1_000,
            }],
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(shares, 2_000);
    assert_eq!(mock::balance(&KSA_ASSET_ID, &pair), 1_000);
}
