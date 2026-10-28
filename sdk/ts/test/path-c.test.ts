/**
 * Path C (D-032, D-036): chunking and push encoding of the KSN1 data script (byte-for-byte the same as rust-bitcoin's `push_slice`),
 * parsing rules, a transaction's data carriers with the envelopes in its inputs (D-081), the native transaction payload, and
 * building → signing → extracting → recognizing a native transaction.
 */

import assert from 'node:assert/strict';
import { test } from 'node:test';

import { schnorr } from '@noble/curves/secp256k1.js';
import { Transaction } from '@scure/btc-signer';

import {
  DEVNET,
  SCURE_TX_OPTIONS,
  SIGHASH_ALL,
  SIGHASH_DEFAULT,
  type OutPoint,
  type Tx,
  type VmOp,
  accountOf,
  buildNativeTx,
  compressedPublicKey,
  concatBytes,
  ecdsaVerifyDer,
  encodePayload,
  extractTransaction,
  fromHex,
  fromJson,
  hash160,
  isKsn1Envelope,
  isKsn1Output,
  ksn1DataScript,
  ksn1EnvelopeScript,
  nativeFirstInput,
  nativeOpFromTx,
  nativePayload,
  nativeTxFirstInput,
  p2pkhScriptFromHash,
  p2trScript,
  p2wpkhScript,
  parseKsn1DataScript,
  parseKsn1Envelope,
  parseTx,
  sighashBip143,
  sighashBip341KeyPath,
  signNativeTx,
  signedVmop,
  toHex,
  txData,
  txPayload,
  txVsize,
  verifyVmop,
  xOnlyPublicKey,
} from '../src/index.js';
import { loadVectors, pseudoRandomBytes, testKey } from './helpers.js';

const MAGIC_HEX = '4b534e31';

test('KSN1 data script: byte-for-byte result for a 1,100-byte payload (520 + 520 + 60)', () => {
  const payload = pseudoRandomBytes(1, 1100);
  const expected = concatBytes(
    fromHex(`6a04${MAGIC_HEX}`),
    fromHex('4d0802'),
    payload.slice(0, 520),
    fromHex('4d0802'),
    payload.slice(520, 1040),
    fromHex('3c'),
    payload.slice(1040),
  );
  const script = ksn1DataScript(DEVNET.magic, payload);
  assert.equal(toHex(script), toHex(expected));
  assert.equal(script.length, 1 + 5 + 3 + 520 + 3 + 520 + 1 + 60);
  assert.deepEqual(parseKsn1DataScript(DEVNET.magic, script), payload);
});

test('KSN1 data script: a 1-byte payload uses OP_PUSHBYTES_1 (not OP_1..OP_16); push opcodes for each length', () => {
  for (const byte of [0x00, 0x01, 0x10, 0x81, 0xff]) {
    assert.equal(toHex(ksn1DataScript(DEVNET.magic, Uint8Array.of(byte))), `6a04${MAGIC_HEX}01${byte.toString(16).padStart(2, '0')}`);
  }
  const head = (length: number): string => toHex(ksn1DataScript(DEVNET.magic, new Uint8Array(length).fill(7))).slice(12, 18);
  assert.equal(head(75).slice(0, 2), '4b');
  assert.equal(head(76).slice(0, 4), '4c4c');
  assert.equal(head(255).slice(0, 4), '4cff');
  assert.equal(head(256), '4d0001');
  assert.equal(head(520), '4d0802');
  // 521 bytes: 520 + 1, the last chunk is still OP_PUSHBYTES_1
  const script = ksn1DataScript(DEVNET.magic, new Uint8Array(521).fill(7));
  assert.equal(toHex(script.slice(-2)), '0107');
  assert.throws(() => ksn1DataScript(DEVNET.magic, new Uint8Array(0)), /empty/);
  assert.throws(() => ksn1DataScript(Uint8Array.of(1, 2, 3), Uint8Array.of(1)), /magic.*4/);
});

