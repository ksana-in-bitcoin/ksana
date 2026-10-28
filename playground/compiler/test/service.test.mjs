// The compile service's HTTP side with a stand-in builder: routes, limits, the queue, and the answers' shapes.
import assert from 'node:assert/strict';
import http from 'node:http';
import { after, before, describe, it } from 'node:test';
import { MAX_SOURCE_BYTES } from '../lib/build.mjs';
import { BusyError, clientOf, createHandler, Limiter, Pool } from '../lib/service.mjs';

function fakeBuilder(delayMs = 0) {
  return {
    builds: [],
    async build(crate, source, signal) {
      this.builds.push({ crate, source });
      if (delayMs) {
        await new Promise((resolve) => {
          const timer = setTimeout(resolve, delayMs);
          signal?.addEventListener('abort', () => {
            clearTimeout(timer);
            resolve();
          });
        });
        if (signal?.aborted) return { ok: false, aborted: true, diagnostics: [] };
      }
      if (source.includes('broken')) {
        return { ok: false, diagnostics: [{ level: 'error', message: 'broken', rendered: 'error: broken\n', spans: [] }] };
      }
      const wasm = Buffer.from([0, 0x61, 0x73, 0x6d, 1, 0, 0, 0]);
      return { ok: true, wasm, sha256: 'ab'.repeat(32), diagnostics: [] };
    },
  };
}

async function listen(handler) {
  const server = http.createServer(handler);
  await new Promise((resolve) => server.listen(0, '127.0.0.1', resolve));
  return { server, base: `http://127.0.0.1:${server.address().port}` };
}

const post = (base, path, body, headers = { 'content-type': 'application/json' }) =>
  fetch(`${base}${path}`, { method: 'POST', headers, body: typeof body === 'string' ? body : JSON.stringify(body) });

