// The page's market math against the lending contract's formulas (contracts/examples/lending/src/lib.rs), most cases with the
// expectations of the contract's own tests (contracts/examples/lending/tests/lending.rs), rounding and saturation included.
import { describe, expect, it } from 'vitest';
import { parseUnits } from '../../chain/assets';
import {
  BPS, INTEREST_MARGIN_SECONDS, SECONDS_PER_YEAR, U128_MAX, WAD, borrowRateBps, borrowRoom, closeLimit, collateralValue, coveredRepay,
  debtWithMargin, freeCollateral, grownDebt, healthBps, healthState, healthText, indexAfter, limitUsedBps, limitsOf, liquidity,
  maxWithdrawShares, minCollateralFor, mulDiv, mulDivUp, percentText, priceDecimals, priceDropBps, priceFromTyped, priceText,
  quoteLiquidation, rateCorners, sharesForAmount, sharesValue, supplyRateBps, supplyShares, totalAssets, utilizationBps,
  withdrawProblem, type Books, type Params,
} from './math';

/** The contract tests' market: `params()` in tests/lending.rs */
const PARAMS: Params = {
  collateralFactorBps: 7_500n,
  liquidationThresholdBps: 8_000n,
  liquidationBonusBps: 500n,
  reserveFactorBps: 1_000n,
  baseRateBps: 200n,
  slope1Bps: 1_000n,
  kinkBps: 8_000n,
  slope2Bps: 10_000n,
};

/** The playground's deploy preset (src/examples/index.ts) */
const PRESET: Params = { ...PARAMS, slope1Bps: 400n, slope2Bps: 6_000n };

const P127 = 1n << 127n;

function books(cash: bigint, totalBorrows: bigint, reserves: bigint, totalShares: bigint): Books {
  return { cash, totalBorrows, reserves, totalShares };
}

/** A deterministic sequence of numbers below `bound` */
function sequence(seed: bigint) {
  let s = seed;
  return (bound: bigint) => {
    s = (s * 6_364_136_223_846_793_005n + 1_442_695_040_888_963_407n) % (1n << 64n);
    return ((s << 64n) | (s >> 7n)) % bound;
  };
}

describe('mulDiv', () => {
  it('rounds down, and up for mulDivUp, where the quotient fits', () => {
    for (let a = 0n; a < 12n; a++) {
      for (let b = 0n; b < 12n; b++) {
        for (let c = 1n; c < 12n; c++) {
          expect(mulDiv(a, b, c)).toBe((a * b) / c);
          expect(mulDivUp(a, b, c)).toBe((a * b + c - 1n) / c);
        }
      }
    }
  });

  it('matches the contract tests beyond u128', () => {
    const MAX = U128_MAX;
    expect(mulDiv(MAX, MAX, MAX)).toBe(MAX);
    expect(mulDivUp(MAX, MAX, MAX)).toBe(MAX);
    expect(mulDiv(P127, 4n, 2n)).toBeNull();
    expect(mulDiv(P127, 2n, 2n)).toBe(P127);
    expect(mulDiv(MAX, 2n, 2n)).toBe(MAX);
    expect(mulDiv(MAX, MAX, 1n)).toBeNull();
    expect(mulDiv(MAX, MAX - 1n, MAX)).toBe(MAX - 1n);
    expect(mulDiv(MAX, MAX, MAX - 1n)).toBeNull();
    expect(mulDiv(P127 + 1n, P127 + 3n, P127 + 2n)).toBe(P127 + 1n);
    expect(mulDivUp(P127 + 1n, P127 + 3n, P127 + 2n)).toBe(P127 + 2n);
    expect(mulDiv(MAX, 3n, 4n)).toBe((3n << 126n) - 1n);
    expect(mulDivUp(MAX, 3n, 4n)).toBe(3n << 126n);
    expect(mulDiv(1n << 64n, 1n << 64n, 1n)).toBeNull();
    expect(mulDiv(1n << 64n, 1n << 64n, 2n)).toBe(P127);
    expect(mulDiv(WAD * WAD, WAD * WAD, WAD * WAD)).toBe(WAD * WAD);
    expect(mulDiv(WAD * WAD, WAD * WAD, WAD)).toBeNull();
    expect(mulDiv(0n, 0n, 0n)).toBeNull();
    expect(mulDivUp(MAX, MAX, 0n)).toBeNull();
    expect(mulDiv(0n, MAX, 1n)).toBe(0n);
    expect(mulDivUp(MAX, 0n, 7n)).toBe(0n);
    expect(mulDivUp(MAX, 2n, 2n)).toBe(MAX);
    expect(mulDivUp(MAX - 1n, MAX, MAX - 1n)).toBe(MAX);
    expect(mulDivUp(MAX, MAX, MAX - 1n)).toBeNull();
  });
});

