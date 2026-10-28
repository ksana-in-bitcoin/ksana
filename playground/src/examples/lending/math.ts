/**
 * The lending contract's arithmetic (contracts/examples/lending) in bigint, with its rounding: the price in tokens, the rate curve,
 * the limits and the health of a position, the amount ⇄ shares conversion, the liquidation preview, and the interest a debt adds
 * while an operation waits for its block.
 */
import { formatUnits } from '../../chain/assets';

/** 10^18: the fixed point of the price and of the borrow index */
export const WAD = 10n ** 18n;
/** 100% in basis points */
export const BPS = 10_000n;
/** The year the rates are given per, in seconds */
export const SECONDS_PER_YEAR = 31_536_000n;
/** The largest u128: the contract reverts where a result it computes goes above it */
export const U128_MAX = (1n << 128n) - 1n;
/** How far ahead the page counts a debt's interest when it fills in an amount: a debt grows every second until the block */
export const INTEREST_MARGIN_SECONDS = 3_600n;
/** A position is near its limit when a fall of less than this in the collateral's price makes it liquidatable */
export const NEAR_DROP_BPS = 1_000n;

/** The market's parameters, fixed at `init` */
export interface Params {
  collateralFactorBps: bigint;
  liquidationThresholdBps: bigint;
  liquidationBonusBps: bigint;
  reserveFactorBps: bigint;
  baseRateBps: bigint;
  slope1Bps: bigint;
  kinkBps: bigint;
  slope2Bps: bigint;
}

/** The market's books as `market` returns them, with interest accrued to the block's time */
export interface Books {
  cash: bigint;
  totalBorrows: bigint;
  reserves: bigint;
  totalShares: bigint;
}

function min(a: bigint, b: bigint): bigint {
  return a < b ? a : b;
}

function ceilDiv(n: bigint, d: bigint): bigint {
  return n % d === 0n ? n / d : n / d + 1n;
}

/** `floor(a · b / c)`, exact like the contract's `mul_div`; null when `c` is 0 or the quotient does not fit in u128 */
export function mulDiv(a: bigint, b: bigint, c: bigint): bigint | null {
  if (c <= 0n) return null;
  const q = (a * b) / c;
  return q <= U128_MAX ? q : null;
}

/** `ceil(a · b / c)`, like `mul_div_up` */
export function mulDivUp(a: bigint, b: bigint, c: bigint): bigint | null {
  if (c <= 0n) return null;
  const q = ceilDiv(a * b, c);
  return q <= U128_MAX ? q : null;
}

// ---------------------------------------------------------------------------
// The market

/** The suppliers' assets, `cash + total_borrows − reserves` (0 when that is negative): what all the shares are worth */
export function totalAssets(b: Books): bigint {
  const v = b.cash + b.totalBorrows - b.reserves;
  return v > 0n ? v : 0n;
}

/** The cash neither lent nor reserved, `cash − reserves`: the most a withdrawal or a borrow can take */
export function liquidity(b: Books): bigint {
  return b.cash > b.reserves ? b.cash - b.reserves : 0n;
}

/** `total_borrows · 10 000 / total_assets` rounded down: 0 without assets, at most 10 000 */
export function utilizationBps(b: Books): bigint {
  const assets = totalAssets(b);
  if (assets === 0n) return 0n;
  return min((b.totalBorrows * BPS) / assets, BPS);
}

/**
 * The borrow rate at a utilization, in basis points a year: `base + slope1 · min(U, kink) / kink + slope2 · (U − kink) /
 * (10 000 − kink)`, each term rounded down and the last only above the kink. Null for a kink of 0, which `init` refuses.
 */
export function borrowRateBps(p: Params, utilization: bigint): bigint | null {
  const first = mulDiv(p.slope1Bps, min(utilization, p.kinkBps), p.kinkBps);
  if (first === null) return null;
  let second = 0n;
  if (utilization > p.kinkBps) {
    const span = mulDiv(p.slope2Bps, utilization - p.kinkBps, BPS - p.kinkBps);
    if (span === null) return null;
    second = span;
  }
  return p.baseRateBps + first + second;
}

