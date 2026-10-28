// The multisig page's actions encode as the contract decodes them: every variant through the contract's ABI block, byte for byte
// as its `Decode` reads it, and back; the views decode into what the page shows; the rules match the contract's revert reasons.
import { readFileSync } from 'node:fs';
import { describe, expect, it } from 'vitest';
import { decodeReturn, decodeValue, encodeArgs, encodeValue } from '../../abi/codec';
import { parseAbi } from '../../abi/parse';
import type { AbiType, Value } from '../../abi/types';
import {
  actionOf, actionValue, checkAction, eventOf, executeBlocker, proposalOf, sortedFunds, walletOf, type Proposal, type WalletAction,
  type WalletInfo,
} from './actions';

const source = readFileSync(new URL('../../../../contracts/examples/multisig/src/lib.rs', import.meta.url), 'utf8');
const parsed = parseAbi(source);
if (!parsed) throw new Error('the multisig contract declares no ABI block');
const abi = parsed;
const ACTION: AbiType = { kind: 'enum', name: 'Action' };
const PROPOSAL: AbiType = { kind: 'struct', name: 'Proposal' };

const id = (byte: number) => new Uint8Array(32).fill(byte);
const hex = (byte: number) => byte.toString(16).padStart(2, '0').repeat(32);
const le = (value: bigint, bytes: number) => Uint8Array.from({ length: bytes }, (_, i) => Number((value >> BigInt(8 * i)) & 0xffn));
const cat = (...parts: (Uint8Array | number[])[]) => Uint8Array.from(parts.flatMap((p) => [...p]));
const utf8 = (text: string) => new TextEncoder().encode(text);

const KSA = hex(0);
const USD = hex(0x05);
const ALICE = hex(0xa1);
const BOB = hex(0xb0);
const CAROL = hex(0xc4);
const DAVE = hex(0xd4);
const EVE = hex(0xe5);
const CALLEE = hex(0xca);
const SELF = hex(0x44);
const U128_MAX = (1n << 128n) - 1n;

/** Each variant with the bytes the contract's `Action::decode` reads: a u8 tag, then the fields in order */
const VARIANTS: { name: string; action: WalletAction; bytes: Uint8Array }[] = [
  { name: 'Transfer', action: { kind: 'Transfer', asset: KSA, to: DAVE, amount: 400n }, bytes: cat([0], id(0), id(0xd4), le(400n, 16)) },
  { name: 'Transfer of u128::MAX', action: { kind: 'Transfer', asset: USD, to: EVE, amount: U128_MAX }, bytes: cat([0], id(0x05), id(0xe5), le(U128_MAX, 16)) },
  {
    name: 'Call with arguments and funds',
    action: { kind: 'Call', contract: CALLEE, method: 'echo', args: Uint8Array.from([1, 2, 3]), funds: [{ asset: KSA, amount: 400n }, { asset: USD, amount: 50n }] },
    bytes: cat([1], id(0xca), le(4n, 4), utf8('echo'), le(3n, 4), [1, 2, 3], le(2n, 4), id(0), le(400n, 16), id(0x05), le(50n, 16)),
  },
  {
    name: 'Call with a multibyte method and nothing else',
    action: { kind: 'Call', contract: CALLEE, method: 'señal', args: new Uint8Array(0), funds: [] },
    bytes: cat([1], id(0xca), le(6n, 4), utf8('señal'), le(0n, 4), le(0n, 4)),
  },
  { name: 'AddOwner', action: { kind: 'AddOwner', account: DAVE }, bytes: cat([2], id(0xd4)) },
  { name: 'RemoveOwner', action: { kind: 'RemoveOwner', account: BOB }, bytes: cat([3], id(0xb0)) },
  { name: 'SetThreshold', action: { kind: 'SetThreshold', threshold: 2 }, bytes: Uint8Array.from([4, 2]) },
];

