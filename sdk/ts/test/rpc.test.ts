/**
 * JSON-RPC client (`docs/RPC.md`, D-054): request shape (method names, positional parameters), result parsing (unknown fields
 * rejected), error codes, transport errors and timeouts, WebSocket subscription. A mock `fetch` stands in for the node.
 */

import assert from 'node:assert/strict';
import { test } from 'node:test';

import {
  IndexerRpcClient,
  JsonRpcClient,
  KsanaRpcClient,
  MAX_OPS_LIMIT,
  RPC_ERROR_CODES,
  RpcError,
  RpcTransportError,
  type WebSocketLike,
  blockIdParam,
  encodeVmOp,
  formatOpPosition,
  fromHex,
  fromJson,
  parseOpPosition,
  subscribeBlocks,
  toHex,
  toJson,
} from '../src/index.js';
import { loadVectors } from './helpers.js';

const codec = loadVectors('codec.json');
const H = (byte: string): string => byte.repeat(32);

interface Recorded {
  url: string;
  method: string;
  params: unknown[];
  headers: Record<string, string>;
  id: unknown;
}

/** Mock fetch: `handler` returns the result, or throws `{ code, message }` as a JSON-RPC error. */
function mockFetch(handler: (method: string, params: unknown[]) => unknown, calls: Recorded[] = []): typeof fetch {
  return (async (input: string | URL | Request, init?: RequestInit) => {
    const body = JSON.parse(String(init?.body));
    assert.equal(init?.method, 'POST');
    assert.equal(body.jsonrpc, '2.0');
    calls.push({ url: String(input), method: body.method, params: body.params, headers: init?.headers as Record<string, string>, id: body.id });
    let reply: unknown;
    try {
      reply = { jsonrpc: '2.0', id: body.id, result: handler(body.method, body.params) };
    } catch (e) {
      const error = e as { code?: number; message?: string };
      if (typeof error.code !== 'number') throw e;
      reply = { jsonrpc: '2.0', id: body.id, error: { code: error.code, message: error.message } };
    }
    return new Response(JSON.stringify(reply), { status: 200, headers: { 'content-type': 'application/json' } });
  }) as typeof fetch;
}

const blockRef = { height: '12', hash: H('ab') };
const receipt = {
  op_id: H('01'),
  success: true,
  error: null,
  gas_used: '1234',
  output: 'beef',
  events: [{ contract: H('02'), data: '0102' }],
};

test('ksana_submitOp: the only parameter is op (a VmOp or its Borsh encoding); returns op_id', async () => {
  const calls: Recorded[] = [];
  const client = new KsanaRpcClient('http://node:8545', {
    fetch: mockFetch(() => ({ op_id: H('cd') }), calls),
    headers: { authorization: 'Bearer x' },
  });
  const op = fromJson('VmOp', codec.valid.find((v: { type: string }) => v.type === 'VmOp').value);
  const result = await client.submitOp(op);
  assert.deepEqual(result.op_id, fromHex(H('cd')));
  assert.deepEqual(calls[0]!.params, [toHex(encodeVmOp(op))]);
  assert.equal(calls[0]!.method, 'ksana_submitOp');
  assert.equal(calls[0]!.url, 'http://node:8545');
  assert.equal(calls[0]!.headers['content-type'], 'application/json');
  assert.equal(calls[0]!.headers.authorization, 'Bearer x');
  await client.submitOp(encodeVmOp(op));
  assert.deepEqual(calls[1]!.params, [toHex(encodeVmOp(op))]);
  assert.notEqual(calls[0]!.id, calls[1]!.id, 'request IDs increase');
  // A second parameter (PSBT) passed per the old interface is not sent to the node (D-078)
  const untyped = client.submitOp.bind(client) as (...args: unknown[]) => Promise<unknown>;
  await untyped(encodeVmOp(op), Uint8Array.of(0x70, 0x73));
  assert.deepEqual(calls[2]!.params, [toHex(encodeVmOp(op))]);
});

test('fetch is not called with the client as this (the browser fetch requires this to be the global object, otherwise it throws Illegal invocation)', async () => {
  const inner = mockFetch(() => ({ op_id: H('cd') }));
  // The same check as the browser: this may only be undefined or the global object
  const browserLike = function (this: unknown, input: string | URL | Request, init?: RequestInit): Promise<Response> {
    if (this !== undefined && this !== globalThis) throw new TypeError("Failed to execute 'fetch' on 'Window': Illegal invocation");
    return inner(input, init);
  } as typeof fetch;
  const client = new KsanaRpcClient('http://node', { fetch: browserLike });
  const op = fromJson('VmOp', codec.valid.find((v: { type: string }) => v.type === 'VmOp').value);
  assert.deepEqual((await client.submitOp(op)).op_id, fromHex(H('cd')));
});

test('ksana_getOpStatus: parses status, block and receipt; an unknown operation is RpcError(-32004)', async () => {
  const client = new KsanaRpcClient('http://node', {
    fetch: mockFetch((method, params) => {
      assert.equal(method, 'ksana_getOpStatus');
      if (params[0] === H('00')) throw { code: -32004, message: 'unknown operation' };
      if (params[0] === H('11')) return { status: 'pending', block: null, index: null, receipt: null };
      return { status: 'fast_confirmed', block: blockRef, index: 3, receipt };
    }),
  });
  const status = await client.getOpStatus(H('aa'));
  assert.equal(status.status, 'fast_confirmed');
  const skipped = new KsanaRpcClient('http://node', {
    fetch: mockFetch(() => ({ status: 'skipped', block: blockRef, index: 0, receipt })),
  });
  assert.equal((await skipped.getOpStatus(H('aa'))).status, 'skipped');
  assert.deepEqual(status.block, { height: 12n, hash: fromHex(H('ab')) });
  assert.equal(status.index, 3);
  assert.equal(status.receipt?.gas_used, 1234n);
  assert.deepEqual(status.receipt?.events[0]?.data, Uint8Array.of(1, 2));
  assert.deepEqual(await client.getOpStatus(fromHex(H('11'))), { status: 'pending', block: null, index: null, receipt: null });
  await assert.rejects(client.getOpStatus(H('00')), (e: unknown) => e instanceof RpcError && e.code === RPC_ERROR_CODES.NOT_FOUND);
  await assert.rejects(client.getOpStatus('abcd'), /op_id.*32/);
});

