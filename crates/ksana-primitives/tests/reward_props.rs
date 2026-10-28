//! Property tests: the arithmetic of block rewards, reward slots and the block share neither over-issues nor
//! under-issues (D-074, D-084, D-089, D-090, D-105).
//!
//! - Any DAA score and weight: the two parts split to the producer and the share account sum exactly to the block's
//!   slot rewards; the share does not exceed the `BLOCK_SHARE_BPS` fraction; no share when all slots are past
//!   `BLOCK_SHARE_END`.
//! - The single-block reward is monotonically non-increasing in the DAA score, and 0 after 128 halvings.
//! - The halving period of a DAA score is the one a walk over the period lengths finds (the first
//!   `REWARD_INTERVAL_DOUBLINGS` periods double in length, the later ones keep the last length, D-105), and the
//!   rewards of all slots sum to exactly the period-by-period total, which stays under `max_total_rewards`.
//! - On a linear chain the reward slots of the blocks tile 1 to D exactly (D is the tip's DAA score) without
//!   overlapping: the empty-block streak weights 12, 24, ..., 120 are both difficulty multipliers and slot counts, so
//!   the total issuance is determined by the DAA score alone, regardless of how the blocks are split into empty /
//!   non-empty ones.
//! - Only 1 and the empty block weight of some streak are valid weights; the weight is monotone in the streak, capped,
//!   and never overflows.

use ksana_primitives::{Params, DEVNET};
use proptest::prelude::*;

const WEIGHTS: [u64; 6] = [1, 12, 24, 48, 96, 120];

fn weight() -> impl Strategy<Value = u64> {
    prop::sample::select(WEIGHTS.to_vec())
}

