/**
 * Half-aggregated signatures of a block's path B ops (SPEC §4.5, D-154): the items, the aggregate and its verification,
 * and a block's entries from and to the entries with whole signatures. A local check of what a node answers: consensus
 * verification is on the Rust side (`crates/ksana-crypto/src/halfagg.rs`, `vmop.rs`), which is authoritative. Both sides
 * reproduce `signing.json`'s `aggregate`.
 *
 * - `z_0 = 1`, `z_i = int(tagged_hash("HalfAgg/randomizer", R_0 ‖ pk_0 ‖ m_0 ‖ … ‖ R_i ‖ pk_i ‖ m_i)) mod n`;
 * - aggregate: `s = Σ z_i · s_i mod n`;
 * - verify: `s·G = Σ z_i·R_i + Σ z_i·e_i·P_i`, `e_i = int(tagged_hash("BIP0340/challenge", R_i ‖ pk_i ‖ m_i)) mod n`.
 */

import { schnorr, secp256k1 } from '@noble/curves/secp256k1.js';
import { sha256 } from '@noble/hashes/sha2.js';

import { accountSighash, bip322Message } from './bip322.js';
import { concatBytes, utf8 } from './bytes.js';
import { joinVmOp, splitVmOp, taggedHash, vmopSighash } from './hash.js';
import type { Entry, VmOp } from './types.js';

/** What the aggregate holds of one signature: the account, the 32-byte message signed and the signature's first half. */
export interface HalfSigned {
  pubkey: Uint8Array;
  msg: Uint8Array;
  r: Uint8Array;
}

/** The most signatures one aggregate holds. */
export const MAX_AGGREGATED = 0xffff;

const Point = secp256k1.Point;
const N = Point.Fn.ORDER;

const toBigInt = (bytes: Uint8Array): bigint => {
  let n = 0n;
  for (const b of bytes) n = (n << 8n) | BigInt(b);
  return n;
};

const toBytes32 = (n: bigint): Uint8Array => {
  const out = new Uint8Array(32);
  for (let i = 31; i >= 0; i--) {
    out[i] = Number(n & 0xffn);
    n >>= 8n;
  }
  return out;
};

function checkItems(items: readonly HalfSigned[]): void {
  if (items.length > MAX_AGGREGATED) throw new Error(`an aggregate holds at most ${MAX_AGGREGATED} signatures`);
  for (const [i, item] of items.entries()) {
    if (item.pubkey.length !== 32 || item.msg.length !== 32 || item.r.length !== 32) {
      throw new Error(`item ${i}: the account, the message and half a signature must be 32 bytes each`);
    }
  }
}

/** The randomizers `z_i` of the items, in order. */
function randomizers(items: readonly HalfSigned[]): bigint[] {
  const tag = sha256(utf8('HalfAgg/randomizer'));
  const hasher = sha256.create().update(tag).update(tag);
  return items.map((item, i) => {
    hasher.update(item.r).update(item.pubkey).update(item.msg);
    return i === 0 ? 1n : toBigInt(hasher.clone().digest()) % N;
  });
}

/**
 * The item of a path B op (SPEC §4.5), from the op with half its signature or with the whole one: for BIP340 the message
 * is the VmOp sighash, for the two BIP322 forms the BIP341 sighash of BIP322's `to_sign` with `SIGHASH_DEFAULT` or
 * `SIGHASH_ALL`. `null` for a signature without a half form.
 */
export function halfSigned(op: VmOp): HalfSigned | null {
  const half = splitVmOp(op)?.half ?? op;
  switch (half.sig.kind) {
    case 'HalfBip340':
      return { pubkey: op.account, msg: vmopSighash(op), r: half.sig.bytes };
    case 'HalfBip322':
      return { pubkey: op.account, msg: accountSighash(op.account, bip322Message(op), false), r: half.sig.bytes };
    case 'HalfBip322All':
      return { pubkey: op.account, msg: accountSighash(op.account, bip322Message(op), true), r: half.sig.bytes };
    default:
      return null;
  }
}

/** The aggregate of the second halves `seconds` of the signatures of `items`, in order. The signatures are not verified. */
export function aggregate(items: readonly HalfSigned[], seconds: readonly Uint8Array[]): Uint8Array {
  checkItems(items);
  if (items.length !== seconds.length) throw new Error('one second half per item');
  let s = 0n;
  for (const [i, z] of randomizers(items).entries()) {
    const second = seconds[i]!;
    if (second.length !== 32) throw new Error(`second half ${i} must be 32 bytes`);
    const si = toBigInt(second);
    if (si >= N) throw new Error(`second half ${i} is not below the group order`);
    s = (s + z * si) % N;
  }
  return toBytes32(s);
}

