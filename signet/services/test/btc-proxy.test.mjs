import assert from 'node:assert/strict';
import test from 'node:test';
import { Proxy, checkRequest, esploraClient, esploraPath } from '../btc-proxy.mjs';

const SCRIPT = '5120' + '11'.repeat(32);

test('only the wallet methods pass, with their parameters checked', () => {
  assert.deepEqual(checkRequest('getblockcount', []), []);
  assert.deepEqual(checkRequest('getblockcount', undefined), []);
  assert.deepEqual(checkRequest('getblockchaininfo', []), []);
  assert.deepEqual(checkRequest('estimatesmartfee', [6]), [6]);
  assert.deepEqual(checkRequest('estimatesmartfee', [2, 'ECONOMICAL']), [2, 'economical']);
  assert.deepEqual(checkRequest('sendrawtransaction', ['00ff']), ['00ff']);

  const refused = [
    ['getpeerinfo', []],
    ['stop', []],
    ['getblockcount', [1]],
    ['getblockchaininfo', ['x']],
    ['estimatesmartfee', [0]],
    ['estimatesmartfee', [1009]],
    ['estimatesmartfee', [6, 'fast']],
    ['sendrawtransaction', ['0']],
    ['sendrawtransaction', ['zz']],
    ['sendrawtransaction', ['00', 0]],
    ['sendrawtransaction', ['00'.repeat(400_001)]],
    ['getblockcount', { a: 1 }],
  ];
  for (const [method, params] of refused) {
    assert.throws(() => checkRequest(method, params), /./, `${method} ${JSON.stringify(params).slice(0, 60)}`);
  }
});

test('scantxoutset is refused with the reason, which the page shows instead of the coins', () => {
  const scans = [
    ['start', [`raw(${SCRIPT})`]],
    ['start', [{ desc: `raw(${SCRIPT})` }]],
    ['abort', []],
    ['status', []],
    undefined,
  ];
  for (const params of scans) {
    assert.throws(
      () => checkRequest('scantxoutset', params),
      (e) => e.code === -32601 && /Esplora/.test(e.message),
      JSON.stringify(params),
    );
  }
});

/** A bitcoind whose calls wait until the test answers them. */
function fakeBitcoind() {
  const calls = [];
  return {
    calls,
    call(method, params) {
      return new Promise((resolve) => calls.push({ method, params, resolve }));
    },
  };
}

const tick = () => new Promise((r) => setImmediate(r));

test('block counts, chain information and fee estimates are cached', async () => {
  let t = 0;
  const bitcoind = fakeBitcoind();
  const proxy = new Proxy(bitcoind, () => t);
  const first = proxy.handle('getblockcount', []);
  await tick();
  bitcoind.calls[0].resolve({ result: 100 });
  assert.deepEqual(await first, { result: 100 });
  assert.deepEqual(await proxy.handle('getblockcount', []), { result: 100 });
  assert.equal(bitcoind.calls.length, 1);
  t += 2_000;
  const again = proxy.handle('getblockcount', []);
  await tick();
  assert.equal(bitcoind.calls.length, 2, 'a count older than 2 seconds is read again');
  bitcoind.calls[1].resolve({ result: 101 });
  assert.deepEqual(await again, { result: 101 });

  const info = proxy.handle('getblockchaininfo', []);
  await tick();
  assert.equal(bitcoind.calls.length, 3);
  assert.equal(bitcoind.calls[2].method, 'getblockchaininfo');
  bitcoind.calls[2].resolve({ result: { chain: 'signet', blocks: 101 } });
  assert.deepEqual(await info, { result: { chain: 'signet', blocks: 101 } });
  assert.deepEqual(await proxy.handle('getblockchaininfo', []), { result: { chain: 'signet', blocks: 101 } });
  assert.equal(bitcoind.calls.length, 3, 'the chain information comes from the cache for 2 seconds');
});

const ADDRESS = `tb1p${'q'.repeat(58)}`;
const GENESIS = '00000008819873e925422c1ff0f99f7cc9bbb232af63a077a480a3633bee1ef6';

test('only the two Esplora paths of the page pass, for signet Taproot addresses', () => {
  assert.equal(esploraPath('/esplora/block-height/0'), '/block-height/0');
  assert.equal(esploraPath(`/esplora/address/${ADDRESS}/utxo`), `/address/${ADDRESS}/utxo`);
  assert.equal(esploraPath(`/esplora/address/${ADDRESS.toUpperCase()}/utxo`), `/address/${ADDRESS}/utxo`);
  const refused = [
    ['/esplora/block-height/1', 404],
    ['/esplora/tx/' + '00'.repeat(32), 404],
    [`/esplora/address/${ADDRESS}/txs`, 404],
    [`/esplora/address/${ADDRESS}/utxo/x`, 404],
    ['/esplora/address/tb1q/utxo', 400],
    [`/esplora/address/tb1q${'q'.repeat(39)}/utxo`, 400],
    [`/esplora/address/tb1p${'q'.repeat(57)}/utxo`, 400],
    [`/esplora/address/tb1p${'q'.repeat(59)}/utxo`, 400],
    [`/esplora/address/bc1p${'q'.repeat(58)}/utxo`, 400],
    [`/esplora/address/tb1p${'b'.repeat(58)}/utxo`, 400],
    [`/esplora/address/tB1p${'q'.repeat(58)}/utxo`, 400],
    ['/esplora/address/mipcBbFg9gMiCh81Kj8tqqdgoZub1ZJRfn/utxo', 400],
    ['/esplora/address/%2e%2e/utxo', 400],
    ['/', 404],
  ];
  for (const [path, status] of refused) {
    assert.throws(() => esploraPath(path), (e) => e.status === status, path);
  }
});

