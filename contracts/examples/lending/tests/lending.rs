//! Tests of the lending contract on the SDK's in-memory mock host: the 256-bit arithmetic, every method and its
//! reverts, the interest model, liquidations, and long pseudo-random sequences checked step by step against a model of
//! the books.
#![cfg(not(target_arch = "wasm32"))]
#![allow(
    missing_docs,
    clippy::unwrap_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::cast_possible_truncation
)]

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Debug;

use ksana_contract_sdk::mock::{self, Failure};
use ksana_contract_sdk::{decode, encode, Decode, Funds};
use lending::math::{mul_div, mul_div_up, WAD};
use lending::{Market, Params, Position, SECONDS_PER_YEAR};

const ADMIN: [u8; 32] = [0xad; 32];
const ALICE: [u8; 32] = [0xa1; 32];
const BOB: [u8; 32] = [0xb0; 32];
const CAROL: [u8; 32] = [0xc4; 32];
const DAVE: [u8; 32] = [0xd5; 32];
const ERIN: [u8; 32] = [0xe6; 32];
const USERS: [[u8; 32]; 5] = [ALICE, BOB, CAROL, DAVE, ERIN];
const LOAN: [u8; 32] = [0x11; 32];
const COLL: [u8; 32] = [0x22; 32];
const OTHER: [u8; 32] = [0x33; 32];
/// What every user and the admin hold of each asset at the start.
const WALLET: u128 = 1_000_000_000_000_000;
const T0: u64 = 1_800_000_000;
const DAY: u64 = 86_400;
const YEAR: u64 = SECONDS_PER_YEAR;
const MAX: u128 = u128::MAX;

// ---------------------------------------------------------------------------
// Helpers

/// splitmix64: a deterministic pseudo-random sequence without extra crates.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    /// Below `bound` (one draw up to 2^64, two above; the bias of the remainder does not matter here), 0 when
    /// `bound` is 0.
    fn below(&mut self, bound: u128) -> u128 {
        if bound == 0 {
            0
        } else if bound <= 1 << 64 {
            u128::from(self.next()) % bound
        } else {
            ((u128::from(self.next()) << 64) | u128::from(self.next())) % bound
        }
    }

    /// A u128 with a random bit length, so that small, medium and huge values all come up.
    fn bits(&mut self) -> u128 {
        let length = self.below(129) as u32;
        let wide = (u128::from(self.next()) << 64) | u128::from(self.next());
        if length == 0 {
            0
        } else {
            wide >> (128 - length)
        }
    }

    fn index(&mut self, len: usize) -> usize {
        self.below(len as u128) as usize
    }
}

fn params() -> Params {
    Params {
        collateral_factor_bps: 7_500,
        liquidation_threshold_bps: 8_000,
        liquidation_bonus_bps: 500,
        reserve_factor_bps: 1_000,
        base_rate_bps: 200,
        slope1_bps: 1_000,
        kink_bps: 8_000,
        slope2_bps: 10_000,
    }
}

fn deploy(
    nonce: u64,
    loan: [u8; 32],
    coll: [u8; 32],
    price: u128,
    params: Params,
) -> Result<[u8; 32], Failure> {
    mock::deploy(
        ADMIN,
        nonce,
        b"lending",
        lending::dispatch,
        &encode(&(loan, coll, price, params)),
    )
}

/// A fresh mock with `wallet` of each of the three assets in every wallet and a market of `LOAN` against `COLL` at
/// time `T0`.
fn setup_scaled(price: u128, params: Params, wallet: u128) -> [u8; 32] {
    mock::reset();
    mock::set_block_timestamp(T0);
    for asset in [LOAN, COLL, OTHER] {
        mock::register_asset(asset, "asset", 8, [0; 32]);
        for who in USERS.iter().chain([&ADMIN]) {
            mock::mint(asset, *who, wallet).unwrap();
        }
    }
    deploy(0, LOAN, COLL, price, params).unwrap()
}

fn setup_with(price: u128, params: Params) -> [u8; 32] {
    setup_scaled(price, params, WALLET)
}

fn setup() -> [u8; 32] {
    setup_with(WAD, params())
}

fn funds(asset: [u8; 32], amount: u128) -> Funds {
    Funds { asset, amount }
}

fn call<T: Decode>(
    who: [u8; 32],
    m: [u8; 32],
    method: &str,
    args: &[u8],
    attached: &[Funds],
) -> Result<T, Failure> {
    mock::call_with_funds(who, m, method, args, attached).map(|out| decode(&out).unwrap())
}

fn supply(m: [u8; 32], who: [u8; 32], amount: u128) -> Result<u128, Failure> {
    call(who, m, "supply", &[], &[funds(LOAN, amount)])
}

fn withdraw(m: [u8; 32], who: [u8; 32], shares: u128) -> Result<u128, Failure> {
    call(who, m, "withdraw", &encode(&shares), &[])
}

fn deposit(m: [u8; 32], who: [u8; 32], amount: u128) -> Result<u128, Failure> {
    call(who, m, "deposit_collateral", &[], &[funds(COLL, amount)])
}

fn withdraw_collateral(m: [u8; 32], who: [u8; 32], amount: u128) -> Result<u128, Failure> {
    call(who, m, "withdraw_collateral", &encode(&amount), &[])
}

fn borrow(m: [u8; 32], who: [u8; 32], amount: u128) -> Result<u128, Failure> {
    call(who, m, "borrow", &encode(&amount), &[])
}

fn repay(m: [u8; 32], payer: [u8; 32], borrower: [u8; 32], amount: u128) -> Result<u128, Failure> {
    call(
        payer,
        m,
        "repay",
        &encode(&borrower),
        &[funds(LOAN, amount)],
    )
}

fn liquidate(
    m: [u8; 32],
    liquidator: [u8; 32],
    borrower: [u8; 32],
    amount: u128,
) -> Result<(u128, u128), Failure> {
    call(
        liquidator,
        m,
        "liquidate",
        &encode(&borrower),
        &[funds(LOAN, amount)],
    )
}

fn set_price(m: [u8; 32], who: [u8; 32], price: u128) -> Result<(), Failure> {
    call(who, m, "set_price", &encode(&price), &[])
}

fn set_admin(m: [u8; 32], who: [u8; 32], admin: Option<[u8; 32]>) -> Result<(), Failure> {
    call(who, m, "set_admin", &encode(&admin), &[])
}

fn withdraw_reserves(
    m: [u8; 32],
    who: [u8; 32],
    amount: u128,
    to: [u8; 32],
) -> Result<(), Failure> {
    call(who, m, "withdraw_reserves", &encode(&(amount, to)), &[])
}

fn market(m: [u8; 32]) -> Market {
    call(ALICE, m, "market", &[], &[]).unwrap()
}

fn position(m: [u8; 32], who: [u8; 32]) -> Position {
    call(ALICE, m, "position", &encode(&who), &[]).unwrap()
}

/// The reason of a revert, as text.
fn reason<T: Debug>(result: Result<T, Failure>) -> String {
    match result {
        Err(Failure::Revert { reason, .. }) => String::from_utf8(reason).unwrap(),
        other => panic!("expected a revert, got {other:?}"),
    }
}

/// The data of the last event.
fn last_event() -> Vec<u8> {
    mock::events().last().unwrap().data.clone()
}

// ---------------------------------------------------------------------------
// The test's own model of the market (the formulas of the module docs, written independently of the contract)

fn total_assets(m: &Market) -> u128 {
    (m.cash + m.total_borrows).saturating_sub(m.reserves)
}

fn liquidity(m: &Market) -> u128 {
    m.cash.saturating_sub(m.reserves)
}

fn utilization(m: &Market) -> u32 {
    let assets = total_assets(m);
    if assets == 0 {
        0
    } else {
        mul_div(m.total_borrows, 10_000, assets).map_or(10_000, |u| u.min(10_000)) as u32
    }
}

fn borrow_rate(p: &Params, utilization: u32) -> u32 {
    let below = utilization.min(p.kink_bps);
    let mut rate = p.base_rate_bps + p.slope1_bps * below / p.kink_bps;
    if utilization > p.kink_bps {
        let above = u64::from(utilization - p.kink_bps);
        rate += (u64::from(p.slope2_bps) * above / u64::from(10_000 - p.kink_bps)) as u32;
    }
    rate
}

/// Accrues `m` (a stored market) to `now` as the module docs say.
fn accrue_model(m: &mut Market, now: u64) {
    if now <= m.last_accrual {
        return;
    }
    let dt = u128::from(now - m.last_accrual);
    let rate = u128::from(borrow_rate(&m.params, utilization(m)));
    let per_second = rate * WAD / 10_000 / u128::from(YEAR);
    let index = m.borrow_index + mul_div(m.borrow_index, per_second * dt, WAD).unwrap();
    let borrows = mul_div(m.total_borrows, index, m.borrow_index).unwrap();
    m.reserves += (borrows - m.total_borrows) * u128::from(m.params.reserve_factor_bps) / 10_000;
    m.total_borrows = borrows;
    m.borrow_index = index;
    m.last_accrual = now;
}

/// Recomputes the derived fields of a market: utilization and rates.
fn derive_market(m: &mut Market) {
    m.utilization_bps = utilization(m);
    m.borrow_rate_bps = borrow_rate(&m.params, m.utilization_bps);
    m.supply_rate_bps = (u64::from(m.borrow_rate_bps)
        * u64::from(m.utilization_bps)
        * u64::from(10_000 - m.params.reserve_factor_bps)
        / 100_000_000) as u32;
}

/// Recomputes the derived fields of a position in market `m`: supplied, limits and health.
fn derive_position(m: &Market, p: &mut Position) {
    p.supplied = if p.shares == 0 {
        0
    } else {
        mul_div(p.shares, total_assets(m), m.total_shares).unwrap()
    };
    let value = mul_div(p.collateral, m.price, WAD).unwrap_or(MAX);
    p.borrow_limit = mul_div(value, u128::from(m.params.collateral_factor_bps), 10_000).unwrap();
    p.liquidation_limit = mul_div(
        value,
        u128::from(m.params.liquidation_threshold_bps),
        10_000,
    )
    .unwrap();
    p.health_bps =
        (p.debt > 0).then(|| mul_div(p.liquidation_limit, 10_000, p.debt).unwrap_or(MAX));
}

// ---------------------------------------------------------------------------
// The 256-bit arithmetic

