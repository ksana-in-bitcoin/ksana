//! Tests of the AMM contract on the SDK's in-memory mock host: the integer arithmetic, and the interaction with the
//! real token contract logic.
#![cfg(not(target_arch = "wasm32"))]
#![allow(
    missing_docs,
    clippy::unwrap_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::cast_possible_truncation
)]

use amm::math::{deposit, isqrt, swap_output, withdraw};
use amm::{AssetRef, PoolAction};
use ksana_contract_sdk::mock::{self, Failure};
use ksana_contract_sdk::{decode, encode, Funds, KSA_ASSET_ID};

const ALICE: [u8; 32] = [0xa1; 32];
const BOB: [u8; 32] = [0xb0; 32];
const SUPPLY: u128 = 1_000_000_000_000;

// ---------------------------------------------------------------------------
// Pure integer arithmetic

fn check_isqrt(n: u128) {
    let r = isqrt(n);
    assert!(
        r.checked_mul(r).is_some_and(|sq| sq <= n),
        "isqrt({n}) = {r} too big"
    );
    let next = r + 1;
    assert!(
        next.checked_mul(next).is_none_or(|sq| sq > n),
        "isqrt({n}) = {r} too small"
    );
}

#[test]
fn isqrt_is_floor_sqrt() {
    for n in 0..=100_000u128 {
        check_isqrt(n);
    }
    for k in 0..128u32 {
        let p = 1u128 << k;
        check_isqrt(p);
        check_isqrt(p - 1);
        check_isqrt(p + 1);
    }
    for x in [
        2u128,
        3,
        1_000_003,
        u128::from(u32::MAX),
        u128::from(u64::MAX),
    ] {
        check_isqrt(x * x);
        check_isqrt(x * x - 1);
        check_isqrt(x * x + 1);
    }
    assert_eq!(isqrt(u128::MAX), u128::from(u64::MAX));
    let mut seed: u128 = 0x9e37_79b9_7f4a_7c15;
    for _ in 0..10_000 {
        seed = seed
            .wrapping_mul(0x2545_f491_4f6c_dd1d_2545_f491_4f6c_dd1d)
            .wrapping_add(1);
        check_isqrt(seed);
        check_isqrt(seed >> (seed % 128));
    }
}

#[test]
fn swap_output_formula() {
    // 997·1000·1_000_000 / (1_000_000·1000 + 1000·997) = 996_006_981_000 / 1_000_997_000 = 995.00…
    assert_eq!(swap_output(1_000, 1_000_000, 1_000_000), Some(996));
    assert_eq!(swap_output(0, 10, 10), Some(0));
    assert_eq!(swap_output(1, 1_000, 1_000), Some(0));
    assert_eq!(swap_output(1, 0, 0), Some(0));
    assert_eq!(swap_output(0, 0, 5), None, "division by zero");
    assert_eq!(swap_output(u128::MAX, 1, 1), None, "overflow");
    // The output is always strictly less than the output-side reserve.
    for (a, r_in, r_out) in [
        (1u128, 1u128, 1u128),
        (10_000, 1, 1_000),
        (u64::MAX.into(), 7, 1_000_000),
    ] {
        let out = swap_output(a, r_in, r_out).unwrap();
        assert!(out < r_out);
    }
}

#[test]
fn deposit_and_withdraw_math() {
    // First deposit: the shares are the geometric mean and everything is pulled.
    assert_eq!(deposit(400, 900, 0, 0, 0), Some((600, 400, 900)));
    assert_eq!(deposit(u128::MAX, 2, 0, 0, 0), None, "product overflows");
    // Proportional: reserves 1000/2000, total shares 100. Given (500, 500), side b only covers 25 shares, so
    // ceil(25·1000/100) = 250 and ceil(25·2000/100) = 500 are actually pulled.
    assert_eq!(deposit(500, 500, 1_000, 2_000, 100), Some((25, 250, 500)));
    // A case that needs rounding up: reserves 10/10, total shares 3, given (4, 4) → 1 share, ceil(10/3) = 4 pulled.
    assert_eq!(deposit(4, 4, 10, 10, 3), Some((1, 4, 4)));
    // Zero shares.
    assert_eq!(deposit(1, 1, 1_000, 1_000, 10), Some((0, 0, 0)));
    // Withdrawal rounds down.
    assert_eq!(withdraw(1, 10, 20, 3), Some((3, 6)));
    assert_eq!(withdraw(3, 10, 20, 3), Some((10, 20)));
    assert_eq!(withdraw(1, 1, 1, 0), None);
}

/// A proportional deposit never dilutes existing shares: the reserves per share do not decrease after the deposit.
#[test]
fn deposits_never_dilute_existing_shares() {
    let mut seed: u64 = 42;
    let mut next = |bound: u64| {
        seed = seed
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        u128::from((seed >> 11) % bound + 1)
    };
    for _ in 0..5_000 {
        let (ra, rb, total) = (next(1 << 40), next(1 << 40), next(1 << 40));
        let (a, b) = (next(1 << 40), next(1 << 40));
        let (shares, used_a, used_b) = deposit(a, b, ra, rb, total).unwrap();
        assert!(used_a <= a && used_b <= b);
        // (ra + used_a) / (total + shares) >= ra / total, compared by cross-multiplying.
        assert!((ra + used_a) * total >= ra * (total + shares));
        assert!((rb + used_b) * total >= rb * (total + shares));
    }
}

// ---------------------------------------------------------------------------
// The contract

struct Pool {
    token_a: [u8; 32],
    token_b: [u8; 32],
    amm: [u8; 32],
}