test('receipts (D-122): the changes to built-in assets in order, each kind with its own fields; null from an older node', async () => {
  const [a, b, c, x] = [H('a1'), H('b2'), H('c3'), H('ee')];
  const changes = [
    { kind: 'transfer', asset: x, from: a, to: b, amount: '5', events: 0 },
    { kind: 'funds', asset: H('00'), from: a, to: c, amount: '7', events: 0 },
    { kind: 'payout', asset: x, from: c, to: b, amount: '2', events: 1 },
    { kind: 'create', asset: x, capabilities: ['mint', 'freeze', 'pause'], events: 1 },
    { kind: 'issue', asset: x, to: b, amount: '340282366920938463463374607431768211455', events: 1 },
    { kind: 'burn', asset: x, from: b, amount: '1', events: 2 },
    { kind: 'freeze', asset: x, account: a, frozen: true, events: 2 },
    { kind: 'allow', asset: x, account: b, allowed: false, events: 2 },
    { kind: 'pause', asset: x, paused: true, events: 3 },
    { kind: 'renounce', asset: x, capabilities: ['pause'], events: 3 },
  ];
  const call = (reply: unknown) =>
    new KsanaRpcClient('http://node', { fetch: mockFetch(() => reply) }).call({ contract: c, method: 'm', args: new Uint8Array(0) });
  const k = (hex: string) => fromHex(hex, 32);
  assert.deepEqual((await call({ ...receipt, asset_changes: changes })).asset_changes, [
    { kind: 'transfer', asset: k(x), from: k(a), to: k(b), amount: 5n, events: 0 },
    { kind: 'funds', asset: k(H('00')), from: k(a), to: k(c), amount: 7n, events: 0 },
    { kind: 'payout', asset: k(x), from: k(c), to: k(b), amount: 2n, events: 1 },
    { kind: 'create', asset: k(x), capabilities: ['mint', 'freeze', 'pause'], events: 1 },
    { kind: 'issue', asset: k(x), to: k(b), amount: 2n ** 128n - 1n, events: 1 },
    { kind: 'burn', asset: k(x), from: k(b), amount: 1n, events: 2 },
    { kind: 'freeze', asset: k(x), account: k(a), frozen: true, events: 2 },
    { kind: 'allow', asset: k(x), account: k(b), allowed: false, events: 2 },
    { kind: 'pause', asset: k(x), paused: true, events: 3 },
    { kind: 'renounce', asset: k(x), capabilities: ['pause'], events: 3 },
  ]);
  assert.deepEqual((await call({ ...receipt, asset_changes: [] })).asset_changes, []);
  // A node from before D-122 gives no such field, and a receipt it stored before gives null: both read as null
  assert.equal((await call(receipt)).asset_changes, null);
  assert.equal((await call({ ...receipt, asset_changes: null })).asset_changes, null);
  const refused: [unknown, RegExp][] = [
    [{ kind: 'seize', asset: x, from: a, amount: '1', events: 0 }, /asset_changes\[0\]\.kind must be one of transfer \/ funds/],
    [{ kind: 'freeze', asset: x, account: a, frozen: true, paused: true, events: 2 }, /asset_changes\[0\] has unknown field "paused"/],
    [{ kind: 'issue', asset: x, to: b, events: 1 }, /asset_changes\[0\]\.amount must be a decimal string/],
    [{ kind: 'burn', asset: x, from: b, amount: '01', events: 2 }, /asset_changes\[0\]\.amount/],
    [{ kind: 'create', asset: x, capabilities: ['pause', 'mint'], events: 1 }, /capabilities must list each name once, in order/],
    [{ kind: 'renounce', asset: x, capabilities: ['clawback'], events: 1 }, /capabilities\[0\] must be one of mint/],
    [{ kind: 'pause', asset: x, paused: true, events: -1 }, /asset_changes\[0\]\.events must be a non-negative integer/],
    [{ kind: 'pause', asset: H('0'), paused: true, events: 0 }, /asset_changes\[0\]\.asset/],
  ];
  for (const [change, pattern] of refused) {
    await assert.rejects(call({ ...receipt, asset_changes: [change] }), (e: unknown) => e instanceof RpcTransportError && pattern.test(e.message), String(pattern));
  }
  await assert.rejects(call({ ...receipt, asset_changes: {} }), /asset_changes must be an array/);
});

test('positions height:slot:entry (D-111): canonical decimal, u64 height, u32 slot and entry', () => {
  assert.deepEqual(parseOpPosition('81:0:2'), { height: 81n, slot: 0, entry: 2 });
  const max = '18446744073709551615:4294967295:4294967295';
  assert.deepEqual(parseOpPosition(max), { height: (1n << 64n) - 1n, slot: 0xffff_ffff, entry: 0xffff_ffff });
  assert.equal(formatOpPosition(parseOpPosition(max)), max);
  assert.equal(formatOpPosition({ height: 0n, slot: 0, entry: 0 }), '0:0:0');
  for (const bad of ['', '81', '81:0', '81:0:2:0', '081:0:2', '81:00:2', '-1:0:0', '81:0:2 ', '1e3:0:0', '18446744073709551616:0:0', '1:4294967296:0', '1:0:4294967296']) {
    assert.throws(() => parseOpPosition(bad), /position/, bad);
  }
  assert.throws(() => formatOpPosition({ height: -1n, slot: 0, entry: 0 }), /position/);
  assert.throws(() => formatOpPosition({ height: 1n, slot: 0.5, entry: 0 }), /position/);
});

