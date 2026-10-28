//! Tests of the vesting contract on the SDK's in-memory mock host: the exact arithmetic of the vesting curve, every
//! method with its reverts and access control, the funds it takes and refuses, its clock when block timestamps run
//! backwards, and the conservation of the assets it holds against its own accounting.
#![cfg(not(target_arch = "wasm32"))]
#![allow(
    missing_docs,
    clippy::unwrap_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::cast_possible_truncation
)]

use std::collections::{BTreeMap, BTreeSet};

use ksana_contract_sdk::mock::{self, Failure};
use ksana_contract_sdk::{decode, encode, Decode, Funds, KSA_ASSET_ID};
use vesting::math::mul_div;
use vesting::{Schedule, MAX_DURATION, MAX_PAGE};

const ALICE: [u8; 32] = [0xa1; 32];
const BOB: [u8; 32] = [0xb0; 32];
const CAROL: [u8; 32] = [0xc4; 32];
/// Calls the views; holds nothing and takes part in nothing.
const READER: [u8; 32] = [0xee; 32];
/// A second built-in asset, as a contract issues one.
const GOLD: [u8; 32] = [0x60; 32];
const SUPPLY: u128 = 1_000_000_000_000;
/// The block timestamp at deployment.
const T0: u64 = 1_800_000_000;

/// The arguments of `create`: `(beneficiary, start, cliff, duration, revocable)`.
type Terms = ([u8; 32], u64, u64, u64, bool);

// ---------------------------------------------------------------------------
// The arithmetic

/// A small deterministic generator: a 64-bit LCG, of which only the high bits are used.
struct Rng(u64);

impl Rng {
    fn bits(&mut self) -> u64 {
        let mut half = || {
            self.0 = self
                .0
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            self.0 >> 32
        };
        (half() << 32) | half()
    }

    /// A value below `bound`, which is not 0.
    fn below(&mut self, bound: u64) -> u64 {
        self.bits() % bound
    }

    /// A value of any magnitude: random bits shifted right by a random amount.
    fn any_u64(&mut self) -> u64 {
        let shift = self.below(64);
        self.bits() >> shift
    }

    fn any_u128(&mut self) -> u128 {
        let shift = self.below(128);
        ((u128::from(self.bits()) << 64) | u128::from(self.bits())) >> shift
    }

    fn pick<T: Copy>(&mut self, items: &[T]) -> T {
        items[self.below(items.len() as u64) as usize]
    }
}

/// `floor(value · numerator / denominator)` by schoolbook multiplication and long division on 32-bit limbs, another
/// method than the contract's; `None` when the denominator is 0 or the quotient does not fit in u128.
fn reference(value: u128, numerator: u64, denominator: u64) -> Option<u128> {
    const MASK: u64 = 0xffff_ffff;
    if denominator == 0 {
        return None;
    }
    let a = [0, 32, 64, 96].map(|shift| ((value >> shift) as u64) & MASK);
    let b = [numerator & MASK, numerator >> 32];
    // The product's limbs, little-endian, each below 2^32
    let mut product = [0u64; 6];
    for (i, x) in a.into_iter().enumerate() {
        let mut carry = 0;
        for (j, y) in b.into_iter().enumerate() {
            let t = product[i + j] + x * y + carry;
            product[i + j] = t & MASK;
            carry = t >> 32;
        }
        product[i + b.len()] = carry;
    }
    let divisor = u128::from(denominator);
    let (mut quotient, mut remainder) = (0u128, 0u128);
    for limb in product.into_iter().rev() {
        let current = (remainder << 32) | u128::from(limb);
        quotient = quotient
            .checked_mul(1 << 32)?
            .checked_add(current / divisor)?;
        remainder = current % divisor;
    }
    Some(quotient)
}

/// The vesting rule, written out with the reference arithmetic.
fn expected_vested(s: &Schedule, at: u64) -> u128 {
    if s.revoked_at.is_some() {
        s.total
    } else if at < s.start + s.cliff {
        0
    } else if at - s.start >= s.duration {
        s.total
    } else {
        reference(s.total, at - s.start, s.duration).unwrap()
    }
}

fn schedule_of(total: u128, start: u64, cliff: u64, duration: u64) -> Schedule {
    Schedule {
        id: 1,
        creator: ALICE,
        beneficiary: BOB,
        asset: KSA_ASSET_ID,
        total,
        released: 0,
        start,
        cliff,
        duration,
        revocable: true,
        revoked_at: None,
    }
}

#[test]
fn mul_div_is_exact() {
    assert_eq!(mul_div(10, 1, 3), Some(3));
    assert_eq!(mul_div(10, 2, 3), Some(6));
    assert_eq!(mul_div(10, 3, 3), Some(10));
    assert_eq!(mul_div(0, 7, 9), Some(0));
    assert_eq!(mul_div(u128::MAX, 0, 9), Some(0));
    assert_eq!(mul_div(u128::MAX, 1, 2), Some(u128::MAX / 2));
    assert_eq!(mul_div(u128::MAX, u64::MAX, u64::MAX), Some(u128::MAX));
    assert_eq!(
        mul_div(u128::MAX, MAX_DURATION, MAX_DURATION),
        Some(u128::MAX)
    );
    assert_eq!(mul_div(u128::MAX / 2, 2, 1), Some(u128::MAX - 1));
    // Results that do not fit, and a zero denominator
    assert_eq!(mul_div(u128::MAX, 2, 1), None);
    assert_eq!(mul_div(u128::MAX / 2 + 1, 2, 1), None);
    assert_eq!(mul_div(5, 3, 0), None);
    assert_eq!(reference(5, 3, 0), None);
    assert_eq!(reference(u128::MAX, 2, 1), None);
    // The edges of every duration: a result always, and the reference's
    for d in [1, 2, 3, 7, 86_400, MAX_DURATION - 1, MAX_DURATION, u64::MAX] {
        let wide = u128::from(d);
        for total in [0, 1, 2, wide - 1, wide, wide + 1, u128::MAX - 1, u128::MAX] {
            for elapsed in [0, 1, d / 2, d - 1, d] {
                let got = mul_div(total, elapsed, d);
                assert!(got.is_some(), "{total} · {elapsed} / {d}");
                assert_eq!(
                    got,
                    reference(total, elapsed, d),
                    "{total} · {elapsed} / {d}"
                );
            }
        }
    }
    // Random values of every magnitude, with numerators above the denominator too
    let mut rng = Rng(0x5eed);
    for _ in 0..100_000 {
        let value = rng.any_u128();
        let denominator = if rng.below(2) == 0 {
            rng.below(MAX_DURATION) + 1
        } else {
            rng.any_u64().max(1)
        };
        let numerator = match rng.below(4) {
            0 => rng.any_u64(),
            1 => denominator,
            _ => rng.below(denominator),
        };
        let expected = reference(value, numerator, denominator);
        assert_eq!(
            mul_div(value, numerator, denominator),
            expected,
            "{value} · {numerator} / {denominator}"
        );
        if numerator <= denominator {
            assert!(expected.is_some());
        }
    }
}

