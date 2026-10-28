import { describe, expect, it } from 'vitest';
import { fromHex, toHex } from '@ksana/sdk';
import { encodeArgs } from '../../abi/codec';
import { parseAbi } from '../../abi/parse';
import { KSA_HEX, KSA_META } from '../../chain/assets';
import { fakeConnection, type Fake } from '../../chain/fakeNode';
import { assetInstead } from '../../chain/ids';
import { poolSide, type PoolSide, type Side } from './pool';
import { TOKEN_ABI, ammInitProblems, answerOf, approvalNeeded, approveAction, attached, checkSide, sendAction, sideValue } from './token';

const hex = (byte: number) => toHex(new Uint8Array(32).fill(byte));
/** A token contract written before KS-20, with `transfer_from` and `metadata` */
const TOKEN = hex(1);
/** The same without `metadata` */
const BARE = hex(2);
/** The asset example's contract (mintable-token) and the built-in asset it issues */
const MINTER = hex(3);
const ISSUED = hex(4);
/** A KS-20 token, like contracts/examples/token, and a KS-721 collection */
const KS20 = hex(5);
const NFT = hex(6);
const POOL = hex(9);

const returns = (name: string) => TOKEN_ABI.methods.find((m) => m.name === name)!.returns!;
const u128 = (v: bigint) => encodeArgs(TOKEN_ABI, [returns('allowance')], [v]);
const standards = parseAbi('//! ```abi\n//! view standards() -> Vec<String>\n//! ```')!;
const declares = (...ids: string[]) => encodeArgs(standards, [standards.methods[0]!.returns!], [ids]);

function chain(): Fake {
  const token = (withMetadata: boolean, ks20: boolean) => (method: string) => {
    if (method === 'allowance' || method === 'balance_of') return u128(5n);
    if (method === 'metadata' && withMetadata) return encodeArgs(TOKEN_ABI, [returns('metadata')], [['Tutorial', 'TUT', 6n]]);
    if (method === 'standards' && ks20) return declares('KS-20', 'KS-20-burnable', 'KS-20-mintable');
    return undefined;
  };
  return {
    assets: new Map([[ISSUED, { name: 'Magic', decimals: 8, issuer: MINTER }]]),
    contracts: new Map([
      [TOKEN, token(true, false)],
      [BARE, token(false, false)],
      [KS20, token(true, true)],
      [MINTER, (method: string) => (method === 'asset' ? fromHex(ISSUED, 32) : undefined)],
      [NFT, (method: string) => (method === 'standards' ? declares('KS-721', 'KS-721-enumerable') : undefined)],
    ]),
  };
}

const side = (kind: 'Asset' | 'Token', id: string): Side => ({ kind, id: fromHex(id, 32), hex: id });
const asPool = (s: Side, allowance: bigint | null, ks20 = false): PoolSide => ({ ...s, name: 'x', decimals: 0, baseUnits: false, held: null, allowance, ks20 });
/** A u128 or a u32 as Borsh writes it, little-endian */
const le = (v: bigint, bytes: number) => toHex(Uint8Array.from({ length: bytes }, (_, i) => Number((v >> BigInt(8 * i)) & 0xffn)));