test('ksana_getAccountOps and ksana_getLatestOps (D-111): parameters, paging, strict results; an older node has no such method', async () => {
  const calls: Recorded[] = [];
  const op = (id: string, position: string, status = 'fast_confirmed') => ({
    op_id: H(id),
    status,
    block: { height: position.split(':')[0], hash: H('b' + id[1]) },
    index: Number(position.split(':')[2]),
    position,
  });
  const page = { ops: [op('03', '9:1:0', 'btc_final'), op('02', '9:0:4', 'skipped')], next: '9:0:4', at: blockRef };
  const client = new KsanaRpcClient('http://node', { fetch: mockFetch(() => page, calls) });
  const listed = await client.getAccountOps(H('aa'));
  assert.deepEqual(calls[0]!.params, [H('aa'), null, null]);
  assert.equal(calls[0]!.method, 'ksana_getAccountOps');
  assert.equal(listed.ops.length, 2);
  assert.deepEqual(listed.ops[0], {
    op_id: fromHex(H('03')),
    status: 'btc_final',
    block: { height: 9n, hash: fromHex(H('b3')) },
    index: 0,
    position: { height: 9n, slot: 1, entry: 0 },
  });
  assert.equal(listed.ops[1]!.status, 'skipped');
  assert.deepEqual(listed.next, { height: 9n, slot: 0, entry: 4 });
  assert.deepEqual(listed.at, { height: 12n, hash: fromHex(H('ab')) });
  // The next page from `next`, as an object or a string; a limit
  await client.getAccountOps(fromHex(H('aa')), { before: listed.next, limit: 2 });
  assert.deepEqual(calls[1]!.params, [H('aa'), '9:0:4', 2]);
  await client.getAccountOps(H('aa'), { before: '81:0:2', limit: MAX_OPS_LIMIT });
  assert.deepEqual(calls[2]!.params, [H('aa'), '81:0:2', MAX_OPS_LIMIT]);
  await client.getLatestOps();
  assert.equal(calls[3]!.method, 'ksana_getLatestOps');
  assert.deepEqual(calls[3]!.params, [null, null]);
  await client.getLatestOps({ before: null, limit: 1 });
  assert.deepEqual(calls[4]!.params, [null, 1]);
  // Refused before anything is sent
  for (const options of [{ limit: 0 }, { limit: MAX_OPS_LIMIT + 1 }, { limit: 1.5 }, { before: '081:0:2' }, { before: '1:2' }]) {
    await assert.rejects(client.getLatestOps(options), /limit|position/);
  }
  await assert.rejects(client.getAccountOps('abcd'), /account.*32/);
  assert.equal(calls.length, 5);
  // The last page: next is null
  const last = new KsanaRpcClient('http://node', { fetch: mockFetch(() => ({ ops: [], next: null, at: blockRef })) });
  assert.deepEqual(await last.getLatestOps(), { ops: [], next: null, at: { height: 12n, hash: fromHex(H('ab')) } });
  // Malformed results
  for (const broken of [
    { ...page, extra: 1 },
    { ...page, next: 81 },
    { ...page, next: '81:0' },
    { ...page, ops: [{ ...op('03', '9:1:0'), status: 'pending' }] },
    { ...page, ops: [{ ...op('03', '9:1:0'), position: '09:1:0' }] },
    { ...page, ops: [{ ...op('03', '9:1:0'), receipt: null }] },
    { ops: page.ops, next: null },
  ]) {
    const bad = new KsanaRpcClient('http://node', { fetch: mockFetch(() => broken) });
    await assert.rejects(bad.getLatestOps(), RpcTransportError);
  }
  // A node older than D-111
  const old = new KsanaRpcClient('http://node', {
    fetch: mockFetch((method) => {
      throw { code: -32601, message: `Method not found: ${method}` };
    }),
  });
  await assert.rejects(old.getAccountOps(H('aa')), (e: unknown) => e instanceof RpcError && e.code === RPC_ERROR_CODES.METHOD_NOT_FOUND);
});