describe('the market', () => {
  it('counts the suppliers’ assets and the liquidity without the reserves', () => {
    expect(totalAssets(books(500n, 750n, 0n, 1_000n))).toBe(1_250n);
    expect(totalAssets(books(500n, 750n, 100n, 1_000n))).toBe(1_150n);
    // reserves above the cash eat into the borrows, never below 0 (an insolvent market)
    expect(totalAssets(books(1n, 0n, 5_000n, 1_000n))).toBe(0n);
    expect(totalAssets(books(10n, 100n, 30n, 1n))).toBe(80n);
    expect(liquidity(books(250n, 750n, 0n, 1_000n))).toBe(250n);
    expect(liquidity(books(250n, 750n, 40n, 1_000n))).toBe(210n);
    expect(liquidity(books(1n, 0n, 5_000n, 1_000n))).toBe(0n);
  });

  it('caps the utilization at 100%', () => {
    expect(utilizationBps(books(0n, 0n, 0n, 0n))).toBe(0n);
    expect(utilizationBps(books(500_000_000n, 500_000_000n, 0n, 1_000_000_000n))).toBe(5_000n);
    expect(utilizationBps(books(250n, 750n, 0n, 1_000n))).toBe(7_500n);
    // borrows above the assets once the reserves are kept out of them
    expect(utilizationBps(books(10n, 1_120_000_000n, 112_000_000n, 1_000_000_000n))).toBe(10_000n);
    expect(utilizationBps(books(1n, 0n, 5_000n, 1_000n))).toBe(0n);
  });

  it('follows the kinked rate line of the_rate_follows_utilization_around_the_kink', () => {
    const rates = (u: bigint, p = PARAMS) => {
      const borrow = borrowRateBps(p, u);
      return [borrow, borrow === null ? null : supplyRateBps(borrow, u, p.reserveFactorBps)];
    };
    expect(rates(0n)).toEqual([200n, 0n]);
    expect(rates(4_000n)).toEqual([700n, 252n]);
    expect(rates(8_000n)).toEqual([1_200n, 864n]);
    expect(rates(9_000n)).toEqual([6_200n, 5_022n]);
    expect(rates(10_000n)).toEqual([11_200n, 10_080n]);
    // one_year_of_interest_matches_the_rate
    expect(rates(5_000n)).toEqual([825n, 371n]);
    // with the kink at 100% the second slope never applies
    const flat = { ...PARAMS, kinkBps: 10_000n };
    expect(rates(5_000n, flat)).toEqual([700n, 315n]);
    expect(rates(10_000n, flat)).toEqual([1_200n, 1_080n]);
    // an_insolvent_market_refuses_supply_and_withdrawals: 1 010% a year at full use
    expect(borrowRateBps({ ...PARAMS, reserveFactorBps: 5_000n, baseRateBps: 0n, slope2Bps: 100_000n }, 10_000n)).toBe(101_000n);
    // each term is rounded down on its own: 200 + floor(1 000 · 1 / 8 000) + 0
    expect(borrowRateBps(PARAMS, 1n)).toBe(200n);
    expect(borrowRateBps(PARAMS, 8_001n)).toBe(1_205n);
    expect(borrowRateBps({ ...PARAMS, kinkBps: 0n }, 10n)).toBeNull();
  });

  it('draws the line through its corners', () => {
    expect(rateCorners(PARAMS)).toEqual([
      { utilization: 0n, rate: 200n }, { utilization: 8_000n, rate: 1_200n }, { utilization: 10_000n, rate: 11_200n },
    ]);
    expect(rateCorners(PRESET)).toEqual([
      { utilization: 0n, rate: 200n }, { utilization: 8_000n, rate: 600n }, { utilization: 10_000n, rate: 6_600n },
    ]);
    expect(rateCorners({ ...PARAMS, kinkBps: 10_000n })).toEqual([{ utilization: 0n, rate: 200n }, { utilization: 10_000n, rate: 1_200n }]);
  });
});