#[test]
fn vested_at_follows_the_rule() {
    let mut rng = Rng(42);
    for _ in 0..20_000 {
        let duration = rng.below(MAX_DURATION) + 1;
        let cliff = rng.below(duration + 1);
        let start = rng.below(1 << 40);
        let mut s = schedule_of(rng.any_u128(), start, cliff, duration);
        let (cliff_at, end) = (start + cliff, start + duration);
        let mut times = [
            0,
            start.saturating_sub(1),
            start,
            cliff_at.saturating_sub(1),
            cliff_at,
            cliff_at + 1,
            end - 1,
            end,
            end + 1,
            start + rng.below(duration),
            rng.bits(),
            u64::MAX,
        ];
        times.sort_unstable();
        // Exact, never above the total, and never decreasing with time
        let mut previous = 0;
        for at in times {
            let vested = s.vested_at(at).unwrap();
            assert_eq!(vested, expected_vested(&s, at), "{s:?} at {at}");
            assert!(previous <= vested && vested <= s.total, "{s:?} at {at}");
            assert_eq!(s.releasable(at), Some(vested));
            previous = vested;
        }
        // Revoked: the reduced total, whatever the time
        s.total /= 3;
        s.revoked_at = Some(start);
        for at in [0, start, end, u64::MAX] {
            assert_eq!(s.vested_at(at), Some(s.total));
        }
        // What has been released is not releasable again
        s.released = s.total;
        assert_eq!(s.releasable(end), Some(0));
    }
}

// ---------------------------------------------------------------------------
// The contract

fn setup() -> [u8; 32] {
    mock::reset();
    mock::register_asset(KSA_ASSET_ID, "KSA", 8, [0; 32]);
    mock::register_asset(GOLD, "GOLD", 0, [0x99; 32]);
    for who in [ALICE, BOB, CAROL] {
        mock::mint(KSA_ASSET_ID, who, SUPPLY).unwrap();
        mock::mint(GOLD, who, SUPPLY).unwrap();
    }
    mock::set_block_timestamp(T0);
    mock::deploy(ALICE, 0, b"vesting", vesting::dispatch, &[]).unwrap()
}

fn ksa(amount: u128) -> Funds {
    Funds {
        asset: KSA_ASSET_ID,
        amount,
    }
}

fn gold(amount: u128) -> Funds {
    Funds {
        asset: GOLD,
        amount,
    }
}

fn create(v: [u8; 32], who: [u8; 32], funds: &[Funds], terms: Terms) -> Result<u64, Failure> {
    mock::call_with_funds(who, v, "create", &encode(&terms), funds).map(|out| decode(&out).unwrap())
}

fn release(v: [u8; 32], who: [u8; 32], id: u64) -> Result<u128, Failure> {
    mock::call(who, v, "release", &encode(&id)).map(|out| decode(&out).unwrap())
}

fn revoke(v: [u8; 32], who: [u8; 32], id: u64) -> Result<u128, Failure> {
    mock::call(who, v, "revoke", &encode(&id)).map(|out| decode(&out).unwrap())
}

/// Calls a view and decodes its answer.
fn view<T: Decode>(v: [u8; 32], method: &str, args: &[u8]) -> T {
    decode(&mock::call(READER, v, method, args).unwrap()).unwrap()
}

fn schedule(v: [u8; 32], id: u64) -> Option<Schedule> {
    view(v, "schedule", &encode(&id))
}

fn releasable(v: [u8; 32], id: u64) -> u128 {
    view(v, "releasable", &encode(&id))
}

fn vested(v: [u8; 32], id: u64, at: u64) -> u128 {
    view(v, "vested", &encode(&(id, at)))
}

fn count(v: [u8; 32]) -> u64 {
    view(v, "count", &[])
}

fn by_beneficiary(v: [u8; 32], account: [u8; 32], start: u64, limit: u32) -> Vec<Schedule> {
    view(v, "by_beneficiary", &encode(&(account, start, limit)))
}

fn by_creator(v: [u8; 32], account: [u8; 32], start: u64, limit: u32) -> Vec<Schedule> {
    view(v, "by_creator", &encode(&(account, start, limit)))
}

fn ids(list: &[Schedule]) -> Vec<u64> {
    list.iter().map(|s| s.id).collect()
}

fn revert_reason<T: std::fmt::Debug>(result: Result<T, Failure>) -> Vec<u8> {
    match result {
        Err(Failure::Revert { reason, .. }) => reason,
        other => panic!("expected a revert, got {other:?}"),
    }
}

fn last_event() -> Vec<u8> {
    mock::events().last().unwrap().data.clone()
}

/// Of each asset, the contract holds exactly what its schedules still owe: `total − released` summed over the
/// schedules in that asset; and no schedule has released more than its total.
fn assert_conserved(v: [u8; 32], assets: &[[u8; 32]]) {
    let mut owed: BTreeMap<[u8; 32], u128> = assets.iter().map(|asset| (*asset, 0)).collect();
    for id in 1..=count(v) {
        let s = schedule(v, id).unwrap();
        assert!(s.released <= s.total, "schedule {id}");
        let sum = owed.entry(s.asset).or_default();
        *sum = sum.checked_add(s.total - s.released).unwrap();
    }
    for (asset, sum) in owed {
        assert_eq!(mock::balance(&asset, &v), sum);
    }
}

// ---------------------------------------------------------------------------
// The ABI block

const SOURCE: &str = include_str!("../src/lib.rs");

/// The lines of the `abi` block in the module documentation.
fn abi_lines() -> Vec<&'static str> {
    SOURCE
        .lines()
        .filter_map(|line| line.strip_prefix("//!"))
        .map(str::trim)
        .skip_while(|line| *line != "```abi")
        .skip(1)
        .take_while(|line| *line != "```")
        .collect()
}

