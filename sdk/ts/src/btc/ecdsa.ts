/**
 * ECDSA (secp256k1): RFC 6979 deterministic, low-S DER signatures, and the strict DER check (BIP66).
 */

import { secp256k1 } from '@noble/curves/secp256k1.js';

const HALF_ORDER = secp256k1.Point.Fn.ORDER >> 1n;

/**
 * ECDSA-signs a 32-byte digest and returns the DER encoding (without the sighash type byte). RFC 6979 deterministic (no extra
 * entropy, no low-R grinding), low S: byte-for-byte identical to libsecp256k1's `secp256k1_ecdsa_sign` (rust-secp256k1's
 * `sign_ecdsa`).
 */
export function ecdsaSignDer(digest: Uint8Array, secretKey: Uint8Array): Uint8Array {
  if (digest.length !== 32) throw new Error('the digest of an ECDSA signature must be 32 bytes');
  return secp256k1.sign(digest, secretKey, { prehash: false, lowS: true, format: 'der', extraEntropy: false });
}

/**
 * Verifies a DER-encoded ECDSA signature (without the sighash type byte). Requires low S (Bitcoin Core's standardness rule).
 * `publicKey` is the 33-byte compressed public key.
 */
export function ecdsaVerifyDer(der: Uint8Array, digest: Uint8Array, publicKey: Uint8Array): boolean {
  try {
    const sig = secp256k1.Signature.fromBytes(der, 'der');
    if (sig.s > HALF_ORDER) return false;
    return secp256k1.verify(der, digest, publicKey, { prehash: false, lowS: true, format: 'der' });
  } catch {
    return false;
  }
}

/** Whether the S of a DER-encoded ECDSA signature is low (≤ n/2). Returns `false` when it cannot be parsed. */
export function isLowS(der: Uint8Array): boolean {
  try {
    return secp256k1.Signature.fromBytes(der, 'der').s <= HALF_ORDER;
  } catch {
    return false;
  }
}

/**
 * Strict DER signature encoding (BIP66), rule by rule the same as Bitcoin Core's `IsValidSignatureEncoding` (and as
 * `is_strict_der` in the Rust `ksana-btc` crate); `sig` includes the trailing sighash type byte.
 *
 * Format: `0x30 [total length] 0x02 [R length] [R] 0x02 [S length] [S] [sighash type]`.
 */
export function isStrictDer(sig: Uint8Array): boolean {
  const len = sig.length;
  if (len < 9 || len > 73) return false;
  if (sig[0] !== 0x30 || sig[1] !== len - 3) return false;
  const lenR = sig[3]!;
  if (5 + lenR >= len) return false;
  const lenS = sig[5 + lenR]!;
  if (lenR + lenS + 7 !== len) return false;
  if (sig[2] !== 0x02 || lenR === 0 || (sig[4]! & 0x80) !== 0) return false;
  if (lenR > 1 && sig[4] === 0x00 && (sig[5]! & 0x80) === 0) return false;
  if (sig[lenR + 4] !== 0x02 || lenS === 0 || (sig[lenR + 6]! & 0x80) !== 0) return false;
  if (lenS > 1 && sig[lenR + 6] === 0x00 && (sig[lenR + 7]! & 0x80) === 0) return false;
  return true;
}