describe('interest', () => {
  it('accrues a year at 8.25% as one_year_of_interest_matches_the_rate', () => {
    const year = SECONDS_PER_YEAR;
    const index = indexAfter(WAD, 825n, year);
    expect(index).toBe(WAD + ((825n * WAD) / 10_000n / year) * year);
    expect(indexAfter(WAD, 825n, 0n)).toBe(WAD);
    const debt = grownDebt(500_000_000n, WAD, index);
    expect(debt).toBe((500_000_000n * index + WAD - 1n) / WAD);
    const interest = (500_000_000n * index) / WAD - 500_000_000n;
    expect(interest >= 41_249_999n && interest <= 41_250_000n).toBe(true);
    expect(debt - (500_000_000n + interest) <= 1n).toBe(true);
    // a year at full use of the contract tests' market costs 112%, within rounding
    const full = (1_000_000_000n * indexAfter(WAD, 11_200n, year)) / WAD - 1_000_000_000n;
    expect(full >= 1_119_999_999n && full <= 1_120_000_000n).toBe(true);
  });

  it('keeps a margin of at least the interest until an operation lands', () => {
    expect(debtWithMargin(0n, WAD, 6_600n)).toBe(0n);
    expect(debtWithMargin(1_000n, WAD, 0n)).toBe(1_001n);
    // The contract accrues from its last stored accrual at the stored rate; the page reads the view a while after it and the
    // operation lands later still: the margin from the view's debt covers the debt at the block
    const next = sequence(7n);
    for (let i = 0; i < 500; i++) {
      const principal = 1n + next(10n ** 30n);
      const rate = next(300_001n);
      const stored = WAD + next(WAD);
      const seen = 1n + next(86_400n);
      const later = next(600n);
      const viewIndex = indexAfter(stored, rate, seen);
      const blockIndex = indexAfter(stored, rate, seen + later);
      const viewed = grownDebt(principal, stored, viewIndex);
      const atBlock = grownDebt(principal, stored, blockIndex);
      expect(debtWithMargin(viewed, viewIndex, rate) >= atBlock).toBe(true);
    }
    // an hour at 66% a year on 1 000 tokens of 8 decimals: about 0.0075%
    const debt = 1_000n * 100_000_000n;
    const margin = debtWithMargin(debt, WAD, 6_600n) - debt;
    expect(margin).toBe(((debt * ((6_600n * WAD) / BPS / SECONDS_PER_YEAR) * INTEREST_MARGIN_SECONDS) + WAD - 1n) / WAD + 1n);
    expect(margin > 7_530_000n && margin < 7_540_000n).toBe(true);
  });
});

