/**
 * Compact headers in a payload (D-075, D-079), matching the unit tests of `compact.rs` on the Rust side: the size of a
 * chain of empty blocks (the state root is not written when unchanged), parallel blocks and merges, differences wrapping
 * mod 2^64, two-byte references, the parent count limit; round trips of pseudo-random sequences and byte-by-byte mutation
 * (decoding either fails or yields a value that re-encodes byte-for-byte identically). Byte-for-byte agreement between
 * Rust and TS is guaranteed by the shared vectors `tests/vectors/codec.json` (`codec.test.ts`).
 */

import assert from 'node:assert/strict';
import { test } from 'node:test';

import {
  type BlockHeader,
  CodecError,
  DEVNET,
  type Payload,
  blockHash,
  blockHeaderEncodedLen,
  decodePayload,
  emptyEntriesRoot,
  encodePayload,
  encodedHeadersLen,
} from '../src/index.js';
import { pseudoRandomBytes } from './helpers.js';

function filled(byte: number): Uint8Array {
  return new Uint8Array(32).fill(byte);
}

function header(height: bigint, parents: Uint8Array[], producer: number): BlockHeader {
  return {
    version: 2,
    height,
    parents,
    btc_anchor: { height: 100, hash: filled(7) },
    entries_root: emptyEntriesRoot(),
    state_root: filled(Number(height % 256n)),
    producer: filled(producer),
    timestamp: 1_700_000_000n + height,
    bits: 0x207f_ffff,
    nonce: (height * 0x9e37_79b9n) % (1n << 64n),
  };
}

/** A chain: each header's parent is the previous one; the first header's parent is not in the sequence. */
function chain(n: number): BlockHeader[] {
  const headers: BlockHeader[] = [];
  for (let i = 0; i < n; i++) {
    const last = headers[headers.length - 1];
    headers.push(header(10n + BigInt(i), [last === undefined ? filled(1) : blockHash(last)], 5));
  }
  return headers;
}

/** Round trip of a `Headers` payload; returns the encoding. */
function roundtrip(headers: BlockHeader[]): Uint8Array {
  const payload: Payload = { kind: 'Headers', headers, bodies: [], payee: filled(9) };
  const bytes = encodePayload(payload, DEVNET);
  const decoded = decodePayload(bytes, DEVNET);
  assert.deepEqual(decoded, payload);
  assert.deepEqual(encodePayload(decoded, DEVNET), bytes);
  // variant index 1 byte, body count 1 byte (D-077, varint, D-080), payee account 32 bytes (D-095)
  assert.equal(encodedHeadersLen(headers, blockHash), bytes.length - 1 - 1 - 32);
  return bytes;
}

test('headers whose state root changes every block take 46 bytes each (the first writes all fields)', () => {
  const headers = chain(100);
  const bytes = roundtrip(headers);
  // variant index 1 + count 4; the first header: flags, version, height, parent count and reference + 32, anchor, state_root,
  // producer, timestamp, bits, nonce; each one after it: flags 1 + height 1 + parents 2 + state_root 32 + producer 1 +
  // timestamp 1 + nonce 8; finally the body count 1 (D-077, D-080) and the payee account 32 (D-095)
  assert.equal(bytes.length, 1 + 4 + (1 + 4 + 1 + 1 + 1 + 32 + 36 + 32 + 1 + 32 + 5 + 4 + 8) + 99 * 46 + 1 + 32);
  const full = headers.reduce((sum, h) => sum + blockHeaderEncodedLen(h), 0);
  assert.ok(bytes.length * 4 < full, `${bytes.length} vs ${full}`);
});

test('empty blocks keep the state root (D-079): later headers do not write it, 14 bytes each; the first header cannot set the flag, equal ones must', () => {
  const headers = chain(100).map((h) => ({ ...h, state_root: filled(3) }));
  for (let i = 1; i < headers.length; i++) headers[i]!.parents = [blockHash(headers[i - 1]!)];
  const bytes = roundtrip(headers);
  const first = 1 + 4 + 1 + 1 + 1 + 32 + 36 + 32 + 1 + 32 + 5 + 4 + 8;
  assert.equal(bytes.length, 1 + 4 + first + 99 * 14 + 1 + 32);
  // a block in the middle changes the state root: it writes the 32 bytes, and later blocks equal to it do not write it again
  const changed = headers.map((h, i) => ({ ...h, state_root: filled(i >= 50 ? 4 : 3) }));
  for (let i = 1; i < changed.length; i++) changed[i]!.parents = [blockHash(changed[i - 1]!)];
  assert.equal(roundtrip(changed).length, bytes.length + 32);
  // the first header sets the flag: rejected
  const bad = bytes.slice();
  bad[5] = bad[5]! | 0x10;
  assert.throws(() => decodePayload(bad, DEVNET), /the first header cannot refer to a previous one/);
  // equal to the previous one yet writes the 32 bytes: rejected
  const two = encodePayload({ kind: 'Headers', headers: headers.slice(0, 2), bodies: [], payee: filled(9) }, DEVNET);
  const at = 1 + 4 + first;
  const written = new Uint8Array([
    ...two.slice(0, at),
    two[at]! & ~0x10,
    ...two.slice(at + 1, at + 4),
    ...filled(3),
    ...two.slice(at + 4),
  ]);
  assert.throws(() => decodePayload(written, DEVNET), /state root equal to the previous one must use the flag/);
});

