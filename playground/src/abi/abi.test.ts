import { describe, expect, it } from 'vitest';
import { fromHex, toHex } from '@ksana/sdk';
import { AbiError, abiLines, parseAbi } from './parse';
import { CodecError, decodeEvent, decodeReturn, decodeValue, encodeArgs, encodeValue } from './codec';
import { InputError, accountText, draftOf, emptyDraft, formatValue, parseTokenId, serialOf, tokenIdOf, tokenIdText, valueOf, withVariant } from './text';
import type { Abi, AbiType, Value } from './types';

const SOURCE = `//! A test contract.
//!
//! \`\`\`abi
//! struct Funds { asset: AssetId, amount: u128 }
//! enum Action { Transfer(AssetId, Account, u128), Call(ContractId, String, Bytes, Vec<Funds>), SetThreshold(u8), Stop }
//! payable init(owners: Vec<Account>, threshold: u8)     // funds stay in the wallet
//! call propose(action: Action) -> u64                   // owner only
//! view balance(asset: AssetId) -> u128
//! view pair() -> (u128, Option<i64>)
//! call nothing()
//! event deposit(from: Account, asset: AssetId, amount: u128)
//! \`\`\`
#![no_std]
`;

function abi(): Abi {
  const parsed = parseAbi(SOURCE);
  if (!parsed) throw new Error('no abi');
  return parsed;
}

function bytes(...values: number[]): Uint8Array {
  return Uint8Array.from(values);
}

describe('parseAbi', () => {
  it('reads every declaration', () => {
    const a = abi();
    expect([...a.structs.keys()]).toEqual(['Funds']);
    expect(a.enums.get('Action')?.variants.map((v) => v.name)).toEqual(['Transfer', 'Call', 'SetThreshold', 'Stop']);
    expect(a.init?.payable).toBe(true);
    expect(a.init?.doc).toBe('funds stay in the wallet');
    expect(a.init?.line).toBe(6);
    expect(a.methods.map((m) => `${m.kind} ${m.name}`)).toEqual(['call propose', 'view balance', 'view pair', 'call nothing']);
    expect(a.methods[0]!.params[0]!.type).toEqual({ kind: 'enum', name: 'Action' });
    expect(a.methods[2]!.returns).toEqual({ kind: 'tuple', items: [{ kind: 'int', name: 'u128' }, { kind: 'option', inner: { kind: 'int', name: 'i64' } }] });
    expect(a.methods[3]!.returns).toBeNull();
    expect(a.events[0]!.fields.map((f) => f.name)).toEqual(['from', 'asset', 'amount']);
  });

  it('takes Vec<u8> as Bytes', () => {
    const a = parseAbi('//! ```abi\n//! view code() -> Vec<u8>\n//! ```')!;
    expect(a.methods[0]!.returns).toEqual({ kind: 'bytes' });
  });

  it('returns null without a block', () => {
    expect(parseAbi('//! no block here\nfn main() {}')).toBeNull();
    expect(abiLines('')).toBeNull();
  });

  const bad: [string, RegExp][] = [
    ['view x()', /must return/],
    ['payable view x() -> u8', /takes no funds/],
    ['call x(a: Foo)', /unknown type Foo/],
    ['struct A { b: Option<A> }', /contains itself/],
    ['struct A { b: B }\n//! struct B { a: Vec<A> }', /contains itself/],
    ['call x(a: u8, a: u8)', /two fields named a/],
    ['call x()\n//! view x() -> u8', /declared twice/],
    ['init()\n//! init()', /declared twice/],
    ['call init()', /declare init without/],
    ['init() -> u8', /returns nothing/],
    ['struct u8 { a: u8 }', /built-in type/],
    ['enum E { }', /no variants/],
    ['call x(a: [u16; 2])', /only byte arrays/],
    ['call x(a: [u8; 0])', /array length/],
    ['view x() -> Vec<Option<Option<u8>>>', /Some\(None\) would read as None/],
    ['call x(a: u8', /expected/],
    ['frobnicate x()', /unknown declaration/],
    ['call x(a: u8) extra', /end of the declaration/],
    ['call x(a: u8 $)', /unexpected/],
  ];
  for (const [decl, error] of bad) {
    it(`refuses ${decl.split('\n')[0]}`, () => {
      expect(() => parseAbi(`//! \`\`\`abi\n//! ${decl}\n//! \`\`\``)).toThrow(error);
    });
  }

  it('reports the line of an error', () => {
    try {
      parseAbi('// header\n//! ```abi\n//! call ok()\n//! call bad(a: Nope)\n//! ```');
      expect.unreachable();
    } catch (e) {
      expect(e).toBeInstanceOf(AbiError);
      expect((e as AbiError).line).toBe(4);
    }
  });

  it('refuses an unclosed block', () => {
    expect(() => parseAbi('//! ```abi\n//! call x()')).toThrow(/not closed/);
    expect(() => parseAbi('//! ```abi\n//! call x()\nfn f() {}\n//! ```')).toThrow(/must end/);
  });
});

