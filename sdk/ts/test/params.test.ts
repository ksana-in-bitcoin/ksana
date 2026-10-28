/**
 * Protocol parameters (`crates/ksana-primitives/src/params.rs`): the halving schedule of the block reward and the max supply
 * (D-074, D-105), the block share split (D-089), the carrier reward split (D-095); the same as Rust's `Params::reward_period_at`,
 * `block_reward_at`, `max_total_rewards`, `block_share_at`, `block_carrier_at`, `carrier_fee_at` and `block_rewards_split_at`.
 */

import assert from 'node:assert/strict';
import { test } from 'node:test';

import {
  DEVNET,
  MAINNET,
  type Params,
  SIGNET,
  blockCarrierAt,
  blockRewardAt,
  blockRewardsSplitAt,
  blockShareAt,
  carrierFeeAt,
  maxTotalRewards,
  nextUpgrade,
  rewardPeriodAt,
  rulesAt,
} from '../src/index.js';

/** The exact sum of all reward slots, period by period (D-105): period k has `interval << min(k, doublings)` slots of `reward >> k`. */
function exactTotalRewards(p: Params): bigint {
  let total = 0n;
  for (let k = 0n; k < 128n; k++) {
    const doublings = k < BigInt(p.reward_interval_doublings) ? k : BigInt(p.reward_interval_doublings);
    total += (p.block_reward >> k) * (p.reward_halving_interval << doublings);
  }
  return total;
}

test('the block reward halves by DAA score (D-074)', () => {
  const interval = DEVNET.reward_halving_interval;
  assert.equal(blockRewardAt(DEVNET, 0n), 990_000_000n);
  assert.equal(blockRewardAt(DEVNET, interval - 1n), 990_000_000n);
  assert.equal(blockRewardAt(DEVNET, interval), 495_000_000n);
  assert.equal(blockRewardAt(DEVNET, 3n * interval), 123_750_000n);
  assert.equal(blockRewardAt(DEVNET, 127n * interval), 0n);
  assert.equal(blockRewardAt(DEVNET, 128n * interval), 0n);
  assert.equal(blockRewardAt({ ...DEVNET, reward_halving_interval: 0n }, 5n), 0n);
  assert.throws(() => blockRewardAt(DEVNET, -1n), RangeError);
  assert.equal(SIGNET.block_reward, DEVNET.block_reward);
});

test('total rewards do not exceed half of the max supply (D-074)', () => {
  // at most one rewarded block per DAA score: Σ reward(d) ≤ 2 × BLOCK_REWARD × interval
  const total = exactTotalRewards(DEVNET);
  assert.equal(DEVNET.reward_interval_doublings, 0);
  assert.equal(maxTotalRewards(DEVNET), 2n * DEVNET.block_reward * DEVNET.reward_halving_interval);
  assert.ok(total <= maxTotalRewards(DEVNET));
  assert.ok(total <= DEVNET.ksa_max_supply / 2n);
});

