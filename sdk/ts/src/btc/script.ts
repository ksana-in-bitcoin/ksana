/**
 * Locking scripts: encoding and reading of data pushes (the semantics of rust-bitcoin's `push_slice`), construction and recognition
 * of the common output types, dust limit.
 */

import { compactSize, concatBytes, u32le } from '../bytes.js';
import { hash160 } from '../hash.js';
import { taprootOutputKey } from '../keys.js';

export const OP_0 = 0x00;
export const OP_PUSHDATA1 = 0x4c;
export const OP_PUSHDATA2 = 0x4d;
export const OP_PUSHDATA4 = 0x4e;
export const OP_1 = 0x51;
export const OP_16 = 0x60;
export const OP_RETURN = 0x6a;
export const OP_DUP = 0x76;
export const OP_EQUAL = 0x87;
export const OP_EQUALVERIFY = 0x88;
export const OP_HASH160 = 0xa9;
export const OP_CHECKSIG = 0xac;

/**
 * The encoding of one data push, the same as rust-bitcoin's `push_slice`: the opcode is chosen by length
 * (`OP_PUSHBYTES_n` below 76 bytes, then `OP_PUSHDATA1/2/4` in turn); 1-byte data is **not** rewritten as `OP_1`–`OP_16`.
 */
export function pushData(data: Uint8Array): Uint8Array {
  const n = data.length;
  if (n < OP_PUSHDATA1) return concatBytes(Uint8Array.of(n), data);
  if (n < 0x100) return concatBytes(Uint8Array.of(OP_PUSHDATA1, n), data);
  if (n < 0x10000) return concatBytes(Uint8Array.of(OP_PUSHDATA2, n & 0xff, n >> 8), data);
  return concatBytes(Uint8Array.of(OP_PUSHDATA4), u32le(n), data);
}

/** `OP_PUSHBYTES_75`: the largest push whose length is given by the opcode itself. */
const MAX_DIRECT_PUSH = 0x4b;

/**
 * Reads one data push at `at` (`OP_PUSHBYTES_0..75`, `OP_PUSHDATA1/2/4`, non-minimal encodings accepted): `[data, position after
 * it]`; `null` when it is not a data push or is cut short. `read_push` in `crates/ksana-btc/src/data.rs`.
 */
export function readPush(script: Uint8Array, at: number): [Uint8Array, number] | null {
  const opcode = script[at];
  if (opcode === undefined) return null;
  let length: number;
  let start: number;
  if (opcode <= MAX_DIRECT_PUSH) {
    length = opcode;
    start = at + 1;
  } else if (opcode === OP_PUSHDATA1) {
    if (at + 2 > script.length) return null;
    length = script[at + 1]!;
    start = at + 2;
  } else if (opcode === OP_PUSHDATA2) {
    if (at + 3 > script.length) return null;
    length = script[at + 1]! | (script[at + 2]! << 8);
    start = at + 3;
  } else if (opcode === OP_PUSHDATA4) {
    if (at + 5 > script.length) return null;
    length = (script[at + 1]! | (script[at + 2]! << 8) | (script[at + 3]! << 16) | (script[at + 4]! << 24)) >>> 0;
    start = at + 5;
  } else {
    return null;
  }
  if (start + length > script.length) return null;
  return [script.subarray(start, start + length), start + length];
}

/** P2TR (BIP86, no script tree): `OP_1 <tweaked output key>`. */
export function p2trScript(internalKey: Uint8Array): Uint8Array {
  return p2trScriptFromOutputKey(taprootOutputKey(internalKey));
}

/** The P2TR script of an already tweaked output key. */
export function p2trScriptFromOutputKey(outputKey: Uint8Array): Uint8Array {
  if (outputKey.length !== 32) throw new Error('the P2TR output key must be 32 bytes');
  return concatBytes(Uint8Array.of(OP_1, 0x20), outputKey);
}

/** P2WPKH: `OP_0 <hash160(compressed public key)>`. */
export function p2wpkhScript(compressedPublicKey: Uint8Array): Uint8Array {
  if (compressedPublicKey.length !== 33) throw new Error('P2WPKH only accepts a 33-byte compressed public key');
  return concatBytes(Uint8Array.of(OP_0, 0x14), hash160(compressedPublicKey));
}

