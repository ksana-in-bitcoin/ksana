/**
 * A client's network is the node's (A2-11, D-106): nothing in the SDK defaults to devnet; `paramsForChainId` and
 * `KsanaRpcClient.getParams` pick the parameters by the node's chain ID and refuse a node on another network or on none; and a
 * path C transaction is never built for an operation signed for another network's chain ID, which the node would force-include
 * and fail while the BTC payment confirms.
 */

import assert from 'node:assert/strict';
import { test } from 'node:test';

import {
  ChainMismatchError,
  DEVNET,
  type ExchangeLock,
  KsanaRpcClient,
  MAINNET,
  NETWORKS,
  type OutPoint,
  type Params,
  SIGNET,
  type VmOp,
  accountOf,
  btcNetworkOf,
  buildNativeTx,
  buildTopUp,
  compressedPublicKey,
  decode,
  encode,
  extractTransaction,
  nativeFirstInput,
  nativeOpFromTx,
  nativePayload,
  networkOfChainId,
  p2trScript,
  p2wpkhScript,
  paramsForChainId,
  signNativeTx,
  signedVmop,
  xOnlyPublicKey,
} from '../src/index.js';
import { pseudoRandomBytes, testKey } from './helpers.js';

test('networks are looked up by chain ID, as Rust does with Network::from_chain_id', () => {
  assert.equal(networkOfChainId(1001), 'devnet');
  assert.equal(networkOfChainId(1002), 'signet');
  assert.equal(networkOfChainId(1000), 'mainnet');
  assert.equal(networkOfChainId(4242), null);
  for (const [name, params] of Object.entries(NETWORKS)) {
    assert.equal(networkOfChainId(params.chain_id), name);
    assert.equal(paramsForChainId(params.chain_id), params);
    assert.equal(paramsForChainId(params.chain_id, params), params);
  }
  // the BTC network of a chain ID of no network is unknown, not regtest
  assert.equal(btcNetworkOf(MAINNET), 'mainnet');
  assert.throws(() => btcNetworkOf({ ...DEVNET, chain_id: 4242 }), ChainMismatchError);
});

test('a node on another network than expected, or on none, is refused', () => {
  assert.throws(
    () => paramsForChainId(SIGNET.chain_id, DEVNET),
    (e: unknown) => {
      assert.ok(e instanceof ChainMismatchError);
      assert.equal(e.chainId, 1002);
      assert.equal(e.expected, 1001);
      assert.match(e.message, /signet \(chain ID 1002\), not devnet \(chain ID 1001\)/);
      return true;
    },
  );
  assert.throws(() => paramsForChainId(MAINNET.chain_id, SIGNET), ChainMismatchError);
  assert.throws(
    () => paramsForChainId(4242),
    (e: unknown) => e instanceof ChainMismatchError && e.expected === null && /4242 belongs to no Ksana network/.test(e.message),
  );
  assert.throws(() => paramsForChainId(4242, DEVNET), ChainMismatchError);
});

/** A node reporting chain ID `chainId` in `ksana_getChainInfo`; records the methods called. */
function fakeNode(chainId: number, calls: string[]): typeof fetch {
  const at = { height: '7', hash: '11'.repeat(32) };
  return (async (_input: unknown, init?: RequestInit) => {
    const body = JSON.parse(String(init?.body));
    calls.push(body.method);
    if (body.method !== 'ksana_getChainInfo') throw new Error(`unexpected ${body.method}`);
    const result = { chain_id: chainId, tip: at, daa_score: '7', protocol_version: 2, next_upgrade: null, max_known_version: 2, finalized: at };
    return new Response(JSON.stringify({ jsonrpc: '2.0', id: body.id, result }), { status: 200 });
  }) as typeof fetch;
}

test("getParams takes the parameters from the node's chain ID and refuses a mismatch", async () => {
  const calls: string[] = [];
  const signet = new KsanaRpcClient('http://node', { fetch: fakeNode(SIGNET.chain_id, calls) });
  assert.equal(await signet.getParams(), SIGNET);
  assert.equal(await signet.getParams(SIGNET), SIGNET);
  await assert.rejects(signet.getParams(DEVNET), ChainMismatchError);
  await assert.rejects(signet.getParams(MAINNET), /not mainnet/);
  const unknown = new KsanaRpcClient('http://node', { fetch: fakeNode(4242, calls) });
  await assert.rejects(unknown.getParams(), /belongs to no Ksana network/);
  await assert.rejects(unknown.getParams(DEVNET), ChainMismatchError);
  assert.deepEqual([...new Set(calls)], ['ksana_getChainInfo']);
});

