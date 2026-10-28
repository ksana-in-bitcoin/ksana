import assert from 'node:assert/strict';
import { EventEmitter } from 'node:events';
import http from 'node:http';
import test from 'node:test';
import { RateLimit, allowOrigin, clientAddress, readBody, sendJson } from '../lib.mjs';

test('a rate limit spends its capacity, then refills by the minute', () => {
  let t = 0;
  const limit = new RateLimit(2, 6, () => t);
  assert.equal(limit.take('a'), true);
  assert.equal(limit.take('a'), true);
  assert.equal(limit.take('a'), false);
  assert.equal(limit.take('b'), true, 'other keys have their own bucket');
  t += 10_000; // 6 a minute: one token every 10 seconds
  assert.equal(limit.take('a'), true);
  assert.equal(limit.take('a'), false);
  t += 3_600_000;
  limit.sweep();
  assert.equal(limit.buckets.size, 0, 'full buckets are forgotten');
});

test('a rate limit says how long until its next token', () => {
  let t = 0;
  const limit = new RateLimit(2, 6, () => t);
  assert.equal(limit.wait('a'), 0, 'a key not seen yet has a full bucket');
  limit.take('a');
  assert.equal(limit.wait('a'), 0, 'one token left');
  limit.take('a');
  assert.equal(limit.wait('a'), 10_000);
  t += 4_000;
  assert.equal(limit.wait('a'), 6_000);
  t += 6_000;
  assert.equal(limit.wait('a'), 0);
  assert.equal(limit.take('a'), true, 'the token is there once the wait is over');

  // Five a day: from an empty bucket, the sum take() compares is 0.9999999999999999 after exactly 288 minutes
  const daily = new RateLimit(5, 5 / 1_440, () => t);
  for (let i = 0; i < 5; i += 1) daily.take('b');
  const ms = daily.wait('b');
  assert.ok(ms > 288 * 60_000 && ms < 288 * 60_000 + 10, `${ms} ms`);
  t += ms - 1;
  assert.equal(daily.take('b'), false, 'a millisecond early');
  t += 1;
  assert.equal(daily.take('b'), true);
});

test('the client address is the first X-Forwarded-For entry, else the socket', () => {
  const socket = { remoteAddress: '10.0.0.9' };
  assert.equal(clientAddress({ headers: { 'x-forwarded-for': '203.0.113.7, 10.0.0.2' }, socket }), '203.0.113.7');
  assert.equal(clientAddress({ headers: {}, socket }), '10.0.0.9');
});

function request(chunks) {
  const req = new EventEmitter();
  req.destroy = () => {
    req.destroyed = true;
  };
  queueMicrotask(() => {
    for (const chunk of chunks) {
      if (req.destroyed) return;
      req.emit('data', Buffer.from(chunk));
    }
    req.emit('end');
  });
  return req;
}

test('a request body is read up to its limit', async () => {
  assert.equal(await readBody(request(['{"a":', '1}']), 16), '{"a":1}');
  await assert.rejects(readBody(request(['x'.repeat(10), 'y'.repeat(10)]), 16), (e) => e.status === 413);
});

test('a service lets the page of one other origin call it, and answers alone without one', async () => {
  const listen = (listener) =>
    new Promise((resolve) => {
      const server = http.createServer(listener).listen(0, '127.0.0.1', () => resolve(server));
    });
  const service = (req, res) => sendJson(res, req.method === 'OPTIONS' ? 405 : 200, { method: req.method });
  const page = 'https://playground.example';
  const allowed = await listen(allowOrigin(page, service));
  const alone = await listen(allowOrigin('', service));
  try {
    const at = (server) => `http://127.0.0.1:${server.address().port}/`;
    const preflight = await fetch(at(allowed), {
      method: 'OPTIONS',
      headers: { origin: page, 'access-control-request-method': 'POST', 'access-control-request-headers': 'content-type' },
    });
    assert.equal(preflight.status, 204);
    assert.equal(preflight.headers.get('access-control-allow-origin'), page);
    assert.equal(preflight.headers.get('access-control-allow-methods'), 'GET, POST');
    assert.equal(preflight.headers.get('access-control-allow-headers'), 'content-type');
    const post = await fetch(at(allowed), { method: 'POST', headers: { origin: page, 'content-type': 'application/json' }, body: '{}' });
    assert.equal(post.status, 200);
    assert.equal(post.headers.get('access-control-allow-origin'), page);
    assert.equal(post.headers.get('vary'), 'origin');
    assert.deepEqual(await post.json(), { method: 'POST' });
    // without an origin the service answers alone: no page of another origin reads it
    const plain = await fetch(at(alone), { method: 'OPTIONS', headers: { origin: page } });
    assert.equal(plain.status, 405);
    assert.equal(plain.headers.get('access-control-allow-origin'), null);
  } finally {
    allowed.close();
    alone.close();
  }
});
