/**
 * The compressed form of a KSN1 envelope's data (D-141), read as `crates/ksana-btc/src/compress.rs` reads it: the data is the
 * payload itself, or, when it starts with the zstd magic number, one zstd frame (RFC 8878, no dictionary) of the payload followed
 * by zero or more `0x00` bytes.
 *
 * The frame's layout (its header, the extent of its blocks, its content size and checksum) is checked here by the node's rules;
 * the blocks are decoded by `fzstd`, with the output bounded. `fzstd` does not check that a match stays within the frame's own
 * content, so it reads a payload from some frames the node rejects: a reader of the chain takes whether a settlement counts from
 * the node or the indexer, and uses this module only to read what it carries.
 */

import { Decompress } from 'fzstd';

import { concatBytes } from './bytes.js';

/** The zstd frame magic number (RFC 8878 §3.1.1, little-endian `0xFD2FB528`). */
export const ZSTD_MAGIC: Uint8Array = Uint8Array.of(0x28, 0xb5, 0x2f, 0xfd);

/** `MAX_SETTLEMENT_TX_BYTES`: the bound on the payload a transaction's envelope decompresses to (a transaction never holds more). */
export const MAX_SETTLEMENT_TX_BYTES = 400_000;

/** The largest window log the node's decoder takes. */
const MAX_WINDOW_LOG = 31;
/** A block's content never exceeds this, whatever the window (RFC 8878 §3.1.1.2.4). */
const BLOCK_MAX = 1 << 17;

/** The layout of a frame: its length, its stated content size, whether a checksum ends it, and whether it has a window descriptor. */
interface Frame {
  length: number;
  contentSize: number | null;
  checksum: boolean;
  windowed: boolean;
}

/** Little-endian integer of `length` bytes at `at`; `null` beyond the end. */
function readLe(data: Uint8Array, at: number, length: number): bigint | null {
  if (at + length > data.length) return null;
  let value = 0n;
  for (let i = length - 1; i >= 0; i--) value = (value << 8n) | BigInt(data[at + i]!);
  return value;
}

/** The layout of the frame at the start of `data` (after the magic number); `null` when its header or block headers are malformed, it names a dictionary, or it states a content size over `max`. */
function frameLayout(data: Uint8Array, max: number): Frame | null {
  const descriptor = data[4];
  if (descriptor === undefined || (descriptor & 0x08) !== 0) return null;
  const single = (descriptor & 0x20) !== 0;
  let at = 5;
  if (!single) {
    const window = data[at++];
    if (window === undefined || 10 + (window >> 3) > MAX_WINDOW_LOG) return null;
  }
  const idLength = [0, 1, 2, 4][descriptor & 3]!;
  if (readLe(data, at, idLength) !== 0n) return null;
  at += idLength;
  const sizeFlag = descriptor >> 6;
  const sizeLength = sizeFlag === 0 ? (single ? 1 : 0) : 1 << sizeFlag;
  let contentSize: number | null = null;
  if (sizeLength > 0) {
    const stated = readLe(data, at, sizeLength);
    if (stated === null) return null;
    const size = sizeFlag === 1 ? stated + 256n : stated;
    if (size > BigInt(max)) return null;
    contentSize = Number(size);
    at += sizeLength;
  }
  for (;;) {
    const head = readLe(data, at, 3);
    if (head === null) return null;
    const [last, kind, size] = [Number(head & 1n), Number((head >> 1n) & 3n), Number(head >> 3n)];
    if (kind === 3) return null;
    at += 3 + (kind === 1 ? 1 : size);
    if (at > data.length) return null;
    if (last === 1) break;
  }
  const checksum = (descriptor & 0x04) !== 0;
  if (checksum) at += 4;
  return at > data.length ? null : { length: at, contentSize, checksum, windowed: !single };
}

const MASK = (1n << 64n) - 1n;
const [P1, P2, P3, P4, P5] = [0x9e3779b185ebca87n, 0xc2b2ae3d27d4eb4fn, 0x165667b19e3779f9n, 0x85ebca77c2b2ae63n, 0x27d4eb2f165667c5n];
const rotl = (x: bigint, r: bigint): bigint => ((x << r) | (x >> (64n - r))) & MASK;
const round = (acc: bigint, input: bigint): bigint => (rotl((acc + input * P2) & MASK, 31n) * P1) & MASK;