/// The method names the `abi` block declares: its `init`, `call` and `view` lines.
fn declared_methods() -> BTreeSet<&'static str> {
    let mut names = BTreeSet::new();
    for line in abi_lines() {
        let line = line.strip_prefix("payable ").unwrap_or(line);
        let signature = if line.starts_with("init(") {
            line
        } else if let Some(rest) = line
            .strip_prefix("call ")
            .or_else(|| line.strip_prefix("view "))
        {
            rest
        } else {
            continue;
        };
        names.insert(signature.split('(').next().unwrap());
    }
    names
}

/// The event names the `abi` block declares.
fn declared_events() -> BTreeSet<&'static str> {
    abi_lines()
        .into_iter()
        .filter_map(|line| line.strip_prefix("event ")?.split('(').next())
        .collect()
}

/// The method names of the `"name" =>` arms of `dispatch`.
fn dispatched_methods() -> BTreeSet<&'static str> {
    let body = SOURCE.split_once("pub fn dispatch(").unwrap().1;
    let body = body
        .split_once("_ => sdk::revert(\"unknown method\")")
        .unwrap()
        .0;
    body.lines()
        .filter_map(|line| line.trim().strip_prefix('"')?.split_once("\" =>"))
        .map(|(name, _)| name)
        .collect()
}

/// The event names the contract emits: the first field of each `sdk::emit(&(...))`.
fn emitted_events() -> BTreeSet<&'static str> {
    SOURCE
        .split("sdk::emit(&(")
        .skip(1)
        .map(|rest| rest.trim_start().strip_prefix('"').unwrap())
        .map(|rest| rest.split_once('"').unwrap().0)
        .collect()
}

/// The fields of `struct Schedule` in the `abi` block, in order.
fn declared_fields() -> Vec<&'static str> {
    let line = abi_lines()
        .into_iter()
        .find(|line| line.starts_with("struct Schedule {"))
        .unwrap();
    let fields = line.split_once('{').unwrap().1.rsplit_once('}').unwrap().0;
    fields
        .split(',')
        .map(|field| field.split_once(':').unwrap().0.trim())
        .collect()
}

/// The fields of the Rust `Schedule`, in order (the Borsh wire order).
fn struct_fields() -> Vec<&'static str> {
    let body = SOURCE.split_once("pub struct Schedule {").unwrap().1;
    let body = body.split_once("\n}").unwrap().0;
    body.lines()
        .filter_map(|line| line.trim().strip_prefix("pub ")?.split_once(':'))
        .map(|(name, _)| name)
        .collect()
}

#[test]
fn abi_block_matches_the_contract() {
    let methods = declared_methods();
    assert_eq!(methods.len(), 10, "{methods:?}");
    assert_eq!(methods, dispatched_methods());
    assert_eq!(declared_events(), emitted_events());
    assert_eq!(declared_events().len(), 3);
    assert_eq!(declared_fields(), struct_fields());
    assert_eq!(declared_fields().len(), 11);
    // The declared types on the wire: u64, three 32-byte accounts or assets, two u128, three u64, a bool, an Option
    let s = schedule_of(1, 2, 3, 4);
    assert_eq!(encode(&s).len(), 8 + 3 * 32 + 2 * 16 + 3 * 8 + 1 + 1);
    let revoked = Schedule {
        revoked_at: Some(5),
        ..s
    };
    assert_eq!(encode(&revoked).len(), 8 + 3 * 32 + 2 * 16 + 3 * 8 + 1 + 9);
    // Every declared method is dispatched (empty arguments are malformed for most, which is not "unknown method")
    let v = setup();
    for &name in &methods {
        let answer = mock::with_context(READER, v, || vesting::dispatch(name, &[]));
        assert_ne!(answer, Err(b"unknown method".to_vec()), "{name}");
    }
    assert_eq!(
        mock::with_context(READER, v, || vesting::dispatch("withdraw", &[])),
        Err(b"unknown method".to_vec())
    );
}

// ---------------------------------------------------------------------------
// Methods

#[test]
fn init_runs_once_and_takes_nothing() {
    let v = setup();
    assert_eq!(count(v), 0);
    assert_eq!(schedule(v, 1), None);
    // The VM refuses a later `init`, and so does the contract
    assert!(matches!(
        mock::call(ALICE, v, "init", &[]),
        Err(Failure::Trap(_))
    ));
    assert_eq!(
        mock::with_context(ALICE, v, || vesting::dispatch("init", &[])),
        Err(b"already initialized".to_vec())
    );
    // Arguments or funds at deployment
    assert_eq!(
        revert_reason(mock::deploy(ALICE, 1, b"vesting", vesting::dispatch, &[0])),
        b"malformed arguments"
    );
    assert_eq!(
        revert_reason(mock::deploy_with_funds(
            ALICE,
            2,
            b"vesting",
            vesting::dispatch,
            &[],
            &[ksa(5)]
        )),
        b"unexpected attached funds"
    );
    assert_eq!(mock::balance(&KSA_ASSET_ID, &ALICE), SUPPLY);
}

#[test]
fn every_method_needs_init() {
    mock::reset();
    mock::register_asset(KSA_ASSET_ID, "KSA", 8, [0; 32]);
    mock::mint(KSA_ASSET_ID, ALICE, SUPPLY).unwrap();
    // The contract's code at an address where `init` never ran
    let v = [0x5e; 32];
    mock::register(v, vesting::dispatch);
    for (method, args, funds) in [
        (
            "create",
            encode(&(BOB, 0u64, 0u64, 10u64, true)),
            vec![ksa(5)],
        ),
        ("release", encode(&1u64), vec![]),
        ("revoke", encode(&1u64), vec![]),
        ("schedule", encode(&1u64), vec![]),
        ("releasable", encode(&1u64), vec![]),
        ("vested", encode(&(1u64, T0)), vec![]),
        ("count", vec![], vec![]),
        ("by_beneficiary", encode(&(BOB, 0u64, 10u32)), vec![]),
        ("by_creator", encode(&(ALICE, 0u64, 10u32)), vec![]),
    ] {
        assert_eq!(
            revert_reason(mock::call_with_funds(ALICE, v, method, &args, &funds)),
            b"not initialized",
            "{method}"
        );
    }
    assert_eq!(mock::balance(&KSA_ASSET_ID, &ALICE), SUPPLY);
}

