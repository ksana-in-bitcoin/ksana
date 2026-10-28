/**
 * The shared vectors `tests/vectors/codec.json` (D-007) and the strictness of the codec: length limits, full consumption,
 * canonical encoding, strict JSON parsing.
 */

import assert from 'node:assert/strict';
import { test } from 'node:test';

import {
  type Action,
  BLOCK_HEADER_BASE_LEN,
  CodecError,
  DEVNET,
  MAX_CALL_FUNDS,
  SIGNET,
  type Payload,
  type TypeName,
  type VmOp,
  blockHash,
  blockHeaderEncodedLen,
  checkFunds,
  concatBytes,
  decode,
  decodePayload,
  decodeVmOp,
  encode,
  encodeBlockHeader,
  encodePayload,
  encodeVmOp,
  encodeVmOpUnsigned,
  entryData,
  fromHex,
  fromJson,
  ksaAssetId,
  opId,
  sortFunds,
  toHex,
  toJson,
  u32le,
} from '../src/index.js';
import { loadVectors, pseudoRandomBytes } from './helpers.js';

const vectors = loadVectors('codec.json');

/**
 * Decoding of external input: VmOp and payload use the protocol limits, the other types have no limit. The vectors are
 * devnet's: a payload's encoding depends on the network (D-080).
 */
function decodeAny(type: TypeName, bytes: Uint8Array): unknown {
  if (type === 'VmOp') return decodeVmOp(bytes, DEVNET);
  if (type === 'Payload') return decodePayload(bytes, DEVNET);
  return decode(type, bytes, Number.MAX_SAFE_INTEGER);
}

/** Encoding: a payload with the devnet parameters, the other types with the generic codec. */
function encodeAny(type: TypeName, value: unknown): Uint8Array {
  if (type === 'Payload') return encodePayload(value as Payload, DEVNET);
  return encode(type, value as never);
}

test('codec.json covers all consensus types and enum variants', () => {
  const types = new Set(vectors.valid.map((v: { type: string }) => v.type));
  for (const type of ['OutPoint', 'FeeMode', 'Action', 'SigScheme', 'VmOp', 'BtcAnchor', 'BlockHeader', 'Entry', 'EntryData', 'Block', 'Payload']) {
    assert.ok(types.has(type), type);
  }
  // same variant list as codec_vectors_valid on the Rust side (D-078): every variant has a vector
  const variants: [TypeName, string[]][] = [
    ['FeeMode', ['Token', 'BtcNative']],
    ['Action', ['Deploy', 'Call', 'AssetTransfer']],
    ['SigScheme', ['Bip340', 'Bip322Simple']],
    ['Entry', ['Forced', 'Sponsored', 'Unlock']],
    ['EntryData', ['Forced', 'Sponsored', 'Unlock']],
    ['Payload', ['Settlement', 'NativeOp', 'Headers']],
  ];
  for (const [type, names] of variants) {
    const seen = vectors.valid
      .filter((v: { type: string }) => v.type === type)
      .map((v: { value: unknown }) => (fromJson(type, v.value) as { kind: string }).kind);
    assert.deepEqual([...new Set(seen)].sort(), [...names].sort(), type);
  }
  assert.ok(vectors.valid.length >= 38);
  assert.ok(vectors.invalid.length >= 12);
});

for (const v of vectors.valid) {
  test(`codec.json valid: ${v.type}: ${v.name}`, () => {
    const type = v.type as TypeName;
    const value = fromJson(type, v.value);
    assert.equal(toHex(encodeAny(type, value)), v.borsh, 'JSON → Borsh');
    const decoded = decodeAny(type, fromHex(v.borsh));
    assert.deepEqual(decoded, value, 'Borsh → value');
    assert.deepEqual(toJson(type, decoded as never), v.value, 'Borsh → JSON');
    if (v.op_id !== undefined) assert.equal(toHex(opId(decoded as VmOp)), v.op_id, 'op_id');
    if (v.block_hash !== undefined) {
      const header = fromJson('BlockHeader', v.value);
      assert.equal(encodeBlockHeader(header).length, blockHeaderEncodedLen(header));
      assert.equal(encodeBlockHeader(header).length, BLOCK_HEADER_BASE_LEN + 32 * header.parents.length);
      assert.equal(toHex(blockHash(header)), v.block_hash, 'block_hash');
    }
  });
}

for (const v of vectors.invalid) {
  test(`codec.json invalid: ${v.type}: ${v.name}`, () => {
    assert.throws(() => decodeAny(v.type as TypeName, fromHex(v.borsh)), (e: unknown) => {
      assert.ok(e instanceof CodecError, String(e));
      // when error is given, rejected by the specified rule (same error message as on the Rust side, D-075)
      if (v.error !== undefined) assert.ok(e.message.includes(v.error), `"${e.message}" does not contain "${v.error}"`);
      return true;
    });
  });
}

