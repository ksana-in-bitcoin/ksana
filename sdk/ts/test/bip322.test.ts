/**
 * BIP322 simple (D-052, D-092): the official BIP322 vectors (`test/data/bip322/`, taken from `bip-0322/basic-test-vectors.json`
 * and `generated-test-vectors.json` in bitcoin/bips), the round trip of account (Taproot address) signatures, tampering and
 * unsupported shapes. Same rules as `crates/ksana-crypto/tests/bip322.rs`. One local change (D-099): the third `tx_hashes`
 * case had Chinese words in its message, replaced here by `₿ €`, with its hashes recomputed (same files as the Rust copy).
 */

import assert from 'node:assert/strict';
import { test } from 'node:test';

import { schnorr, secp256k1 } from '@noble/curves/secp256k1.js';

import {
  MAX_BIP322_SIG_BYTES,
  SIGHASH_ALL,
  SIGHASH_DEFAULT,
  accountOf,
  addressToScript,
  bip322MessageHash,
  bip322ToSign,
  bip322ToSpend,
  compressedPublicKey,
  concatBytes,
  decodeBip322Signature,
  decodeWitness,
  ecdsaSignDer,
  encodeWitness,
  hash160,
  p2pkhScriptFromHash,
  p2trScriptFromOutputKey,
  p2wpkhScript,
  sighashBip143,
  sighashBip341KeyPath,
  signBip322Simple,
  signBip322SimpleP2tr,
  taprootTweakSecretKey,
  toHex,
  txid,
  txidToRpc,
  verifyBip322ForAccount,
  verifyBip322Simple,
} from '../src/index.js';
import { decodeWif, loadBip322Official, testKey } from './helpers.js';

const FILES = ['basic-test-vectors.json', 'generated-test-vectors.json'];
/** The address types `verifyBip322Simple` can verify (an account can only be P2TR). */
const supported = (kind: string): boolean => kind === 'p2tr' || kind === 'p2wpkh';

/**
 * BIP322 simple signature of a P2WPKH address (`[DER ‖ SIGHASH_ALL, compressed public key]`, RFC 6979): the SDK no longer
 * provides it (since D-092 an account has only a Taproot address); the test assembles it per BIP322 itself to verify the P2WPKH
 * branch of `verifyBip322Simple` and that account verification rejects it.
 */
function signP2wpkhSimple(secret: Uint8Array, message: string): Uint8Array {
  const publicKey = compressedPublicKey(secret);
  const toSign = bip322ToSign(txid(bip322ToSpend(p2wpkhScript(publicKey), message)));
  const sighash = sighashBip143(toSign, 0, p2pkhScriptFromHash(hash160(publicKey)), 0n, SIGHASH_ALL);
  return encodeWitness([concatBytes(ecdsaSignDer(sighash, secret), Uint8Array.of(SIGHASH_ALL)), publicKey]);
}

test('official BIP322 vectors: message_hash and the transaction IDs of to_spend and to_sign', () => {
  const v = loadBip322Official('basic-test-vectors.json');
  assert.equal(v.tx_hashes.length, 3);
  for (const c of v.tx_hashes) {
    assert.equal(toHex(bip322MessageHash(c.message)), c.message_hash, c.message);
    const toSpend = bip322ToSpend(addressToScript(c.address), c.message);
    // the official vectors give transaction IDs in RPC display byte order
    assert.equal(txidToRpc(txid(toSpend)), c.to_spend_tx_hash);
    assert.equal(txidToRpc(txid(bip322ToSign(txid(toSpend)))), c.to_sign_tx_hash);
  }
});

