/**
 * Bitcoin basics: transaction serialization and parsing, BIP143 / BIP341 sighashes (cross-checked against the independent
 * implementation in @scure/btc-signer), addresses, script types, dust limit, BIP86 tweak, strict DER.
 */

import assert from 'node:assert/strict';
import { test } from 'node:test';

import { schnorr } from '@noble/curves/secp256k1.js';
import { bech32, bech32m } from '@scure/base';
import { Transaction } from '@scure/btc-signer';

import {
  DEVNET,
  MAINNET,
  SIGNET,
  type Tx,
  addressToScript,
  btcNetworkOf,
  compressedPublicKey,
  decodeAddress,
  decodeWitness,
  dustLimit,
  encodeWitness,
  hash160,
  isStandardOutputScript,
  isStrictDer,
  p2pkhScriptFromHash,
  p2shScriptFromHash,
  p2trScript,
  p2wpkhScript,
  parseTx,
  pushData,
  reverseBytes,
  scriptToAddress,
  serializeTx,
  sighashBip143,
  sighashBip341KeyPath,
  taprootOutputKey,
  taprootTweakSecretKey,
  toHex,
  txWeight,
  txid,
  txidFromRpc,
  txidToRpc,
  xOnlyPublicKey,
} from '../src/index.js';
import { pseudoRandomBytes, testKey } from './helpers.js';

/** A pseudo-random transaction with 3 inputs and 3 outputs, and its previous outputs (P2TR). */
function sampleTx(seed: number): { tx: Tx; prevouts: { value: bigint; script: Uint8Array }[] } {
  const r = pseudoRandomBytes(seed, 400);
  const tx: Tx = {
    version: 2,
    locktime: seed * 7,
    inputs: [0, 1, 2].map((i) => ({
      txid: r.slice(i * 32, i * 32 + 32),
      vout: r[100 + i]!,
      scriptSig: new Uint8Array(0),
      sequence: 0xffff_fff0 + i,
      witness: [],
    })),
    outputs: [0, 1, 2].map((i) => ({
      value: BigInt(r[110 + i]!) * 1000n + 546n,
      script: p2trScript(xOnlyPublicKey(testKey(20 + i))),
    })),
  };
  const prevouts = [0, 1, 2].map((i) => ({ value: BigInt(r[120 + i]!) * 10_000n + 10_000n, script: p2trScript(xOnlyPublicKey(testKey(30 + i))) }));
  return { tx, prevouts };
}

/** The @scure/btc-signer representation of the same transaction (its transaction ID is in display byte order). */
function scureTx(tx: Tx, prevouts: { value: bigint; script: Uint8Array }[]): Transaction {
  const out = new Transaction({ version: tx.version, lockTime: tx.locktime, allowUnknownOutputs: true });
  tx.inputs.forEach((input, i) =>
    out.addInput({
      txid: reverseBytes(input.txid),
      index: input.vout,
      sequence: input.sequence,
      witnessUtxo: { script: prevouts[i]!.script, amount: prevouts[i]!.value },
    }),
  );
  for (const output of tx.outputs) out.addOutput({ script: output.script, amount: output.value });
  return out;
}

test('transaction serialization and parsing round trip; the transaction ID agrees with @scure/btc-signer', () => {
  for (let seed = 1; seed <= 20; seed++) {
    const { tx, prevouts } = sampleTx(seed);
    const legacy = serializeTx(tx);
    assert.deepEqual(parseTx(legacy), tx);
    const reference = scureTx(tx, prevouts);
    assert.equal(toHex(legacy), toHex(reference.unsignedTx));
    assert.equal(txidToRpc(txid(tx)), reference.id);
    // Transaction with witnesses: BIP144 format, the transaction ID excludes the witnesses
    const witnessed: Tx = { ...tx, inputs: tx.inputs.map((i, k) => ({ ...i, witness: k === 1 ? [] : [Uint8Array.of(k, 1), new Uint8Array(0)] })) };
    const full = serializeTx(witnessed);
    assert.equal(full[4], 0x00);
    assert.equal(full[5], 0x01);
    assert.deepEqual(parseTx(full), witnessed);
    assert.deepEqual(txid(witnessed), txid(tx));
    assert.equal(txWeight(witnessed), serializeTx(witnessed, false).length * 3 + full.length);
  }
  assert.throws(() => parseTx(new Uint8Array([...serializeTx(sampleTx(1).tx), 0])), /extra/);
});

test('byte order conversion of transaction IDs (D-001)', () => {
  const rpc = '00000000000000000000000000000000000000000000000000000000000000ff';
  assert.equal(txidFromRpc(rpc)[0], 0xff);
  assert.equal(txidToRpc(txidFromRpc(rpc)), rpc);
  assert.throws(() => txidFromRpc('ff'), /64/);
});