test('the standards index (D-120): parameters and cursors, the kinds of assets and approvals, strict results; an older node has no such methods', async () => {
  const calls: Recorded[] = [];
  const results: Record<string, unknown> = {
    ksana_getContract: {
      contract: H('c1'),
      code_hash: H('c0'),
      deployer: H('aa'),
      standards: ['KS-20', 'KS-20-burnable'],
      declared: '2:0:1',
      at: blockRef,
    },
    ksana_getAccountAssets: {
      assets: [
        { kind: 'asset', asset: H('00'), balance: '7' },
        { kind: 'token', contract: H('c1'), balance: '1000' },
        { kind: 'nft', contract: H('c2'), count: '2' },
      ],
      next: '01' + H('c2'),
      at: blockRef,
    },
    ksana_getToken: {
      contract: H('c1'),
      standard: 'KS-20',
      extensions: ['KS-20-burnable'],
      name: 'Ksana Coin',
      symbol: 'KCN',
      decimals: 8,
      supply: '1000',
      holders: '3',
      declared: '2:0:1',
      irregular: false,
      at: blockRef,
    },
    ksana_getTokenHolders: { holders: [{ account: H('aa'), balance: '900' }], next: '00'.repeat(16) + H('aa'), at: blockRef },
    ksana_getNfts: { nfts: [{ contract: H('c2'), token_id: H('01') }], next: null, at: blockRef },
    ksana_getApprovals: {
      approvals: [
        { kind: 'allowance', contract: H('c1'), spender: H('bb'), amount: '50', expires_at: '1900000000' },
        { kind: 'operator', contract: H('c2'), operator: H('bb'), expires_at: null },
        { kind: 'token', contract: H('c2'), token_id: H('04'), spender: H('bb') },
      ],
      next: null,
      at: blockRef,
    },
  };
  const client = new KsanaRpcClient('http://node', { fetch: mockFetch((method) => results[method], calls) });
  const at = { height: 12n, hash: fromHex(H('ab')) };

  assert.deepEqual(await client.getContract(H('c1')), {
    contract: fromHex(H('c1')),
    code_hash: fromHex(H('c0')),
    deployer: fromHex(H('aa')),
    standards: ['KS-20', 'KS-20-burnable'],
    declared: { height: 2n, slot: 0, entry: 1 },
    at,
  });
  assert.deepEqual(calls.at(-1)!.params, [H('c1')]);

  const assets = await client.getAccountAssets(fromHex(H('aa')));
  assert.deepEqual(calls.at(-1)!.params, [H('aa'), null, null]);
  assert.deepEqual(assets.assets, [
    { kind: 'asset', asset: fromHex(H('00')), balance: 7n },
    { kind: 'token', contract: fromHex(H('c1')), balance: 1000n },
    { kind: 'nft', contract: fromHex(H('c2')), count: 2n },
  ]);
  assert.equal(assets.next, '01' + H('c2'));
  await client.getAccountAssets(H('aa'), { after: assets.next, limit: 2 });
  assert.deepEqual(calls.at(-1)!.params, [H('aa'), '01' + H('c2'), 2]);

  const token = await client.getToken(H('c1'));
  assert.equal(token.standard, 'KS-20');
  assert.deepEqual(token.extensions, ['KS-20-burnable']);
  assert.equal(token.decimals, 8);
  assert.equal(token.supply, 1000n);
  assert.equal(token.holders, 3n);
  assert.deepEqual(token.declared, { height: 2n, slot: 0, entry: 1 });
  assert.equal(token.irregular, false);

  const holders = await client.getTokenHolders(H('c1'), { after: 'AB'.repeat(48) });
  assert.deepEqual(calls.at(-1)!.params, [H('c1'), 'ab'.repeat(48), null]);
  assert.deepEqual(holders.holders, [{ account: fromHex(H('aa')), balance: 900n }]);
  assert.equal(holders.next, '00'.repeat(16) + H('aa'));

  const nfts = await client.getNfts(H('aa'), { contract: fromHex(H('c2')), after: H('c2') + H('01'), limit: 1 });
  assert.deepEqual(calls.at(-1)!.params, [H('aa'), H('c2'), H('c2') + H('01'), 1]);
  assert.deepEqual(nfts, { nfts: [{ contract: fromHex(H('c2')), token_id: fromHex(H('01')) }], next: null, at });
  await client.getNfts(H('aa'));
  assert.deepEqual(calls.at(-1)!.params, [H('aa'), null, null, null]);

  const approvals = await client.getApprovals(H('aa'), { limit: MAX_OPS_LIMIT });
  assert.deepEqual(calls.at(-1)!.params, [H('aa'), null, MAX_OPS_LIMIT]);
  assert.deepEqual(approvals.approvals, [
    { kind: 'allowance', contract: fromHex(H('c1')), spender: fromHex(H('bb')), amount: 50n, expires_at: 1_900_000_000n },
    { kind: 'operator', contract: fromHex(H('c2')), operator: fromHex(H('bb')), expires_at: null },
    { kind: 'token', contract: fromHex(H('c2')), token_id: fromHex(H('04')), spender: fromHex(H('bb')) },
  ]);

  // Refused before anything is sent
  const sent = calls.length;
  await assert.rejects(client.getAccountAssets(H('aa'), { limit: 0 }), /limit/);
  await assert.rejects(client.getAccountAssets(H('aa'), { after: '01' + H('c2').slice(2) }), /after.*33 bytes/);
  await assert.rejects(client.getTokenHolders(H('c1'), { after: 'zz'.repeat(48) }), /after.*48 bytes/);
  await assert.rejects(client.getNfts(H('aa'), { contract: H('c2'), after: H('c1') + H('01') }), /after must name the contract/);
  await assert.rejects(client.getApprovals(H('aa'), { after: H('c1') }), /after.*65 bytes/);
  await assert.rejects(client.getToken('abcd'), /contract.*32/);
  assert.equal(calls.length, sent);

  // Malformed results
  const assetsOf = (c: KsanaRpcClient) => c.getAccountAssets(H('aa'));
  const tokenOf = (c: KsanaRpcClient) => c.getToken(H('c1'));
  const approvalsOf = (c: KsanaRpcClient) => c.getApprovals(H('aa'));
  const page = (items: Record<string, unknown>, next: string | null = null) => ({ ...items, next, at: blockRef });
  const broken: [string, unknown, (c: KsanaRpcClient) => Promise<unknown>][] = [
    ['ksana_getAccountAssets', page({ assets: [{ kind: 'asset', asset: H('00'), balance: '7', count: '1' }] }), assetsOf],
    ['ksana_getAccountAssets', page({ assets: [{ kind: 'coin', asset: H('00'), balance: '7' }] }), assetsOf],
    ['ksana_getAccountAssets', page({ assets: [] }, H('c2')), assetsOf],
    ['ksana_getToken', { ...(results.ksana_getToken as object), standard: 'KS-1155' }, tokenOf],
    ['ksana_getToken', { ...(results.ksana_getToken as object), decimals: 256 }, tokenOf],
    ['ksana_getToken', { ...(results.ksana_getToken as object), declared: null }, tokenOf],
    ['ksana_getContract', { ...(results.ksana_getContract as object), standards: [1] }, (c) => c.getContract(H('c1'))],
    ['ksana_getTokenHolders', page({ holders: [{ account: H('aa'), balance: 900 }] }), (c) => c.getTokenHolders(H('c1'))],
    ['ksana_getNfts', page({ nfts: [] }, H('c2')), (c) => c.getNfts(H('aa'))],
    ['ksana_getApprovals', page({ approvals: [{ kind: 'operator', contract: H('c2'), operator: H('bb'), amount: '1', expires_at: null }] }), approvalsOf],
    ['ksana_getApprovals', page({ approvals: [{ kind: 'token', contract: H('c2'), token_id: H('04') }] }), approvalsOf],
  ];
  for (const [method, result, run] of broken) {
    const bad = new KsanaRpcClient('http://node', { fetch: mockFetch((m) => (m === method ? result : results[m])) });
    await assert.rejects(run(bad), RpcTransportError, method);
  }

  // A node older than D-120, or whose index is not built
  const old = new KsanaRpcClient('http://node', {
    fetch: mockFetch((method) => {
      throw { code: -32601, message: `Method not found: ${method}` };
    }),
  });
  for (const run of [(c: KsanaRpcClient) => c.getAccountAssets(H('aa')), (c: KsanaRpcClient) => c.getToken(H('c1'))]) {
    await assert.rejects(run(old), (e: unknown) => e instanceof RpcError && e.code === RPC_ERROR_CODES.METHOD_NOT_FOUND);
  }
});

test('ksana_getBlock: block header and entries are parsed strictly per D-007', async () => {
  const block = codec.valid.find((v: { type: string; name: string }) => v.type === 'Block' && v.name.includes('all entry types'));
  const calls: Recorded[] = [];
  const client = new KsanaRpcClient('http://node', {
    fetch: mockFetch(
      () => ({
        hash: H('ee'),
        ...block.value,
        receipts: block.value.entries.map(() => receipt),
        accepted_height: '7',
        merged: [H('a1'), H('a2')],
      }),
      calls,
    ),
  });
  const info = await client.getBlock(7n);
  assert.deepEqual(calls[0]!.params, ['7']);
  assert.equal(info.accepted_height, 7n);
  assert.deepEqual(info.merged, [fromHex(H('a1')), fromHex(H('a2'))]);
  assert.deepEqual(info.header, fromJson('BlockHeader', block.value.header));
  assert.equal(info.entries.length, 4);
  assert.deepEqual(toJson('Entry', info.entries[1]!), block.value.entries[1]);
  assert.deepEqual(info.settlement, {
    commit: fromHex(block.value.settlement.commit),
    reveal: fromHex(block.value.settlement.reveal),
  });
  assert.equal(info.receipts.length, 4);
  await client.getBlock();
  assert.deepEqual(calls[1]!.params, ['latest']);
  // An unknown field in the entries: rejected
  const bad = new KsanaRpcClient('http://node', {
    fetch: mockFetch(() => ({
      hash: H('ee'),
      ...block.value,
      header: { ...block.value.header, extra: 1 },
      receipts: [],
      accepted_height: null,
      merged: [],
    })),
  });
  await assert.rejects(bad.getBlock(), RpcTransportError);
});

const accountReply = {
  account: H('33'),
  nonce: '7',
  balances: [{ asset: H('00'), amount: '340282366920938463463374607431768211455' }],
  locked_fees: [{ block_hash: H('44'), amount: '5' }],
  at: blockRef,
};

