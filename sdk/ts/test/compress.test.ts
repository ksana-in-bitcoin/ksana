/**
 * Compressed envelope data (`src/compress.ts`, D-141): the shared vectors `tests/vectors/compress.json` case by case, the same
 * file Rust's `crates/ksana-btc/tests/compress.rs` verifies; the content checksum's hash; the bound on what a frame allocates.
 */

import assert from 'node:assert/strict';
import { test } from 'node:test';

import { MAX_SETTLEMENT_TX_BYTES, ZSTD_MAGIC, concatBytes, fromHex, toHex, unpackEnvelopeData, utf8, xxh64 } from '../src/index.js';
import { loadVectors } from './helpers.js';

const vectors = loadVectors('compress.json');

/**
 * Frames the node rejects and the SDK reads all the same: `fzstd` does not check that a match stays within the frame's own
 * content (see `src/compress.ts`).
 */
const READ_ALL_THE_SAME = new Set(['matches-before-the-start']);

test('the shared vectors compress.json match case by case (D-141)', () => {
  assert.ok(vectors.cases.length >= 39);
  const read: string[] = [];
  for (const { name, max, data, payload } of vectors.cases) {
    const found = unpackEnvelopeData(fromHex(data), max);
    if (READ_ALL_THE_SAME.has(name)) {
      assert.equal(payload, null, name);
      assert.notEqual(found, null, name);
      read.push(name);
      continue;
    }
    assert.equal(found === null ? null : toHex(found), payload, name);
  }
  assert.deepEqual(read, [...READ_ALL_THE_SAME]);
});

test('data that is not a frame is returned as it is; the bound defaults to MAX_SETTLEMENT_TX_BYTES', () => {
  const raw = Uint8Array.of(2, 1, 2, 3);
  assert.equal(unpackEnvelopeData(raw), raw);
  assert.equal(MAX_SETTLEMENT_TX_BYTES, 400_000);
  // one RLE block of exactly the bound, and of one byte more
  const rle = (size: number) => {
    const head = (size << 3) | (1 << 1) | 1;
    return concatBytes(ZSTD_MAGIC, Uint8Array.of(0x00, 0x50, head & 0xff, (head >> 8) & 0xff, head >> 16, 2));
  };
  assert.deepEqual(unpackEnvelopeData(rle(400_000)), new Uint8Array(400_000).fill(2));
  assert.equal(unpackEnvelopeData(rle(400_001)), null);
  // a block longer than 128 KiB and than the frame's window decodes, as it does in the node
  assert.deepEqual(unpackEnvelopeData(rle(131_073)), new Uint8Array(131_073).fill(2));
  assert.equal(unpackEnvelopeData(rle(131_073), 131_072), null);
});

test('a frame that states the largest window allocates no more than the bound', () => {
  // window log 31, mantissa 7: about 3.75 GiB
  const frame = concatBytes(ZSTD_MAGIC, fromHex('00af'), fromHex('210000'), Uint8Array.of(2, 0xaa, 0xbb, 0xcc));
  const before = process.memoryUsage().arrayBuffers;
  assert.deepEqual(unpackEnvelopeData(frame), Uint8Array.of(2, 0xaa, 0xbb, 0xcc));
  assert.ok(process.memoryUsage().arrayBuffers - before < 4 * 1024 * 1024);
});

test('XXH64 with seed 0: the reference values', () => {
  const hex = (value: bigint) => value.toString(16).padStart(16, '0');
  assert.equal(hex(xxh64(new Uint8Array(0))), 'ef46db3751d8e999');
  assert.equal(hex(xxh64(utf8('a'))), 'd24ec4f1a98c6e5b');
  assert.equal(hex(xxh64(utf8('abc'))), '44bc2cf5ad770999');
  assert.equal(hex(xxh64(utf8('Nobody inspects the spammish repetition'))), 'fbcea83c8a378bf1');
});