test('KSN1 parsing (D-032): accepts non-minimal pushes; rejects other opcodes, truncation and chunks over 520 bytes', () => {
  const magic = DEVNET.magic;
  const m = `6a04${MAGIC_HEX}`;
  const parse = (hex: string): string | null => {
    const out = parseKsn1DataScript(magic, fromHex(hex));
    return out === null ? null : toHex(out);
  };
  assert.equal(parse(`${m}03aabbcc`), 'aabbcc');
  assert.equal(parse(`${m}4c03aabbcc`), 'aabbcc', 'non-minimal encoding with OP_PUSHDATA1');
  assert.equal(parse(`${m}4d0300aabbcc`), 'aabbcc', 'OP_PUSHDATA2');
  assert.equal(parse(`${m}4e03000000aabbcc01dd`), 'aabbccdd', 'OP_PUSHDATA4 followed by a direct push');
  assert.equal(parse(`${m}00`), '', 'empty push');
  assert.equal(parse(m), '', 'magic only');
  assert.equal(parse(`6a4d0400${MAGIC_HEX}01ee`), 'ee', 'magic with OP_PUSHDATA2');
  for (const bad of [`${m}51`, `${m}4f`, `${m}6a`, `${m}03aabb`, `${m}4c`, `${m}4d01`, `6a05${MAGIC_HEX}00`, `6a04${MAGIC_HEX.slice(0, 6)}00`, `51${m.slice(2)}`]) {
    assert.equal(parse(bad), null, bad);
  }
  // A chunk over 520 bytes
  assert.equal(parseKsn1DataScript(magic, concatBytes(fromHex(`${m}4d0902`), new Uint8Array(521))), null);
  assert.ok(isKsn1Output(magic, fromHex(`${m}51`)), 'a KSN1 output is judged by its start only; one with a malformed data part still counts as a KSN1 output');
});

/** A transaction with these output scripts and inputs with these witnesses (one input without a witness when none is given). */
function txWith(scripts: Uint8Array[], witnesses: Uint8Array[][] = [[]]): Tx {
  return {
    version: 2,
    locktime: 0,
    inputs: witnesses.map((witness, i) => ({ txid: new Uint8Array(32).fill(i + 1), vout: 0, scriptSig: new Uint8Array(0), sequence: 0, witness })),
    outputs: scripts.map((script) => ({ value: 0n, script })),
  };
}

/** The witness of a script-path spend revealing `leaf` (a signature, the leaf, the control block), with an annex when asked. */
function revealWitness(leaf: Uint8Array, annex = false): Uint8Array[] {
  const witness = [new Uint8Array(64).fill(7), leaf, concatBytes(Uint8Array.of(0xc0), xOnlyPublicKey(testKey(2)))];
  return annex ? [...witness, Uint8Array.of(0x50, 1)] : witness;
}

test('txData: recognized only with exactly one KSN1 output', () => {
  const data = ksn1DataScript(DEVNET.magic, Uint8Array.of(1, 2, 3));
  const other = { value: 1000n, script: p2trScript(xOnlyPublicKey(testKey(1))) };
  assert.deepEqual(txData(DEVNET.magic, txWith([other.script])), { kind: 'none' });
  assert.deepEqual(txData(DEVNET.magic, txWith([other.script, data])), {
    kind: 'one',
    carrier: { kind: 'Output', index: 1 },
    payload: Uint8Array.of(1, 2, 3),
  });
  assert.deepEqual(txData(DEVNET.magic, txWith([data, data])), { kind: 'invalid' });
  assert.deepEqual(txData(DEVNET.magic, txWith([concatBytes(data, Uint8Array.of(0x51))])), { kind: 'invalid' });
  // An OP_RETURN not starting with the magic does not count
  assert.deepEqual(txData(DEVNET.magic, txWith([fromHex('6a0401020304'), data])).kind, 'one');
});

