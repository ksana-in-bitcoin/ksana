/** The AMM contract's integer arithmetic in bigint, with its rounding and overflow limit, and prices and percentages as text. */

/** The largest u128: the contract reverts when a product it computes goes above it */
export const U128_MAX = (1n << 128n) - 1n;
/** Basis points in a whole */
export const BPS = 10_000n;
const MILLION = 1_000_000n;

function inRange(...values: bigint[]): boolean {
  return values.every((v) => v >= 0n && v <= U128_MAX);
}

/** u128 multiplication: null where the contract's `checked_mul` would fail */
function mul(a: bigint, b: bigint): bigint | null {
  const v = a * b;
  return v <= U128_MAX ? v : null;
}

function ceilDiv(n: bigint, d: bigint): bigint | null {
  if (d === 0n) return null;
  return n % d === 0n ? n / d : n / d + 1n;
}

/** `floor(sqrt(n))` by the contract's Newton iteration, from `2^(floor(log2 n)/2 + 1)` down */
export function isqrt(n: bigint): bigint {
  if (n < 2n) return n < 0n ? 0n : n;
  let x = 1n << (BigInt((n.toString(2).length - 1) >> 1) + 1n);
  for (;;) {
    const y = (n / x + x) >> 1n;
    if (y >= x) return x;
    x = y;
  }
}

/** What a swap pays out, `in·997·R_out / (R_in·1000 + in·997)` rounded down; null where the contract overflows or divides by 0 */
export function swapOutput(amountIn: bigint, reserveIn: bigint, reserveOut: bigint): bigint | null {
  if (!inRange(amountIn, reserveIn, reserveOut)) return null;
  const inWithFee = mul(amountIn, 997n);
  if (inWithFee === null) return null;
  const numerator = mul(inWithFee, reserveOut);
  const scaled = mul(reserveIn, 1000n);
  if (numerator === null || scaled === null) return null;
  const denominator = scaled + inWithFee;
  if (denominator > U128_MAX || denominator === 0n) return null;
  return numerator / denominator;
}

export interface Deposit {
  shares: bigint;
  /** What the contract keeps of each side; the rest of a built-in asset attached is refunded */
  usedA: bigint;
  usedB: bigint;
}

/**
 * A deposit as `add_liquidity` works it out: the first one (no shares yet) gets `isqrt(a·b)` shares and keeps both amounts;
 * later ones get `min(a·S/R_a, b·S/R_b)` shares (rounded down) and keep `ceil(shares·R/S)` of each side.
 */
export function deposit(amountA: bigint, amountB: bigint, reserveA: bigint, reserveB: bigint, totalShares: bigint): Deposit | null {
  if (!inRange(amountA, amountB, reserveA, reserveB, totalShares)) return null;
  if (totalShares === 0n) {
    const product = mul(amountA, amountB);
    return product === null ? null : { shares: isqrt(product), usedA: amountA, usedB: amountB };
  }
  const pa = mul(amountA, totalShares);
  const pb = mul(amountB, totalShares);
  if (pa === null || pb === null || reserveA === 0n || reserveB === 0n) return null;
  const sharesA = pa / reserveA;
  const sharesB = pb / reserveB;
  const shares = sharesA < sharesB ? sharesA : sharesB;
  const qa = mul(shares, reserveA);
  const qb = mul(shares, reserveB);
  if (qa === null || qb === null) return null;
  const usedA = ceilDiv(qa, totalShares);
  const usedB = ceilDiv(qb, totalShares);
  return usedA === null || usedB === null ? null : { shares, usedA, usedB };
}

/** What `remove_liquidity` pays out for `shares`: `shares·R/S` of each side, rounded down */
export function withdraw(shares: bigint, reserveA: bigint, reserveB: bigint, totalShares: bigint): { a: bigint; b: bigint } | null {
  if (!inRange(shares, reserveA, reserveB, totalShares) || totalShares === 0n) return null;
  const pa = mul(shares, reserveA);
  const pb = mul(shares, reserveB);
  return pa === null || pb === null ? null : { a: pa / totalShares, b: pb / totalShares };
}

/**
 * The other side of a deposit at the pool's ratio: the shares `amount` alone buys, `floor(amount·S/R_from)`, and what the
 * contract keeps of the other side for them, `ceil(shares·R_to/S)`, so that side is used in full. Null while the pool has no
 * shares, or where the contract would overflow.
 */