test('halving periods double in length and then stay (D-105, the same as halving_periods_double_then_stay in Rust)', () => {
  const p: Params = { ...DEVNET, block_reward: 1_000n, reward_halving_interval: 3n, reward_interval_doublings: 2 };
  // periods 3, 6, 12, 12, … long: boundaries at 3, 9, 21, 33, 45, …
  const cases: [bigint, bigint][] = [
    [0n, 0n],
    [2n, 0n],
    [3n, 1n],
    [8n, 1n],
    [9n, 2n],
    [20n, 2n],
    [21n, 3n],
    [32n, 3n],
    [33n, 4n],
    [45n, 5n],
  ];
  for (const [daa, period] of cases) {
    assert.equal(rewardPeriodAt(p, daa), period, `${daa}`);
    assert.equal(blockRewardAt(p, daa), 1_000n >> period, `${daa}`);
  }
  const u64Max = 2n ** 64n - 1n;
  assert.equal(rewardPeriodAt(p, u64Max), (u64Max - 9n) / 12n + 2n);
  assert.equal(blockRewardAt(p, u64Max), 0n);
  for (const daa of [0n, 2n, 3n, 8n, 9n, 20n, 21n, 1_000n, u64Max]) {
    assert.equal(rewardPeriodAt({ ...p, reward_interval_doublings: 0 }, daa), daa / 3n, `${daa}`);
  }
  assert.equal(rewardPeriodAt({ ...p, reward_halving_interval: 0n }, 5n), null);
  assert.equal(blockRewardAt({ ...p, reward_halving_interval: 0n }, 5n), 0n);
  assert.throws(() => rewardPeriodAt(p, -1n), RangeError);
  // doubling without end: with interval 1 period k starts at 2^k − 1, so the largest DAA score starts period 64
  const endless: Params = { ...DEVNET, block_reward: 2n ** 128n - 1n, reward_halving_interval: 1n, reward_interval_doublings: 2 ** 32 - 1 };
  assert.equal(rewardPeriodAt(endless, u64Max - 1n), 63n);
  assert.equal(rewardPeriodAt(endless, u64Max), 64n);
  assert.equal(blockRewardAt(endless, u64Max), (2n ** 128n - 1n) >> 64n);
  assert.equal(maxTotalRewards(endless), 2n ** 128n - 1n, 'at most 2^128 − 1');
  // the bound and the exact sum
  assert.equal(maxTotalRewards(p), 4n * 1_000n * 3n);
  assert.equal(exactTotalRewards(p), 3n * 1_000n + 6n * 500n + 12n * (250n + 125n + 62n + 31n + 15n + 7n + 3n + 1n));
});

test('the upgrade schedule layers rules by DAA score (D-076, the same as rules_follow_the_upgrade_schedule in Rust)', () => {
  const p: Params = {
    ...DEVNET,
    upgrades: [
      { version: 3, daa_score: 100n, block_gas_limit: 200_000_000n, max_vmop_version: 3 },
      { version: 4, daa_score: 250n, native_max_gas: 2_000_000n, min_vmop_version: 3 },
    ],
  };
  const base = {
    version: 2,
    block_gas_limit: 100_000_000n,
    native_max_gas: 1_000_000n,
    min_vmop_version: 2,
    max_vmop_version: 2,
    block_share_bps: 2_000,
    block_share_end: 50_457_600n,
    carrier_bps: 500,
    carrier_fee_bps: 5_000,
  };
  assert.deepEqual(rulesAt(DEVNET, 0n), base);
  assert.deepEqual(rulesAt(SIGNET, 2n ** 64n - 1n), base);
  assert.deepEqual(rulesAt(p, 99n), base);
  const v3 = { ...base, version: 3, block_gas_limit: 200_000_000n, max_vmop_version: 3 };
  assert.deepEqual(rulesAt(p, 100n), v3);
  assert.deepEqual(rulesAt(p, 249n), v3);
  assert.deepEqual(rulesAt(p, 250n), { ...v3, version: 4, native_max_gas: 2_000_000n, min_vmop_version: 3 });
  assert.equal(nextUpgrade(p, 0n)?.version, 3);
  assert.equal(nextUpgrade(p, 100n)?.version, 4);
  assert.equal(nextUpgrade(p, 250n), null);
  assert.equal(nextUpgrade(DEVNET, 0n), null);
  assert.throws(() => rulesAt(DEVNET, -1n), RangeError);
});