/// `a · b` as four 64-bit limbs, least significant first: schoolbook multiplication, independent of the library.
fn product(a: u128, b: u128) -> [u64; 4] {
    let a = [a as u64, (a >> 64) as u64];
    let b = [b as u64, (b >> 64) as u64];
    let mut out = [0u64; 4];
    for i in 0..2 {
        let mut carry = 0u128;
        for j in 0..2 {
            let t = u128::from(a[i]) * u128::from(b[j]) + u128::from(out[i + j]) + carry;
            out[i + j] = t as u64;
            carry = t >> 64;
        }
        out[i + 2] = carry as u64;
    }
    out
}

/// `x + y` on four limbs; panics on a carry out of 256 bits.
fn add(x: [u64; 4], y: [u64; 4]) -> [u64; 4] {
    let mut out = [0u64; 4];
    let mut carry = 0u128;
    for i in 0..4 {
        let t = u128::from(x[i]) + u128::from(y[i]) + carry;
        out[i] = t as u64;
        carry = t >> 64;
    }
    assert_eq!(carry, 0);
    out
}

fn compare(x: [u64; 4], y: [u64; 4]) -> Ordering {
    x.iter().rev().cmp(y.iter().rev())
}

fn limbs(value: u128) -> [u64; 4] {
    [value as u64, (value >> 64) as u64, 0, 0]
}

/// Checks `mul_div` and `mul_div_up` on `(a, b, c)` against the 256-bit oracle.
fn check_mul_div(a: u128, b: u128, c: u128) {
    let down = mul_div(a, b, c);
    let up = mul_div_up(a, b, c);
    if c == 0 {
        assert_eq!((down, up), (None, None), "{a} · {b} / 0");
        return;
    }
    let ab = product(a, b);
    match down {
        Some(q) => {
            // q · c ≤ a · b < (q + 1) · c
            let qc = product(q, c);
            assert_ne!(
                compare(qc, ab),
                Ordering::Greater,
                "{a} · {b} / {c} = {q} too big"
            );
            assert_eq!(
                compare(add(qc, limbs(c)), ab),
                Ordering::Greater,
                "{a} · {b} / {c} = {q} too small"
            );
            let exact = compare(qc, ab) == Ordering::Equal;
            let expected_up = if exact { Some(q) } else { q.checked_add(1) };
            assert_eq!(up, expected_up, "ceil({a} · {b} / {c})");
        }
        None => {
            // The quotient does not fit: a · b ≥ 2^128 · c
            let shifted = [0, 0, c as u64, (c >> 64) as u64];
            assert_ne!(compare(ab, shifted), Ordering::Less, "{a} · {b} / {c} fits");
            assert_eq!(up, None);
        }
    }
}

#[test]
fn mul_div_matches_u128_arithmetic_where_it_fits() {
    for a in 0..24u128 {
        for b in 0..24u128 {
            for c in 0..24u128 {
                assert_eq!(mul_div(a, b, c), (a * b).checked_div(c));
                assert_eq!(mul_div_up(a, b, c), (c > 0).then(|| (a * b).div_ceil(c)));
            }
        }
    }
    let mut rng = Rng(1);
    let mut checked = 0;
    while checked < 20_000 {
        let (a, b, c) = (rng.bits(), rng.bits(), rng.bits());
        let Some(ab) = a.checked_mul(b) else {
            continue;
        };
        checked += 1;
        assert_eq!(mul_div(a, b, c), ab.checked_div(c), "{a} · {b} / {c}");
        assert_eq!(
            mul_div_up(a, b, c),
            (c > 0).then(|| ab.div_ceil(c)),
            "ceil({a} · {b} / {c})"
        );
    }
}

#[test]
fn mul_div_is_exact_beyond_u128() {
    let p127 = 1u128 << 127;
    // Hand-computed cases whose product does not fit in u128
    assert_eq!(mul_div(MAX, MAX, MAX), Some(MAX));
    assert_eq!(mul_div_up(MAX, MAX, MAX), Some(MAX));
    assert_eq!(mul_div(p127, 4, 2), None);
    assert_eq!(mul_div(p127, 2, 2), Some(p127));
    assert_eq!(mul_div(MAX, 2, 2), Some(MAX));
    assert_eq!(mul_div(MAX, MAX, 1), None);
    assert_eq!(mul_div(MAX, MAX - 1, MAX), Some(MAX - 1));
    // MAX² / (MAX − 1) = MAX + 1 + 1 / (MAX − 1)
    assert_eq!(mul_div(MAX, MAX, MAX - 1), None);
    // (2^127 + 1)(2^127 + 3) / (2^127 + 2) = 2^127 + 2 − 1 / (2^127 + 2)
    assert_eq!(mul_div(p127 + 1, p127 + 3, p127 + 2), Some(p127 + 1));
    assert_eq!(mul_div_up(p127 + 1, p127 + 3, p127 + 2), Some(p127 + 2));
    // (2^128 − 1) · 3 / 4 = 3 · 2^126 − 3/4
    assert_eq!(mul_div(MAX, 3, 4), Some((3 << 126) - 1));
    assert_eq!(mul_div_up(MAX, 3, 4), Some(3 << 126));
    assert_eq!(mul_div(1 << 64, 1 << 64, 1), None);
    assert_eq!(mul_div(1 << 64, 1 << 64, 2), Some(p127));
    assert_eq!(mul_div(1 << 100, 1 << 100, 1 << 90), Some(1 << 110));
    let e30 = 10u128.pow(30);
    assert_eq!(mul_div(e30, e30, 10u128.pow(25)), Some(10u128.pow(35)));
    assert_eq!(mul_div(e30, e30 + 7, e30), Some(e30 + 7));
    assert_eq!(mul_div(WAD * WAD, WAD * WAD, WAD * WAD), Some(WAD * WAD));
    assert_eq!(mul_div(WAD * WAD, WAD * WAD, WAD), None);
    // Division by zero and zero factors
    assert_eq!(mul_div(0, 0, 0), None);
    assert_eq!(mul_div_up(MAX, MAX, 0), None);
    assert_eq!(mul_div(0, MAX, 1), Some(0));
    assert_eq!(mul_div_up(MAX, 0, 7), Some(0));
    // Rounding up at the very top: exact quotients of u128::MAX stay, larger ones do not fit
    assert_eq!(mul_div_up(MAX, 2, 2), Some(MAX));
    assert_eq!(mul_div_up(MAX - 1, MAX, MAX - 1), Some(MAX));
    assert_eq!(mul_div_up(MAX, MAX, MAX - 1), None);

    // Random cases of every size against the oracle
    let mut rng = Rng(2);
    for _ in 0..50_000 {
        check_mul_div(rng.bits(), rng.bits(), rng.bits());
    }
    // Divisors close to the high half, where the long division carries out of u128 most often
    for _ in 0..20_000 {
        let (a, b) = (rng.bits() | p127, rng.bits() | p127);
        let high = mul_div(a, b, MAX).unwrap_or(MAX);
        let c = high.saturating_add(rng.below(3)).max(1);
        check_mul_div(a, b, c);
        check_mul_div(a, b, c | p127);
    }
}

// ---------------------------------------------------------------------------
// init

#[test]
fn init_validates_the_market() {
    mock::reset();
    mock::set_block_timestamp(T0);
    mock::register_asset(LOAN, "asset", 8, [0; 32]);
    mock::mint(LOAN, ADMIN, 10).unwrap();
    let with = |change: fn(&mut Params)| {
        let mut p = params();
        change(&mut p);
        p
    };
    let factors = "invalid collateral factor or liquidation threshold";
    let bonus = "invalid liquidation bonus";
    let rates = "invalid interest rate model";
    let refused = [
        (with(|p| p.collateral_factor_bps = 0), factors),
        (with(|p| p.collateral_factor_bps = 8_001), factors),
        (
            with(|p| {
                p.liquidation_threshold_bps = 10_000;
                p.liquidation_bonus_bps = 0;
            }),
            factors,
        ),
        (
            with(|p| {
                p.collateral_factor_bps = 4_000;
                p.liquidation_threshold_bps = 4_000;
                p.liquidation_bonus_bps = 2_001;
            }),
            bonus,
        ),
        // 8 334 · 12 000 and 9 600 · 10 500 exceed 10^8
        (
            with(|p| {
                p.liquidation_threshold_bps = 8_334;
                p.liquidation_bonus_bps = 2_000;
            }),
            bonus,
        ),
        (with(|p| p.liquidation_threshold_bps = 9_600), bonus),
        (
            with(|p| p.reserve_factor_bps = 5_001),
            "invalid reserve factor",
        ),
        (with(|p| p.kink_bps = 0), rates),
        (with(|p| p.kink_bps = 10_001), rates),
        (with(|p| p.base_rate_bps = 100_001), rates),
        (with(|p| p.slope1_bps = 100_001), rates),
        (with(|p| p.slope2_bps = 100_001), rates),
    ];
    for (nonce, (p, expected)) in refused.into_iter().enumerate() {
        assert_eq!(
            reason(deploy(nonce as u64, LOAN, COLL, WAD, p)),
            expected,
            "{p:?}"
        );
    }
    assert_eq!(
        reason(deploy(20, LOAN, LOAN, WAD, params())),
        "loan and collateral assets must differ"
    );
    assert_eq!(
        reason(deploy(21, LOAN, COLL, 0, params())),
        "price must be positive"
    );
    let args = encode(&(LOAN, COLL, WAD, params()));
    assert_eq!(
        reason(mock::deploy(
            [0; 32],
            0,
            b"lending",
            lending::dispatch,
            &args
        )),
        "zero account"
    );
    let attached = [funds(LOAN, 10)];
    let result =
        mock::deploy_with_funds(ADMIN, 22, b"lending", lending::dispatch, &args, &attached);
    assert_eq!(reason(result), "unexpected attached funds");
    assert_eq!(mock::balance(&LOAN, &ADMIN), 10);
    let short = encode(&(LOAN, COLL, WAD));
    assert_eq!(
        reason(mock::deploy(
            ADMIN,
            23,
            b"lending",
            lending::dispatch,
            &short
        )),
        "malformed arguments"
    );
    // The bounds themselves are accepted
    let edges = [
        with(|p| {
            p.collateral_factor_bps = 9_999;
            p.liquidation_threshold_bps = 9_999;
            p.liquidation_bonus_bps = 0;
        }),
        with(|p| {
            p.collateral_factor_bps = 1;
            p.liquidation_threshold_bps = 8_333;
            p.liquidation_bonus_bps = 2_000;
        }),
        with(|p| p.liquidation_threshold_bps = 9_523),
        Params {
            collateral_factor_bps: 1,
            liquidation_threshold_bps: 1,
            liquidation_bonus_bps: 2_000,
            reserve_factor_bps: 5_000,
            base_rate_bps: 100_000,
            slope1_bps: 100_000,
            kink_bps: 10_000,
            slope2_bps: 100_000,
        },
        Params {
            reserve_factor_bps: 0,
            base_rate_bps: 0,
            slope1_bps: 0,
            kink_bps: 1,
            slope2_bps: 0,
            ..params()
        },
    ];
    for (nonce, p) in edges.into_iter().enumerate() {
        deploy(30 + nonce as u64, LOAN, COLL, 1, p).unwrap();
    }
    deploy(40, LOAN, COLL, MAX, params()).unwrap();
}

