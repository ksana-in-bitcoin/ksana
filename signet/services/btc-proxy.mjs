// BTC proxy of the public signet (D-107): the web wallet's path C and top-ups read and write Bitcoin through
// bitcoind's JSON-RPC (src/wallet/btc.ts of ksana-web), which needs credentials and sends no CORS headers, so the hosted page
// reaches it through this proxy at /btc. Four of the methods the wallet calls pass, with their parameters checked:
//
//   getblockcount                        cached for 2 seconds
//   getblockchaininfo                    cached for 2 seconds; the page reads its chain before it lists coins or
//                                        broadcasts, to refuse a bitcoind on another BTC network (A2-11, D-106)
//   estimatesmartfee <1..1008> [mode]    cached for 60 seconds per target and mode
//   sendrawtransaction <hex>             at most 400 000 bytes, bitcoind's default fee rate cap
//
// The signet page lists an address's coins from an Esplora API instead (mempool.space's, ESPLORA_URL; D-108), and
// scantxoutset, its fallback when that fails, is refused: one scan reads signet's whole UTXO set for 30 to 50 seconds
// and bitcoind runs one at a time, so anyone could keep that one scan, and the server's disk, busy with a request every
// half minute (D-107, D-109). Two GET paths pass, unchanged:
//
//   GET esplora/block-height/0           the genesis block's hash, which the page checks is signet's; kept once read
//   GET esplora/address/<tb1p…>/utxo     the coins of a signet Taproot address, the page's accounts (D-092);
//                                        cached for 5 seconds (D-108: a few seconds at most)
//
// Per client address (X-Forwarded-For from Caddy): 120 requests a minute, 20 broadcasts a minute and 30 Esplora
// requests a minute; Esplora requests that reach mempool.space, from everyone together, 2 a second.
//
//   BTC_RPC_URL=http://bitcoind:38332 BTC_RPC_USER=ksana BTC_RPC_PASS=... PORT=8080 node signet/services/btc-proxy.mjs
import http from 'node:http';
import { RateLimit, clientAddress, env, log, readBody, sendJson } from './lib.mjs';

const MAX_BODY = 1_000_000;
const MAX_TX_BYTES = 400_000;
const COUNT_CACHE_MS = 2_000;
const FEE_CACHE_MS = 60_000;
const ESPLORA_URL = 'https://mempool.space/signet/api';
const ESPLORA_TIMEOUT_MS = 15_000;
const UTXO_CACHE_MS = 5_000;
const GENESIS_PATH = '/block-height/0';
/** A signet Taproot address (witness version 1, a 32-byte program): the only addresses the page lists coins of. */
const SIGNET_TAPROOT = /^tb1p[qpzry9x8gf2tvdw0s3jn54khce6mua7l]{58}$/;

/** A JSON-RPC error: `code` and `message` go to the client. */
class RpcFailure extends Error {
  constructor(code, message) {
    super(message);
    this.code = code;
  }
}

/**
 * Checks a request against the allowed methods and returns the parameters to send to bitcoind (normalized); throws
 * `RpcFailure` for anything else.
 */
export function checkRequest(method, params) {
  const list = params === undefined || params === null ? [] : params;
  if (!Array.isArray(list)) throw new RpcFailure(-32602, 'params must be an array');
  switch (method) {
    case 'getblockcount':
    case 'getblockchaininfo':
      if (list.length !== 0) throw new RpcFailure(-32602, `${method} takes no parameters`);
      return [];
    case 'estimatesmartfee': {
      const [target, mode] = list;
      if (list.length < 1 || list.length > 2 || !Number.isInteger(target) || target < 1 || target > 1008) {
        throw new RpcFailure(-32602, 'estimatesmartfee takes a confirmation target of 1 to 1008 blocks');
      }
      if (mode === undefined) return [target];
      if (typeof mode !== 'string' || !['unset', 'economical', 'conservative'].includes(mode.toLowerCase())) {
        throw new RpcFailure(-32602, 'estimatesmartfee mode must be unset, economical or conservative');
      }
      return [target, mode.toLowerCase()];
    }
    case 'scantxoutset':
      // the page's fallback when Esplora gives no answer: the page shows this message instead of the coins
      throw new RpcFailure(
        -32601,
        `scantxoutset is not available here (a scan of signet's UTXO set takes 30 to 50 seconds): coins are listed ` +
          'through Esplora, which did not answer; try again shortly',
      );
    case 'sendrawtransaction': {
      const [hex] = list;
      if (list.length !== 1 || typeof hex !== 'string' || hex.length % 2 !== 0 || !/^[0-9a-fA-F]+$/.test(hex)) {
        throw new RpcFailure(-32602, 'sendrawtransaction takes one transaction in hex');
      }
      if (hex.length / 2 > MAX_TX_BYTES) throw new RpcFailure(-32602, `transactions are limited to ${MAX_TX_BYTES} bytes`);
      return [hex];
    }
    default:
      throw new RpcFailure(-32601, `method ${String(method)} is not available here`);
  }
}