test('BIP143 sighash is byte-for-byte identical to @scure/btc-signer (all sighash types, all inputs)', () => {
  for (let seed = 1; seed <= 5; seed++) {
    const { tx, prevouts } = sampleTx(seed);
    const reference = scureTx(tx, prevouts);
    const scriptCode = p2pkhScriptFromHash(hash160(compressedPublicKey(testKey(40))));
    for (const hashType of [0x01, 0x02, 0x03, 0x81, 0x82, 0x83]) {
      for (let i = 0; i < 3; i++) {
        assert.equal(
          toHex(sighashBip143(tx, i, scriptCode, prevouts[i]!.value, hashType)),
          toHex(reference.preimageWitnessV0(i, scriptCode, hashType, prevouts[i]!.value)),
          `seed ${seed} type ${hashType} input ${i}`,
        );
      }
    }
  }
});

test('BIP341 key-path sighash is byte-for-byte identical to @scure/btc-signer (all sighash types, all inputs)', () => {
  for (let seed = 1; seed <= 5; seed++) {
    const { tx, prevouts } = sampleTx(seed);
    const reference = scureTx(tx, prevouts);
    for (const hashType of [0x00, 0x01, 0x02, 0x03, 0x81, 0x82, 0x83]) {
      for (let i = 0; i < 3; i++) {
        assert.equal(
          toHex(sighashBip341KeyPath(tx, i, prevouts, hashType)),
          toHex(reference.preimageWitnessV1(i, prevouts.map((p) => p.script), hashType, prevouts.map((p) => p.value))),
          `seed ${seed} type ${hashType} input ${i}`,
        );
      }
    }
  }
  const { tx, prevouts } = sampleTx(1);
  assert.throws(() => sighashBip341KeyPath(tx, 0, prevouts, 0x04), /invalid/);
  assert.throws(() => sighashBip341KeyPath(tx, 0, prevouts.slice(1), 0x00), /number of prevouts/);
});

test('BIP86 tweak: the output key agrees with the tweaked secret key and with p2tr of @scure/btc-signer', async () => {
  const { p2tr } = await import('@scure/btc-signer');
  for (let byte = 1; byte <= 20; byte++) {
    const secret = testKey(byte);
    const internal = xOnlyPublicKey(secret);
    const outputKey = taprootOutputKey(internal);
    assert.deepEqual(schnorr.getPublicKey(taprootTweakSecretKey(secret)), outputKey);
    assert.equal(toHex(p2trScript(internal)), toHex(p2tr(internal).script));
  }
});

test('conversion between addresses and locking scripts', () => {
  const secret = testKey(5);
  const tr = p2trScript(xOnlyPublicKey(secret));
  const wpkh = p2wpkhScript(compressedPublicKey(secret));
  for (const network of ['mainnet', 'testnet', 'signet', 'regtest'] as const) {
    for (const script of [tr, wpkh, p2pkhScriptFromHash(new Uint8Array(20).fill(1)), p2shScriptFromHash(new Uint8Array(20).fill(2))]) {
      const address = scriptToAddress(script, network);
      assert.deepEqual(addressToScript(address), script, address);
      assert.ok(decodeAddress(address).networks.includes(network));
    }
  }
  assert.match(scriptToAddress(tr, 'regtest'), /^bcrt1p/);
  assert.match(scriptToAddress(wpkh, 'signet'), /^tb1q/);
  assert.match(scriptToAddress(tr, 'mainnet'), /^bc1p/);
  // chain params -> address network (D-055, D-096)
  assert.equal(btcNetworkOf(DEVNET), 'regtest');
  assert.equal(btcNetworkOf(SIGNET), 'signet');
  assert.equal(btcNetworkOf(MAINNET), 'mainnet');
  // BIP350: witness v1 must use bech32m, v0 must use bech32
  assert.throws(() => addressToScript('bc1pss0zhytly75awhm6x2hhvd5lnzv3vssgrf9axfheq8ldyzn88gesx')); // checksum error
  assert.equal(toHex(addressToScript('bc1q9vza2e8x573nczrlzms0wvx3gsqjx7vavgkx0l')), '00142b05d564e6a7a33c087f16e0f730d1440123799d');
  const program = tr.slice(2);
  assert.throws(() => addressToScript(bech32.encode('bc', [1, ...bech32.toWords(program)])), /bech32m/);
  assert.throws(() => addressToScript(bech32m.encode('bc', [0, ...bech32.toWords(program)])), /bech32/);
  assert.throws(() => addressToScript(bech32m.encode('xx', [1, ...bech32.toWords(program)])));
});