test('txData: the envelopes in the inputs count with the KSN1 outputs, together exactly one (D-081, D-133)', () => {
  const key = xOnlyPublicKey(testKey(1));
  const leaf = ksn1EnvelopeScript(key, DEVNET.magic, Uint8Array.of(4, 5, 6));
  const data = ksn1DataScript(DEVNET.magic, Uint8Array.of(1, 2, 3));
  const one = { kind: 'one', carrier: { kind: 'Input', index: 1 }, payload: Uint8Array.of(4, 5, 6) };
  assert.deepEqual(txData(DEVNET.magic, txWith([], [[], revealWitness(leaf)])), one);
  assert.deepEqual(txData(DEVNET.magic, txWith([], [[], revealWitness(leaf, true)])), one, 'the annex is left out first (BIP341)');
  // A KSN1 output and an envelope, or two envelopes: the transaction is not recognized at all
  assert.deepEqual(txData(DEVNET.magic, txWith([data], [revealWitness(leaf)])), { kind: 'invalid' });
  assert.deepEqual(txData(DEVNET.magic, txWith([], [revealWitness(leaf), revealWitness(leaf)])), { kind: 'invalid' });
  // An envelope is judged by its start: a malformed one is still a carrier, and makes the transaction invalid
  const malformed = concatBytes(leaf.subarray(0, leaf.length - 1), Uint8Array.of(0x51, 0x68));
  assert.ok(isKsn1Envelope(DEVNET.magic, malformed));
  assert.equal(parseKsn1Envelope(DEVNET.magic, malformed), null);
  assert.deepEqual(txData(DEVNET.magic, txWith([], [revealWitness(malformed)])), { kind: 'invalid' });
  // A witness whose leaf is not an envelope, or one of another magic, carries nothing
  assert.deepEqual(txData(DEVNET.magic, txWith([], [revealWitness(other(leaf))])), { kind: 'none' });
  assert.deepEqual(txData(fromHex('4b534e32'), txWith([], [revealWitness(leaf)])), { kind: 'none' });
  assert.deepEqual(txData(DEVNET.magic, txWith([], [[leaf]])), { kind: 'none' }, 'a single item is a key-path spend');

  function other(script: Uint8Array): Uint8Array {
    const copy = Uint8Array.from(script);
    copy[33] = 0xad;
    return copy;
  }
});

test('txPayload: a KSN1 output carries only a path C payload, an envelope only a settlement or a republish one (D-081, D-133)', () => {
  const secret = testKey(21);
  const op = nativeOp(secret, { txid: new Uint8Array(32).fill(1), vout: 0 });
  const native = encodePayload({ kind: 'NativeOp', op }, DEVNET);
  const headers = encodePayload({ kind: 'Headers', headers: [], bodies: [], payee: new Uint8Array(32).fill(9) }, DEVNET);
  const key = xOnlyPublicKey(testKey(1));
  const asOutput = (payload: Uint8Array) => txWith([ksn1DataScript(DEVNET.magic, payload)]);
  const asEnvelope = (payload: Uint8Array) => txWith([], [revealWitness(ksn1EnvelopeScript(key, DEVNET.magic, payload))]);
  assert.deepEqual(txPayload(asOutput(native), DEVNET), { carrier: { kind: 'Output', index: 0 }, payload: { kind: 'NativeOp', op } });
  assert.deepEqual(nativeOpFromTx(asOutput(native), DEVNET), op);
  assert.equal(txPayload(asOutput(headers), DEVNET), null);
  assert.equal(txPayload(asEnvelope(native), DEVNET), null);
  assert.equal(nativeOpFromTx(asEnvelope(native), DEVNET), null);
  assert.deepEqual(txPayload(asEnvelope(headers), DEVNET), {
    carrier: { kind: 'Input', index: 0 },
    payload: { kind: 'Headers', headers: [], bodies: [], payee: new Uint8Array(32).fill(9) },
  });
  // A native operation's transaction that also spends an envelope is no native transaction
  const both = txWith([ksn1DataScript(DEVNET.magic, native)], [revealWitness(ksn1EnvelopeScript(key, DEVNET.magic, headers))]);
  assert.equal(txPayload(both, DEVNET), null);
  assert.equal(nativeOpFromTx(both, DEVNET), null);
});