#[test]
fn init_opens_an_empty_market() {
    let m = setup();
    let events: Vec<Vec<u8>> = mock::events().into_iter().map(|e| e.data).collect();
    assert_eq!(
        events,
        vec![
            encode(&("admin", None::<[u8; 32]>, Some(ADMIN))),
            encode(&("price", WAD))
        ]
    );
    let empty = Market {
        loan_asset: LOAN,
        collateral_asset: COLL,
        admin: Some(ADMIN),
        price: WAD,
        params: params(),
        cash: 0,
        total_borrows: 0,
        reserves: 0,
        total_shares: 0,
        total_collateral: 0,
        borrow_index: WAD,
        last_accrual: T0,
        utilization_bps: 0,
        borrow_rate_bps: 200,
        supply_rate_bps: 0,
    };
    assert_eq!(market(m), empty);
    let nothing = Position {
        shares: 0,
        supplied: 0,
        collateral: 0,
        debt: 0,
        borrow_limit: 0,
        liquidation_limit: 0,
        health_bps: None,
    };
    assert_eq!(position(m, ALICE), nothing);
    // init runs once: the VM refuses it after the deployment, and the contract refuses a second one too
    let args = encode(&(LOAN, COLL, WAD, params()));
    assert!(matches!(
        mock::call(ADMIN, m, "init", &args),
        Err(Failure::Trap(_))
    ));
    assert_eq!(
        mock::with_context(ADMIN, m, || lending::dispatch("init", &args)),
        Err(b"already initialized".to_vec())
    );
    assert_eq!(
        reason(mock::call(ADMIN, m, "nonexistent", &[])),
        "unknown method"
    );
    assert_eq!(
        reason(mock::call(ADMIN, m, "market", &[0])),
        "malformed arguments"
    );
}

#[test]
fn every_method_needs_init() {
    mock::reset();
    let calls = [
        ("supply", vec![]),
        ("withdraw", encode(&1u128)),
        ("deposit_collateral", vec![]),
        ("withdraw_collateral", encode(&1u128)),
        ("borrow", encode(&1u128)),
        ("repay", encode(&BOB)),
        ("liquidate", encode(&BOB)),
        ("set_price", encode(&WAD)),
        ("set_admin", encode(&Some(BOB))),
        ("withdraw_reserves", encode(&(1u128, BOB))),
        ("market", vec![]),
        ("position", encode(&BOB)),
    ];
    for (method, args) in calls {
        let result = mock::with_context(ALICE, [0x77; 32], || lending::dispatch(method, &args));
        assert_eq!(result, Err(b"not initialized".to_vec()), "{method}");
    }
}

// ---------------------------------------------------------------------------
// Supplying, collateral, borrowing and repaying

#[test]
fn suppliers_get_shares_and_take_them_out() {
    let m = setup();
    assert_eq!(supply(m, ALICE, 1_000), Ok(1_000));
    assert_eq!(
        last_event(),
        encode(&("supply", ALICE, 1_000u128, 1_000u128))
    );
    assert_eq!(supply(m, BOB, 500), Ok(500));
    assert_eq!(mock::balance(&LOAN, &ALICE), WALLET - 1_000);
    assert_eq!(mock::balance(&LOAN, &m), 1_500);
    let mk = market(m);
    assert_eq!((mk.cash, mk.total_shares), (1_500, 1_500));
    let p = position(m, ALICE);
    assert_eq!((p.shares, p.supplied), (1_000, 1_000));
    // Only the loan asset, alone
    for attached in [
        vec![],
        vec![funds(COLL, 10)],
        vec![funds(OTHER, 10)],
        vec![funds(LOAN, 10), funds(COLL, 10)],
    ] {
        let result = call::<u128>(ALICE, m, "supply", &[], &attached);
        assert_eq!(reason(result), "attach only the loan asset", "{attached:?}");
    }
    let result = call::<u128>(ALICE, m, "supply", &encode(&1u128), &[funds(LOAN, 1)]);
    assert_eq!(reason(result), "malformed arguments");
    assert_eq!(reason(withdraw(m, ALICE, 0)), "amount must be positive");
    assert_eq!(reason(withdraw(m, ALICE, 1_001)), "insufficient shares");
    assert_eq!(reason(withdraw(m, CAROL, 1)), "insufficient shares");
    assert_eq!(withdraw(m, ALICE, 400), Ok(400));
    assert_eq!(last_event(), encode(&("withdraw", ALICE, 400u128, 400u128)));
    assert_eq!(mock::balance(&LOAN, &ALICE), WALLET - 600);
    assert_eq!(withdraw(m, ALICE, 600), Ok(600));
    // A zero balance removes its key
    let key = [b"s".as_slice(), ALICE.as_slice()].concat();
    assert_eq!(mock::storage_value(&m, &key), None);
    assert_eq!(market(m).total_shares, 500);
    assert_eq!(mock::balance(&LOAN, &m), 500);
    assert_eq!(mock::balance(&LOAN, &ALICE), WALLET);
}

#[test]
fn collateral_comes_in_and_out_within_the_borrow_limit() {
    let m = setup();
    supply(m, ALICE, 1_000_000).unwrap();
    assert_eq!(deposit(m, BOB, 1_000), Ok(1_000));
    assert_eq!(
        last_event(),
        encode(&("deposit_collateral", BOB, 1_000u128))
    );
    assert_eq!(deposit(m, BOB, 500), Ok(1_500));
    assert_eq!(mock::balance(&COLL, &m), 1_500);
    assert_eq!(market(m).total_collateral, 1_500);
    for attached in [
        vec![],
        vec![funds(LOAN, 10)],
        vec![funds(OTHER, 10)],
        vec![funds(LOAN, 10), funds(COLL, 10)],
    ] {
        let result = call::<u128>(BOB, m, "deposit_collateral", &[], &attached);
        assert_eq!(
            reason(result),
            "attach only the collateral asset",
            "{attached:?}"
        );
    }
    assert_eq!(
        reason(withdraw_collateral(m, BOB, 0)),
        "amount must be positive"
    );
    assert_eq!(
        reason(withdraw_collateral(m, BOB, 1_501)),
        "insufficient collateral"
    );
    assert_eq!(withdraw_collateral(m, BOB, 500), Ok(1_000));
    assert_eq!(last_event(), encode(&("withdraw_collateral", BOB, 500u128)));
    assert_eq!(mock::balance(&COLL, &BOB), WALLET - 1_000);
    // 1 000 at price 1 lets BOB borrow 750; 800 still cover a debt of 600, 799 do not (599)
    assert_eq!(borrow(m, BOB, 600), Ok(600));
    assert_eq!(withdraw_collateral(m, BOB, 200), Ok(800));
    assert_eq!(
        reason(withdraw_collateral(m, BOB, 1)),
        "position would be unhealthy"
    );
    assert_eq!(market(m).total_collateral, 800);
    assert_eq!(mock::balance(&COLL, &m), 800);
}

#[test]
fn borrowing_stays_within_the_borrow_limit_and_the_liquidity() {
    let m = setup();
    supply(m, ALICE, 1_000).unwrap();
    assert_eq!(reason(borrow(m, BOB, 0)), "amount must be positive");
    assert_eq!(reason(borrow(m, BOB, 1)), "borrow limit exceeded");
    deposit(m, BOB, 1_000).unwrap();
    assert_eq!(position(m, BOB).borrow_limit, 750);
    assert_eq!(reason(borrow(m, BOB, 751)), "borrow limit exceeded");
    assert_eq!(borrow(m, BOB, 700), Ok(700));
    assert_eq!(last_event(), encode(&("borrow", BOB, 700u128)));
    assert_eq!(borrow(m, BOB, 50), Ok(750));
    assert_eq!(reason(borrow(m, BOB, 1)), "borrow limit exceeded");
    assert_eq!(mock::balance(&LOAN, &BOB), WALLET + 750);
    let mk = market(m);
    assert_eq!((mk.cash, mk.total_borrows), (250, 750));
    assert_eq!(
        position(m, BOB),
        Position {
            shares: 0,
            supplied: 0,
            collateral: 1_000,
            debt: 750,
            borrow_limit: 750,
            liquidation_limit: 800,
            health_bps: Some(10_666),
        }
    );
    // Only the 250 not lent can be borrowed
    deposit(m, CAROL, 10_000).unwrap();
    assert_eq!(reason(borrow(m, CAROL, 251)), "insufficient liquidity");
    assert_eq!(borrow(m, CAROL, 250), Ok(250));
    assert_eq!(reason(borrow(m, CAROL, 1)), "insufficient liquidity");
}

#[test]
fn withdrawals_wait_for_liquidity() {
    let m = setup();
    supply(m, ALICE, 1_000_000_000).unwrap();
    deposit(m, BOB, 2_000_000_000).unwrap();
    borrow(m, BOB, 750_000_000).unwrap();
    assert_eq!(
        reason(withdraw(m, ALICE, 1_000_000_000)),
        "insufficient liquidity"
    );
    assert_eq!(
        reason(withdraw(m, ALICE, 250_000_001)),
        "insufficient liquidity"
    );
    assert_eq!(withdraw(m, ALICE, 250_000_000), Ok(250_000_000));
    assert_eq!(reason(withdraw(m, ALICE, 1)), "insufficient liquidity");
    // The repayment brings the cash back
    assert_eq!(repay(m, BOB, BOB, 750_000_000), Ok(0));
    assert_eq!(withdraw(m, ALICE, 750_000_000), Ok(750_000_000));
    assert_eq!(mock::balance(&LOAN, &m), 0);
}