test('script types and dust limit (the same as the Bitcoin Core defaults)', () => {
  const secret = testKey(6);
  const tr = p2trScript(xOnlyPublicKey(secret));
  const wpkh = p2wpkhScript(compressedPublicKey(secret));
  const wsh = Uint8Array.of(0x00, 0x20, ...new Uint8Array(32));
  const pkh = p2pkhScriptFromHash(new Uint8Array(20));
  const sh = p2shScriptFromHash(new Uint8Array(20));
  assert.equal(dustLimit(wpkh), 294n);
  assert.equal(dustLimit(tr), 330n);
  assert.equal(dustLimit(wsh), 330n);
  assert.equal(dustLimit(sh), 540n);
  assert.equal(dustLimit(pkh), 546n);
  assert.equal(dustLimit(Uint8Array.of(0x6a, 0x01, 0x00)), 0n);
  for (const s of [tr, wpkh, wsh, pkh, sh]) assert.ok(isStandardOutputScript(s));
  for (const s of [Uint8Array.of(0x6a), Uint8Array.of(0x51, 0x02, 0x4e, 0x73), new Uint8Array(0)]) assert.ok(!isStandardOutputScript(s));
});

test('data push is the same as the push_slice of rust-bitcoin (not rewritten as OP_1..OP_16)', () => {
  assert.equal(toHex(pushData(Uint8Array.of(1))), '0101');
  assert.equal(toHex(pushData(new Uint8Array(0))), '00');
  assert.equal(toHex(pushData(new Uint8Array(75))).slice(0, 2), '4b');
  assert.equal(toHex(pushData(new Uint8Array(76))).slice(0, 4), '4c4c');
  assert.equal(toHex(pushData(new Uint8Array(255))).slice(0, 4), '4cff');
  assert.equal(toHex(pushData(new Uint8Array(256))).slice(0, 6), '4d0001');
  assert.equal(toHex(pushData(new Uint8Array(520))).slice(0, 6), '4d0802');
  assert.equal(toHex(pushData(new Uint8Array(65536))).slice(0, 10), '4e00000100');
});

test('witness stack encoding round trip; truncated and trailing bytes are rejected', () => {
  const items = [Uint8Array.of(1, 2, 3), new Uint8Array(0), new Uint8Array(300).fill(9)];
  const bytes = encodeWitness(items);
  assert.deepEqual(decodeWitness(bytes), items);
  assert.throws(() => decodeWitness(bytes.slice(0, -1)));
  assert.throws(() => decodeWitness(new Uint8Array([...bytes, 0])));
  // CompactSize must be the shortest encoding
  assert.throws(() => decodeWitness(Uint8Array.of(0xfd, 0x01, 0x00, 0x00)), /shortest/);
});

test('strict DER (BIP66), the same cases as on the Rust side', () => {
  const der = (r: number[], s: number[]): Uint8Array =>
    Uint8Array.from([0x30, 4 + r.length + s.length, 0x02, r.length, ...r, 0x02, s.length, ...s, 0x01]);
  assert.ok(isStrictDer(der([1], [1])));
  assert.ok(isStrictDer(der([0x00, 0x80], new Array(32).fill(0x7f))));
  assert.ok(isStrictDer(der(new Array(33).fill(0x7f), new Array(32).fill(0x7f))));
  assert.ok(!isStrictDer(der([0x80], [1])));
  assert.ok(!isStrictDer(der([1], [0x80])));
  assert.ok(!isStrictDer(der([0x00, 0x01], [1])));
  assert.ok(!isStrictDer(der([1], [0x00, 0x01])));
  assert.ok(!isStrictDer(der([], [1, 2])));
  assert.ok(!isStrictDer(der([1, 2], [])));
  assert.ok(!isStrictDer(der(new Array(34).fill(0x7f), new Array(33).fill(0x7f))));
  assert.ok(!isStrictDer(Uint8Array.of(0x30, 0x06, 0x02, 0x01, 0x01, 0x02, 0x01, 0x01)));
  const mutate = (i: number, f: (b: number) => number): Uint8Array => {
    const b = der([1], [1]);
    b[i] = f(b[i]!);
    return b;
  };
  assert.ok(!isStrictDer(mutate(0, () => 0x31)));
  assert.ok(!isStrictDer(mutate(1, (b) => b + 1)));
  assert.ok(!isStrictDer(mutate(2, () => 0x03)));
  assert.ok(!isStrictDer(mutate(3, () => 0x10)));
});
