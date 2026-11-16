/**
 * Hashes: SHA-256, sha256d, BIP340 tagged hash, the VmOp signing message and sighash, `op_id`, block hash, RFC 6962 Merkle
 * root (SPEC §4.2, §4.4, §6.1; D-004, D-086).
 */

import { ripemd160 as nobleRipemd160 } from '@noble/hashes/legacy.js';
import { sha256 as nobleSha256 } from '@noble/hashes/sha2.js';

import { assertBytes, concatBytes, utf8 } from './bytes.js';
import { encodeBlockHeader, encodeEntry, encodeVmOp, encodeVmOpUnsigned } from './codec.js';
import type { BlockHeader, Entry, OutPoint, SigScheme, UnsignedVmOp, VmOp } from './types.js';

/** SHA-256. */
export function sha256(data: Uint8Array): Uint8Array {
  return nobleSha256(data);
}

/** Double SHA-256: `sha256(sha256(data))`. */
export function sha256d(data: Uint8Array): Uint8Array {
  return nobleSha256(nobleSha256(data));
}

/** `ripemd160(sha256(data))` (the public key hash of P2WPKH / P2PKH). */
export function hash160(data: Uint8Array): Uint8Array {
  return nobleRipemd160(nobleSha256(data));
}

/** BIP340 tagged hash: `sha256(sha256(tag) ‖ sha256(tag) ‖ msg)`. */
export function taggedHash(tag: string, msg: Uint8Array): Uint8Array {
  const tagHash = nobleSha256(utf8(tag));
  return nobleSha256(concatBytes(tagHash, tagHash, msg));
}

/** The BIP340 tag of the VmOp sighash. */
export const VMOP_SIGHASH_TAG = 'Ksana/VmOp/v1';

/** Encoding of an outpoint (D-086): `txid ‖ vout` (little-endian u32), 36 bytes; the signing message of a path C operation ends with this encoding of the carrying transaction's first input. */
export function encodeOutPoint(point: OutPoint): Uint8Array {
  assertBytes(point.txid, 32, 'outpoint txid');
  if (!Number.isInteger(point.vout) || point.vout < 0 || point.vout > 0xffff_ffff) {
    throw new RangeError('outpoint vout out of u32 range');
  }
  const out = new Uint8Array(36);
  out.set(point.txid, 0);
  new DataView(out.buffer).setUint32(32, point.vout, true);
  return out;
}

/**
 * The signing message of a VmOp (SPEC §4.2): the Borsh encoding of the VmOp without `sig`; for a path C operation (`fee` is
 * `BtcNative`) it is followed by the encoding of the first input of the BTC transaction carrying it
 * (`encodeOutPoint(firstInput)`, D-086), so an operation copied into another transaction fails verification. `firstInput`
 * must be given on path C and must not be given on path B.
 */
export function vmopSigningMessage(op: UnsignedVmOp, firstInput?: OutPoint): Uint8Array {
  const unsigned = encodeVmOpUnsigned(op);
  if (op.fee.kind === 'BtcNative') {
    if (firstInput === undefined) throw new Error('the signature of a path C operation is bound to the first input of the carrying transaction: firstInput must be given (D-086)');
    return concatBytes(unsigned, encodeOutPoint(firstInput));
  }
  if (firstInput !== undefined) throw new Error('a path B operation is not bound to a BTC transaction input: firstInput must not be given');
  return unsigned;
}

/** The VmOp sighash: the tagged hash with tag `Ksana/VmOp/v1` over `vmopSigningMessage(op, firstInput)` (SPEC §4.2, D-086). */
export function vmopSighash(op: UnsignedVmOp, firstInput?: OutPoint): Uint8Array {
  return taggedHash(VMOP_SIGHASH_TAG, vmopSigningMessage(op, firstInput));
}

const WITNESS_ONE_ITEM = 0x01;
const SIGHASH_ALL = 0x01;

/**
 * A whole signature as its half form and its second half `s` (SPEC §4.5, D-154): `Bip340(R ‖ s)` gives `HalfBip340(R)`;
 * a `Bip322Simple` splits only when its bytes are exactly `01 40 ‖ R ‖ s` (`HalfBip322`) or `01 41 ‖ R ‖ s ‖ 01`
 * (`HalfBip322All`), the two witnesses consensus accepts. `null` for any other bytes and for half a signature.
 */