test('nativePayload is byte-for-byte equal to the NativeOp payload vector in codec.json', () => {
  const vector = loadVectors('codec.json').valid.find((v: { type: string; name: string }) => v.type === 'Payload' && v.name === 'NativeOp');
  const payload = fromJson('Payload', vector.value);
  assert.equal(payload.kind, 'NativeOp');
  if (payload.kind !== 'NativeOp') return;
  assert.equal(toHex(nativePayload(payload.op, DEVNET)), vector.borsh);
  assert.throws(() => nativePayload({ ...payload.op, fee: { kind: 'Token', amount: 1n } }, DEVNET), /BtcNative/);
});

/**
 * A path C deploy operation (the account is the private key's Taproot output key, D-092), its signature bound to the carrying
 * transaction's first input `firstInput` (D-086).
 */
function nativeOp(secret: Uint8Array, firstInput: OutPoint, codeLength = 8): VmOp {
  return signedVmop(
    {
      version: 1,
      chain_id: DEVNET.chain_id,
      account: accountOf(secret),
      nonce: 0n,
      fee: { kind: 'BtcNative' },
      max_gas: DEVNET.native_max_gas,
      action: { kind: 'Deploy', code: pseudoRandomBytes(codeLength, codeLength), init_args: new Uint8Array(0), funds: [] },
    },
    secret,
    new Uint8Array(32),
    firstInput,
  );
}

/** Verifies every input of a signed transaction per BIP341 / BIP143 (P2TR key-path and P2WPKH). */
function verifyInputs(tx: Tx, prevouts: { value: bigint; script: Uint8Array }[]): void {
  tx.inputs.forEach((input, i) => {
    const prev = prevouts[i]!;
    if (prev.script[0] === 0x51) {
      const [sig] = input.witness;
      assert.equal(input.witness.length, 1);
      const hashType = sig!.length === 64 ? SIGHASH_DEFAULT : sig![64]!;
      const sighash = sighashBip341KeyPath(tx, i, prevouts, hashType);
      assert.ok(schnorr.verify(sig!.slice(0, 64), sighash, prev.script.slice(2)), `input ${i}`);
    } else {
      const [sig, pub] = input.witness;
      assert.equal(sig![sig!.length - 1], SIGHASH_ALL);
      const sighash = sighashBip143(tx, i, p2pkhScriptFromHash(hash160(pub!)), prev.value, SIGHASH_ALL);
      assert.ok(ecdsaVerifyDer(sig!.slice(0, -1), sighash, pub!), `input ${i}`);
    }
  });
}