#[test]
fn anyone_can_repay_and_the_excess_is_refunded() {
    let m = setup();
    supply(m, ALICE, 1_000_000).unwrap();
    deposit(m, BOB, 1_000_000).unwrap();
    borrow(m, BOB, 600_000).unwrap();
    assert_eq!(reason(repay(m, CAROL, CAROL, 1)), "no debt");
    for attached in [
        vec![],
        vec![funds(COLL, 10)],
        vec![funds(LOAN, 10), funds(COLL, 10)],
    ] {
        let result = call::<u128>(CAROL, m, "repay", &encode(&BOB), &attached);
        assert_eq!(reason(result), "attach only the loan asset", "{attached:?}");
    }
    assert_eq!(repay(m, CAROL, BOB, 100_000), Ok(500_000));
    assert_eq!(last_event(), encode(&("repay", CAROL, BOB, 100_000u128)));
    assert_eq!(mock::balance(&LOAN, &CAROL), WALLET - 100_000);
    // Overpaying takes only the debt
    assert_eq!(repay(m, BOB, BOB, 800_000), Ok(0));
    assert_eq!(last_event(), encode(&("repay", BOB, BOB, 500_000u128)));
    assert_eq!(mock::balance(&LOAN, &BOB), WALLET + 600_000 - 500_000);
    let mk = market(m);
    assert_eq!((mk.cash, mk.total_borrows), (1_000_000, 0));
    assert_eq!(position(m, BOB).debt, 0);
    assert_eq!(reason(repay(m, BOB, BOB, 1)), "no debt");
    let key = [b"d".as_slice(), BOB.as_slice()].concat();
    assert_eq!(mock::storage_value(&m, &key), None);
    // Without a debt all the collateral comes back
    assert_eq!(withdraw_collateral(m, BOB, 1_000_000), Ok(0));
}

// ---------------------------------------------------------------------------
// Interest

#[test]
fn one_year_of_interest_matches_the_rate() {
    let m = setup();
    supply(m, ALICE, 1_000_000_000).unwrap();
    deposit(m, BOB, 10_000_000_000).unwrap();
    borrow(m, BOB, 500_000_000).unwrap();
    // 50% utilization, below the kink: 200 + 1 000 · 5 000 / 8 000 = 825 bps; suppliers get 825 · 50% · 90%
    let start = market(m);
    assert_eq!(
        (
            start.utilization_bps,
            start.borrow_rate_bps,
            start.supply_rate_bps
        ),
        (5_000, 825, 371)
    );
    mock::set_block_timestamp(T0 + YEAR);
    let year = market(m);
    let index = WAD + 825 * WAD / 10_000 / u128::from(YEAR) * u128::from(YEAR);
    assert_eq!(year.borrow_index, index);
    assert_eq!(year.total_borrows, 500_000_000 * index / WAD);
    // 8.25% of 500 000 000 is 41 250 000; the rate per second, rounded down, costs at most one unit
    let interest = year.total_borrows - 500_000_000;
    assert!((41_249_999..=41_250_000).contains(&interest), "{interest}");
    assert_eq!(year.reserves, interest / 10);
    assert_eq!(year.last_accrual, T0 + YEAR);
    // The debt is rounded up, total_borrows down
    let debt = position(m, BOB).debt;
    assert_eq!(debt, (500_000_000 * index).div_ceil(WAD));
    assert!(debt - year.total_borrows <= 1);
    assert_eq!(
        position(m, ALICE).supplied,
        1_000_000_000 + interest - interest / 10
    );
    // A share is now worth more than one unit
    assert_eq!(reason(supply(m, CAROL, 1)), "amount too small");
    // Repaying the whole debt empties total_borrows even when it is a unit short of the debt
    assert_eq!(repay(m, BOB, BOB, debt), Ok(0));
    let repaid = market(m);
    assert_eq!((repaid.total_borrows, repaid.cash), (0, 500_000_000 + debt));
    // The supplier takes all but the reserves, the admin the reserves: nothing is left behind
    assert_eq!(withdraw(m, ALICE, 1_000_000_000), Ok(total_assets(&repaid)));
    assert_eq!(withdraw_reserves(m, ADMIN, repaid.reserves, ADMIN), Ok(()));
    assert_eq!(mock::balance(&LOAN, &m), 0);
    let end = market(m);
    assert_eq!((end.cash, end.reserves, end.total_shares), (0, 0, 0));
}

#[test]
fn the_rate_follows_utilization_around_the_kink() {
    let m = setup();
    let rates = |mk: Market| (mk.utilization_bps, mk.borrow_rate_bps, mk.supply_rate_bps);
    assert_eq!(rates(market(m)), (0, 200, 0));
    supply(m, ALICE, 1_000_000_000).unwrap();
    deposit(m, BOB, 10_000_000_000).unwrap();
    for (borrowed, expected) in [
        // 200 + 1 000 · 4 000 / 8 000
        (400_000_000u128, (4_000, 700, 252)),
        // the kink
        (800_000_000, (8_000, 1_200, 864)),
        // 200 + 1 000 + 10 000 · 1 000 / 2 000
        (900_000_000, (9_000, 6_200, 5_022)),
        (1_000_000_000, (10_000, 11_200, 10_080)),
    ] {
        let debt = position(m, BOB).debt;
        borrow(m, BOB, borrowed - debt).unwrap();
        assert_eq!(rates(market(m)), expected, "{borrowed}");
    }
    // A year at full utilization costs 112%, within rounding
    mock::set_block_timestamp(T0 + YEAR);
    let mk = market(m);
    let interest = mk.total_borrows - 1_000_000_000;
    assert!(
        (1_119_999_999..=1_120_000_000).contains(&interest),
        "{interest}"
    );
    // The reserves are kept out of the assets: the utilization stays capped at 100%
    assert!(mk.total_borrows > total_assets(&mk));
    assert_eq!(mk.utilization_bps, 10_000);
    // With the kink at 100% the second slope never applies
    let flat = deploy(
        1,
        LOAN,
        COLL,
        WAD,
        Params {
            kink_bps: 10_000,
            ..params()
        },
    )
    .unwrap();
    supply(flat, ALICE, 1_000).unwrap();
    deposit(flat, BOB, 10_000).unwrap();
    borrow(flat, BOB, 500).unwrap();
    assert_eq!(rates(market(flat)), (5_000, 700, 315));
    borrow(flat, BOB, 500).unwrap();
    assert_eq!(rates(market(flat)), (10_000, 1_200, 1_080));
}

#[test]
fn a_timestamp_that_goes_back_accrues_nothing() {
    let m = setup();
    supply(m, ALICE, 1_000_000_000).unwrap();
    deposit(m, BOB, 10_000_000_000).unwrap();
    borrow(m, BOB, 500_000_000).unwrap();
    mock::set_block_timestamp(T0 + 1_000);
    supply(m, CAROL, 1_000).unwrap();
    let stored = market(m);
    assert_eq!(stored.last_accrual, T0 + 1_000);
    assert!(stored.total_borrows > 500_000_000);
    // An earlier block: nothing accrues, nothing fails, and the last accrual stays where it was
    mock::set_block_timestamp(T0 + 500);
    assert_eq!(market(m), stored);
    supply(m, CAROL, 1_000).unwrap();
    borrow(m, BOB, 1).unwrap();
    let back = market(m);
    assert_eq!(back.last_accrual, T0 + 1_000);
    assert_eq!(back.borrow_index, stored.borrow_index);
    assert_eq!(back.total_borrows, stored.total_borrows + 1);
    // Later blocks accrue from the last accrual: 600 seconds, not 1 100
    mock::set_block_timestamp(T0 + 1_600);
    let mut expected = back;
    accrue_model(&mut expected, T0 + 1_600);
    derive_market(&mut expected);
    assert_eq!(market(m), expected);
    assert_eq!(expected.last_accrual, T0 + 1_600);
    assert!(expected.total_borrows > back.total_borrows);
}

#[test]
fn views_accrue_without_writing() {
    let m = setup();
    supply(m, ALICE, 1_000_000).unwrap();
    deposit(m, BOB, 1_000_000).unwrap();
    borrow(m, BOB, 500_000).unwrap();
    let stored = mock::storage_value(&m, b"state").unwrap();
    mock::clear_events();
    mock::set_block_timestamp(T0 + 30 * DAY);
    let viewed = market(m);
    assert!(viewed.total_borrows > 500_000);
    assert!(position(m, BOB).debt > 500_000);
    assert_eq!(mock::storage_value(&m, b"state").unwrap(), stored);
    assert!(mock::events().is_empty());
    // set_price stores the same accrual before the new price
    set_price(m, ADMIN, 2 * WAD).unwrap();
    assert_ne!(mock::storage_value(&m, b"state").unwrap(), stored);
    let after = market(m);
    assert_eq!(
        (
            after.total_borrows,
            after.borrow_index,
            after.reserves,
            after.last_accrual
        ),
        (
            viewed.total_borrows,
            viewed.borrow_index,
            viewed.reserves,
            T0 + 30 * DAY
        )
    );
}

// ---------------------------------------------------------------------------
// Liquidations

