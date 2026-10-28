// The hosted copy's deployment.json (hosted/, D-119), which the page reads as it starts: one it could not read would leave the
// page at playground.ksana.network with no node, faucet or compile service.
import { readFileSync } from 'node:fs';
import { expect, it } from 'vitest';
import { parseDeployment } from './deployment';

it('reads the hosted deployment.json: the signet, its endpoints on the signet server, and known examples', () => {
  const raw = JSON.parse(readFileSync(new URL('../../hosted/deployment.json', import.meta.url), 'utf8')) as Record<string, unknown>;
  const deployment = parseDeployment(raw);
  expect(deployment).not.toBeNull();
  expect(deployment!.chain).toBe('signet');
  for (const url of [deployment!.rpc, deployment!.faucet, deployment!.compile, deployment!.explorer]) {
    expect(new URL(url ?? '').origin).toBe('https://signet.ksana.network');
  }
  // every shared contract it names is kept, and under an example's ID
  const ids = [...readFileSync(new URL('../examples/index.ts', import.meta.url), 'utf8').matchAll(/id: '([a-z]+)'/g)].map((m) => m[1]);
  const named = Object.keys((raw.instances ?? {}) as Record<string, unknown>);
  expect(Object.keys(deployment!.instances ?? {})).toEqual(named);
  for (const example of named) expect(ids).toContain(example);
});