/** P2PKH: `OP_DUP OP_HASH160 <hash> OP_EQUALVERIFY OP_CHECKSIG` (also the BIP143 scriptCode of P2WPKH). */
export function p2pkhScriptFromHash(pubkeyHash: Uint8Array): Uint8Array {
  if (pubkeyHash.length !== 20) throw new Error('the public key hash must be 20 bytes');
  return concatBytes(Uint8Array.of(OP_DUP, OP_HASH160, 0x14), pubkeyHash, Uint8Array.of(OP_EQUALVERIFY, OP_CHECKSIG));
}

/** P2SH: `OP_HASH160 <hash> OP_EQUAL`. */
export function p2shScriptFromHash(scriptHash: Uint8Array): Uint8Array {
  if (scriptHash.length !== 20) throw new Error('the script hash must be 20 bytes');
  return concatBytes(Uint8Array.of(OP_HASH160, 0x14), scriptHash, Uint8Array.of(OP_EQUAL));
}

/** Witness program (`OP_n <2..40 bytes>`): returns `{ version, program }`, or `null` when the script is not one (the same check as rust-bitcoin). */
export function witnessProgram(script: Uint8Array): { version: number; program: Uint8Array } | null {
  if (script.length < 4 || script.length > 42) return null;
  const first = script[0]!;
  let version: number;
  if (first === OP_0) version = 0;
  else if (first >= OP_1 && first <= OP_16) version = first - OP_1 + 1;
  else return null;
  const push = script[1]!;
  if (push < 2 || push > 40 || push !== script.length - 2) return null;
  return { version, program: Uint8Array.from(script.subarray(2)) };
}

/** Whether the script is P2TR (witness v1, 32-byte program). */
export function isP2tr(script: Uint8Array): boolean {
  return script.length === 34 && script[0] === OP_1 && script[1] === 0x20;
}

/** Whether the script is P2WPKH (witness v0, 20-byte program). */
export function isP2wpkh(script: Uint8Array): boolean {
  return script.length === 22 && script[0] === OP_0 && script[1] === 0x14;
}

/** Whether the script is P2WSH (witness v0, 32-byte program). */
export function isP2wsh(script: Uint8Array): boolean {
  return script.length === 34 && script[0] === OP_0 && script[1] === 0x20;
}

/** Whether the script is P2PKH. */
export function isP2pkh(script: Uint8Array): boolean {
  return (
    script.length === 25 &&
    script[0] === OP_DUP &&
    script[1] === OP_HASH160 &&
    script[2] === 0x14 &&
    script[23] === OP_EQUALVERIFY &&
    script[24] === OP_CHECKSIG
  );
}

/** Whether the script is P2SH. */
export function isP2sh(script: Uint8Array): boolean {
  return script.length === 23 && script[0] === OP_HASH160 && script[1] === 0x14 && script[22] === OP_EQUAL;
}

/** Locking scripts allowed for change: P2PKH, P2SH, P2WPKH, P2WSH or P2TR (the same as the change of settlement transactions, D-034). */
export function isStandardOutputScript(script: Uint8Array): boolean {
  return isP2pkh(script) || isP2sh(script) || isP2wpkh(script) || isP2wsh(script) || isP2tr(script);
}

/** Bitcoin Core's default dust relay fee (3 sat/vB). */
export const DUST_RELAY_FEE_SAT_PER_KVB = 3000n;

/**
 * The dust limit of an output (sats), the same as rust-bitcoin's `minimal_non_dust` / Bitcoin Core's `GetDustThreshold`:
 * a spend cost of 67 vB for witness programs and 148 vB for everything else, plus the size of the output itself, times 3 sat/vB.
 * `OP_RETURN` is 0. Examples: P2WPKH 294, P2TR / P2WSH 330, P2SH 540, P2PKH 546.
 */
export function dustLimit(script: Uint8Array): bigint {
  if (script[0] === OP_RETURN) return 0n;
  const spendCost = witnessProgram(script) === null ? 32 + 4 + 1 + 107 + 4 : 32 + 4 + 1 + Math.floor(107 / 4) + 4;
  const size = BigInt(spendCost + 8 + compactSize(script.length).length + script.length);
  return (size * DUST_RELAY_FEE_SAT_PER_KVB) / 1000n;
}