#[test]
fn a_price_drop_makes_a_position_liquidatable() {
    let m = setup();
    supply(m, ALICE, 1_000_000_000).unwrap();
    deposit(m, BOB, 1_000_000_000).unwrap();
    assert_eq!(reason(borrow(m, BOB, 750_000_001)), "borrow limit exceeded");
    borrow(m, BOB, 750_000_000).unwrap();
    assert_eq!(position(m, BOB).health_bps, Some(10_666));
    assert_eq!(
        reason(liquidate(m, CAROL, BOB, 1_000)),
        "position is healthy"
    );
    assert_eq!(
        reason(liquidate(m, CAROL, DAVE, 1_000)),
        "position is healthy"
    );
    // 5% down: 950 000 000 · 80% = 760 000 000 still covers the debt
    set_price(m, ADMIN, WAD / 100 * 95).unwrap();
    assert_eq!(position(m, BOB).health_bps, Some(10_133));
    assert_eq!(
        reason(liquidate(m, CAROL, BOB, 1_000)),
        "position is healthy"
    );
    // 10% down: 720 000 000 does not
    set_price(m, ADMIN, WAD / 10 * 9).unwrap();
    let before = position(m, BOB);
    assert_eq!(
        (before.liquidation_limit, before.health_bps),
        (720_000_000, Some(9_600))
    );
    assert_eq!(
        reason(liquidate(m, BOB, BOB, 1_000)),
        "cannot liquidate yourself"
    );
    for attached in [
        vec![],
        vec![funds(COLL, 10)],
        vec![funds(LOAN, 10), funds(COLL, 10)],
    ] {
        let result = call::<(u128, u128)>(CAROL, m, "liquidate", &encode(&BOB), &attached);
        assert_eq!(reason(result), "attach only the loan asset", "{attached:?}");
    }
    // Half the debt at most, the rest of the attached amount refunded;
    // 375 000 000 · 10 500 / 10 000 · WAD / (0.9 · WAD) = 437 500 000 seized
    assert_eq!(
        liquidate(m, CAROL, BOB, 1_000_000_000),
        Ok((375_000_000, 437_500_000))
    );
    assert_eq!(
        last_event(),
        encode(&("liquidate", CAROL, BOB, 375_000_000u128, 437_500_000u128))
    );
    assert_eq!(mock::balance(&LOAN, &CAROL), WALLET - 375_000_000);
    assert_eq!(mock::balance(&COLL, &CAROL), WALLET + 437_500_000);
    let after = position(m, BOB);
    assert_eq!((after.debt, after.collateral), (375_000_000, 562_500_000));
    let mk = market(m);
    assert_eq!(
        (mk.cash, mk.total_borrows, mk.total_collateral),
        (625_000_000, 375_000_000, 562_500_000)
    );
    assert_eq!(mock::balance(&COLL, &m), 562_500_000);
    // Healthy again: 562 500 000 · 0.9 · 80% = 405 000 000
    assert_eq!(after.liquidation_limit, 405_000_000);
    assert_eq!(
        reason(liquidate(m, DAVE, BOB, 1_000)),
        "position is healthy"
    );
    // Half price: 100 000 000 repaid seizes 100 000 000 · 1.05 / 0.5 = 210 000 000
    set_price(m, ADMIN, WAD / 2).unwrap();
    assert_eq!(
        liquidate(m, DAVE, BOB, 100_000_000),
        Ok((100_000_000, 210_000_000))
    );
    assert_eq!(position(m, BOB).debt, 275_000_000);
    // An odd debt: half of it rounds up
    set_price(m, ADMIN, WAD / 10).unwrap();
    repay(m, BOB, BOB, 1).unwrap();
    let (repaid, _) = liquidate(m, DAVE, BOB, 1_000_000_000).unwrap();
    assert_eq!(repaid, 137_500_000);
}

#[test]
fn a_liquidation_that_takes_all_the_collateral_writes_off_the_rest() {
    let m = setup();
    supply(m, ALICE, 1_000_000_000).unwrap();
    deposit(m, BOB, 1_000_000_000).unwrap();
    borrow(m, BOB, 700_000_000).unwrap();
    // The collateral falls to a tenth: worth 100 000 000 against a debt of 700 000 000.
    // Half the debt would seize 350 000 000 · 1.05 / 0.1 = 3 675 000 000, more than all of it.
    set_price(m, ADMIN, WAD / 10).unwrap();
    mock::clear_events();
    assert_eq!(
        liquidate(m, CAROL, BOB, 400_000_000),
        Ok((350_000_000, 1_000_000_000))
    );
    let events: Vec<Vec<u8>> = mock::events().into_iter().map(|e| e.data).collect();
    assert_eq!(
        events,
        vec![
            encode(&("liquidate", CAROL, BOB, 350_000_000u128, 1_000_000_000u128)),
            encode(&("write_off", BOB, 350_000_000u128)),
        ]
    );
    assert_eq!(mock::balance(&LOAN, &CAROL), WALLET - 350_000_000);
    assert_eq!(mock::balance(&COLL, &CAROL), WALLET + 1_000_000_000);
    let gone = position(m, BOB);
    assert_eq!((gone.collateral, gone.debt, gone.health_bps), (0, 0, None));
    let mk = market(m);
    assert_eq!(
        (mk.cash, mk.total_borrows, mk.total_collateral),
        (650_000_000, 0, 0)
    );
    // The suppliers bear the loss
    assert_eq!(position(m, ALICE).supplied, 650_000_000);
    assert_eq!(withdraw(m, ALICE, 1_000_000_000), Ok(650_000_000));
    assert_eq!((mock::balance(&LOAN, &m), mock::balance(&COLL, &m)), (0, 0));
    assert_eq!(reason(liquidate(m, CAROL, BOB, 1)), "position is healthy");
    assert_eq!(reason(repay(m, CAROL, BOB, 1)), "no debt");
}

#[test]
fn an_insolvent_market_refuses_supply_and_withdrawals() {
    let steep = Params {
        reserve_factor_bps: 5_000,
        base_rate_bps: 0,
        slope2_bps: 100_000,
        ..params()
    };
    let m = setup_with(WAD, steep);
    supply(m, ALICE, 1_000).unwrap();
    deposit(m, BOB, 2_000).unwrap();
    borrow(m, BOB, 1_000).unwrap();
    // A year at full utilization: 1 010% a year, half of the interest to the reserves
    mock::set_block_timestamp(T0 + YEAR);
    let mk = market(m);
    assert_eq!(mk.borrow_rate_bps, 101_000);
    assert!(mk.reserves > 5_000);
    // The price collapses: one unit seizes all the collateral and the rest of the debt is written off
    set_price(m, ADMIN, 1).unwrap();
    assert_eq!(liquidate(m, CAROL, BOB, 1), Ok((1, 2_000)));
    let mk = market(m);
    assert_eq!((mk.cash, mk.total_borrows, mk.utilization_bps), (1, 0, 0));
    // The reserves exceed what the market holds: the suppliers' assets are worth nothing
    assert_eq!(total_assets(&mk), 0);
    assert_eq!(position(m, ALICE).supplied, 0);
    assert_eq!(reason(supply(m, DAVE, 1_000_000)), "market is insolvent");
    assert_eq!(reason(withdraw(m, ALICE, 1_000)), "market is insolvent");
    // The admin takes out only the cash there is
    assert_eq!(
        reason(withdraw_reserves(m, ADMIN, 2, ADMIN)),
        "insufficient cash"
    );
    assert_eq!(withdraw_reserves(m, ADMIN, 1, ADMIN), Ok(()));
    assert_eq!(mock::balance(&LOAN, &m), 0);
}

// ---------------------------------------------------------------------------
// The admin, attached funds and the limits of u128

#[test]
fn only_the_admin_sets_the_price_and_the_admin() {
    let m = setup();
    assert_eq!(reason(set_price(m, BOB, 2 * WAD)), "only the admin");
    assert_eq!(reason(set_price(m, ADMIN, 0)), "price must be positive");
    assert_eq!(set_price(m, ADMIN, 2 * WAD), Ok(()));
    assert_eq!(last_event(), encode(&("price", 2 * WAD)));
    assert_eq!(market(m).price, 2 * WAD);
    assert_eq!(reason(set_admin(m, BOB, Some(BOB))), "only the admin");
    assert_eq!(reason(set_admin(m, ADMIN, Some([0; 32]))), "zero account");
    assert_eq!(set_admin(m, ADMIN, Some(BOB)), Ok(()));
    assert_eq!(last_event(), encode(&("admin", Some(ADMIN), Some(BOB))));
    assert_eq!(market(m).admin, Some(BOB));
    assert_eq!(reason(set_price(m, ADMIN, WAD)), "only the admin");
    assert_eq!(
        reason(withdraw_reserves(m, ADMIN, 1, ADMIN)),
        "only the admin"
    );
    assert_eq!(set_price(m, BOB, 3 * WAD), Ok(()));
    // None leaves the market without an admin: the price is frozen for good
    assert_eq!(set_admin(m, BOB, None), Ok(()));
    assert_eq!(
        last_event(),
        encode(&("admin", Some(BOB), None::<[u8; 32]>))
    );
    assert_eq!(market(m).admin, None);
    assert_eq!(mock::storage_value(&m, b"admin"), None);
    for who in [ADMIN, BOB] {
        assert_eq!(reason(set_price(m, who, WAD)), "only the admin");
        assert_eq!(reason(set_admin(m, who, Some(who))), "only the admin");
        assert_eq!(reason(withdraw_reserves(m, who, 1, who)), "only the admin");
    }
    assert_eq!(market(m).price, 3 * WAD);
}

#[test]
fn the_admin_withdraws_reserves() {
    let m = setup();
    supply(m, ALICE, 1_000_000_000).unwrap();
    deposit(m, BOB, 10_000_000_000).unwrap();
    borrow(m, BOB, 500_000_000).unwrap();
    mock::set_block_timestamp(T0 + YEAR);
    let reserves = market(m).reserves;
    assert!(reserves > 0);
    assert_eq!(reason(withdraw_reserves(m, BOB, 1, BOB)), "only the admin");
    assert_eq!(
        reason(withdraw_reserves(m, ADMIN, 0, ADMIN)),
        "amount must be positive"
    );
    assert_eq!(
        reason(withdraw_reserves(m, ADMIN, 1, [0; 32])),
        "zero account"
    );
    assert_eq!(
        reason(withdraw_reserves(m, ADMIN, reserves + 1, ADMIN)),
        "amount exceeds reserves"
    );
    assert_eq!(withdraw_reserves(m, ADMIN, reserves, DAVE), Ok(()));
    assert_eq!(last_event(), encode(&("withdraw_reserves", DAVE, reserves)));
    assert_eq!(mock::balance(&LOAN, &DAVE), WALLET + reserves);
    let mk = market(m);
    assert_eq!((mk.reserves, mk.cash), (0, 500_000_000 - reserves));
    assert_eq!(mock::balance(&LOAN, &m), mk.cash);
}

