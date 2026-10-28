/** The pool math against values worked out from the AMM contract's formulas and its tests, rounding and overflow included. */
import { describe, expect, it } from 'vitest';
import {
  U128_MAX, bpsText, decimalRatio, deposit, isqrt, matchingAmount, parsePercent, percentText, priceImpact, swapOutput, unitPrice,
  withSlippage, withdraw,
} from './math';

/** A deterministic sequence of numbers below `bound` (the contract tests' LCG) */
function sequence(seed: bigint) {
  let s = seed;
  return (bound: bigint) => {
    s = (s * 6_364_136_223_846_793_005n + 1_442_695_040_888_963_407n) % (1n << 64n);
    return (s >> 11n) % bound;
  };
}

describe('isqrt', () => {
  it('is floor(sqrt(n))', () => {
    expect([0n, 1n, 2n, 3n, 4n, 8n, 9n, 15n, 16n].map(isqrt)).toEqual([0n, 1n, 1n, 1n, 2n, 2n, 3n, 3n, 4n]);
    expect(isqrt(2_000_000n)).toBe(1_414n);
    expect(isqrt(3_600_000_000n)).toBe(60_000n);
    expect(isqrt(144n)).toBe(12n);
    expect(isqrt(U128_MAX)).toBe((1n << 64n) - 1n);
    for (const x of [3n, 1_000_003n, (1n << 32n) - 1n, (1n << 64n) - 1n]) {
      expect(isqrt(x * x)).toBe(x);
      expect(isqrt(x * x - 1n)).toBe(x - 1n);
      expect(isqrt(x * x + 1n)).toBe(x);
    }
    const next = sequence(9n);
    for (let i = 0; i < 2_000; i++) {
      const n = next(1n << 64n) * next(1n << 64n);
      const r = isqrt(n);
      expect(r * r <= n && (r + 1n) * (r + 1n) > n).toBe(true);
    }
  });
});

describe('swapOutput', () => {
  it('charges 0.3% and rounds down', () => {
    // 1000·997·10^6 / (10^6·1000 + 1000·997) = 997 000 000 000 / 1 000 997 000 = 996.006…
    expect(swapOutput(1_000n, 1_000_000n, 1_000_000n)).toBe(996n);
    // 200 000·997·10^6 / (10^9 + 199 400 000) = 166 249.79…
    expect(swapOutput(200_000n, 1_000_000n, 1_000_000n)).toBe(166_249n);
    // 1000·997·99 000 / (44 000·1000 + 997 000) = 98 703 000 000 / 44 997 000 = 2193.55…
    expect(swapOutput(1_000n, 44_000n, 99_000n)).toBe(2_193n);
    expect(swapOutput(0n, 10n, 10n)).toBe(0n);
    expect(swapOutput(1n, 1_000n, 1_000n)).toBe(0n);
    expect(swapOutput(1n, 0n, 0n)).toBe(0n);
  });

  it('is null where the contract overflows or divides by zero', () => {
    expect(swapOutput(0n, 0n, 5n)).toBeNull();
    expect(swapOutput(U128_MAX, 1n, 1n)).toBeNull();
    expect(swapOutput(U128_MAX / 2n, 1_000_000n, 1_000_000n)).toBeNull();
    // in·997 fits, in·997·R_out does not
    expect(swapOutput(1n << 100n, 1n, 1n << 40n)).toBeNull();
    expect(swapOutput(-1n, 10n, 10n)).toBeNull();
  });
});