describe('actions', () => {
  const propose = abi.methods.find((m) => m.name === 'propose');

  it('the ABI block declares the enum the page builds', () => {
    expect(abi.enums.get('Action')?.variants.map((v) => v.name)).toEqual(['Transfer', 'Call', 'AddOwner', 'RemoveOwner', 'SetThreshold']);
    expect(propose?.params.map((p) => p.type)).toEqual([ACTION]);
  });

  it.each(VARIANTS)('$name encodes as the contract decodes it, and back', ({ action, bytes }) => {
    const value = actionValue(action);
    expect(encodeValue(abi, ACTION, value)).toEqual(bytes);
    // what `callAction(abi, contract, 'propose', [value])` sends: the single argument is itself
    expect(encodeArgs(abi, propose!.params.map((p) => p.type), [value])).toEqual(bytes);
    expect(actionOf(decodeValue(abi, ACTION, bytes))).toEqual(action);
  });

  it('refuses a shape it does not know', () => {
    expect(actionOf({ variant: 'Transfer', values: [id(0), id(1)] })).toBeNull();
    expect(actionOf({ variant: 'Burn', values: [] })).toBeNull();
    expect(actionOf({ variant: 'SetThreshold', values: [256n] })).toBeNull();
  });
});

describe('views', () => {
  it('a page of proposals decodes into what the cards show', () => {
    const action: WalletAction = { kind: 'Call', contract: CALLEE, method: 'echo', args: Uint8Array.from([9]), funds: [{ asset: KSA, amount: 1n }] };
    const value: Value = {
      id: 7n, proposer: id(0xa1), action: actionValue(action), approvals: [id(0xa1), id(0xb0)], status: { variant: 'Executed', values: [] }, created_at: 1_760_000_060n,
    };
    const one = encodeValue(abi, PROPOSAL, value);
    // the contract's Proposal: id, proposer, action, approvals, status, created_at
    expect(one).toEqual(cat(le(7n, 8), id(0xa1), encodeValue(abi, ACTION, actionValue(action)), le(2n, 4), id(0xa1), id(0xb0), [1], le(1_760_000_060n, 8)));
    const page = decodeReturn(abi, abi.methods.find((m) => m.name === 'proposals')!.returns, cat(le(1n, 4), one));
    expect(Array.isArray(page) ? page.map(proposalOf) : null).toEqual([
      { id: 7n, proposer: ALICE, action, approvals: [ALICE, BOB], status: 'Executed', createdAt: 1_760_000_060n } satisfies Proposal,
    ]);
  });

  it('the wallet view decodes into the owners, the threshold and the latest ID', () => {
    const bytes = cat(le(3n, 4), id(0xa1), id(0xb0), id(0xc4), [2], le(5n, 8));
    const value = decodeReturn(abi, abi.methods.find((m) => m.name === 'wallet')!.returns, bytes);
    expect(walletOf(value)).toEqual({ owners: [ALICE, BOB, CAROL], threshold: 2, latest: 5n });
  });

  it('events read as the page writes them', () => {
    expect(eventOf('deposit', { from: id(0xe5), asset: id(0x05), amount: 20n })).toEqual({ kind: 'deposit', from: EVE, asset: USD, amount: 20n });
    expect(eventOf('execute', { id: 1n, executor: id(0xc4) })).toEqual({ kind: 'execute', id: 1n, who: CAROL });
    expect(eventOf('threshold', { threshold: 3n })).toEqual({ kind: 'threshold', threshold: 3 });
    expect(eventOf('approve', { id: 1n })).toBeNull();
  });
});

