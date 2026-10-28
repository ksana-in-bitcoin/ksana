import { describe, expect, it } from 'vitest';
import { fromHex, toHex } from '@ksana/sdk';
import { parseAbi } from '../abi/parse';
import type { Value } from '../abi/types';
import { KSA_HEX } from './assets';
import { fakeConnection, type Fake } from './fakeNode';
import { assetArgProblems, assetInstead, assetProblem, assetRefAsToken, replaceId, tokenProblem, whatIs } from './ids';

const hex = (byte: number) => toHex(new Uint8Array(32).fill(byte));
/** The asset example's contract and the asset it issues */
const TOKEN = hex(1);
const ISSUED = hex(2);
const OTHER = hex(3);
const NOTHING = hex(4);
/** A KS-20 token and a KS-721 collection, which declare their standards and issue no asset */
const KS20 = hex(5);
const KS721 = hex(6);

/** The Borsh encoding of a `Vec<String>`, as `standards()` returns it */
function strings(...items: string[]): Uint8Array {
  const parts = items.map((item) => new TextEncoder().encode(item));
  const out = new Uint8Array(4 + parts.reduce((n, p) => n + 4 + p.length, 0));
  const view = new DataView(out.buffer);
  view.setUint32(0, items.length, true);
  let at = 4;
  for (const p of parts) {
    view.setUint32(at, p.length, true);
    out.set(p, at + 4);
    at += 4 + p.length;
  }
  return out;
}

/** A node where TOKEN issues the asset ISSUED ("Magic"), OTHER is a contract that issues nothing, and KS20 and KS721 declare their standards */
function chain(): Fake {
  return {
    assets: new Map([[ISSUED, { name: 'Magic', decimals: 8, issuer: TOKEN }]]),
    contracts: new Map([
      [TOKEN, (method: string) => (method === 'asset' ? fromHex(ISSUED, 32) : undefined)],
      [OTHER, () => undefined],
      [KS20, (method: string) => (method === 'standards' ? strings('KS-20', 'KS-20-burnable') : undefined)],
      [KS721, (method: string) => (method === 'standards' ? strings('KS-721') : undefined)],
    ]),
  };
}

describe('what an ID names', () => {
  it('tells assets, contracts and nothing apart', async () => {
    const conn = fakeConnection(chain());
    expect(await whatIs(conn, KSA_HEX)).toMatchObject({ kind: 'asset', meta: { id: KSA_HEX } });
    expect(await whatIs(conn, ISSUED)).toMatchObject({ kind: 'asset', meta: { name: 'Magic', issuer: TOKEN } });
    expect(await whatIs(conn, TOKEN)).toMatchObject({ kind: 'contract', issued: { id: ISSUED, name: 'Magic' }, standard: null });
    expect(await whatIs(conn, OTHER)).toEqual({ kind: 'contract', issued: null, standard: null });
    expect(await whatIs(conn, NOTHING)).toEqual({ kind: 'nothing' });
  });

  it('reads the token standard a contract declares', async () => {
    const conn = fakeConnection(chain());
    expect(await whatIs(conn, KS20)).toEqual({ kind: 'contract', issued: null, standard: 'KS-20' });
    expect(await whatIs(conn, KS721)).toEqual({ kind: 'contract', issued: null, standard: 'KS-721' });
    // a contract whose standards() answers something else declares none
    const odd = fakeConnection({ assets: new Map(), contracts: new Map([[OTHER, (method: string) => (method === 'standards' ? strings('ERC-20') : undefined)]]) });
    expect(await whatIs(odd, OTHER)).toEqual({ kind: 'contract', issued: null, standard: null });
  });

  it('says what is wrong with an ID taken as an asset or as a token contract', async () => {
    const conn = fakeConnection(chain());
    expect(assetProblem(ISSUED, await whatIs(conn, ISSUED))).toBeNull();
    const contract = assetProblem(TOKEN, await whatIs(conn, TOKEN));
    expect(contract).toMatchObject({ kind: 'assetIsContract', id: TOKEN, asset: { id: ISSUED } });
    expect(assetInstead(contract!)?.id).toBe(ISSUED);
    expect(assetProblem(NOTHING, await whatIs(conn, NOTHING))).toEqual({ kind: 'assetUnknown', id: NOTHING, asset: null });

    const asset = tokenProblem(ISSUED, await whatIs(conn, ISSUED));
    expect(asset).toMatchObject({ kind: 'tokenIsAsset', asset: { name: 'Magic' } });
    expect(assetInstead(asset)?.id).toBe(ISSUED);
    expect(tokenProblem(TOKEN, await whatIs(conn, TOKEN))).toMatchObject({ kind: 'notToken', asset: { id: ISSUED } });
    const other = tokenProblem(OTHER, await whatIs(conn, OTHER));
    expect(other).toEqual({ kind: 'notToken', id: OTHER, asset: null });
    expect(assetInstead(other)).toBeNull();
    expect(tokenProblem(NOTHING, await whatIs(conn, NOTHING)).kind).toBe('tokenMissing');

    // a KS-20 token is no built-in asset, and a KS-721 collection no fungible token: neither leads to an asset
    const token = assetProblem(KS20, await whatIs(conn, KS20));
    expect(token).toEqual({ kind: 'assetIsToken', id: KS20, asset: null });
    expect(assetInstead(token!)).toBeNull();
    const collection = tokenProblem(KS721, await whatIs(conn, KS721));
    expect(collection).toEqual({ kind: 'tokenIsNft', id: KS721, asset: null });
    expect(assetInstead(collection)).toBeNull();
    expect(assetProblem(KS721, await whatIs(conn, KS721))?.kind).toBe('assetIsContract');
  });

  it('throws when the node does not answer', async () => {
    const conn = fakeConnection(chain());
    (conn.node as unknown as { getAsset: () => Promise<never> }).getAsset = async () => { throw new Error('connection refused'); };
    await expect(whatIs(conn, ISSUED)).rejects.toThrow('connection refused');
  });
});

