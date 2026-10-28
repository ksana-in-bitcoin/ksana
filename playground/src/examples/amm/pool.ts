/** The pool as the page reads it: its two sides with their names and decimals, the reserves and the shares, and amounts as text. */
import { toHex } from '@ksana/sdk';
import { isEnumValue, type Value } from '../../abi/types';
import type { AccountView } from '../../chain/account';
import { formatUnits, type AssetMeta } from '../../chain/assets';

export interface Side {
  /** `Asset`: a built-in asset, attached to calls; `Token`: a token contract, sent with `transfer_and_call` or pulled with `transfer_from` */
  kind: 'Asset' | 'Token';
  /** The asset ID or the token contract's ID */
  id: Uint8Array;
  hex: string;
}

export interface PoolSide extends Side {
  name: string;
  /** null while the asset's or the token's metadata loads */
  decimals: number | null;
  /** A token contract without metadata: its amounts are shown in its base units */
  baseUnits: boolean;
  /** What the connected wallet holds of it; null without a wallet or while it loads */
  held: bigint | null;
  /** A token side: what the pool may pull from the wallet (the token's `allowance`); null for an asset side, without a wallet or while it loads */
  allowance: bigint | null;
  /** A token side that declares KS-20: it pays the pool in one operation with its `transfer_and_call` */
  ks20: boolean;
}

export interface Pool {
  a: PoolSide;
  b: PoolSide;
  reserveA: bigint;
  reserveB: bigint;
  totalShares: bigint;
}

/** A decoded `Side`, or null when the value is not one */
export function sideOf(value: Value | undefined): Side | null {
  if (value === undefined || value === null || !isEnumValue(value)) return null;
  const id = value.values[0];
  if ((value.variant !== 'Asset' && value.variant !== 'Token') || !(id instanceof Uint8Array) || id.length !== 32) return null;
  return { kind: value.variant, id, hex: toHex(id) };
}

/** What `tokens()` returns: the two sides */
export function pairOf(value: Value | null): [Side, Side] | null {
  if (!Array.isArray(value) || value.length !== 2) return null;
  const a = sideOf(value[0]);
  const b = sideOf(value[1]);
  return a && b ? [a, b] : null;
}

/** What `reserves()` returns */
export function reservesOf(value: Value | null): [bigint, bigint] | null {
  if (!Array.isArray(value) || value.length !== 2) return null;
  const [a, b] = value;
  return typeof a === 'bigint' && typeof b === 'bigint' ? [a, b] : null;
}

/** What the page knows of a side besides its kind and ID */
export interface SideFacts {
  /** An asset side's metadata from the node */
  meta: AssetMeta | null;
  /** A token side: its metadata (symbol and decimals), null when it has none; undefined while the page reads it */
  token?: { symbol: string; decimals: number } | null;
  /** A token side that declares KS-20 */
  ks20?: boolean;
  held: bigint | null;
  allowance: bigint | null;
}

/** A side with its name and decimals: an asset's from the node, a token contract's from its metadata, or its short ID in base units */
export function poolSide(side: Side, facts: SideFacts, tokenName: (id: string) => string): PoolSide {
  const short = `${side.hex.slice(0, 8)}…`;
  const { held, allowance } = facts;
  if (side.kind === 'Token') {
    const ks20 = facts.ks20 ?? false;
    if (facts.token === undefined) return { ...side, name: tokenName(short), decimals: null, baseUnits: false, held, allowance, ks20 };
    if (facts.token === null) return { ...side, name: tokenName(short), decimals: 0, baseUnits: true, held, allowance, ks20 };
    return { ...side, name: facts.token.symbol || tokenName(short), decimals: facts.token.decimals, baseUnits: false, held, allowance, ks20 };
  }
  const { meta } = facts;
  return { ...side, name: meta?.name ?? short, decimals: meta ? meta.decimals : null, baseUnits: false, held, allowance: null, ks20: false };
}

/** An amount of a side in its units, without the name; … while its decimals load */
export function numberText(side: PoolSide, value: bigint): string {
  return side.decimals === null ? '…' : formatUnits(value, side.decimals);
}

/** An amount of a side in its units with its name */
export function amountText(side: PoolSide, value: bigint): string {
  return side.decimals === null ? '…' : `${formatUnits(value, side.decimals)} ${side.name}`;
}

/** An amount as an input holds it: in units, without the thousands commas */
export function inputText(value: bigint, decimals: number): string {
  return formatUnits(value, decimals).replace(/,/g, '');
}

/** The connected account's balance of a built-in asset side; null while it loads, without a wallet, or for a token contract */
export function balanceOf(account: AccountView | null, side: Side): bigint | null {
  if (!account || side.kind !== 'Asset') return null;
  return account.balances.get(side.hex) ?? 0n;
}