test('EntryData and Entry encode byte-for-byte identically except for Forced (D-003)', () => {
  const entries = vectors.valid.filter((v: { type: string }) => v.type === 'Entry');
  const data = vectors.valid.filter((v: { type: string }) => v.type === 'EntryData');
  assert.equal(entries.length, 4);
  for (const [i, e] of entries.entries()) {
    const entry = fromJson('Entry', e.value);
    const expected = data[i];
    assert.equal(toHex(encode('EntryData', entryData(entry))), expected.borsh);
    if (entry.kind !== 'Forced') assert.equal(e.borsh, expected.borsh);
  }
});

test('the signing message content is the full encoding without the trailing sig', () => {
  for (const v of vectors.valid.filter((x: { type: string }) => x.type === 'VmOp')) {
    const op = fromJson('VmOp', v.value);
    const full = encodeVmOp(op);
    const unsigned = encodeVmOpUnsigned(op);
    assert.deepEqual(concatBytes(unsigned, encode('SigScheme', op.sig)), full);
  }
});

test('the length limit is checked before decoding', () => {
  const op = fromJson('VmOp', vectors.valid.find((v: { type: string }) => v.type === 'VmOp').value);
  // MAX_OP_BYTES + 1 bytes: rejected as TooLong without attempting to decode
  assert.throws(() => decodeVmOp(new Uint8Array(DEVNET.max_op_bytes + 1), DEVNET), /exceeds the limit 65536/);
  assert.throws(() => decodePayload(new Uint8Array(DEVNET.max_block_data_bytes + 1), DEVNET), /exceeds the limit 81920/);
  // a valid VmOp exactly at the limit: deploying code within 64 KiB
  const filler = 64 * 1024 - encodeVmOp({ ...op, action: { kind: 'Deploy', code: new Uint8Array(0), init_args: new Uint8Array(0), funds: [] } }).length;
  const big: VmOp = { ...op, action: { kind: 'Deploy', code: new Uint8Array(filler), init_args: new Uint8Array(0), funds: [] } };
  const bytes = encodeVmOp(big);
  assert.equal(bytes.length, DEVNET.max_op_bytes);
  assert.deepEqual(decodeVmOp(bytes, DEVNET), big);
  const tooBig: VmOp = { ...big, action: { kind: 'Deploy', code: new Uint8Array(filler + 1), init_args: new Uint8Array(0), funds: [] } };
  assert.throws(() => decodeVmOp(encodeVmOp(tooBig), DEVNET), /exceeds the limit/);
  // the limit can be given by network parameters
  assert.deepEqual(decodeVmOp(bytes, SIGNET), big);
});

/** Encoding of a Call action: variant index 1, contract, method name (u32 length + bytes), empty args, no attached funds. */
function rawCall(methodBytes: Uint8Array): Uint8Array {
  return concatBytes(Uint8Array.of(1), new Uint8Array(32), u32le(methodBytes.length), methodBytes, u32le(0), u32le(0));
}

test('the method name must be valid UTF-8 (non-canonical encodings and surrogates are rejected)', () => {
  const ok = rawCall(new TextEncoder().encode('überweisen'));
  assert.equal((decode('Action', ok, 1000) as Extract<Action, { kind: 'Call' }>).method, 'überweisen');
  for (const bad of [
    [0xc0, 0x80], // overlong encoding of U+0000
    [0xed, 0xa0, 0x80], // surrogate U+D800
    [0xff], // invalid byte
    [0xe8, 0xbd], // truncated multi-byte sequence
    [0x80], // lone continuation byte
    [0xf4, 0x90, 0x80, 0x80], // beyond U+10FFFF
  ]) {
    assert.throws(() => decode('Action', rawCall(Uint8Array.from(bad)), 1000), CodecError, toHex(Uint8Array.from(bad)));
  }
});