test('the block share is split per slot until BLOCK_SHARE_END (D-089, the same as block_share_splits_each_slot_until_block_share_end in Rust)', () => {
  assert.equal(DEVNET.block_share_bps, 2_000);
  assert.equal(DEVNET.block_share_end, 2n * DEVNET.reward_halving_interval);
  assert.equal(blockShareAt(DEVNET, 0n), 198_000_000n);
  assert.equal(blockShareAt(DEVNET, DEVNET.block_share_end - 1n), 99_000_000n);
  assert.equal(blockShareAt(DEVNET, DEVNET.block_share_end), 0n);
  const p: Params = { ...DEVNET, block_reward: 1_001n, reward_halving_interval: 10n, block_share_bps: 2_000, block_share_end: 20n, carrier_bps: 0 };
  const split = (producer: bigint, share: bigint, carrier = 0n) => ({ producer, share, carrier });
  assert.equal(blockShareAt(p, 0n), 200n, '1 001 × 20% rounded down');
  assert.equal(blockShareAt(p, 9n), 200n);
  assert.equal(blockShareAt(p, 10n), 100n, '500 × 20% after the halving');
  assert.equal(blockShareAt(p, 19n), 100n);
  assert.equal(blockShareAt(p, 20n), 0n, 'share ended');
  assert.deepEqual(blockRewardsSplitAt(p, 5n, 1n), split(801n, 200n));
  // weight 12, DAA score 25: slots 14-19 are 500 each (share 100), slots 20-25 are 250 each (no share)
  assert.deepEqual(blockRewardsSplitAt(p, 25n, 12n), split(6n * 400n + 6n * 250n, 600n));
  assert.deepEqual(blockRewardsSplitAt(p, 5n, 0n), split(0n, 0n));
  assert.deepEqual(blockRewardsSplitAt(p, 1n, 12n), split(2n * 801n, 2n * 200n), 'only slots 0 and 1');
  assert.deepEqual(blockRewardsSplitAt({ ...p, block_share_bps: 10_000 }, 0n, 1n), split(0n, 1_001n));
  assert.deepEqual(blockRewardsSplitAt({ ...p, block_share_bps: 0 }, 0n, 1n), split(1_001n, 0n));
  assert.equal(blockShareAt({ ...p, block_share_bps: 20_000 }, 0n), 1_001n, 'above 100% counts as 100%');
  assert.throws(() => blockShareAt(DEVNET, -1n), RangeError);
  assert.throws(() => blockRewardsSplitAt(DEVNET, 0n, -1n), RangeError);
  // upgrade schedule: from version 3 (activation point 15) the ratio becomes 10% and the cutoff moves to 40
  const q: Params = { ...p, upgrades: [{ version: 3, daa_score: 15n, block_share_bps: 1_000, block_share_end: 40n }] };
  assert.equal(blockShareAt(q, 14n), 100n, 'still 20% before activation');
  assert.equal(blockShareAt(q, 15n), 50n, '10% after activation');
  assert.equal(blockShareAt(q, 39n), 12n, '125 × 10% rounded down, cutoff moved later');
  assert.equal(blockShareAt(q, 40n), 0n);
  assert.deepEqual(blockRewardsSplitAt(q, 16n, 3n), split(400n + 2n * 450n, 100n + 2n * 50n));
  assert.equal(rulesAt(q, 15n).block_share_bps, 1_000);
  assert.equal(rulesAt(q, 14n).block_share_end, 20n);
});

test('the carrier reward is taken per slot from the producer portion and the sponsored fees are split by ratio (D-095, the same as carrier_takes_a_share_of_each_slot_and_of_the_fees in Rust)', () => {
  assert.equal(DEVNET.carrier_bps, 500);
  assert.equal(DEVNET.carrier_fee_bps, 5_000);
  assert.equal(DEVNET.carrier_repost_delay, 2);
  const p: Params = {
    ...DEVNET,
    block_reward: 1_001n,
    reward_halving_interval: 10n,
    block_share_bps: 2_000,
    block_share_end: 20n,
    carrier_bps: 500,
    carrier_fee_bps: 5_000,
  };
  const split = (producer: bigint, share: bigint, carrier: bigint) => ({ producer, share, carrier });
  assert.equal(blockCarrierAt(p, 0n), 50n, '1 001 × 5% rounded down');
  assert.equal(blockCarrierAt(p, 10n), 25n, '500 × 5% after the halving');
  assert.equal(blockCarrierAt(p, 20n), 12n, 'the carrier reward continues after the share ends: 250 × 5%');
  assert.deepEqual(blockRewardsSplitAt(p, 5n, 1n), split(751n, 200n, 50n));
  // weight 12, DAA score 25: slots 14-19 are 500 each (share 100, carrier 25), slots 20-25 are 250 each (carrier 12)
  assert.deepEqual(blockRewardsSplitAt(p, 25n, 12n), split(6n * 375n + 6n * 238n, 600n, 6n * 25n + 6n * 12n));
  assert.equal(carrierFeeAt(p, 0n, 1_001n), 500n, 'sponsored fees split in half, rounded down');
  assert.equal(carrierFeeAt(p, 0n, 1n), 0n);
  // mainnet (D-105): the carrier's part of a 4.2 KSA slot is 0.21
  assert.equal(blockCarrierAt(MAINNET, 0n), 21_000_000n);
  // the two ratios add up to more than 100%: the carrier only gets what is left after the share
  const over: Params = { ...p, block_share_bps: 9_800 };
  assert.equal(blockCarrierAt(over, 0n), 1_001n - 980n);
  assert.deepEqual(blockRewardsSplitAt(over, 0n, 1n), split(0n, 980n, 21n));
  // upgrade schedule: from version 3 (activation point 15) the carrier gets 10%, and 30% of the sponsored fees
  const q: Params = { ...p, upgrades: [{ version: 3, daa_score: 15n, carrier_bps: 1_000, carrier_fee_bps: 3_000 }] };
  assert.equal(blockCarrierAt(q, 14n), 25n, 'still 5% before activation');
  assert.equal(blockCarrierAt(q, 15n), 50n, '10% after activation');
  assert.equal(carrierFeeAt(q, 14n, 1_000n), 500n);
  assert.equal(carrierFeeAt(q, 15n, 1_000n), 300n);
  assert.equal(rulesAt(q, 15n).carrier_bps, 1_000);
  assert.throws(() => carrierFeeAt(p, 0n, -1n), RangeError);
});