describe('positions', () => {
  it('values the collateral and its limits as collateral_comes_in_and_out_within_the_borrow_limit', () => {
    expect(limitsOf(1_000n, WAD, PARAMS)).toEqual({ borrowLimit: 750n, liquidationLimit: 800n });
    expect(limitsOf(799n, WAD, PARAMS)).toEqual({ borrowLimit: 599n, liquidationLimit: 639n });
    expect(limitsOf(0n, WAD, PARAMS)).toEqual({ borrowLimit: 0n, liquidationLimit: 0n });
    expect(collateralValue(1_000_000_000n, (WAD / 100n) * 95n)).toBe(950_000_000n);
    // above u128 the value counts as u128::MAX (extreme_prices_saturate_instead_of_reverting)
    expect(collateralValue(2n * WAD, U128_MAX)).toBe(U128_MAX);
    expect(limitsOf(2n * WAD, U128_MAX, PARAMS)).toEqual({
      borrowLimit: mulDiv(U128_MAX, 7_500n, 10_000n), liquidationLimit: mulDiv(U128_MAX, 8_000n, 10_000n),
    });
  });

  it('reads the health of a_price_drop_makes_a_position_liquidatable', () => {
    expect(healthBps(800n, 750n)).toBe(10_666n);
    expect(healthBps(0n, 0n)).toBeNull();
    const at = (price: bigint) => limitsOf(1_000_000_000n, price, PARAMS).liquidationLimit;
    expect(healthBps(at((WAD / 100n) * 95n), 750_000_000n)).toBe(10_133n);
    expect(at((WAD / 10n) * 9n)).toBe(720_000_000n);
    expect(healthBps(at((WAD / 10n) * 9n), 750_000_000n)).toBe(9_600n);
    expect(limitsOf(562_500_000n, (WAD / 10n) * 9n, PARAMS).liquidationLimit).toBe(405_000_000n);
    expect(healthBps(mulDiv(U128_MAX, 8_000n, 10_000n) ?? 0n, 1n)).toBe(U128_MAX);

    expect(healthState(0n, 800n)).toBe('none');
    expect(healthState(750n, 800n)).toBe('near');
    expect(healthState(800n, 800n)).toBe('near');
    expect(healthState(801n, 800n)).toBe('liquidatable');
    expect(healthState(500n, 800n)).toBe('safe');
    expect(healthState(750_000_000n, 720_000_000n)).toBe('liquidatable');
    expect(healthState(1n, 0n)).toBe('liquidatable');
    // near means a fall of less than 10% in the price: health 1.1111 is near, 1.1112 is not
    expect(priceDropBps(10_666n)).toBe(624n);
    expect(priceDropBps(11_111n)).toBe(999n);
    expect(priceDropBps(11_112n)).toBe(1_000n);
    expect(priceDropBps(9_600n)).toBe(0n);
    expect(healthState(10_000n, 11_111n)).toBe('near');
    expect(healthState(10_000n, 11_112n)).toBe('safe');

    expect(limitUsedBps(0n, 800n)).toBe(0n);
    expect(limitUsedBps(750n, 800n)).toBe(9_375n);
    expect(limitUsedBps(1n, 3n)).toBe(3_334n);
    expect(limitUsedBps(900n, 800n)).toBe(10_000n);
    expect(limitUsedBps(1n, 0n)).toBe(10_000n);
  });

  it('frees only the collateral the debt does not need', () => {
    // 800 still cover a debt of 600, 799 do not
    expect(minCollateralFor(600n, WAD, 7_500n)).toBe(800n);
    expect(freeCollateral(1_000n, 600n, WAD, 7_500n)).toBe(200n);
    expect(freeCollateral(800n, 600n, WAD, 7_500n)).toBe(0n);
    expect(freeCollateral(1_000n, 0n, WAD, 7_500n)).toBe(1_000n);
    expect(freeCollateral(1_000n, 751n, WAD, 7_500n)).toBe(0n);
    expect(minCollateralFor(0n, WAD, 7_500n)).toBe(0n);
    // whatever the price and the factor, the least collateral covers the debt and one unit less does not
    const next = sequence(3n);
    for (let i = 0; i < 2_000; i++) {
      const debt = 1n + next(10n ** 24n);
      const price = 1n + next(1_000n * WAD);
      const factor = 1n + next(9_999n);
      const least = minCollateralFor(debt, price, factor);
      if (least === null) continue;
      expect(limitsOf(least, price, { ...PARAMS, collateralFactorBps: factor }).borrowLimit >= debt).toBe(true);
      expect(limitsOf(least - 1n, price, { ...PARAMS, collateralFactorBps: factor }).borrowLimit < debt).toBe(true);
    }
    expect(minCollateralFor(U128_MAX, 1n, 1n)).toBeNull();
  });

  it('lends within the borrow limit and the liquidity, as borrowing_stays_within_the_borrow_limit_and_the_liquidity', () => {
    expect(borrowRoom(750n, 0n, 1_000n)).toBe(750n);
    expect(borrowRoom(750n, 700n, 300n)).toBe(50n);
    expect(borrowRoom(750n, 750n, 300n)).toBe(0n);
    expect(borrowRoom(750n, 0n, 250n)).toBe(250n);
    expect(borrowRoom(750n, 760n, 1_000n)).toBe(0n);
  });
});