test('trailing bytes, truncation and invalid tags are all rejected', () => {
  const header = fromJson('BlockHeader', vectors.valid.find((v: { type: string }) => v.type === 'BlockHeader').value);
  const bytes = encodeBlockHeader(header);
  assert.deepEqual(decode('BlockHeader', bytes, bytes.length), header);
  assert.throws(() => decode('BlockHeader', bytes, bytes.length - 1), CodecError);
  assert.throws(() => decode('BlockHeader', concatBytes(bytes, Uint8Array.of(0)), 1000), /trailing bytes/);
  assert.throws(() => decode('BlockHeader', bytes.slice(0, bytes.length - 1), 1000), CodecError);
  // an Option tag can only be 0 or 1
  const block = concatBytes(bytes, u32le(0), Uint8Array.of(1), u32le(1), Uint8Array.of(7), u32le(1), Uint8Array.of(8));
  assert.deepEqual(decode('Block', block, 1000).settlement, { commit: Uint8Array.of(7), reveal: Uint8Array.of(8) });
  // only the commit transaction, the reveal transaction missing (D-081): truncated
  assert.throws(() => decode('Block', block.slice(0, block.length - 5), 1000), CodecError);
  const badOption = concatBytes(bytes, u32le(0), Uint8Array.of(2));
  assert.throws(() => decode('Block', badOption, 1000), CodecError);
  // fixed-size and variable-length byte strings
  assert.throws(() => decode('OutPoint', new Uint8Array(35), 100), CodecError);
  assert.throws(() => decode('SigScheme', concatBytes(Uint8Array.of(1), u32le(5), new Uint8Array(4)), 100), CodecError);
});

test('value ranges are checked before encoding', () => {
  const op = fromJson('VmOp', vectors.valid.find((v: { type: string }) => v.type === 'VmOp').value);
  const cases: [string, VmOp][] = [
    ['version 256', { ...op, version: 256 }],
    ['version negative', { ...op, version: -1 }],
    ['chain_id fractional', { ...op, chain_id: 1.5 }],
    ['chain_id exceeds u32', { ...op, chain_id: 2 ** 32 }],
    ['nonce negative', { ...op, nonce: -1n }],
    ['nonce exceeds u64', { ...op, nonce: 1n << 64n }],
    ['nonce not a bigint', { ...op, nonce: 1 as unknown as bigint }],
    ['max_gas exceeds u64', { ...op, max_gas: 1n << 64n }],
    ['account 31 bytes', { ...op, account: new Uint8Array(31) }],
    ['Token exceeds u128', { ...op, fee: { kind: 'Token', amount: 1n << 128n } }],
    ['lone surrogate', { ...op, action: { kind: 'Call', contract: new Uint8Array(32), method: 'a\ud800', args: new Uint8Array(0), funds: [] } }],
    ['unknown variant', { ...op, fee: { kind: 'Free' } as unknown as VmOp['fee'] }],
    ['Bip340 63 bytes', { ...op, sig: { kind: 'Bip340', bytes: new Uint8Array(63) } }],
  ];
  for (const [name, bad] of cases) {
    assert.throws(() => encodeVmOp(bad), CodecError, name);
  }
  // boundary values can be encoded
  const max: VmOp = { ...op, version: 255, chain_id: 2 ** 32 - 1, nonce: (1n << 64n) - 1n, max_gas: 0n };
  assert.deepEqual(decodeVmOp(encodeVmOp(max), DEVNET), max);
});

test('JSON: unknown fields, missing fields and non-canonical notation are rejected (D-007)', () => {
  const v = vectors.valid.find((x: { type: string }) => x.type === 'VmOp').value;
  assert.throws(() => fromJson('VmOp', { ...v, extra: 1 }), /unknown field/);
  const { nonce: _nonce, ...missing } = v;
  assert.throws(() => fromJson('VmOp', missing), /missing field/);
  for (const nonce of ['01', '+1', '-1', '1.0', '', ' 1', '1e3', '0x10', '18446744073709551616', 1]) {
    assert.throws(() => fromJson('VmOp', { ...v, nonce }), CodecError, String(nonce));
  }
  for (const version of [256, -1, 1.5, '1', null]) {
    assert.throws(() => fromJson('VmOp', { ...v, version }), CodecError, String(version));
  }
  assert.throws(() => fromJson('VmOp', { ...v, account: v.account.slice(2) }), /32 bytes/);
  assert.throws(() => fromJson('VmOp', { ...v, account: `0x${v.account.slice(2)}` }), CodecError);
  // same as Rust's hex crate: upper-case hex is accepted, the output is lower case
  const upper = fromJson('VmOp', { ...v, account: v.account.toUpperCase() });
  assert.equal((toJson('VmOp', upper) as { account: string }).account, v.account);
  // enums: a unit variant is a string (serde also accepts {"BtcNative": null}); unknown variants and multi-key objects are rejected
  assert.deepEqual(fromJson('FeeMode', 'BtcNative'), { kind: 'BtcNative' });
  assert.deepEqual(fromJson('FeeMode', { BtcNative: null }), { kind: 'BtcNative' });
  assert.throws(() => fromJson('FeeMode', 'Token'), CodecError);
  assert.throws(() => fromJson('FeeMode', { Free: {} }), /unknown variant/);
  assert.throws(() => fromJson('FeeMode', { Token: { amount: '1' }, BtcNative: null }), /exactly one key/);
  assert.throws(() => fromJson('FeeMode', { Token: { amount: '1', extra: '2' } }), /unknown field/);
  assert.equal(toJson('FeeMode', { kind: 'BtcNative' }), 'BtcNative');
  assert.deepEqual(toJson('SigScheme', { kind: 'Bip322Simple', bytes: Uint8Array.of(1, 2) }), { Bip322Simple: '0102' });
});