proptest! {
    #[test]
    fn the_split_of_a_block_sums_to_its_slot_rewards(
        daa in any::<u64>(),
        weight in weight(),
        interval in 1u64..5_000_000,
        doublings in 0u32..4,
        share_end in 0u64..20_000_000,
        bps in 0u32..=10_000,
    ) {
        let params = Params {
            reward_halving_interval: interval,
            reward_interval_doublings: doublings,
            block_share_end: share_end,
            block_share_bps: bps,
            ..DEVNET
        };
        let total = params.block_rewards_at(daa, weight);
        let split = params.block_rewards_split_at(daa, weight);
        let (producer, share, carrier) = (split.producer, split.share, split.carrier);
        prop_assert_eq!(producer.checked_add(share).and_then(|s| s.checked_add(carrier)), Some(total));
        prop_assert_eq!(split.total(), total);
        prop_assert!(share <= total * u128::from(bps) / 10_000);
        prop_assert!(carrier <= total * u128::from(params.carrier_bps) / 10_000);
        prop_assert!(total <= u128::from(weight) * params.block_reward);
        let first_slot = daa.saturating_sub(weight - 1);
        if first_slot >= share_end || bps == 0 {
            prop_assert_eq!(share, 0);
        }
        if daa < share_end && bps == 10_000 {
            prop_assert_eq!((producer, share, carrier), (0, total, 0), "when the share takes everything the carrier only gets what is left: nothing");
        }
        // Check slot by slot
        let slots: u128 = (0..weight)
            .filter_map(|i| daa.checked_sub(i))
            .map(|slot| params.block_reward_at(slot))
            .sum();
        prop_assert_eq!(slots, total);
        let shares: u128 = (0..weight)
            .filter_map(|i| daa.checked_sub(i))
            .map(|slot| params.block_share_at(slot))
            .sum();
        prop_assert_eq!(shares, share);
        let carriers: u128 = (0..weight)
            .filter_map(|i| daa.checked_sub(i))
            .map(|slot| params.block_carrier_at(slot))
            .sum();
        prop_assert_eq!(carriers, carrier);
    }

    /// D-095: the carrier's part per slot is `CARRIER_BPS / 10 000` of the reward (rounded down), taken out of the
    /// producer's part, the block share unchanged; the split of the sponsored fees does not exceed
    /// `CARRIER_FEE_BPS / 10 000`, and the two parts sum to the total.
    #[test]
    fn the_carrier_share_comes_out_of_the_producer_share(
        daa in any::<u64>(),
        weight in weight(),
        carrier_bps in 0u32..=8_000,
        fee_bps in 0u32..=10_000,
        amount in any::<u128>(),
    ) {
        let base = Params {
            reward_halving_interval: 1_000,
            block_share_bps: 2_000,
            block_share_end: 5_000,
            carrier_bps: 0,
            ..DEVNET
        };
        let with = Params {
            carrier_bps,
            carrier_fee_bps: fee_bps,
            ..base.clone()
        };
        let without = base.block_rewards_split_at(daa, weight);
        let split = with.block_rewards_split_at(daa, weight);
        prop_assert_eq!(split.share, without.share, "block share unchanged");
        prop_assert_eq!(split.producer.checked_add(split.carrier), Some(without.producer), "the carrier's part comes out of the producer's part");
        prop_assert_eq!(split.total(), with.block_rewards_at(daa, weight));
        for slot in (0..weight).filter_map(|i| daa.checked_sub(i)) {
            let reward = with.block_reward_at(slot);
            prop_assert_eq!(with.block_carrier_at(slot), reward / 10_000 * u128::from(carrier_bps) + reward % 10_000 * u128::from(carrier_bps) / 10_000);
        }
        let fee = with.carrier_fee_at(daa, amount);
        prop_assert!(fee <= amount);
        prop_assert_eq!(fee, amount / 10_000 * u128::from(fee_bps) + amount % 10_000 * u128::from(fee_bps) / 10_000);
    }

    #[test]
    fn the_block_reward_never_increases(
        daa in any::<u64>(),
        step in 0u64..100_000_000,
        interval in 1u64..50_000_000,
        doublings in 0u32..70,
    ) {
        let later = daa.saturating_add(step);
        prop_assert!(DEVNET.block_reward_at(later) <= DEVNET.block_reward_at(daa));
        prop_assert!(DEVNET.block_reward_at(daa) <= DEVNET.block_reward);
        let params = Params {
            reward_halving_interval: interval,
            reward_interval_doublings: doublings,
            ..DEVNET
        };
        prop_assert!(params.block_reward_at(later) <= params.block_reward_at(daa));
        prop_assert!(params.reward_period_at(later) >= params.reward_period_at(daa));
        prop_assert!(params.block_reward_at(daa) <= params.block_reward);
    }

    /// D-105: period k is `interval << min(k, doublings)` DAA scores long; the period found matches a walk over those
    /// lengths, the reward is `BLOCK_REWARD >> k`, and the next DAA score is in the same period or the next one.
    #[test]
    fn the_halving_period_follows_the_doubling_lengths(
        daa in 0u64..20_000,
        interval in 1u64..50,
        doublings in 0u32..8,
    ) {
        let params = Params {
            reward_halving_interval: interval,
            reward_interval_doublings: doublings,
            ..DEVNET
        };
        let (mut start, mut period) = (0u64, 0u64);
        while daa >= start + (interval << period.min(u64::from(doublings))) {
            start += interval << period.min(u64::from(doublings));
            period += 1;
        }
        prop_assert_eq!(params.reward_period_at(daa), Some(period));
        let reward = u32::try_from(period).ok().and_then(|k| params.block_reward.checked_shr(k)).unwrap_or(0);
        prop_assert_eq!(params.block_reward_at(daa), reward);
        let next = params.reward_period_at(daa + 1);
        prop_assert!(next == Some(period) || next == Some(period + 1));
    }

    /// D-105: summed slot by slot, the rewards equal the period-by-period total, which stays under
    /// `(doublings + 2) × BLOCK_REWARD × interval`.
    #[test]
    fn the_rewards_of_all_slots_stay_under_the_bound(
        reward in 1u128..1_000_000,
        interval in 1u64..20,
        doublings in 0u32..6,
    ) {
        let params = Params {
            block_reward: reward,
            reward_halving_interval: interval,
            reward_interval_doublings: doublings,
            ..DEVNET
        };
        // The reward is below 2^20, so it is 0 from period 20 on
        let end: u64 = (0..20u32).map(|k| interval << k.min(doublings)).sum();
        prop_assert_eq!(params.block_reward_at(end), 0);
        let total: u128 = (0..end).map(|d| params.block_reward_at(d)).sum();
        let exact: u128 = (0..20u32)
            .map(|k| (reward >> k) * (u128::from(interval) << k.min(doublings)))
            .sum();
        prop_assert_eq!(total, exact);
        prop_assert!(total <= params.max_total_rewards());
        prop_assert_eq!(params.max_total_rewards(), u128::from(doublings + 2) * reward * u128::from(interval));
    }

    #[test]
    fn only_real_block_weights_are_accepted(weight in any::<u64>()) {
        prop_assert_eq!(DEVNET.is_block_weight(weight), WEIGHTS.contains(&weight));
    }

    #[test]
    fn empty_block_weights_are_monotone_capped_and_never_overflow(streak in any::<u64>()) {
        let w = DEVNET.empty_block_weight(streak);
        prop_assert!(WEIGHTS[1..].contains(&w), "{}", w);
        prop_assert!(DEVNET.empty_block_weight(streak.saturating_add(1)) >= w);
        prop_assert!(DEVNET.is_block_weight(w));
    }

    /// A linear chain: each block takes its weight from its D-090 streak and the DAA score accumulates the weights;
    /// the slots (D − w + 1 ..= D) of the blocks tile 1 to D exactly.
    #[test]
    fn a_chain_tiles_the_reward_slots_exactly_once(
        empties in prop::collection::vec(any::<bool>(), 1..60),
        doublings in 0u32..3,
    ) {
        let params = Params {
            block_reward: 1_000,
            reward_halving_interval: 37,
            reward_interval_doublings: doublings,
            block_share_end: 100,
            ..DEVNET
        };
        let (mut streak, mut daa) = (0u64, 0u64);
        let mut covered: Vec<u64> = Vec::new();
        let mut issued = 0u128;
        let mut shared = 0u128;
        for &empty in &empties {
            streak = if empty { streak + 1 } else { 0 };
            let weight = if empty { params.empty_block_weight(streak) } else { 1 };
            prop_assert!(params.is_block_weight(weight));
            daa += weight;
            covered.extend(daa - weight + 1..=daa);
            issued += params.block_rewards_at(daa, weight);
            shared += params.block_rewards_split_at(daa, weight).share;
        }
        prop_assert_eq!(covered, (1..=daa).collect::<Vec<_>>());
        let by_slot: u128 = (1..=daa).map(|s| params.block_reward_at(s)).sum();
        prop_assert_eq!(issued, by_slot);
        let shared_by_slot: u128 = (1..=daa).map(|s| params.block_share_at(s)).sum();
        prop_assert_eq!(shared, shared_by_slot);
        prop_assert!(issued <= params.max_total_rewards());
    }
}

#[test]
fn the_reward_is_zero_after_128_halvings_and_the_cap_covers_every_slot() {
    let p = Params {
        block_reward: 1_000,
        reward_halving_interval: 3,
        ..DEVNET
    };
    assert_eq!(p.block_reward_at(0), 1_000);
    assert_eq!(p.block_reward_at(2), 1_000);
    assert_eq!(p.block_reward_at(3), 500);
    assert_eq!(p.block_reward_at(3 * 127), 1_000 >> 127);
    assert_eq!(p.block_reward_at(3 * 128), 0);
    assert_eq!(p.block_reward_at(u64::MAX), 0);
    // The sum of the rewards of every slot does not exceed the bound 2 × block reward × halving interval
    let total: u128 = (0..3 * 130).map(|s| p.block_reward_at(s)).sum();
    assert!(total <= p.max_total_rewards());
    assert_eq!(p.max_total_rewards(), 2 * 1_000 * 3);
}