#[test]
fn create_records_the_schedule_and_its_lists() {
    let v = setup();
    let id = create(v, ALICE, &[ksa(1_000)], (BOB, T0 + 100, 50, 1_000, true)).unwrap();
    assert_eq!(id, 1);
    assert_eq!(count(v), 1);
    let first = Schedule {
        id: 1,
        creator: ALICE,
        beneficiary: BOB,
        asset: KSA_ASSET_ID,
        total: 1_000,
        released: 0,
        start: T0 + 100,
        cliff: 50,
        duration: 1_000,
        revocable: true,
        revoked_at: None,
    };
    assert_eq!(schedule(v, 1), Some(first.clone()));
    assert_eq!(mock::balance(&KSA_ASSET_ID, &v), 1_000);
    assert_eq!(mock::balance(&KSA_ASSET_ID, &ALICE), SUPPLY - 1_000);
    let event = mock::events().last().unwrap().clone();
    assert_eq!(event.contract, v);
    assert_eq!(
        event.data,
        encode(&("create", 1u64, ALICE, BOB, KSA_ASSET_ID, 1_000u128))
    );
    // Another asset and another creator; a creator may also be the beneficiary
    assert_eq!(create(v, CAROL, &[gold(7)], (BOB, 0, 0, 10, false)), Ok(2));
    assert_eq!(create(v, BOB, &[ksa(3)], (BOB, 0, 0, 10, true)), Ok(3));
    assert_eq!(count(v), 3);
    assert_eq!(schedule(v, 0), None);
    assert_eq!(schedule(v, 4), None);
    assert_eq!(schedule(v, 2).unwrap().asset, GOLD);
    assert_eq!(mock::balance(&GOLD, &v), 7);
    assert_eq!(ids(&by_beneficiary(v, BOB, 0, 10)), [1, 2, 3]);
    assert_eq!(by_creator(v, ALICE, 0, 10), [first]);
    assert_eq!(ids(&by_creator(v, CAROL, 0, 10)), [2]);
    assert_eq!(ids(&by_creator(v, BOB, 0, 10)), [3]);
    assert!(by_beneficiary(v, ALICE, 0, 10).is_empty());
    assert_conserved(v, &[KSA_ASSET_ID, GOLD]);
}

#[test]
fn start_zero_means_now() {
    let v = setup();
    // Deployment kept T0 as the clock: a block with an earlier timestamp still starts at T0
    mock::set_block_timestamp(T0 - 100);
    let id = create(v, ALICE, &[ksa(100)], (BOB, 0, 0, 100, false)).unwrap();
    assert_eq!(schedule(v, id).unwrap().start, T0);
    mock::set_block_timestamp(T0 + 500);
    let id = create(v, ALICE, &[ksa(100)], (BOB, 0, 0, 100, false)).unwrap();
    assert_eq!(schedule(v, id).unwrap().start, T0 + 500);
    assert_eq!(releasable(v, id), 0);
    mock::set_block_timestamp(T0 + 400);
    let id = create(v, ALICE, &[ksa(100)], (BOB, 0, 0, 100, false)).unwrap();
    assert_eq!(schedule(v, id).unwrap().start, T0 + 500);
    // A start of 1 is a start in 1970: all of it has vested already
    let id = create(v, ALICE, &[ksa(100)], (BOB, 1, 0, 100, false)).unwrap();
    assert_eq!(schedule(v, id).unwrap().start, 1);
    assert_eq!(releasable(v, id), 100);
    assert_eq!(release(v, BOB, id), Ok(100));
    assert_conserved(v, &[KSA_ASSET_ID, GOLD]);
}

#[test]
fn create_checks_its_terms() {
    let v = setup();
    let terms: Terms = (BOB, 0, 10, 100, true);
    let far = T0 + MAX_DURATION;
    let duration = "duration must be 1 to 3153600000 seconds";
    let start = "start must be at most 100 years from now";
    for (funds, args, reason) in [
        (vec![], encode(&terms), "attach one asset to vest"),
        (
            vec![ksa(10), gold(10)],
            encode(&terms),
            "attach one asset to vest",
        ),
        (
            vec![ksa(10)],
            encode(&([0u8; 32], 0u64, 10u64, 100u64, true)),
            "beneficiary must not be zero",
        ),
        (
            vec![ksa(10)],
            encode(&(v, 0u64, 10u64, 100u64, true)),
            "beneficiary must not be this contract",
        ),
        (
            vec![ksa(10)],
            encode(&(BOB, 0u64, 0u64, 0u64, true)),
            duration,
        ),
        (
            vec![ksa(10)],
            encode(&(BOB, 0u64, 0u64, MAX_DURATION + 1, true)),
            duration,
        ),
        (
            vec![ksa(10)],
            encode(&(BOB, 0u64, 0u64, u64::MAX, true)),
            duration,
        ),
        (
            vec![ksa(10)],
            encode(&(BOB, 0u64, 101u64, 100u64, true)),
            "cliff must not exceed duration",
        ),
        (
            vec![ksa(10)],
            encode(&(BOB, far + 1, 0u64, 100u64, true)),
            start,
        ),
        // A start in milliseconds, and the one whose end would overflow
        (
            vec![ksa(10)],
            encode(&(BOB, T0 * 1_000, 0u64, 100u64, true)),
            start,
        ),
        (
            vec![ksa(10)],
            encode(&(BOB, u64::MAX, 0u64, 1u64, true)),
            start,
        ),
        // Malformed: a missing field, a bool that is not 0 or 1, a trailing byte
        (
            vec![ksa(10)],
            encode(&(BOB, 0u64, 10u64, 100u64)),
            "malformed arguments",
        ),
        (
            vec![ksa(10)],
            encode(&(BOB, 0u64, 10u64, 100u64, 2u8)),
            "malformed arguments",
        ),
        (
            vec![ksa(10)],
            encode(&(BOB, 0u64, 10u64, 100u64, true, 0u8)),
            "malformed arguments",
        ),
    ] {
        assert_eq!(
            revert_reason(mock::call_with_funds(ALICE, v, "create", &args, &funds)),
            reason.as_bytes(),
            "{reason}"
        );
    }
    assert_eq!(count(v), 0);
    assert_eq!(mock::balance(&KSA_ASSET_ID, &ALICE), SUPPLY);
    assert_eq!(mock::balance(&GOLD, &ALICE), SUPPLY);
    // The edges themselves are taken
    for terms in [
        // a cliff as long as the longest duration
        (BOB, 0, MAX_DURATION, MAX_DURATION, false),
        // the latest start, with the longest duration
        (BOB, far, 0, MAX_DURATION, true),
        // one second
        (BOB, 0, 0, 1, false),
        // for the creator herself
        (ALICE, 0, 0, 1, false),
    ] {
        create(v, ALICE, &[ksa(1)], terms).unwrap();
    }
    assert_eq!(count(v), 4);
    assert_conserved(v, &[KSA_ASSET_ID, GOLD]);
}