/** An answer to an HTTP request that is not JSON-RPC (the Esplora paths): `status` and a plain text message. */
class HttpFailure extends Error {
  constructor(status, message) {
    super(message);
    this.status = status;
  }
}

/**
 * The Esplora path for a GET below the proxy (`pathname` as the proxy sees it, `/esplora/...`), or `HttpFailure` for
 * anything but the two shapes the page uses.
 */
export function esploraPath(pathname) {
  if (pathname === `/esplora${GENESIS_PATH}`) return GENESIS_PATH;
  const match = /^\/esplora\/address\/([^/]+)\/utxo$/.exec(pathname);
  if (!match) throw new HttpFailure(404, 'not found');
  // bech32 is all lower case or all upper case
  const address = match[1] === match[1].toUpperCase() ? match[1].toLowerCase() : match[1];
  if (!SIGNET_TAPROOT.test(address)) throw new HttpFailure(400, 'not a signet Taproot address');
  return `/address/${address}/utxo`;
}

/** The Esplora API: `get` returns the status and body of a GET of `path`. */
export function esploraClient(base, fetchFn = fetch) {
  return {
    async get(path) {
      const res = await fetchFn(`${base}${path}`, { signal: AbortSignal.timeout(ESPLORA_TIMEOUT_MS) });
      return { status: res.status, body: await res.text() };
    },
  };
}

/** Whether a 200 answer to `path` has the shape the page expects: a block hash, or a JSON array of coins. */
function esploraAnswerFits(path, body) {
  if (path === GENESIS_PATH) return /^[0-9a-f]{64}$/.test(body.trim());
  try {
    return Array.isArray(JSON.parse(body));
  } catch {
    return false;
  }
}

/** The bitcoind connection: `call` returns bitcoind's JSON-RPC response object (`result` or `error`). */
export function bitcoindClient(url, user, pass, fetchFn = fetch) {
  const authorization = `Basic ${Buffer.from(`${user}:${pass}`).toString('base64')}`;
  return {
    async call(method, params) {
      const res = await fetchFn(url, {
        method: 'POST',
        headers: { authorization, 'content-type': 'application/json' },
        body: JSON.stringify({ jsonrpc: '2.0', id: 'btc-proxy', method, params }),
        signal: AbortSignal.timeout(60_000),
      });
      const text = await res.text();
      try {
        return JSON.parse(text);
      } catch {
        throw new Error(`bitcoind ${method}: HTTP ${res.status}`);
      }
    },
  };
}

/** Caching in front of bitcoind and the Esplora API. */
export class Proxy {
  constructor(bitcoind, now = () => Date.now(), esplora = esploraClient(ESPLORA_URL)) {
    this.bitcoind = bitcoind;
    this.now = now;
    this.esploraApi = esplora;
    this.cache = new Map();
    this.upstream = new RateLimit(20, 120, now);
    this.inflight = new Map();
  }

  /**
   * The Esplora API's answer to `path` (from `esploraPath`): `{ status, type, body }`, with the proxy's own content type
   * for each path whatever mempool.space sent, so that no answer is ever served as a page. A 200 answer of the expected
   * shape is cached, the genesis hash for good; requests for the same path while one is under way share it. Throws
   * `HttpFailure` when mempool.space cannot be reached, answers something else, or the proxy's own budget for it is
   * spent; the page then falls back to scantxoutset, which `checkRequest` refuses with the reason.
   */
  async esplora(path) {
    const key = `esplora ${path}`;
    const hit = this.cached(key, path === GENESIS_PATH ? Infinity : UTXO_CACHE_MS);
    if (hit) return hit;
    const pending = this.inflight.get(key);
    if (pending) return pending;
    if (!this.upstream.take('all')) throw new HttpFailure(503, 'the Esplora API is busy here; try again shortly');
    const run = (async () => {
      let answer;
      try {
        answer = await this.esploraApi.get(path);
      } catch (e) {
        throw new HttpFailure(502, `the Esplora API is unavailable: ${e?.message ?? e}`);
      }
      if (answer.status !== 200) {
        throw new HttpFailure(answer.status >= 500 ? 502 : answer.status, answer.body.slice(0, 200));
      }
      if (!esploraAnswerFits(path, answer.body)) throw new HttpFailure(502, 'unexpected answer from the Esplora API');
      const checked = { status: 200, type: path === GENESIS_PATH ? 'text/plain' : 'application/json', body: answer.body };
      this.remember(key, checked);
      return checked;
    })();
    this.inflight.set(key, run);
    try {
      return await run;
    } finally {
      this.inflight.delete(key);
    }
  }

