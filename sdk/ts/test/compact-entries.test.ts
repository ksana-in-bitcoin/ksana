/**
 * Compact encoding of the entries in a payload (D-080), matching the unit tests of `compact_entries.rs` on the Rust side:
 * the size of a path B transfer shrinks with the key dictionary, a key repeated within an entry references the position
 * just added by this entry, round trips of the various variants, the chain ID is not written into the payload.
 * Byte-for-byte agreement between Rust and TS and the rejection of non-canonical encodings are guaranteed by the shared
 * vectors `tests/vectors/codec.json` (`codec.test.ts`).
 */

import assert from 'node:assert/strict';
import { test } from 'node:test';

import {
  ByteReader,
  CodecError,
  DEVNET,
  type EntryData,
  EntryDecoder,
  EntryEncoder,
  type Payload,
  VMOP_VERSION,
  decodePayload,
  encode,
  encodePayload,
} from '../src/index.js';

function filled(byte: number, length = 32): Uint8Array {
  return new Uint8Array(length).fill(byte);
}

function transfer(account: number, to: number, nonce: bigint): EntryData {
  return {
    kind: 'Sponsored',
    op: {
      version: VMOP_VERSION,
      chain_id: DEVNET.chain_id,
      account: filled(account),
      nonce,
      fee: { kind: 'Token', amount: 2n },
      max_gas: 100_000n,
      action: { kind: 'AssetTransfer', asset: new Uint8Array(32), to: filled(to), amount: 1_000n },
      sig: { kind: 'Bip340', bytes: filled(7, 64) },
    },
  };
}

function decodeEntries(bytes: Uint8Array, chainId = DEVNET.chain_id): EntryData[] {
  const reader = new ByteReader(bytes, 'entries');
  const entries = new EntryDecoder(chainId).entries(reader);
  assert.equal(reader.remaining, 0);
  return entries;
}

test('path B transfer: 217 bytes in Borsh, 143 bytes when the keys first appear, 79 bytes when they are all in the dictionary', () => {
  const first = transfer(1, 2, 0n);
  assert.equal(encode('EntryData', first).length, 217);
  const encoder = new EntryEncoder(DEVNET.chain_id);
  assert.equal(encoder.encodedLength(first), 143);
  assert.equal(encoder.encodedLength(first), 143, 'computing the length alone does not change the dictionary');
  assert.equal(encoder.encode(first).length, 143);
  const again = transfer(1, 2, 1n);
  assert.equal(encoder.encodedLength(again), 79);
  const back = transfer(2, 1, 0n);
  const bytes = new EntryEncoder(DEVNET.chain_id).encodeEntries([first, again, back]);
  assert.equal(bytes.length, 1 + 143 + 79 + 79);
  assert.deepEqual(decodeEntries(bytes), [first, again, back]);
  // the chain ID is filled back in by the decoder
  const other = decodeEntries(bytes, 1002);
  assert.equal(other[0]!.kind === 'Sponsored' && other[0]!.op.chain_id, 1002);
});

test('a key appearing a second time in the same entry references the position this entry just added to the dictionary', () => {
  const selfTransfer = transfer(5, 5, 0n);
  const encoder = new EntryEncoder(DEVNET.chain_id);
  assert.equal(encoder.encodedLength(selfTransfer), 143 - 32);
  assert.equal(encoder.encode(selfTransfer).length, 143 - 32);
  assert.equal(encoder.encodedLength(selfTransfer), 79);
});

