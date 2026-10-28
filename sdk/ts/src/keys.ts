/**
 * secp256k1 keys: x-only public keys, compressed public keys, the BIP86 / BIP341 Taproot tweak, and Ksana accounts (SPEC §4,
 * D-092).
 *
 * The account is the 32-byte output key of the secret key's Taproot address (BIP86 key path, no script tree):
 * `accountOf(d) = taprootOutputKey(xOnlyPublicKey(d))`. Signatures are verified with the account as the public key, so BIP340
 * signing uses the tweaked secret key `taprootTweakSecretKey(d)`.
 */

import { schnorr, secp256k1 } from '@noble/curves/secp256k1.js';
import { bytesToNumberBE, numberToBytesBE } from '@noble/curves/utils.js';

import { assertBytes } from './bytes.js';
import { taggedHash } from './hash.js';

const Point = secp256k1.Point;
/** The curve order n. */
export const CURVE_ORDER: bigint = Point.Fn.ORDER;

/** Whether the secret key is within `[1, n-1]`. */
export function isValidSecretKey(secretKey: Uint8Array): boolean {
  if (!(secretKey instanceof Uint8Array) || secretKey.length !== 32) return false;
  const d = bytesToNumberBE(secretKey);
  return d > 0n && d < CURVE_ORDER;
}

function secretScalar(secretKey: Uint8Array): bigint {
  assertBytes(secretKey, 32, 'secret key');
  const d = bytesToNumberBE(secretKey);
  if (d <= 0n || d >= CURVE_ORDER) throw new Error('invalid secret key (not within [1, n-1])');
  return d;
}

/** The x-only public key of a secret key (32 bytes): the Taproot internal key (the account is its BIP86-tweaked output key, `accountOf`). */
export function xOnlyPublicKey(secretKey: Uint8Array): Uint8Array {
  secretScalar(secretKey);
  return schnorr.getPublicKey(secretKey);
}

/**
 * The Ksana account of a secret key (D-092): the BIP86-tweaked Taproot output key (32 bytes), that is, the witness program in
 * the secret key's `bc1p…` address; `accountToAddress` writes it as an address.
 */
export function accountOf(secretKey: Uint8Array): Uint8Array {
  return taprootOutputKey(xOnlyPublicKey(secretKey));
}

/** The 33-byte compressed public key of a secret key. */
export function compressedPublicKey(secretKey: Uint8Array): Uint8Array {
  secretScalar(secretKey);
  return secp256k1.getPublicKey(secretKey, true);
}

/** Whether this is a valid x-only public key (the x coordinate of a point on the curve). */
export function isValidXOnlyPublicKey(key: Uint8Array): boolean {
  if (!(key instanceof Uint8Array) || key.length !== 32) return false;
  try {
    schnorr.utils.lift_x(bytesToNumberBE(key));
    return true;
  } catch {
    return false;
  }
}

/** Whether this is a valid 33-byte compressed public key. */
export function isValidCompressedPublicKey(key: Uint8Array): boolean {
  if (!(key instanceof Uint8Array) || key.length !== 33 || (key[0] !== 0x02 && key[0] !== 0x03)) return false;
  try {
    Point.fromBytes(key);
    return true;
  } catch {
    return false;
  }
}

/** The x-only public key (x coordinate) from a 33-byte compressed or a 32-byte x-only public key. */
export function toXOnly(publicKey: Uint8Array): Uint8Array {
  if (publicKey.length === 32) {
    if (!isValidXOnlyPublicKey(publicKey)) throw new Error('invalid x-only public key');
    return Uint8Array.from(publicKey);
  }
  if (publicKey.length === 33) {
    if (!isValidCompressedPublicKey(publicKey)) throw new Error('invalid compressed public key');
    return Uint8Array.from(publicKey.subarray(1));
  }
  throw new Error(`the public key must be 33 bytes (compressed) or 32 bytes (x-only), got ${publicKey.length} bytes`);
}

/** The BIP341 `TapTweak` scalar (no script tree, BIP86). */
function tweakScalar(internalKey: Uint8Array): bigint {
  const t = bytesToNumberBE(taggedHash('TapTweak', internalKey));
  if (t >= CURVE_ORDER) throw new Error('TapTweak exceeds the curve order');
  return t;
}

/** The output key (x-only, 32 bytes) after the BIP86 tweak (no script tree) with `internalKey` as the internal key. */
export function taprootOutputKey(internalKey: Uint8Array): Uint8Array {
  assertBytes(internalKey, 32, 'internal key');
  let p;
  try {
    p = schnorr.utils.lift_x(bytesToNumberBE(internalKey));
  } catch {
    throw new Error('the internal key is not a valid x-only public key');
  }
  const t = tweakScalar(internalKey);
  const q = t === 0n ? p : p.add(Point.BASE.multiply(t));
  if (q.equals(Point.ZERO)) throw new Error('the tweaked public key is the point at infinity');
  return numberToBytesBE(q.x, 32);
}

/** BIP86-tweak a secret key (no script tree): `d' = (±d + t) mod n`, where `±` makes the y of the internal key even. */
export function taprootTweakSecretKey(secretKey: Uint8Array): Uint8Array {
  const d0 = secretScalar(secretKey);
  const p = Point.BASE.multiply(d0);
  const d = p.y % 2n === 0n ? d0 : CURVE_ORDER - d0;
  const t = tweakScalar(numberToBytesBE(p.x, 32));
  const tweaked = (d + t) % CURVE_ORDER;
  if (tweaked === 0n) throw new Error('the tweaked secret key is 0');
  return numberToBytesBE(tweaked, 32);
}
