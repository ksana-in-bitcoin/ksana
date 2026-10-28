import { readFileSync } from 'node:fs';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { DEVNET, SIGNET, RPC_ERROR_CODES, RpcError, KsanaRpcClient, UnisatAdapter, decodeVmOp, feeFor, opCost, opId, sha256, toHex, fromHex,
  type Action, type VmOp } from '@ksana/sdk';
import { MockUnisatWallet } from '@ksana/sdk/wallet/mock';
import { Journal, KSA, PlaygroundError, amountsNeeded, callGas, checkFunds, contractIdOf, deployGas, endpointKey, prepare, releaseNonce,
  shouldPoll, sortFunds, submit, validateWasm, withWriteLock, type Context, type Entry } from './ops';

const wasm = new Uint8Array(readFileSync(new URL('../examples/wasm/playground-counter.wasm', import.meta.url)));
const deploy = (): Action => ({ kind: 'Deploy', code: wasm.slice(), init_args: new Uint8Array(), funds: [] });
const TOKEN = new Uint8Array(32).fill(7);
const GENESIS = 'ee'.repeat(32);

beforeEach(() => {
  const held = new Set<string>();
  vi.stubGlobal('navigator', { locks: { request: async (key: string, _options: unknown, fn: (lock: object | null) => unknown) => {
    if (held.has(key)) return fn(null);
    held.add(key);
    try { return await fn({ name: key }); } finally { held.delete(key); }
  } } });
});
afterEach(() => { vi.restoreAllMocks(); vi.unstubAllGlobals(); });

async function setup() {
  const values = new Map<string, string>();
  const state = {
    nonce: 0n, balance: 10_000_000_000n, token: 0n, rate: 1000n, chainId: DEVNET.chain_id, current: true,
    signing: 0, sent: 0, writes: 0, failStorageAfter: Infinity,
    outcome: 'accepted', afterSign: () => {}, afterSubmit: () => {}, notified: 0,
    pending: [] as { opId: string; nonce: bigint }[], submitted: null as VmOp | null,
  };
  const storage = {
    getItem: (key: string) => values.get(key) ?? null,
    setItem: (key: string, value: string) => {
      state.writes += 1;
      if (state.writes > state.failStorageAfter) throw new Error('storage unavailable');
      values.set(key, value);
    },
  };
  const adapter = new UnisatAdapter(new MockUnisatWallet({ secretKey: sha256(new TextEncoder().encode('playground/unit-test-only')), network: 'regtest' }));
  const account = await adapter.getAccount();
  const originalSign = adapter.signBip322.bind(adapter);
  adapter.signBip322 = async (message) => {
    state.signing += 1;
    const sig = await originalSign(message);
    state.afterSign();
    return sig;
  };
  const at = { height: '9', hash: 'aa'.repeat(32) };
  const fetchFn = (async (_url, init) => {
    const request = JSON.parse(String(init?.body));
    let result: unknown;
    let error: unknown;
    switch (request.method) {
      case 'ksana_getChainInfo': result = { chain_id: state.chainId, tip: at, daa_score: '9', protocol_version: 2,
        next_upgrade: null, max_known_version: 2, finalized: at }; break;
      case 'ksana_getAccount': result = { account: account.account, nonce: state.nonce.toString(),
        balances: [{ asset: '00'.repeat(32), amount: state.balance.toString() },
          ...(state.token ? [{ asset: toHex(TOKEN), amount: state.token.toString() }] : [])], locked_fees: [], at }; break;
      case 'ksana_estimateFee': result = { feerate_sat_vb: '2', path_b: { amount: '1', per_kib: state.rate.toString() }, at }; break;
      case 'ksana_submitOp': {
        state.sent += 1;
        state.submitted = decodeVmOp(fromHex(request.params[0]), DEVNET);
        // The durable ID must already exist when the request reaches even this fake node.
        expect([...values.values()].some((s) => s.includes(toHex(opId(state.submitted!))))).toBe(true);
        if (state.outcome === 'lost') throw new Error('response lost after acceptance');
        if (state.outcome === 'rejected') error = { code: -32011, message: 'policy rejection' };
        else if (state.outcome === 'internal') error = { code: -32603, message: 'internal error after admission' };
        else result = { op_id: state.outcome === 'wrong-id' ? 'bb'.repeat(32) : toHex(opId(state.submitted)) };
        state.afterSubmit();
        break;
      }
      default: throw new Error(`unexpected RPC: ${request.method}`);
    }
    return new Response(JSON.stringify({ jsonrpc: '2.0', id: request.id, ...(error ? { error } : { result }) }));
  }) as typeof fetch;
  const journal = new Journal(storage, DEVNET.chain_id, GENESIS, account.account);
  const c: Context = {
    node: new KsanaRpcClient('http://node.test', { fetch: fetchFn }), params: DEVNET, genesis: GENESIS, network: 'regtest',
    endpoint: endpointKey('http://node.test', DEVNET.chain_id), wallet: { name: 'test', account, adapter }, journal,
    isCurrent: () => state.current, pendingOperations: () => state.pending, onSubmitted: () => { state.notified += 1; },
  };
  return { c, state, storage, values };
}
const problem = (name: string) => ({ name: 'PlaygroundError', problem: name });
const missing = () => new RpcError(RPC_ERROR_CODES.NOT_FOUND, 'operation not found', undefined, 'ksana_getOpStatus');
const row = (c: Context, nonce: bigint, id: string, state: Entry['state'] = 'accepted'): Entry => ({
  endpoint: c.endpoint, chainId: c.params.chain_id, account: c.wallet.account.account, nonce: nonce.toString(),
  opId: id, contract: 'ab'.repeat(32), kind: 'Call', at: 1, state,
});
async function unresolved() {
  const fixture = await setup();
  fixture.state.outcome = 'lost';
  const { entry } = await submit(fixture.c, await prepare(fixture.c, deploy(), 1_000_000n));
  const status = vi.spyOn(fixture.c.node, 'getOpStatus').mockRejectedValue(missing());
  return { ...fixture, entry, status };
}

