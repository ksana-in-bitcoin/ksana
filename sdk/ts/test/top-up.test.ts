/**
 * Top-up (D-078, D-085): encoding and decoding of exchange-contract locks (same Borsh field order as the contract's `Lock`), the
 * `lock` / `claim` arguments, quotes, and building the top-up transaction (data output + amount paid to the lock's payment script
 * + change) → signing → extracting → recognizing it as a native transaction; the claim operation is signed by a one-time key with
 * nonce 0.
 */

import assert from 'node:assert/strict';
import { test } from 'node:test';

import {
  DEFAULT_EXPIRY_MARGIN,
  DEVNET,
  type ExchangeLock,
  accountOf,
  buildNativeTx,
  buildTopUp,
  claimArgs,
  compressedPublicKey,
  concatBytes,
  decodeLock,
  encodeLock,
  extractTransaction,
  lockArgs,
  nativeFirstInput,
  nativeOpFromTx,
  p2trScript,
  p2wpkhScript,
  parseTx,
  quoteTopUp,
  signNativeTx,
  u32le,
  u64le,
  verifyVmop,
  xOnlyPublicKey,
} from '../src/index.js';
import { pseudoRandomBytes, testKey } from './helpers.js';

function u128le(value: bigint): Uint8Array {
  const out = new Uint8Array(16);
  for (let i = 0; i < 16; i += 1) out[i] = Number((value >> BigInt(8 * i)) & 0xffn);
  return out;
}

const SELLER = Uint8Array.from({ length: 32 }, () => 0x5e);
const BUYER = Uint8Array.from({ length: 32 }, () => 0xb1);
const SCRIPT = p2trScript(xOnlyPublicKey(testKey(40)));
const EXCHANGE = Uint8Array.from({ length: 32 }, () => 0xec);
const EXPIRY = 1_000;

function lock(overrides: Partial<ExchangeLock> = {}): ExchangeLock {
  return {
    seller: SELLER,
    beneficiary: BUYER,
    script: SCRIPT,
    price: 3n,
    ksa: 30_000n,
    expiry: EXPIRY,
    state: { kind: 'Open' },
    ...overrides,
  };
}

test('lock: encodes and decodes in the contract Borsh field order; rejects trailing bytes, truncation and an unknown state variant', () => {
  const open = lock({ price: (1n << 127n) - 1n, ksa: (1n << 100n) + 7n });
  // Hand-written encoding identical to the Rust contract's `Lock`
  const manual = concatBytes(
    open.seller,
    open.beneficiary,
    u32le(open.script.length),
    open.script,
    u128le(open.price),
    u128le(open.ksa),
    u32le(open.expiry),
    Uint8Array.of(0),
  );
  assert.deepEqual(encodeLock(open), manual);
  assert.deepEqual(decodeLock(manual), open);
  const claimed = lock({ state: { kind: 'Claimed', paidSats: 5_000n, ksaPaid: 15_000n } });
  const prefix = concatBytes(SELLER, BUYER, u32le(SCRIPT.length), SCRIPT);
  assert.deepEqual(
    encodeLock(claimed),
    concatBytes(prefix, u128le(3n), u128le(30_000n), u32le(EXPIRY), Uint8Array.of(1), u64le(5_000n), u128le(15_000n)),
  );
  assert.deepEqual(decodeLock(encodeLock(claimed)), claimed);
  const released = lock({ state: { kind: 'Released' } });
  assert.deepEqual(decodeLock(encodeLock(released)), released);
  assert.throws(() => decodeLock(concatBytes(manual, Uint8Array.of(0))), /trailing bytes/);
  assert.throws(() => decodeLock(manual.slice(0, -1)), /truncated/);
  const badState = Uint8Array.from(manual);
  badState[badState.length - 1] = 3;
  assert.throws(() => decodeLock(badState), /state variant/);
});

test('lock / claim arguments and quotes', () => {
  assert.deepEqual(
    lockArgs({ ksa: 30_000n, price: 3n, script: SCRIPT, beneficiary: BUYER, expiry: EXPIRY }),
    concatBytes(u128le(30_000n), u128le(3n), u32le(SCRIPT.length), SCRIPT, BUYER, u32le(EXPIRY)),
  );
  assert.throws(() => lockArgs({ ksa: 1n, price: 1n, script: new Uint8Array(65), beneficiary: BUYER, expiry: 1 }), /payment script/);
  assert.throws(() => lockArgs({ ksa: 1n, price: 1n, script: SCRIPT, beneficiary: new Uint8Array(31), expiry: 1 }), /beneficiary account/);
  assert.throws(() => lockArgs({ ksa: -1n, price: 1n, script: SCRIPT, beneficiary: BUYER, expiry: 1 }), RangeError);
  assert.throws(() => lockArgs({ ksa: 1n, price: 1n, script: SCRIPT, beneficiary: BUYER, expiry: 2 ** 32 }), RangeError);
  assert.deepEqual(claimArgs(7n), u64le(7n));
  // Quote: min(sats × price, ksa); all the KSA costs ceil(ksa / price) sats
  assert.deepEqual(quoteTopUp(lock(), 1_000n), { ksa: 3_000n, satsForAll: 10_000n });
  assert.deepEqual(quoteTopUp(lock(), 20_000n), { ksa: 30_000n, satsForAll: 10_000n });
  assert.deepEqual(quoteTopUp(lock({ ksa: 1_000n }), 334n), { ksa: 1_000n, satsForAll: 334n });
  // The same saturating multiplication as the contract
  assert.deepEqual(quoteTopUp(lock({ price: 1n << 127n, ksa: (1n << 128n) - 1n }), 4n), {
    ksa: (1n << 128n) - 1n,
    satsForAll: 2n,
  });
});