export function splitSig(sig: SigScheme): { half: SigScheme; s: Uint8Array } | null {
  const halves = (kind: 'HalfBip340' | 'HalfBip322' | 'HalfBip322All', bytes: Uint8Array) =>
    bytes.length === 64 ? { half: { kind, bytes: bytes.slice(0, 32) }, s: bytes.slice(32) } : null;
  switch (sig.kind) {
    case 'Bip340':
      return halves('HalfBip340', sig.bytes);
    case 'Bip322Simple': {
      const w = sig.bytes;
      if (w.length === 66 && w[0] === WITNESS_ONE_ITEM && w[1] === 64) return halves('HalfBip322', w.subarray(2));
      if (w.length === 67 && w[0] === WITNESS_ONE_ITEM && w[1] === 65 && w[66] === SIGHASH_ALL) {
        return halves('HalfBip322All', w.subarray(2, 66));
      }
      return null;
    }
    default:
      return null;
  }
}

/** The whole signature of half a signature and its second half `s` (32 bytes), the reverse of `splitSig`; `null` for a whole signature. */
export function joinSig(half: SigScheme, s: Uint8Array): SigScheme | null {
  if (s.length !== 32) throw new Error('the second half of a signature must be 32 bytes');
  if (half.kind !== 'HalfBip340' && half.kind !== 'HalfBip322' && half.kind !== 'HalfBip322All') return null;
  if (half.bytes.length !== 32) throw new Error('half a signature must be 32 bytes');
  switch (half.kind) {
    case 'HalfBip340':
      return { kind: 'Bip340', bytes: concatBytes(half.bytes, s) };
    case 'HalfBip322':
      return { kind: 'Bip322Simple', bytes: concatBytes(Uint8Array.of(WITNESS_ONE_ITEM, 64), half.bytes, s) };
    case 'HalfBip322All':
      return { kind: 'Bip322Simple', bytes: concatBytes(Uint8Array.of(WITNESS_ONE_ITEM, 65), half.bytes, s, Uint8Array.of(SIGHASH_ALL)) };
  }
}

/** A VmOp with a whole signature as the op a block holds, with half its signature, and the second half (D-154); `null` as for `splitSig`. */
export function splitVmOp(op: VmOp): { half: VmOp; s: Uint8Array } | null {
  const split = splitSig(op.sig);
  return split === null ? null : { half: { ...op, sig: split.half }, s: split.s };
}

/** The VmOp with its whole signature, from the op a block holds and the second half beside the block (D-154); `null` as for `joinSig`. */
export function joinVmOp(half: VmOp, s: Uint8Array): VmOp | null {
  const sig = joinSig(half.sig, s);
  return sig === null ? null : { ...half, sig };
}

/**
 * `op_id` (SPEC §4.4, D-154): for a path C op `sha256(full VmOp encoding)`; for a path B op the same hash over the op with
 * half its signature, whichever form is given, so the ID a client computes when it signs is the ID of the op in a block.
 * A path B op whose signature has no half form (it never verifies) hashes as it is.
 */
export function opId(op: VmOp): Uint8Array {
  const split = op.fee.kind === 'Token' ? splitVmOp(op) : null;
  return nobleSha256(encodeVmOp(split === null ? op : split.half));
}

/** Block hash: `sha256d(borsh(header))` (SPEC §6.1). */
export function blockHash(header: BlockHeader): Uint8Array {
  return sha256d(encodeBlockHeader(header));
}

const LEAF_PREFIX = Uint8Array.of(0x00);
const NODE_PREFIX = Uint8Array.of(0x01);

/** The largest power of 2 less than `n` (`n >= 2`). */
function splitPoint(n: number): number {
  let k = 1;
  while (k * 2 < n) k *= 2;
  return k;
}

function mth(leaves: readonly Uint8Array[], start: number, end: number): Uint8Array {
  const n = end - start;
  if (n === 0) return nobleSha256(new Uint8Array(0));
  if (n === 1) return nobleSha256(concatBytes(LEAF_PREFIX, leaves[start]!));
  const k = splitPoint(n);
  return nobleSha256(concatBytes(NODE_PREFIX, mth(leaves, start, start + k), mth(leaves, start + k, end)));
}

/**
 * RFC 6962 Merkle Tree Hash (SHA-256, D-004): `MTH({}) = sha256("")`, leaves `sha256(0x00 ‖ d)`, internal nodes
 * `sha256(0x01 ‖ left ‖ right)`; the left subtree takes the largest power of 2 less than n leaves.
 */
export function merkleRoot(leaves: readonly Uint8Array[]): Uint8Array {
  return mth(leaves, 0, leaves.length);
}

/** `entries_root`: the leaves are the Borsh encodings of the block's entries, in entry order (SPEC §6.1). */
export function entriesRoot(entries: readonly Entry[]): Uint8Array {
  return merkleRoot(entries.map(encodeEntry));
}