/** An Esplora API whose answers the test gives, counting the requests. */
function fakeEsplora(answer) {
  const asked = [];
  return {
    asked,
    async get(path) {
      asked.push(path);
      return answer(path);
    },
  };
}

test('Esplora answers are passed on and cached: the genesis hash for good, coins for 5 seconds', async () => {
  let t = 0;
  const coins = JSON.stringify([{ txid: '11'.repeat(32), vout: 0, value: 5000, status: { confirmed: true, block_height: 9 } }]);
  const esplora = fakeEsplora((path) =>
    path === '/block-height/0' ? { status: 200, body: GENESIS } : { status: 200, body: coins },
  );
  const proxy = new Proxy(fakeBitcoind(), () => t, esplora);
  assert.deepEqual(await proxy.esplora('/block-height/0'), { status: 200, type: 'text/plain', body: GENESIS });
  const utxo = `/address/${ADDRESS}/utxo`;
  assert.deepEqual(await proxy.esplora(utxo), { status: 200, type: 'application/json', body: coins });
  t += 4_999;
  await proxy.esplora(utxo);
  assert.equal(esplora.asked.length, 2);
  t += 1;
  await proxy.esplora(utxo);
  assert.equal(esplora.asked.length, 3, 'coins older than 5 seconds are asked again');
  t += 86_400_000;
  await proxy.esplora('/block-height/0');
  assert.equal(esplora.asked.length, 3, 'the genesis hash is kept');

  // the same request while one is under way shares it
  let release;
  const slow = fakeEsplora(() => new Promise((r) => (release = () => r({ status: 200, body: '[]' }))));
  const shared = new Proxy(fakeBitcoind(), () => t, slow);
  const both = [shared.esplora(utxo), shared.esplora(utxo)];
  await tick();
  release();
  assert.deepEqual((await Promise.all(both)).map((a) => a.body), ['[]', '[]']);
  assert.equal(slow.asked.length, 1);
});

test("answers carry the proxy's content types, never mempool.space's", async () => {
  // a JSON array served as a page would run in the wallet page's origin
  const body = '["<script>alert(1)</script>"]';
  const html = async () => new Response(body, { status: 200, headers: { 'content-type': 'text/html' } });
  const client = esploraClient('https://esplora.test', html);
  assert.deepEqual(await client.get('/x'), { status: 200, body });
  const proxy = new Proxy(fakeBitcoind(), () => 0, client);
  assert.deepEqual(await proxy.esplora(`/address/${ADDRESS}/utxo`), { status: 200, type: 'application/json', body });
});

test('failed or unexpected Esplora answers are errors and are not cached', async () => {
  const answers = [
    { status: 200, body: 'not a hash' },
    { status: 200, body: '{"error":1}' },
    { status: 400, body: 'Invalid Bitcoin address' },
    { status: 503, body: 'down' },
  ];
  let next = 0;
  const esplora = fakeEsplora(() => answers[next++]);
  const proxy = new Proxy(fakeBitcoind(), () => 0, esplora);
  const utxo = `/address/${ADDRESS}/utxo`;
  await assert.rejects(proxy.esplora('/block-height/0'), (e) => e.status === 502);
  await assert.rejects(proxy.esplora(utxo), (e) => e.status === 502);
  await assert.rejects(proxy.esplora(utxo), (e) => e.status === 400);
  await assert.rejects(proxy.esplora(utxo), (e) => e.status === 502);
  const unreachable = new Proxy(fakeBitcoind(), () => 0, {
    async get() {
      throw new Error('getaddrinfo ENOTFOUND mempool.space');
    },
  });
  await assert.rejects(unreachable.esplora(utxo), (e) => e.status === 502 && /unavailable/.test(e.message));
});

test('requests to the Esplora API are limited for everyone together', async () => {
  const esplora = fakeEsplora(() => ({ status: 200, body: '[]' }));
  const proxy = new Proxy(fakeBitcoind(), () => 0, esplora);
  const addresses = Array.from({ length: 21 }, (_, i) => `/address/tb1p${String.fromCharCode(113 + (i % 2)).repeat(56)}${'qpzry9x8gf2tvdw0s3jn54khce6mua7l'[i]}${'q'}/utxo`);
  for (const path of addresses.slice(0, 20)) await proxy.esplora(path);
  await assert.rejects(proxy.esplora(addresses[20]), (e) => e.status === 503);
  assert.equal(esplora.asked.length, 20);
});
