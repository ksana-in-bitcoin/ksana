/**
 * A pool's sides as the node has them: a token-contract side is read through the token interface the pool takes it with, a KS-20
 * token (`docs/STANDARDS.md` §3) or a token written before KS-20 with the same `transfer_from`, and a side that names no such
 * thing, like a token side given an asset's ID or a KS-721 collection, is found before anyone tries to trade, or deploys such a
 * pool. A KS-20 side is paid in one operation with its `transfer_and_call`; a side the pool pulls is approved first.
 */
import { useEffect, useState } from 'react';
import { fromHex, toHex, type Action, type Funds } from '@ksana/sdk';
import { decodeReturn, decodeValue, encodeArgs, encodeValue } from '../../abi/codec';
import { parseAbi } from '../../abi/parse';
import { type Abi, type Field, type Value, isEnumValue } from '../../abi/types';
import { useConnection, useTip, type Connection } from '../../chain/connection';
import { assetInstead, assetProblem, declaredStandard, tokenProblem, whatIs, type IdProblem, type InitProblem } from '../../chain/ids';
import { useView } from '../../chain/views';
import { callAction } from '../kit';
import type { PoolSide, Side } from './pool';

/**
 * The part of a token's interface the page uses: what the pool may pull (`allowance`), granted with `approve` as tokens before
 * KS-20 take it (a KS-20 token takes it too, as one that never expires)
 */
export const TOKEN_ABI: Abi = parseAbi([
  '//! ```abi',
  '//! call approve(spender: ContractId, amount: u128)',
  '//! view balance_of(account: Account) -> u128',
  '//! view allowance(owner: Account, spender: ContractId) -> u128',
  '//! view metadata() -> (String, String, u8)',
  '//! ```',
].join('\n'))!;

/**
 * KS-20's own: `approve` with an expiry, and `transfer_and_call`, which moves the tokens to the pool and calls its
 * `on_token_received` with a `PoolAction` (the AMM's), passing on the assets attached
 */
export const KS20_ABI: Abi = parseAbi([
  '//! ```abi',
  '//! enum PoolAction { Swap(u128), AddLiquidity(u128, u128) }',
  '//! call approve(spender: ContractId, amount: u128, expires_at: Option<u64>)',
  '//! payable call transfer_and_call(to: ContractId, amount: u128, data: Bytes) -> Bytes',
  '//! ```',
].join('\n'))!;

/** How long an approval for the pool counts, as wallets approve one operation's amount with an expiry (`docs/STANDARDS.md` §3.5) */
export const APPROVAL_SECONDS = 7n * 86_400n;

function method(name: string) {
  return TOKEN_ABI.methods.find((m) => m.name === name)!;
}

export interface TokenMeta {
  name: string;
  symbol: string;
  decimals: number;
}

export type SideCheck =
  /**
   * `token`: a token side's metadata, null for an asset side and for a token that has none (its amounts are base units);
   * `ks20`: the token side declares KS-20, so it pays the pool with `transfer_and_call`
   */
  | { kind: 'ok'; token: TokenMeta | null; ks20: boolean }
  /** The side names no asset or token contract the pool can take */
  | { kind: 'broken'; problem: IdProblem };

/** Whether the pool can take the side: an asset the node knows, or a contract that answers as a token; throws when the node does not answer */
export async function checkSide(conn: Connection, side: Side, pool: string): Promise<SideCheck> {
  if (side.kind === 'Asset') {
    const problem = assetProblem(side.hex, await whatIs(conn, side.hex));
    return problem ? { kind: 'broken', problem } : { kind: 'ok', token: null, ks20: false };
  }
  const allowance = method('allowance');
  const probe = await conn.node.call({
    contract: side.hex, method: allowance.name, args: encodeArgs(TOKEN_ABI, allowance.params.map((p) => p.type), [fromHex(pool, 32), fromHex(pool, 32)]),
  });
  let answers = probe.success;
  if (answers) {
    try {
      answers = typeof decodeReturn(TOKEN_ABI, allowance.returns, probe.output) === 'bigint';
    } catch {
      answers = false;
    }
  }
  if (!answers) return { kind: 'broken', problem: tokenProblem(side.hex, await whatIs(conn, side.hex)) };
  const ks20 = (await declaredStandard(conn, side.hex)) === 'KS-20';
  const metadata = method('metadata');
  const md = await conn.node.call({ contract: side.hex, method: metadata.name, args: new Uint8Array() });
  if (!md.success) return { kind: 'ok', token: null, ks20 };
  try {
    const v = decodeReturn(TOKEN_ABI, metadata.returns, md.output);
    if (Array.isArray(v) && typeof v[0] === 'string' && typeof v[1] === 'string' && typeof v[2] === 'bigint') {
      return { kind: 'ok', token: { name: v[0], symbol: v[1], decimals: Number(v[2]) }, ks20 };
    }
  } catch {
    /* metadata in another shape: amounts in base units */
  }
  return { kind: 'ok', token: null, ks20 };
}