test('official BIP322 vectors: all P2TR and P2WPKH simple signatures pass, every other type fails', (t) => {
  let checked = 0;
  let rejected = 0;
  for (const file of FILES) {
    const v = loadBip322Official(file);
    for (const c of v.simple) {
      const script = addressToScript(c.address);
      for (const signature of c.bip322_signatures) {
        const bytes = decodeBip322Signature(signature);
        const ok = verifyBip322Simple(script, c.message, bytes);
        if (supported(c.type)) {
          assert.ok(ok, `${file} ${c.type} ${c.address}`);
          checked++;
        } else {
          assert.ok(!ok, `${file} ${c.type} must not pass`);
          rejected++;
        }
      }
      if (!supported(c.type)) continue;
      const secret = decodeWif(c.private_keys[0]);
      const account = accountOf(secret);
      if (c.type === 'p2tr') {
        // the P2TR address of the official vector is the account address of that secret key (BIP86): it also verifies by
        // account, and this SDK's signature verifies likewise (the official signatures used random aux_rand, so they can
        // only be verified, not reproduced)
        assert.deepEqual(p2trScriptFromOutputKey(account), script, 'BIP86 address');
        for (const signature of c.bip322_signatures) {
          assert.ok(verifyBip322ForAccount(account, c.message, decodeBip322Signature(signature)));
        }
        assert.ok(verifyBip322Simple(script, c.message, signBip322Simple(secret, c.message, new Uint8Array(32))));
      } else {
        // P2WPKH: one of the official vector's signatures is exactly the RFC 6979 deterministic signature (the other is the
        // result of low-R grinding); the signature assembled per BIP322 reproduces it byte for byte. But a P2WPKH address
        // is not an account (D-092), so verification by account always fails
        const ours = signP2wpkhSimple(secret, c.message);
        assert.ok(verifyBip322Simple(script, c.message, ours));
        const same = c.bip322_signatures.some((s: string) => toHex(decodeBip322Signature(s)) === toHex(ours));
        assert.ok(same, `${file} p2wpkh "${c.message}": the RFC 6979 signature must equal one of the official vectors byte for byte`);
        t.diagnostic(`${file} p2wpkh "${c.message}": the RFC 6979 signature equals one of the official vectors byte for byte`);
        for (const signature of [...c.bip322_signatures.map(decodeBip322Signature), ours]) {
          assert.ok(!verifyBip322ForAccount(account, c.message, signature), 'P2WPKH is not an account address');
        }
      }
    }
  }
  assert.ok(checked >= 6, `verified ${checked} P2TR / P2WPKH signatures`);
  assert.ok(rejected >= 3, `rejected ${rejected} signatures of other types`);
});

test('official BIP322 vectors: all error cases fail', () => {
  let count = 0;
  for (const file of FILES) {
    const v = loadBip322Official(file);
    for (const c of v.error) {
      let script: Uint8Array;
      try {
        script = addressToScript(c.address);
      } catch {
        continue;
      }
      let bytes: Uint8Array;
      try {
        bytes = decodeBip322Signature(c.signature);
      } catch {
        count++; // undecodable (invalid base64, empty signature, full format) counts as a failure too
        continue;
      }
      assert.ok(!verifyBip322Simple(script, c.message, bytes), `${file}: ${c.description}`);
      count++;
    }
  }
  assert.ok(count >= 30, `verified ${count} error cases`);
});

test('decodeBip322Signature: strips the smp prefix, rejects ful / pof and invalid base64', () => {
  const raw = Uint8Array.of(1, 2, 3);
  assert.deepEqual(decodeBip322Signature('AQID'), raw);
  assert.deepEqual(decodeBip322Signature('smpAQID'), raw);
  assert.throws(() => decodeBip322Signature('fulAQID'), /simple/);
  assert.throws(() => decodeBip322Signature('pofAQID'), /simple/);
  assert.throws(() => decodeBip322Signature('not-valid-base64!!!'), /base64/);
  assert.throws(() => decodeBip322Signature(''), /empty/);
});

test('round trip of account signatures: the address is the Taproot address of the account; a P2WPKH signature by the same secret key does not count', () => {
  const secret = testKey(7);
  const account = accountOf(secret);
  const other = accountOf(testKey(8));
  for (const message of ['', 'Hello World', 'ab'.repeat(32)]) {
    const sig = signBip322Simple(secret, message, new Uint8Array(32).fill(1));
    assert.ok(verifyBip322ForAccount(account, message, sig));
    assert.ok(verifyBip322Simple(p2trScriptFromOutputKey(account), message, sig));
    assert.ok(!verifyBip322ForAccount(other, message, sig));
    assert.ok(!verifyBip322ForAccount(account, 'other', sig));
    // a P2WPKH signature by the same secret key is valid for its P2WPKH address, but a P2WPKH address is not an account
    const wpkh = signP2wpkhSimple(secret, message);
    assert.ok(verifyBip322Simple(p2wpkhScript(compressedPublicKey(secret)), message, wpkh));
    assert.ok(!verifyBip322ForAccount(account, message, wpkh));
  }
  // the signature is deterministic; the old name signBip322SimpleP2tr is the same function
  assert.deepEqual(signBip322Simple(secret, 'm', new Uint8Array(32)), signBip322Simple(secret, 'm', new Uint8Array(32)));
  assert.equal(signBip322SimpleP2tr, signBip322Simple);
  // the witness is 1 element of 64 bytes
  assert.equal(decodeWitness(signBip322Simple(secret, 'm'))[0]!.length, 64);
  // 32 bytes not on the curve, such as a contract ID, are not an account: verification always fails (no throw)
  assert.ok(!verifyBip322ForAccount(new Uint8Array(32).fill(0xff), 'm', signBip322Simple(secret, 'm')));
});