for (const kind of ['p2tr', 'p2wpkh'] as const) {
  test(`${kind}: top-up transaction = data output + amount paid to the lock + change; the claim operation is signed by a one-time key and carries only the lock id`, () => {
    const secret = testKey(kind === 'p2tr' ? 31 : 32);
    const internal = xOnlyPublicKey(secret);
    const script = kind === 'p2tr' ? p2trScript(internal) : p2wpkhScript(compressedPublicKey(secret));
    const utxos = [
      {
        txid: pseudoRandomBytes(200, 32),
        vout: 1,
        value: 60_000n,
        script,
        ...(kind === 'p2tr' ? { tapInternalKey: internal } : {}),
      },
    ];
    const oneTimeKey = testKey(33);
    const draft = buildTopUp({
      exchange: EXCHANGE,
      lockId: 4n,
      lock: lock(),
      beneficiary: BUYER,
      sats: 10_000n,
      currentBtcHeight: 900,
      utxos,
      changeScript: script,
      feeRate: 2,
      params: DEVNET,
      oneTimeKey,
    });
    assert.equal(draft.ksa, 30_000n);
    // Claim operation: a one-time account (the one-time key's Taproot output key, D-092), nonce 0, path C, calling the exchange contract's claim(lock)
    assert.deepEqual(draft.op.account, accountOf(oneTimeKey));
    assert.notDeepEqual(draft.op.account, xOnlyPublicKey(oneTimeKey));
    assert.equal(draft.op.nonce, 0n);
    assert.deepEqual(draft.op.fee, { kind: 'BtcNative' });
    assert.equal(draft.op.max_gas, 1_000_000n, 'defaults to NATIVE_MAX_GAS');
    assert.deepEqual(draft.op.action, {
      kind: 'Call',
      contract: EXCHANGE,
      method: 'claim',
      args: claimArgs(4n),
      funds: [],
    });
    // The claim operation's signature is bound to the payment transaction's first input (D-086): copied into another transaction it fails authentication
    assert.equal(verifyVmop(draft.op, nativeFirstInput(utxos[0]!)), 'valid');
    assert.equal(verifyVmop(draft.op), 'invalid');

    const raw = extractTransaction(signNativeTx(draft.psbt, secret, new Uint8Array(32)));
    const tx = parseTx(raw);
    assert.equal(tx.outputs.length, 3);
    assert.deepEqual(tx.outputs[1], { value: 10_000n, script: SCRIPT });
    assert.equal(tx.outputs[2]!.value, 60_000n - 10_000n - draft.fee);
    assert.deepEqual(nativeOpFromTx(raw, DEVNET), draft.op);
  });
}