#[test]
fn vesting_follows_the_cliff_and_the_line() {
    let v = setup();
    // 1 000 000 over 1 000 s from a start in the future, nothing of it before a cliff of 250 s
    let start = T0 + 1_000;
    let id = create(v, ALICE, &[ksa(1_000_000)], (BOB, start, 250, 1_000, false)).unwrap();
    for (at, expected) in [
        (0, 0),
        (T0, 0),
        (start - 1, 0),
        (start, 0),
        (start + 249, 0),
        (start + 250, 250_000),
        (start + 251, 251_000),
        (start + 999, 999_000),
        (start + 1_000, 1_000_000),
        (start + 1_001, 1_000_000),
        (u64::MAX, 1_000_000),
    ] {
        assert_eq!(vested(v, id, at), expected, "at {at}");
    }
    // Before the start and until the cliff, nothing to release
    assert_eq!(releasable(v, id), 0);
    mock::set_block_timestamp(start + 249);
    assert_eq!(releasable(v, id), 0);
    assert_eq!(revert_reason(release(v, BOB, id)), b"nothing to release");
    // At the cliff, the linear amount since the start; a stranger's release pays the beneficiary
    mock::set_block_timestamp(start + 250);
    assert_eq!(releasable(v, id), 250_000);
    assert_eq!(release(v, CAROL, id), Ok(250_000));
    assert_eq!(mock::balance(&KSA_ASSET_ID, &BOB), SUPPLY + 250_000);
    assert_eq!(mock::balance(&KSA_ASSET_ID, &CAROL), SUPPLY);
    assert_eq!(last_event(), encode(&("release", id, BOB, 250_000u128)));
    assert_eq!(revert_reason(release(v, BOB, id)), b"nothing to release");
    mock::set_block_timestamp(start + 600);
    assert_eq!(releasable(v, id), 350_000);
    assert_eq!(release(v, BOB, id), Ok(350_000));
    assert_conserved(v, &[KSA_ASSET_ID, GOLD]);
    // At the end everything, and past it nothing more
    mock::set_block_timestamp(start + 1_000);
    assert_eq!(release(v, ALICE, id), Ok(400_000));
    mock::set_block_timestamp(start + 100_000);
    assert_eq!(releasable(v, id), 0);
    assert_eq!(revert_reason(release(v, BOB, id)), b"nothing to release");
    let s = schedule(v, id).unwrap();
    assert_eq!((s.total, s.released), (1_000_000, 1_000_000));
    assert_eq!(mock::balance(&KSA_ASSET_ID, &BOB), SUPPLY + 1_000_000);
    assert_eq!(mock::balance(&KSA_ASSET_ID, &v), 0);
    assert_conserved(v, &[KSA_ASSET_ID, GOLD]);
}

#[test]
fn rounding_goes_down_and_the_parts_add_up() {
    let v = setup();
    // 10 over 3 s: 3, then 6, then all 10
    let id = create(v, ALICE, &[ksa(10)], (BOB, T0, 0, 3, false)).unwrap();
    let mut paid = Vec::new();
    for second in 1..=3 {
        mock::set_block_timestamp(T0 + second);
        paid.push(release(v, BOB, id).unwrap());
    }
    assert_eq!(paid, [3, 3, 4]);
    // 1 over 2 s: nothing until the end
    let id = create(v, ALICE, &[ksa(1)], (BOB, T0, 0, 2, false)).unwrap();
    assert_eq!(
        [
            vested(v, id, T0),
            vested(v, id, T0 + 1),
            vested(v, id, T0 + 2)
        ],
        [0, 0, 1]
    );
    // A cliff as long as the duration: nothing, then everything
    let id = create(v, ALICE, &[gold(1_000)], (BOB, T0, 100, 100, false)).unwrap();
    assert_eq!(
        [vested(v, id, T0 + 99), vested(v, id, T0 + 100)],
        [0, 1_000]
    );
    // No cliff: linear from the start
    let id = create(v, ALICE, &[gold(1_000)], (BOB, T0, 0, 100, false)).unwrap();
    assert_eq!(
        [
            vested(v, id, T0),
            vested(v, id, T0 + 1),
            vested(v, id, T0 + 99)
        ],
        [0, 10, 990]
    );
    assert_conserved(v, &[KSA_ASSET_ID, GOLD]);
}

#[test]
fn an_earlier_timestamp_counts_as_no_time_passed() {
    let v = setup();
    let id = create(v, ALICE, &[ksa(1_000)], (BOB, T0, 0, 1_000, true)).unwrap();
    mock::set_block_timestamp(T0 + 600);
    assert_eq!(release(v, BOB, id), Ok(600));
    // A block with an earlier timestamp than the release's: no time has passed since, and nothing fails
    mock::set_block_timestamp(T0 + 500);
    assert_eq!(releasable(v, id), 0);
    assert_eq!(revert_reason(release(v, BOB, id)), b"nothing to release");
    assert_eq!(schedule(v, id).unwrap().released, 600);
    // `vested(id, at)` is the schedule's curve at any time, whatever the clock
    assert_eq!(vested(v, id, T0 + 500), 500);
    // Time goes on from the latest timestamp seen
    mock::set_block_timestamp(T0 + 700);
    assert_eq!(release(v, BOB, id), Ok(100));
    // A revoke in a block with an earlier timestamp counts what had vested by the latest one seen
    mock::set_block_timestamp(T0 + 650);
    assert_eq!(revoke(v, ALICE, id), Ok(300));
    let s = schedule(v, id).unwrap();
    assert_eq!(
        (s.total, s.released, s.revoked_at),
        (700, 700, Some(T0 + 700))
    );
    assert_eq!(mock::balance(&KSA_ASSET_ID, &BOB), SUPPLY + 700);
    assert_eq!(mock::balance(&KSA_ASSET_ID, &ALICE), SUPPLY - 700);
    assert_conserved(v, &[KSA_ASSET_ID, GOLD]);

    // Nor can a creator take back what had vested when the schedule was created: half of it here
    mock::set_block_timestamp(T0 + 2_000);
    let id = create(v, ALICE, &[ksa(1_000)], (BOB, T0, 0, 4_000, true)).unwrap();
    mock::set_block_timestamp(T0 + 1_000);
    assert_eq!(releasable(v, id), 500);
    assert_eq!(revoke(v, ALICE, id), Ok(500));
    assert_eq!(release(v, CAROL, id), Ok(500));
    assert_eq!(mock::balance(&KSA_ASSET_ID, &BOB), SUPPLY + 1_200);
    assert_conserved(v, &[KSA_ASSET_ID, GOLD]);
}

