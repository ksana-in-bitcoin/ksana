/**
 * The KSN1 envelope (D-081), the carrier of settlement and republish payloads: a Taproot leaf script
 * `OP_PUSHBYTES_32 <key> OP_CHECKSIG OP_FALSE OP_IF <magic> <chunk>… OP_ENDIF` revealed in the witness of the input spending the
 * commitment output. Mirrors `crates/ksana-btc/src/envelope.rs`:
 *
 * - `ksn1EnvelopeScript`: the leaf script, the payload in 520-byte chunks written like rust-bitcoin's `push_slice`;
 * - `witnessLeaf`: the leaf script of a witness by BIP341 (the annex removed first);
 * - `isKsn1Envelope`: whether a leaf is an envelope, judged by its start (the key, the three opcodes and the magic, any push
 *   encoding); `parseKsn1Envelope`: its key and payload, when the rest is data pushes of at most 520 bytes each and the script ends
 *   with its `OP_ENDIF`.
 *
 * A transaction's data carriers, the KSN1 outputs and the envelopes together, are counted by `txData` (`pathC.ts`).
 */

import { assertBytes, concatBytes, equalBytes } from './bytes.js';
import { OP_CHECKSIG, OP_0, pushData, readPush } from './btc/script.js';
import { isValidXOnlyPublicKey } from './keys.js';

const OP_PUSHBYTES_32 = 0x20;
const OP_IF = 0x63;
const OP_ENDIF = 0x68;
/** The first byte of a BIP341 annex. */
const ANNEX_TAG = 0x50;
/** The longest data chunk (the limit of one push in a script). */
const MAX_CHUNK = 520;

/**
 * The envelope leaf script with `key` (an x-only public key: the producer's one-time key for this envelope) carrying `payload`.
 * The payload cannot be empty.
 */
export function ksn1EnvelopeScript(key: Uint8Array, magic: Uint8Array, payload: Uint8Array): Uint8Array {
  assertBytes(key, 32, 'key');
  assertBytes(magic, 4, 'magic');
  assertBytes(payload, undefined, 'payload');
  if (!isValidXOnlyPublicKey(key)) throw new Error('the key is not a valid x-only public key');
  if (payload.length === 0) throw new Error('payload is empty');
  const parts = [pushData(key), Uint8Array.of(OP_CHECKSIG, OP_0, OP_IF), pushData(magic)];
  for (let offset = 0; offset < payload.length; offset += MAX_CHUNK) {
    parts.push(pushData(payload.subarray(offset, offset + MAX_CHUNK)));
  }
  parts.push(Uint8Array.of(OP_ENDIF));
  return concatBytes(...parts);
}

/**
 * The leaf script of a witness (BIP341): when it has at least two items and the last starts with `0x50`, the last is the annex and
 * is left out; then the second-to-last item, or `null` with fewer than two items.
 */
export function witnessLeaf(witness: readonly Uint8Array[]): Uint8Array | null {
  const annex = witness.length >= 2 && witness[witness.length - 1]![0] === ANNEX_TAG;
  const length = annex ? witness.length - 1 : witness.length;
  return length >= 2 ? witness[length - 2]! : null;
}

/** The key and the position after the magic, when the leaf starts as an envelope does. */
function envelopeBody(magic: Uint8Array, leaf: Uint8Array): [Uint8Array, number] | null {
  if (leaf.length < 36 || leaf[0] !== OP_PUSHBYTES_32 || leaf[33] !== OP_CHECKSIG || leaf[34] !== OP_0 || leaf[35] !== OP_IF) return null;
  const push = readPush(leaf, 36);
  if (push === null || !equalBytes(push[0], magic)) return null;
  return [leaf.subarray(1, 33), push[1]];
}

/** Whether a leaf script is a KSN1 envelope: judged by its start alone, as a KSN1 output is (`isKsn1Output`). */
export function isKsn1Envelope(magic: Uint8Array, leaf: Uint8Array): boolean {
  return envelopeBody(magic, leaf) !== null;
}

/** A parsed envelope: its key, its payload (the chunks concatenated in order) and the number of chunks. */
export interface Ksn1Envelope {
  key: Uint8Array;
  payload: Uint8Array;
  chunks: number;
}

/**
 * Parses a KSN1 envelope: after the magic only data pushes of at most 520 bytes each (any push encoding), then `OP_ENDIF` as the
 * script's last byte; otherwise `null`.
 */
export function parseKsn1Envelope(magic: Uint8Array, leaf: Uint8Array): Ksn1Envelope | null {
  const body = envelopeBody(magic, leaf);
  if (body === null) return null;
  const [key, start] = body;
  const chunks: Uint8Array[] = [];
  let at = start;
  while (!(at === leaf.length - 1 && leaf[at] === OP_ENDIF)) {
    const push = readPush(leaf, at);
    if (push === null || push[0].length > MAX_CHUNK) return null;
    chunks.push(push[0]);
    at = push[1];
  }
  return { key: Uint8Array.from(key), payload: concatBytes(...chunks), chunks: chunks.length };
}