describe('inputs', () => {
  it('validates the shipped module and refuses others', async () => {
    await expect(validateWasm(wasm)).resolves.toBeUndefined();
    await expect(validateWasm(new Uint8Array([0, 1]))).rejects.toBeInstanceOf(PlaygroundError);
    await expect(validateWasm(new Uint8Array(65_537))).rejects.toBeInstanceOf(PlaygroundError);
    await expect(validateWasm(new Uint8Array([0, 97, 115, 109, 1, 0, 0, 0]))).rejects.toBeInstanceOf(PlaygroundError);
  });
  it('sorts and merges attached funds as the protocol takes them', () => {
    const a = new Uint8Array(32).fill(2);
    expect(sortFunds([{ asset: a, amount: 1n }, { asset: KSA, amount: 2n }, { asset: a, amount: 3n }]))
      .toEqual([{ asset: KSA, amount: 2n }, { asset: a, amount: 4n }]);
    for (const funds of [[{ asset: KSA, amount: 0n }], [{ asset: KSA, amount: -1n }], [{ asset: new Uint8Array(31), amount: 1n }],
      [{ asset: KSA, amount: 1n << 127n }, { asset: KSA, amount: 1n << 127n }]]) expect(() => sortFunds(funds)).toThrow(PlaygroundError);
    expect(() => sortFunds(Array.from({ length: 17 }, (_, i) => ({ asset: new Uint8Array(32).fill(i + 1), amount: 1n })))).toThrow(PlaygroundError);
    expect(() => checkFunds([{ asset: a, amount: 1n }, { asset: KSA, amount: 1n }])).toThrow(PlaygroundError);
    expect(() => checkFunds([{ asset: a, amount: 1n }, { asset: a, amount: 1n }])).toThrow(PlaygroundError);
  });
  it('sizes gas for deployments and calls', () => {
    expect(deployGas(0)).toBe(1_000_000n);
    expect(deployGas(10)).toBe(1_000_210n);
    expect(callGas(100_000n, DEVNET)).toBe(170_000n);
    expect(callGas(DEVNET.block_gas_limit, DEVNET)).toBe(DEVNET.block_gas_limit);
  });
  it('predicts contract IDs as the protocol derives them', () => {
    const deployer = new Uint8Array(32).fill(1);
    const codeHash = new Uint8Array(32).fill(2);
    const preimage = new Uint8Array([...deployer, 5, 0, 0, 0, 0, 0, 0, 0, ...codeHash]);
    expect(contractIdOf(deployer, 5n, codeHash)).toBe(toHex(sha256(preimage)));
  });
  it('adds up what an operation takes per asset', () => {
    const need = amountsNeeded({ kind: 'Call', contract: KSA, method: 'm', args: new Uint8Array(), funds: [{ asset: KSA, amount: 5n }, { asset: TOKEN, amount: 9n }] }, 2n);
    expect(need.get(toHex(KSA))).toBe(7n);
    expect(need.get(toHex(TOKEN))).toBe(9n);
    expect(amountsNeeded({ kind: 'AssetTransfer', asset: TOKEN, to: KSA, amount: 3n }, 1n).get(toHex(TOKEN))).toBe(3n);
  });
});