/** `checkSide` for the page: null while it runs, tried again at the next block when the node did not answer */
export function useSideCheck(side: Side | null, pool: string): SideCheck | null {
  const conn = useConnection();
  const tip = useTip();
  const [result, setResult] = useState<{ key: string; check: SideCheck } | null>(null);
  const key = side && conn ? `${conn.endpoint}:${side.kind}:${side.hex}:${pool}` : '';
  const settled = result?.key === key;
  useEffect(() => {
    if (!side || !conn || settled) return;
    let alive = true;
    checkSide(conn, side, pool).then((check) => { if (alive) setResult({ key, check }); }, () => { /* the next block tries again */ });
    return () => { alive = false; };
  }, [key, settled, tip?.tip.height]);
  return settled ? result.check : null;
}

/** The connected account's balance of a token side and what the pool may pull of it; nulls for an asset side, without a wallet or while they load */
export function useTokenHoldings(side: Side | null, check: SideCheck | null, pool: string, me: Uint8Array | null): { held: bigint | null; allowance: bigint | null } {
  const enabled = !!me && side?.kind === 'Token' && check?.kind === 'ok';
  const token = side?.hex ?? null;
  const held = useView<bigint>(token, TOKEN_ABI, 'balance_of', me ? [me] : [], { enabled });
  const allowance = useView<bigint>(token, TOKEN_ABI, 'allowance', me ? [me, fromHex(pool, 32)] : [], { enabled });
  return {
    held: enabled && typeof held.data === 'bigint' ? held.data : null,
    allowance: enabled && typeof allowance.data === 'bigint' ? allowance.data : null,
  };
}

/**
 * The `approve` that lets the pool pull `amount` of a token side with `transfer_from`: a KS-20 token's until `expiresAt` (the
 * chain's time plus {@link APPROVAL_SECONDS}), a token before KS-20's without an expiry, which it does not take
 */
export function approveAction(side: PoolSide, pool: string, amount: bigint, expiresAt: bigint): Action {
  return side.ks20 ? callAction(KS20_ABI, side.hex, 'approve', [fromHex(pool, 32), amount, expiresAt])
    : callAction(TOKEN_ABI, side.hex, 'approve', [fromHex(pool, 32), amount]);
}

/** How much a token side must be approved for before the pool can pull `amount` of it; null when nothing is missing or it is not known yet */
export function approvalNeeded(side: PoolSide, amount: bigint | null): bigint | null {
  if (side.kind !== 'Token' || amount === null || amount <= 0n || side.allowance === null) return null;
  return side.allowance < amount ? amount : null;
}

/** The funds a call attaches: the asset sides' amounts; a token side is pulled by the pool or sent by its own token instead, and attaching it reverts */
export function attached(parts: [PoolSide, bigint][]): Funds[] {
  return parts.filter(([side, amount]) => side.kind === 'Asset' && amount > 0n).map(([side, amount]) => ({ asset: side.id, amount }));
}

/** The pool's `PoolAction`: a swap with its minimum out, or a deposit with the other side's amount and the minimum shares */
export type PoolAction = { variant: 'Swap'; values: [bigint] } | { variant: 'AddLiquidity'; values: [bigint, bigint] };

/**
 * `transfer_and_call(pool, amount, action)` on a KS-20 side's token: it moves `amount` to the pool and calls the pool's
 * `on_token_received` with the action, attaching `funds` (the other side of a deposit, when it is a built-in asset)
 */
export function sendAction(side: PoolSide, pool: string, amount: bigint, action: PoolAction, funds: Funds[] = []): Action {
  const data = encodeValue(KS20_ABI, { kind: 'enum', name: 'PoolAction' }, action);
  return callAction(KS20_ABI, side.hex, 'transfer_and_call', [fromHex(pool, 32), amount, data], funds);
}

/** The `u128` in a pool's answer to `transfer_and_call` (`Bytes` holding the amount paid out or the shares); null when it is not one */
export function answerOf(output: Uint8Array): bigint | null {
  try {
    const bytes = decodeValue(KS20_ABI, { kind: 'bytes' }, output);
    if (!(bytes instanceof Uint8Array)) return null;
    const value = decodeValue(KS20_ABI, { kind: 'int', name: 'u128' }, bytes);
    return typeof value === 'bigint' ? value : null;
  } catch {
    return null;
  }
}

/** A side as `init` takes it */
export function sideValue(kind: 'Asset' | 'Token', id: Uint8Array): Value {
  return { variant: kind, values: [id] };
}

/**
 * The AMM's own check of `init`'s arguments: a `Token` side must name a contract that answers as a token, and when its ID leads
 * to an asset the side is mended to take that asset (an `Asset` side's ID is checked as every asset ID is)
 */
export async function ammInitProblems(conn: Connection, _abi: Abi, params: Field[], values: Value[]): Promise<InitProblem[]> {
  const out: InitProblem[] = [];
  for (const [i, p] of params.entries()) {
    const value = values[i] ?? null;
    if (p.type.kind !== 'enum' || value === null || !isEnumValue(value) || value.variant !== 'Token') continue;
    const id = value.values[0];
    if (!(id instanceof Uint8Array) || id.length !== 32) continue;
    const hex = toHex(id);
    const check = await checkSide(conn, { kind: 'Token', id, hex }, hex);
    if (check.kind !== 'broken') continue;
    const instead = assetInstead(check.problem);
    out.push({ path: p.name, problem: check.problem, fix: instead ? { param: i, value: sideValue('Asset', fromHex(instead.id, 32)), as: 'asset' } : null });
  }
  return out;
}