describe('codec', () => {
  const a = abi();
  const u128: AbiType = { kind: 'int', name: 'u128' };

  it('encodes integers little-endian', () => {
    expect(toHex(encodeValue(a, u128, 1n))).toBe(`01${'00'.repeat(15)}`);
    expect(toHex(encodeValue(a, { kind: 'int', name: 'i16' }, -2n))).toBe('feff');
    expect(toHex(encodeValue(a, { kind: 'int', name: 'u64' }, 0x0102n))).toBe('0201000000000000');
    expect(() => encodeValue(a, { kind: 'int', name: 'u8' }, 256n)).toThrow(CodecError);
    expect(() => encodeValue(a, { kind: 'int', name: 'i8' }, -129n)).toThrow(CodecError);
  });

  it('encodes strings, options, lists and tuples', () => {
    expect(toHex(encodeValue(a, { kind: 'string' }, 'ñ'))).toBe('02000000c3b1');
    expect(toHex(encodeValue(a, { kind: 'option', inner: { kind: 'bool' } }, null))).toBe('00');
    expect(toHex(encodeValue(a, { kind: 'option', inner: { kind: 'bool' } }, true))).toBe('0101');
    expect(toHex(encodeValue(a, { kind: 'vec', inner: { kind: 'int', name: 'u8' } }, [1n, 2n]))).toBe('020000000102');
    expect(toHex(encodeValue(a, { kind: 'bytes' }, bytes(9)))).toBe('0100000009');
    expect(toHex(encodeValue(a, { kind: 'tuple', items: [{ kind: 'bool' }, { kind: 'int', name: 'u16' }] }, [false, 3n]))).toBe('000300');
  });

  it('encodes enums with their declaration index', () => {
    const asset = new Uint8Array(32).fill(1);
    const value: Value = { variant: 'Call', values: [asset, 'go', bytes(7), [{ asset, amount: 5n }]] };
    const encoded = encodeValue(a, { kind: 'enum', name: 'Action' }, value);
    expect(encoded[0]).toBe(1);
    expect(decodeValue(a, { kind: 'enum', name: 'Action' }, encoded)).toEqual(value);
    expect(toHex(encodeValue(a, { kind: 'enum', name: 'Action' }, { variant: 'Stop', values: [] }))).toBe('03');
    expect(() => encodeValue(a, { kind: 'enum', name: 'Action' }, { variant: 'Nope', values: [] })).toThrow(/no variant/);
  });

  it('encodes arguments as the tuple of parameters', () => {
    const init = a.init!;
    const owner = new Uint8Array(32).fill(2);
    const args = encodeArgs(a, init.params.map((p) => p.type), [[owner], 1n]);
    expect(toHex(args)).toBe(`01000000${'02'.repeat(32)}01`);
    expect(encodeArgs(a, [], [])).toEqual(new Uint8Array());
  });

  it('keeps a leading byte order mark, as Rust does', () => {
    // A string, and an event's name: U+FEFF is part of the text (D-133)
    expect(decodeValue(a, { kind: 'string' }, bytes(3, 0, 0, 0, 0xef, 0xbb, 0xbf))).toBe('\ufeff');
    const named = (name: number[]) => bytes(name.length, 0, 0, 0, ...name, ...new Array(32).fill(4), ...new Array(32).fill(0), 9, ...new Array(15).fill(0));
    const deposit = [...'deposit'].map((c) => c.charCodeAt(0));
    expect(decodeEvent(a, named(deposit))?.name).toBe('deposit');
    expect(decodeEvent(a, named([0xef, 0xbb, 0xbf, ...deposit]))).toBeNull();
  });

  it('refuses a list of a zero-sized type at any length, as the contract SDK does', () => {
    const zero = parseAbi('//! ```abi\n//! struct Unit { }\n//! enum One { A }\n//! enum Two { A, B }\n//! view x() -> u8\n//! ```')!;
    for (const inner of [
      { kind: 'tuple', items: [] },
      { kind: 'tuple', items: [{ kind: 'tuple', items: [] }] },
      { kind: 'struct', name: 'Unit' },
      { kind: 'enum', name: 'One' },
    ] satisfies AbiType[]) {
      expect(() => decodeValue(zero, { kind: 'vec', inner }, bytes(0, 0, 0, 0)), JSON.stringify(inner)).toThrow(/list length/);
    }
    expect(decodeValue(zero, { kind: 'vec', inner: { kind: 'enum', name: 'Two' } }, bytes(1, 0, 0, 0, 1))).toEqual([{ variant: 'B', values: [] }]);
    expect(decodeValue(zero, { kind: 'vec', inner: { kind: 'option', inner: { kind: 'tuple', items: [] } } }, bytes(1, 0, 0, 0, 1))).toEqual([[]]);
  });

  it('decodes strictly', () => {
    expect(() => decodeValue(a, { kind: 'bool' }, bytes(2))).toThrow(/invalid bool/);
    expect(() => decodeValue(a, { kind: 'option', inner: { kind: 'bool' } }, bytes(2))).toThrow(/option tag/);
    expect(() => decodeValue(a, { kind: 'string' }, bytes(1, 0, 0, 0, 0xff))).toThrow(/UTF-8/);
    expect(() => decodeValue(a, u128, bytes(1))).toThrow(/end of data/);
    expect(() => decodeValue(a, { kind: 'int', name: 'u8' }, bytes(1, 2))).toThrow(/left after/);
    expect(() => decodeValue(a, { kind: 'vec', inner: { kind: 'int', name: 'u8' } }, bytes(0xff, 0xff, 0xff, 0x7f))).toThrow(/list length/);
    expect(decodeValue(a, { kind: 'int', name: 'i8' }, bytes(0xff))).toBe(-1n);
    expect(decodeReturn(a, null, new Uint8Array())).toEqual([]);
  });

  it('round-trips structs', () => {
    const value: Value = { asset: new Uint8Array(32).fill(3), amount: (1n << 128n) - 1n };
    expect(decodeValue(a, { kind: 'struct', name: 'Funds' }, encodeValue(a, { kind: 'struct', name: 'Funds' }, value))).toEqual(value);
  });

  it('decodes declared events and ignores others', () => {
    const from = new Uint8Array(32).fill(4);
    const data = encodeValue(a, { kind: 'tuple', items: [{ kind: 'string' }, { kind: 'id', name: 'Account' }, { kind: 'id', name: 'AssetId' }, u128] },
      ['deposit', from, new Uint8Array(32), 9n]);
    expect(decodeEvent(a, data)).toEqual({ name: 'deposit', fields: [
      { name: 'from', value: from }, { name: 'asset', value: new Uint8Array(32) }, { name: 'amount', value: 9n },
    ] });
    expect(decodeEvent(a, encodeValue(a, { kind: 'string' }, 'other'))).toBeNull();
    expect(decodeEvent(a, bytes(1, 2, 3))).toBeNull();
    expect(decodeEvent(a, new Uint8Array([...data, 0]))).toBeNull();
  });
});