fn setup() -> Pool {
    mock::reset();
    let token_a = mock::deploy(
        ALICE,
        0,
        b"token",
        token::dispatch,
        &encode(&("Token A", "A", 8u8, SUPPLY)),
    )
    .unwrap();
    let token_b = mock::deploy(
        ALICE,
        1,
        b"token",
        token::dispatch,
        &encode(&("Token B", "B", 8u8, SUPPLY)),
    )
    .unwrap();
    let amm = mock::deploy(
        ALICE,
        2,
        b"amm",
        amm::dispatch,
        &encode(&(AssetRef::Token(token_a), AssetRef::Token(token_b))),
    )
    .unwrap();
    // BOB holds both tokens too; both approve the AMM.
    for token in [token_a, token_b] {
        mock::call(ALICE, token, "transfer", &encode(&(BOB, SUPPLY / 2))).unwrap();
        for who in [ALICE, BOB] {
            mock::call(who, token, "approve", &encode(&(amm, u128::MAX))).unwrap();
        }
    }
    Pool {
        token_a,
        token_b,
        amm,
    }
}

fn balance(token: [u8; 32], account: [u8; 32]) -> u128 {
    decode(&mock::call(ALICE, token, "balance_of", &encode(&account)).unwrap()).unwrap()
}

fn reserves(pool: &Pool) -> (u128, u128) {
    decode(&mock::call(ALICE, pool.amm, "reserves", &[]).unwrap()).unwrap()
}

fn shares_of(pool: &Pool, account: [u8; 32]) -> u128 {
    decode(&mock::call(ALICE, pool.amm, "shares_of", &encode(&account)).unwrap()).unwrap()
}

fn total_shares(pool: &Pool) -> u128 {
    decode(&mock::call(ALICE, pool.amm, "total_shares", &[]).unwrap()).unwrap()
}

fn add(pool: &Pool, who: [u8; 32], a: u128, b: u128) -> Result<u128, Failure> {
    mock::call(who, pool.amm, "add_liquidity", &encode(&(a, b))).map(|out| decode(&out).unwrap())
}

fn remove(pool: &Pool, who: [u8; 32], shares: u128) -> Result<(u128, u128), Failure> {
    mock::call(who, pool.amm, "remove_liquidity", &encode(&shares)).map(|out| decode(&out).unwrap())
}

fn add_at_least(
    pool: &Pool,
    who: [u8; 32],
    a: u128,
    b: u128,
    min_shares: u128,
) -> Result<u128, Failure> {
    mock::call(who, pool.amm, "add_liquidity", &encode(&(a, b, min_shares)))
        .map(|out| decode(&out).unwrap())
}

fn remove_at_least(
    pool: &Pool,
    who: [u8; 32],
    shares: u128,
    min_a: u128,
    min_b: u128,
) -> Result<(u128, u128), Failure> {
    mock::call(
        who,
        pool.amm,
        "remove_liquidity",
        &encode(&(shares, min_a, min_b)),
    )
    .map(|out| decode(&out).unwrap())
}

fn swap(
    pool: &Pool,
    who: [u8; 32],
    token_in: [u8; 32],
    amount: u128,
    min_out: u128,
) -> Result<u128, Failure> {
    mock::call(who, pool.amm, "swap", &encode(&(token_in, amount, min_out)))
        .map(|out| decode(&out).unwrap())
}

fn revert_reason(result: Result<impl std::fmt::Debug, Failure>) -> Vec<u8> {
    match result {
        Err(Failure::Revert { reason, .. }) => reason,
        other => panic!("expected a revert, got {other:?}"),
    }
}

/// The reserves match the tokens the contract actually holds, and the total shares equal the sum of the holders'
/// shares.
fn assert_consistent(pool: &Pool) {
    let (ra, rb) = reserves(pool);
    assert_eq!(balance(pool.token_a, pool.amm), ra);
    assert_eq!(balance(pool.token_b, pool.amm), rb);
    assert_eq!(
        total_shares(pool),
        shares_of(pool, ALICE) + shares_of(pool, BOB)
    );
}

#[test]
fn init_records_pair_and_rejects_same_token() {
    let pool = setup();
    let tokens: (AssetRef, AssetRef) =
        decode(&mock::call(BOB, pool.amm, "tokens", &[]).unwrap()).unwrap();
    assert_eq!(
        tokens,
        (AssetRef::Token(pool.token_a), AssetRef::Token(pool.token_b))
    );
    assert_eq!(reserves(&pool), (0, 0));
    // Two sides with the same ID (whatever their kinds) are rejected
    for (nonce, pair) in [
        (
            3,
            (AssetRef::Token(pool.token_a), AssetRef::Token(pool.token_a)),
        ),
        (
            4,
            (AssetRef::Token(pool.token_a), AssetRef::Asset(pool.token_a)),
        ),
    ] {
        let result = mock::deploy(ALICE, nonce, b"amm", amm::dispatch, &encode(&pair));
        assert!(matches!(result, Err(Failure::Revert { .. })));
    }
}

#[test]
fn first_deposit_uses_geometric_mean() {
    let pool = setup();
    let shares = add(&pool, ALICE, 40_000, 90_000).unwrap();
    assert_eq!(shares, 60_000);
    assert_eq!(reserves(&pool), (40_000, 90_000));
    assert_eq!(shares_of(&pool, ALICE), 60_000);
    assert_eq!(balance(pool.token_a, ALICE), SUPPLY / 2 - 40_000);
    assert_eq!(balance(pool.token_b, ALICE), SUPPLY / 2 - 90_000);
    assert_consistent(&pool);
    let last = mock::events().last().unwrap().clone();
    assert_eq!(last.contract, pool.amm);
    assert_eq!(
        last.data,
        encode(&("add_liquidity", ALICE, 40_000u128, 90_000u128, 60_000u128))
    );
}