/** The rate suppliers earn: `borrow_rate · U · (10 000 − reserve_factor) / 10 000 / 10 000`, rounded down at each step */
export function supplyRateBps(borrowRate: bigint, utilization: bigint, reserveFactorBps: bigint): bigint {
  return (borrowRate * utilization * (BPS - reserveFactorBps)) / BPS / BPS;
}

/** The corners of the kinked rate line: no use, the kink, full use (the last two are one point when the kink is at 100%) */
export function rateCorners(p: Params): { utilization: bigint; rate: bigint }[] {
  const at = (u: bigint) => ({ utilization: u, rate: borrowRateBps(p, u) ?? p.baseRateBps });
  return p.kinkBps >= BPS ? [at(0n), at(BPS)] : [at(0n), at(p.kinkBps), at(BPS)];
}

/**
 * The borrow index after `seconds` at `rateBps` a year, accrued as the contract does it: `index + index · rate_per_second ·
 * seconds / WAD` with `rate_per_second = rate · WAD / 10 000 / 31 536 000`, each rounded down.
 */
export function indexAfter(index: bigint, rateBps: bigint, seconds: bigint): bigint {
  if (seconds <= 0n) return index;
  const perSecond = (rateBps * WAD) / BPS / SECONDS_PER_YEAR;
  return index + (index * perSecond * seconds) / WAD;
}

/** A debt carried from borrow index `index` to `next`: `debt · next / index`, rounded up as the contract rounds debts */
export function grownDebt(debt: bigint, index: bigint, next: bigint): bigint {
  return index > 0n ? ceilDiv(debt * next, index) : debt;
}

/**
 * The debt an hour from now at the current borrow rate, one unit more for the rounding: what to attach to repay all of it, and
 * what to keep room for under the borrow limit, since the operation lands a few blocks after the page read the debt.
 */
export function debtWithMargin(debt: bigint, index: bigint, rateBps: bigint): bigint {
  if (debt === 0n) return 0n;
  return grownDebt(debt, index, indexAfter(index, rateBps, INTEREST_MARGIN_SECONDS)) + 1n;
}

// ---------------------------------------------------------------------------
// Positions

/** What collateral is worth in the loan asset: `collateral · price / WAD` rounded down, and u128::MAX above it */
export function collateralValue(collateral: bigint, price: bigint): bigint {
  return mulDiv(collateral, price, WAD) ?? U128_MAX;
}

export interface Limits {
  /** The most the account may owe after a `borrow` or a `withdraw_collateral` */
  borrowLimit: bigint;
  /** The debt above which anyone else can liquidate the account */
  liquidationLimit: bigint;
}

/** The borrow limit and the liquidation limit of `collateral`: its value times the collateral factor and the threshold, rounded down */
export function limitsOf(collateral: bigint, price: bigint, p: Params): Limits {
  const value = collateralValue(collateral, price);
  return {
    borrowLimit: (value * p.collateralFactorBps) / BPS,
    liquidationLimit: (value * p.liquidationThresholdBps) / BPS,
  };
}

/** `liquidation_limit · 10 000 / debt` rounded down (null without debt, u128::MAX above it): below 10 000 it can be liquidated */
export function healthBps(liquidationLimit: bigint, debt: bigint): bigint | null {
  if (debt === 0n) return null;
  return mulDiv(liquidationLimit, BPS, debt) ?? U128_MAX;
}

/** How far the collateral's price can fall before the position can be liquidated: `1 − 1 / health` in basis points, rounded down */
export function priceDropBps(health: bigint): bigint {
  return health > BPS ? ((health - BPS) * BPS) / health : 0n;
}

export type HealthState = 'none' | 'safe' | 'near' | 'liquidatable';

/** No debt; safe; near the limit (a fall of less than 10% in the price makes it liquidatable); liquidatable (debt above the limit) */
export function healthState(debt: bigint, liquidationLimit: bigint): HealthState {
  if (debt === 0n) return 'none';
  if (debt > liquidationLimit) return 'liquidatable';
  return priceDropBps(healthBps(liquidationLimit, debt) ?? U128_MAX) < NEAR_DROP_BPS ? 'near' : 'safe';
}