describe('shares', () => {
  it('credits shares as suppliers_get_shares_and_take_them_out', () => {
    expect(supplyShares(1_000n, books(0n, 0n, 0n, 0n))).toBe(1_000n);
    expect(supplyShares(500n, books(1_000n, 0n, 0n, 1_000n))).toBe(500n);
    // a share worth more than a unit: one unit is too small (one_year_of_interest_matches_the_rate)
    const year = books(500_000_000n, 541_250_000n, 4_125_000n, 1_000_000_000n);
    expect(supplyShares(1n, year)).toBe('tooSmall');
    expect(supplyShares(1_037_125_000n, year)).toBe(1_000_000_000n);
    expect(supplyShares(1_000_000n, books(1n, 0n, 5_000n, 1_000n))).toBe('insolvent');
    // the first shares need no assets
    expect(supplyShares(7n, books(3n, 0n, 0n, 0n))).toBe(7n);
  });

  it('pays shares out, rounded down, within the liquidity (withdrawals_wait_for_liquidity)', () => {
    const lent = books(250_000_000n, 750_000_000n, 0n, 1_000_000_000n);
    const held = 1_000_000_000n;
    expect(withdrawProblem(1_000_000_000n, held, lent)).toBe('liquidity');
    expect(withdrawProblem(250_000_001n, held, lent)).toBe('liquidity');
    expect(withdrawProblem(250_000_000n, held, lent)).toBeNull();
    expect(sharesValue(250_000_000n, lent)).toBe(250_000_000n);
    expect(withdrawProblem(0n, held, lent)).toBe('zero');
    expect(withdrawProblem(1_001n, 1_000n, books(1_500n, 0n, 0n, 1_500n))).toBe('shares');
    expect(withdrawProblem(1_000n, 1_000n, books(1n, 0n, 5_000n, 1_000n))).toBe('insolvent');
    // a share worth less than a unit pays nothing on its own
    expect(withdrawProblem(1n, 3n, books(2n, 0n, 0n, 3n))).toBe('tooSmall');
    expect(maxWithdrawShares(held, lent)).toBe(250_000_000n);
    expect(maxWithdrawShares(held, books(1_000_000_000n, 0n, 0n, 1_000_000_000n))).toBe(held);
    expect(maxWithdrawShares(1_000n, books(1n, 0n, 5_000n, 1_000n))).toBe(0n);
    // the supplier of one_year_of_interest_matches_the_rate takes all but the reserves
    const repaid = books(1_041_250_001n, 0n, 4_125_000n, 1_000_000_000n);
    expect(sharesValue(1_000_000_000n, repaid)).toBe(totalAssets(repaid));
    expect(maxWithdrawShares(1_000_000_000n, repaid)).toBe(1_000_000_000n);
  });

  it('turns an amount into the fewest shares that pay it', () => {
    const b = books(6_000n, 0n, 0n, 3_000n);
    expect(sharesForAmount(2_000n, 1_000n, b)).toBe(1_000n);
    expect(sharesForAmount(1n, 1_000n, b)).toBe(1n);
    expect(sharesValue(1n, b)).toBe(2n);
    expect(sharesForAmount(5_000n, 1_000n, b)).toBe(1_000n);
    expect(sharesForAmount(10n, 1_000n, books(1n, 0n, 5_000n, 1_000n))).toBe(0n);
    const next = sequence(11n);
    for (let i = 0; i < 2_000; i++) {
      const shares = 1n + next(10n ** 27n);
      const assets = 1n + next(10n ** 27n);
      const held = 1n + next(shares);
      const market = books(assets, 0n, 0n, shares);
      const amount = 1n + next(sharesValue(held, market) + 1n);
      const s = sharesForAmount(amount, held, market);
      expect(s <= held).toBe(true);
      // at least the amount unless capped by the shares held, and one share fewer pays less
      if (s < held) expect(sharesValue(s, market) >= amount).toBe(true);
      if (s > 0n) expect(sharesValue(s - 1n, market) < amount || s === held).toBe(true);
      // the most the liquidity covers never pays more than it
      const available = next(assets + 1n);
      const capped = maxWithdrawShares(held, books(available, assets - available, 0n, shares));
      expect(sharesValue(capped, books(available, assets - available, 0n, shares)) <= available).toBe(true);
    }
  });
});