describe('deposit', () => {
  it('gives the geometric mean on the first deposit and keeps everything', () => {
    expect(deposit(400n, 900n, 0n, 0n, 0n)).toEqual({ shares: 600n, usedA: 400n, usedB: 900n });
    expect(deposit(40_000n, 90_000n, 0n, 0n, 0n)).toEqual({ shares: 60_000n, usedA: 40_000n, usedB: 90_000n });
    expect(deposit(1_000n, 2_000n, 0n, 0n, 0n)?.shares).toBe(1_414n);
    expect(deposit(9n, 16n, 0n, 0n, 0n)?.shares).toBe(12n);
    expect(deposit(U128_MAX, 2n, 0n, 0n, 0n)).toBeNull();
    expect(deposit(1n << 64n, 1n << 64n, 0n, 0n, 0n)).toBeNull();
  });

  it('keeps only the amounts at the ratio later, rounded up', () => {
    // reserves 1000 / 2000 and 100 shares: side b covers 25 shares, ceil(25·1000/100) = 250 and ceil(25·2000/100) = 500 are kept
    expect(deposit(500n, 500n, 1_000n, 2_000n, 100n)).toEqual({ shares: 25n, usedA: 250n, usedB: 500n });
    // reserves 10 / 10 and 3 shares: 1 share, ceil(10/3) = 4 kept
    expect(deposit(4n, 4n, 10n, 10n, 3n)).toEqual({ shares: 1n, usedA: 4n, usedB: 4n });
    expect(deposit(1n, 1n, 1_000n, 1_000n, 10n)).toEqual({ shares: 0n, usedA: 0n, usedB: 0n });
    // 1414 shares on 1000 / 2000: side a limits to 707, ceil(707·1000/1414) = 500 and ceil(707·2000/1414) = 1000
    expect(deposit(500n, 5_000n, 1_000n, 2_000n, 1_414n)).toEqual({ shares: 707n, usedA: 500n, usedB: 1_000n });
    // min(10 000·60 000/40 000, 9 000·60 000/90 000) = 6 000; ceil(6 000·40 000/60 000) = 4 000 of the 10 000 kept
    expect(deposit(10_000n, 9_000n, 40_000n, 90_000n, 60_000n)).toEqual({ shares: 6_000n, usedA: 4_000n, usedB: 9_000n });
    expect(deposit(100n, 225n, 40_000n, 90_000n, 60_000n)?.shares).toBe(150n);
    // after a swap of 200 000 into 10^6 / 10^6: min(10 000·10^6/1 200 000, 10 000·10^6/833 751) = 8 333, either way round
    expect(deposit(10_000n, 10_000n, 1_200_000n, 833_751n, 1_000_000n)?.shares).toBe(8_333n);
    expect(deposit(10_000n, 10_000n, 833_751n, 1_200_000n, 1_000_000n)?.shares).toBe(8_333n);
  });

  it('is null where the contract overflows or divides by zero', () => {
    expect(deposit(1n, 1n, 0n, 10n, 10n)).toBeNull();
    expect(deposit(1n << 100n, 1n, 1n, 1n, 1n << 30n)).toBeNull();
  });
});

describe('withdraw', () => {
  it('pays shares·R/S rounded down', () => {
    expect(withdraw(1n, 10n, 20n, 3n)).toEqual({ a: 3n, b: 6n });
    expect(withdraw(3n, 10n, 20n, 3n)).toEqual({ a: 10n, b: 20n });
    expect(withdraw(707n, 1_000n, 2_000n, 1_414n)).toEqual({ a: 500n, b: 1_000n });
    expect(withdraw(1n, 1n, 1n, 0n)).toBeNull();
    expect(withdraw(1n << 100n, 1n << 40n, 1n, 1n << 100n)).toBeNull();
  });
});

describe('matchingAmount', () => {
  it('fills the other side with what the contract keeps for the shares the first side buys', () => {
    // 500 of a buys floor(500·1414/1000) = 707 shares, which keep ceil(707·2000/1414) = 1000 of b
    expect(matchingAmount(500n, 1_000n, 2_000n, 1_414n)).toBe(1_000n);
    // and the other way round: 1000 of b buys 707 shares, which keep 500 of a
    expect(matchingAmount(1_000n, 2_000n, 1_000n, 1_414n)).toBe(500n);
    // 501 of a buys floor(708.4) = 708 shares: ceil(708·2000/1414) = ceil(1001.4) = 1002 of b
    expect(matchingAmount(501n, 1_000n, 2_000n, 1_414n)).toBe(1_002n);
    // 5 of 10 with 3 shares buys 1 share, for which ceil(10/3) = 4 of the other side is kept (and 1 of the 5 comes back)
    expect(matchingAmount(5n, 10n, 10n, 3n)).toBe(4n);
    expect(deposit(5n, 4n, 10n, 10n, 3n)).toEqual({ shares: 1n, usedA: 4n, usedB: 4n });
    expect(matchingAmount(1n, 1_000n, 1_000n, 10n)).toBe(0n);
    expect(matchingAmount(10n, 0n, 0n, 0n)).toBeNull();
    expect(matchingAmount(1n << 100n, 1n, 1n, 1n << 30n)).toBeNull();
  });

  it('makes a deposit that keeps the filled side whole and never more of the typed one', () => {
    const next = sequence(42n);
    for (let i = 0; i < 2_000; i++) {
      const [ra, rb, total, amount] = [next(1n << 40n) + 1n, next(1n << 40n) + 1n, next(1n << 40n) + 1n, next(1n << 40n) + 1n];
      const other = matchingAmount(amount, ra, rb, total);
      expect(other).not.toBeNull();
      const d = deposit(amount, other!, ra, rb, total)!;
      expect(d.shares).toBe((amount * total) / ra);
      expect(d.usedB).toBe(other);
      expect(d.usedA <= amount).toBe(true);
    }
  });
});

