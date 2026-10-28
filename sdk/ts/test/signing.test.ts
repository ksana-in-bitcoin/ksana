/**
 * Shared vectors `tests/vectors/signing.json`: tagged hash, BIP340 signatures (the account is the Taproot output key, signed
 * with the tweaked secret key, D-092; reproduced byte for byte), path C signatures bound to the first input of the carrying
 * transaction (D-086), verification failing after tampering, BIP322 simple cases (message, message_hash, to_spend / to_sign
 * transaction IDs, witness reproduced byte for byte, D-052).
 */

import assert from 'node:assert/strict';
import { test } from 'node:test';

import {
  MockUnisatWallet,
  UnisatAdapter,
  VMOP_SIGHASH_TAG,
  type VmOp,
  accountOf,
  bip322Message,
  bip322MessageHash,
  bip322ToSign,
  bip322ToSpend,
  concatBytes,
  decodeBip322Signature,
  encodeOutPoint,
  encodeVmOp,
  encodeVmOpUnsigned,
  fromHex,
  fromJson,
  opId,
  p2trScriptFromOutputKey,
  signBip322Simple,
  signVmop,
  signVmopBip322,
  signVmopWithWallet,
  signedVmop,
  taggedHash,
  toHex,
  taprootOutputKey,
  txid,
  u32le,
  verifyBip322ForAccount,
  verifyVmop,
  verifyVmopBip322,
  vmopSighash,
  vmopSigningMessage,
  xOnlyPublicKey,
} from '../src/index.js';
import { loadVectors } from './helpers.js';

const vectors = loadVectors('signing.json');

test('the sighash tag', () => {
  assert.equal(vectors.tag, VMOP_SIGHASH_TAG);
});

test('signing.json: tagged_hash', () => {
  assert.ok(vectors.tagged_hash.length >= 3);
  for (const t of vectors.tagged_hash) {
    assert.equal(toHex(taggedHash(t.tag, fromHex(t.msg))), t.hash, t.tag);
  }
});

/** The vector's `first_input` (only path C cases have one). */
function firstInputOf(c: { first_input?: { txid: string; vout: number } }) {
  return c.first_input === undefined ? undefined : { txid: fromHex(c.first_input.txid, 32), vout: c.first_input.vout };
}

test('signing.json: path C cases carry first_input', () => {
  assert.ok(vectors.cases.some((c: { first_input?: unknown }) => c.first_input !== undefined));
});

for (const c of vectors.cases) {
  test(`signing.json cases: ${c.name}`, () => {
    const secret = fromHex(c.secret_key, 32);
    assert.equal(toHex(accountOf(secret)), c.account, 'account');
    // the account is the BIP86-tweaked output key of the internal key, not the internal key itself (D-092)
    assert.equal(toHex(taprootOutputKey(xOnlyPublicKey(secret))), c.account);
    assert.notEqual(toHex(xOnlyPublicKey(secret)), c.account);
    const op = fromJson('VmOp', c.op);
    assert.equal(toHex(op.account), c.account, 'op.account');
    assert.equal(toHex(encodeVmOpUnsigned(op)), c.unsigned_borsh, 'signing message content');
    // the path C signing message ends with the encoding of the carrying transaction's first input (txid ‖ little-endian vout)
    // (D-086); on path B it is just unsigned_borsh
    const firstInput = firstInputOf(c);
    assert.equal(firstInput !== undefined, op.fee.kind === 'BtcNative', 'path C cases carry first_input');
    const message =
      firstInput === undefined
        ? fromHex(c.unsigned_borsh)
        : concatBytes(fromHex(c.unsigned_borsh), fromHex(c.first_input.txid, 32), u32le(c.first_input.vout));
    if (firstInput !== undefined) assert.equal(toHex(encodeOutPoint(firstInput)), c.first_input.txid + toHex(u32le(firstInput.vout)));
    assert.deepEqual(vmopSigningMessage(op, firstInput), message, 'signing message');
    assert.equal(toHex(vmopSighash(op, firstInput)), c.sighash, 'sighash');
    // the BIP340 signature is determined by (secret_key, sighash, aux_rand): reproduced byte for byte
    assert.deepEqual(signVmop(op, secret, fromHex(c.aux_rand, 32), firstInput), op.sig, 'signature');
    assert.deepEqual(signedVmop(op, secret, fromHex(c.aux_rand, 32), firstInput), op);
    assert.equal(verifyVmop(op, firstInput), 'valid', 'verification');
    const other = { txid: new Uint8Array(32), vout: 7 };
    if (firstInput === undefined) {
      // path B is not bound to an outpoint: rejected when one is given
      assert.throws(() => vmopSighash(op, other), /path B/);
      assert.equal(verifyVmop(op, other), 'invalid');
    } else {
      // path C: neither omitting the outpoint nor swapping it (another transaction, another output) passes
      assert.throws(() => vmopSighash(op), /firstInput/);
      assert.equal(verifyVmop(op), 'invalid');
      assert.equal(verifyVmop(op, other), 'invalid');
      assert.equal(verifyVmop(op, { ...firstInput, vout: firstInput.vout + 1 }), 'invalid');
    }
    assert.equal(toHex(encodeVmOp(op)), c.borsh, 'encoding');
    assert.equal(toHex(opId(op)), c.op_id, 'op_id');
  });
}