describe('liquidations', () => {
  it('repays at most half the debt, rounded up', () => {
    expect(closeLimit(750_000_000n)).toBe(375_000_000n);
    expect(closeLimit(274_999_999n)).toBe(137_500_000n);
    expect(closeLimit(1n)).toBe(1n);
    expect(closeLimit(0n)).toBe(0n);
  });

  it('seizes collateral as a_price_drop_makes_a_position_liquidatable', () => {
    expect(quoteLiquidation(1_000_000_000n, 750_000_000n, 1_000_000_000n, (WAD / 10n) * 9n, 500n)).toEqual({
      repaid: 375_000_000n, seized: 437_500_000n, refund: 625_000_000n, writtenOff: 0n,
      debtLeft: 375_000_000n, collateralLeft: 562_500_000n, capped: false,
    });
    const half = quoteLiquidation(100_000_000n, 375_000_000n, 562_500_000n, WAD / 2n, 500n);
    expect(half?.repaid).toBe(100_000_000n);
    expect(half?.seized).toBe(210_000_000n);
    expect(half?.debtLeft).toBe(275_000_000n);
    expect(quoteLiquidation(1_000_000_000n, 274_999_999n, 352_500_000n, WAD / 10n, 500n)?.repaid).toBe(137_500_000n);
  });

  it('writes off what the collateral cannot cover (a_liquidation_that_takes_all_the_collateral_writes_off_the_rest)', () => {
    expect(quoteLiquidation(400_000_000n, 700_000_000n, 1_000_000_000n, WAD / 10n, 500n)).toEqual({
      repaid: 350_000_000n, seized: 1_000_000_000n, refund: 50_000_000n, writtenOff: 350_000_000n,
      debtLeft: 0n, collateralLeft: 0n, capped: true,
    });
    // one unit at the lowest price takes all the collateral (an_insolvent_market_refuses_supply_and_withdrawals)
    const all = quoteLiquidation(1n, 1_100n, 2_000n, 1n, 500n);
    expect([all?.repaid, all?.seized, all?.capped]).toEqual([1n, 2_000n, true]);
    // a seizure above u128 is capped at the collateral too (extreme_prices_saturate_instead_of_reverting)
    const big = 1n << 100n;
    const extreme = quoteLiquidation(big, big, 2n * WAD, 1n, 500n);
    expect([extreme?.repaid, extreme?.seized, extreme?.writtenOff]).toEqual([big / 2n, 2n * WAD, big / 2n]);
  });

  it('suggests no more than the collateral covers', () => {
    const covered = coveredRepay(1_000_000_000n, WAD / 10n, 500n);
    expect(covered).toBe(95_238_095n);
    const at = quoteLiquidation(covered, 700_000_000n, 1_000_000_000n, WAD / 10n, 500n);
    expect([at?.seized, at?.capped, at?.writtenOff]).toEqual([999_999_990n, false, 0n]);
    expect(quoteLiquidation(covered + 2n, 700_000_000n, 1_000_000_000n, WAD / 10n, 500n)?.capped).toBe(true);
    // at a price of 1 with a 5% bonus, 1 000 000 000 of collateral is worth 1 000 000 000 and covers 952 380 952
    expect(coveredRepay(1_000_000_000n, WAD, 500n)).toBe(952_380_952n);
  });
});