#[test]
fn later_deposits_only_pull_the_proportional_amounts() {
    let pool = setup();
    add(&pool, ALICE, 1_000, 2_000).unwrap(); // shares isqrt(2_000_000) = 1414
    let total = total_shares(&pool);
    assert_eq!(total, 1_414);
    let bob_a = balance(pool.token_a, BOB);
    let bob_b = balance(pool.token_b, BOB);
    // Given (500, 5_000): side a limits the shares to 500·1414/1000 = 707,
    // pulling a = ceil(707·1000/1414) = 500 and b = ceil(707·2000/1414) = 1000.
    let shares = add(&pool, BOB, 500, 5_000).unwrap();
    assert_eq!(shares, 707);
    assert_eq!(balance(pool.token_a, BOB), bob_a - 500);
    assert_eq!(balance(pool.token_b, BOB), bob_b - 1_000);
    assert_eq!(reserves(&pool), (1_500, 3_000));
    assert_consistent(&pool);
    // A deposit that is too small gets 0 shares: it reverts and transfers nothing.
    assert_eq!(
        revert_reason(add(&pool, BOB, 1, 1)),
        b"deposit too small".to_vec()
    );
    assert_eq!(
        revert_reason(add(&pool, BOB, 0, 10)),
        b"amount must be positive".to_vec()
    );
    assert_consistent(&pool);
}

#[test]
fn swap_charges_the_fee_and_keeps_k() {
    let pool = setup();
    add(&pool, ALICE, 1_000_000, 1_000_000).unwrap();
    let bob_a = balance(pool.token_a, BOB);
    let bob_b = balance(pool.token_b, BOB);
    let out = swap(&pool, BOB, pool.token_a, 1_000, 0).unwrap();
    assert_eq!(out, 996);
    assert_eq!(balance(pool.token_a, BOB), bob_a - 1_000);
    assert_eq!(balance(pool.token_b, BOB), bob_b + 996);
    let (ra, rb) = reserves(&pool);
    assert_eq!((ra, rb), (1_001_000, 999_004));
    assert!(ra * rb >= 1_000_000u128 * 1_000_000, "k must not decrease");
    assert_consistent(&pool);
    assert_eq!(
        mock::events().last().unwrap().data,
        encode(&("swap", BOB, pool.token_a, 1_000u128, 996u128))
    );
    // The other direction.
    let expected = swap_output(500, 999_004, 1_001_000).unwrap();
    assert_eq!(
        swap(&pool, BOB, pool.token_b, 500, expected).unwrap(),
        expected
    );
    assert_consistent(&pool);
}

#[test]
fn swap_failures_roll_back() {
    let pool = setup();
    add(&pool, ALICE, 1_000_000, 1_000_000).unwrap();
    let before = (
        reserves(&pool),
        balance(pool.token_a, BOB),
        balance(pool.token_b, BOB),
    );
    assert_eq!(
        revert_reason(swap(&pool, BOB, pool.token_a, 1_000, 997)),
        b"output below min_out".to_vec()
    );
    assert_eq!(
        revert_reason(swap(&pool, BOB, pool.token_a, 1, 0)),
        b"output amount is zero".to_vec()
    );
    assert_eq!(
        revert_reason(swap(&pool, BOB, [9; 32], 1_000, 0)),
        b"token is not in this pool".to_vec()
    );
    assert_eq!(
        revert_reason(swap(&pool, BOB, pool.token_a, 0, 0)),
        b"amount must be positive".to_vec()
    );
    assert_eq!(
        revert_reason(swap(&pool, BOB, pool.token_a, u128::MAX / 2, 0)),
        b"arithmetic overflow".to_vec()
    );
    // No allowance: the token contract's transfer_from reverts, the whole operation fails, and the reserves the AMM
    // already wrote are rolled back too.
    let carol = [0xc4; 32];
    mock::call(BOB, pool.token_a, "transfer", &encode(&(carol, 10_000u128))).unwrap();
    let result = swap(&pool, carol, pool.token_a, 1_000, 0);
    assert_eq!(
        result,
        Err(Failure::Revert {
            contract: pool.token_a,
            reason: b"insufficient allowance".to_vec()
        })
    );
    let bob_a_after_gift = before.1 - 10_000;
    assert_eq!(
        (
            reserves(&pool),
            balance(pool.token_a, BOB),
            balance(pool.token_b, BOB)
        ),
        (before.0, bob_a_after_gift, before.2)
    );
}

#[test]
fn swap_needs_liquidity() {
    let pool = setup();
    assert_eq!(
        revert_reason(swap(&pool, BOB, pool.token_a, 1_000, 0)),
        b"no liquidity".to_vec()
    );
}

#[test]
fn remove_liquidity_returns_proportional_amounts() {
    let pool = setup();
    add(&pool, ALICE, 1_000, 2_000).unwrap(); // 1414 shares
    add(&pool, BOB, 500, 1_000).unwrap(); // 707 shares
    swap(&pool, BOB, pool.token_a, 300, 0).unwrap();
    let (ra, rb) = reserves(&pool);
    let total = total_shares(&pool);
    let bob_shares = shares_of(&pool, BOB);
    let bob_a = balance(pool.token_a, BOB);
    let bob_b = balance(pool.token_b, BOB);
    let (out_a, out_b) = remove(&pool, BOB, bob_shares).unwrap();
    assert_eq!(
        (out_a, out_b),
        (bob_shares * ra / total, bob_shares * rb / total)
    );
    assert_eq!(balance(pool.token_a, BOB), bob_a + out_a);
    assert_eq!(balance(pool.token_b, BOB), bob_b + out_b);
    assert_eq!(shares_of(&pool, BOB), 0);
    assert_consistent(&pool);
    // More than held.
    assert_eq!(
        revert_reason(remove(&pool, BOB, 1)),
        b"insufficient shares".to_vec()
    );
    // After everything is withdrawn the pool is empty and can start again from the geometric mean.
    let alice_shares = shares_of(&pool, ALICE);
    remove(&pool, ALICE, alice_shares).unwrap();
    assert_eq!(reserves(&pool), (0, 0));
    assert_eq!(total_shares(&pool), 0);
    assert_consistent(&pool);
    assert_eq!(add(&pool, BOB, 9, 16).unwrap(), 12);
}