// The funding inputs can be P2TR or P2WPKH UTXOs (the funding wallet address is unrelated to the account; the account is only the
// operation's signer)
for (const kind of ['p2tr', 'p2wpkh'] as const) {
  test(`${kind} funding: build native transaction → sign → extract → recognized as a native transaction`, () => {
    const secret = testKey(kind === 'p2tr' ? 21 : 22);
    const internal = xOnlyPublicKey(secret);
    const script = kind === 'p2tr' ? p2trScript(internal) : p2wpkhScript(compressedPublicKey(secret));
    const utxos = [0, 1, 2].map((i) => ({
      txid: pseudoRandomBytes(100 + i, 32),
      vout: i,
      value: 3_000n,
      script,
      ...(kind === 'p2tr' ? { tapInternalKey: internal } : {}),
    }));
    // The operation is signed against the first UTXO first (D-086), then the transaction is built
    const op = nativeOp(secret, nativeFirstInput(utxos[0]!), 2_000);
    const draft = buildNativeTx({ params: DEVNET, op, utxos, changeScript: script, feeRate: 2 });
    // The data output with 2,000 bytes of code (about 2,100 vB at 2 sat/vB) needs two 3,000-sat UTXOs
    assert.equal(draft.inputs.length, 2);
    const unsigned = Transaction.fromPSBT(draft.psbt, SCURE_TX_OPTIONS);
    assert.equal(unsigned.inputsLength, 2);
    for (let i = 0; i < unsigned.inputsLength; i++) {
      assert.equal(unsigned.getInput(i).sighashType, undefined, 'left to the wallet to sign with the default type (ALL / DEFAULT)');
    }

    const raw = extractTransaction(signNativeTx(draft.psbt, secret, new Uint8Array(32)));
    const tx = parseTx(raw);
    assert.equal(tx.version, 2);
    assert.equal(tx.locktime, 0);
    assert.equal(tx.outputs[0]!.value, 0n);
    assert.deepEqual(tx.inputs[1]!.txid, utxos[1]!.txid, 'txid written in internal byte order');
    assert.equal(tx.inputs[0]!.sequence, 0xffff_fffd);
    const inputsValue = 6_000n;
    const outputsValue = tx.outputs.reduce((sum, o) => sum + o.value, 0n);
    assert.equal(inputsValue - outputsValue, draft.fee, 'fee');
    assert.ok(draft.change !== null && tx.outputs[1]!.value === draft.change.value);
    assert.ok(txVsize(tx) <= draft.vsize, `actual vsize ${txVsize(tx)} does not exceed the estimate ${draft.vsize}`);
    assert.ok(draft.fee >= BigInt(txVsize(tx)) * 2n, 'fee rate not below 2 sat/vB');
    verifyInputs(tx, draft.inputs.map((u) => ({ value: u.value, script: u.script })));
    // The node's recognition rule: exactly one KSN1 output, the payload is NativeOp and fee is BtcNative
    assert.deepEqual(nativeOpFromTx(raw, DEVNET), op);
    assert.deepEqual(nativeOpFromTx(tx, DEVNET), op);
    assert.deepEqual(txPayload(raw, DEVNET)?.payload, { kind: 'NativeOp', op });
    if (kind === 'p2tr') assert.deepEqual(tx.inputs[0]!.witness[0]!.length, 64);
    // The signature is bound to the first input (D-086): verifies against the on-chain transaction's first input, fails with
    // another outpoint; building is refused when the first UTXO is swapped for another
    assert.deepEqual(nativeTxFirstInput(tx), nativeFirstInput(utxos[0]!));
    assert.deepEqual(nativeTxFirstInput(raw), nativeFirstInput(utxos[0]!));
    assert.equal(verifyVmop(op, nativeTxFirstInput(tx)!), 'valid');
    assert.equal(verifyVmop(op, nativeFirstInput(utxos[1]!)), 'invalid');
    assert.equal(verifyVmop(op), 'invalid');
    assert.throws(() => buildNativeTx({ params: DEVNET, op, utxos: [utxos[1]!, utxos[0]!, utxos[2]!], changeScript: script, feeRate: 2 }), /not bound/);
  });
}

