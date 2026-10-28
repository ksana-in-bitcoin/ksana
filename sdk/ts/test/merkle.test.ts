/**
 * Shared vectors `tests/vectors/merkle.json`: RFC 6962 Merkle roots and `entries_root` (D-004).
 */

import assert from 'node:assert/strict';
import { test } from 'node:test';

import { type Entry, entriesRoot, fromHex, fromJson, merkleRoot, sha256, toHex } from '../src/index.js';
import { loadVectors } from './helpers.js';

const vectors = loadVectors('merkle.json');

test('merkle.json: RFC 6962 reference roots (0 to 8 leaves)', () => {
  assert.equal(vectors.rfc6962.length, 9);
  for (const c of vectors.rfc6962) {
    assert.equal(toHex(merkleRoot(c.leaves.map((l: string) => fromHex(l)))), c.root, `${c.leaves.length} leaves`);
  }
});

for (const c of vectors.entries_root) {
  test(`merkle.json entries_root: ${c.name}`, () => {
    const entries: Entry[] = c.entries.map((e: unknown) => fromJson('Entry', e));
    assert.equal(toHex(entriesRoot(entries)), c.entries_root);
  });
}

test('the empty tree is sha256(""), a single leaf is sha256(0x00 ‖ d)', () => {
  assert.equal(toHex(merkleRoot([])), toHex(sha256(new Uint8Array(0))));
  assert.equal(toHex(merkleRoot([Uint8Array.of(1, 2)])), toHex(sha256(Uint8Array.of(0, 1, 2))));
});