describe('wallet-signed path B operations', () => {
  it('prices deployment bytes/gas instead of copying a transfer fee, and signs the reviewed operation', async () => {
    const { c, state } = await setup();
    const a = deploy();
    const p = await prepare(c, a, 1_000_000n);
    expect(p.fee).toBeGreaterThan(1000n);
    if (a.kind === 'Deploy') a.code.fill(0); // the review is an independent copy
    const result = await submit(c, p);
    expect(result.entry.state).toBe('accepted');
    expect(state.sent).toBe(1);
    expect(state.signing).toBe(1);
    expect(state.submitted?.action.kind).toBe('Deploy');
    expect(c.journal.read()[0]?.opId).toBe(toHex(opId(state.submitted!)));
    expect(p.fee).toBeGreaterThanOrEqual(feeFor(state.rate, opCost(DEVNET, wasm.length, 1_000_000n), 1n));
  });
  it('accepts a zero per-KiB quote with the node minimum as the fee floor', async () => {
    const { c, state } = await setup(); state.rate = 0n;
    expect((await prepare(c, deploy(), 1_000_000n)).fee).toBe(1n);
  });
  it('records the method of a call and the asset of a transfer', async () => {
    const { c, state } = await setup(); state.token = 10n;
    await submit(c, await prepare(c, { kind: 'Call', contract: TOKEN, method: 'get', args: new Uint8Array(), funds: [] }, 50_000n));
    expect(c.journal.read()[0]).toMatchObject({ kind: 'Call', method: 'get', contract: toHex(TOKEN) });
    state.nonce = 1n;
    await submit(c, await prepare(c, { kind: 'AssetTransfer', asset: TOKEN, to: new Uint8Array(32).fill(9), amount: 10n }, 10_000n));
    expect(c.journal.read().at(-1)).toMatchObject({ kind: 'AssetTransfer', contract: toHex(TOKEN), nonce: '1' });
    expect(state.submitted?.action.kind).toBe('AssetTransfer');
  });
  it('refuses unsorted, duplicate, negative and zero funds before any signing', async () => {
    const { c, state } = await setup();
    for (const funds of [[{ asset: TOKEN, amount: 1n }, { asset: KSA, amount: 1n }], [{ asset: new Uint8Array(32), amount: 0n }],
      [{ asset: new Uint8Array(32), amount: -1n }], [{ asset: KSA, amount: 1n }, { asset: KSA, amount: 2n }]]) {
      const a = deploy(); if (a.kind !== 'Deploy') throw new Error('test action'); a.funds = funds;
      await expect(prepare(c, a, 1_000_000n)).rejects.toMatchObject(problem('funds'));
    }
    await expect(prepare(c, { kind: 'AssetTransfer', asset: TOKEN, to: KSA, amount: 0n }, 10_000n)).rejects.toMatchObject(problem('funds'));
    expect(state.signing).toBe(0);
  });
  it('checks fee plus attached KSA and other assets against the current balances', async () => {
    const { c, state } = await setup(); state.balance = 100_000_000n;
    const a = deploy(); if (a.kind === 'Deploy') a.funds = [{ asset: KSA, amount: 100_000_000n }];
    await expect(prepare(c, a, 1_000_000n)).rejects.toMatchObject(problem('balance'));
    state.balance = 10_000_000_000n; state.token = 4n;
    const b = deploy(); if (b.kind === 'Deploy') b.funds = [{ asset: TOKEN, amount: 5n }];
    await expect(prepare(c, b, 1_000_000n)).rejects.toMatchObject(problem('balance'));
    state.token = 5n;
    await expect(prepare(c, b, 1_000_000n)).resolves.toHaveProperty('fee');
  });
  it('rejects a quote reused with a different account, endpoint or network before asking for a signature', async () => {
    const { c, state } = await setup(); const p = await prepare(c, deploy(), 1_000_000n);
    for (const changed of [{ ...c, params: SIGNET }, { ...c, endpoint: endpointKey('http://other.test', DEVNET.chain_id) },
      { ...c, wallet: { ...c.wallet, account: { ...c.wallet.account, account: 'cc'.repeat(32) } } }]) {
      await expect(submit(changed, p)).rejects.toMatchObject(problem('changed'));
    }
    expect(state.signing).toBe(0); expect(state.sent).toBe(0);
  });
  it('checks the node network before preparing or signing', async () => {
    const { c, state } = await setup(); state.chainId = SIGNET.chain_id;
    await expect(prepare(c, deploy(), 1_000_000n)).rejects.toMatchObject({ name: 'ChainMismatchError' });
    expect(state.signing).toBe(0);
  });
  it('refuses a wallet set to another BTC network', async () => {
    const { c, state } = await setup();
    c.wallet.adapter.getNetwork = async () => ({ network: 'testnet', name: 'Testnet4' });
    await expect(prepare(c, deploy(), 1_000_000n)).rejects.toMatchObject(problem('walletNetwork'));
    expect(state.signing).toBe(0);
  });
  for (const [label, change, expected] of [
    ['source change', (s: Awaited<ReturnType<typeof setup>>['state']) => { s.current = false; }, 'changed'],
    ['nonce change', (s: Awaited<ReturnType<typeof setup>>['state']) => { s.nonce = 1n; }, 'nonce'],
    ['fee increase', (s: Awaited<ReturnType<typeof setup>>['state']) => { s.rate *= 2n; }, 'feeChanged'],
    ['balance drop', (s: Awaited<ReturnType<typeof setup>>['state']) => { s.balance = 0n; }, 'balance'],
  ] as const) it(`does not broadcast after a ${label} during signing`, async () => {
    const { c, state } = await setup(); const p = await prepare(c, deploy(), 1_000_000n);
    state.afterSign = () => change(state);
    await expect(submit(c, p)).rejects.toMatchObject(problem(expected));
    expect(state.sent).toBe(0);
  });
  it('preserves wallet rejection without contacting submitOp', async () => {
    const { c, state } = await setup(); const p = await prepare(c, deploy(), 1_000_000n);
    c.wallet.adapter.signBip322 = async () => { throw new Error('user rejected'); };
    await expect(submit(c, p)).rejects.toThrow('user rejected');
    expect(state.sent).toBe(0); expect(c.journal.read()).toEqual([]);
  });
  for (const outcome of ['lost', 'internal', 'wrong-id']) it(`persists ${outcome} as unknown and blocks a second write, including on another RPC`, async () => {
    const { c, state, storage } = await setup(); const p = await prepare(c, deploy(), 1_000_000n);
    state.outcome = outcome;
    const result = await submit(c, p);
    expect(result.entry.state).toBe('unknown'); expect(state.sent).toBe(1);
    const reloaded = { ...c, journal: new Journal(storage, DEVNET.chain_id, GENESIS, c.wallet.account.account), endpoint: endpointKey('http://other.test', DEVNET.chain_id) };
    await expect(prepare(reloaded, deploy(), 1_000_000n)).rejects.toMatchObject(problem('pending'));
    expect(state.signing).toBe(1); expect(state.sent).toBe(1);
  });
  it('a definite RPC rejection allows a fresh review, never an automatic resend', async () => {
    const { c, state } = await setup(); state.outcome = 'rejected';
    const result = await submit(c, await prepare(c, deploy(), 1_000_000n));
    expect(result.entry.state).toBe('rejected');
    await expect(prepare(c, deploy(), 1_000_000n)).resolves.toHaveProperty('fee');
    expect(state.sent).toBe(1);
  });
  it('refuses to send without a durable record, but keeps an accepted result if later storage fails', async () => {
    const first = await setup(); first.state.failStorageAfter = 0;
    await expect(submit(first.c, await prepare(first.c, deploy(), 1_000_000n))).rejects.toMatchObject(problem('storage'));
    expect(first.state.sent).toBe(0);
    const second = await setup(); second.state.failStorageAfter = 1;
    const result = await submit(second.c, await prepare(second.c, deploy(), 1_000_000n));
    expect(result.entry.state).toBe('accepted'); expect(result.error).toBeInstanceOf(PlaygroundError);
    expect(second.c.journal.read()[0]?.state).toBe('submitting'); expect(second.state.sent).toBe(1);
  });
  it('blocks concurrent submissions before a second wallet prompt', async () => {
    const { c, state } = await setup(); const p = await prepare(c, deploy(), 1_000_000n);
    const first = submit(c, p);
    await expect(submit(c, p)).rejects.toMatchObject(problem('busy'));
    await first; expect(state.signing).toBe(1); expect(state.sent).toBe(1);
  });
  it('keeps an accepted result in its original journal without notifying a wallet selected during the request', async () => {
    const { c, state } = await setup();
    const p = await prepare(c, deploy(), 1_000_000n);
    state.afterSubmit = () => { state.current = false; };
    const result = await submit(c, p);
    expect(result.entry.state).toBe('accepted');
    expect(c.journal.read()[0]?.account).toBe(c.wallet.account.account);
    expect(state.notified).toBe(0);
    expect(state.sent).toBe(1);
  });
  it('keeps separate journal keys for different accounts and rejects corrupt storage', async () => {
    const { c, storage, values } = await setup();
    await submit(c, await prepare(c, deploy(), 1_000_000n));
    const other = new Journal(storage, DEVNET.chain_id, GENESIS, 'dd'.repeat(32));
    expect(other.read()).toEqual([]);
    expect(other.key).not.toBe(c.journal.key);
    values.set(other.key, '{}'); expect(() => other.read()).toThrow(PlaygroundError);
    expect(c.journal.read()).toHaveLength(1);
  });
  it('starts a fresh journal on a chain started again from genesis, whose nonces the rows before would block for good', async () => {
    const { c, state, storage } = await setup();
    await submit(c, await prepare(c, deploy(), 1_000_000n));
    expect(c.journal.read()[0]?.state).toBe('accepted');
    // the same chain ID, another genesis block: the account's nonce is 0 again
    const restarted: Context = { ...c, genesis: 'ff'.repeat(32), journal: new Journal(storage, DEVNET.chain_id, 'ff'.repeat(32), c.wallet.account.account) };
    expect(restarted.journal.read()).toEqual([]);
    await expect(prepare(c, deploy(), 1_000_000n)).rejects.toMatchObject(problem('pending'));
    await expect(prepare(restarted, deploy(), 1_000_000n)).resolves.toHaveProperty('fee');
    // a journal of the chain before is never used with the new one
    await expect(prepare({ ...restarted, journal: c.journal }, deploy(), 1_000_000n)).rejects.toMatchObject(problem('changed'));
    expect(state.sent).toBe(1);
  });
});