describe('drafts', () => {
  const a = abi();
  const action: AbiType = { kind: 'enum', name: 'Action' };

  it('builds empty drafts for every type', () => {
    const d = emptyDraft(a, action);
    expect(d.variant).toBe(0);
    expect(d.items).toHaveLength(3);
    expect(emptyDraft(a, { kind: 'option', inner: { kind: 'string' } }).items).toHaveLength(1);
  });

  it('reads typed text', () => {
    const owner = new Uint8Array(32).fill(5);
    let d = withVariant(a, action, emptyDraft(a, action), 0);
    d = { ...d, items: [{ ...d.items[0]!, text: 'ksa' }, { ...d.items[1]!, text: `0x${toHex(owner)}` }, { ...d.items[2]!, text: '1_000' }] };
    expect(valueOf(a, action, d)).toEqual({ variant: 'Transfer', values: [new Uint8Array(32), owner, 1000n] });
  });

  it('names the field that is wrong', () => {
    const d = withVariant(a, action, emptyDraft(a, action), 2);
    d.items[0]!.text = '300';
    expect(() => valueOf(a, action, d)).toThrow(InputError);
    expect(() => valueOf(a, action, d)).toThrow(/SetThreshold\.1: must be between 0 and 255/);
  });

  it('accepts a Taproot address for an account on its network', () => {
    const account = valueOf(a, { kind: 'id', name: 'Account' },
      { text: 'tb1pqqqqp399et2xygdj5xreqhjjvcmzhxw4aywxecjdzew6hylgvsesf3hn0c', on: false, items: [], variant: 0 }, { network: 'signet' });
    expect(account).toBeInstanceOf(Uint8Array);
    expect((account as Uint8Array).length).toBe(32);
  });

  it('round-trips values through drafts', () => {
    const values: [AbiType, Value][] = [
      [action, { variant: 'Call', values: [new Uint8Array(32).fill(1), 'm', bytes(1, 2), [{ asset: new Uint8Array(32), amount: 3n }]] }],
      [{ kind: 'option', inner: { kind: 'int', name: 'i32' } }, -7n],
      [{ kind: 'option', inner: { kind: 'int', name: 'i32' } }, null],
      [{ kind: 'tuple', items: [{ kind: 'bool' }, { kind: 'fixed', len: 2 }] }, [true, bytes(0xab, 0xcd)]],
    ];
    for (const [type, value] of values) expect(valueOf(a, type, draftOf(a, type, value))).toEqual(value);
  });

  it('formats values on one line', () => {
    expect(formatValue(a, action, { variant: 'Stop', values: [] })).toBe('Stop');
    expect(formatValue(a, action, { variant: 'SetThreshold', values: [2n] })).toBe('SetThreshold(2)');
    expect(formatValue(a, { kind: 'struct', name: 'Funds' }, { asset: new Uint8Array(32), amount: 1n })).toBe('{ asset: KSA, amount: 1 }');
    expect(formatValue(a, { kind: 'option', inner: { kind: 'string' } }, null)).toBe('None');
    expect(formatValue(a, { kind: 'bytes' }, bytes())).toBe('0x');
  });

  it('writes an account as its address, and a contract as its ID even when the ID is a valid key', () => {
    const account: AbiType = { kind: 'id', name: 'Account' };
    // on the signet: the pool the factory made, whose ID is a key as well as a hash, the tutorial's pool, whose ID is not, and the
    // tutorial's account
    const pool = fromHex('2a220d0a8c6910fb557785daf4fdfed5eaea6f4690aeaedd9e0acb0ac008efb6', 32);
    const tutorialPool = fromHex('eb5ca55289984d64598a12303a0638ee9dd6edc50f4bebe6d43a7a46121fc745', 32);
    const person = fromHex('b7edd1c3bdca216534c8bd7882ac7cd0346d4508c9f3b6e7eb59d2136f43f09d', 32);
    const personAddress = 'tb1pklkarsaaegsk2dxgh4ug9tru6q6x63gge8emdeltt8fpxm6r7zwse04hlm';
    expect(accountText(pool, 'signet')).toBe('tb1p9g3q6z5vdyg0k4thshd0fl076h4w5m6xjzh2ahv7pt9s4sqga7mq6h98gc');
    expect(accountText(pool, 'signet', true)).toBe(toHex(pool));
    expect(accountText(tutorialPool, 'signet')).toBe(toHex(tutorialPool));
    expect(accountText(person, 'signet')).toBe(personAddress);
    expect(accountText(person)).toBe(toHex(person));
    const contract = (x: Uint8Array) => toHex(x) === toHex(pool);
    expect(formatValue(a, account, pool, { network: 'signet' })).toBe(accountText(pool, 'signet'));
    expect(formatValue(a, { kind: 'vec', inner: account }, [pool, person], { network: 'signet', contract }))
      .toBe(`[${toHex(pool)}, ${personAddress}]`);
  });

  it('reads and writes token IDs as #n', () => {
    const tokenId: AbiType = { kind: 'id', name: 'TokenId' };
    const twelve = tokenIdOf(12n);
    expect(toHex(twelve)).toBe(`${'00'.repeat(31)}0c`);
    expect(toHex(tokenIdOf(0x0102n))).toBe(`${'00'.repeat(30)}0102`);
    expect(serialOf(tokenIdOf((1n << 128n) - 1n))).toBe((1n << 128n) - 1n);
    expect(() => tokenIdOf(1n << 128n)).toThrow();
    for (const text of ['#12', '12', ' # 12 ', '#0_012', toHex(twelve), `0x${toHex(twelve)}`]) expect(parseTokenId(text)).toEqual(twelve);
    // Another collection's ID, with bytes in its first half: hex only
    const other = new Uint8Array(32).fill(7);
    expect(serialOf(other)).toBeNull();
    expect(tokenIdText(other)).toBe(toHex(other));
    expect(parseTokenId(toHex(other))).toEqual(other);
    // 64 decimal digits are an ID's hex, a shorter run of them a token number
    expect(parseTokenId('1'.repeat(64))).toEqual(new Uint8Array(32).fill(0x11));
    for (const bad of ['', '#', '#-1', '#1x', `#${1n << 128n}`, 'ff']) expect(() => parseTokenId(bad)).toThrow();
    expect(formatValue(a, tokenId, twelve)).toBe('#12');
    expect(formatValue(a, tokenId, other)).toBe(toHex(other));
    expect(draftOf(a, tokenId, twelve).text).toBe('#12');
    expect(valueOf(a, tokenId, draftOf(a, tokenId, other))).toEqual(other);
    expect(() => valueOf(a, tokenId, { text: '#x', on: false, items: [], variant: 0 }, {}, 'token_id')).toThrow(/token_id: enter #n, or 64 hex digits/);
  });
});