describe('the handler', () => {
  let server;
  let base;
  const builder = fakeBuilder();
  const limiter = new Limiter({ perMinute: 0 });
  before(async () => {
    ({ server, base } = await listen(createHandler({
      pool: new Pool([builder], 2), limiter, crates: ['playground-counter', 'amm'], info: () => ({ rustc: 'rustc 1.98.1', crates: ['playground-counter', 'amm'] }),
    })));
  });
  after(() => server.close());

  it('answers its health with what it builds', async () => {
    for (const path of ['/health', '/compile/health']) {
      const res = await fetch(`${base}${path}`);
      assert.equal(res.status, 200);
      assert.equal(res.headers.get('cache-control'), 'no-store');
      assert.deepEqual(await res.json(), { ok: true, rustc: 'rustc 1.98.1', crates: ['playground-counter', 'amm'], busy: 0, queued: 0 });
    }
  });

  it('builds a source as the crate it names, with or without the /compile prefix', async () => {
    let res = await post(base, '/', { source: 'fn main() {}', crate: 'amm' });
    assert.equal(res.status, 200);
    const body = await res.json();
    assert.equal(body.ok, true);
    assert.equal(Buffer.from(body.wasm, 'base64').toString('hex'), '0061736d01000000');
    assert.equal(body.sha256, 'ab'.repeat(32));
    assert.deepEqual(body.diagnostics, []);
    assert.equal(typeof body.ms, 'number');
    assert.deepEqual(builder.builds.at(-1), { crate: 'amm', source: 'fn main() {}' });
    res = await post(base, '/compile', { source: 'broken', crate: 'playground-counter' });
    assert.deepEqual((await res.json()).ok, false);
  });

  it('refuses what it does not build', async () => {
    const cases = [
      [post(base, '/', { source: 'x', crate: 'token' }), 400, /crate is one of/],
      [post(base, '/', { crate: 'amm' }), 400, /no source/],
      [post(base, '/', 'not json'), 400, /not JSON/],
      [post(base, '/', { source: 'x', crate: 'amm' }, { 'content-type': 'text/plain' }), 415, /application\/json/],
      [post(base, '/', { source: 'x'.repeat(MAX_SOURCE_BYTES + 1), crate: 'amm' }), 413, /too large/],
      [fetch(`${base}/other`), 404, /POST \//],
    ];
    for (const [request, status, error] of cases) {
      const res = await request;
      assert.equal(res.status, status);
      assert.match((await res.json()).error, error);
    }
  });

  it('sends its CORS origin with every answer when it has one, a build included', async () => {
    const open = await listen(createHandler({
      pool: new Pool([fakeBuilder()], 2), limiter: new Limiter({ perMinute: 0 }), crates: ['amm'], info: () => ({}), cors: 'http://localhost:5174',
    }));
    try {
      const preflight = await fetch(`${open.base}/`, { method: 'OPTIONS' });
      assert.equal(preflight.status, 204);
      assert.match(preflight.headers.get('access-control-allow-methods') ?? '', /POST/);
      for (const res of [
        await post(open.base, '/', { source: 'fn a() {}', crate: 'amm' }),
        await post(open.base, '/', { source: 'broken', crate: 'amm' }),
        await post(open.base, '/', { source: 'x', crate: 'other' }),
        await fetch(`${open.base}/health`),
      ]) {
        assert.equal(res.headers.get('access-control-allow-origin'), 'http://localhost:5174');
        await res.arrayBuffer();
      }
      const res = await fetch(`${base}/health`);
      assert.equal(res.headers.get('access-control-allow-origin'), null);
      await res.arrayBuffer();
    } finally {
      open.server.close();
    }
  });

  it('limits the builds of one address', async () => {
    const limited = await listen(createHandler({ pool: new Pool([fakeBuilder()], 2), limiter: new Limiter({ perMinute: 6, burst: 2 }), crates: ['amm'], info: () => ({}) }));
    try {
      for (let i = 0; i < 2; i++) assert.equal((await post(limited.base, '/', { source: 'fn a() {}', crate: 'amm' })).status, 200);
      const res = await post(limited.base, '/', { source: 'fn a() {}', crate: 'amm' });
      assert.equal(res.status, 429);
      assert.ok(Number(res.headers.get('retry-after')) >= 1);
      assert.match((await res.json()).error, /too many builds/);
    } finally {
      limited.server.close();
    }
  });
});

describe('the queue', () => {
  it('runs one build a slot and makes the rest wait, up to its length', async () => {
    const builder = fakeBuilder(150);
    const pool = new Pool([builder], 1);
    const first = pool.run((b) => b.build('amm', 'one'));
    const second = pool.run((b) => b.build('amm', 'two'));
    await assert.rejects(pool.run((b) => b.build('amm', 'three')), BusyError);
    assert.equal(pool.busy, 1);
    assert.equal(pool.queued, 1);
    await Promise.all([first, second]);
    assert.deepEqual(builder.builds.map((b) => b.source), ['one', 'two']);
    assert.equal(pool.busy, 0);
  });

  it('drops a build that is given up while it waits', async () => {
    const builder = fakeBuilder(100);
    const pool = new Pool([builder], 4);
    const first = pool.run((b) => b.build('amm', 'one'));
    const controller = new AbortController();
    const second = pool.run((b) => b.build('amm', 'two'), controller.signal);
    controller.abort();
    await assert.rejects(second, { name: 'AbortError' });
    await first;
    assert.deepEqual(builder.builds.map((b) => b.source), ['one']);
    assert.equal(pool.queued, 0);
  });

  it('answers busy over HTTP when the queue is full', async () => {
    const pool = new Pool([fakeBuilder(300)], 0);
    const { server, base } = await listen(createHandler({ pool, limiter: new Limiter({ perMinute: 0 }), crates: ['amm'], info: () => ({}) }));
    try {
      const first = post(base, '/', { source: 'one', crate: 'amm' });
      await new Promise((resolve) => setTimeout(resolve, 50));
      const res = await post(base, '/', { source: 'two', crate: 'amm' });
      assert.equal(res.status, 503);
      assert.match((await res.json()).error, /busy/);
      assert.equal((await first).status, 200);
    } finally {
      server.close();
    }
  });
});

describe('the limit per address', () => {
  it('refills at its rate up to its burst', () => {
    const limiter = new Limiter({ perMinute: 6, burst: 2 });
    assert.equal(limiter.take('a', 0), true);
    assert.equal(limiter.take('a', 0), true);
    assert.equal(limiter.take('a', 0), false);
    assert.equal(limiter.take('b', 0), true);
    assert.equal(limiter.take('a', 9_999), false);
    assert.equal(limiter.take('a', 10_000), true);
    limiter.sweep(120_000);
    assert.equal(limiter.buckets.size, 0);
  });

  it('reads the forwarded address only behind a proxy', () => {
    const req = { headers: { 'x-forwarded-for': '203.0.113.7, 10.0.0.2' }, socket: { remoteAddress: '172.18.0.5' } };
    assert.equal(clientOf(req, true), '203.0.113.7');
    assert.equal(clientOf(req, false), '172.18.0.5');
  });
});