test('ksana_getAccount and ksana_call', async () => {
  const calls: Recorded[] = [];
  const client = new KsanaRpcClient('http://node', {
    fetch: mockFetch((method) => (method === 'ksana_getAccount' ? accountReply : receipt), calls),
  });
  const account = await client.getAccount(fromHex(H('33')));
  assert.deepEqual(account, {
    account: fromHex(H('33')),
    nonce: 7n,
    balances: [{ asset: new Uint8Array(32), amount: (1n << 128n) - 1n }],
    locked_fees: [{ block_hash: fromHex(H('44')), amount: 5n }],
    at: { height: 12n, hash: fromHex(H('ab')) },
  });
  assert.deepEqual(calls[0]!.params, [H('33')]);
  const result = await client.call({ contract: fromHex(H('55')), method: 'balance_of', args: Uint8Array.of(9), max_gas: 1000n });
  assert.equal(result.success, true);
  assert.deepEqual(calls[1]!.params, [{ contract: H('55'), method: 'balance_of', args: '09', max_gas: '1000' }]);
  await client.call({ contract: H('55'), method: 'm', args: new Uint8Array(0), caller: fromHex(H('66')) });
  assert.deepEqual(calls[2]!.params, [{ contract: H('55'), method: 'm', args: '', caller: H('66') }]);
});

test('ksana_getAccount: rejects unknown fields such as the removed fee-debt and refusal-list fields (D-078)', async () => {
  for (const [broken, field] of [
    [{ ...accountReply, fee_debt: '0' }, 'fee_debt'],
    [{ ...accountReply, path_a_refused: false }, 'path_a_refused'],
    [{ ...accountReply, balances: [{ ...accountReply.balances[0]!, locked: '1' }] }, 'locked'],
    [{ ...accountReply, at: { ...blockRef, extra: 1 } }, 'extra'],
  ] as const) {
    const client = new KsanaRpcClient('http://node', { fetch: mockFetch(() => broken) });
    await assert.rejects(client.getAccount(H('33')), (e: unknown) => {
      return e instanceof RpcTransportError && e.message.includes(`unknown field "${field}"`);
    });
  }
});

test('ksana_getAsset', async () => {
  const calls: Recorded[] = [];
  const asset = {
    asset: H('00'),
    name: 'KSA',
    symbol: 'KSA',
    decimals: 8,
    total_supply: '100000000990000000',
    issuer: H('00'),
    capabilities: [],
    paused: false,
    at: blockRef,
  };
  const client = new KsanaRpcClient('http://node', {
    fetch: mockFetch((method) => {
      assert.equal(method, 'ksana_getAsset');
      return asset;
    }, calls),
  });
  const info = await client.getAsset(new Uint8Array(32));
  assert.equal(info.name, 'KSA');
  assert.equal(info.symbol, 'KSA');
  assert.equal(info.decimals, 8);
  assert.equal(info.total_supply, 100_000_000_990_000_000n);
  assert.deepEqual(info.issuer, new Uint8Array(32));
  assert.deepEqual(info.capabilities, []);
  assert.equal(info.paused, false);
  assert.deepEqual(calls[0]!.params, [H('00')]);
  // an asset whose issuing contract holds capabilities (D-121)
  const regulated = { ...asset, name: 'Dollar', symbol: 'USDX', capabilities: ['mint', 'freeze', 'pause'], paused: true };
  const held = await new KsanaRpcClient('http://node', { fetch: mockFetch(() => regulated) }).getAsset(H('01'));
  assert.deepEqual(held.capabilities, ['mint', 'freeze', 'pause']);
  assert.equal(held.paused, true);
  for (const broken of [
    { ...asset, decimals: 256 },
    { ...asset, total_supply: 5 },
    { ...asset, name: 1 },
    { ...asset, symbol: null },
    { ...asset, capabilities: ['clawback'] },
    { ...asset, capabilities: ['pause', 'mint'] },
    { ...asset, capabilities: ['mint', 'mint'] },
    { ...asset, capabilities: 'mint' },
    { ...asset, paused: 0 },
  ]) {
    const bad = new KsanaRpcClient('http://node', { fetch: mockFetch(() => broken) });
    await assert.rejects(bad.getAsset(H('00')), RpcTransportError, JSON.stringify(broken));
  }
});

test('ksana_getAssetStatus (D-121)', async () => {
  const calls: Recorded[] = [];
  const reply = {
    asset: H('01'),
    account: H('33'),
    status: ['frozen', 'allowed'],
    can_send: false,
    can_receive: false,
    at: blockRef,
  };
  const client = new KsanaRpcClient('http://node', {
    fetch: mockFetch((method) => {
      assert.equal(method, 'ksana_getAssetStatus');
      return reply;
    }, calls),
  });
  const standing = await client.getAssetStatus(H('01'), H('33'));
  assert.deepEqual(standing.status, ['frozen', 'allowed']);
  assert.equal(standing.can_send, false);
  assert.equal(standing.can_receive, false);
  assert.deepEqual(standing.account, fromHex(H('33')));
  assert.deepEqual(calls[0]!.params, [H('01'), H('33')]);
  for (const broken of [
    { ...reply, status: ['allowed', 'frozen'] },
    { ...reply, status: ['banned'] },
    { ...reply, can_send: 'no' },
    { ...reply, can_receive: undefined },
    { ...reply, extra: 1 },
  ]) {
    const bad = new KsanaRpcClient('http://node', { fetch: mockFetch(() => broken) });
    await assert.rejects(bad.getAssetStatus(H('01'), H('33')), RpcTransportError, JSON.stringify(broken));
  }
  await assert.rejects(client.getAssetStatus(H('01'), H('3333')), /account must be 32 bytes/);
});

test('ksana_getChainInfo (D-076)', async () => {
  const calls: Recorded[] = [];
  const reply = {
    chain_id: 1001,
    tip: blockRef,
    daa_score: '120',
    protocol_version: 2,
    next_upgrade: { version: 3, daa_score: '200' },
    max_known_version: 3,
    finalized: { height: '7', hash: H('07') },
  };
  const client = new KsanaRpcClient('http://node', {
    fetch: mockFetch((method) => {
      assert.equal(method, 'ksana_getChainInfo');
      return reply;
    }, calls),
  });
  const info = await client.getChainInfo();
  assert.equal(info.daa_score, 120n);
  assert.equal(info.protocol_version, 2);
  assert.deepEqual(info.next_upgrade, { version: 3, daa_score: 200n });
  assert.equal(info.max_known_version, 3);
  assert.deepEqual(info.finalized, { height: 7n, hash: fromHex(H('07')) });
  assert.deepEqual(calls[0]!.params, []);
  const none = new KsanaRpcClient('http://node', { fetch: mockFetch(() => ({ ...reply, next_upgrade: null })) });
  assert.equal((await none.getChainInfo()).next_upgrade, null);
  for (const broken of [{ ...reply, daa_score: 120 }, { ...reply, next_upgrade: { version: 3 } }, { ...reply, protocol_version: '2' }]) {
    const bad = new KsanaRpcClient('http://node', { fetch: mockFetch(() => broken) });
    await assert.rejects(bad.getChainInfo(), RpcTransportError);
  }
});

