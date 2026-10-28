/**
 * What a 32-byte ID names on the node: a built-in asset, a contract (a KS-20 token or a KS-721 collection among them), or
 * nothing. Asset IDs and contract IDs look the same, and an argument that takes one is easily given the other: the asset
 * example's contract issues an asset with an ID of its own, a pool whose token side names a contract by that asset's ID can
 * never pull it, and a KS-20 token is no built-in asset, so it cannot be attached to calls.
 */
import { useEffect, useState } from 'react';
import { fromHex, toHex } from '@ksana/sdk';
import { decodeReturn } from '../abi/codec';
import { parseAbi } from '../abi/parse';
import { type Abi, type AbiType, type Field, type Value, isEnumValue } from '../abi/types';
import { KSA_HEX, KSA_META, assetMeta, type AssetMeta } from './assets';
import { useConnection, useTip, type Connection } from './connection';
import { failureText } from './views';

/** The node's reason for a call of a contract it does not have (the VM's own words) */
export const NO_CONTRACT = 'contract does not exist';

/** The token standards a contract can declare (`docs/STANDARDS.md` §2) */
export type TokenStandard = 'KS-20' | 'KS-721';

export type Named =
  | { kind: 'asset'; meta: AssetMeta }
  /**
   * `issued`: the asset the contract says it issues, through an `asset()` view like the asset example's; null when it says none.
   * `standard`: the token standard it declares with `standards()`; null when it declares none
   */
  | { kind: 'contract'; issued: AssetMeta | null; standard: TokenStandard | null }
  | { kind: 'nothing' };

/** The view the asset example answers with the ID of the asset it issues */
const ISSUER: Abi = parseAbi('//! ```abi\n//! view asset() -> AssetId\n//! ```')!;

/** The view a token declares its standard with, first in the list (`docs/STANDARDS.md` §2) */
const STANDARDS: Abi = parseAbi('//! ```abi\n//! view standards() -> Vec<String>\n//! ```')!;

/** The token standard a contract declares, the first ID its `standards()` returns; null when it declares none; throws when the node does not answer */
export async function declaredStandard(conn: Connection, contract: string): Promise<TokenStandard | null> {
  const receipt = await conn.node.call({ contract, method: 'standards', args: new Uint8Array() });
  if (!receipt.success) return null;
  try {
    const ids = decodeReturn(STANDARDS, STANDARDS.methods[0]!.returns, receipt.output);
    const first = Array.isArray(ids) ? ids[0] : undefined;
    return first === 'KS-20' || first === 'KS-721' ? first : null;
  } catch {
    return null;
  }
}

/** Whether the node knows the asset: KSA, or one some contract issued (an unknown ID reads as a placeholder without an issuer) */
export function knownAsset(meta: AssetMeta): boolean {
  return meta.id === KSA_HEX || meta.issuer !== null;
}

/** What `id` (64 hex digits) names; throws when the node does not answer */
export async function whatIs(conn: Connection, id: string): Promise<Named> {
  if (id === KSA_HEX) return { kind: 'asset', meta: KSA_META };
  const meta = await assetMeta(conn, id);
  if (knownAsset(meta)) return { kind: 'asset', meta };
  const receipt = await conn.node.call({ contract: id, method: 'asset', args: new Uint8Array() });
  if (!receipt.success && failureText(receipt.error) === NO_CONTRACT) return { kind: 'nothing' };
  let issued: AssetMeta | null = null;
  if (receipt.success) {
    try {
      const asset = decodeReturn(ISSUER, ISSUER.methods[0]!.returns, receipt.output);
      if (asset instanceof Uint8Array) {
        const info = await assetMeta(conn, toHex(asset));
        if (knownAsset(info)) issued = info;
      }
    } catch {
      /* an `asset` method that answers something else: no asset to suggest */
    }
  }
  return { kind: 'contract', issued, standard: await declaredStandard(conn, id) };
}