  cached(key, maxAge) {
    const hit = this.cache.get(key);
    return hit && this.now() - hit.at < maxAge ? hit.response : undefined;
  }

  remember(key, response) {
    if (this.cache.size > 5_000) this.cache.clear();
    this.cache.set(key, { at: this.now(), response });
  }

  /** bitcoind's response to an allowed request (`params` already checked). */
  async handle(method, params) {
    switch (method) {
      case 'getblockcount':
      case 'getblockchaininfo':
      case 'estimatesmartfee': {
        const key = JSON.stringify([method, params]);
        const hit = this.cached(key, method === 'estimatesmartfee' ? FEE_CACHE_MS : COUNT_CACHE_MS);
        if (hit) return hit;
        const response = await this.bitcoind.call(method, params);
        if (!response.error) this.remember(key, response);
        return response;
      }
      default:
        return this.bitcoind.call(method, params);
    }
  }
}

/** Writes a plain text response. */
function sendText(res, status, type, body) {
  res.writeHead(status, { 'content-type': type, 'x-content-type-options': 'nosniff', 'cache-control': 'no-store' });
  res.end(body);
}

function serve() {
  const port = Number(env('PORT', '8080'));
  const bitcoind = bitcoindClient(env('BTC_RPC_URL', 'http://bitcoind:38332'), env('BTC_RPC_USER', 'ksana'), env('BTC_RPC_PASS'));
  const proxy = new Proxy(bitcoind, undefined, esploraClient(env('ESPLORA_URL', ESPLORA_URL)));
  const requests = new RateLimit(120, 120);
  const broadcasts = new RateLimit(20, 20);
  const esploras = new RateLimit(30, 30);

  const server = http.createServer(async (req, res) => {
    if (req.method === 'OPTIONS') {
      res.writeHead(204, { allow: 'GET, POST' });
      res.end();
      return;
    }
    if (req.method === 'GET') {
      const client = clientAddress(req);
      try {
        const path = esploraPath(new URL(req.url ?? '/', 'http://proxy').pathname);
        if (!requests.take(client) || !esploras.take(client)) {
          throw new HttpFailure(429, 'too many requests from this address; slow down');
        }
        const answer = await proxy.esplora(path);
        sendText(res, 200, answer.type, answer.body);
      } catch (e) {
        if (!(e instanceof HttpFailure)) log('btc-proxy', `${client} GET ${req.url}: ${e?.message ?? e}`);
        sendText(res, e instanceof HttpFailure ? e.status : 502, 'text/plain', e instanceof HttpFailure ? e.message : 'unavailable');
      }
      return;
    }
    if (req.method !== 'POST') {
      sendJson(res, 405, { error: 'POST a JSON-RPC request, or GET an Esplora path' });
      return;
    }
    const client = clientAddress(req);
    let id = null;
    let method = '?';
    try {
      if (!requests.take(client)) throw new RpcFailure(-32029, 'too many requests from this address; slow down');
      const request = JSON.parse(await readBody(req, MAX_BODY));
      if (typeof request !== 'object' || request === null || Array.isArray(request)) {
        throw new RpcFailure(-32600, 'send one JSON-RPC request (no batches)');
      }
      id = request.id ?? null;
      method = String(request.method);
      const params = checkRequest(request.method, request.params);
      if (method === 'sendrawtransaction' && !broadcasts.take(client)) throw new RpcFailure(-32029, 'too many broadcasts from this address');
      const response = await proxy.handle(method, params);
      const body = response.error ? { jsonrpc: '2.0', id, error: response.error } : { jsonrpc: '2.0', id, result: response.result };
      if (method === 'sendrawtransaction') log('btc-proxy', `${client} sendrawtransaction: ${response.error ? `refused: ${response.error.message}` : response.result}`);
      sendJson(res, 200, body);
    } catch (e) {
      if (e instanceof RpcFailure) {
        sendJson(res, e.code === -32029 ? 429 : 200, { jsonrpc: '2.0', id, error: { code: e.code, message: e.message } });
      } else if (e?.status === 413) {
        sendJson(res, 413, { jsonrpc: '2.0', id, error: { code: -32600, message: e.message } });
      } else if (e instanceof SyntaxError) {
        sendJson(res, 200, { jsonrpc: '2.0', id, error: { code: -32700, message: 'the request is not JSON' } });
      } else {
        log('btc-proxy', `${client} ${method}: ${e?.message ?? e}`);
        sendJson(res, 502, { jsonrpc: '2.0', id, error: { code: -32603, message: 'bitcoind is unavailable' } });
      }
    }
  });
  server.listen(port, () => log('btc-proxy', `listening on ${port}`));
}

if (import.meta.url === `file://${process.argv[1]}`) serve();