test('ksana_getSettlement', async () => {
  const client = new KsanaRpcClient('http://node', {
    fetch: mockFetch(() => ({
      block: blockRef,
      txid: H('77'),
      txid_rpc: H('77'),
      commit_txid: H('76'),
      commit_txid_rpc: H('76'),
      txs: { commit: '0200', reveal: '0201' },
      carrier: null,
      status: 'confirmed',
      confirmed: { txid: H('78'), height: 105, block: H('79'), confirmations: 3 },
      settled: true,
      btc_final: false,
    })),
  });
  const s = await client.getSettlement(H('ab'));
  assert.equal(s.status, 'confirmed');
  assert.equal(s.confirmed?.confirmations, 3);
  // Commit and reveal transactions (D-081); txid is the reveal transaction carrying the payload
  assert.deepEqual(s.txs, { commit: Uint8Array.of(2, 0), reveal: Uint8Array.of(2, 1) });
  assert.deepEqual(s.commit_txid, fromHex(H('76')));
  assert.equal(s.carrier, null);
  assert.equal(s.settled, true);
});

test('ksana_getSettlement: a block without a settlement transaction (D-077)', async () => {
  const pending = {
    block: blockRef,
    txid: null,
    txid_rpc: null,
    commit_txid: null,
    commit_txid_rpc: null,
    txs: null,
    carrier: null,
    status: 'pending',
    confirmed: null,
    settled: false,
    btc_final: false,
  };
  const carried = {
    ...pending,
    txid: H('77'),
    txid_rpc: H('77'),
    commit_txid: H('76'),
    commit_txid_rpc: H('76'),
    txs: { commit: '0200', reveal: '0201' },
    carrier: { ...blockRef, height: '8' },
  };
  const answers = [pending, carried];
  const client = new KsanaRpcClient('http://node', { fetch: mockFetch(() => answers.shift()) });
  const before = await client.getSettlement(H('ab'));
  assert.equal(before.txid, null);
  assert.equal(before.txs, null);
  assert.equal(before.carrier, null);
  const after = await client.getSettlement(H('ab'));
  assert.deepEqual(after.txs?.reveal, Uint8Array.of(2, 1));
  assert.equal(after.carrier?.height, 8n);
});

test('result objects always reject unknown fields (like deny_unknown_fields on the node result types)', async () => {
  const block = codec.valid.find((v: { type: string; name: string }) => v.type === 'Block' && v.name.includes('all entry types'));
  const blockReply = { hash: H('ee'), ...block.value, receipts: [], accepted_height: null, merged: [] };
  const settlement = {
    block: blockRef,
    txid: H('77'),
    txid_rpc: H('77'),
    commit_txid: H('76'),
    commit_txid_rpc: H('76'),
    txs: { commit: '0200', reveal: '0201' },
    carrier: null,
    status: 'confirmed',
    confirmed: { txid: H('78'), height: 105, block: H('79'), confirmations: 3 },
    settled: true,
    btc_final: false,
  };
  const chainInfo = {
    chain_id: 1001,
    tip: blockRef,
    daa_score: '120',
    protocol_version: 2,
    next_upgrade: { version: 3, daa_score: '200' },
    max_known_version: 3,
    finalized: blockRef,
  };
  const asset = {
    asset: H('00'),
    name: 'KSA',
    symbol: 'KSA',
    decimals: 8,
    total_supply: '1',
    issuer: H('00'),
    capabilities: [],
    paused: false,
    at: blockRef,
  };
  const included = { status: 'fast_confirmed', block: blockRef, index: 0, receipt };
  const cases: [string, (c: KsanaRpcClient) => Promise<unknown>, unknown][] = [
    ['submitOp', (c) => c.submitOp(Uint8Array.of(1)), { op_id: H('cd'), extra: 1 }],
    ['getOpStatus', (c) => c.getOpStatus(H('aa')), { ...included, extra: 1 }],
    ['getOpStatus block', (c) => c.getOpStatus(H('aa')), { ...included, block: { ...blockRef, extra: 1 } }],
    ['getOpStatus receipt', (c) => c.getOpStatus(H('aa')), { ...included, receipt: { ...receipt, extra: 1 } }],
    ['getOpStatus event', (c) => c.getOpStatus(H('aa')), { ...included, receipt: { ...receipt, events: [{ contract: H('02'), data: '', extra: 1 }] } }],
    ['getBlock', (c) => c.getBlock(), { ...blockReply, extra: 1 }],
    ['getAsset', (c) => c.getAsset(H('00')), { ...asset, extra: 1 }],
    ['getChainInfo', (c) => c.getChainInfo(), { ...chainInfo, extra: 1 }],
    ['getChainInfo upgrade', (c) => c.getChainInfo(), { ...chainInfo, next_upgrade: { version: 3, daa_score: '200', extra: 1 } }],
    ['getSettlement', (c) => c.getSettlement(), { ...settlement, extra: 1 }],
    ['getSettlement confirmed', (c) => c.getSettlement(), { ...settlement, confirmed: { ...settlement.confirmed, extra: 1 } }],
    ['getSettlement transactions', (c) => c.getSettlement(), { ...settlement, txs: { ...settlement.txs, extra: 1 } }],
    ['getBlock settlement', (c) => c.getBlock(), { ...blockReply, settlement: { ...block.value.settlement, extra: 1 } }],
    ['call', (c) => c.call({ contract: H('55'), method: 'm', args: new Uint8Array(0) }), { ...receipt, extra: 1 }],
  ];
  for (const [name, call, reply] of cases) {
    // Parses once the extra field is removed
    const { extra: _extra, ...valid } = reply as { extra?: unknown };
    if (_extra !== undefined) await call(new KsanaRpcClient('http://node', { fetch: mockFetch(() => valid) }));
    const client = new KsanaRpcClient('http://node', { fetch: mockFetch(() => reply) });
    await assert.rejects(call(client), (e: unknown) => e instanceof RpcTransportError && e.message.includes('has unknown field "extra"'), name);
  }
  const indexer = new IndexerRpcClient('http://indexer', {
    fetch: mockFetch((method) =>
      method === 'indexer_status'
        ? { height: '1', hash: H('01'), state_root: H('02'), btc_height: 7, finalized: blockRef, halted: null, extra: 1 }
        : { height: '1', hash: H('01'), state_root: H('02'), extra: 1 },
    ),
  });
  await assert.rejects(indexer.status(), /indexer status has unknown field "extra"/);
  await assert.rejects(indexer.stateRoot(1), /state root has unknown field "extra"/);
});

