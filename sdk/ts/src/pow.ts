/**
 * PoW numeric conventions and the network's mining algorithm Argon2id (SPEC §6.6; D-025, D-084, D-094), byte-for-byte the
 * same as `pow.rs` / `algorithm.rs` in `crates/ksana-consensus`:
 *
 * - compact `bits` → target (Bitcoin's rule): target = mantissa × 256^(exponent − 3); a set sign bit, a target of 0, a target
 *   ≥ 2^256 and a target above the PoW limit are all invalid;
 * - effective target = target / weight (integer division, D-084): a non-empty block has weight 1, an empty block 12, 24, …, 120
 *   by streak; the header is invalid when the result is 0;
 * - PoW value = the output of `Argon2id(message = borsh(header), salt = "ksana-pow" ‖ chain_id as 4 little-endian bytes,
 *   memory / passes / parallelism per the network parameters, version 0x13, 32-byte output)` interpreted as a 256-bit integer
 *   in **little-endian**;
 * - the PoW is met when PoW value ≤ effective target.
 *
 * This only verifies (a single header takes a few milliseconds in a browser or Node); it does not mine. Whether `bits`
 * follows the difficulty adjustment (ASERT) is not checked here. The shared vectors `tests/vectors/pow.json` are verified
 * case by case by `test/pow.test.ts`.
 */

import { argon2id } from '@noble/hashes/argon2.js';

import { u32le, utf8 } from './bytes.js';
import { encodeBlockHeader } from './codec.js';
import type { Params } from './params.js';
import type { BlockHeader } from './types.js';

/** The fixed prefix of the Argon2id salt (followed by the chain ID as 4 little-endian bytes). */
export const POW_SALT_PREFIX = 'ksana-pow';

/** Output length of the PoW value (bytes). */
export const POW_OUTPUT_LEN = 32;

/** The Argon2 version number (RFC 9106: 0x13). */
export const POW_ARGON2_VERSION = 0x13;

const SIGN_BIT = 0x0080_0000;
const MANTISSA_MASK = 0x007f_ffff;
const TWO_256 = 1n << 256n;

function assertU32(value: number, what: string): void {
  if (!Number.isInteger(value) || value < 0 || value > 0xffff_ffff) throw new RangeError(`${what} is not a u32: ${value}`);
}

/**
 * Compact decoding (the semantics of Bitcoin's `SetCompact`): target = mantissa × 256^(exponent − 3); for an exponent ≤ 3 the
 * mantissa is shifted right and the low bits are dropped. Returns `null` for a set sign bit, a target of 0 or a target ≥ 2^256.
 */
export function decodeCompact(bits: number): bigint | null {
  assertU32(bits, 'bits');
  if ((bits & SIGN_BIT) !== 0) return null;
  const size = bits >>> 24;
  const mantissa = BigInt(bits & MANTISSA_MASK);
  let target: bigint;
  if (size <= 3) {
    target = mantissa >> BigInt(8 * (3 - size));
  } else {
    target = mantissa << BigInt(8 * (size - 3));
    if (target >= TWO_256) return null;
  }
  return target === 0n ? null : target;
}

/** The target of the PoW limit (lowest difficulty); `powLimitBits` itself must be a valid compact target. */
export function powLimitTarget(powLimitBits: number): bigint {
  const limit = decodeCompact(powLimitBits);
  if (limit === null) throw new RangeError(`PoW limit ${powLimitBits.toString(16)} is not a valid compact target`);
  return limit;
}

/** compact `bits` → target (D-025): a set sign bit, a target of 0, a target ≥ 2^256 and a target above the PoW limit all throw `RangeError`. */
export function targetFromBits(bits: number, powLimitBits: number): bigint {
  const limit = powLimitTarget(powLimitBits);
  const target = decodeCompact(bits);
  if (target === null || target > limit) {
    throw new RangeError(`bits ${bits.toString(16)} is invalid (sign bit set, target 0, target ≥ 2^256 or above the PoW limit)`);
  }
  return target;
}

/**
 * The effective target of a header with weight `weight` (two-speed difficulty, D-084): the target of `bits` divided by the
 * weight (integer division). Throws `RangeError` when the weight is 0 or the result is 0 (target too small).
 */
export function effectiveTarget(bits: number, powLimitBits: number, weight: number | bigint): bigint {
  const w = BigInt(weight);
  if (w <= 0n) throw new RangeError(`weight must be positive: ${weight}`);
  const target = targetFromBits(bits, powLimitBits) / w;
  if (target === 0n) throw new RangeError(`the target of bits ${bits.toString(16)} divided by weight ${weight} is 0 and cannot be met (D-084)`);
  return target;
}

/** The salt of network `chainId`: `"ksana-pow" ‖ chain_id` (4 little-endian bytes), 13 bytes in total. */
export function powSalt(chainId: number): Uint8Array {
  const prefix = utf8(POW_SALT_PREFIX);
  const salt = new Uint8Array(prefix.length + 4);
  salt.set(prefix, 0);
  salt.set(u32le(chainId), prefix.length);
  return salt;
}

/** The Argon2id output of a header (32 bytes, the raw bytes of the PoW value), per the algorithm parameters and chain ID in the network parameters. */
export function powHash(params: Params, header: BlockHeader): Uint8Array {
  const { memory_kib, iterations, lanes } = params.pow_algorithm.argon2id;
  return argon2id(encodeBlockHeader(header), powSalt(params.chain_id), {
    m: memory_kib,
    t: iterations,
    p: lanes,
    dkLen: POW_OUTPUT_LEN,
    version: POW_ARGON2_VERSION,
  });
}

/** 32 bytes interpreted as a 256-bit integer in little-endian (the byte order convention of D-025). */
export function bytesToBigIntLE(bytes: Uint8Array): bigint {
  let value = 0n;
  for (let i = bytes.length - 1; i >= 0; i--) value = (value << 8n) | BigInt(bytes[i]!);
  return value;
}

/** The PoW value of a header: `powHash` interpreted in little-endian. */
export function powValue(params: Params, header: BlockHeader): bigint {
  return bytesToBigIntLE(powHash(params, header));
}

/**
 * PoW check (the same as Rust's `check_pow`): `bits` is valid and PoW value ≤ effective target (by weight `weight`). Throws
 * `RangeError` when `bits` is invalid or the effective target is 0 (the header is invalid). Does not check whether `bits`
 * follows the difficulty adjustment.
 */
export function checkPow(params: Params, header: BlockHeader, weight: number | bigint): boolean {
  const target = effectiveTarget(header.bits, params.pow_limit_bits, weight);
  return powValue(params, header) <= target;
}
