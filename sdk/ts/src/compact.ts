/**
 * Compact encoding of the header sequence in a payload (D-075, D-079, revising SPEC §7.1), byte-for-byte identical to
 * `crates/ksana-primitives/src/compact.rs`: the headers in `Settlement` and `Headers` payloads are each encoded relative
 * to the headers before them in the sequence, and decoding restores the full headers (the headers themselves and the
 * block hashes are unchanged).
 *
 * Sequence = `u32` count (little-endian), then for each header:
 *
 * | Field | Encoding |
 * | --- | --- |
 * | `flags: u8` | bit 0 version equal to the previous one, bit 1 BTC anchor equal to the previous one, bit 2 `bits` equal to the previous one, bit 3 `entries_root` is the root of an empty entry list, bit 4 `state_root` equal to the previous one (D-079); the other bits are 0 |
 * | version | `u32` when bit 0 is not set |
 * | height | varint for the first header, then the zigzag varint of the difference from the previous one (mod 2^64, interpreted as `i64`) |
 * | parents | varint count, then each parent as a varint `r`: `r ≥ 1` refers to the hash of the `r`-th header before it in the sequence, `r = 0` is followed by the 32-byte hash |
 * | BTC anchor | `u32` height + 32-byte hash when bit 1 is not set |
 * | `entries_root` | 32 bytes when bit 3 is not set |
 * | `state_root` | 32 bytes when bit 4 is not set |
 * | producer | varint `r`: `r ≥ 1` is the producer of the `r`-th header before it, `r = 0` is followed by the 32 bytes |
 * | timestamp | as height |
 * | `bits` | `u32` when bit 2 is not set |
 * | `nonce` | `u64` |
 *
 * A varint is LEB128 (unsigned, shortest encoding, at most `u64`). The encoding is canonical and decoding checks it: a
 * field equal to the previous one must use the flag, the first header does not set bits 0 to 2 and 4, the empty root must
 * use bit 3; a parent that appears in the sequence must be referenced (the most recent one), and likewise a producer
 * references the most recent equal producer; a varint must be the shortest encoding. The error messages are the same as
 * on the Rust side (the `error` of the shared vectors `tests/vectors/codec.json`).
 *
 * Referencing a parent needs the block hash, and the block hash needs the Borsh encoding of the header (`codec.ts`), so
 * the hash function is passed in by the caller.
 */

import { sha256 } from '@noble/hashes/sha2.js';

import { ByteReader, concatBytes, equalBytes, u32le, u64le } from './bytes.js';
import { CodecError } from './schema.js';
import type { BlockHeader, BtcAnchor } from './types.js';

const SAME_VERSION = 1;
const SAME_ANCHOR = 1 << 1;
const SAME_BITS = 1 << 2;
const EMPTY_ENTRIES = 1 << 3;
const SAME_STATE_ROOT = 1 << 4;
const KNOWN_FLAGS = SAME_VERSION | SAME_ANCHOR | SAME_BITS | EMPTY_ENTRIES | SAME_STATE_ROOT;
/** Flags the first header cannot set (there is no previous one to compare with). */
const SAME_AS_PREVIOUS = SAME_VERSION | SAME_ANCHOR | SAME_BITS | SAME_STATE_ROOT;
/** Maximum number of parents of one header (a codec limit, far above `MAX_BLOCK_PARENTS`, only to bound allocation). */
const MAX_PARENTS = 255n;
const U64_MOD = 1n << 64n;
const I64_MIN = 1n << 63n;

/** A function computing the block hash (`sha256d(borsh(header))`). */
export type HeaderHasher = (header: BlockHeader) => Uint8Array;

/** The `entries_root` of an empty entry list (the empty tree of RFC 6962: `sha256("")`). */
export function emptyEntriesRoot(): Uint8Array {
  return sha256(new Uint8Array(0));
}

const EMPTY_ROOT = emptyEntriesRoot();

/** Throw a decoding error (the error message is the same as on the Rust side). */
export function invalid(message: string): never {
  throw new CodecError(`Borsh decode failed: ${message}`);
}

/** LEB128 (unsigned). */
export function writeVarint(out: number[], value: bigint): void {
  let rest = value;
  for (;;) {
    const low = Number(rest & 0x7fn);
    rest >>= 7n;
    if (rest === 0n) {
      out.push(low);
      return;
    }
    out.push(low | 0x80);
  }
}

/** LEB128; the shortest encoding is required and the value must fit in `u64`. */
export function readVarint(reader: ByteReader): bigint {
  let value = 0n;
  let shift = 0n;
  for (;;) {
    const byte = reader.u8();
    const low = BigInt(byte & 0x7f);
    if (shift === 63n && low > 1n) invalid('varint exceeds u64');
    value |= low << shift;
    if ((byte & 0x80) === 0) {
      if (byte === 0 && shift > 0n) invalid('varint is not minimally encoded');
      return value;
    }
    shift += 7n;
    if (shift > 63n) invalid('varint too long');
  }
}