test('ksana_estimateFee (D-054): no parameters; the result is the settlement fee rate and the suggested path B token amount', async () => {
  const calls: Recorded[] = [];
  const result = { feerate_sat_vb: '2', path_b: { amount: '1000', per_kib: '40' }, at: blockRef };
  const client = new KsanaRpcClient('http://node', { fetch: mockFetch(() => result, calls) });
  const fee = await client.estimateFee();
  assert.deepEqual(calls[0]!.params, []);
  assert.equal(calls[0]!.method, 'ksana_estimateFee');
  assert.deepEqual(fee, { feerate_sat_vb: 2n, path_b: { amount: 1000n, per_kib: 40n }, at: { height: 12n, hash: fromHex(H('ab')) } });
  // Nodes before M23 give no fee rate: treated as 0
  const older = new KsanaRpcClient('http://node', { fetch: mockFetch(() => ({ ...result, path_b: { amount: '1000' } })) });
  assert.deepEqual((await older.estimateFee()).path_b, { amount: 1000n, per_kib: 0n });
  // A request object per the old interface (removed in D-078) is not sent to the node: the node accepts no parameters
  const untyped = client.estimateFee.bind(client) as (...args: unknown[]) => Promise<unknown>;
  await untyped({ max_gas: 50_000n, op_bytes: 230, funding: 'p2wpkh' });
  assert.deepEqual(calls[1]!.params, []);
  // u64 / u128 fields must be decimal strings (D-007)
  const numeric = new KsanaRpcClient('http://node', { fetch: mockFetch(() => ({ ...result, feerate_sat_vb: 3 })) });
  await assert.rejects(numeric.estimateFee(), RpcTransportError);
  const missing = new KsanaRpcClient('http://node', { fetch: mockFetch(() => ({ ...result, feerate_sat_vb: undefined })) });
  await assert.rejects(missing.estimateFee(), /feerate_sat_vb/);
  // Fields of old node results that only served path A (removed in D-078) are unknown fields
  for (const [field, value] of [
    ['path_a', { vbytes: '181', contribution_sat: '1362' }],
    ['min_gas_price_msat', '1'],
    ['max_gas', '50000'],
  ] as const) {
    const old = new KsanaRpcClient('http://node', { fetch: mockFetch(() => ({ ...result, [field]: value })) });
    await assert.rejects(old.estimateFee(), (e: unknown) => e instanceof RpcTransportError && e.message.includes(`unknown field "${field}"`), field);
  }
  const nested = new KsanaRpcClient('http://node', { fetch: mockFetch(() => ({ ...result, path_b: { amount: '1', extra: '2' } })) });
  await assert.rejects(nested.estimateFee(), /path_b has unknown field "extra"/);
});

test('indexer: indexer_status and indexer_stateRoot', async () => {
  const calls: Recorded[] = [];
  const client = new IndexerRpcClient('http://indexer:8546', {
    fetch: mockFetch((method) => {
      if (method === 'indexer_status') return { height: '10', hash: H('01'), state_root: H('02'), btc_height: null, finalized: { height: '3', hash: H('05') }, halted: null };
      return { height: '9', hash: H('03'), state_root: H('04') };
    }, calls),
  });
  const status = await client.status();
  assert.deepEqual(status, {
    height: 10n,
    hash: fromHex(H('01')),
    state_root: fromHex(H('02')),
    btc_height: null,
    finalized: { height: 3n, hash: fromHex(H('05')) },
    halted: null,
  });
  const root = await client.stateRoot(9);
  assert.equal(root.height, 9n);
  assert.deepEqual(calls.map((c) => [c.method, c.params]), [
    ['indexer_status', []],
    ['indexer_stateRoot', ['9']],
  ]);
});

test('block identifier', () => {
  assert.equal(blockIdParam('latest'), 'latest');
  assert.equal(blockIdParam(0), '0');
  assert.equal(blockIdParam(5n), '5');
  assert.equal(blockIdParam('42'), '42');
  assert.equal(blockIdParam(fromHex(H('ab'))), H('ab'));
  assert.equal(blockIdParam(H('AB')), H('ab'));
  for (const bad of ['05', '-1', 'abc', '', H('ab').slice(2)]) assert.throws(() => blockIdParam(bad), Error, bad);
  assert.throws(() => blockIdParam(-1), RangeError);
  assert.throws(() => blockIdParam(new Uint8Array(31)), /32 bytes/);
});

test('transport errors: HTTP error, non-JSON, failed request, malformed result, timeout', async () => {
  const http500 = new KsanaRpcClient('http://node', {
    fetch: (async () => new Response('Internal Server Error', { status: 500 })) as typeof fetch,
  });
  await assert.rejects(http500.getBlock(), (e: unknown) => e instanceof RpcTransportError && e.status === 500);
  const down = new KsanaRpcClient('http://node', {
    fetch: (async () => {
      throw new TypeError('fetch failed');
    }) as typeof fetch,
  });
  await assert.rejects(down.getBlock(), /request failed/);
  const malformed = new KsanaRpcClient('http://node', {
    fetch: mockFetch(() => ({ account: H('33'), nonce: 7.5, balances: [], locked_fees: [], at: blockRef })),
  });
  await assert.rejects(malformed.getAccount(H('33')), RpcTransportError);
  const noResult = new KsanaRpcClient('http://node', {
    fetch: (async () => new Response(JSON.stringify({ jsonrpc: '2.0', id: 1 }), { status: 200 })) as typeof fetch,
  });
  await assert.rejects(noResult.getBlock(), /missing result/);
  const slow = new KsanaRpcClient('http://node', {
    timeoutMs: 20,
    fetch: ((_input: unknown, init?: RequestInit) =>
      new Promise((_, reject) => {
        init?.signal?.addEventListener('abort', () => reject(new Error('aborted')));
      })) as typeof fetch,
  });
  await assert.rejects(slow.getBlock(), /request failed: aborted/);
  // JSON-RPC error: carries the error code and the method name
  const invalid = new KsanaRpcClient('http://node', {
    fetch: mockFetch(() => {
      throw { code: -32010, message: 'nonce not consecutive' };
    }),
  });
  await assert.rejects(invalid.submitOp(Uint8Array.of(1)), (e: unknown) => {
    return e instanceof RpcError && e.code === RPC_ERROR_CODES.OP_INVALID && e.method === 'ksana_submitOp' && /nonce not consecutive/.test(e.message);
  });
});