#[test]
fn revoke_before_the_cliff_refunds_everything() {
    let v = setup();
    let id = create(v, ALICE, &[ksa(1_000)], (BOB, T0, 100, 1_000, true)).unwrap();
    mock::set_block_timestamp(T0 + 99);
    assert_eq!(revoke(v, ALICE, id), Ok(1_000));
    assert_eq!(last_event(), encode(&("revoke", id, 1_000u128)));
    let s = schedule(v, id).unwrap();
    assert_eq!((s.total, s.released, s.revoked_at), (0, 0, Some(T0 + 99)));
    assert_eq!(mock::balance(&KSA_ASSET_ID, &ALICE), SUPPLY);
    assert_eq!(mock::balance(&KSA_ASSET_ID, &v), 0);
    // Nothing ever vests for the beneficiary
    mock::set_block_timestamp(T0 + 10_000);
    assert_eq!(releasable(v, id), 0);
    assert_eq!(vested(v, id, u64::MAX), 0);
    assert_eq!(revert_reason(release(v, BOB, id)), b"nothing to release");
    assert_conserved(v, &[KSA_ASSET_ID, GOLD]);
}

#[test]
fn revoke_in_the_middle_keeps_what_has_vested() {
    let v = setup();
    let id = create(v, ALICE, &[gold(1_000)], (BOB, T0, 100, 1_000, true)).unwrap();
    mock::set_block_timestamp(T0 + 200);
    assert_eq!(release(v, BOB, id), Ok(200));
    mock::set_block_timestamp(T0 + 450);
    assert_eq!(revoke(v, ALICE, id), Ok(550));
    assert_eq!(last_event(), encode(&("revoke", id, 550u128)));
    let s = schedule(v, id).unwrap();
    assert_eq!(
        (s.total, s.released, s.revoked_at),
        (450, 200, Some(T0 + 450))
    );
    assert_eq!(mock::balance(&GOLD, &ALICE), SUPPLY - 450);
    assert_conserved(v, &[KSA_ASSET_ID, GOLD]);
    // A revoked schedule has vested its reduced total: the rest of it stays releasable, and nothing more
    for at in [0, T0, T0 + 450, u64::MAX] {
        assert_eq!(vested(v, id, at), 450);
    }
    assert_eq!(releasable(v, id), 250);
    mock::set_block_timestamp(T0 + 5_000);
    assert_eq!(release(v, CAROL, id), Ok(250));
    assert_eq!(revert_reason(release(v, BOB, id)), b"nothing to release");
    assert_eq!(mock::balance(&GOLD, &BOB), SUPPLY + 450);
    assert_eq!(mock::balance(&GOLD, &v), 0);
    assert_conserved(v, &[KSA_ASSET_ID, GOLD]);
}

#[test]
fn revoke_after_the_end_refunds_nothing() {
    let v = setup();
    let id = create(v, ALICE, &[ksa(1_000)], (BOB, T0, 0, 1_000, true)).unwrap();
    mock::set_block_timestamp(T0 + 1_000);
    assert_eq!(revoke(v, ALICE, id), Ok(0));
    assert_eq!(last_event(), encode(&("revoke", id, 0u128)));
    assert_eq!(mock::balance(&KSA_ASSET_ID, &ALICE), SUPPLY - 1_000);
    let s = schedule(v, id).unwrap();
    assert_eq!(
        (s.total, s.released, s.revoked_at),
        (1_000, 0, Some(T0 + 1_000))
    );
    assert_eq!(revert_reason(revoke(v, ALICE, id)), b"already revoked");
    assert_eq!(release(v, BOB, id), Ok(1_000));
    assert_conserved(v, &[KSA_ASSET_ID, GOLD]);
}

#[test]
fn only_the_creator_revokes_a_revocable_schedule_once() {
    let v = setup();
    let fixed = create(v, ALICE, &[ksa(1_000)], (BOB, T0, 0, 1_000, false)).unwrap();
    let id = create(v, ALICE, &[ksa(1_000)], (BOB, T0, 0, 1_000, true)).unwrap();
    mock::set_block_timestamp(T0 + 300);
    assert_eq!(
        revert_reason(revoke(v, ALICE, fixed)),
        b"schedule is not revocable"
    );
    // Not the beneficiary, nor anyone else
    for who in [BOB, CAROL, READER] {
        assert_eq!(revert_reason(revoke(v, who, id)), b"only the creator");
        assert_eq!(revert_reason(revoke(v, who, fixed)), b"only the creator");
    }
    assert_eq!(revoke(v, ALICE, id), Ok(700));
    assert_eq!(revert_reason(revoke(v, ALICE, id)), b"already revoked");
    assert_eq!(revert_reason(revoke(v, BOB, id)), b"only the creator");
    // The schedule that cannot be revoked vests on for its beneficiary
    assert_eq!(release(v, BOB, fixed), Ok(300));
    assert_eq!(release(v, BOB, id), Ok(300));
    assert_eq!(mock::balance(&KSA_ASSET_ID, &ALICE), SUPPLY - 1_300);
    assert_conserved(v, &[KSA_ASSET_ID, GOLD]);
}

#[test]
fn unknown_schedules_and_malformed_arguments() {
    let v = setup();
    create(v, ALICE, &[ksa(10)], (BOB, 0, 0, 10, true)).unwrap();
    for id in [0u64, 2, u64::MAX] {
        assert_eq!(schedule(v, id), None);
        for (method, args) in [
            ("release", encode(&id)),
            ("revoke", encode(&id)),
            ("releasable", encode(&id)),
            ("vested", encode(&(id, T0))),
        ] {
            assert_eq!(
                revert_reason(mock::call(ALICE, v, method, &args)),
                b"unknown schedule",
                "{method} {id}"
            );
        }
    }
    for (method, args) in [
        ("release", vec![1, 0, 0]),
        ("release", encode(&(1u64, 0u8))),
        ("revoke", vec![]),
        ("schedule", encode(&1u32)),
        ("releasable", encode(&1u128)),
        ("vested", encode(&1u64)),
        ("count", vec![0]),
        ("by_beneficiary", encode(&(BOB, 0u64))),
        ("by_creator", encode(&(ALICE, 0u64, 10u64))),
    ] {
        assert_eq!(
            revert_reason(mock::call(ALICE, v, method, &args)),
            b"malformed arguments",
            "{method}"
        );
    }
}