describe('a pool side as the node has it', () => {
  it('takes a token contract that answers as one, with its metadata when it has some', async () => {
    const conn = fakeConnection(chain());
    expect(await checkSide(conn, side('Token', TOKEN), POOL)).toEqual({ kind: 'ok', token: { name: 'Tutorial', symbol: 'TUT', decimals: 6 }, ks20: false });
    expect(await checkSide(conn, side('Token', BARE), POOL)).toEqual({ kind: 'ok', token: null, ks20: false });
    expect(await checkSide(conn, side('Asset', KSA_HEX), POOL)).toEqual({ kind: 'ok', token: null, ks20: false });
    expect(await checkSide(conn, side('Asset', ISSUED), POOL)).toEqual({ kind: 'ok', token: null, ks20: false });
  });

  it('marks a token that declares KS-20, and finds a KS-721 collection or a KS-20 token on the wrong side', async () => {
    const conn = fakeConnection(chain());
    expect(await checkSide(conn, side('Token', KS20), POOL)).toEqual({ kind: 'ok', token: { name: 'Tutorial', symbol: 'TUT', decimals: 6 }, ks20: true });
    expect(await checkSide(conn, side('Token', NFT), POOL)).toEqual({ kind: 'broken', problem: { kind: 'tokenIsNft', id: NFT, asset: null } });
    expect(await checkSide(conn, side('Asset', KS20), POOL)).toEqual({ kind: 'broken', problem: { kind: 'assetIsToken', id: KS20, asset: null } });
  });

  it('finds a token side given an asset’s ID, and the asset to take instead', async () => {
    const conn = fakeConnection(chain());
    const check = await checkSide(conn, side('Token', ISSUED), POOL);
    expect(check).toMatchObject({ kind: 'broken', problem: { kind: 'tokenIsAsset', id: ISSUED } });
    expect(check.kind === 'broken' && assetInstead(check.problem)?.id).toBe(ISSUED);
  });

  it('finds a token side given the contract that issues an asset, a missing contract, and an asset side given a contract', async () => {
    const conn = fakeConnection(chain());
    const minter = await checkSide(conn, side('Token', MINTER), POOL);
    expect(minter).toMatchObject({ kind: 'broken', problem: { kind: 'notToken', asset: { id: ISSUED, name: 'Magic' } } });
    expect(await checkSide(conn, side('Token', hex(7)), POOL)).toMatchObject({ kind: 'broken', problem: { kind: 'tokenMissing' } });
    expect(await checkSide(conn, side('Asset', MINTER), POOL)).toMatchObject({ kind: 'broken', problem: { kind: 'assetIsContract', asset: { id: ISSUED } } });
  });
});

describe('approving and attaching', () => {
  it('asks for an approval only when a token side’s allowance is known and short of the amount', () => {
    const token = side('Token', TOKEN);
    expect(approvalNeeded(asPool(token, 4n), 5n)).toBe(5n);
    expect(approvalNeeded(asPool(token, 5n), 5n)).toBeNull();
    expect(approvalNeeded(asPool(token, null), 5n)).toBeNull();
    expect(approvalNeeded(asPool(token, 0n), 0n)).toBeNull();
    expect(approvalNeeded(asPool(token, 0n), null)).toBeNull();
    expect(approvalNeeded(asPool(side('Asset', KSA_HEX), 0n), 5n)).toBeNull();
  });

  it('attaches the asset sides only', () => {
    const ksa = asPool(side('Asset', KSA_HEX), null);
    const token = asPool(side('Token', TOKEN), 0n);
    expect(attached([[ksa, 3n], [token, 4n]])).toEqual([{ asset: fromHex(KSA_HEX, 32), amount: 3n }]);
    expect(attached([[token, 4n]])).toEqual([]);
    expect(attached([[ksa, 0n]])).toEqual([]);
  });

  it('approves the pool on the token contract: a KS-20 token until the expiry, one before KS-20 with its two arguments', () => {
    const legacy = approveAction(asPool(side('Token', TOKEN), 0n), POOL, 258n, 1_700_000_000n);
    expect(legacy.kind).toBe('Call');
    if (legacy.kind !== 'Call') return;
    expect(toHex(legacy.contract)).toBe(TOKEN);
    expect(legacy.method).toBe('approve');
    expect(toHex(legacy.args)).toBe(POOL + le(258n, 16));
    expect(legacy.funds).toEqual([]);
    const ks20 = approveAction(asPool(side('Token', KS20), 0n, true), POOL, 258n, 1_700_000_000n);
    if (ks20.kind !== 'Call') throw new Error('not a call');
    expect(toHex(ks20.contract)).toBe(KS20);
    // Some(expires_at): 1, then the u64
    expect(toHex(ks20.args)).toBe(`${POOL}${le(258n, 16)}01${le(1_700_000_000n, 8)}`);
  });
});

