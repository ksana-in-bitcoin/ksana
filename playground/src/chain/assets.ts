/** Names and decimals of the built-in assets the pages show, read once per asset from the node (`ksana_getAsset`). */
import { useEffect, useState } from 'react';
import { RPC_ERROR_CODES, RpcError, toHex } from '@ksana/sdk';
import type { Connection } from './connection';

export interface AssetMeta {
  id: string;
  name: string;
  decimals: number;
  /** null: the node has no such asset yet (nothing issued) */
  totalSupply: bigint | null;
  issuer: string | null;
}

export const KSA_HEX = '00'.repeat(32);
export const KSA_META: AssetMeta = { id: KSA_HEX, name: 'KSA', decimals: 8, totalSupply: null, issuer: null };

const cache = new Map<string, Promise<AssetMeta>>();

export function assetMeta(conn: Connection, asset: string | Uint8Array): Promise<AssetMeta> {
  const id = typeof asset === 'string' ? asset.toLowerCase() : toHex(asset);
  if (id === KSA_HEX) return Promise.resolve(KSA_META);
  const key = `${conn.endpoint}:${id}`;
  let hit = cache.get(key);
  if (!hit) {
    hit = conn.node.getAsset(id).then(
      (info) => ({ id, name: info.name, decimals: info.decimals, totalSupply: info.total_supply, issuer: toHex(info.issuer) }),
      (e: unknown) => {
        cache.delete(key);
        if (e instanceof RpcError && e.code === RPC_ERROR_CODES.NOT_FOUND) {
          return { id, name: `${id.slice(0, 8)}…`, decimals: 0, totalSupply: null, issuer: null };
        }
        throw e;
      },
    );
    cache.set(key, hit);
  }
  return hit;
}

/** Forgets what is known of an asset (its supply changed) */
export function forgetAsset(conn: Connection, asset: string): void {
  cache.delete(`${conn.endpoint}:${asset.toLowerCase()}`);
}

/** The asset's metadata, or null while it loads */
export function useAssetMeta(conn: Connection | null, asset: string | null, refresh = 0): AssetMeta | null {
  const [meta, setMeta] = useState<AssetMeta | null>(asset === KSA_HEX ? KSA_META : null);
  useEffect(() => {
    let alive = true;
    if (!conn || !asset || !/^[0-9a-f]{64}$/.test(asset)) {
      setMeta(null);
      return;
    }
    if (refresh) forgetAsset(conn, asset);
    assetMeta(conn, asset).then((m) => { if (alive) setMeta(m); }, () => { if (alive) setMeta(null); });
    return () => { alive = false; };
  }, [conn, asset, refresh]);
  return meta;
}

/** An amount in base units as a decimal with the asset's decimals (trailing zeros dropped) */
export function formatUnits(amount: bigint, decimals: number, maxFraction = decimals): string {
  const negative = amount < 0n;
  let v = negative ? -amount : amount;
  const base = 10n ** BigInt(decimals);
  const whole = v / base;
  v %= base;
  let frac = decimals > 0 ? v.toString().padStart(decimals, '0').slice(0, Math.max(0, maxFraction)) : '';
  frac = frac.replace(/0+$/, '');
  const wholeText = whole.toString().replace(/\B(?=(\d{3})+(?!\d))/g, ',');
  return `${negative ? '-' : ''}${wholeText}${frac ? `.${frac}` : ''}`;
}

/** Why a typed amount is not one (`errorText` says it in the page's language) */
export class AmountError extends Error {
  constructor(readonly problem: 'notAmount' | 'decimals' | 'tooLarge', readonly decimals: number) {
    super(problem === 'notAmount' ? 'enter an amount' : problem === 'decimals' ? `at most ${decimals} decimal places` : 'amount too large');
    this.name = 'AmountError';
  }
}

/** A decimal amount typed by a person in base units; throws an `AmountError` on more fraction digits than the asset has */
export function parseUnits(text: string, decimals: number): bigint {
  const t = text.trim().replace(/[,_\s]/g, '');
  const m = /^(\d*)(?:\.(\d*))?$/.exec(t);
  if (!m || (m[1] === '' && (m[2] ?? '') === '')) throw new AmountError('notAmount', decimals);
  const frac = m[2] ?? '';
  if (frac.length > decimals) throw new AmountError('decimals', decimals);
  const value = BigInt(m[1] || '0') * 10n ** BigInt(decimals) + BigInt(frac.padEnd(decimals, '0') || '0');
  if (value >= 1n << 128n) throw new AmountError('tooLarge', decimals);
  return value;
}