#[test]
fn methods_that_are_not_payable_refuse_attached_funds() {
    let m = setup();
    supply(m, ALICE, 1_000).unwrap();
    deposit(m, BOB, 1_000).unwrap();
    for (method, args) in [
        ("withdraw", encode(&1u128)),
        ("withdraw_collateral", encode(&1u128)),
        ("borrow", encode(&1u128)),
        ("set_price", encode(&WAD)),
        ("set_admin", encode(&Some(BOB))),
        ("withdraw_reserves", encode(&(1u128, ADMIN))),
        ("market", vec![]),
        ("position", encode(&ALICE)),
    ] {
        for who in [ALICE, BOB, ADMIN] {
            for attached in [
                vec![funds(LOAN, 5)],
                vec![funds(COLL, 5)],
                vec![funds(LOAN, 5), funds(OTHER, 5)],
            ] {
                let result = mock::call_with_funds(who, m, method, &args, &attached);
                assert_eq!(reason(result), "unexpected attached funds", "{method}");
            }
        }
    }
    assert_eq!(mock::balance(&LOAN, &m), 1_000);
    assert_eq!(mock::balance(&COLL, &m), 1_000);
    assert_eq!(mock::balance(&OTHER, &m), 0);
}

#[test]
fn extreme_prices_saturate_instead_of_reverting() {
    let m = setup_with(MAX, params());
    let big = 1u128 << 100;
    mock::mint(LOAN, ALICE, big).unwrap();
    mock::mint(LOAN, CAROL, big).unwrap();
    mock::mint(COLL, BOB, 2 * WAD).unwrap();
    supply(m, ALICE, big).unwrap();
    deposit(m, BOB, 2 * WAD).unwrap();
    // Collateral worth 2 · u128::MAX counts as u128::MAX
    let p = position(m, BOB);
    assert_eq!(p.borrow_limit, mul_div(MAX, 7_500, 10_000).unwrap());
    assert_eq!(p.liquidation_limit, mul_div(MAX, 8_000, 10_000).unwrap());
    // With a debt of one unit the health is above u128::MAX
    borrow(m, BOB, 1).unwrap();
    assert_eq!(position(m, BOB).health_bps, Some(MAX));
    borrow(m, BOB, big - 1).unwrap();
    // At the lowest price, half the debt would seize far more than u128::MAX: capped at the collateral
    set_price(m, ADMIN, 1).unwrap();
    assert_eq!(liquidate(m, CAROL, BOB, big), Ok((big / 2, 2 * WAD)));
    let mk = market(m);
    assert_eq!((mk.total_borrows, mk.total_collateral), (0, 0));
    assert_eq!(mock::balance(&COLL, &CAROL), WALLET + 2 * WAD);
    assert_eq!(mock::balance(&LOAN, &CAROL), WALLET + big / 2);
}

#[test]
fn an_accrual_that_overflows_reverts() {
    let fast = Params {
        base_rate_bps: 100_000,
        slope1_bps: 100_000,
        kink_bps: 10_000,
        slope2_bps: 100_000,
        ..params()
    };
    let m = setup_with(MAX, fast);
    let huge = 10u128.pow(35);
    mock::mint(LOAN, ALICE, huge).unwrap();
    mock::mint(COLL, BOB, 2 * WAD).unwrap();
    supply(m, ALICE, huge).unwrap();
    deposit(m, BOB, 2 * WAD).unwrap();
    borrow(m, BOB, huge).unwrap();
    // 2 000% a year for a thousand years multiplies the borrows by 20 001: beyond u128
    mock::set_block_timestamp(T0 + 1_000 * YEAR);
    assert_eq!(
        reason(call::<Market>(ALICE, m, "market", &[], &[])),
        "arithmetic overflow"
    );
    assert_eq!(reason(supply(m, ALICE, 1)), "arithmetic overflow");
    assert_eq!(reason(repay(m, BOB, BOB, 1)), "arithmetic overflow");
    assert_eq!(mock::balance(&LOAN, &BOB), WALLET + huge);
}

// ---------------------------------------------------------------------------
// Pseudo-random sequences, checked against the model step by step

/// Wallet slots after the five users: the admin, then the market itself.
const ADMIN_SLOT: usize = 5;
const MARKET_SLOT: usize = 6;

/// Everything observable: the market and every user's position (views at the current time) and every wallet.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Books {
    market: Market,
    positions: Vec<Position>,
    loan: Vec<u128>,
    coll: Vec<u128>,
}

fn books(m: [u8; 32]) -> Books {
    let wallets = |asset: [u8; 32]| -> Vec<u128> {
        USERS
            .iter()
            .chain([&ADMIN, &m])
            .map(|who| mock::balance(&asset, who))
            .collect()
    };
    Books {
        market: market(m),
        positions: USERS.iter().map(|who| position(m, *who)).collect(),
        loan: wallets(LOAN),
        coll: wallets(COLL),
    }
}

/// Recomputes every derived field of `books` from the primary ones.
fn derive(books: &mut Books) {
    derive_market(&mut books.market);
    let market = books.market;
    for p in &mut books.positions {
        derive_position(&market, p);
    }
}

fn events_data() -> Vec<Vec<u8>> {
    mock::events().into_iter().map(|e| e.data).collect()
}

/// A market driven by explicit operations; each result, every field of the books and the events are checked against
/// the model before the next operation.
struct Sim {
    m: [u8; 32],
    /// What every wallet held of each asset at the start.
    wallet: u128,
    /// The books now, as checked.
    view: Books,
    /// The market as the last operation that succeeded stored it.
    stored: Market,
    /// Each user's debt as stored, `(principal, index)`.
    debts: [(u128, u128); 5],
    /// The interest stored so far, the reserves it added, and the reserves withdrawn.
    interest: u128,
    reserves_added: u128,
    reserves_withdrawn: u128,
    /// A bound of how far the debts (unrounded) may exceed `total_borrows`: each stored accrual adds less than one unit
    /// (`total_borrows` rounds down) and scales what was there by the index's growth; each debt that changes adds less
    /// than one unit (its rounding up becomes part of the stored principal).
    slack: u128,
    /// The operations that succeeded, by name, the reverts, by name and reason, and the write-offs.
    done: BTreeMap<&'static str, u32>,
    reverts: BTreeMap<String, u32>,
    write_offs: u32,
}

impl Sim {
    fn new(params: Params, wallet: u128) -> Sim {
        let m = setup_scaled(WAD, params, wallet);
        mock::clear_events();
        let view = books(m);
        Sim {
            m,
            wallet,
            stored: view.market,
            view,
            debts: [(0, WAD); 5],
            interest: 0,
            reserves_added: 0,
            reserves_withdrawn: 0,
            slack: 0,
            done: BTreeMap::new(),
            reverts: BTreeMap::new(),
            write_offs: 0,
        }
    }

    /// User `i`'s debt at borrow index `index`: the stored principal grown by the index, rounded up.
    fn debt(&self, i: usize, index: u128) -> u128 {
        let (principal, snapshot) = self.debts[i];
        mul_div_up(principal, index, snapshot).unwrap()
    }

    /// The user with a debt whose health is the lowest, if any.
    fn weakest(&self) -> Option<usize> {
        (0..5)
            .filter(|&u| self.view.positions[u].debt > 0)
            .min_by_key(|&u| self.view.positions[u].health_bps)
    }

    /// A random user with a debt, if any.
    fn debtor(&self, rng: &mut Rng) -> Option<usize> {
        let debtors: Vec<usize> = (0..5)
            .filter(|&u| self.view.positions[u].debt > 0)
            .collect();
        (!debtors.is_empty()).then(|| debtors[rng.index(debtors.len())])
    }

    /// Moves the clock to `now`: the views must show the stored market accrued to `now` by the model.
    fn advance(&mut self, now: u64) {
        mock::set_block_timestamp(now);
        let mut expected = self.view.clone();
        expected.market = self.stored;
        accrue_model(&mut expected.market, now);
        let index = expected.market.borrow_index;
        for (i, p) in expected.positions.iter_mut().enumerate() {
            p.debt = self.debt(i, index);
        }
        derive(&mut expected);
        assert_eq!(books(self.m), expected, "accrual to {now}");
        self.view = expected;
    }

    /// Checks one operation: `got` is the contract's answer and `want` the model's, from the books before it; on
    /// success `apply` makes the expected changes on a copy of those books and returns the expected events. A revert
    /// must leave everything as it was.
    fn run<T: Copy + PartialEq + Debug>(
        &mut self,
        name: &'static str,
        got: Result<T, Failure>,
        want: Result<T, &str>,
        changes_debt: bool,
        apply: impl FnOnce(&mut Books, T) -> Vec<Vec<u8>>,
    ) -> Option<T> {
        let mut expected = self.view.clone();
        let result = match (got, want) {
            (Ok(got), Ok(want)) => {
                assert_eq!(got, want, "{name}");
                assert_eq!(events_data(), apply(&mut expected, got), "{name}: events");
                Some(got)
            }
            (Err(Failure::Revert { reason, .. }), Err(want)) => {
                assert_eq!(String::from_utf8(reason).unwrap(), want, "{name}");
                assert!(mock::events().is_empty(), "{name}: events of a revert");
                *self.reverts.entry(format!("{name}: {want}")).or_default() += 1;
                None
            }
            (got, want) => panic!("{name}: got {got:?}, expected {want:?}"),
        };
        derive(&mut expected);
        let actual = books(self.m);
        assert_eq!(actual, expected, "{name}");
        if result.is_some() {
            // The operation stored the accrual its views showed
            let (before, stored) = (self.view.market, self.stored);
            if before.borrow_index != stored.borrow_index {
                self.slack = (self.slack * before.borrow_index).div_ceil(stored.borrow_index) + 1;
            }
            self.interest += before.total_borrows - stored.total_borrows;
            self.reserves_added += before.reserves - stored.reserves;
            if changes_debt {
                self.slack += 1;
            }
            *self.done.entry(name).or_default() += 1;
            self.stored = actual.market;
        }
        self.view = actual;
        mock::clear_events();
        result
    }

    fn supply(&mut self, i: usize, amount: u128) {
        let mk = self.view.market;
        let assets = total_assets(&mk);
        let want = if mk.total_shares == 0 {
            Ok(amount)
        } else if assets == 0 {
            Err("market is insolvent")
        } else {
            Ok(mul_div(amount, mk.total_shares, assets).unwrap())
        }
        .and_then(|shares| {
            if shares == 0 {
                Err("amount too small")
            } else {
                Ok(shares)
            }
        });
        let got = supply(self.m, USERS[i], amount);
        self.run("supply", got, want, false, |b, shares| {
            b.market.cash += amount;
            b.market.total_shares += shares;
            b.positions[i].shares += shares;
            b.loan[i] -= amount;
            b.loan[MARKET_SLOT] += amount;
            vec![encode(&("supply", USERS[i], amount, shares))]
        });
    }