describe('explicit unresolved-nonce recovery', () => {
  it('requires confirmation and NOT_FOUND, keeps the ID, and does not sign or send', async () => {
    const { c, state, entry, status } = await unresolved();
    await expect(releaseNonce(c, entry.opId)).rejects.toMatchObject(problem('release'));
    expect(status).not.toHaveBeenCalled();
    await expect(prepare(c, deploy(), 1_000_000n)).rejects.toMatchObject(problem('pending'));
    await expect(releaseNonce(c, entry.opId, true)).resolves.toMatchObject({ opId: entry.opId, state: 'released' });
    expect(c.journal.read()).toHaveLength(1);
    expect(c.journal.read()[0]?.opId).toBe(entry.opId);
    await expect(prepare(c, deploy(), 1_000_000n)).resolves.toMatchObject({ unsigned: { nonce: 0n } });
    expect(state.signing).toBe(1); expect(state.sent).toBe(1);
  });
  for (const error of [new Error('request timed out'), new RpcError(-32603, 'internal error')]) {
    it(`does not release on ${error.message}`, async () => {
      const { c, entry, status } = await unresolved(); status.mockRejectedValue(error);
      await expect(releaseNonce(c, entry.opId, true)).rejects.toBe(error);
      expect(c.journal.read()[0]?.state).toBe('unknown');
    });
  }
  it('does not release an operation the node can still find', async () => {
    const { c, entry, status } = await unresolved();
    status.mockResolvedValue({ status: 'pending', block: null, index: null, receipt: null });
    await expect(releaseNonce(c, entry.opId, true)).rejects.toMatchObject(problem('pending'));
    expect(c.journal.read()[0]?.state).toBe('unknown');
  });
  it('rechecks the current nonce after the not-found response', async () => {
    const { c, state, entry, status } = await unresolved();
    status.mockImplementation(async () => { state.nonce = 1n; throw missing(); });
    await expect(releaseNonce(c, entry.opId, true)).rejects.toMatchObject(problem('nonce'));
    expect(c.journal.read()[0]?.state).toBe('unknown');
  });
  it('does not release a future nonce', async () => {
    const { c, entry } = await unresolved();
    c.journal.put({ ...entry, nonce: '1' });
    await expect(releaseNonce(c, entry.opId, true)).rejects.toMatchObject(problem('nonce'));
    expect(c.journal.read()[0]?.state).toBe('unknown');
  });
  it('can explicitly recover an old inclusion report only when the node now reports missing and its nonce is current again', async () => {
    const { c, state, entry } = await unresolved();
    c.journal.put({ ...entry, state: 'included' });
    state.nonce = 1n;
    await expect(releaseNonce(c, entry.opId, true)).rejects.toMatchObject(problem('nonce'));
    state.nonce = 0n;
    await expect(releaseNonce(c, entry.opId, true)).resolves.toHaveProperty('state', 'released');
    expect(state.sent).toBe(1);
  });
  for (const change of ['wallet', 'network'] as const) it(`does not release after a ${change} change during the lookup`, async () => {
    const { c, state, entry, status } = await unresolved();
    status.mockImplementation(async () => {
      if (change === 'wallet') state.current = false;
      else state.chainId = SIGNET.chain_id;
      throw missing();
    });
    await expect(releaseNonce(c, entry.opId, true)).rejects.toThrow();
    expect(c.journal.read()[0]?.state).toBe('unknown');
  });
  it('shares the per-account Web Lock and refuses a browser without it', async () => {
    const { c, entry } = await unresolved();
    await withWriteLock(c.journal, async () => {
      await expect(releaseNonce({ ...c, endpoint: endpointKey('http://other.test', DEVNET.chain_id) }, entry.opId, true))
        .rejects.toMatchObject(problem('busy'));
    });
    vi.stubGlobal('navigator', {});
    await expect(releaseNonce(c, entry.opId, true)).rejects.toMatchObject(problem('locks'));
    expect(c.journal.read()[0]?.state).toBe('unknown');
  });
  it('leaves the guard intact when persisting the release fails', async () => {
    const { c, state, entry } = await unresolved(); state.failStorageAfter = state.writes;
    await expect(releaseNonce(c, entry.opId, true)).rejects.toMatchObject(problem('storage'));
    expect(c.journal.pending(0n)).toBe(true);
    expect(state.signing).toBe(1); expect(state.sent).toBe(1);
  });
  it('ignores only the explicitly released ID in the local pending cache', async () => {
    const { c, state, entry } = await unresolved();
    state.pending = [{ opId: entry.opId, nonce: 0n }];
    await releaseNonce(c, entry.opId, true);
    await expect(prepare(c, deploy(), 1_000_000n)).resolves.toHaveProperty('fee');
    state.pending.push({ opId: 'fe'.repeat(32), nonce: 0n });
    await expect(prepare(c, deploy(), 1_000_000n)).rejects.toMatchObject(problem('pending'));
  });
  it('cannot release a different operation holding the same nonce', async () => {
    const { c, entry } = await unresolved();
    c.journal.put(row(c, 0n, 'ee'.repeat(32), 'unknown'));
    await releaseNonce(c, entry.opId, true);
    await expect(prepare(c, deploy(), 1_000_000n)).rejects.toMatchObject(problem('pending'));
  });
  it('retires used nonces without claiming inclusion, then frees capacity for a new write', async () => {
    const { c, state } = await setup();
    for (let n = 0; n < 64; n++) c.journal.put(row(c, BigInt(n), n.toString(16).padStart(64, '0')));
    state.nonce = 64n;
    const prepared = await prepare(c, deploy(), 1_000_000n);
    expect(c.journal.read().every((entry) => entry.state === 'retired')).toBe(true);
    await submit(c, prepared);
    expect(c.journal.read()).toHaveLength(64);
    expect(c.journal.read().at(-1)?.nonce).toBe('64');
    expect(state.sent).toBe(1);
  });
  it('does not silently release a retired nonce when the account nonce moves backwards', async () => {
    const { c } = await setup(); c.journal.put(row(c, 0n, 'ee'.repeat(32), 'retired'));
    await expect(prepare(c, deploy(), 1_000_000n)).rejects.toMatchObject(problem('pending'));
  });
  it('preserves a release on a late pending report, but records actual inclusion', async () => {
    const { c, entry } = await unresolved(); await releaseNonce(c, entry.opId, true);
    c.journal.observe(entry.opId, 'accepted');
    expect(c.journal.read()[0]?.state).toBe('released');
    c.journal.observe(entry.opId, 'included');
    expect(c.journal.read()[0]?.state).toBe('included');
    c.journal.observe('ee'.repeat(32), 'accepted');
    expect(c.journal.read()).toHaveLength(1);
  });
});