/** Whether `s` is the aggregate of valid signatures of `items`, in order (the rule of a block, SPEC §4.5). */
export function verifyAggregate(items: readonly HalfSigned[], s: Uint8Array): boolean {
  try {
    checkItems(items);
    if (s.length !== 32) return false;
    const scalar = toBigInt(s);
    if (scalar >= N) return false;
    let sum = Point.ZERO;
    for (const [i, z] of randomizers(items).entries()) {
      const { pubkey, msg, r } = items[i]!;
      const e = toBigInt(taggedHash('BIP0340/challenge', concatBytes(r, pubkey, msg))) % N;
      const ze = (z * e) % N;
      // lift_x refuses an x that is not on the curve; a zero scalar contributes nothing
      const R = schnorr.utils.lift_x(toBigInt(r));
      const P = schnorr.utils.lift_x(toBigInt(pubkey));
      sum = sum.add(R.multiplyUnsafe(z));
      if (ze !== 0n) sum = sum.add(P.multiplyUnsafe(ze));
    }
    const left = scalar === 0n ? Point.ZERO : Point.BASE.multiplyUnsafe(scalar);
    return left.equals(sum);
  } catch {
    return false;
  }
}

/**
 * The entries of a block from entries with whole signatures (what a producer does): each path B op with half its
 * signature, the aggregate entry last when there is a path B entry, and the second halves in order. The signatures are
 * not verified.
 */
export function sealEntries(entries: readonly Entry[]): { entries: Entry[]; secondHalves: Uint8Array[] } {
  const sealed: Entry[] = [];
  const items: HalfSigned[] = [];
  const secondHalves: Uint8Array[] = [];
  for (const [i, entry] of entries.entries()) {
    if (entry.kind === 'Aggregate') throw new Error(`entry ${i}: the entries already carry an aggregate`);
    if (entry.kind !== 'Sponsored') {
      sealed.push(entry);
      continue;
    }
    const split = splitVmOp(entry.op);
    const item = split === null ? null : halfSigned(split.half);
    if (split === null || item === null) throw new Error(`entry ${i}: the signature of the path B op has no half form`);
    items.push(item);
    secondHalves.push(split.s);
    sealed.push({ kind: 'Sponsored', op: split.half });
  }
  if (items.length > 0) sealed.push({ kind: 'Aggregate', s: aggregate(items, secondHalves) });
  return { entries: sealed, secondHalves };
}

/**
 * The path B ops of a block with their whole signatures, from the block's entries and the second halves beside it, in
 * order; `null` when the second halves are not one per path B entry or an entry carries no half signature. The restored
 * signatures are not verified: second halves are no part of what a block commits to.
 */
export function unsealEntries(entries: readonly Entry[], secondHalves: readonly Uint8Array[]): VmOp[] | null {
  const ops: VmOp[] = [];
  for (const entry of entries) {
    if (entry.kind !== 'Sponsored') continue;
    const s = secondHalves[ops.length];
    const op = s === undefined || s.length !== 32 ? null : joinVmOp(entry.op, s);
    if (op === null) return null;
    ops.push(op);
  }
  return ops.length === secondHalves.length ? ops : null;
}

/**
 * Whether a block's entries satisfy the rule on its path B signatures (SPEC §4.5, §6.3): every path B entry carries half
 * a signature, exactly one aggregate entry stands last when there is a path B entry and none otherwise, and the aggregate
 * verifies over the path B entries in their order. The other rules of a block are not checked.
 */
export function verifyEntriesAggregate(entries: readonly Entry[]): boolean {
  const items: HalfSigned[] = [];
  let s: Uint8Array | null = null;
  for (const [i, entry] of entries.entries()) {
    if (entry.kind === 'Aggregate') {
      if (i + 1 !== entries.length) return false;
      s = entry.s;
    } else if (entry.kind === 'Sponsored') {
      const half = entry.op.sig.kind;
      if (half !== 'HalfBip340' && half !== 'HalfBip322' && half !== 'HalfBip322All') return false;
      const item = halfSigned(entry.op);
      if (item === null) return false;
      items.push(item);
    }
  }
  if (items.length === 0) return s === null;
  return s !== null && verifyAggregate(items, s);
}
