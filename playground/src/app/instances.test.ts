// The contracts the page keeps in this browser, per chain: a network started again from genesis keeps its chain ID, and none of
// the contracts kept for the chain before (the visitor's own or the operator's shared ones) are offered on it.
import { afterEach, beforeEach, expect, it, vi } from 'vitest';
import { DEVNET, SIGNET } from '@ksana/sdk';
import { loadDeployment } from '../chain/deployment';
import { addInstance, currentInstance, instancesOf } from './instances';

const id = (n: number) => n.toString(16).padStart(2, '0').repeat(32);
const before = { params: SIGNET, genesis: 'aa'.repeat(32) };
const after = { params: SIGNET, genesis: 'bb'.repeat(32) };

beforeEach(() => {
  const values = new Map<string, string>();
  vi.stubGlobal('window', {
    location: { href: 'https://playground.test/' },
    localStorage: {
      getItem: (key: string) => values.get(key) ?? null,
      setItem: (key: string, value: string) => void values.set(key, value),
      removeItem: (key: string) => void values.delete(key),
    },
  });
});
afterEach(() => vi.unstubAllGlobals());

async function deployment(value: unknown) {
  vi.stubGlobal('fetch', async () => new Response(JSON.stringify(value)));
  await loadDeployment();
}

it('keeps the contracts of a chain apart from those of the chain its ID had before a restart from genesis', () => {
  addInstance(before, 'amm', { id: id(1), origin: 'mine', at: 1 });
  expect(instancesOf(before, 'amm').map((i) => i.id)).toEqual([id(1)]);
  expect(currentInstance(before, 'amm')?.id).toBe(id(1));
  expect(instancesOf(after, 'amm')).toEqual([]);
  expect(currentInstance(after, 'amm')).toBeNull();
  addInstance(after, 'amm', { id: id(2), origin: 'opened', at: 2 });
  expect(instancesOf(before, 'amm').map((i) => i.id)).toEqual([id(1)]);
  expect(instancesOf(after, 'amm').map((i) => i.id)).toEqual([id(2)]);
});

it('offers the operator’s contracts only on the chain whose genesis block the deployment names', async () => {
  await deployment({ chain: 'signet', rpc: 'https://node.test/rpc', genesis: before.genesis.toUpperCase(), instances: { amm: id(3) } });
  expect(instancesOf(before, 'amm')).toEqual([{ id: id(3), origin: 'shared', at: 0 }]);
  expect(currentInstance(before, 'amm')?.id).toBe(id(3));
  expect(instancesOf(after, 'amm')).toEqual([]);
  expect(instancesOf({ params: DEVNET, genesis: before.genesis }, 'amm')).toEqual([]);
  // a deployment that names no genesis block offers none
  await deployment({ chain: 'signet', rpc: 'https://node.test/rpc', instances: { amm: id(3) } });
  expect(instancesOf(before, 'amm')).toEqual([]);
});