test('top-up: refuses a mismatched beneficiary account, a lock that is not Open, imminent expiry, over- / underpayment (allowOverpay / allowPartial permit them) and an oversized gas limit; the one-time key is random by default', () => {
  const secret = testKey(34);
  const script = p2wpkhScript(compressedPublicKey(secret));
  const utxos = [{ txid: pseudoRandomBytes(201, 32), vout: 0, value: 60_000n, script }];
  const base = {
    exchange: EXCHANGE,
    lockId: 0n,
    beneficiary: BUYER,
    sats: 10_000n,
    currentBtcHeight: 900,
    utxos,
    changeScript: script,
    feeRate: 1,
    params: DEVNET,
  };
  // The beneficiary account must be the buyer's own account
  assert.throws(() => buildTopUp({ ...base, lock: lock(), beneficiary: SELLER }), /beneficiary account/);
  assert.throws(() => buildTopUp({ ...base, lock: lock(), beneficiary: new Uint8Array(31) }), /beneficiary account/);
  // The gas limit cannot exceed native_max_gas (otherwise the operation is void and the nonce consumed)
  assert.throws(() => buildTopUp({ ...base, lock: lock(), maxGas: DEVNET.native_max_gas + 1n }), RangeError);
  assert.throws(() => buildTopUp({ ...base, lock: lock(), maxGas: 0n }), RangeError);
  assert.equal(buildTopUp({ ...base, lock: lock(), maxGas: DEVNET.native_max_gas }).op.max_gas, DEVNET.native_max_gas);
  // Latest confirmation height: expiry + RELEASE_DELAY − NATIVE_CONFIRMATIONS − 1
  assert.equal(buildTopUp({ ...base, lock: lock() }).payBy, EXPIRY + 6 - DEVNET.native_confirmations - 1);
  assert.throws(() => buildTopUp({ ...base, lock: lock({ state: { kind: 'Claimed', paidSats: 1n, ksaPaid: 3n } }) }), /not Open \(Claimed\)/);
  assert.throws(() => buildTopUp({ ...base, lock: lock({ state: { kind: 'Released' } }) }), /not Open \(Released\)/);
  // Expiry check: refused when current height + margin ≥ expiry height
  assert.equal(DEFAULT_EXPIRY_MARGIN, 3);
  assert.throws(() => buildTopUp({ ...base, lock: lock(), currentBtcHeight: EXPIRY - 3 }), /expires/);
  assert.throws(() => buildTopUp({ ...base, lock: lock(), currentBtcHeight: EXPIRY + 10 }), /expires/);
  assert.ok(buildTopUp({ ...base, lock: lock(), currentBtcHeight: EXPIRY - 4 }));
  assert.throws(() => buildTopUp({ ...base, lock: lock(), currentBtcHeight: EXPIRY - 4, expiryMargin: 4 }), /expires/);
  assert.ok(buildTopUp({ ...base, lock: lock(), currentBtcHeight: EXPIRY - 1, expiryMargin: 0 }));
  // Overpayment: 30 000 KSA at 3 per sat need only 10 000 sats
  assert.throws(() => buildTopUp({ ...base, lock: lock(), sats: 10_001n }), /overpaid/);
  const over = buildTopUp({ ...base, lock: lock(), sats: 10_001n, allowOverpay: true });
  assert.equal(over.ksa, 30_000n);
  // A remainder below one sat is not an overpayment: 1 000 KSA at 3 per sat cost 334 sats
  assert.equal(buildTopUp({ ...base, lock: lock({ ksa: 1_000n }), sats: 334n }).ksa, 1_000n);
  // Underpayment: buys only a part and leaves the rest locked; refused by default
  assert.throws(() => buildTopUp({ ...base, lock: lock(), sats: 9_999n }), /underpay/);
  assert.equal(buildTopUp({ ...base, lock: lock(), sats: 9_999n, allowPartial: true }).ksa, 29_997n);
  assert.throws(() => buildTopUp({ ...base, lock: lock(), sats: 0n }), RangeError);
  assert.throws(() => buildTopUp({ ...base, lock: lock(), currentBtcHeight: -1 }), RangeError);
  assert.throws(() => buildTopUp({ ...base, lock: lock(), utxos: [] }), /no UTXO available/);
  // Every build uses a fresh one-time account
  const a = buildTopUp({ ...base, lock: lock() });
  const b = buildTopUp({ ...base, lock: lock() });
  assert.notDeepEqual(a.op.account, b.op.account);
});

test('payment outputs of a native transaction: standard script, not below the dust limit; funds must cover payments and fee', () => {
  const secret = testKey(35);
  const script = p2wpkhScript(compressedPublicKey(secret));
  const draft = buildTopUp({
    exchange: EXCHANGE,
    lockId: 0n,
    lock: lock(),
    beneficiary: BUYER,
    sats: 1_000n,
    allowPartial: true,
    currentBtcHeight: 900,
    utxos: [{ txid: pseudoRandomBytes(202, 32), vout: 0, value: 60_000n, script }],
    changeScript: script,
    feeRate: 1,
    params: DEVNET,
  });
  const op = draft.op;
  // The same first input (the operation's signature is bound to it, D-086), but only 5,000 sats
  const utxos = [{ txid: pseudoRandomBytes(202, 32), vout: 0, value: 5_000n, script }];
  assert.throws(
    () => buildNativeTx({ params: DEVNET, op, utxos, changeScript: script, feeRate: 1, payments: [{ value: 100n, script: SCRIPT }] }),
    /dust/,
  );
  assert.throws(
    () => buildNativeTx({ params: DEVNET, op, utxos, changeScript: script, feeRate: 1, payments: [{ value: 1_000n, script: Uint8Array.of(0x6a) }] }),
    /locking script/,
  );
  assert.throws(
    () => buildNativeTx({ params: DEVNET, op, utxos, changeScript: script, feeRate: 1, payments: [{ value: 4_900n, script: SCRIPT }] }),
    /insufficient funds/,
  );
  // Not enough for a non-dust change after the payment: the change is omitted and the remainder all goes to the fee
  const tight = buildNativeTx({ params: DEVNET, op, utxos, changeScript: script, feeRate: 1, payments: [{ value: 4_400n, script: SCRIPT }] });
  assert.equal(tight.change, null);
  assert.equal(tight.fee, 600n);
});
