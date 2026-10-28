/**
 * The KSN1 envelope (D-081): the leaf script byte for byte as `crates/ksana-btc/src/envelope.rs` writes it, its parsing rules, and
 * the leaf of a witness by BIP341's annex rule.
 */

import assert from 'node:assert/strict';
import { test } from 'node:test';

import { DEVNET, concatBytes, fromHex, isKsn1Envelope, ksn1EnvelopeScript, parseKsn1Envelope, toHex, witnessLeaf, xOnlyPublicKey } from '../src/index.js';
import { pseudoRandomBytes, testKey } from './helpers.js';

const MAGIC = DEVNET.magic;
const KEY = xOnlyPublicKey(testKey(11));

test('envelope script: the layout of envelope.rs, and the payload in 520-byte chunks', () => {
  const script = ksn1EnvelopeScript(KEY, MAGIC, new TextEncoder().encode('abc'));
  assert.equal(toHex(script), `20${toHex(KEY)}ac0063044b534e3103616263` + '68');
  assert.deepEqual(parseKsn1Envelope(MAGIC, script), { key: KEY, payload: new TextEncoder().encode('abc'), chunks: 1 });
  assert.ok(isKsn1Envelope(MAGIC, script));
  // 1,100 bytes: 520 + 520 + 60, each written like rust-bitcoin's push_slice
  const payload = pseudoRandomBytes(3, 1100);
  const long = ksn1EnvelopeScript(KEY, MAGIC, payload);
  const head = concatBytes(fromHex('20'), KEY, fromHex('ac0063044b534e31'));
  assert.deepEqual(
    long,
    concatBytes(head, fromHex('4d0802'), payload.subarray(0, 520), fromHex('4d0802'), payload.subarray(520, 1040), fromHex('3c'), payload.subarray(1040), fromHex('68')),
  );
  assert.deepEqual(parseKsn1Envelope(MAGIC, long), { key: KEY, payload, chunks: 3 });
  assert.throws(() => ksn1EnvelopeScript(KEY, MAGIC, new Uint8Array(0)), /empty/);
  assert.throws(() => ksn1EnvelopeScript(new Uint8Array(32).fill(0xff), MAGIC, Uint8Array.of(1)), /x-only/);
  assert.throws(() => ksn1EnvelopeScript(KEY.subarray(1), MAGIC, Uint8Array.of(1)));
});

test('envelope parsing: data pushes of any encoding up to 520 bytes, then OP_ENDIF as the last byte', () => {
  const head = concatBytes(fromHex('20'), KEY, fromHex('ac0063'));
  const parse = (rest: string) => {
    const found = parseKsn1Envelope(MAGIC, concatBytes(head, fromHex(rest)));
    return found === null ? null : toHex(found.payload);
  };
  assert.equal(parse('044b534e3103aabbcc68'), 'aabbcc');
  assert.equal(parse('4c044b534e314c03aabbcc4d0100dd68'), 'aabbccdd', 'non-minimal pushes of the magic and the chunks');
  assert.equal(parse('044b534e3168'), '', 'no chunk');
  assert.equal(parse('044b534e310068'), '', 'an empty push');
  for (const bad of ['044b534e3103aabbcc', '044b534e3103aabbcc6868', '044b534e3103aabbcc6800', '044b534e315168', '044b534e3103aabb68', '044b534e3268', '']) {
    assert.equal(parse(bad), null, bad);
  }
  assert.equal(parseKsn1Envelope(MAGIC, concatBytes(head, fromHex('044b534e314d0902'), new Uint8Array(521), fromHex('68'))), null, 'a chunk over 520 bytes');
  // Judged by its start: the key push, OP_CHECKSIG OP_FALSE OP_IF and the magic
  assert.ok(isKsn1Envelope(MAGIC, concatBytes(head, fromHex('044b534e3151'))));
  for (const start of [fromHex('21'), fromHex('20')]) {
    assert.equal(isKsn1Envelope(MAGIC, concatBytes(start, KEY.subarray(0, 31), fromHex('ac0063044b534e3168'))), false);
  }
  assert.equal(isKsn1Envelope(MAGIC, concatBytes(fromHex('20'), KEY, fromHex('ad0063044b534e3168'))), false);
  assert.equal(isKsn1Envelope(MAGIC, concatBytes(fromHex('20'), KEY, fromHex('ac0064044b534e3168'))), false);
});

test('witness leaf: the second-to-last item once a BIP341 annex is left out', () => {
  const sig = new Uint8Array(64).fill(1);
  const leaf = Uint8Array.of(0xaa);
  const control = new Uint8Array(33).fill(0xc0);
  assert.deepEqual(witnessLeaf([sig, leaf, control]), leaf);
  assert.deepEqual(witnessLeaf([sig, leaf, control, Uint8Array.of(0x50)]), leaf);
  assert.equal(witnessLeaf([sig]), null);
  assert.equal(witnessLeaf([sig, Uint8Array.of(0x50)]), null, 'two items, the last an annex: one item is left');
  assert.equal(witnessLeaf([]), null);
  assert.deepEqual(witnessLeaf([leaf, new Uint8Array(0)]), leaf, 'an empty last item is no annex');
});
