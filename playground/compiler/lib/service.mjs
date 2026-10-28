// The compile service's HTTP side (playground/compiler/README.md): POST / builds a source, GET /health says what it
// builds; a queue in front of the slots, and a limit of builds per address.
import { MAX_SOURCE_BYTES } from './build.mjs';

/** The most a request body may hold: a source at its limit, escaped as JSON */
export const MAX_BODY_BYTES = 2 * MAX_SOURCE_BYTES + 1024;

export class BusyError extends Error {
  constructor() {
    super('busy');
    this.name = 'BusyError';
  }
}

/** Builds by address: `perMinute` a minute on average, at most `burst` at once (a token bucket) */
export class Limiter {
  constructor({ perMinute, burst = perMinute }) {
    this.rate = perMinute / 60_000;
    this.burst = burst;
    this.buckets = new Map();
  }

  /** Takes one build for `key` at time `now` (ms); false when the address has none left */
  take(key, now = Date.now()) {
    if (this.rate <= 0) return true;
    let b = this.buckets.get(key);
    if (!b) {
      b = { tokens: this.burst, at: now };
      this.buckets.set(key, b);
    }
    b.tokens = Math.min(this.burst, b.tokens + (now - b.at) * this.rate);
    b.at = now;
    if (this.buckets.size > 10_000) this.sweep(now);
    if (b.tokens < 1) return false;
    b.tokens -= 1;
    return true;
  }

  /** Seconds until `key` has a build again */
  retryAfter(key) {
    const b = this.buckets.get(key);
    if (!b || this.rate <= 0) return 0;
    return Math.max(1, Math.ceil((1 - b.tokens) / this.rate / 1000));
  }

  /** Forgets the addresses whose buckets are full again */
  sweep(now = Date.now()) {
    for (const [key, b] of this.buckets) {
      if (b.tokens + (now - b.at) * this.rate >= this.burst) this.buckets.delete(key);
    }
  }
}

/** The slots' builders, each taking one build at a time, and at most `maxQueue` builds waiting for one */
export class Pool {
  constructor(builders, maxQueue) {
    this.free = [...builders];
    this.size = builders.length;
    this.maxQueue = maxQueue;
    this.waiting = [];
  }

  get busy() {
    return this.size - this.free.length;
  }

  get queued() {
    return this.waiting.length;
  }

  /** Runs `job(builder)` on a free builder; throws BusyError when the queue is full, AbortError when aborted while waiting */
  async run(job, signal) {
    let builder = this.free.pop();
    if (!builder) {
      if (this.waiting.length >= this.maxQueue) throw new BusyError();
      builder = await new Promise((resolve, reject) => {
        const entry = { resolve, reject };
        this.waiting.push(entry);
        signal?.addEventListener('abort', () => {
          const i = this.waiting.indexOf(entry);
          if (i >= 0) {
            this.waiting.splice(i, 1);
            reject(new DOMException('aborted', 'AbortError'));
          }
        }, { once: true });
      });
    }
    try {
      return await job(builder);
    } finally {
      const next = this.waiting.shift();
      if (next) next.resolve(builder);
      else this.free.push(builder);
    }
  }
}

class HttpError extends Error {
  constructor(status, message, headers = {}) {
    super(message);
    this.status = status;
    this.headers = headers;
  }
}

function readBody(req, limit) {
  return new Promise((resolve, reject) => {
    const declared = Number(req.headers['content-length']);
    if (Number.isFinite(declared) && declared > limit) {
      reject(new HttpError(413, 'the source is too large'));
      return;
    }
    const chunks = [];
    let size = 0;
    req.on('data', (chunk) => {
      size += chunk.length;
      if (size > limit) {
        reject(new HttpError(413, 'the source is too large'));
        req.destroy();
        return;
      }
      chunks.push(chunk);
    });
    req.on('end', () => resolve(Buffer.concat(chunks)));
    req.on('error', reject);
  });
}

/** The address a request comes from: Caddy's X-Forwarded-For when the service sits behind it, else the socket's */
export function clientOf(req, trustProxy) {
  if (trustProxy) {
    const forwarded = req.headers['x-forwarded-for'];
    const first = (Array.isArray(forwarded) ? forwarded[0] : forwarded)?.split(',')[0]?.trim();
    if (first) return first;
  }
  return req.socket.remoteAddress ?? 'unknown';
}

