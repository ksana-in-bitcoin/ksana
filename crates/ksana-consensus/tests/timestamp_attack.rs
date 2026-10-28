//! Adversarial test: the numeric bounds of timestamp attacks (D-083, D-090; the SPEC §15 verification gap "numeric
//! simulation of timestamp attacks").
//!
//! ASERT's target varies exponentially with the "time ahead of / behind schedule": every τ seconds of timestamp
//! inflation doubles the next block's target. D-083 limits the timestamp to the anchored BTC block's time plus
//! `MAX_TIMESTAMP_AHEAD_OF_ANCHOR` (4 hours), and D-090 sets the half-life τ to 7 200 seconds, so:
//!
//! - A single block can inflate the next block's target by at most 2^(14 400 / 7 200) = 4 times (2^24 times with a
//!   600-second half-life); 1 second more and it is invalid.
//! - A producer that always inflates timestamps to the bound cannot keep producing blocks faster: ASERT catches up
//!   with the timestamps by "height × interval", and inflating by Δ seconds only puts the chain Δ / T blocks ahead of
//!   schedule once (2 880 blocks on devnet), after which the block rate returns to one block per T seconds. The number
//!   of reward slots follows the number of blocks, so the entire gain from inflating timestamps is a one-time 2 880
//!   slots.
//! - Backdating (only required to exceed the median time of the previous 11 blocks) can understate by at most about
//!   5 intervals, shrinking the target by less than 0.3%, with almost no effect on others.

use ksana_consensus::{
    check_anchor_time, genesis_header, median_time_past, pow_limit_target, work_from_target, Asert,
    ConsensusError, U256,
};
use ksana_primitives::{BlockHeader, BtcAnchor, Params, DEVNET};

const GENESIS_TIME: u64 = 1_700_000_000;
const T: u64 = 5;
const TAU: u64 = 7_200;
const MAX_AHEAD: u64 = 4 * 3600;

/// A genesis block whose anchor is 2^16 times harder than the PoW limit: the target inflated 4 times is still below
/// the limit and is not clamped.
fn hard_genesis() -> BlockHeader {
    let mut g = genesis_header(
        &DEVNET,
        GENESIS_TIME,
        BtcAnchor {
            height: 100,
            hash: [7; 32],
        },
        [9; 32],
    )
    .unwrap();
    g.bits = 0x1f00_ffff;
    g
}

fn on_schedule(height: u64) -> u64 {
    GENESIS_TIME + height * T
}

fn to_f64(x: U256) -> f64 {
    x.to_limbs().iter().rev().fold(0.0, |acc, &limb| {
        acc * 18_446_744_073_709_551_616.0 + limb as f64
    })
}

#[test]
fn the_anchor_bound_lets_one_block_lower_the_next_difficulty_at_most_four_times() {
    assert_eq!(DEVNET.max_timestamp_ahead_of_anchor_secs, MAX_AHEAD);
    assert_eq!(DEVNET.asert_half_life_secs, TAU);
    assert_eq!(MAX_AHEAD / TAU, 2, "the bound is exactly two half-lives");
    let g = hard_genesis();
    let asert = Asert::from_genesis(&DEVNET, &g).unwrap();
    let limit = pow_limit_target(DEVNET.pow_limit_bits).unwrap();
    for height in [1u64, 7, 1_000, 1_000_000] {
        let honest = asert.next_target(height, on_schedule(height)).unwrap();
        let inflated = asert
            .next_target(height, on_schedule(height) + MAX_AHEAD)
            .unwrap();
        assert_eq!(inflated, honest.shl(2), "height {height}: exactly 4 times");
        assert!(inflated < limit, "not clamped by the limit");
        // one more half-life of inflation would be 8 times: D-083 does not allow it
        assert_eq!(
            asert
                .next_target(height, on_schedule(height) + MAX_AHEAD + TAU)
                .unwrap(),
            honest.shl(3)
        );
    }
    // a timestamp exactly at the bound is valid, 1 second more is invalid (anchor_time is the anchored BTC block's
    // time)
    let anchor_time = 1_700_100_000u32;
    let header = |timestamp: u64| BlockHeader {
        timestamp,
        ..g.clone()
    };
    assert_eq!(
        check_anchor_time(
            &DEVNET,
            &header(u64::from(anchor_time) + MAX_AHEAD),
            anchor_time
        ),
        Ok(())
    );
    assert_eq!(
        check_anchor_time(
            &DEVNET,
            &header(u64::from(anchor_time) + MAX_AHEAD + 1),
            anchor_time
        ),
        Err(ConsensusError::TimestampAheadOfAnchor {
            max: u64::from(anchor_time) + MAX_AHEAD,
            got: u64::from(anchor_time) + MAX_AHEAD + 1,
        })
    );
    // if the half-life were still 600 seconds, the same 4 hours would be 2^24 times: D-090's half-life is half of this
    // bound
    let old = Params {
        asert_half_life_secs: 600,
        ..DEVNET
    };
    let old_asert = Asert::from_genesis(&old, &g).unwrap();
    let height = 1_000;
    let honest = old_asert.next_target(height, on_schedule(height)).unwrap();
    assert_eq!(
        old_asert
            .next_target(height, on_schedule(height) + MAX_AHEAD)
            .unwrap(),
        limit.min(honest.shl(24))
    );
}