/// A deposit in the pool's ratio gets fewer shares after a swap in either direction, so a minimum set 1% below the
/// shares quoted before the swap stops it; without a minimum it enters at the moved ratio.
#[test]
fn min_shares_stops_a_deposit_after_a_swap() {
    for side in [0, 1] {
        let pool = setup();
        add(&pool, ALICE, 1_000_000, 1_000_000).unwrap();
        // The quote: 10 000 of each side gets 10 000 of the 1 000 000 shares
        let min_shares = 9_900;
        let token_in = [pool.token_a, pool.token_b][side];
        swap(&pool, ALICE, token_in, 200_000, 0).unwrap();
        let state = |pool: &Pool| {
            (
                reserves(pool),
                total_shares(pool),
                balance(pool.token_a, BOB),
                balance(pool.token_b, BOB),
            )
        };
        let before = state(&pool);
        assert_eq!(
            revert_reason(add_at_least(&pool, BOB, 10_000, 10_000, min_shares)),
            b"shares below min_shares".to_vec()
        );
        assert_eq!(state(&pool), before);
        // 200 000 in leaves 833 751 of the other side: min(10 000 · 10^6 / 1 200 000, 10 000 · 10^6 / 833 751)
        assert_eq!(add(&pool, BOB, 10_000, 10_000).unwrap(), 8_333);
        assert_consistent(&pool);
    }
    // In the pool's ratio, a minimum the deposit meets changes nothing: the same shares as without one
    let pool = setup();
    add(&pool, ALICE, 1_000_000, 1_000_000).unwrap();
    assert_eq!(add_at_least(&pool, BOB, 10_000, 10_000, 10_000), Ok(10_000));
    assert_eq!(add_at_least(&pool, BOB, 10_000, 10_000, 0), Ok(10_000));
    assert_eq!(add(&pool, BOB, 10_000, 10_000), Ok(10_000));
    assert_eq!(shares_of(&pool, BOB), 30_000);
    assert_consistent(&pool);
    // Any other length of arguments is malformed
    for args in [
        encode(&(10_000u128, 10_000u128, 0u64)),
        encode(&(10_000u128, 10_000u128, 0u128, 0u128)),
    ] {
        assert_eq!(
            revert_reason(mock::call(BOB, pool.amm, "add_liquidity", &args)),
            b"malformed arguments".to_vec()
        );
    }
}

#[test]
fn remove_liquidity_reverts_below_its_minimums() {
    let pool = setup();
    add(&pool, ALICE, 1_000, 2_000).unwrap(); // 1414 shares
    let state = |pool: &Pool| {
        (
            reserves(pool),
            shares_of(pool, ALICE),
            balance(pool.token_a, ALICE),
            balance(pool.token_b, ALICE),
        )
    };
    let before = state(&pool);
    // 707 shares withdraw 707 · 1000 / 1414 = 500 and 707 · 2000 / 1414 = 1000
    for (min_a, min_b) in [(501, 0), (0, 1_001), (501, 1_001)] {
        assert_eq!(
            revert_reason(remove_at_least(&pool, ALICE, 707, min_a, min_b)),
            b"output below min_a or min_b".to_vec()
        );
        assert_eq!(state(&pool), before);
    }
    assert_eq!(
        remove_at_least(&pool, ALICE, 707, 500, 1_000),
        Ok((500, 1_000))
    );
    assert_eq!(remove(&pool, ALICE, 707), Ok((500, 1_000)));
    assert_eq!(reserves(&pool), (0, 0));
    assert_consistent(&pool);
    assert_eq!(
        revert_reason(mock::call(
            ALICE,
            pool.amm,
            "remove_liquidity",
            &encode(&(1u128, 0u128))
        )),
        b"malformed arguments".to_vec()
    );
}

#[test]
fn first_deposit_overflow_reverts() {
    let pool = setup();
    // amount_a · amount_b exceeds u128: the example AMM simply reverts, as the spec says.
    let big = 1u128 << 64;
    assert_eq!(
        revert_reason(add(&pool, ALICE, big, big)),
        b"arithmetic overflow".to_vec()
    );
    assert_consistent(&pool);
}

#[test]
fn random_operations_keep_invariants() {
    let pool = setup();
    add(&pool, ALICE, 10_000_000, 30_000_000).unwrap();
    let mut seed: u64 = 7;
    let mut next = |bound: u64| {
        seed = seed
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        u128::from((seed >> 11) % bound)
    };
    for _ in 0..200 {
        let who = if next(2) == 0 { ALICE } else { BOB };
        let (ra, rb) = reserves(&pool);
        match next(3) {
            0 => {
                let token_in = if next(2) == 0 {
                    pool.token_a
                } else {
                    pool.token_b
                };
                let amount = next(1_000_000) + 1;
                if let Ok(out) = swap(&pool, who, token_in, amount, 0) {
                    let (na, nb) = reserves(&pool);
                    assert!(na * nb >= ra * rb, "k must not decrease");
                    let expected = if token_in == pool.token_a {
                        swap_output(amount, ra, rb)
                    } else {
                        swap_output(amount, rb, ra)
                    };
                    assert_eq!(Some(out), expected);
                }
            }
            1 => {
                let _ = add(&pool, who, next(5_000_000) + 1, next(5_000_000) + 1);
            }
            _ => {
                let held = shares_of(&pool, who);
                if held > 0 {
                    let _ = remove(&pool, who, next(u64::try_from(held).unwrap()) + 1);
                }
            }
        }
        assert_consistent(&pool);
    }
}