describe('slippage and price impact', () => {
  it('takes the tolerance off the quote, rounding down', () => {
    expect(withSlippage(1_000n, 50n)).toBe(995n);
    // 996·9950/10 000 = 991.02
    expect(withSlippage(996n, 50n)).toBe(991n);
    expect(withSlippage(10_000n, 10n)).toBe(9_990n);
    expect(withSlippage(10_000n, 100n)).toBe(9_900n);
    expect(withSlippage(1n, 50n)).toBe(0n);
    expect(withSlippage(1_000n, 0n)).toBe(1_000n);
    expect(withSlippage(1_000n, 10_000n)).toBe(0n);
    expect(withSlippage(0n, 50n)).toBe(0n);
  });

  it('measures the price impact in millionths', () => {
    // 996·10^6·1000·10^6 / (1000·10^6·997) = 998 996.9…, so 10^6 − 998 996
    expect(priceImpact(1_000n, 996n, 1_000_000n, 1_000_000n)).toBe(1_004n);
    // 166 249·10^15 / (200 000·10^6·997) = 833 746.2…
    expect(priceImpact(200_000n, 166_249n, 1_000_000n, 1_000_000n)).toBe(166_254n);
    expect(priceImpact(0n, 0n, 10n, 10n)).toBeNull();
    expect(priceImpact(10n, 1n, 0n, 10n)).toBeNull();
  });
});

describe('text', () => {
  it('writes ratios with six significant digits, rounded down', () => {
    expect(decimalRatio(10n, 1n)).toBe('10');
    expect(decimalRatio(5n, 2n)).toBe('2.5');
    expect(decimalRatio(1n, 3n)).toBe('0.333333');
    expect(decimalRatio(2n, 3n)).toBe('0.666666');
    expect(decimalRatio(9_876_543n, 1_000_000n)).toBe('9.87654');
    expect(decimalRatio(1_234_567_891n, 1_000n)).toBe('1,234,567');
    expect(decimalRatio(123n, 1_000_000n)).toBe('0.000123');
    expect(decimalRatio(0n, 5n)).toBe('0');
    expect(decimalRatio(1n, 0n)).toBeNull();
  });

  it('prices one whole unit in the other asset', () => {
    // 1000 KSA against 10 000 PLAY, both with 8 decimals
    expect(unitPrice(1_000n * 10n ** 8n, 10_000n * 10n ** 8n, 8, 8)).toBe('10');
    expect(unitPrice(10_000n * 10n ** 8n, 1_000n * 10n ** 8n, 8, 8)).toBe('0.1');
    // 1 of an 8-decimal asset against 2.50 of a 2-decimal one
    expect(unitPrice(10n ** 8n, 250n, 8, 2)).toBe('2.5');
    expect(unitPrice(0n, 10n, 8, 8)).toBeNull();
  });

  it('writes percentages and basis points', () => {
    expect(percentText(1n, 100n)).toBe('1%');
    expect(percentText(1n, 3n)).toBe('33.33%');
    expect(percentText(1n, 8n)).toBe('12.5%');
    expect(percentText(3n, 3n)).toBe('100%');
    expect(percentText(1n, 1_000_000n)).toBe('< 0.01%');
    expect(percentText(166_254n, 1_000_000n)).toBe('16.62%');
    expect(percentText(0n, 10n)).toBe('0%');
    expect(percentText(5n, 0n)).toBe('0%');
    expect([0n, 5n, 50n, 100n, 1_234n, 5_000n].map(bpsText)).toEqual(['0', '0.05', '0.5', '1', '12.34', '50']);
  });

  it('reads a typed percentage', () => {
    expect(parsePercent('0.5')).toBe(50n);
    expect(parsePercent('1')).toBe(100n);
    expect(parsePercent('2.25')).toBe(225n);
    expect(parsePercent('.5')).toBe(50n);
    expect(parsePercent(' 1 ')).toBe(100n);
    expect(parsePercent('0.5%')).toBe(50n);
    expect(parsePercent('0')).toBe(0n);
    expect(parsePercent('50')).toBe(5_000n);
    for (const bad of ['', '.', 'abc', '50.01', '1.234', '-1', '1,5']) expect(parsePercent(bad)).toBeNull();
  });
});