/** Zigzag of the difference from the previous value (mod 2^64, interpreted as `i64`): any `u64` can be encoded, and nearby values take only one or two bytes. */
function zigzagDelta(value: bigint, previous: bigint): bigint {
  const diff = (((value - previous) % U64_MOD) + U64_MOD) % U64_MOD;
  const signed = diff >= I64_MIN ? diff - U64_MOD : diff;
  return signed >= 0n ? signed << 1n : ((-signed) << 1n) - 1n;
}

function applyDelta(previous: bigint, zigzag: bigint): bigint {
  const signed = (zigzag & 1n) === 0n ? zigzag >> 1n : -(zigzag >> 1n) - 1n;
  return (((previous + signed) % U64_MOD) + U64_MOD) % U64_MOD;
}

function sameAnchor(a: BtcAnchor, b: BtcAnchor): boolean {
  return a.height === b.height && equalBytes(a.hash, b.hash);
}

/** Distance (`r ≥ 1`) from the last element of the sequence satisfying `matches` to the end of the sequence (i.e. the next position). */
function backReference<T>(items: readonly T[], matches: (item: T) => boolean): number | undefined {
  for (let i = items.length - 1; i >= 0; i--) {
    if (matches(items[i]!)) return items.length - i;
  }
  return undefined;
}

/** The key of the `r`-th (`r ≥ 1`) element before the next position in the sequence, checking that it is the element with that key closest to the next position (canonical encoding). */
function reference<T>(items: readonly T[], r: bigint, key: (item: T) => Uint8Array): Uint8Array {
  if (r > BigInt(items.length)) invalid('reference out of range');
  const position = items.length - Number(r);
  const target = key(items[position]!);
  for (let i = position + 1; i < items.length; i++) {
    if (equalBytes(key(items[i]!), target)) invalid('reference is not the most recent equal element (non-canonical encoding)');
  }
  return target;
}

/** Header-by-header encoding of a header sequence: each header relative to the headers encoded before it (see the module docs). Used to fit headers one at a time within the payload limit. */
export class HeaderEncoder {
  private previous: BlockHeader | undefined;
  private readonly hashes: Uint8Array[] = [];
  private readonly producers: Uint8Array[] = [];

  constructor(private readonly hash: HeaderHasher) {}

  /** Encoding of the next header in the sequence (without the count at the start of the sequence). Throws `CodecError` when there are more than 255 parents. */
  encode(header: BlockHeader): Uint8Array {
    if (BigInt(header.parents.length) > MAX_PARENTS) throw new CodecError('Borsh encode failed: too many parents');
    const previous = this.previous;
    let flags = 0;
    if (previous !== undefined && previous.version === header.version) flags |= SAME_VERSION;
    if (previous !== undefined && sameAnchor(previous.btc_anchor, header.btc_anchor)) flags |= SAME_ANCHOR;
    if (previous !== undefined && previous.bits === header.bits) flags |= SAME_BITS;
    if (equalBytes(header.entries_root, EMPTY_ROOT)) flags |= EMPTY_ENTRIES;
    if (previous !== undefined && equalBytes(previous.state_root, header.state_root)) flags |= SAME_STATE_ROOT;
    const parts: Uint8Array[] = [Uint8Array.of(flags)];
    const varint = (value: bigint): void => {
      const out: number[] = [];
      writeVarint(out, value);
      parts.push(Uint8Array.from(out));
    };
    if ((flags & SAME_VERSION) === 0) parts.push(u32le(header.version));
    varint(previous === undefined ? header.height : zigzagDelta(header.height, previous.height));
    varint(BigInt(header.parents.length));
    for (const parent of header.parents) {
      const r = backReference(this.hashes, (h) => equalBytes(h, parent));
      if (r === undefined) {
        varint(0n);
        parts.push(parent);
      } else {
        varint(BigInt(r));
      }
    }
    if ((flags & SAME_ANCHOR) === 0) parts.push(u32le(header.btc_anchor.height), header.btc_anchor.hash);
    if ((flags & EMPTY_ENTRIES) === 0) parts.push(header.entries_root);
    if ((flags & SAME_STATE_ROOT) === 0) parts.push(header.state_root);
    const producer = backReference(this.producers, (p) => equalBytes(p, header.producer));
    if (producer === undefined) {
      varint(0n);
      parts.push(header.producer);
    } else {
      varint(BigInt(producer));
    }
    varint(previous === undefined ? header.timestamp : zigzagDelta(header.timestamp, previous.timestamp));
    if ((flags & SAME_BITS) === 0) parts.push(u32le(header.bits));
    parts.push(u64le(header.nonce));
    this.hashes.push(this.hash(header));
    this.producers.push(header.producer);
    this.previous = header;
    return concatBytes(...parts);
  }
}