/** The debt as a share of the liquidation limit, in basis points rounded up and at most 10 000 (a full bar can be liquidated) */
export function limitUsedBps(debt: bigint, liquidationLimit: bigint): bigint {
  if (debt === 0n) return 0n;
  if (liquidationLimit === 0n) return BPS;
  return min(ceilDiv(debt * BPS, liquidationLimit), BPS);
}

/**
 * The least collateral whose borrow limit covers `debt` at `price` (0 for no debt): the borrow limit is
 * `floor(floor(c · price / WAD) · factor / 10 000)`, so `c = ceil(ceil(debt · 10 000 / factor) · WAD / price)`. Null when no
 * amount does.
 */
export function minCollateralFor(debt: bigint, price: bigint, collateralFactorBps: bigint): bigint | null {
  if (debt === 0n) return 0n;
  if (price <= 0n || collateralFactorBps <= 0n) return null;
  const value = ceilDiv(debt * BPS, collateralFactorBps);
  if (value > U128_MAX) return null;
  const collateral = ceilDiv(value * WAD, price);
  return collateral <= U128_MAX ? collateral : null;
}

/** The most collateral `withdraw_collateral` lets go of while `debt` stays within the borrow limit of what is left */
export function freeCollateral(collateral: bigint, debt: bigint, price: bigint, collateralFactorBps: bigint): bigint {
  const needed = minCollateralFor(debt, price, collateralFactorBps);
  if (needed === null || needed >= collateral) return 0n;
  return collateral - needed;
}

/** The most `borrow` lends: what the borrow limit leaves above `debt`, and no more than the market has available */
export function borrowRoom(borrowLimit: bigint, debt: bigint, available: bigint): bigint {
  return borrowLimit > debt ? min(borrowLimit - debt, available) : 0n;
}

// ---------------------------------------------------------------------------
// Supplying and withdrawing

/**
 * The shares `supply(amount)` credits: the amount itself for the first shares, else `amount · total_shares / total_assets` rounded
 * down; `insolvent` when there are shares but no assets, `tooSmall` when it comes to no share (the contract's reverts).
 */
export function supplyShares(amount: bigint, b: Books): bigint | 'insolvent' | 'tooSmall' {
  let shares = amount;
  if (b.totalShares > 0n) {
    const assets = totalAssets(b);
    if (assets === 0n) return 'insolvent';
    shares = (amount * b.totalShares) / assets;
  }
  return shares > 0n ? shares : 'tooSmall';
}

/** What `withdraw(shares)` pays: `shares · total_assets / total_shares`, rounded down (0 without shares) */
export function sharesValue(shares: bigint, b: Books): bigint {
  if (b.totalShares === 0n) return 0n;
  return (shares * totalAssets(b)) / b.totalShares;
}

/** The fewest shares that pay at least `amount` (rounded up), and no more than the `held` ones: an amount typed to withdraw */
export function sharesForAmount(amount: bigint, held: bigint, b: Books): bigint {
  const assets = totalAssets(b);
  if (assets === 0n || b.totalShares === 0n) return 0n;
  return min(ceilDiv(amount * b.totalShares, assets), held);
}

/** The shares a withdrawal of the most takes: all of them when the market's liquidity pays them out, else the most it covers */
export function maxWithdrawShares(held: bigint, b: Books): bigint {
  const assets = totalAssets(b);
  if (assets === 0n) return 0n;
  const available = liquidity(b);
  if (sharesValue(held, b) <= available) return held;
  return min((available * b.totalShares) / assets, held);
}

export type WithdrawProblem = 'zero' | 'shares' | 'insolvent' | 'tooSmall' | 'liquidity';

/** Why `withdraw(shares)` would revert now, checked in the contract's order, or null when it pays out */
export function withdrawProblem(shares: bigint, held: bigint, b: Books): WithdrawProblem | null {
  if (shares === 0n) return 'zero';
  if (shares > held) return 'shares';
  if (totalAssets(b) === 0n) return 'insolvent';
  const amount = sharesValue(shares, b);
  if (amount === 0n) return 'tooSmall';
  return amount > liquidity(b) ? 'liquidity' : null;
}

// ---------------------------------------------------------------------------
// Liquidations

/** The most one liquidation repays: half the debt, rounded up */
export function closeLimit(debt: bigint): bigint {
  return (debt + 1n) / 2n;
}