test('encoding round trip of pseudo-random VmOps', () => {
  for (let seed = 1; seed <= 200; seed++) {
    const r = pseudoRandomBytes(seed, 160);
    const pick = r[0]! % 3;
    // the codec does not check the attached-funds rules (D-073); any value must round-trip
    const funds = Array.from({ length: r[21]! % 3 }, (_, i) => ({ asset: r.slice(60 + i, 92 + i), amount: BigInt(r[22 + i]!) }));
    const action: Action =
      pick === 0
        ? { kind: 'Deploy', code: r.slice(1, 1 + (r[1]! % 100)), init_args: r.slice(2, 2 + (r[2]! % 7)), funds }
        : pick === 1
          ? { kind: 'Call', contract: r.slice(10, 42), method: ['', 'mint', 'kṣaṇa', '😄'][r[3]! % 4]!, args: r.slice(50, 50 + (r[4]! % 30)), funds }
          : { kind: 'AssetTransfer', asset: ksaAssetId(), to: r.slice(42, 74), amount: BigInt(`0x${toHex(r.slice(80, 96))}`) };
    const op: VmOp = {
      version: r[6]!,
      chain_id: new DataView(r.buffer).getUint32(7, true),
      account: r.slice(100, 132),
      nonce: new DataView(r.buffer).getBigUint64(11, true),
      fee: r[5]! % 2 === 0 ? { kind: 'Token', amount: BigInt(`0x${toHex(r.slice(132, 148))}`) } : { kind: 'BtcNative' },
      max_gas: new DataView(r.buffer).getBigUint64(21, true),
      action,
      sig: r[29]! % 2 === 0 ? { kind: 'Bip340', bytes: r.slice(30, 94) } : { kind: 'Bip322Simple', bytes: r.slice(30, 30 + (r[31]! % 120)) },
    };
    const bytes = encodeVmOp(op);
    assert.deepEqual(decodeVmOp(bytes, DEVNET), op);
    assert.deepEqual(fromJson('VmOp', JSON.parse(JSON.stringify(toJson('VmOp', op)))), op);
  }
});

test('a Node.js Buffer (a Uint8Array subclass over the shared memory pool) as input', () => {
  const v = vectors.valid.find((x: { type: string }) => x.type === 'VmOp');
  // small Buffer.from data lives in the 8 KiB shared memory pool, with a non-zero byteOffset
  const pooled = Buffer.from(v.borsh, 'hex');
  const op = decodeVmOp(pooled, DEVNET);
  assert.deepEqual(op, fromJson('VmOp', v.value));
  const withBuffers: VmOp = { ...op, account: Buffer.from(op.account), sig: { kind: 'Bip340', bytes: Buffer.from(op.sig.bytes) } };
  assert.equal(toHex(encodeVmOp(withBuffers)), v.borsh);
});

test('attached funds: JSON representation and rules (D-073)', () => {
  const a = { asset: new Uint8Array(32).fill(1), amount: 7n };
  const b = { asset: new Uint8Array(32).fill(2), amount: 340282366920938463463374607431768211455n };
  assert.deepEqual(toJson('Funds', a), { asset: '01'.repeat(32), amount: '7' });
  assert.deepEqual(fromJson('Funds', toJson('Funds', b)), b);
  assert.deepEqual(toHex(encode('Funds', a)), `${'01'.repeat(32)}07${'00'.repeat(15)}`);
  // ascending, amounts greater than 0, within the limit
  checkFunds([]);
  checkFunds([a, b]);
  assert.deepEqual(sortFunds([b, a]), [a, b]);
  assert.throws(() => checkFunds([b, a]), /strictly ascending/);
  assert.throws(() => checkFunds([a, a]), /strictly ascending/);
  assert.throws(() => checkFunds([{ ...a, amount: 0n }]), /greater than 0/);
  const many = Array.from({ length: MAX_CALL_FUNDS + 1 }, (_, i) => ({ asset: new Uint8Array(32).fill(i), amount: 1n }));
  checkFunds(many.slice(0, MAX_CALL_FUNDS));
  assert.throws(() => checkFunds(many), /exceed 16 items/);
});