test('native transaction: insufficient funds, dust change omitted, limit checks', () => {
  const secret = testKey(23);
  const script = p2wpkhScript(compressedPublicKey(secret));
  const utxo = (value: bigint, i = 0) => ({ txid: pseudoRandomBytes(200 + i, 32), vout: 0, value, script });
  // The first UTXO of every transaction below is utxo(…, 0): the operation is bound to it
  const op = nativeOp(secret, nativeFirstInput(utxo(1n)));
  assert.throws(() => buildNativeTx({ params: DEVNET, op, utxos: [utxo(100n)], changeScript: script, feeRate: 1 }), /insufficient funds/);
  assert.throws(() => buildNativeTx({ params: DEVNET, op, utxos: [], changeScript: script, feeRate: 1 }), /insufficient funds/);
  // Change below 294 sats is omitted; the excess goes to the fee
  const small = buildNativeTx({ params: DEVNET, op, utxos: [utxo(500n)], changeScript: script, feeRate: 1 });
  assert.equal(small.change, null);
  assert.equal(small.fee, 500n);
  const tx = parseTx(extractTransaction(signNativeTx(small.psbt, secret)));
  assert.equal(tx.outputs.length, 1);
  assert.deepEqual(nativeOpFromTx(tx, DEVNET), op);
  // The fee rate is rounded up at a precision of one thousandth of a sat
  const fractional = buildNativeTx({ params: DEVNET, op, utxos: [utxo(100_000n)], changeScript: script, feeRate: 1.5 });
  assert.equal(fractional.fee, BigInt(Math.ceil(fractional.vsize * 1.5)));
  assert.throws(() => buildNativeTx({ params: DEVNET, op, utxos: [utxo(100_000n)], changeScript: script, feeRate: 0 }), /fee rate/);
  // The change script must be a standard output; a UTXO value cannot be negative; the transaction cannot exceed the standard weight limit
  assert.throws(() => buildNativeTx({ params: DEVNET, op, utxos: [utxo(100_000n)], changeScript: Uint8Array.of(0x6a), feeRate: 1 }), /change/);
  assert.throws(() => buildNativeTx({ params: DEVNET, op, utxos: [utxo(-1n)], changeScript: script, feeRate: 1 }), RangeError);
  const many = Array.from({ length: 1_500 }, (_, i) => utxo(1n, i));
  assert.throws(() => buildNativeTx({ params: DEVNET, op, utxos: many, changeScript: script, feeRate: 1 }), /weight limit/);
  // Only P2TR and P2WPKH UTXOs are supported
  const legacy = { ...utxo(100_000n), script: p2pkhScriptFromHash(new Uint8Array(20)) };
  assert.throws(() => buildNativeTx({ params: DEVNET, op, utxos: [legacy], changeScript: script, feeRate: 1 }), /P2TR and P2WPKH/);
  // A VmOp over MAX_OP_BYTES cannot be recognized by the node; building is refused
  const huge = nativeOp(secret, nativeFirstInput(utxo(1n)), DEVNET.max_op_bytes);
  assert.throws(() => nativePayload(huge, DEVNET), /MAX_OP_BYTES/);
  // Recognition: a NativeOp payload whose fee is not BtcNative is not a native transaction
  const wrongFee = { ...op, fee: { kind: 'Token' as const, amount: 1n } };
  const fakeTx: Tx = {
    version: 2,
    locktime: 0,
    inputs: [{ txid: new Uint8Array(32), vout: 0, scriptSig: new Uint8Array(0), sequence: 0, witness: [] }],
    outputs: [{ value: 0n, script: ksn1DataScript(DEVNET.magic, encodePayload({ kind: 'NativeOp', op: wrongFee }, DEVNET)) }],
  };
  assert.equal(nativeOpFromTx(fakeTx, DEVNET), null);
  assert.equal(nativeOpFromTx(Uint8Array.of(1, 2, 3), DEVNET), null);
  assert.deepEqual(nativeTxFirstInput(fakeTx), { txid: new Uint8Array(32), vout: 0 });
  assert.equal(nativeTxFirstInput(Uint8Array.of(1, 2, 3)), null);
  // A coinbase is not a native transaction and has no first input (D-086)
  const coinbase: Tx = {
    ...fakeTx,
    inputs: [{ txid: new Uint8Array(32), vout: 0xffff_ffff, scriptSig: Uint8Array.of(0x01, 0x64), sequence: 0xffff_ffff, witness: [] }],
    outputs: [{ value: 0n, script: ksn1DataScript(DEVNET.magic, encodePayload({ kind: 'NativeOp', op }, DEVNET)) }],
  };
  assert.equal(nativeOpFromTx(coinbase, DEVNET), null);
  assert.equal(nativeTxFirstInput(coinbase), null);
  assert.deepEqual(nativeOpFromTx({ ...coinbase, inputs: fakeTx.inputs }, DEVNET), op);
});