// ---------------------------------------------------------------------------
// Built-in asset side (D-073: attached funds in calls)

/// Pair: a is KSA (a built-in asset), b is a token contract.
struct NativePool {
    token: [u8; 32],
    amm: [u8; 32],
}

fn ksa(amount: u128) -> Funds {
    Funds {
        asset: KSA_ASSET_ID,
        amount,
    }
}

fn setup_native() -> NativePool {
    mock::reset();
    mock::register_asset(KSA_ASSET_ID, "KSA", 8, [0; 32]);
    mock::mint(KSA_ASSET_ID, ALICE, SUPPLY).unwrap();
    mock::mint(KSA_ASSET_ID, BOB, SUPPLY).unwrap();
    let token = mock::deploy(
        ALICE,
        0,
        b"token",
        token::dispatch,
        &encode(&("Token B", "B", 8u8, SUPPLY)),
    )
    .unwrap();
    let amm = mock::deploy(
        ALICE,
        1,
        b"amm",
        amm::dispatch,
        &encode(&(AssetRef::Asset(KSA_ASSET_ID), AssetRef::Token(token))),
    )
    .unwrap();
    mock::call(ALICE, token, "transfer", &encode(&(BOB, SUPPLY / 2))).unwrap();
    for who in [ALICE, BOB] {
        mock::call(who, token, "approve", &encode(&(amm, u128::MAX))).unwrap();
    }
    NativePool { token, amm }
}

fn native_reserves(pool: &NativePool) -> (u128, u128) {
    decode(&mock::call(ALICE, pool.amm, "reserves", &[]).unwrap()).unwrap()
}

/// The reserves match the KSA and tokens the contract actually holds.
fn assert_native_consistent(pool: &NativePool) {
    let (ra, rb) = native_reserves(pool);
    assert_eq!(mock::balance(&KSA_ASSET_ID, &pool.amm), ra);
    assert_eq!(balance(pool.token, pool.amm), rb);
}