describe('asset IDs among init arguments', () => {
  const abi = parseAbi([
    '//! ```abi',
    '//! struct Leg { asset: AssetId, weight: u32 }',
    '//! enum Side { Token(ContractId), Asset(AssetId) }',
    '//! init(loan: AssetId, legs: Vec<Leg>, side: Side, spare: Option<AssetId>, owner: Account)',
    '//! ```',
  ].join('\n'))!;
  const params = abi.init!.params;
  const values = (loan: string, leg: string, side: Value, spare: string | null): Value[] => [
    fromHex(loan, 32),
    [{ asset: fromHex(leg, 32), weight: 1n }, { asset: fromHex(ISSUED, 32), weight: 2n }],
    side,
    spare === null ? null : fromHex(spare, 32),
    fromHex(TOKEN, 32),
  ];

  it('finds none when every asset ID names an asset, and ignores contract IDs and accounts', async () => {
    const conn = fakeConnection(chain());
    const side = { variant: 'Token', values: [fromHex(TOKEN, 32)] };
    expect(await assetArgProblems(conn, abi, params, values(KSA_HEX, ISSUED, side, null))).toEqual([]);
  });

  it('names the place of each wrong one, mended to the asset its contract issues', async () => {
    const conn = fakeConnection(chain());
    const side = { variant: 'Asset', values: [fromHex(TOKEN, 32)] };
    const problems = await assetArgProblems(conn, abi, params, values(TOKEN, NOTHING, side, OTHER));
    expect(problems.map((p) => [p.path, p.problem.kind])).toEqual([
      ['loan', 'assetIsContract'],
      ['legs[0].asset', 'assetUnknown'],
      ['side', 'assetIsContract'],
      ['spare', 'assetIsContract'],
    ]);
    const [loan, leg, inSide, spare] = problems;
    expect(loan!.fix).toEqual({ param: 0, value: fromHex(ISSUED, 32), as: 'asset' });
    expect(leg!.fix).toBeNull();
    expect(inSide!.fix).toEqual({ param: 2, value: { variant: 'Asset', values: [fromHex(ISSUED, 32)] }, as: 'asset' });
    expect(spare!.fix).toBeNull();
  });

  it('mends an asset side given a KS-20 token into a token side, and nothing else', async () => {
    const conn = fakeConnection(chain());
    const side = { variant: 'Asset', values: [fromHex(KS20, 32)] };
    const problems = await assetArgProblems(conn, abi, params, values(KS20, ISSUED, side, null));
    expect(problems.map((p) => [p.path, p.problem.kind])).toEqual([['loan', 'assetIsToken'], ['side', 'assetIsToken']]);
    const [loan, inSide] = problems;
    // a plain asset ID cannot take a token
    expect(loan!.fix).toBeNull();
    expect(inSide!.fix).toEqual({ param: 2, value: { variant: 'Token', values: [fromHex(KS20, 32)] }, as: 'token' });
  });

  it('turns the asset sides naming an ID into token sides', () => {
    const before: Value = [{ variant: 'Asset', values: [fromHex(KS20, 32)] }, { variant: 'Asset', values: [fromHex(ISSUED, 32)] }, { variant: 'Token', values: [fromHex(KS20, 32)] }];
    expect(assetRefAsToken(before, KS20)).toEqual([
      { variant: 'Token', values: [fromHex(KS20, 32)] }, { variant: 'Asset', values: [fromHex(ISSUED, 32)] }, { variant: 'Token', values: [fromHex(KS20, 32)] },
    ]);
  });

  it('replaces an ID wherever it appears in a value', () => {
    const before: Value = { a: [fromHex(TOKEN, 32), fromHex(OTHER, 32)], b: { variant: 'Asset', values: [fromHex(TOKEN, 32)] }, c: 5n, d: new Uint8Array([1]) };
    const after = replaceId(before, TOKEN, fromHex(ISSUED, 32));
    expect(after).toEqual({ a: [fromHex(ISSUED, 32), fromHex(OTHER, 32)], b: { variant: 'Asset', values: [fromHex(ISSUED, 32)] }, c: 5n, d: new Uint8Array([1]) });
  });
});