/** Compact encoding of a header sequence (including the 4-byte count at the start). */
export function encodeHeaders(headers: readonly BlockHeader[], hash: HeaderHasher): Uint8Array {
  const encoder = new HeaderEncoder(hash);
  return concatBytes(u32le(headers.length), ...headers.map((h) => encoder.encode(h)));
}

/** Length of the compact encoding of a header sequence (including the 4-byte count at the start). */
export function encodedHeadersLen(headers: readonly BlockHeader[], hash: HeaderHasher): number {
  const encoder = new HeaderEncoder(hash);
  return headers.reduce((len, h) => len + encoder.encode(h).length, 4);
}

/**
 * Read a header sequence of at most `maxCount` headers from the current position of `reader` (a payload carries at most
 * `MAX_SETTLEMENT_HEADERS`, checked on the count before any header is read) and check that the encoding is canonical (see
 * the module docs). The caller bounds the total length beforehand (a payload is at most `MAX_BLOCK_DATA_BYTES`). The
 * error `reader` throws on truncation is converted by the caller.
 */
export function readHeaders(reader: ByteReader, hash: HeaderHasher, maxCount: number = Number.MAX_SAFE_INTEGER): BlockHeader[] {
  const count = reader.u32();
  if (count > maxCount) invalid('too many headers');
  const headers: BlockHeader[] = [];
  const hashes: Uint8Array[] = [];
  for (let index = 0; index < count; index++) {
    const previous = headers[index - 1];
    const flags = reader.u8();
    if ((flags & ~KNOWN_FLAGS) !== 0) invalid('unknown flag bit');
    if (previous === undefined && (flags & SAME_AS_PREVIOUS) !== 0) {
      invalid('the first header cannot refer to a previous one');
    }
    let version: number;
    if (previous !== undefined && (flags & SAME_VERSION) !== 0) {
      version = previous.version;
    } else {
      version = reader.u32();
      if (previous !== undefined && previous.version === version) invalid('version equal to the previous one must use the flag (non-canonical encoding)');
    }
    const height = previous === undefined ? readVarint(reader) : applyDelta(previous.height, readVarint(reader));
    const parentCount = readVarint(reader);
    if (parentCount > MAX_PARENTS) invalid('too many parents');
    const parents: Uint8Array[] = [];
    for (let i = 0n; i < parentCount; i++) {
      const r = readVarint(reader);
      if (r === 0n) {
        const parent = reader.read(32);
        if (hashes.some((h) => equalBytes(h, parent))) invalid('a parent within the sequence must be referenced (non-canonical encoding)');
        parents.push(parent);
      } else {
        parents.push(reference(hashes, r, (h) => h).slice());
      }
    }
    let btcAnchor: BtcAnchor;
    if (previous !== undefined && (flags & SAME_ANCHOR) !== 0) {
      btcAnchor = { height: previous.btc_anchor.height, hash: previous.btc_anchor.hash.slice() };
    } else {
      btcAnchor = { height: reader.u32(), hash: reader.read(32) };
      if (previous !== undefined && sameAnchor(previous.btc_anchor, btcAnchor)) {
        invalid('anchor equal to the previous one must use the flag (non-canonical encoding)');
      }
    }
    let entriesRoot: Uint8Array;
    if ((flags & EMPTY_ENTRIES) !== 0) {
      entriesRoot = EMPTY_ROOT.slice();
    } else {
      entriesRoot = reader.read(32);
      if (equalBytes(entriesRoot, EMPTY_ROOT)) invalid('the root of an empty entry list must use the flag (non-canonical encoding)');
    }
    let stateRoot: Uint8Array;
    if (previous !== undefined && (flags & SAME_STATE_ROOT) !== 0) {
      stateRoot = previous.state_root.slice();
    } else {
      stateRoot = reader.read(32);
      if (previous !== undefined && equalBytes(previous.state_root, stateRoot)) {
        invalid('state root equal to the previous one must use the flag (non-canonical encoding)');
      }
    }
    const r = readVarint(reader);
    let producer: Uint8Array;
    if (r === 0n) {
      producer = reader.read(32);
      if (headers.some((h) => equalBytes(h.producer, producer))) {
        invalid('a producer already seen in the sequence must be referenced (non-canonical encoding)');
      }
    } else {
      producer = reference(headers, r, (h) => h.producer).slice();
    }
    const timestamp =
      previous === undefined ? readVarint(reader) : applyDelta(previous.timestamp, readVarint(reader));
    let bits: number;
    if (previous !== undefined && (flags & SAME_BITS) !== 0) {
      bits = previous.bits;
    } else {
      bits = reader.u32();
      if (previous !== undefined && previous.bits === bits) invalid('bits equal to the previous one must use the flag (non-canonical encoding)');
    }
    const nonce = reader.u64();
    const header: BlockHeader = {
      version,
      height,
      parents,
      btc_anchor: btcAnchor,
      entries_root: entriesRoot,
      state_root: stateRoot,
      producer,
      timestamp,
      bits,
      nonce,
    };
    hashes.push(hash(header));
    headers.push(header);
  }
  return headers;
}