export function matchingAmount(amount: bigint, reserveFrom: bigint, reserveTo: bigint, totalShares: bigint): bigint | null {
  if (!inRange(amount, reserveFrom, reserveTo, totalShares) || totalShares === 0n || reserveFrom === 0n) return null;
  const p = mul(amount, totalShares);
  if (p === null) return null;
  const q = mul(p / reserveFrom, reserveTo);
  return q === null ? null : ceilDiv(q, totalShares);
}

/** The least of `amount` to accept when the price may move against it by `bps` basis points, rounded down */
export function withSlippage(amount: bigint, bps: bigint): bigint {
  if (amount <= 0n) return 0n;
  const keep = bps <= 0n ? BPS : bps >= BPS ? 0n : BPS - bps;
  return (amount * keep) / BPS;
}

/**
 * How much less a swap gets than the pool's current price after its fee, in millionths: `1 − out·R_in·1000 / (in·R_out·997)`,
 * which is the trade's size against the pool and the rounding down. Null without an amount or reserves.
 */
export function priceImpact(amountIn: bigint, amountOut: bigint, reserveIn: bigint, reserveOut: bigint): bigint | null {
  if (amountIn <= 0n || amountOut < 0n || reserveIn <= 0n || reserveOut <= 0n) return null;
  const got = (amountOut * reserveIn * 1000n * MILLION) / (amountIn * reserveOut * 997n);
  return got >= MILLION ? 0n : MILLION - got;
}

function grouped(whole: bigint): string {
  return whole.toString().replace(/\B(?=(\d{3})+(?!\d))/g, ',');
}

/** `num / den` as a decimal with `significant` significant digits (rounded down), the whole part grouped by thousands */
export function decimalRatio(num: bigint, den: bigint, significant = 6): string | null {
  if (den <= 0n || num < 0n) return null;
  if (num === 0n) return '0';
  const whole = num / den;
  let digits: number;
  if (whole > 0n) {
    digits = Math.max(0, significant - whole.toString().length);
  } else {
    // the zeros between the point and the first significant digit
    let zeros = 0;
    for (let probe = num * 10n; probe < den; probe *= 10n) zeros++;
    digits = zeros + significant;
  }
  const frac = digits > 0 ? (((num % den) * 10n ** BigInt(digits)) / den).toString().padStart(digits, '0').replace(/0+$/, '') : '';
  return frac ? `${grouped(whole)}.${frac}` : grouped(whole);
}

/** The price of one whole unit of the base in whole units of the quote, from two amounts in base units and their decimals */
export function unitPrice(base: bigint, quote: bigint, baseDecimals: number, quoteDecimals: number, significant = 6): string | null {
  if (base <= 0n || quote < 0n) return null;
  return decimalRatio(quote * 10n ** BigInt(baseDecimals), base * 10n ** BigInt(quoteDecimals), significant);
}

/** Basis points as a percentage without the sign: 50 → "0.5", 1234 → "12.34" */
export function bpsText(bps: bigint): string {
  if (bps < 0n) return `-${bpsText(-bps)}`;
  const frac = (bps % 100n).toString().padStart(2, '0').replace(/0+$/, '');
  return frac ? `${bps / 100n}.${frac}` : `${bps / 100n}`;
}

/** `part / whole` as a percentage with at most two decimals, rounded down; "< 0.01%" for a part too small to show */
export function percentText(part: bigint, whole: bigint): string {
  if (whole <= 0n || part <= 0n) return '0%';
  const bps = (part * BPS) / whole;
  return bps === 0n ? '< 0.01%' : `${bpsText(bps)}%`;
}

/** A percentage typed by a person (`0.5`, `1`, `2.25`, `0.5%`) in basis points; null unless it is 0 to `maxBps` with at most two decimals */
export function parsePercent(text: string, maxBps = 5_000n): bigint | null {
  const m = /^\s*(\d{0,3})(?:\.(\d{0,2}))?\s*%?\s*$/.exec(text);
  if (!m) return null;
  const whole = m[1] ?? '';
  const frac = m[2] ?? '';
  if (!whole && !frac) return null;
  const bps = BigInt(whole || '0') * 100n + BigInt(frac.padEnd(2, '0'));
  return bps <= maxBps ? bps : null;
}