/** A path C operation for the network of `params`, signed over `firstInput` (D-086). */
function nativeOp(params: Params, secret: Uint8Array, firstInput: OutPoint): VmOp {
  return signedVmop(
    {
      version: 2,
      chain_id: params.chain_id,
      account: accountOf(secret),
      nonce: 0n,
      fee: { kind: 'BtcNative' },
      max_gas: params.native_max_gas,
      action: { kind: 'AssetTransfer', asset: new Uint8Array(32), to: new Uint8Array(32).fill(7), amount: 5n },
    },
    secret,
    new Uint8Array(32),
    firstInput,
  );
}

test("a path C transaction is not built for an operation signed for another network's chain ID", () => {
  const secret = testKey(51);
  const script = p2trScript(xOnlyPublicKey(secret));
  const utxos = [{ txid: pseudoRandomBytes(300, 32), vout: 0, value: 50_000n, script, tapInternalKey: xOnlyPublicKey(secret) }];
  const devnetOp = nativeOp(DEVNET, secret, nativeFirstInput(utxos[0]!));
  for (const params of [SIGNET, MAINNET]) {
    assert.throws(
      () => buildNativeTx({ params, op: devnetOp, utxos, changeScript: script, feeRate: 1 }),
      (e: unknown) => e instanceof ChainMismatchError && e.chainId === DEVNET.chain_id && e.expected === params.chain_id,
    );
    assert.throws(() => nativePayload(devnetOp, params), ChainMismatchError);
  }
  // signed for the node's network it is built, and carries that chain ID
  const signetOp = nativeOp(SIGNET, secret, nativeFirstInput(utxos[0]!));
  const draft = buildNativeTx({ params: SIGNET, op: signetOp, utxos, changeScript: script, feeRate: 1 });
  const raw = extractTransaction(signNativeTx(draft.psbt, secret, new Uint8Array(32)));
  assert.equal(nativeOpFromTx(raw, SIGNET)?.chain_id, SIGNET.chain_id);
  // a JavaScript caller that leaves out the parameters gets an error, not devnet's
  assert.throws(() => buildNativeTx({ op: signetOp, utxos, changeScript: script, feeRate: 1 } as never), /params is required/);
});

test("a top-up's claim operation is signed for the network it is built for", () => {
  const secret = testKey(52);
  const script = p2wpkhScript(compressedPublicKey(secret));
  const beneficiary = new Uint8Array(32).fill(0xb1);
  const lock: ExchangeLock = {
    seller: new Uint8Array(32).fill(0x5e),
    beneficiary,
    script: p2trScript(xOnlyPublicKey(testKey(53))),
    price: 3n,
    ksa: 30_000n,
    expiry: 1_000,
    state: { kind: 'Open' },
  };
  const args = {
    exchange: new Uint8Array(32).fill(0xec),
    lockId: 0n,
    lock,
    beneficiary,
    sats: 10_000n,
    currentBtcHeight: 900,
    utxos: [{ txid: pseudoRandomBytes(301, 32), vout: 0, value: 60_000n, script }],
    changeScript: script,
    feeRate: 1,
  };
  for (const params of [DEVNET, SIGNET, MAINNET]) {
    assert.equal(buildTopUp({ ...args, params }).op.chain_id, params.chain_id);
  }
  assert.throws(() => buildTopUp(args as never), /params is required/);
});

test("a payload has no generic Borsh codec: its encoding depends on the network", () => {
  const op = nativeOp(DEVNET, testKey(54), { txid: new Uint8Array(32), vout: 0 });
  assert.throws(() => encode('Payload' as never, { kind: 'NativeOp', op } as never), /encodePayload\(payload, params\)/);
  assert.throws(() => decode('Payload' as never, nativePayload(op, DEVNET), 1_000), /decodePayload\(bytes, params\)/);
});