/**
 * The request handler.
 *
 * @param {{ pool: Pool, limiter: Limiter, crates: string[], info: () => object, trustProxy?: boolean, cors?: string,
 *   log?: (line: string) => void }} options
 */
export function createHandler({ pool, limiter, crates, info, trustProxy = false, cors = '', log = () => {} }) {
  // every answer carries them, a build's included, so a page of another origin reads it
  const corsHeaders = cors ? { 'access-control-allow-origin': cors, vary: 'origin' } : {};
  const send = (res, status, body, headers = {}) => {
    const text = JSON.stringify(body);
    res.writeHead(status, {
      'content-type': 'application/json',
      'cache-control': 'no-store',
      'x-content-type-options': 'nosniff',
      'content-length': Buffer.byteLength(text),
      ...corsHeaders,
      ...headers,
    });
    res.end(text);
  };

  const compile = async (req, res) => {
    const type = String(req.headers['content-type'] ?? '');
    if (!/^application\/json\b/i.test(type)) throw new HttpError(415, 'send the source as application/json');
    const client = clientOf(req, trustProxy);
    if (!limiter.take(client)) {
      throw new HttpError(429, 'too many builds from this address: wait a minute', { 'retry-after': String(limiter.retryAfter(client)) });
    }
    const body = await readBody(req, MAX_BODY_BYTES);
    let request;
    try {
      request = JSON.parse(body.toString('utf8'));
    } catch {
      throw new HttpError(400, 'the body is not JSON');
    }
    const { source, crate } = request ?? {};
    if (typeof source !== 'string' || source.trim() === '') throw new HttpError(400, 'no source');
    if (Buffer.byteLength(source) > MAX_SOURCE_BYTES) throw new HttpError(413, 'the source is too large');
    if (typeof crate !== 'string' || !crates.includes(crate)) throw new HttpError(400, `crate is one of ${crates.join(', ')}`);
    const controller = new AbortController();
    // a visitor who leaves (or compiles again) frees the slot
    res.on('close', () => {
      if (!res.writableFinished) controller.abort();
    });
    const started = Date.now();
    let result;
    try {
      result = await pool.run((builder) => builder.build(crate, source, controller.signal), controller.signal);
    } catch (e) {
      if (e instanceof BusyError) throw new HttpError(503, 'busy: every build slot is taken, try again in a moment', { 'retry-after': '5' });
      if (e?.name === 'AbortError') return;
      throw e;
    }
    if (result.aborted || res.destroyed) return;
    const ms = Date.now() - started;
    const errors = result.diagnostics.filter((d) => d.level === 'error').length;
    log(`build ${crate}: ${result.ok ? `ok, ${result.wasm.length} bytes` : `failed, ${errors} error(s)`} in ${ms} ms`);
    send(res, 200, result.ok
      ? { ok: true, wasm: result.wasm.toString('base64'), sha256: result.sha256, diagnostics: result.diagnostics, ms }
      : { ok: false, diagnostics: result.diagnostics, ms });
  };

  return async (req, res) => {
    try {
      const url = new URL(req.url ?? '/', 'http://localhost');
      // the same paths with or without a /compile prefix
      const route = url.pathname.replace(/^\/compile(?=\/|$)/, '') || '/';
      if (req.method === 'OPTIONS' && cors) {
        res.writeHead(204, { ...corsHeaders, 'access-control-allow-methods': 'GET, POST', 'access-control-allow-headers': 'content-type', 'access-control-max-age': '600' });
        res.end();
        return;
      }
      if (route === '/health' && (req.method === 'GET' || req.method === 'HEAD')) {
        send(res, 200, { ok: true, ...info(), busy: pool.busy, queued: pool.queued });
        return;
      }
      if (route === '/' && req.method === 'POST') {
        await compile(req, res);
        return;
      }
      throw new HttpError(404, 'POST / builds a source; GET /health says what this service builds');
    } catch (e) {
      if (res.headersSent || res.destroyed) return;
      if (e instanceof HttpError) {
        send(res, e.status, { error: e.message }, e.headers);
        return;
      }
      log(`internal error: ${e instanceof Error ? e.stack ?? e.message : String(e)}`);
      send(res, 500, { error: 'the build failed inside the service' });
    }
  };
}