describe('the price in tokens', () => {
  it('reads the contract’s price with both assets’ decimals', () => {
    expect(priceDecimals(8, 8)).toBe(18);
    expect(priceText(WAD, 8, 8)).toBe('1');
    expect(priceText(2n * WAD, 8, 8)).toBe('2');
    expect(priceText((WAD / 100n) * 95n, 8, 8)).toBe('0.95');
    expect(priceText(WAD / 2n, 8, 8)).toBe('0.5');
    expect(priceText(1n, 8, 8)).toBe('0.000000000000000001');
    // 1 token of 0 decimals worth 2.5 KSA: 250 000 000 base units per base unit
    expect(priceText(250_000_000n * WAD, 8, 0)).toBe('2.5');
    // 1 token of 8 decimals worth 3 units of an asset with none: 3 / 10^8 per base unit
    expect(priceText(30_000_000_000n, 0, 8)).toBe('3');
    expect(priceText(5n, 0, 30)).toBe('5,000,000,000,000');
  });

  it('turns a typed price back into the contract’s price exactly', () => {
    expect(priceFromTyped(parseUnits('1', 18), 8, 8)).toBe(WAD);
    expect(priceFromTyped(parseUnits('0.95', 18), 8, 8)).toBe((WAD / 100n) * 95n);
    expect(priceFromTyped(parseUnits('2.5', priceDecimals(8, 0)), 8, 0)).toBe(250_000_000n * WAD);
    expect(priceFromTyped(parseUnits('3', priceDecimals(0, 8)), 0, 8)).toBe(30_000_000_000n);
    expect(priceFromTyped(parseUnits('0', 18), 8, 8)).toBeNull();
    expect(priceFromTyped(5_000_000_000_000n, 0, 30)).toBe(5n);
    expect(priceFromTyped(5_000_000_000_001n, 0, 30)).toBeNull();
    for (const text of ['1', '0.95', '12.345678', '0.000000000000000001', '1000000']) {
      const price = priceFromTyped(parseUnits(text, 18), 8, 8);
      expect(price === null ? null : priceText(price, 8, 8).replace(/,/g, '')).toBe(text);
    }
  });
});

describe('text', () => {
  it('writes rates and health for people', () => {
    expect(percentText(825n)).toBe('8.25%');
    expect(percentText(10_000n)).toBe('100%');
    expect(percentText(0n)).toBe('0%');
    expect(percentText(101_000n)).toBe('1,010%');
    expect(healthText(10_666n)).toBe('1.06');
    expect(healthText(9_600n)).toBe('0.96');
    expect(healthText(10_000n)).toBe('1.00');
    expect(healthText(10_133n)).toBe('1.01');
    expect(healthText(999_999n)).toBe('99.99');
    expect(healthText(1_000_000n)).toBe('100+');
    expect(healthText(U128_MAX)).toBe('100+');
  });
});