test('a request timeout covers a stalled response body without restarting at the headers', async (t) => {
  t.mock.timers.enable({ apis: ['setTimeout'] });
  let signal: AbortSignal | null | undefined;
  let body: ReadableStreamDefaultController<Uint8Array> | undefined;
  let reading!: () => void;
  const readingBody = new Promise<void>((resolve) => { reading = resolve; });
  const client = new JsonRpcClient('http://node', {
    timeoutMs: 100,
    fetch: (async (_input, init) => {
      signal = init?.signal;
      await new Promise<void>((resolve) => setTimeout(resolve, 40));
      const response = new Response(new ReadableStream<Uint8Array>({
        start(controller) {
          body = controller;
          controller.enqueue(new TextEncoder().encode('{"jsonrpc":"2.0","id":1,"result":'));
          signal?.addEventListener('abort', () => controller.error(signal?.reason), { once: true });
        },
      }), { status: 200 });
      const json = response.json.bind(response);
      response.json = () => { reading(); return json(); };
      return response;
    }) as typeof fetch,
  });
  const outcome = client.request('test_method').catch((error: unknown) => error);
  // Also settle the body on a failed assertion, so the regression fails without leaving a pending request.
  t.after(async () => { body?.error(new Error('test cleanup')); await outcome; });
  t.mock.timers.tick(40);
  await readingBody;
  assert.ok(signal);
  t.mock.timers.tick(59);
  assert.equal(signal.aborted, false);
  t.mock.timers.tick(1);
  assert.equal(signal.aborted, true, 'the original 100 ms deadline still applies after HTTP headers');
  const error = await outcome;
  assert.ok(error instanceof RpcTransportError);
  assert.equal(error.status, undefined, 'a timed-out request carries no status, as one that timed out before its headers');
  assert.match(error.message, /test_method: request failed:/);
  assert.doesNotMatch(error.message, /response is not JSON/);
});

test('request timers are cleared after success, malformed JSON, RPC errors and fetch errors', async (t) => {
  t.mock.timers.enable({ apis: ['setTimeout'] });
  const cases = [
    { body: '{"jsonrpc":"2.0","id":1,"result":"ok"}', error: undefined },
    { body: 'not JSON', error: /HTTP 200, response is not JSON/ },
    { body: '{"jsonrpc":"2.0","id":1,"error":{"code":-32601,"message":"not found"}}', error: RpcError },
    { body: undefined, error: /request failed: offline/ },
  ];
  for (const entry of cases) {
    let signal: AbortSignal | null | undefined;
    const client = new JsonRpcClient('http://node', {
      timeoutMs: 100,
      fetch: (async (_input, init) => {
        signal = init?.signal;
        if (entry.body === undefined) throw new Error('offline');
        return new Response(entry.body, { status: 200 });
      }) as typeof fetch,
    });
    if (entry.error) await assert.rejects(client.request('test_method'), entry.error);
    else assert.equal(await client.request('test_method'), 'ok');
    assert.ok(signal);
    t.mock.timers.tick(100);
    assert.equal(signal.aborted, false, 'a completed request must not leave its timeout running');
  }
});

test('a request without timeoutMs can finish reading a delayed body', async (t) => {
  t.mock.timers.enable({ apis: ['setTimeout'] });
  let body!: ReadableStreamDefaultController<Uint8Array>;
  const client = new JsonRpcClient('http://node', {
    fetch: (async (_input, init) => {
      assert.equal(init?.signal, undefined);
      return new Response(new ReadableStream<Uint8Array>({ start(controller) { body = controller; } }));
    }) as typeof fetch,
  });
  const result = client.request('test_method');
  await Promise.resolve();
  t.mock.timers.tick(60_000);
  body.enqueue(new TextEncoder().encode('{"jsonrpc":"2.0","id":1,"result":"ok"}'));
  body.close();
  assert.equal(await result, 'ok');
});

class FakeSocket implements WebSocketLike {
  static last: FakeSocket | undefined;
  sent: string[] = [];
  closed = false;
  onopen: ((ev: unknown) => void) | null = null;
  onmessage: ((ev: { data: unknown }) => void) | null = null;
  onerror: ((ev: unknown) => void) | null = null;
  onclose: ((ev: unknown) => void) | null = null;
  constructor(readonly url: string) {
    FakeSocket.last = this;
  }
  send(data: string): void {
    this.sent.push(data);
  }
  close(): void {
    this.closed = true;
  }
  receive(message: unknown): void {
    this.onmessage?.({ data: JSON.stringify(message) });
  }
}

test('ksana_subscribeBlocks: WebSocket subscription to new blocks', () => {
  const blocks: bigint[] = [];
  const errors: Error[] = [];
  const sub = subscribeBlocks('ws://node:8545', (b) => blocks.push(b.height), { WebSocket: FakeSocket, onError: (e) => errors.push(e) });
  const socket = FakeSocket.last!;
  assert.equal(socket.url, 'ws://node:8545');
  socket.onopen?.({});
  assert.deepEqual(JSON.parse(socket.sent[0]!), { jsonrpc: '2.0', id: 1, method: 'ksana_subscribeBlocks', params: [] });
  const summary = (height: string) => ({ height, hash: H('ab'), entries: 2, timestamp: '1790000000' });
  // Notifications before the subscription is confirmed are ignored
  socket.receive({ jsonrpc: '2.0', method: 'ksana_block', params: { subscription: 'sub-1', result: summary('4') } });
  socket.receive({ jsonrpc: '2.0', id: 1, result: 'sub-1' });
  socket.receive({ jsonrpc: '2.0', method: 'ksana_block', params: { subscription: 'sub-1', result: summary('5') } });
  socket.receive({ jsonrpc: '2.0', method: 'ksana_block', params: { subscription: 'other', result: summary('6') } });
  socket.receive({ jsonrpc: '2.0', method: 'ksana_block', params: { subscription: 'sub-1', result: { height: 'x' } } });
  socket.receive({ jsonrpc: '2.0', method: 'ksana_block', params: { subscription: 'sub-1', result: { ...summary('7'), extra: 1 } } });
  assert.deepEqual(blocks, [5n]);
  assert.equal(errors.length, 2);
  assert.match(errors[1]!.message, /has unknown field "extra"/);
  sub.close();
  assert.ok(socket.closed);
});