test('signing.json tampered: verification fails after tampering with each field', () => {
  const original = fromJson('VmOp', vectors.cases[0].op);
  // 5 scalar fields, swapping the fee variant and the Token amount, swapping the action variant and the 4 sub-fields of Call,
  // the signature bytes and the signature scheme (since D-078 only the Token fee has sub-fields)
  assert.ok(vectors.tampered.length >= 14);
  for (const t of vectors.tampered) {
    const op = fromJson('VmOp', t.op);
    assert.notDeepEqual(op, original, `${t.name}: must differ from the original operation`);
    assert.notEqual(verifyVmop(op), 'valid', `${t.name}: verification still passes after tampering`);
  }
  // swapping the Bip340 signature for Bip322Simple: the SDK does not verify Bip322Simple (the Rust side is authoritative)
  const scheme = vectors.tampered.find((t: { name: string }) => t.name === 'sig scheme');
  assert.equal(verifyVmop(fromJson('VmOp', scheme.op)), 'unsupported');
});

test('the signing secret key must match the account', () => {
  const c = vectors.cases[0];
  const op = fromJson('VmOp', c.op);
  assert.throws(() => signVmop(op, fromHex(vectors.cases[1].secret_key, 32), new Uint8Array(32)), /does not match/);
  // without aux_rand the signature is still valid (random)
  assert.equal(verifyVmop(signedVmop(op, fromHex(c.secret_key, 32))), 'valid');
});

const bip322Cases: unknown[] | undefined = vectors.bip322;