test('the various variants and integer limits round-trip; bodies precede the entries of this block and share the key dictionary', () => {
  const call: EntryData = {
    kind: 'Sponsored',
    op: {
      version: VMOP_VERSION,
      chain_id: DEVNET.chain_id,
      account: filled(9),
      nonce: 2n ** 64n - 1n,
      fee: { kind: 'BtcNative' },
      max_gas: 0n,
      action: {
        kind: 'Call',
        contract: filled(8),
        method: 'échanger',
        args: Uint8Array.of(1, 2, 3),
        funds: [
          { asset: new Uint8Array(32), amount: 2n ** 128n - 1n },
          { asset: filled(9), amount: 1n },
        ],
      },
      sig: { kind: 'Bip322Simple', bytes: filled(4, 70) },
    },
  };
  const deploy: EntryData = {
    kind: 'Sponsored',
    op: {
      version: 1,
      chain_id: DEVNET.chain_id,
      account: filled(8),
      nonce: 0n,
      fee: { kind: 'Token', amount: 0n },
      max_gas: 2n ** 64n - 1n,
      action: { kind: 'Deploy', code: new Uint8Array(300), init_args: new Uint8Array(0), funds: [] },
      sig: { kind: 'Bip340', bytes: filled(1, 64) },
    },
  };
  const entries: EntryData[] = [
    { kind: 'Forced', btc_txid: filled(3) },
    { kind: 'Unlock', height: 2n ** 64n - 1n, index: 0xffff_ffff },
    call,
    deploy,
    transfer(9, 8, 3n),
  ];
  assert.deepEqual(decodeEntries(new EntryEncoder(DEVNET.chain_id).encodeEntries(entries)), entries);
  const header = (height: bigint) => ({
    version: 2,
    height,
    parents: [filled(1)],
    btc_anchor: { height: 100, hash: filled(2) },
    entries_root: filled(3),
    state_root: filled(Number(height)),
    producer: filled(5),
    timestamp: 1_700_000_000n + height,
    bits: 0x207f_ffff,
    nonce: height,
  });
  const payload: Payload = {
    kind: 'Settlement',
    headers: [header(1n), header(2n)],
    entries: [transfer(1, 2, 1n)],
    bodies: [{ header: 0, entries: [transfer(1, 2, 0n)] }],
  };
  const bytes = encodePayload(payload, DEVNET);
  assert.deepEqual(decodePayload(bytes, DEVNET), payload);
  // the transfer of this block references the two accounts seen in the body: 64 bytes shorter than the one in the body
  const alone = encodePayload({ ...payload, bodies: [] }, DEVNET);
  const bodyOnly = encodePayload({ ...payload, entries: [] }, DEVNET);
  assert.equal(bytes.length - bodyOnly.length, 79);
  assert.equal(alone.length - encodePayload({ ...payload, bodies: [], entries: [] }, DEVNET).length, 143);
});

test('a method name that starts with U+FEFF keeps it, as in Rust (audit F-18)', () => {
  const entry = transfer(1, 2, 0n);
  if (entry.kind !== 'Sponsored') throw new Error('expected a sponsored entry');
  entry.op.action = { kind: 'Call', contract: filled(8), method: '\u{feff}x', args: new Uint8Array(0), funds: [] };
  const bytes = new EntryEncoder(DEVNET.chain_id).encodeEntries([entry]);
  // the method is written as its UTF-8 bytes: length 4, then EF BB BF 78
  const at = bytes.findIndex((_, i) => bytes[i] === 4 && bytes[i + 1] === 0xef && bytes[i + 2] === 0xbb);
  assert.deepEqual([...bytes.subarray(at, at + 5)], [4, 0xef, 0xbb, 0xbf, 0x78]);
  assert.deepEqual(decodeEntries(bytes), [entry]);
  const payload: Payload = { kind: 'Settlement', headers: [], entries: [entry], bodies: [] };
  assert.deepEqual(decodePayload(encodePayload(payload, DEVNET), DEVNET), payload);
});

test('a VmOp whose chain ID differs from the network cannot be written into a payload', () => {
  const entry = transfer(1, 2, 0n);
  if (entry.kind === 'Sponsored') entry.op.chain_id = 7;
  assert.throws(() => new EntryEncoder(DEVNET.chain_id).encode(entry), CodecError);
  const payload: Payload = { kind: 'Settlement', headers: [], entries: [entry], bodies: [] };
  assert.throws(() => encodePayload(payload, DEVNET), /chain_id does not match the network/);
  const other = { ...DEVNET, chain_id: 7 };
  assert.deepEqual(decodePayload(encodePayload(payload, other), other), payload);
});