describe('rules', () => {
  const wallet: WalletInfo = { owners: [ALICE, BOB, CAROL], threshold: 2, latest: 0n };
  const call = (contract: string, method: string, args = 0, funds: { asset: string; amount: bigint }[] = []): WalletAction =>
    ({ kind: 'Call', contract, method, args: new Uint8Array(args), funds });

  it('check what the contract checks when proposing, in its order', () => {
    const many = Array.from({ length: 17 }, (_, i) => ({ asset: hex(i + 1), amount: 1n }));
    const cases: [WalletAction, string | null][] = [
      [{ kind: 'Transfer', asset: KSA, to: hex(0), amount: 1n }, 'zeroRecipient'],
      [{ kind: 'Transfer', asset: KSA, to: EVE, amount: 0n }, 'zeroAmount'],
      [{ kind: 'Transfer', asset: KSA, to: EVE, amount: U128_MAX }, null],
      [call(SELF, 'deposit'), 'selfCall'],
      [call(hex(0), 'x'), 'zeroContract'],
      [call(CALLEE, ''), 'method'],
      [call(CALLEE, 'm'.repeat(65)), 'method'],
      [call(CALLEE, 'ñ'.repeat(33)), 'method'],
      [call(CALLEE, 'init'), 'method'],
      [call(CALLEE, 'init_later'), null],
      [call(CALLEE, 'm'.repeat(64), 16_384, many.slice(0, 16)), null],
      [call(CALLEE, 'x', 16_385), 'argsTooLong'],
      [call(CALLEE, 'x', 0, many), 'tooManyFunds'],
      [call(CALLEE, 'x', 0, [{ asset: USD, amount: 1n }, { asset: KSA, amount: 1n }]), 'fundsOrder'],
      [call(CALLEE, 'x', 0, [{ asset: KSA, amount: 1n }, { asset: KSA, amount: 2n }]), 'fundsOrder'],
      [call(CALLEE, 'x', 0, [{ asset: KSA, amount: 1n }, { asset: USD, amount: 0n }]), 'zeroFunds'],
      [{ kind: 'AddOwner', account: hex(0) }, 'zeroOwner'],
      [{ kind: 'AddOwner', account: SELF }, 'selfOwner'],
      [{ kind: 'AddOwner', account: BOB }, 'alreadyOwner'],
      [{ kind: 'AddOwner', account: DAVE }, null],
      [{ kind: 'RemoveOwner', account: EVE }, 'unknownOwner'],
      [{ kind: 'RemoveOwner', account: SELF }, 'unknownOwner'],
      [{ kind: 'RemoveOwner', account: CAROL }, null],
      [{ kind: 'SetThreshold', threshold: 0 }, 'threshold'],
      [{ kind: 'SetThreshold', threshold: 4 }, 'threshold'],
      [{ kind: 'SetThreshold', threshold: 3 }, null],
    ];
    for (const [action, problem] of cases) expect(checkAction(action, wallet, SELF), JSON.stringify(action, (_k, v) => (typeof v === 'bigint' ? `${v}` : v))).toBe(problem);
    expect(checkAction({ kind: 'AddOwner', account: DAVE }, { ...wallet, owners: Array.from({ length: 20 }, (_, i) => hex(i + 1)) }, SELF)).toBe('tooManyOwners');
    expect(checkAction({ kind: 'RemoveOwner', account: BOB }, { owners: [ALICE, BOB], threshold: 2, latest: 0n }, SELF)).toBe('belowThreshold');
  });

  it('sort the attached funds as the contract takes them', () => {
    expect(sortedFunds([{ asset: USD, amount: 1n }, { asset: KSA, amount: 2n }])).toEqual({ funds: [{ asset: KSA, amount: 2n }, { asset: USD, amount: 1n }], problem: null });
    expect(sortedFunds([{ asset: USD, amount: 1n }, { asset: USD, amount: 2n }]).problem).toBe('fundsOrder');
  });

  it('tell why execute would revert now', () => {
    const p: Proposal = { id: 1n, proposer: ALICE, action: { kind: 'Transfer', asset: USD, to: EVE, amount: 300n }, approvals: [ALICE], status: 'Pending', createdAt: 0n };
    const held = new Map([[USD, 299n]]);
    expect(executeBlocker(p, wallet, SELF, held)).toEqual({ kind: 'approvals', missing: 1 });
    const approved = { ...p, approvals: [ALICE, BOB] };
    expect(executeBlocker(approved, wallet, SELF, null)).toEqual({ kind: 'reading' });
    expect(executeBlocker(approved, wallet, SELF, held)).toEqual({ kind: 'balance', asset: USD, held: 299n, needed: 300n });
    expect(executeBlocker(approved, wallet, SELF, new Map([[USD, 300n]]))).toBeNull();
    // an owner action checked again against the owners of the moment
    expect(executeBlocker({ ...approved, action: { kind: 'AddOwner', account: CAROL } }, wallet, SELF, held)).toEqual({ kind: 'rule', problem: 'alreadyOwner' });
  });
});