describe('paying the pool with a KS-20 token’s transfer_and_call', () => {
  it('sends the amount to the pool with the swap or the deposit as the data, attaching the other side', () => {
    const token = asPool(side('Token', KS20), null, true);
    const swap = sendAction(token, POOL, 1_000n, { variant: 'Swap', values: [990n] });
    if (swap.kind !== 'Call') throw new Error('not a call');
    expect(toHex(swap.contract)).toBe(KS20);
    expect(swap.method).toBe('transfer_and_call');
    // to, amount, then data as Bytes: its length, PoolAction::Swap (0) and min_out
    expect(toHex(swap.args)).toBe(`${POOL}${le(1_000n, 16)}${le(17n, 4)}00${le(990n, 16)}`);
    expect(swap.funds).toEqual([]);
    const ksa = [{ asset: fromHex(KSA_HEX, 32), amount: 500n }];
    const add = sendAction(token, POOL, 1_000n, { variant: 'AddLiquidity', values: [500n, 700n] }, ksa);
    if (add.kind !== 'Call') throw new Error('not a call');
    expect(toHex(add.args)).toBe(`${POOL}${le(1_000n, 16)}${le(33n, 4)}01${le(500n, 16)}${le(700n, 16)}`);
    expect(add.funds).toEqual(ksa);
  });

  it('reads the amount out of the pool’s answer, which the token returns as it got it', () => {
    expect(answerOf(fromHex(`${le(16n, 4)}${le(123_456n, 16)}`))).toBe(123_456n);
    // not Bytes holding a u128
    expect(answerOf(fromHex(le(123_456n, 16)))).toBeNull();
    expect(answerOf(fromHex(`${le(0n, 4)}`))).toBeNull();
    expect(answerOf(new Uint8Array())).toBeNull();
  });
});

describe('the AMM’s check before a deployment', () => {
  const abi = parseAbi(['//! ```abi', '//! enum Side { Token(ContractId), Asset(AssetId) }', '//! init(side_a: Side, side_b: Side)', '//! ```'].join('\n'))!;
  const params = abi.init!.params;

  it('passes a token contract and leaves asset sides to the generic check', async () => {
    const conn = fakeConnection(chain());
    const values = [sideValue('Token', fromHex(TOKEN, 32)), sideValue('Asset', fromHex(MINTER, 32))];
    expect(await ammInitProblems(conn, abi, params, values)).toEqual([]);
  });

  it('mends a token side given an asset’s ID, or its issuer’s, to take the asset', async () => {
    const conn = fakeConnection(chain());
    const values = [sideValue('Token', fromHex(ISSUED, 32)), sideValue('Token', fromHex(MINTER, 32))];
    const problems = await ammInitProblems(conn, abi, params, values);
    expect(problems.map((p) => [p.path, p.problem.kind, p.fix])).toEqual([
      ['side_a', 'tokenIsAsset', { param: 0, value: sideValue('Asset', fromHex(ISSUED, 32)), as: 'asset' }],
      ['side_b', 'notToken', { param: 1, value: sideValue('Asset', fromHex(ISSUED, 32)), as: 'asset' }],
    ]);
  });

  it('cannot mend a token side that names nothing, or a KS-721 collection', async () => {
    const conn = fakeConnection(chain());
    const values = [sideValue('Token', fromHex(NFT, 32)), sideValue('Token', fromHex(hex(7), 32))];
    const problems = await ammInitProblems(conn, abi, params, values);
    expect(problems.map((p) => [p.path, p.problem.kind, p.fix])).toEqual([['side_a', 'tokenIsNft', null], ['side_b', 'tokenMissing', null]]);
  });

  it('passes a KS-20 token side', async () => {
    const conn = fakeConnection(chain());
    expect(await ammInitProblems(conn, abi, params, [sideValue('Asset', fromHex(KSA_HEX, 32)), sideValue('Token', fromHex(KS20, 32))])).toEqual([]);
  });
});

describe('a side’s name and units', () => {
  const facts = { meta: null, held: null, allowance: null };

  it('names a token by its symbol in its decimals, by its ID in base units without metadata, and waits while it is read', () => {
    const token = side('Token', TOKEN);
    expect(poolSide(token, { ...facts, token: { symbol: 'TUT', decimals: 6 } }, (id) => `token ${id}`)).toMatchObject({ name: 'TUT', decimals: 6, baseUnits: false, ks20: false });
    expect(poolSide(token, { ...facts, token: { symbol: 'TUT', decimals: 6 }, ks20: true }, (id) => `token ${id}`)).toMatchObject({ name: 'TUT', ks20: true });
    expect(poolSide(token, { ...facts, token: null }, (id) => `token ${id}`)).toMatchObject({ name: `token ${TOKEN.slice(0, 8)}…`, decimals: 0, baseUnits: true });
    expect(poolSide(token, facts, (id) => `token ${id}`)).toMatchObject({ decimals: null, baseUnits: false });
  });

  it('names an asset from its metadata, and carries what the wallet holds', () => {
    const ksa = poolSide(side('Asset', KSA_HEX), { meta: KSA_META, held: 7n, allowance: 3n }, (id) => id);
    expect(ksa).toMatchObject({ name: 'KSA', decimals: 8, held: 7n, allowance: null, baseUnits: false, ks20: false });
  });
});