test('parallel blocks and merges: parents in the sequence and the most recent producer are referenced, changed fields are written out', () => {
  const headers = chain(3);
  const [a, b] = [blockHash(headers[1]!), blockHash(headers[2]!)];
  const merge = header(13n, [b, a, filled(9)], 6);
  merge.btc_anchor = { height: 101, hash: filled(7) };
  merge.bits = 0x1e00_ffff;
  merge.version = 3;
  merge.entries_root = filled(4);
  merge.timestamp = 1_699_999_000n;
  headers.push(merge);
  headers.push(header(14n, [blockHash(merge)], 5));
  roundtrip(headers);
  roundtrip([]);
  // Settlement payload: the headers are followed by the plain Borsh-encoded entries
  const settlement: Payload = {
    kind: 'Settlement',
    headers,
    entries: [
      { kind: 'Forced', btc_txid: filled(1) },
      { kind: 'Unlock', height: 12n, index: 2 },
    ],
    bodies: [{ header: 1, entries: [{ kind: 'Unlock', height: 2n ** 64n - 1n, index: 0xffff_ffff }] }],
  };
  assert.deepEqual(decodePayload(encodePayload(settlement, DEVNET), DEVNET), settlement);
});

test('height and timestamp differences are taken mod 2^64: any value can be encoded', () => {
  const headers = chain(4);
  const values: [bigint, bigint][] = [
    [2n ** 64n - 1n, 0n],
    [0n, 2n ** 64n - 1n],
    [2n ** 63n, 5n],
    [3n, 2n ** 63n + 7n],
  ];
  values.forEach(([height, timestamp], i) => {
    headers[i]!.height = height;
    headers[i]!.timestamp = timestamp;
  });
  roundtrip(headers);
});

test('a reference 128 or more headers back in the sequence takes two bytes', () => {
  const headers = chain(130);
  const before = encodedHeadersLen(headers, blockHash);
  headers.push(header(200n, [blockHash(headers[0]!)], 5));
  roundtrip(headers);
  // flags 1 + height delta 1 + parent count 1 + reference 130 (two bytes) + state_root 32 + producer 1 + timestamp delta 1 + nonce 8
  assert.equal(encodedHeadersLen(headers, blockHash) - before, 1 + 1 + 1 + 2 + 32 + 1 + 1 + 8);
});

test('parent count limit 255: symmetric in encoding and decoding', () => {
  const many = header(1n, Array.from({ length: 256 }, () => filled(3)), 5);
  assert.throws(() => encodePayload({ kind: 'Headers', headers: [many], bodies: [], payee: filled(9) }, DEVNET), CodecError);
  many.parents.pop();
  roundtrip([many]);
});

/** A pseudo-random u64. */
function randomU64(seed: number): bigint {
  return pseudoRandomBytes(seed, 8).reduce((v, b) => (v << 8n) | BigInt(b), 0n);
}

/** A pseudo-random header sequence: parents are chosen from earlier headers in the sequence and from hashes outside it, fields from small value sets (so that equal and different values are both common). */
function randomHeaders(seed: number): BlockHeader[] {
  const r = pseudoRandomBytes(seed, 4096);
  let at = 0;
  const next = (): number => r[at++ % r.length]!;
  const headers: BlockHeader[] = [];
  const count = next() % 12;
  for (let i = 0; i < count; i++) {
    const parents: Uint8Array[] = [];
    for (let p = next() % 5; p > 0; p--) {
      const earlier = headers[next() % (headers.length + 1)];
      parents.push(earlier === undefined || next() % 4 === 0 ? filled(next()) : blockHash(earlier));
    }
    const previous = headers[headers.length - 1];
    const small = (base: bigint): bigint => (previous === undefined || next() % 3 === 0 ? randomU64(seed * 131 + i) : base + BigInt(next() % 7) - 3n);
    let height = small(previous?.height ?? 0n);
    let timestamp = small(previous?.timestamp ?? 0n);
    height = ((height % 2n ** 64n) + 2n ** 64n) % 2n ** 64n;
    timestamp = ((timestamp % 2n ** 64n) + 2n ** 64n) % 2n ** 64n;
    headers.push({
      version: next() % 2,
      height,
      parents,
      btc_anchor: { height: next() % 2, hash: filled(next() % 2) },
      entries_root: next() % 2 === 0 ? emptyEntriesRoot() : filled(next() % 3),
      state_root: filled(next() % 3),
      producer: filled(next() % 3),
      timestamp,
      bits: next() % 2,
      nonce: randomU64(seed * 7 + i),
    });
  }
  return headers;
}

test('pseudo-random sequences round-trip; after a byte-by-byte mutation decoding either fails or re-encodes byte-for-byte identically', () => {
  for (let seed = 1; seed <= 300; seed++) {
    const headers = randomHeaders(seed);
    const bytes = roundtrip(headers);
    const flip = pseudoRandomBytes(seed + 10_000, 8);
    for (let k = 0; k < 4; k++) {
      const mutated = bytes.slice();
      const index = ((flip[2 * k]! << 8) | flip[2 * k + 1]!) % mutated.length;
      mutated[index] = mutated[index]! ^ (1 << (flip[k]! % 8));
      let decoded: Payload;
      try {
        decoded = decodePayload(mutated, DEVNET);
      } catch (e) {
        assert.ok(e instanceof CodecError, String(e));
        continue;
      }
      assert.deepEqual(encodePayload(decoded, DEVNET), mutated);
    }
  }
});

test('a header sequence holds at most MAX_SETTLEMENT_HEADERS headers (§7.1, D-100): one more fails to decode', () => {
  const params = { ...DEVNET, max_settlement_headers: 3 };
  const republish = (n: number): Payload => ({ kind: 'Headers', headers: chain(n), bodies: [], payee: filled(9) });
  assert.deepEqual(decodePayload(encodePayload(republish(3), params), params), republish(3));
  assert.throws(() => decodePayload(encodePayload(republish(4), params), params), /too many headers/);
  // the count is checked before any header is read
  assert.throws(() => decodePayload(new Uint8Array([2, 4, 0, 0, 0]), params), /too many headers/);
});