/// A producer with fixed hashrate produces blocks at the expected solve time (no randomness). The honest producer's
/// timestamp is the simulated clock; the attacker's timestamps are always inflated by `ahead` seconds.
/// Returns the number of blocks produced within `hours` hours and the maximum ratio of the target to the honest target
/// along the way.
fn simulate(ahead: u64, hours: u64) -> (u64, f64) {
    let g = hard_genesis();
    let asert = Asert::from_genesis(&DEVNET, &g).unwrap();
    let anchor_work = to_f64(work_from_target(&asert.anchor_target()).unwrap());
    // the hashrate makes the expected solve time of the anchor target exactly T: the honest chain is on schedule from
    // the start
    let hashrate = anchor_work / T as f64;
    let honest_target = to_f64(asert.anchor_target());
    let end = (hours * 3600) as f64;
    let mut clock = 0f64;
    let mut height = 0u64;
    let mut parent_timestamp = GENESIS_TIME + ahead;
    let mut max_ratio = 0f64;
    loop {
        let target = asert.next_target(height, parent_timestamp).unwrap();
        max_ratio = max_ratio.max(to_f64(target) / honest_target);
        let work = to_f64(work_from_target(&target).unwrap());
        clock += work / hashrate;
        if clock > end {
            return (height, max_ratio);
        }
        height += 1;
        parent_timestamp = GENESIS_TIME + clock.floor() as u64 + ahead;
    }
}

#[test]
fn inflating_timestamps_to_the_bound_gains_a_one_time_advance_of_the_bound_over_the_spacing() {
    let (honest_24h, honest_ratio) = simulate(0, 24);
    assert_eq!(
        honest_24h,
        24 * 3600 / T,
        "the honest producer is on schedule: one block per T seconds"
    );
    assert!((honest_ratio - 1.0).abs() < 1e-9);
    let gain = MAX_AHEAD / T; // 2 880
    for hours in [24u64, 48] {
        let (attacker, ratio) = simulate(MAX_AHEAD, hours);
        let honest = hours * 3600 / T;
        let advance = attacker as i64 - honest as i64;
        assert!(
            (advance - gain as i64).abs() <= gain as i64 / 100,
            "{hours} hours: attacker {attacker} blocks, honest {honest} blocks, {advance} more (expected about {gain})"
        );
        assert!(
            ratio <= 4.0 + 1e-9,
            "the target is inflated to at most 4 times the honest target, actual {ratio}"
        );
    }
    // the advance does not grow with time: 48 hours and 24 hours give the same number of extra blocks
    assert_eq!(
        simulate(MAX_AHEAD, 48).0 - simulate(MAX_AHEAD, 24).0,
        24 * 3600 / T
    );
}

#[test]
fn backdating_within_the_median_rule_barely_raises_the_next_difficulty() {
    let g = hard_genesis();
    let asert = Asert::from_genesis(&DEVNET, &g).unwrap();
    let height = 10_000u64;
    // the previous 11 blocks on schedule: the median time is 5 intervals back; the timestamp only has to exceed it
    let recent: Vec<u64> = (height - 10..=height).map(on_schedule).collect();
    let median = median_time_past(&recent).unwrap();
    assert_eq!(median, on_schedule(height) - 5 * T);
    let earliest = median + 1;
    let honest = asert.next_target(height, on_schedule(height)).unwrap();
    let backdated = asert.next_target(height, earliest).unwrap();
    assert!(backdated < honest);
    // 2^(−24 / 7200) ≈ 0.99769: shrinks by less than 0.3%
    assert!(backdated.checked_mul_u64(1_000).unwrap() > honest.checked_mul_u64(997).unwrap());
}