export interface LiquidationQuote {
  /** Taken from the attached amount: at most half the debt */
  repaid: bigint;
  /** The borrower's collateral the liquidator receives */
  seized: bigint;
  /** What comes back of the attached amount */
  refund: bigint;
  /** The rest of the debt, when the seizure takes all the collateral: the suppliers bear it */
  writtenOff: bigint;
  debtLeft: bigint;
  collateralLeft: bigint;
  /** The bonus would take more than all the collateral: the liquidator still pays all of `repaid` */
  capped: boolean;
}

/**
 * What `liquidate` does with `attached` of the loan asset: `repaid = min(attached, ceil(debt / 2))`, and
 * `repaid · (10 000 + bonus) / 10 000 · WAD / price` of the collateral (rounded down at each step, at most all of it); when no
 * collateral is left the rest of the debt is written off. Null where the contract's arithmetic overflows.
 */
export function quoteLiquidation(attached: bigint, debt: bigint, collateral: bigint, price: bigint, bonusBps: bigint): LiquidationQuote | null {
  const repaid = min(attached, closeLimit(debt));
  const value = mulDiv(repaid, BPS + bonusBps, BPS);
  if (value === null) return null;
  const uncapped = mulDiv(value, WAD, price);
  const capped = uncapped === null || uncapped > collateral;
  const seized = uncapped === null || uncapped > collateral ? collateral : uncapped;
  const collateralLeft = collateral - seized;
  const rest = debt - repaid;
  const writtenOff = collateralLeft === 0n ? rest : 0n;
  return { repaid, seized, refund: attached - repaid, writtenOff, debtLeft: rest - writtenOff, collateralLeft, capped };
}

/**
 * The most worth repaying in one liquidation: the repayment whose seizure, bonus included, is all the collateral,
 * `collateral · price · 10 000 / (WAD · (10 000 + bonus))` rounded down. Past it the liquidator pays more for no more collateral.
 */
export function coveredRepay(collateral: bigint, price: bigint, bonusBps: bigint): bigint {
  return (collateral * price * BPS) / (WAD * (BPS + bonusBps));
}

// ---------------------------------------------------------------------------
// The price in tokens

/**
 * The decimal places of a price in tokens (loan tokens per collateral token) that the contract's price holds exactly:
 * `18 + loan decimals − collateral decimals`. Negative only when the collateral has more than 18 decimals more than the loan asset.
 */
export function priceDecimals(loanDecimals: number, collateralDecimals: number): number {
  return 18 + loanDecimals - collateralDecimals;
}

/** The contract's price (loan base units per collateral base unit, times WAD) as loan tokens per collateral token, exactly */
export function priceText(price: bigint, loanDecimals: number, collateralDecimals: number): string {
  const e = priceDecimals(loanDecimals, collateralDecimals);
  return e >= 0 ? formatUnits(price, e) : formatUnits(price * 10n ** BigInt(-e), 0);
}

/**
 * A price in tokens typed with `max(priceDecimals, 0)` decimals (as a whole number of those steps) as the contract's price; null
 * when it is not one the contract can hold: 0, above u128, or (with a negative exponent) not a whole multiple.
 */
export function priceFromTyped(typed: bigint, loanDecimals: number, collateralDecimals: number): bigint | null {
  const e = priceDecimals(loanDecimals, collateralDecimals);
  let price = typed;
  if (e < 0) {
    const step = 10n ** BigInt(-e);
    if (typed % step !== 0n) return null;
    price = typed / step;
  }
  return price > 0n && price <= U128_MAX ? price : null;
}

// ---------------------------------------------------------------------------
// Text

/** Basis points as a percentage: 825 → "8.25%" */
export function percentText(bps: bigint): string {
  return `${formatUnits(bps, 2)}%`;
}

/** A health in basis points as a factor with two decimals, rounded down: 10 666 → "1.06", 9 600 → "0.96"; "100+" from 100 on */
export function healthText(bps: bigint): string {
  if (bps >= 100n * BPS) return '100+';
  return `${bps / BPS}.${((bps % BPS) / 100n).toString().padStart(2, '0')}`;
}