    fn withdraw(&mut self, i: usize, shares: u128) {
        let mk = self.view.market;
        let assets = total_assets(&mk);
        let want = if shares == 0 {
            Err("amount must be positive")
        } else if shares > self.view.positions[i].shares {
            Err("insufficient shares")
        } else if assets == 0 {
            Err("market is insolvent")
        } else {
            match mul_div(shares, assets, mk.total_shares).unwrap() {
                0 => Err("amount too small"),
                amount if amount > liquidity(&mk) => Err("insufficient liquidity"),
                amount => Ok(amount),
            }
        };
        let got = withdraw(self.m, USERS[i], shares);
        self.run("withdraw", got, want, false, |b, amount| {
            b.market.cash -= amount;
            b.market.total_shares -= shares;
            b.positions[i].shares -= shares;
            b.loan[i] += amount;
            b.loan[MARKET_SLOT] -= amount;
            vec![encode(&("withdraw", USERS[i], shares, amount))]
        });
    }

    fn deposit(&mut self, i: usize, amount: u128) {
        let want = Ok(self.view.positions[i].collateral + amount);
        let got = deposit(self.m, USERS[i], amount);
        self.run("deposit_collateral", got, want, false, |b, collateral| {
            b.market.total_collateral += amount;
            b.positions[i].collateral = collateral;
            b.coll[i] -= amount;
            b.coll[MARKET_SLOT] += amount;
            vec![encode(&("deposit_collateral", USERS[i], amount))]
        });
    }

    fn withdraw_collateral(&mut self, i: usize, amount: u128) {
        let p = self.view.positions[i];
        let want = if amount == 0 {
            Err("amount must be positive")
        } else if amount > p.collateral {
            Err("insufficient collateral")
        } else {
            let mut rest = Position {
                collateral: p.collateral - amount,
                ..p
            };
            derive_position(&self.view.market, &mut rest);
            if p.debt > rest.borrow_limit {
                Err("position would be unhealthy")
            } else {
                Ok(rest.collateral)
            }
        };
        let got = withdraw_collateral(self.m, USERS[i], amount);
        self.run("withdraw_collateral", got, want, false, |b, rest| {
            b.market.total_collateral -= amount;
            b.positions[i].collateral = rest;
            b.coll[i] += amount;
            b.coll[MARKET_SLOT] -= amount;
            vec![encode(&("withdraw_collateral", USERS[i], amount))]
        });
    }

    fn borrow(&mut self, i: usize, amount: u128) {
        let mk = self.view.market;
        let p = self.view.positions[i];
        let want = if amount == 0 {
            Err("amount must be positive")
        } else if amount > liquidity(&mk) {
            Err("insufficient liquidity")
        } else if p.debt + amount > p.borrow_limit {
            Err("borrow limit exceeded")
        } else {
            Ok(p.debt + amount)
        };
        let got = borrow(self.m, USERS[i], amount);
        let done = self.run("borrow", got, want, true, |b, debt| {
            b.market.cash -= amount;
            b.market.total_borrows += amount;
            b.positions[i].debt = debt;
            b.loan[i] += amount;
            b.loan[MARKET_SLOT] -= amount;
            vec![encode(&("borrow", USERS[i], amount))]
        });
        if let Some(debt) = done {
            self.debts[i] = (debt, self.stored.borrow_index);
        }
    }

    fn repay(&mut self, payer: usize, j: usize, amount: u128) {
        let debt = self.view.positions[j].debt;
        let paid = amount.min(debt);
        let want = if debt == 0 {
            Err("no debt")
        } else {
            Ok(debt - paid)
        };
        let got = repay(self.m, USERS[payer], USERS[j], amount);
        let done = self.run("repay", got, want, true, |b, rest| {
            b.market.cash += paid;
            b.market.total_borrows = b.market.total_borrows.saturating_sub(paid);
            b.positions[j].debt = rest;
            b.loan[payer] -= paid;
            b.loan[MARKET_SLOT] += paid;
            vec![encode(&("repay", USERS[payer], USERS[j], paid))]
        });
        if let Some(rest) = done {
            self.debts[j] = (rest, self.stored.borrow_index);
        }
    }

    fn liquidate(&mut self, k: usize, j: usize, amount: u128) {
        let mk = self.view.market;
        let p = self.view.positions[j];
        let want = if k == j {
            Err("cannot liquidate yourself")
        } else if p.debt <= p.liquidation_limit {
            Err("position is healthy")
        } else {
            let repaid = amount.min(p.debt.div_ceil(2));
            let value = repaid * u128::from(10_000 + mk.params.liquidation_bonus_bps) / 10_000;
            let seized = mul_div(value, WAD, mk.price)
                .unwrap_or(MAX)
                .min(p.collateral);
            Ok((repaid, seized))
        };
        let got = liquidate(self.m, USERS[k], USERS[j], amount);
        let done = self.run("liquidate", got, want, true, |b, (repaid, seized)| {
            let left = p.collateral - seized;
            let rest = p.debt - repaid;
            let written_off = if left == 0 { rest } else { 0 };
            b.market.cash += repaid;
            b.market.total_borrows = b
                .market
                .total_borrows
                .saturating_sub(repaid)
                .saturating_sub(written_off);
            b.market.total_collateral -= seized;
            b.positions[j].collateral = left;
            b.positions[j].debt = rest - written_off;
            b.loan[k] -= repaid;
            b.loan[MARKET_SLOT] += repaid;
            b.coll[k] += seized;
            b.coll[MARKET_SLOT] -= seized;
            let mut events = vec![encode(&("liquidate", USERS[k], USERS[j], repaid, seized))];
            if written_off > 0 {
                events.push(encode(&("write_off", USERS[j], written_off)));
            }
            events
        });
        if done.is_some() {
            let after = self.view.positions[j];
            self.debts[j] = (after.debt, self.stored.borrow_index);
            if after.collateral == 0 && p.debt > done.map_or(0, |(repaid, _)| repaid) {
                self.write_offs += 1;
            }
        }
    }

    fn set_price(&mut self, who: [u8; 32], price: u128) {
        let want = if who == ADMIN {
            Ok(())
        } else {
            Err("only the admin")
        };
        let got = set_price(self.m, who, price);
        self.run("set_price", got, want, false, |b, ()| {
            b.market.price = price;
            vec![encode(&("price", price))]
        });
    }

    fn withdraw_reserves(&mut self, amount: u128) {
        let mk = self.view.market;
        let want = if amount > mk.reserves {
            Err("amount exceeds reserves")
        } else if amount > mk.cash {
            Err("insufficient cash")
        } else {
            Ok(())
        };
        let got = withdraw_reserves(self.m, ADMIN, amount, ADMIN);
        let done = self.run("withdraw_reserves", got, want, false, |b, ()| {
            b.market.reserves -= amount;
            b.market.cash -= amount;
            b.loan[ADMIN_SLOT] += amount;
            b.loan[MARKET_SLOT] -= amount;
            vec![encode(&("withdraw_reserves", ADMIN, amount))]
        });
        if done.is_some() {
            self.reserves_withdrawn += amount;
        }
    }

    /// The invariants of the books.
    fn check(&self, step: usize) {
        let (b, mk) = (&self.view, &self.view.market);
        // The contract holds exactly what its books say
        assert_eq!(b.loan[MARKET_SLOT], mk.cash, "step {step}: loan asset held");
        assert_eq!(
            b.coll[MARKET_SLOT], mk.total_collateral,
            "step {step}: collateral held"
        );
        assert_eq!(
            b.positions.iter().map(|p| p.shares).sum::<u128>(),
            mk.total_shares,
            "step {step}: shares"
        );
        assert_eq!(
            b.positions.iter().map(|p| p.collateral).sum::<u128>(),
            mk.total_collateral,
            "step {step}: collateral"
        );
        assert!(
            b.positions.iter().map(|p| p.supplied).sum::<u128>() <= total_assets(mk),
            "step {step}: suppliers' claims above the assets"
        );
        // The debts never add up to less than total_borrows, and to more only by rounding
        let debts: u128 = b.positions.iter().map(|p| p.debt).sum();
        let debtors = b.positions.iter().filter(|p| p.debt > 0).count() as u128;
        let slack = if mk.borrow_index == self.stored.borrow_index {
            self.slack
        } else {
            (self.slack * mk.borrow_index).div_ceil(self.stored.borrow_index) + 1
        };
        assert!(
            debts >= mk.total_borrows,
            "step {step}: debts {debts} below total_borrows {}",
            mk.total_borrows
        );
        assert!(
            debts - mk.total_borrows <= slack + debtors,
            "step {step}: debts {debts} exceed total_borrows {} by more than {slack} + {debtors}",
            mk.total_borrows
        );
        // The reserves come only from interest, at the reserve factor
        assert_eq!(
            self.stored.reserves + self.reserves_withdrawn,
            self.reserves_added,
            "step {step}: reserves"
        );
        assert!(
            self.reserves_added
                <= mul_div(
                    self.interest,
                    u128::from(mk.params.reserve_factor_bps),
                    10_000
                )
                .unwrap(),
            "step {step}: reserves above their share of the interest"
        );
        // Nothing is created or lost
        assert_eq!(b.loan.iter().sum::<u128>(), 6 * self.wallet, "step {step}");
        assert_eq!(b.coll.iter().sum::<u128>(), 6 * self.wallet, "step {step}");
    }

    /// Every borrower repays and takes the collateral back, the suppliers withdraw until nothing more comes out, and
    /// the admin takes the reserves: the market ends empty, but for less than one unit per remaining holder, the
    /// rounding of shares worth less than a unit each.
    fn wind_down(&mut self) {
        for i in 0..5 {
            let debt = self.view.positions[i].debt;
            if debt > 0 {
                assert!(debt <= self.view.loan[i], "user {i} cannot repay {debt}");
                self.repay(i, i, debt);
            }
        }
        assert_eq!(self.view.market.total_borrows, 0);
        for i in 0..5 {
            let collateral = self.view.positions[i].collateral;
            if collateral > 0 {
                self.withdraw_collateral(i, collateral);
            }
        }
        loop {
            let withdrawn = self.done.get("withdraw").copied();
            for i in 0..5 {
                let shares = self.view.positions[i].shares;
                if shares > 0 {
                    self.withdraw(i, shares);
                }
            }
            if self.done.get("withdraw").copied() == withdrawn {
                break;
            }
        }
        let mk = self.view.market;
        let amount = mk.reserves.min(mk.cash);
        if amount > 0 {
            self.withdraw_reserves(amount);
        }
        self.check(usize::MAX);
        let end = self.view.market;
        let holders = self.view.positions.iter().filter(|p| p.shares > 0).count() as u128;
        assert_eq!((end.total_borrows, end.total_collateral), (0, 0));
        assert!(end.cash == 0 || end.cash < holders, "{} left", end.cash);
        assert_eq!(
            (self.view.loan[MARKET_SLOT], self.view.coll[MARKET_SLOT]),
            (end.cash, 0)
        );
    }
}

