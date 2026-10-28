import { afterEach, describe, expect, it, vi } from 'vitest';
import { RpcError } from '@ksana/sdk';
import { BUSY_RETRY_MS, CALLS_IN_FLIGHT, PageNode, SERVER_IS_BUSY } from './node';

const RECEIPT = { success: true, gas_used: '1', output: '', events: [], error: null, op_id: null };
const call = () => ({ contract: new Uint8Array(32), method: 'get', args: new Uint8Array() });

/** A node that answers ksana_call when `release` is called, after answering busy to the first `busy` calls */
function fakeNode(busy = 0) {
  const state = { inFlight: 0, most: 0, calls: 0, busy, held: [] as (() => void)[] };
  const fetchFn = (async (_url, init) => {
    const request = JSON.parse(String(init?.body));
    expect(request.method).toBe('ksana_call');
    state.calls += 1;
    if (state.busy > 0) {
      state.busy -= 1;
      return new Response(JSON.stringify({ jsonrpc: '2.0', id: request.id, error: { code: SERVER_IS_BUSY, message: '4 read-only calls are already running; try again later' } }));
    }
    state.inFlight += 1;
    state.most = Math.max(state.most, state.inFlight);
    await new Promise<void>((resolve) => state.held.push(resolve));
    state.inFlight -= 1;
    return new Response(JSON.stringify({ jsonrpc: '2.0', id: request.id, result: RECEIPT }));
  }) as typeof fetch;
  const release = () => { for (const r of state.held.splice(0)) r(); };
  return { node: new PageNode('http://node.test', { fetch: fetchFn }), state, release };
}

const settle = () => new Promise((resolve) => setTimeout(resolve, 0));

afterEach(() => { vi.useRealTimers(); });

describe('the page node', () => {
  it('keeps at most CALLS_IN_FLIGHT calls in flight, the rest in order behind them', async () => {
    const { node, state, release } = fakeNode();
    const done: number[] = [];
    const calls = Array.from({ length: 7 }, (_, i) => node.call(call()).then(() => { done.push(i); }));
    for (let round = 0; round < 4; round++) {
      await settle();
      expect(state.inFlight).toBeLessThanOrEqual(CALLS_IN_FLIGHT);
      release();
      await settle();
    }
    await Promise.all(calls);
    expect(state.most).toBe(CALLS_IN_FLIGHT);
    expect(state.calls).toBe(7);
    expect(done).toEqual([0, 1, 2, 3, 4, 5, 6]);
  });

  it('asks again for a call the node was too busy to run', async () => {
    vi.useFakeTimers();
    const { node, state, release } = fakeNode(2);
    const receipt = node.call(call());
    await vi.advanceTimersByTimeAsync(BUSY_RETRY_MS[0]! + BUSY_RETRY_MS[1]!);
    expect(state.calls).toBe(3);
    release();
    await expect(receipt).resolves.toMatchObject({ success: true });
  });

  it('gives the busy answer once the tries are spent, and passes other errors on at once', async () => {
    vi.useFakeTimers();
    const { node, state } = fakeNode(BUSY_RETRY_MS.length + 1);
    const receipt = node.call(call());
    const failed = expect(receipt).rejects.toMatchObject({ name: 'RpcError', code: SERVER_IS_BUSY });
    await vi.advanceTimersByTimeAsync(BUSY_RETRY_MS.reduce((a, b) => a + b, 0));
    await failed;
    expect(state.calls).toBe(BUSY_RETRY_MS.length + 1);

    const other = new PageNode('http://node.test', {
      fetch: (async () => new Response(JSON.stringify({ jsonrpc: '2.0', id: 1, error: { code: -32602, message: 'max_gas above the limit' } }))) as typeof fetch,
    });
    await expect(other.call(call())).rejects.toSatisfy((e: unknown) => e instanceof RpcError && e.code === -32602);
  });

  it('frees its place when a call fails, so the calls behind it go', async () => {
    let n = 0;
    const node = new PageNode('http://node.test', {
      fetch: (async (_url, init) => {
        const request = JSON.parse(String(init?.body));
        n += 1;
        return new Response(JSON.stringify(n <= CALLS_IN_FLIGHT
          ? { jsonrpc: '2.0', id: request.id, error: { code: -32603, message: 'internal' } }
          : { jsonrpc: '2.0', id: request.id, result: RECEIPT }));
      }) as typeof fetch,
    });
    const calls = Array.from({ length: CALLS_IN_FLIGHT + 2 }, () => node.call(call()).then(() => 'ok', () => 'failed'));
    expect(await Promise.all(calls)).toEqual([...Array(CALLS_IN_FLIGHT).fill('failed'), 'ok', 'ok']);
  });
});