#[test]
fn totals_near_u128_max_vest_exactly() {
    const BIG: [u8; 32] = [0xb1; 32];
    mock::reset();
    mock::register_asset(BIG, "BIG", 0, [0x99; 32]);
    mock::mint(BIG, ALICE, u128::MAX).unwrap();
    mock::set_block_timestamp(T0);
    let v = mock::deploy(ALICE, 0, b"vesting", vesting::dispatch, &[]).unwrap();
    let big = |amount| Funds { asset: BIG, amount };
    // The whole supply, split in two schedules of 100 years
    let total = u128::MAX - 7;
    let whole = create(v, ALICE, &[big(total)], (BOB, T0, 0, MAX_DURATION, true)).unwrap();
    let small = create(
        v,
        ALICE,
        &[big(7)],
        (CAROL, T0, MAX_DURATION / 2, MAX_DURATION, false),
    )
    .unwrap();
    assert_eq!(mock::balance(&BIG, &v), u128::MAX);
    assert_conserved(v, &[BIG]);
    for elapsed in [
        0,
        1,
        2,
        3,
        999_999_937,
        MAX_DURATION / 3,
        MAX_DURATION / 2,
        MAX_DURATION - 1,
    ] {
        assert_eq!(
            vested(v, whole, T0 + elapsed),
            reference(total, elapsed, MAX_DURATION).unwrap(),
            "{elapsed}"
        );
    }
    assert_eq!(vested(v, whole, T0 + MAX_DURATION), total);
    assert_eq!(vested(v, whole, u64::MAX), total);
    // Released at a third, revoked at half, released again: the parts add up to the whole
    mock::set_block_timestamp(T0 + MAX_DURATION / 3);
    let first = release(v, BOB, whole).unwrap();
    assert_eq!(
        first,
        reference(total, MAX_DURATION / 3, MAX_DURATION).unwrap()
    );
    mock::set_block_timestamp(T0 + MAX_DURATION / 2);
    let kept = reference(total, MAX_DURATION / 2, MAX_DURATION).unwrap();
    let refund = revoke(v, ALICE, whole).unwrap();
    assert_eq!(refund, total - kept);
    let second = release(v, CAROL, whole).unwrap();
    assert_eq!(first + second, kept);
    assert_eq!(mock::balance(&BIG, &BOB), kept);
    assert_eq!(mock::balance(&BIG, &ALICE), refund);
    assert_conserved(v, &[BIG]);
    // The small one reaches its cliff at half time: floor(7 / 2), then the rest at the end
    assert_eq!(release(v, BOB, small), Ok(3));
    mock::set_block_timestamp(T0 + MAX_DURATION);
    assert_eq!(release(v, BOB, small), Ok(4));
    assert_eq!(mock::balance(&BIG, &CAROL), 7);
    assert_eq!(mock::balance(&BIG, &v), 0);
    assert_conserved(v, &[BIG]);
}

#[test]
fn lists_page_by_position() {
    let v = setup();
    // 55 schedules for BOB: ALICE creates the odd IDs and CAROL the even ones; one for ALICE from BOB
    for n in 0..55u128 {
        let creator = if n % 2 == 0 { ALICE } else { CAROL };
        create(v, creator, &[ksa(n + 1)], (BOB, 0, 0, 100, false)).unwrap();
    }
    create(v, BOB, &[gold(3)], (ALICE, 0, 0, 100, true)).unwrap();
    assert_eq!(count(v), 56);
    assert_eq!(ids(&by_beneficiary(v, BOB, 0, 3)), [1, 2, 3]);
    assert_eq!(ids(&by_beneficiary(v, BOB, 3, 2)), [4, 5]);
    // At most 50 at a time
    let page = by_beneficiary(v, BOB, 0, u32::MAX);
    assert_eq!(page.len(), MAX_PAGE as usize);
    assert_eq!(ids(&page), (1..=50).collect::<Vec<u64>>());
    assert_eq!(page[6], schedule(v, 7).unwrap());
    assert_eq!(
        ids(&by_beneficiary(v, BOB, 50, 1_000)),
        (51..=55).collect::<Vec<u64>>()
    );
    // At the end, past it, and a limit of 0: an empty list
    for (start, limit) in [(55, 10), (56, 10), (u64::MAX, u32::MAX), (0, 0)] {
        assert!(by_beneficiary(v, BOB, start, limit).is_empty());
    }
    assert_eq!(ids(&by_beneficiary(v, ALICE, 0, 50)), [56]);
    assert!(by_beneficiary(v, CAROL, 0, 50).is_empty());
    // By creator: ALICE's are 1, 3, …, 55 and CAROL's 2, 4, …, 54
    assert_eq!(
        ids(&by_creator(v, ALICE, 0, 50)),
        (1..=55).step_by(2).collect::<Vec<u64>>()
    );
    assert_eq!(ids(&by_creator(v, CAROL, 10, 5)), [22, 24, 26, 28, 30]);
    assert_eq!(ids(&by_creator(v, CAROL, 25, 5)), [52, 54]);
    assert_eq!(by_creator(v, BOB, 0, 1), [schedule(v, 56).unwrap()]);
    assert!(by_creator(v, READER, 0, 50).is_empty());
}

#[test]
fn only_create_takes_funds() {
    let v = setup();
    let id = create(v, ALICE, &[ksa(1_000)], (BOB, T0, 0, 1_000, true)).unwrap();
    mock::set_block_timestamp(T0 + 500);
    let state = || {
        (
            mock::balance(&KSA_ASSET_ID, &v),
            mock::balance(&GOLD, &v),
            schedule(v, id),
            count(v),
        )
    };
    let before = state();
    for (method, args) in [
        ("release", encode(&id)),
        ("revoke", encode(&id)),
        ("schedule", encode(&id)),
        ("releasable", encode(&id)),
        ("vested", encode(&(id, T0))),
        ("count", vec![]),
        ("by_beneficiary", encode(&(BOB, 0u64, 10u32))),
        ("by_creator", encode(&(ALICE, 0u64, 10u32))),
    ] {
        for funds in [ksa(1), gold(1_000)] {
            for caller in [ALICE, BOB] {
                assert_eq!(
                    revert_reason(mock::call_with_funds(caller, v, method, &args, &[funds])),
                    b"unexpected attached funds",
                    "{method}"
                );
            }
        }
    }
    assert_eq!(state(), before);
    assert_eq!(mock::balance(&KSA_ASSET_ID, &ALICE), SUPPLY - 1_000);
    assert_eq!(mock::balance(&KSA_ASSET_ID, &BOB), SUPPLY);
    assert_eq!(mock::balance(&GOLD, &BOB), SUPPLY);
    // Without funds the same calls go through
    assert_eq!(release(v, BOB, id), Ok(500));
    assert_eq!(revoke(v, ALICE, id), Ok(500));
    assert_conserved(v, &[KSA_ASSET_ID, GOLD]);
}

