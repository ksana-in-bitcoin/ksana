/**
 * Test utilities: loading the shared vectors (`tests/vectors/` at the repository root) and the official BIP322 vectors
 * (`sdk/ts/test/data/bip322/`).
 *
 * Paths are resolved relative to the compiled test file (`sdk/ts/dist/test/*.js`), independent of the current working
 * directory.
 */

import { readFileSync } from 'node:fs';

import { createBase58check } from '@scure/base';
import { sha256 } from '@noble/hashes/sha2.js';

/** A file under the repository root. */
export function repoFile(relative: string): URL {
  return new URL(`../../../../${relative}`, import.meta.url);
}

/** Load `tests/vectors/<name>`. */
export function loadVectors(name: string): any {
  return JSON.parse(readFileSync(repoFile(`tests/vectors/${name}`), 'utf8'));
}

/** Load `sdk/ts/test/data/bip322/<name>`. */
export function loadBip322Official(name: string): any {
  return JSON.parse(readFileSync(new URL(`../../test/data/bip322/${name}`, import.meta.url), 'utf8'));
}

/** Decode a WIF secret key (mainnet 0x80 / testnet 0xef, with an optional compression flag). */
export function decodeWif(wif: string): Uint8Array {
  const payload = createBase58check(sha256).decode(wif);
  if (payload[0] !== 0x80 && payload[0] !== 0xef) throw new Error('not a WIF secret key');
  if (payload.length === 34 && payload[33] === 0x01) return payload.slice(1, 33);
  if (payload.length === 33) return payload.slice(1);
  throw new Error('invalid WIF length');
}

/** Deterministic pseudo-random bytes (xorshift32), for tests. */
export function pseudoRandomBytes(seed: number, length: number): Uint8Array {
  let x = seed >>> 0 || 1;
  const out = new Uint8Array(length);
  for (let i = 0; i < length; i++) {
    x ^= x << 13;
    x >>>= 0;
    x ^= x >>> 17;
    x ^= x << 5;
    x >>>= 0;
    out[i] = x & 0xff;
  }
  return out;
}

/** A secret key made of one byte repeated 32 times (for tests). */
export function testKey(byte: number): Uint8Array {
  return new Uint8Array(32).fill(byte);
}