/// `steps` random operations of five users and the admin, with time jumps of up to three years and blocks whose
/// timestamp goes back, then a full wind-down. Amounts supplied and deposited go up to 10^9 · `scale`.
fn random_run(seed: u64, steps: usize, params: Params, scale: u128) -> Sim {
    let mut sim = Sim::new(params, WALLET * scale);
    let mut rng = Rng(seed);
    let most = 1_000_000_000 * scale;
    let mut now = T0;
    for step in 0..steps {
        now = match rng.below(100) {
            0 => now + 1 + rng.below(u128::from(3 * YEAR)) as u64,
            1..=5 => now.saturating_sub(1 + rng.below(u128::from(DAY)) as u64),
            6..=30 => now,
            _ => now + rng.below(u128::from(2 * DAY)) as u64,
        };
        sim.advance(now);
        let (i, j) = (rng.index(5), rng.index(5));
        let mk = sim.view.market;
        let p = sim.view.positions[i];
        match rng.below(100) {
            0..=13 => sim.supply(i, 1 + rng.below(most)),
            14..=23 => {
                let shares = match rng.below(4) {
                    0 => p.shares + 1 + rng.below(10),
                    1 => p.shares,
                    _ => 1 + rng.below(p.shares),
                };
                sim.withdraw(i, shares);
            }
            24..=35 => sim.deposit(i, 1 + rng.below(most)),
            36..=43 => {
                let amount = match rng.below(4) {
                    0 => p.collateral + 1,
                    1 => p.collateral,
                    _ => 1 + rng.below(p.collateral),
                };
                sim.withdraw_collateral(i, amount);
            }
            44..=63 => {
                let room = p.borrow_limit.saturating_sub(p.debt).min(liquidity(&mk));
                let amount = match rng.below(4) {
                    0 => room,
                    1 => room + 1 + rng.below(1_000),
                    _ => 1 + rng.below(room),
                };
                sim.borrow(i, amount);
            }
            64..=75 => {
                let borrower = sim.debtor(&mut rng).unwrap_or(j);
                let debt = sim.view.positions[borrower].debt;
                let amount = match rng.below(4) {
                    0 => debt + 1 + rng.below(1_000),
                    1 => debt.max(1),
                    _ => 1 + rng.below(debt),
                };
                sim.repay(i, borrower, amount.min(sim.view.loan[i]));
            }
            76..=88 => {
                let borrower = if rng.below(2) == 0 {
                    sim.weakest()
                } else {
                    sim.debtor(&mut rng)
                }
                .unwrap_or(j);
                let debt = sim.view.positions[borrower].debt;
                let amount = match rng.below(3) {
                    0 => debt.div_ceil(2) + rng.below(1_000),
                    1 => 1 + rng.below(debt),
                    _ => 1 + rng.below(1_000),
                }
                .max(1);
                sim.liquidate(i, borrower, amount.min(sim.view.loan[i]));
            }
            89..=96 => {
                let who = if rng.below(8) == 0 { USERS[i] } else { ADMIN };
                let factor = [40, 60, 80, 90, 95, 100, 105, 110, 125, 150, 175][rng.index(11)];
                sim.set_price(who, (mk.price * factor / 100).clamp(WAD / 100, 100 * WAD));
            }
            _ => {
                let amount = match rng.below(4) {
                    0 => mk.reserves + 1,
                    1 => mk.cash + 1,
                    _ => 1 + rng.below(mk.reserves.min(mk.cash)),
                };
                sim.withdraw_reserves(amount);
            }
        }
        sim.check(step);
    }
    sim.wind_down();
    sim
}

#[test]
fn random_operations_keep_the_books_balanced() {
    let runs = [
        (1, params(), 1),
        // Steep rates, half the interest to the reserves
        (
            2,
            Params {
                collateral_factor_bps: 5_000,
                liquidation_threshold_bps: 6_000,
                liquidation_bonus_bps: 1_000,
                reserve_factor_bps: 5_000,
                base_rate_bps: 0,
                slope1_bps: 2_000,
                kink_bps: 9_000,
                slope2_bps: 20_000,
            },
            1,
        ),
        // Thin margins, no reserves, no second slope
        (
            3,
            Params {
                collateral_factor_bps: 9_000,
                liquidation_threshold_bps: 9_500,
                liquidation_bonus_bps: 300,
                reserve_factor_bps: 0,
                base_rate_bps: 50,
                slope1_bps: 400,
                kink_bps: 10_000,
                slope2_bps: 0,
            },
            1,
        ),
        // The largest bonus, with the threshold at its bound: 8 333 · 12 000 ≤ 10^8
        (
            4,
            Params {
                collateral_factor_bps: 8_000,
                liquidation_threshold_bps: 8_333,
                liquidation_bonus_bps: 2_000,
                reserve_factor_bps: 2_500,
                base_rate_bps: 100,
                slope1_bps: 800,
                kink_bps: 5_000,
                slope2_bps: 3_000,
            },
            1,
        ),
        // Amounts up to 10^29: total_borrows · borrow_index far above 2^128, and debts so much larger than the index
        // that taking the interest from the index's own growth matters for the rounding
        (5, params(), 100_000_000_000_000_000_000),
    ];
    let (mut write_offs, mut reverts) = (0, BTreeSet::new());
    for (seed, p, scale) in runs {
        let sim = random_run(seed, 600, p, scale);
        for name in [
            "supply",
            "withdraw",
            "deposit_collateral",
            "withdraw_collateral",
            "borrow",
            "repay",
            "liquidate",
            "set_price",
            "withdraw_reserves",
        ] {
            if name != "withdraw_reserves" || p.reserve_factor_bps > 0 {
                assert!(
                    sim.done.contains_key(name),
                    "seed {seed}: no {name} succeeded"
                );
            }
        }
        assert!(sim.interest > 0, "seed {seed}: no interest");
        write_offs += sim.write_offs;
        reverts.extend(sim.reverts.into_keys());
    }
    assert!(write_offs > 0, "no write-off in any run");
    for expected in [
        "withdraw: amount must be positive",
        "withdraw: insufficient shares",
        "withdraw: insufficient liquidity",
        "withdraw_collateral: amount must be positive",
        "withdraw_collateral: insufficient collateral",
        "withdraw_collateral: position would be unhealthy",
        "borrow: amount must be positive",
        "borrow: insufficient liquidity",
        "borrow: borrow limit exceeded",
        "repay: no debt",
        "liquidate: cannot liquidate yourself",
        "liquidate: position is healthy",
        "set_price: only the admin",
        "withdraw_reserves: amount exceeds reserves",
    ] {
        assert!(reverts.contains(expected), "no run met {expected:?}");
    }
}

// ---------------------------------------------------------------------------
// The ABI block

/// The names declared in the ABI block are the methods `dispatch` answers and the events the contract emits, and the
/// structs list the fields of the Rust structs in their order.
#[test]
fn the_abi_block_matches_the_contract() {
    let source = include_str!("../src/lib.rs");
    let block: Vec<&str> = source
        .lines()
        .filter_map(|line| line.strip_prefix("//!"))
        .map(str::trim)
        .skip_while(|line| *line != "```abi")
        .skip(1)
        .take_while(|line| *line != "```")
        .collect();
    let name = |rest: &str| rest.split(['(', ' ']).next().unwrap().to_string();
    let (mut methods, mut events, mut structs) = (BTreeSet::new(), BTreeSet::new(), Vec::new());
    for line in &block {
        let declaration = line.strip_prefix("payable ").unwrap_or(line);
        if declaration.starts_with("init(") {
            methods.insert("init".to_string());
        } else if let Some(rest) = declaration
            .strip_prefix("call ")
            .or_else(|| declaration.strip_prefix("view "))
        {
            methods.insert(name(rest));
        } else if let Some(rest) = declaration.strip_prefix("event ") {
            events.insert(name(rest));
        } else if let Some(rest) = declaration.strip_prefix("struct ") {
            let (struct_name, fields) = rest.split_once('{').unwrap();
            let fields: Vec<String> = fields
                .trim_end_matches('}')
                .split(',')
                .map(|field| field.split(':').next().unwrap().trim().to_string())
                .collect();
            structs.push((struct_name.trim().to_string(), fields));
        } else {
            panic!("unexpected line in the ABI block: {line}");
        }
    }
    let dispatched: BTreeSet<String> = source
        .lines()
        .filter_map(|line| line.trim().strip_prefix('"')?.split_once("\" =>"))
        .map(|(method, _)| method.to_string())
        .collect();
    assert_eq!(methods.len(), 13);
    assert_eq!(methods, dispatched);
    let emitted: BTreeSet<String> = source
        .split("sdk::emit(&(\"")
        .skip(1)
        .map(|rest| rest.split('"').next().unwrap().to_string())
        .collect();
    assert_eq!(events.len(), 11);
    assert_eq!(events, emitted);
    assert_eq!(structs.len(), 3);
    for (struct_name, fields) in structs {
        let body = source
            .split_once(&format!("pub struct {struct_name} {{"))
            .unwrap()
            .1
            .split_once("\n}")
            .unwrap()
            .0;
        let rust_fields: Vec<String> = body
            .lines()
            .filter_map(|line| line.trim().strip_prefix("pub ")?.split_once(':'))
            .map(|(field, _)| field.to_string())
            .collect();
        assert_eq!(fields, rust_fields, "struct {struct_name}");
    }
    // dispatch answers every declared name, and only those
    mock::reset();
    for method in &methods {
        let result = mock::with_context(ALICE, [0x77; 32], || lending::dispatch(method, &[]));
        assert_ne!(result, Err(b"unknown method".to_vec()), "{method}");
    }
    let result = mock::with_context(ALICE, [0x77; 32], || lending::dispatch("liquidity", &[]));
    assert_eq!(result, Err(b"unknown method".to_vec()));
}