#[test]
fn native_asset_side_takes_attached_funds() {
    let pool = setup_native();
    // First deposit: exactly 40 000 KSA attached
    let shares: u128 = decode(
        &mock::call_with_funds(
            ALICE,
            pool.amm,
            "add_liquidity",
            &encode(&(40_000u128, 90_000u128)),
            &[ksa(40_000)],
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(shares, 60_000);
    assert_eq!(native_reserves(&pool), (40_000, 90_000));
    assert_eq!(mock::balance(&KSA_ASSET_ID, &ALICE), SUPPLY - 40_000);
    assert_native_consistent(&pool);

    // Only part is used proportionally: the excess KSA is refunded
    let before = mock::balance(&KSA_ASSET_ID, &BOB);
    mock::call_with_funds(
        BOB,
        pool.amm,
        "add_liquidity",
        &encode(&(10_000u128, 9_000u128)),
        &[ksa(10_000)],
    )
    .unwrap();
    // shares = min(10 000 · 60 000 / 40 000, 9 000 · 60 000 / 90 000) = 6 000;
    // ceil(6 000 · 40 000 / 60 000) = 4 000 used
    assert_eq!(mock::balance(&KSA_ASSET_ID, &BOB), before - 4_000);
    assert_eq!(native_reserves(&pool), (44_000, 99_000));
    assert_native_consistent(&pool);

    // KSA → token: the input is attached
    let expected = swap_output(1_000, 44_000, 99_000).unwrap();
    let out: u128 = decode(
        &mock::call_with_funds(
            BOB,
            pool.amm,
            "swap",
            &encode(&(KSA_ASSET_ID, 1_000u128, 1u128)),
            &[ksa(1_000)],
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(out, expected);
    assert_native_consistent(&pool);

    // Token → KSA: nothing attached; the KSA is pushed out with asset_transfer
    let ksa_before = mock::balance(&KSA_ASSET_ID, &BOB);
    let (ra, rb) = native_reserves(&pool);
    let expected = swap_output(2_000, rb, ra).unwrap();
    let out: u128 = decode(
        &mock::call(
            BOB,
            pool.amm,
            "swap",
            &encode(&(pool.token, 2_000u128, 1u128)),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(out, expected);
    assert_eq!(mock::balance(&KSA_ASSET_ID, &BOB), ksa_before + expected);
    assert_native_consistent(&pool);

    // Removing liquidity: KSA and tokens, each in proportion
    let (a, b): (u128, u128) =
        decode(&mock::call(ALICE, pool.amm, "remove_liquidity", &encode(&30_000u128)).unwrap())
            .unwrap();
    assert!(a > 0 && b > 0);
    assert_native_consistent(&pool);
}

#[test]
fn attached_funds_must_match_the_pool() {
    let pool = setup_native();
    mock::call_with_funds(
        ALICE,
        pool.amm,
        "add_liquidity",
        &encode(&(40_000u128, 90_000u128)),
        &[ksa(40_000)],
    )
    .unwrap();
    mock::register_asset([7; 32], "X", 0, [0; 32]);
    mock::mint([7; 32], BOB, 100).unwrap();
    let x = Funds {
        asset: [7; 32],
        amount: 5,
    };
    let swap_in = |amount: u128| encode(&(KSA_ASSET_ID, amount, 1u128));
    for (method, args, funds, reason) in [
        // Attached amount does not match the argument
        (
            "swap",
            swap_in(1_000),
            vec![ksa(999)],
            "attached funds must equal the amount",
        ),
        (
            "swap",
            swap_in(1_000),
            vec![],
            "attached funds must equal the amount",
        ),
        // An asset outside the pair
        (
            "swap",
            swap_in(1_000),
            vec![ksa(1_000), x],
            "unexpected attached funds",
        ),
        // KSA cannot be attached when the input is the token side (KSA is the output side)
        (
            "swap",
            encode(&(pool.token, 1_000u128, 1u128)),
            vec![ksa(1)],
            "unexpected attached funds",
        ),
        (
            "remove_liquidity",
            encode(&1u128),
            vec![ksa(1)],
            "unexpected attached funds",
        ),
        (
            "add_liquidity",
            encode(&(100u128, 100u128)),
            vec![ksa(99)],
            "attached funds must equal the amount",
        ),
        // 100 KSA and 225 tokens get min(100 · 60 000 / 40 000, 225 · 60 000 / 90 000) = 150 shares
        (
            "add_liquidity",
            encode(&(100u128, 225u128, 151u128)),
            vec![ksa(100)],
            "shares below min_shares",
        ),
    ] {
        let before = native_reserves(&pool);
        let reason_got = revert_reason(mock::call_with_funds(BOB, pool.amm, method, &args, &funds));
        assert_eq!(reason_got, reason.as_bytes(), "{method}");
        assert_eq!(native_reserves(&pool), before);
    }
    assert_eq!(mock::balance(&KSA_ASSET_ID, &BOB), SUPPLY);
    assert_eq!(mock::balance(&[7; 32], &BOB), 100);
    assert_native_consistent(&pool);
    // Funds attached at deployment are rejected too
    let result = mock::deploy_with_funds(
        ALICE,
        9,
        b"amm",
        amm::dispatch,
        &encode(&(AssetRef::Asset(KSA_ASSET_ID), AssetRef::Token(pool.token))),
        &[ksa(1)],
    );
    assert!(matches!(result, Err(Failure::Revert { .. })));
}

#[test]
fn two_native_assets_make_a_pool() {
    mock::reset();
    mock::register_asset(KSA_ASSET_ID, "KSA", 8, [0; 32]);
    mock::register_asset([7; 32], "X", 0, [0; 32]);
    mock::mint(KSA_ASSET_ID, ALICE, SUPPLY).unwrap();
    mock::mint([7; 32], ALICE, SUPPLY).unwrap();
    let amm = mock::deploy(
        ALICE,
        0,
        b"amm",
        amm::dispatch,
        &encode(&(AssetRef::Asset(KSA_ASSET_ID), AssetRef::Asset([7; 32]))),
    )
    .unwrap();
    let x = |amount| Funds {
        asset: [7; 32],
        amount,
    };
    mock::call_with_funds(
        ALICE,
        amm,
        "add_liquidity",
        &encode(&(1_000u128, 4_000u128)),
        &[ksa(1_000), x(4_000)],
    )
    .unwrap();
    let out: u128 = decode(
        &mock::call_with_funds(
            ALICE,
            amm,
            "swap",
            &encode(&([7u8; 32], 400u128, 1u128)),
            &[x(400)],
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(out, swap_output(400, 4_000, 1_000).unwrap());
    assert_eq!(mock::balance(&KSA_ASSET_ID, &amm), 1_000 - out);
    assert_eq!(mock::balance(&[7; 32], &amm), 4_400);
}

// ---------------------------------------------------------------------------
// Tokens in one step: `transfer_and_call` and `on_token_received` (docs/STANDARDS.md §3.3)

const CAROL: [u8; 32] = [0xc4; 32];

/// `token.transfer_and_call(pool, amount, action)` by `who`, attaching `funds`; the pool's answer (the `u128` inside
/// the `Bytes` the token returns).
fn send_and_call(
    who: [u8; 32],
    token: [u8; 32],
    amm: [u8; 32],
    amount: u128,
    action: PoolAction,
    funds: &[Funds],
) -> Result<u128, Failure> {
    mock::call_with_funds(
        who,
        token,
        "transfer_and_call",
        &encode(&(amm, amount, encode(&action))),
        funds,
    )
    .map(|out| decode(&decode::<Vec<u8>>(&out).unwrap()).unwrap())
}

#[test]
fn a_token_swaps_in_one_step_without_an_allowance() {
    let pool = setup();
    add(&pool, ALICE, 1_000_000, 1_000_000).unwrap();
    // CAROL never approved the pool
    mock::call(BOB, pool.token_a, "transfer", &encode(&(CAROL, 5_000u128))).unwrap();
    let out = send_and_call(
        CAROL,
        pool.token_a,
        pool.amm,
        1_000,
        PoolAction::Swap(996),
        &[],
    )
    .unwrap();
    assert_eq!(out, 996);
    assert_eq!(balance(pool.token_a, CAROL), 4_000);
    assert_eq!(balance(pool.token_b, CAROL), 996);
    assert_eq!(reserves(&pool), (1_001_000, 999_004));
    assert_consistent(&pool);
    assert_eq!(
        mock::events().last().unwrap().data,
        encode(&("swap", CAROL, pool.token_a, 1_000u128, 996u128))
    );
    // The other direction, and a minimum it misses: the whole op fails and the tokens stay with CAROL
    assert_eq!(
        revert_reason(send_and_call(
            CAROL,
            pool.token_b,
            pool.amm,
            996,
            PoolAction::Swap(1_000),
            &[]
        )),
        b"output below min_out".to_vec()
    );
    assert_eq!(balance(pool.token_b, CAROL), 996);
    let back = swap_output(996, 999_004, 1_001_000).unwrap();
    assert_eq!(
        send_and_call(
            CAROL,
            pool.token_b,
            pool.amm,
            996,
            PoolAction::Swap(back),
            &[]
        ),
        Ok(back)
    );
    assert_consistent(&pool);
}

#[test]
fn only_a_token_of_the_pool_calls_back() {
    let pool = setup();
    add(&pool, ALICE, 1_000_000, 1_000_000).unwrap();
    let before = (reserves(&pool), balance(pool.token_b, BOB));
    // Called directly, with a made-up sender and amount
    let direct = mock::call(
        BOB,
        pool.amm,
        "on_token_received",
        &encode(&(BOB, 1_000u128, encode(&PoolAction::Swap(0)))),
    );
    assert_eq!(
        revert_reason(direct),
        b"caller is not a token of this pool".to_vec()
    );
    // A token outside the pair: its transfer fails with the pool's refusal and its tokens stay with the sender
    let other = mock::deploy(
        BOB,
        7,
        b"token",
        token::dispatch,
        &encode(&("Other", "O", 8u8, 1_000_000u128)),
    )
    .unwrap();
    assert_eq!(
        revert_reason(send_and_call(
            BOB,
            other,
            pool.amm,
            1_000,
            PoolAction::Swap(0),
            &[]
        )),
        b"caller is not a token of this pool".to_vec()
    );
    assert_eq!(balance(other, BOB), 1_000_000);
    // Data that is no pool action, funds with a swap of two tokens, and nothing at all
    let bad = mock::call(
        BOB,
        pool.token_a,
        "transfer_and_call",
        &encode(&(pool.amm, 1_000u128, vec![9u8])),
    );
    assert_eq!(revert_reason(bad), b"data is not a pool action".to_vec());
    mock::register_asset(KSA_ASSET_ID, "KSA", 8, [0; 32]);
    mock::mint(KSA_ASSET_ID, BOB, 100).unwrap();
    assert_eq!(
        revert_reason(send_and_call(
            BOB,
            pool.token_a,
            pool.amm,
            1_000,
            PoolAction::Swap(0),
            &[ksa(10)]
        )),
        b"unexpected attached funds".to_vec()
    );
    assert_eq!(mock::balance(&KSA_ASSET_ID, &BOB), 100);
    assert_eq!((reserves(&pool), balance(pool.token_b, BOB)), before);
    assert_consistent(&pool);
}

#[test]
fn a_token_and_an_attached_asset_add_liquidity_in_one_step() {
    let pool = setup_native();
    // The first deposit: 40 000 KSA attached to the token's transfer_and_call, which passes them on
    let shares = send_and_call(
        ALICE,
        pool.token,
        pool.amm,
        90_000,
        PoolAction::AddLiquidity(40_000, 60_000),
        &[ksa(40_000)],
    )
    .unwrap();
    assert_eq!(shares, 60_000);
    assert_eq!(native_reserves(&pool), (40_000, 90_000));
    assert_native_consistent(&pool);
    assert_eq!(
        mock::events().last().unwrap().data,
        encode(&("add_liquidity", ALICE, 40_000u128, 90_000u128, 60_000u128))
    );
    // CAROL gives no allowance: 10 000 KSA and 9 000 tokens get 6 000 shares, using 4 000 KSA and all the tokens;
    // the 6 000 KSA left come back
    mock::mint(KSA_ASSET_ID, CAROL, 10_000).unwrap();
    mock::call(BOB, pool.token, "transfer", &encode(&(CAROL, 20_000u128))).unwrap();
    let shares = send_and_call(
        CAROL,
        pool.token,
        pool.amm,
        9_000,
        PoolAction::AddLiquidity(10_000, 0),
        &[ksa(10_000)],
    )
    .unwrap();
    assert_eq!(shares, 6_000);
    assert_eq!(mock::balance(&KSA_ASSET_ID, &CAROL), 6_000);
    assert_eq!(balance(pool.token, CAROL), 11_000);
    assert_native_consistent(&pool);
    // 1 000 KSA and 9 000 tokens get 1 500 shares, using 1 000 KSA and 2 250 tokens: 6 750 tokens come back
    let shares = send_and_call(
        CAROL,
        pool.token,
        pool.amm,
        9_000,
        PoolAction::AddLiquidity(1_000, 1_500),
        &[ksa(1_000)],
    )
    .unwrap();
    assert_eq!(shares, 1_500);
    assert_eq!(balance(pool.token, CAROL), 11_000 - 2_250);
    assert_eq!(shares_of_native(&pool, CAROL), 7_500);
    assert_native_consistent(&pool);
    // The asset must be attached exactly, and the minimum holds
    for (funds, min_shares, reason) in [
        (vec![ksa(999)], 0, "attached funds must equal the amount"),
        (vec![], 0, "attached funds must equal the amount"),
        (vec![ksa(1_000)], 1_501, "shares below min_shares"),
    ] {
        let before = (native_reserves(&pool), balance(pool.token, CAROL));
        let result = send_and_call(
            CAROL,
            pool.token,
            pool.amm,
            2_250,
            PoolAction::AddLiquidity(1_000, min_shares),
            &funds,
        );
        assert_eq!(revert_reason(result), reason.as_bytes());
        assert_eq!((native_reserves(&pool), balance(pool.token, CAROL)), before);
    }
    // Swapping in one step pays the KSA out with asset_transfer
    let ksa_before = mock::balance(&KSA_ASSET_ID, &CAROL);
    let (ra, rb) = native_reserves(&pool);
    let expected = swap_output(2_000, rb, ra).unwrap();
    assert_eq!(
        send_and_call(
            CAROL,
            pool.token,
            pool.amm,
            2_000,
            PoolAction::Swap(expected),
            &[]
        ),
        Ok(expected)
    );
    assert_eq!(mock::balance(&KSA_ASSET_ID, &CAROL), ksa_before + expected);
    assert_native_consistent(&pool);
}

fn shares_of_native(pool: &NativePool, account: [u8; 32]) -> u128 {
    decode(&mock::call(ALICE, pool.amm, "shares_of", &encode(&account)).unwrap()).unwrap()
}

#[test]
fn two_tokens_add_liquidity_with_one_sent_and_one_pulled() {
    let pool = setup();
    add(&pool, ALICE, 1_000, 2_000).unwrap(); // 1414 shares
    let (bob_a, bob_b) = (balance(pool.token_a, BOB), balance(pool.token_b, BOB));
    // 500 of a sent, up to 5 000 of b pulled: 707 shares use 500 of a and 1 000 of b
    let shares = send_and_call(
        BOB,
        pool.token_a,
        pool.amm,
        500,
        PoolAction::AddLiquidity(5_000, 707),
        &[],
    )
    .unwrap();
    assert_eq!(shares, 707);
    assert_eq!(balance(pool.token_a, BOB), bob_a - 500);
    assert_eq!(balance(pool.token_b, BOB), bob_b - 1_000);
    assert_consistent(&pool);
    // b sent, more of it than the ratio takes, and a pulled: the b left over comes back
    let shares = send_and_call(
        BOB,
        pool.token_b,
        pool.amm,
        5_000,
        PoolAction::AddLiquidity(500, 0),
        &[],
    )
    .unwrap();
    assert_eq!(shares, 707);
    assert_eq!(balance(pool.token_a, BOB), bob_a - 1_000);
    assert_eq!(balance(pool.token_b, BOB), bob_b - 2_000);
    assert_consistent(&pool);
    // Without an allowance for the side pulled, the whole op fails
    mock::call(BOB, pool.token_a, "transfer", &encode(&(CAROL, 1_000u128))).unwrap();
    let result = send_and_call(
        CAROL,
        pool.token_a,
        pool.amm,
        500,
        PoolAction::AddLiquidity(1_000, 0),
        &[],
    );
    assert!(matches!(result, Err(Failure::Revert { .. })), "{result:?}");
    assert_eq!(balance(pool.token_a, CAROL), 1_000);
    assert_consistent(&pool);
}

// ---------------------------------------------------------------------------
// A token that charges a fee (docs/STANDARDS.md, KS-20 rule 4)

mod taxed {
    use ksana_contract_sdk::ks20::{Ks20, Policy};
    use ksana_contract_sdk::{self as sdk, standard, CallResult};

    /// 1% of every transfer burned.
    pub struct Tax;

    impl Policy for Tax {
        fn fee(
            &self,
            _from: &[u8; 32],
            _to: &[u8; 32],
            amount: u128,
        ) -> Result<Option<([u8; 32], u128)>, Vec<u8>> {
            Ok(Some((standard::ZERO_ACCOUNT, amount / 100)))
        }

        fn capabilities(&self) -> Result<Vec<&'static str>, Vec<u8>> {
            Ok(vec![standard::CAP_FEE])
        }
    }

    const TOKEN: Ks20<Tax> = Ks20::new().with_policy(Tax);

    pub fn dispatch(method: &str, args: &[u8]) -> CallResult {
        if method == "init" {
            let supply: u128 = sdk::decode(args)?;
            TOKEN.init("Taxed", "TAX", 0)?;
            TOKEN.mint(&sdk::caller(), supply)?;
            return sdk::ok(&());
        }
        TOKEN
            .dispatch(method, args)
            .unwrap_or_else(|| sdk::revert("unknown method"))
    }
}

#[test]
fn a_token_with_a_fee_trades_at_what_the_pool_received() {
    mock::reset();
    mock::register_asset(KSA_ASSET_ID, "KSA", 8, [0; 32]);
    mock::mint(KSA_ASSET_ID, ALICE, SUPPLY).unwrap();
    let tax = mock::deploy(ALICE, 0, b"taxed", taxed::dispatch, &encode(&SUPPLY)).unwrap();
    let amm = mock::deploy(
        ALICE,
        1,
        b"amm",
        amm::dispatch,
        &encode(&(AssetRef::Asset(KSA_ASSET_ID), AssetRef::Token(tax))),
    )
    .unwrap();
    let pool = NativePool { token: tax, amm };
    // 100 000 sent arrive as 99 000
    let shares = send_and_call(
        ALICE,
        tax,
        amm,
        100_000,
        PoolAction::AddLiquidity(99_000, 0),
        &[ksa(99_000)],
    )
    .unwrap();
    assert_eq!(shares, 99_000);
    assert_eq!(native_reserves(&pool), (99_000, 99_000));
    assert_native_consistent(&pool);
    // A swap in one step trades the 9 900 that arrive of 10 000
    let expected = swap_output(9_900, 99_000, 99_000).unwrap();
    assert_eq!(
        send_and_call(ALICE, tax, amm, 10_000, PoolAction::Swap(1), &[]),
        Ok(expected)
    );
    assert_native_consistent(&pool);
    // Pulled, the token would leave the pool short of its reserve: both ways of pulling revert
    mock::call(
        ALICE,
        tax,
        "approve",
        &encode(&(amm, u128::MAX, None::<u64>)),
    )
    .unwrap();
    let before = native_reserves(&pool);
    assert_eq!(
        revert_reason(mock::call(
            ALICE,
            amm,
            "swap",
            &encode(&(tax, 10_000u128, 1u128))
        )),
        b"received less than the amount".to_vec()
    );
    assert_eq!(
        revert_reason(mock::call_with_funds(
            ALICE,
            amm,
            "add_liquidity",
            &encode(&(1_000u128, 1_000u128)),
            &[ksa(1_000)]
        )),
        b"received less than the amount".to_vec()
    );
    assert_eq!(native_reserves(&pool), before);
    // Paying the token out: the pool's balance goes down by the amount, the recipient gets it less the fee
    let held = shares_of_native(&pool, ALICE);
    let (out_ksa, out_tax): (u128, u128) =
        decode(&mock::call(ALICE, amm, "remove_liquidity", &encode(&(held / 2))).unwrap()).unwrap();
    assert!(out_ksa > 0 && out_tax > 0);
    assert_native_consistent(&pool);
}