if (bip322Cases === undefined) {
  test.skip('signing.json bip322: the vector file has no bip322 section yet (D-052), skipped');
} else {
  test('signing.json bip322: at least one case, all P2TR (since D-092 an account has only a Taproot address)', () => {
    const types = (bip322Cases as { address_type: string }[]).map((c) => c.address_type);
    assert.ok(types.length >= 1);
    assert.ok(types.every((t) => t === 'p2tr'));
  });
  for (const raw of bip322Cases) {
    const c = raw as Record<string, string>;
    test(`signing.json bip322: ${c.name}`, async () => {
      const secret = fromHex(c.secret_key!, 32);
      const account = fromHex(c.account!, 32);
      assert.deepEqual(accountOf(secret), account, 'account');
      const op: VmOp = fromJson('VmOp', (raw as { op: unknown }).op);
      assert.deepEqual(op.account, account, 'op.account');
      assert.equal(op.sig.kind, 'Bip322Simple');
      // address: the account's Taproot address OP_1 <account>
      const script = p2trScriptFromOutputKey(account);
      assert.equal(toHex(script), c.script_pubkey, 'script_pubkey');
      // message = lowercase hex of the sighash
      assert.equal(toHex(vmopSighash(op)), c.sighash, 'sighash');
      assert.equal(bip322Message(op), c.message, 'message');
      assert.equal(c.message, c.sighash);
      assert.equal(toHex(bip322MessageHash(c.message!)), c.message_hash, 'message_hash');
      // transaction IDs of to_spend / to_sign (internal byte order)
      const toSpend = bip322ToSpend(script, c.message!);
      assert.equal(toHex(txid(toSpend)), c.to_spend_txid, 'to_spend_txid');
      assert.equal(toHex(txid(bip322ToSign(txid(toSpend)))), c.to_sign_txid, 'to_sign_txid');
      // the witness is reproduced byte for byte (determined by aux_rand)
      const witness = signBip322Simple(secret, c.message!, fromHex(c.aux_rand!, 32));
      assert.equal(toHex(witness), c.witness, 'witness');
      assert.equal(toHex(op.sig.bytes), c.witness, 'op.sig');
      const unsigned = { ...op } as Partial<VmOp>;
      delete unsigned.sig;
      assert.deepEqual(signVmopBip322(unsigned as Omit<VmOp, 'sig'>, secret, fromHex(c.aux_rand!, 32)), op);
      assert.equal(toHex(encodeVmOp(op)), c.op_borsh, 'op_borsh');
      assert.equal(toHex(opId(op)), c.op_id, 'op_id');
      // the local self-check passes; the SDK's verifyVmop does not verify Bip322Simple
      assert.ok(verifyBip322ForAccount(account, c.message!, witness));
      assert.ok(verifyVmopBip322(op));
      assert.equal(verifyVmop(op), 'unsupported');
      // the mock wallet (Unisat interface) reproduces the same witness byte for byte, and the wallet flow yields the same VmOp
      const wallet = new MockUnisatWallet({ secretKey: secret, auxRand: fromHex(c.aux_rand!, 32) });
      assert.equal(toHex(decodeBip322Signature(await wallet.signMessage(c.message!, 'bip322-simple'))), c.witness);
      assert.deepEqual(await signVmopWithWallet(unsigned as Omit<VmOp, 'sig'>, new UnisatAdapter(wallet)), op);
    });
  }
}

test('a BIP322-signed path C operation is likewise bound to the first input of the carrying transaction (D-086)', async () => {
  const c = vectors.cases.find((v: { first_input?: unknown }) => v.first_input !== undefined);
  const secret = fromHex(c.secret_key, 32);
  const firstInput = firstInputOf(c)!;
  const other = { ...firstInput, vout: firstInput.vout + 1 };
  const unsigned = { ...fromJson('VmOp', c.op) } as Partial<VmOp>;
  delete unsigned.sig;
  const op = unsigned as Omit<VmOp, 'sig'>;
  const aux = new Uint8Array(32);
  assert.throws(() => signVmopBip322(op, secret, aux), /firstInput/, 'a path C operation must give firstInput');
  assert.throws(() => bip322Message(op), /firstInput/);
  const signed = signVmopBip322(op, secret, aux, firstInput);
  assert.equal(signed.sig.kind, 'Bip322Simple');
  assert.equal(bip322Message(op, firstInput), toHex(vmopSighash(op, firstInput)));
  assert.ok(verifyVmopBip322(signed, firstInput));
  assert.ok(!verifyVmopBip322(signed), 'fails without the outpoint');
  assert.ok(!verifyVmopBip322(signed, other), 'fails with a different outpoint');
  // wallet flow: the same outpoint as firstInput yields the same VmOp; rejected when it is not given
  const wallet = new UnisatAdapter(new MockUnisatWallet({ secretKey: secret, auxRand: aux }));
  assert.deepEqual(await signVmopWithWallet(op, wallet, { firstInput }), signed);
  await assert.rejects(signVmopWithWallet(op, wallet), /firstInput/);
});