test('mainnet parameters (D-096): chain ID 1000, differs from devnet only in the items §13 lists and the block share placeholders (D-134), total rewards exactly equal the max supply (D-089, D-105)', () => {
  assert.equal(MAINNET.chain_id, 1000);
  assert.equal(MAINNET.native_confirmations, 3);
  assert.equal(MAINNET.pow_limit_bits, 0x1f00_ffff);
  assert.equal(MAINNET.block_reward, 420_000_000n);
  assert.equal(MAINNET.reward_halving_interval, 12_500_000n);
  assert.equal(MAINNET.reward_interval_doublings, 2);
  assert.equal(MAINNET.ksa_max_supply, 21_000_000_000_000_000n);
  assert.equal(MAINNET.block_share_bps, 0);
  assert.equal(MAINNET.block_share_end, 0n);
  assert.deepEqual(
    {
      ...MAINNET,
      chain_id: DEVNET.chain_id,
      native_confirmations: DEVNET.native_confirmations,
      pow_limit_bits: DEVNET.pow_limit_bits,
      block_reward: DEVNET.block_reward,
      reward_halving_interval: DEVNET.reward_halving_interval,
      reward_interval_doublings: DEVNET.reward_interval_doublings,
      ksa_max_supply: DEVNET.ksa_max_supply,
      block_share_bps: DEVNET.block_share_bps,
      block_share_end: DEVNET.block_share_end,
    },
    DEVNET,
  );
  assert.notEqual(MAINNET.chain_id, SIGNET.chain_id);
  assert.notEqual(MAINNET.chain_id, DEVNET.chain_id);
  assert.equal(maxTotalRewards(MAINNET), MAINNET.ksa_max_supply);
  // the exact sum is 4 KSA short of the cap
  assert.equal(exactTotalRewards(MAINNET), 20_999_999_600_000_000n);
  // the period boundaries: 12 500 000, 37 500 000, 87 500 000, then every 50 000 000; 0 from DAA score 1 387 500 000 on
  const cases: [bigint, bigint, bigint][] = [
    [0n, 0n, 420_000_000n],
    [12_499_999n, 0n, 420_000_000n],
    [12_500_000n, 1n, 210_000_000n],
    [37_499_999n, 1n, 210_000_000n],
    [37_500_000n, 2n, 105_000_000n],
    [87_499_999n, 2n, 105_000_000n],
    [87_500_000n, 3n, 52_500_000n],
    [137_500_000n, 4n, 26_250_000n],
    [1_387_499_999n, 28n, 1n],
    [1_387_500_000n, 29n, 0n],
  ];
  for (const [daa, period, reward] of cases) {
    assert.equal(rewardPeriodAt(MAINNET, daa), period, `${daa}`);
    assert.equal(blockRewardAt(MAINNET, daa), reward, `${daa}`);
  }
});
