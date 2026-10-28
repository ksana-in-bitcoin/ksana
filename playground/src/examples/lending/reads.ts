/** The lending contract's `market` and `position` views as typed records, checked field by field, and the assets' units. */
import { toHex, type BtcNetwork } from '@ksana/sdk';
import { accountText } from '../../abi/text';
import { isEnumValue, type Abi, type Value } from '../../abi/types';
import { formatUnits, type AssetMeta } from '../../chain/assets';
import type { Books, Params } from './math';

export interface Market extends Books {
  loanAsset: Uint8Array;
  collateralAsset: Uint8Array;
  /** null once the role has been given up: the price is frozen */
  admin: Uint8Array | null;
  /** Loan base units per collateral base unit, times WAD */
  price: bigint;
  params: Params;
  totalCollateral: bigint;
  borrowIndex: bigint;
  lastAccrual: bigint;
  utilizationBps: bigint;
  borrowRateBps: bigint;
  supplyRateBps: bigint;
}

export interface Position {
  shares: bigint;
  /** What the shares are worth in the loan asset, rounded down */
  supplied: bigint;
  collateral: bigint;
  /** Rounded up */
  debt: bigint;
  borrowLimit: bigint;
  liquidationLimit: bigint;
  /** `liquidation_limit · 10 000 / debt`; null without debt */
  healthBps: bigint | null;
}

/** An asset as the page writes its amounts */
export interface Unit {
  hex: string;
  bytes: Uint8Array;
  name: string;
  decimals: number;
}

/** What every panel of the page works from: the contract, its market as last read, the two assets, and who is looking */
export interface MarketContext {
  contract: string;
  abi: Abi;
  market: Market;
  loan: Unit;
  coll: Unit;
  /** The connected wallet's account */
  me: Uint8Array | null;
  network: BtcNetwork | undefined;
}

type Fields = { [field: string]: Value };

function record(v: Value | null | undefined): Fields | null {
  if (v === null || v === undefined || typeof v !== 'object' || Array.isArray(v) || v instanceof Uint8Array || isEnumValue(v)) return null;
  return v as Fields;
}

function int(r: Fields, name: string): bigint | null {
  const v = r[name];
  return typeof v === 'bigint' && v >= 0n ? v : null;
}

function id(v: Value | undefined): Uint8Array | null {
  return v instanceof Uint8Array && v.length === 32 ? v : null;
}

function ints<K extends string>(r: Fields, names: readonly K[]): Record<K, bigint> | null {
  const out = {} as Record<K, bigint>;
  for (const name of names) {
    const v = int(r, name);
    if (v === null) return null;
    out[name] = v;
  }
  return out;
}

const PARAMS = [
  'collateral_factor_bps', 'liquidation_threshold_bps', 'liquidation_bonus_bps', 'reserve_factor_bps', 'base_rate_bps', 'slope1_bps', 'kink_bps',
  'slope2_bps',
] as const;
const MARKET = [
  'price', 'cash', 'total_borrows', 'reserves', 'total_shares', 'total_collateral', 'borrow_index', 'last_accrual', 'utilization_bps',
  'borrow_rate_bps', 'supply_rate_bps',
] as const;
const POSITION = ['shares', 'supplied', 'collateral', 'debt', 'borrow_limit', 'liquidation_limit'] as const;

/** What `market()` returned, or null when it is not the lending contract's `Market` */
export function readMarket(v: Value | null): Market | null {
  const r = record(v);
  if (!r) return null;
  const p = ints(record(r.params) ?? {}, PARAMS);
  const n = ints(r, MARKET);
  const loanAsset = id(r.loan_asset);
  const collateralAsset = id(r.collateral_asset);
  const admin = r.admin === null ? null : id(r.admin);
  if (!p || !n || !loanAsset || !collateralAsset || (r.admin !== null && !admin) || p.kink_bps === 0n) return null;
  return {
    loanAsset,
    collateralAsset,
    admin,
    price: n.price,
    params: {
      collateralFactorBps: p.collateral_factor_bps,
      liquidationThresholdBps: p.liquidation_threshold_bps,
      liquidationBonusBps: p.liquidation_bonus_bps,
      reserveFactorBps: p.reserve_factor_bps,
      baseRateBps: p.base_rate_bps,
      slope1Bps: p.slope1_bps,
      kinkBps: p.kink_bps,
      slope2Bps: p.slope2_bps,
    },
    cash: n.cash,
    totalBorrows: n.total_borrows,
    reserves: n.reserves,
    totalShares: n.total_shares,
    totalCollateral: n.total_collateral,
    borrowIndex: n.borrow_index,
    lastAccrual: n.last_accrual,
    utilizationBps: n.utilization_bps,
    borrowRateBps: n.borrow_rate_bps,
    supplyRateBps: n.supply_rate_bps,
  };
}

/** What `position(account)` returned, or null when it is not a `Position` */
export function readPosition(v: Value | null): Position | null {
  const r = record(v);
  if (!r) return null;
  const n = ints(r, POSITION);
  const health = r.health_bps;
  if (!n || (health !== null && typeof health !== 'bigint')) return null;
  return {
    shares: n.shares,
    supplied: n.supplied,
    collateral: n.collateral,
    debt: n.debt,
    borrowLimit: n.borrow_limit,
    liquidationLimit: n.liquidation_limit,
    healthBps: health,
  };
}

/** An asset's unit from its metadata */
export function unitOf(bytes: Uint8Array, meta: AssetMeta): Unit {
  return { hex: toHex(bytes), bytes, name: meta.name, decimals: meta.decimals };
}

/** An amount as text: "12.5 KSA" */
export function amountText(unit: Unit, value: bigint): string {
  return `${formatUnits(value, unit.decimals)} ${unit.name}`;
}

/** A number of supply shares as text: shares start one for one with the loan asset's base units, so they read in its decimals */
export function sharesText(loan: Unit, shares: bigint): string {
  return formatUnits(shares, loan.decimals);
}

/** An account as a short address (a contract as its ID), for an operation's title */
export function shortAccount(account: Uint8Array, network: BtcNetwork | undefined, contract = false): string {
  const text = accountText(account, network, contract);
  return text.length > 20 ? `${text.slice(0, 10)}…${text.slice(-6)}` : text;
}
