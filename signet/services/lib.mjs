// Helpers shared by the public signet's services (btc-proxy.mjs, faucet.mjs; D-107): request bodies, client
// addresses behind Caddy, per-client rate limits, JSON responses and a page of another origin. No dependencies beyond
// Node.js.

/** Reads a request body of at most `limit` bytes; rejects with `status` 413 beyond it. */
export function readBody(req, limit) {
  return new Promise((resolve, reject) => {
    const chunks = [];
    let size = 0;
    req.on('data', (chunk) => {
      size += chunk.length;
      if (size > limit) {
        reject(Object.assign(new Error(`request body over ${limit} bytes`), { status: 413 }));
        req.destroy();
        return;
      }
      chunks.push(chunk);
    });
    req.on('end', () => resolve(Buffer.concat(chunks).toString('utf8')));
    req.on('error', reject);
  });
}

/**
 * The client's address: the first entry of `X-Forwarded-For`, which Caddy sets to the address the request came from
 * (the services are reachable only through Caddy), else the socket's address.
 */
export function clientAddress(req) {
  const forwarded = req.headers['x-forwarded-for'];
  const first = (Array.isArray(forwarded) ? forwarded[0] : forwarded)?.split(',')[0]?.trim();
  return first || req.socket.remoteAddress || 'unknown';
}

/**
 * Token buckets per key: `capacity` requests at once, refilled at `perMinute` a minute. `take(key)` spends one token
 * and returns whether there was one; `wait(key)` is how long until it would find one. Buckets that are full again are
 * forgotten.
 */
export class RateLimit {
  constructor(capacity, perMinute, now = () => Date.now()) {
    this.capacity = capacity;
    this.perMs = perMinute / 60_000;
    this.now = now;
    this.buckets = new Map();
  }

  take(key) {
    const t = this.now();
    const bucket = this.buckets.get(key) ?? { tokens: this.capacity, at: t };
    bucket.tokens = Math.min(this.capacity, bucket.tokens + (t - bucket.at) * this.perMs);
    bucket.at = t;
    if (bucket.tokens < 1) {
      this.buckets.set(key, bucket);
      return false;
    }
    bucket.tokens -= 1;
    this.buckets.set(key, bucket);
    if (this.buckets.size > 10_000) this.sweep(t);
    return true;
  }

  /** Milliseconds until `take(key)` finds a token; 0 when it would now. */
  wait(key) {
    const bucket = this.buckets.get(key);
    if (!bucket) return 0;
    const t = this.now();
    const tokensAt = (when) => Math.min(this.capacity, bucket.tokens + (when - bucket.at) * this.perMs);
    if (tokensAt(t) >= 1) return 0;
    let ms = Math.ceil((1 - tokensAt(t)) / this.perMs);
    // take() compares the same sum, which can round to just below 1 at the exact moment (0.9999999999999999 from an
    // empty bucket after exactly 288 minutes at five a day): step past it
    while (tokensAt(t + ms) < 1) ms += 1;
    return ms;
  }

  sweep(t = this.now()) {
    for (const [key, bucket] of this.buckets) {
      if (bucket.tokens + (t - bucket.at) * this.perMs >= this.capacity) this.buckets.delete(key);
    }
  }
}

/** Writes a JSON response. */
export function sendJson(res, status, value) {
  const body = JSON.stringify(value);
  res.writeHead(status, { 'content-type': 'application/json', 'cache-control': 'no-store' });
  res.end(body);
}

/**
 * A request listener that lets the page of one other origin call `listener` from a browser (CORS), as the contract
 * playground's page calls the faucet (D-119): every answer allows `origin`, and a preflight gets 204 for GET and POST
 * with a JSON body. Without an origin, `listener` itself.
 */
export function allowOrigin(origin, listener) {
  if (!origin) return listener;
  return (req, res) => {
    res.setHeader('access-control-allow-origin', origin);
    res.setHeader('vary', 'origin');
    if (req.method === 'OPTIONS') {
      res.writeHead(204, {
        'access-control-allow-methods': 'GET, POST',
        'access-control-allow-headers': 'content-type',
        'access-control-max-age': '600',
      });
      res.end();
      return;
    }
    return listener(req, res);
  };
}

/** An environment variable, or `fallback`; exits when it is required and missing. */
export function env(name, fallback) {
  const value = process.env[name];
  if (value !== undefined && value !== '') return value;
  if (fallback === undefined) {
    console.error(`${name} is not set`);
    process.exit(2);
  }
  return fallback;
}

/** Logs one line with a UTC timestamp. */
export function log(service, message) {
  console.log(`${new Date().toISOString()} [${service}] ${message}`);
}