export interface IdProblem {
  /**
   * `assetIsContract` / `assetIsToken` / `assetUnknown`: taken as a built-in asset, the ID names a contract, a KS-20 token, or
   * nothing; `tokenIsAsset` / `tokenIsNft` / `notToken` / `tokenMissing`: taken as a token contract, it names a built-in asset, a
   * KS-721 collection, a contract that does not answer as a token, or nothing
   */
  kind: 'assetIsContract' | 'assetIsToken' | 'assetUnknown' | 'tokenIsAsset' | 'tokenIsNft' | 'notToken' | 'tokenMissing';
  id: string;
  /** `tokenIsAsset`: the asset the ID names; `assetIsContract` and `notToken`: the asset the contract issues, when it says */
  asset: AssetMeta | null;
}

/** What is wrong with an ID taken as a built-in asset; null when it names one */
export function assetProblem(id: string, named: Named): IdProblem | null {
  if (named.kind === 'asset') return null;
  if (named.kind === 'contract') {
    return named.standard === 'KS-20' && !named.issued ? { kind: 'assetIsToken', id, asset: null } : { kind: 'assetIsContract', id, asset: named.issued };
  }
  return { kind: 'assetUnknown', id, asset: null };
}

/** What `id` (64 hex digits) names, for a page: null while it is checked; tried again at the next block when the node did not answer */
export function useNamed(id: string | null): Named | null {
  const conn = useConnection();
  const tip = useTip();
  const [result, setResult] = useState<{ key: string; named: Named } | null>(null);
  const key = id && conn ? `${conn.endpoint}:${id}` : '';
  const settled = result?.key === key;
  useEffect(() => {
    if (!id || !conn || settled) return;
    let alive = true;
    whatIs(conn, id).then((named) => { if (alive) setResult({ key, named }); }, () => { /* the next block tries again */ });
    return () => { alive = false; };
  }, [key, settled, tip?.tip.height]);
  return settled ? result.named : null;
}

/** `assetProblem` for a page: null while it is checked and when the ID names an asset */
export function useAssetProblem(id: string | null): IdProblem | null {
  const named = useNamed(id);
  return id && named ? assetProblem(id, named) : null;
}

/** What is wrong with an ID taken as a token contract that does not answer as one */
export function tokenProblem(id: string, named: Named): IdProblem {
  if (named.kind === 'asset') return { kind: 'tokenIsAsset', id, asset: named.meta };
  if (named.kind === 'contract') return named.standard === 'KS-721' ? { kind: 'tokenIsNft', id, asset: null } : { kind: 'notToken', id, asset: named.issued };
  return { kind: 'tokenMissing', id, asset: null };
}

/** The asset to take instead, when the problem leads to one: the asset the ID names, or the one the contract issues */
export function assetInstead(problem: IdProblem): AssetMeta | null {
  return problem.kind === 'tokenIsAsset' || problem.kind === 'assetIsContract' || problem.kind === 'notToken' ? problem.asset : null;
}

/**
 * A problem with one of `init`'s arguments, with the argument's value that mends it when there is one: the asset the ID leads
 * to (`as: 'asset'`), or, for an `AssetRef` side given a KS-20 token's ID as an asset, the same ID as a token (`as: 'token'`)
 */
export interface InitProblem {
  /** The argument as the deploy form names it (`side_b`, `params.asset`) */
  path: string;
  problem: IdProblem;
  fix: { param: number; value: Value; as: 'asset' | 'token' } | null;
}

/** `value` with every occurrence of the ID `from` replaced by `to` */
export function replaceId(value: Value, from: string, to: Uint8Array): Value {
  if (value instanceof Uint8Array) return value.length === 32 && toHex(value) === from ? to : value;
  if (Array.isArray(value)) return value.map((v) => replaceId(v, from, to));
  if (value !== null && typeof value === 'object') {
    if (isEnumValue(value)) return { variant: value.variant, values: value.values.map((v) => replaceId(v, from, to)) };
    return Object.fromEntries(Object.entries(value).map(([k, v]) => [k, replaceId(v, from, to)]));
  }
  return value;
}

/**
 * Whether an enum is `AssetRef` (`docs/STANDARDS.md` §5.3) or has its shape: a `Token` variant taking a contract ID beside the
 * `Asset` variant, so that an asset side can be mended into a token side
 */