/** Sign to_sign with the BIP86-tweaked secret key and `hashType`; returns the witness encoding (with the type byte appended when not DEFAULT). */
function p2trWithType(secret: Uint8Array, message: string, hashType: number): Uint8Array {
  const challenge = p2trScriptFromOutputKey(accountOf(secret));
  const toSign = bip322ToSign(txid(bip322ToSpend(challenge, message)));
  const sighash = sighashBip341KeyPath(toSign, 0, [{ value: 0n, script: challenge }], hashType);
  const sig = schnorr.sign(sighash, taprootTweakSecretKey(secret), new Uint8Array(32));
  return encodeWitness([hashType === SIGHASH_DEFAULT ? sig : concatBytes(sig, Uint8Array.of(hashType))]);
}

test('P2TR: only SIGHASH_DEFAULT (64 bytes) and SIGHASH_ALL (65 bytes) are accepted', () => {
  const secret = testKey(9);
  const account = accountOf(secret);
  assert.ok(verifyBip322ForAccount(account, 'm', p2trWithType(secret, 'm', SIGHASH_DEFAULT)));
  assert.ok(verifyBip322ForAccount(account, 'm', p2trWithType(secret, 'm', SIGHASH_ALL)));
  for (const hashType of [0x02, 0x03, 0x81, 0x83]) {
    assert.ok(!verifyBip322ForAccount(account, 'm', p2trWithType(secret, 'm', hashType)), hashType.toString(16));
  }
  // a 64-byte signature followed by 0x00 (the explicit spelling of DEFAULT) is rejected too
  const explicit = decodeWitness(p2trWithType(secret, 'm', SIGHASH_DEFAULT))[0]!;
  assert.ok(!verifyBip322ForAccount(account, 'm', encodeWitness([concatBytes(explicit, Uint8Array.of(0))])));
});

test('malformed and unsupported witnesses always fail', () => {
  const secret = testKey(10);
  const account = accountOf(secret);
  const wpkh = signP2wpkhSimple(secret, 'm');
  const wpkhScript = p2wpkhScript(compressedPublicKey(secret));
  const tr = signBip322Simple(secret, 'm', new Uint8Array(32));
  for (const sig of [
    concatBytes(tr, Uint8Array.of(0)),
    tr.slice(0, -1),
    wpkh,
    Uint8Array.of(0),
    new Uint8Array(0),
    encodeWitness([new Uint8Array(64).fill(1), new Uint8Array(33).fill(2), Uint8Array.of(3)]),
    encodeWitness([new Uint8Array(MAX_BIP322_SIG_BYTES)]),
  ]) {
    assert.ok(!verifyBip322ForAccount(account, 'm', sig));
  }
  // the P2WPKH branch (only in verifyBip322Simple): sighash type, signature bytes, public key, uncompressed public key, high S
  const [derAll, publicKey] = decodeWitness(wpkh) as [Uint8Array, Uint8Array];
  const rebuild = (a: Uint8Array, b: Uint8Array): Uint8Array => encodeWitness([a, b]);
  assert.ok(verifyBip322Simple(wpkhScript, 'm', rebuild(derAll, publicKey)));
  const wrongType = Uint8Array.from(derAll);
  wrongType[wrongType.length - 1] = 0x02;
  assert.ok(!verifyBip322Simple(wpkhScript, 'm', rebuild(wrongType, publicKey)));
  const flipped = Uint8Array.from(derAll);
  flipped[10]! ^= 1;
  assert.ok(!verifyBip322Simple(wpkhScript, 'm', rebuild(flipped, publicKey)));
  assert.ok(!verifyBip322Simple(wpkhScript, 'm', rebuild(derAll, compressedPublicKey(testKey(11)))));
  // uncompressed public key: P2WPKH takes hash160 of the raw bytes, which corresponds to another address
  const uncompressed = secp256k1.getPublicKey(secret, false);
  assert.ok(!verifyBip322Simple(wpkhScript, 'm', rebuild(derAll, uncompressed)));
  // high S: s → n − s is still mathematically valid but not a standard signature
  const parsed = secp256k1.Signature.fromBytes(derAll.slice(0, -1), 'der');
  const high = new secp256k1.Signature(parsed.r, secp256k1.Point.Fn.ORDER - parsed.s);
  assert.ok(!verifyBip322Simple(wpkhScript, 'm', rebuild(concatBytes(high.toBytes('der'), Uint8Array.of(1)), publicKey)));
  // P2TR: tampered signature
  const bad = Uint8Array.from(tr);
  bad[5]! ^= 1;
  assert.ok(!verifyBip322ForAccount(account, 'm', bad));
});