describe('browser preflight and status polling', () => {
  it('compiles the shipped >4 KiB WASM asynchronously and never instantiates it', async () => {
    const compile = vi.spyOn(WebAssembly, 'compile');
    const instantiate = vi.spyOn(WebAssembly, 'instantiate');
    expect(wasm.length).toBeGreaterThan(4096);
    await validateWasm(wasm);
    expect(compile).toHaveBeenCalledOnce(); expect(instantiate).not.toHaveBeenCalled();
  });
  it('snapshots the action before asynchronous preflight', async () => {
    const { c } = await setup(); const action = deploy();
    const preparing = prepare(c, action, 1_000_000n);
    if (action.kind === 'Deploy') action.code.fill(0);
    const prepared = await preparing;
    expect(prepared.unsigned.action.kind).toBe('Deploy');
    if (prepared.unsigned.action.kind === 'Deploy') expect(prepared.unsigned.action.code).toEqual(wasm);
  });
  it('stops on BTC finality and retains the bounded retry limits', () => {
    expect(shouldPoll('btc_final', 1, 0)).toBe(false);
    for (const status of ['pending', 'fast_confirmed', 'failed', 'skipped', undefined] as const) {
      expect(shouldPoll(status, 1, 0)).toBe(true);
      expect(shouldPoll(status, 120, 0)).toBe(false);
      expect(shouldPoll(status, 3, 3)).toBe(false);
    }
  });
});