function isAssetRef(abi: Abi, name: string): boolean {
  const variants = abi.enums.get(name)?.variants ?? [];
  const token = variants.find((v) => v.name === 'Token');
  const only = token?.fields.length === 1 ? token.fields[0] : undefined;
  return only?.kind === 'id' && only.name === 'ContractId' && variants.some((v) => v.name === 'Asset');
}

/** `value` with every `Asset(id)` of an `AssetRef` enum turned into `Token(id)` */
export function assetRefAsToken(value: Value, id: string): Value {
  if (Array.isArray(value)) return value.map((v) => assetRefAsToken(v, id));
  if (value !== null && typeof value === 'object' && !(value instanceof Uint8Array)) {
    if (isEnumValue(value)) {
      const [only] = value.values;
      if (value.variant === 'Asset' && value.values.length === 1 && only instanceof Uint8Array && toHex(only) === id) return { variant: 'Token', values: [only] };
      return { variant: value.variant, values: value.values.map((v) => assetRefAsToken(v, id)) };
    }
    return Object.fromEntries(Object.entries(value).map(([k, v]) => [k, assetRefAsToken(v, id)]));
  }
  return value;
}

/** Every asset ID in a value of `type`, with the path the form names its place by and whether it is an `AssetRef`'s asset side */
function assetIds(abi: Abi, type: AbiType, value: Value, path: string, out: { path: string; id: string; side: boolean }[], side = false): void {
  switch (type.kind) {
    case 'id':
      if (type.name === 'AssetId' && value instanceof Uint8Array && value.length === 32) out.push({ path, id: toHex(value), side });
      break;
    case 'option':
      if (value !== null) assetIds(abi, type.inner, value, path, out);
      break;
    case 'vec':
      if (Array.isArray(value)) value.forEach((v, i) => assetIds(abi, type.inner, v, `${path}[${i}]`, out));
      break;
    case 'tuple':
      if (Array.isArray(value)) type.items.forEach((t, i) => assetIds(abi, t, value[i] ?? null, `${path}.${i}`, out));
      break;
    case 'struct': {
      const fields: Field[] = abi.structs.get(type.name)?.fields ?? [];
      if (value !== null && typeof value === 'object' && !Array.isArray(value) && !(value instanceof Uint8Array) && !isEnumValue(value)) {
        const record = value as { [field: string]: Value };
        for (const f of fields) assetIds(abi, f.type, record[f.name] ?? null, `${path}.${f.name}`, out);
      }
      break;
    }
    case 'enum': {
      if (value === null || !isEnumValue(value)) break;
      const variant = abi.enums.get(type.name)?.variants.find((v) => v.name === value.variant);
      const values = value.values;
      const asset = value.variant === 'Asset' && isAssetRef(abi, type.name);
      variant?.fields.forEach((t, i) => assetIds(abi, t, values[i] ?? null, path, out, asset));
      break;
    }
    default:
      break;
  }
}

/**
 * The asset IDs among `init`'s arguments that name no asset, each with its argument mended when the ID leads somewhere: to the
 * asset the contract it names issues, or, for an `AssetRef` side, to the KS-20 token it names taken as a token; throws when the
 * node does not answer
 */
export async function assetArgProblems(conn: Connection, abi: Abi, params: Field[], values: Value[]): Promise<InitProblem[]> {
  const out: InitProblem[] = [];
  for (const [i, p] of params.entries()) {
    const value = values[i] ?? null;
    const found: { path: string; id: string; side: boolean }[] = [];
    assetIds(abi, p.type, value, p.name, found);
    for (const { path, id, side } of found) {
      const problem = assetProblem(id, await whatIs(conn, id));
      if (!problem) continue;
      const instead = assetInstead(problem);
      const fix = instead ? { param: i, value: replaceId(value, id, fromHex(instead.id, 32)), as: 'asset' as const }
        : problem.kind === 'assetIsToken' && side ? { param: i, value: assetRefAsToken(value, id), as: 'token' as const } : null;
      out.push({ path, problem, fix });
    }
  }
  return out;
}