/** XXH64 with seed 0 (the hash of a zstd frame's content checksum, RFC 8878 §3.1.1). */
export function xxh64(data: Uint8Array): bigint {
  const word = (at: number, length: number): bigint => readLe(data, at, length)!;
  let at = 0;
  let hash: bigint;
  if (data.length >= 32) {
    const lanes: [bigint, bigint, bigint, bigint] = [(P1 + P2) & MASK, P2, 0n, (MASK + 1n - P1) & MASK];
    for (; at + 32 <= data.length; at += 32) {
      for (let lane = 0; lane < 4; lane++) lanes[lane] = round(lanes[lane]!, word(at + 8 * lane, 8));
    }
    hash = (rotl(lanes[0], 1n) + rotl(lanes[1], 7n) + rotl(lanes[2], 12n) + rotl(lanes[3], 18n)) & MASK;
    for (const lane of lanes) hash = ((hash ^ round(0n, lane)) * P1 + P4) & MASK;
  } else {
    hash = P5;
  }
  hash = (hash + BigInt(data.length)) & MASK;
  for (; at + 8 <= data.length; at += 8) hash = (rotl(hash ^ round(0n, word(at, 8)), 27n) * P1 + P4) & MASK;
  if (at + 4 <= data.length) {
    hash = (rotl(hash ^ ((word(at, 4) * P1) & MASK), 23n) * P2 + P3) & MASK;
    at += 4;
  }
  for (; at < data.length; at++) hash = (rotl(hash ^ ((BigInt(data[at]!) * P5) & MASK), 11n) * P1) & MASK;
  hash = ((hash ^ (hash >> 33n)) * P2) & MASK;
  hash = ((hash ^ (hash >> 29n)) * P3) & MASK;
  return hash ^ (hash >> 32n);
}

/** The content of the zstd frame `frame`, `null` when it does not decode or its content exceeds `max` bytes. */
function frameContent(frame: Uint8Array, layout: Frame, max: number): Uint8Array | null {
  let input = frame;
  if (layout.windowed) {
    // The decoder allocates the window the frame states. No match reaches further back than the content, which `max` bounds,
    // so a window of at least `max` bytes (and a block's 128 KiB) decodes the same content
    input = Uint8Array.from(frame);
    input[5] = (Math.ceil(Math.log2(Math.max(max, BLOCK_MAX))) - 10) << 3;
  }
  const chunks: Uint8Array[] = [];
  let length = 0;
  try {
    new Decompress((chunk) => {
      length += chunk.length;
      if (length > max) throw new Error('the content exceeds the bound');
      chunks.push(chunk);
    }).push(input, true);
  } catch {
    return null;
  }
  return concatBytes(...chunks);
}

/**
 * The payload in envelope data `data` (the envelope's chunks concatenated): `data` itself when it does not start with
 * `ZSTD_MAGIC`, otherwise the content of its zstd frame. `null` when the frame is malformed, names a dictionary, decompresses to
 * more than `max` bytes or to nothing, does not match its content size or checksum, or is followed by anything other than `0x00`
 * bytes.
 */
export function unpackEnvelopeData(data: Uint8Array, max: number = MAX_SETTLEMENT_TX_BYTES): Uint8Array | null {
  if (!ZSTD_MAGIC.every((byte, i) => data[i] === byte)) return data;
  const layout = frameLayout(data, max);
  if (layout === null) return null;
  for (let i = layout.length; i < data.length; i++) if (data[i] !== 0) return null;
  const frame = data.subarray(0, layout.length);
  const content = frameContent(frame, layout, max);
  if (content === null || content.length === 0) return null;
  if (layout.contentSize !== null && layout.contentSize !== content.length) return null;
  if (layout.checksum && (xxh64(content) & 0xffff_ffffn) !== readLe(frame, frame.length - 4, 4)) return null;
  return content;
}