#[test]
fn views_only_read() {
    let v = setup();
    let id = create(v, ALICE, &[ksa(1_000)], (BOB, T0, 0, 1_000, true)).unwrap();
    // Every key the contract has written so far (the storage layout in src/lib.rs)
    let keys = [
        b"count".to_vec(),
        b"clock".to_vec(),
        [b"s".as_slice(), &id.to_be_bytes()].concat(),
        [b"bn".as_slice(), &BOB].concat(),
        [b"bi".as_slice(), &BOB, &0u64.to_be_bytes()].concat(),
        [b"cn".as_slice(), &ALICE].concat(),
        [b"ci".as_slice(), &ALICE, &0u64.to_be_bytes()].concat(),
    ];
    let state = || {
        (
            keys.iter()
                .map(|key| mock::storage_value(&v, key))
                .collect::<Vec<_>>(),
            mock::events(),
            mock::balance(&KSA_ASSET_ID, &v),
        )
    };
    let before = state();
    assert!(before.0.iter().all(Option::is_some));
    // In a later block the views answer for its time, and keep nothing
    mock::set_block_timestamp(T0 + 600);
    assert_eq!(releasable(v, id), 600);
    assert_eq!(vested(v, id, T0 + 600), 600);
    assert!(schedule(v, id).is_some());
    assert_eq!(count(v), 1);
    assert_eq!(by_beneficiary(v, BOB, 0, 50).len(), 1);
    assert_eq!(by_creator(v, ALICE, 0, 50).len(), 1);
    assert_eq!(state(), before);
    // So a release in a block with an earlier timestamp counts from that one
    mock::set_block_timestamp(T0 + 500);
    assert_eq!(release(v, BOB, id), Ok(500));
    assert_ne!(state(), before);
}

/// Random creates, releases and revokes by anyone, with block timestamps that mostly move on and sometimes run back:
/// the contract agrees with a model of the rules, and holds exactly what its schedules owe.
#[test]
fn random_operations_keep_the_books() {
    let v = setup();
    let accounts = [ALICE, BOB, CAROL];
    let assets = [KSA_ASSET_ID, GOLD];
    let mut rng = Rng(7);
    // The model: the contract's clock, and every schedule as it should be
    let mut clock = T0;
    let mut model: Vec<Schedule> = Vec::new();
    let mut time = T0;
    let (mut refused, mut empty) = (0, 0);
    for step in 0..400 {
        time = if rng.below(4) == 0 {
            time - rng.below(600)
        } else {
            time + rng.below(600)
        };
        mock::set_block_timestamp(time);
        let now = clock.max(time);
        let who = rng.pick(&accounts);
        let touched = match rng.below(3) {
            0 => {
                let beneficiary = rng.pick(&accounts);
                let duration = rng.below(3_000) + 1;
                let cliff = rng.below(duration + 1);
                let start = match rng.below(3) {
                    0 => 0,
                    1 => now - rng.below(5_000),
                    _ => now + rng.below(5_000),
                };
                let revocable = rng.below(2) == 0;
                let funds = Funds {
                    asset: rng.pick(&assets),
                    amount: u128::from(rng.below(1_000_000_000)) + 1,
                };
                let terms = (beneficiary, start, cliff, duration, revocable);
                let id = create(v, who, &[funds], terms).unwrap();
                assert_eq!(id, model.len() as u64 + 1);
                model.push(Schedule {
                    id,
                    creator: who,
                    beneficiary,
                    asset: funds.asset,
                    total: funds.amount,
                    released: 0,
                    start: if start == 0 { now } else { start },
                    cliff,
                    duration,
                    revocable,
                    revoked_at: None,
                });
                clock = now;
                id
            }
            _ if model.is_empty() => continue,
            1 => {
                let index = rng.below(model.len() as u64) as usize;
                let s = &mut model[index];
                let due = expected_vested(s, now) - s.released;
                let result = release(v, who, s.id);
                if due == 0 {
                    assert_eq!(revert_reason(result), b"nothing to release");
                    empty += 1;
                } else {
                    assert_eq!(result, Ok(due));
                    s.released += due;
                    clock = now;
                }
                s.id
            }
            _ => {
                let index = rng.below(model.len() as u64) as usize;
                let s = &mut model[index];
                let result = revoke(v, who, s.id);
                let refusal: Option<&[u8]> = if who != s.creator {
                    Some(b"only the creator")
                } else if !s.revocable {
                    Some(b"schedule is not revocable")
                } else if s.revoked_at.is_some() {
                    Some(b"already revoked")
                } else {
                    None
                };
                if let Some(reason) = refusal {
                    assert_eq!(revert_reason(result), reason);
                    refused += 1;
                } else {
                    let vested = expected_vested(s, now).max(s.released);
                    assert_eq!(result, Ok(s.total - vested));
                    s.total = vested;
                    s.revoked_at = Some(now);
                    clock = now;
                }
                s.id
            }
        };
        // The schedule touched is as the model says; the contract holds what all of them owe
        let s = &model[touched as usize - 1];
        assert_eq!(schedule(v, touched).as_ref(), Some(s), "step {step}");
        assert_eq!(
            releasable(v, touched),
            expected_vested(s, clock.max(time)) - s.released
        );
        for asset in assets {
            let owed: u128 = model
                .iter()
                .filter(|s| s.asset == asset)
                .map(|s| s.total - s.released)
                .sum();
            assert_eq!(mock::balance(&asset, &v), owed, "step {step}");
        }
        if step % 100 == 99 {
            assert_eq!(count(v), model.len() as u64);
            for s in &model {
                assert_eq!(schedule(v, s.id).as_ref(), Some(s));
            }
            assert_conserved(v, &assets);
        }
    }
    // The run went through every kind of outcome
    assert!(model.len() > 100);
    assert!(refused > 10 && empty > 0);
    assert!(model.iter().any(|s| s.revoked_at.is_some()));
    assert!(model.iter().any(|s| 0 < s.released && s.released < s.total));
    assert!(model.iter().any(|s| s.released == s.total));
}
