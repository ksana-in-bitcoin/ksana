/**
 * Shared vectors `tests/vectors/signing.json`, `aggregate` (SPEC §4.5, D-154): a block's entries with whole signatures and
 * as the block holds them, the items, the aggregate and entry lists whose aggregate must not verify.
 */

import assert from 'node:assert/strict';
import { test } from 'node:test';

import {
  type Entry,
  type VmOp,
  aggregate,
  encodeVmOp,
  entriesRoot,
  fromHex,
  fromJson,
  halfSigned,
  joinVmOp,
  opId,
  sealEntries,
  splitVmOp,
  toHex,
  toJson,
  unsealEntries,
  verifyAggregate,
  verifyEntriesAggregate,
  verifyVmop,
  verifyVmopBip322,
} from '../src/index.js';
import { loadVectors } from './helpers.js';

const vectors = loadVectors('signing.json').aggregate;
const signed: Entry[] = vectors.signed.map((e: unknown) => fromJson('Entry', e));
const sealed: Entry[] = vectors.sealed.map((e: unknown) => fromJson('Entry', e));
const seconds: Uint8Array[] = vectors.second_halves.map((s: string) => fromHex(s));
const items = vectors.items.map((i: { pubkey: string; msg: string; r: string }) => ({
  pubkey: fromHex(i.pubkey),
  msg: fromHex(i.msg),
  r: fromHex(i.r),
}));
const pathB = (entries: Entry[]): VmOp[] => entries.flatMap((e) => (e.kind === 'Sponsored' ? [e.op] : []));

test('signing.json aggregate: the three signature forms are all there', () => {
  assert.deepEqual([...new Set(pathB(sealed).map((op) => op.sig.kind))].sort(), ['HalfBip322', 'HalfBip322All', 'HalfBip340']);
  assert.equal(items.length, 4);
  assert.equal(seconds.length, 4);
});

test('signing.json aggregate: sealing gives the block its entries, the second halves and the root', () => {
  const out = sealEntries(signed);
  assert.deepEqual(
    out.entries.map((e) => toJson('Entry', e)),
    vectors.sealed,
  );
  assert.deepEqual(out.secondHalves.map(toHex), vectors.second_halves);
  assert.equal(toHex(entriesRoot(out.entries)), vectors.entries_root);
  assert.deepEqual(sealed.at(-1), { kind: 'Aggregate', s: fromHex(vectors.s) });
  // what is sealed is not sealed again: half a signature has no half form, and an aggregate is already there
  assert.throws(() => sealEntries(sealed), /no half form/);
  assert.throws(() => sealEntries([sealed[0]!, sealed.at(-1)!]), /already carry an aggregate/);
});

test('signing.json aggregate: the items, and one ID for an op in both forms', () => {
  const whole = pathB(signed);
  for (const [i, half] of pathB(sealed).entries()) {
    const expected = vectors.items[i];
    for (const op of [half, whole[i]!]) {
      const item = halfSigned(op)!;
      assert.deepEqual([toHex(item.pubkey), toHex(item.msg), toHex(item.r)], [expected.pubkey, expected.msg, expected.r]);
      assert.equal(toHex(opId(op)), expected.op_id);
    }
    const split = splitVmOp(whole[i]!)!;
    assert.deepEqual(split.half, half);
    assert.equal(toHex(split.s), vectors.second_halves[i]);
    assert.deepEqual(joinVmOp(half, split.s), whole[i]);
    assert.equal(splitVmOp(half), null);
    assert.equal(joinVmOp(whole[i]!, split.s), null);
    // 32 bytes less for BIP340; a BIP322 witness also loses its length prefix and framing (38 or 39 bytes)
    const saved = encodeVmOp(whole[i]!).length - encodeVmOp(half).length;
    assert.equal(saved, { HalfBip340: 32, HalfBip322: 38, HalfBip322All: 39 }[half.sig.kind as 'HalfBip340']);
    // half a signature is no signature alone
    assert.equal(verifyVmop(half), 'invalid');
    assert.equal(verifyVmopBip322(half), false);
  }
});

test('signing.json aggregate: the aggregate is reproduced and verifies', () => {
  assert.equal(toHex(aggregate(items, seconds)), vectors.s);
  assert.equal(verifyAggregate(items, fromHex(vectors.s)), true);
  assert.equal(verifyEntriesAggregate(sealed), true);
  assert.deepEqual(unsealEntries(sealed, seconds), pathB(signed));
  assert.equal(unsealEntries(sealed, seconds.slice(1)), null);
  assert.equal(unsealEntries(sealed, []), null);
  assert.deepEqual(unsealEntries([], []), []);
});

test('signing.json aggregate: what must not verify does not', () => {
  assert.ok(vectors.failing.length >= 4);
  for (const f of vectors.failing) {
    const ops: VmOp[] = f.ops.map((op: unknown) => fromJson('VmOp', op));
    const entries: Entry[] = [...ops.map((op): Entry => ({ kind: 'Sponsored', op })), { kind: 'Aggregate', s: fromHex(f.s) }];
    assert.equal(verifyEntriesAggregate(entries), false, f.name);
    assert.equal(verifyAggregate(ops.map((op) => halfSigned(op)!), fromHex(f.s)), false, f.name);
  }
  const s = fromHex(vectors.s);
  // the group order and above is no scalar
  assert.equal(verifyAggregate(items, fromHex('fffffffffffffffffffffffffffffffebaaedce6af48a03bbfd25e8cd0364141')), false);
  // an x coordinate off the curve
  const off = [{ ...items[0], r: new Uint8Array(32).fill(0xff) }, ...items.slice(1)];
  assert.equal(verifyAggregate(off, s), false);
  assert.throws(() => aggregate(items, seconds.slice(1)), /one second half per item/);
});

test('the rule on where the aggregate entry stands', () => {
  const aggregateEntry = sealed.at(-1)!;
  const others = sealed.filter((e) => e.kind !== 'Sponsored' && e.kind !== 'Aggregate');
  assert.equal(verifyEntriesAggregate([]), true);
  assert.equal(verifyEntriesAggregate(others), true);
  // an aggregate without path B entries, none with them, one that is not last, and two
  assert.equal(verifyEntriesAggregate([...others, aggregateEntry]), false);
  assert.equal(verifyEntriesAggregate(sealed.slice(0, -1)), false);
  assert.equal(verifyEntriesAggregate([aggregateEntry, ...sealed.slice(0, -1)]), false);
  assert.equal(verifyEntriesAggregate([...sealed, aggregateEntry]), false);
  // whole signatures are not what a block holds
  assert.equal(verifyEntriesAggregate([...signed, aggregateEntry]), false);
  // a path C op in a path B entry is in no aggregate: the answer is false, not an exception
  const native = sealed.map((e): Entry => (e.kind === 'Sponsored' ? { kind: 'Sponsored', op: { ...e.op, fee: { kind: 'BtcNative' } } } : e));
  assert.equal(verifyEntriesAggregate(native), false);
  assert.equal(halfSigned(native.find((e) => e.kind === 'Sponsored')!.op), null);
});
