/**
 * PoW (`src/pow.ts`, D-025, D-084, D-094): the shared vectors `tests/vectors/pow.json` verified case by case (header encoding
 * and hash, salt, Argon2id output, PoW value, effective target, whether the PoW is met), the same file Rust's
 * `crates/ksana-consensus/tests/argon2id.rs` verifies; the decoding rules of compact `bits`.
 */

import assert from 'node:assert/strict';
import { test } from 'node:test';

import {
  DEVNET,
  POW_SALT_PREFIX,
  type Params,
  SIGNET,
  blockHash,
  bytesToBigIntLE,
  checkPow,
  decodeCompact,
  effectiveTarget,
  encodeBlockHeader,
  fromJson,
  powHash,
  powSalt,
  powValue,
  targetFromBits,
  toHex,
  utf8,
} from '../src/index.js';
import { loadVectors } from './helpers.js';

const vectors = loadVectors('pow.json');
const hex64 = (value: bigint) => value.toString(16).padStart(64, '0');

test('the shared vectors pow.json match case by case (D-094)', () => {
  assert.equal(vectors.algorithm, 'argon2id');
  assert.equal(vectors.version, 0x13);
  assert.equal(vectors.salt_prefix, toHex(utf8(POW_SALT_PREFIX)));
  assert.equal(vectors.devnet.chain_id, DEVNET.chain_id);
  assert.deepEqual(vectors.devnet.argon2, { ...DEVNET.pow_algorithm.argon2id });
  assert.deepEqual(SIGNET.pow_algorithm, DEVNET.pow_algorithm);
  assert.ok(vectors.cases.length >= 10);
  let met = 0;
  for (const c of vectors.cases) {
    const header = fromJson('BlockHeader', c.header);
    assert.equal(toHex(encodeBlockHeader(header)), c.header_borsh, c.name);
    assert.equal(toHex(blockHash(header)), c.block_hash, c.name);
    const params: Params = {
      ...DEVNET,
      chain_id: c.chain_id,
      pow_limit_bits: c.pow_limit_bits,
      pow_algorithm: { argon2id: c.argon2 },
    };
    assert.equal(toHex(powSalt(c.chain_id)), c.salt, c.name);
    assert.equal(toHex(powHash(params, header)), c.pow_hash, c.name);
    assert.equal(hex64(powValue(params, header)), c.pow_value, c.name);
    assert.equal(hex64(effectiveTarget(header.bits, c.pow_limit_bits, c.weight)), c.effective_target, c.name);
    assert.equal(checkPow(params, header, c.weight), c.meets, c.name);
    if (c.meets) met++;
  }
  // the solved cases are met; the cases with a target below the limit and with other salts / parameters mostly are not
  assert.ok(met >= 2 && met < vectors.cases.length, `${met}`);
});

test('salt and little-endian interpretation', () => {
  assert.equal(toHex(powSalt(1001)), toHex(utf8('ksana-pow')) + 'e9030000');
  assert.equal(toHex(powSalt(1002)), toHex(utf8('ksana-pow')) + 'ea030000');
  assert.equal(bytesToBigIntLE(Uint8Array.of(0x01, 0x02)), 0x0201n);
  assert.equal(bytesToBigIntLE(new Uint8Array(32).fill(0xff)), (1n << 256n) - 1n);
  assert.equal(bytesToBigIntLE(new Uint8Array(0)), 0n);
});

test('decoding of compact bits and the effective target (D-025, D-084)', () => {
  // the bits of the Bitcoin genesis block
  assert.equal(hex64(targetFromBits(0x1d00ffff, 0x1d00ffff)), '00000000ffff0000000000000000000000000000000000000000000000000000');
  // the devnet limit: 0x7fffff × 256^29
  assert.equal(targetFromBits(0x207fffff, 0x207fffff), 0x7fffffn << 232n);
  // exponent ≤ 3: the mantissa is shifted right and the low bits are dropped
  assert.equal(targetFromBits(0x01010000, 0x207fffff), 1n);
  assert.equal(targetFromBits(0x02123456, 0x207fffff), 0x1234n);
  assert.equal(targetFromBits(0x03123456, 0x207fffff), 0x123456n);
  // sign bit, target 0, above the limit, ≥ 2^256
  assert.equal(decodeCompact(0x04800000), null);
  assert.equal(decodeCompact(0), null);
  assert.equal(decodeCompact(0x217fffff), null);
  assert.equal(decodeCompact(0x2100ffff), 0xffffn << 240n);
  for (const bad of [0x04800000, 0, 0x2100ffff, 0x217fffff, 0x20800000]) {
    assert.throws(() => targetFromBits(bad, 0x207fffff), RangeError, bad.toString(16));
  }
  assert.throws(() => targetFromBits(0x207fffff, 0x20800000), RangeError, 'the limit itself is invalid');
  assert.throws(() => decodeCompact(-1), RangeError);
  assert.throws(() => decodeCompact(2 ** 32), RangeError);
  // effective target = target / weight (integer division); weight 0 or a result of 0 is invalid
  assert.equal(effectiveTarget(0x207fffff, 0x207fffff, 1), 0x7fffffn << 232n);
  assert.equal(effectiveTarget(0x207fffff, 0x207fffff, 12), (0x7fffffn << 232n) / 12n);
  assert.equal(effectiveTarget(0x207fffff, 0x207fffff, 120n), (0x7fffffn << 232n) / 120n);
  assert.equal(effectiveTarget(0x01010000, 0x207fffff, 1), 1n);
  assert.throws(() => effectiveTarget(0x01010000, 0x207fffff, 12), RangeError);
  assert.throws(() => effectiveTarget(0x207fffff, 0x207fffff, 0), RangeError);
  assert.throws(() => effectiveTarget(0x207fffff, 0x207fffff, -1), RangeError);
});
