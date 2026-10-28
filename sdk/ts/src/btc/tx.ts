/**
 * Bitcoin transactions: serialization / parsing, transaction ID, weight, and the BIP143 (witness v0) and BIP341 (Taproot
 * key-path) sighashes.
 *
 * Transaction IDs are always in **internal byte order** (the raw sha256d output, D-001).
 */

import {
  ByteReader,
  compactSize,
  concatBytes,
  i32le,
  reverseBytes,
  toHex,
  u32le,
  u64le,
  varBytes,
} from '../bytes.js';
import { sha256, sha256d, taggedHash } from '../hash.js';

/** Transaction input. */
export interface TxIn {
  /** ID of the transaction being spent (internal byte order). */
  txid: Uint8Array;
  /** Index of the output being spent. */
  vout: number;
  /** scriptSig. */
  scriptSig: Uint8Array;
  /** nSequence. */
  sequence: number;
  /** Witness stack (an empty array when there is none). */
  witness: Uint8Array[];
}

/** Transaction output (also used for the previous output being spent: amount and locking script). */
export interface TxOut {
  /** Amount (sats). */
  value: bigint;
  /** Locking script (scriptPubKey). */
  script: Uint8Array;
}

/** Transaction. */
export interface Tx {
  /** nVersion (signed 32-bit). */
  version: number;
  /** Inputs. */
  inputs: TxIn[];
  /** Outputs. */
  outputs: TxOut[];
  /** nLockTime. */
  locktime: number;
}

/** Transaction ID in Bitcoin's internal byte order → the hex shown by bitcoind RPC and block explorers (D-001). */
export function txidToRpc(txid: Uint8Array): string {
  return toHex(reverseBytes(txid));
}

/** Transaction ID (hex) as shown by bitcoind RPC and block explorers → internal byte order (D-001). */
export function txidFromRpc(hex: string): Uint8Array {
  if (!/^[0-9a-fA-F]{64}$/.test(hex)) throw new Error('the transaction ID must be 64 hex digits');
  const out = new Uint8Array(32);
  for (let i = 0; i < 32; i++) out[31 - i] = parseInt(hex.slice(i * 2, i * 2 + 2), 16);
  return out;
}

/** Consensus encoding of a witness stack: number of items + each item (length-prefixed). A BIP322 simple signature is exactly this encoding (D-052). */
export function encodeWitness(items: readonly Uint8Array[]): Uint8Array {
  return concatBytes(compactSize(items.length), ...items.map(varBytes));
}

/** Decodes the consensus encoding of a witness stack, requiring the input to be fully consumed. */
export function decodeWitness(bytes: Uint8Array): Uint8Array[] {
  const reader = new ByteReader(bytes, 'witness');
  const items = readWitness(reader);
  reader.finish();
  return items;
}

function readWitness(reader: ByteReader): Uint8Array[] {
  const count = reader.compactSize();
  if (count > reader.remaining) throw new Error('the number of witness items exceeds the data length');
  const items: Uint8Array[] = [];
  for (let i = 0; i < count; i++) items.push(reader.varBytes());
  return items;
}

function encodeOutputs(outputs: readonly TxOut[]): Uint8Array {
  return concatBytes(compactSize(outputs.length), ...outputs.map(encodeOutput));
}

/** Serialization of a single output: amount (u64 little-endian) + length-prefixed script. */
export function encodeOutput(out: TxOut): Uint8Array {
  return concatBytes(u64le(out.value), varBytes(out.script));
}

function encodeOutpoint(txid: Uint8Array, vout: number): Uint8Array {
  if (txid.length !== 32) throw new Error('the transaction ID must be 32 bytes');
  return concatBytes(txid, u32le(vout));
}

/**
 * Network serialization of a transaction. The BIP144 segwit format is used when `withWitness` is set and at least one input
 * has a witness; a transaction without inputs is always in the legacy format (otherwise it would be confused with the segwit
 * marker and Bitcoin Core could not parse it).
 */
export function serializeTx(tx: Tx, withWitness = true): Uint8Array {
  const segwit = withWitness && tx.inputs.length > 0 && tx.inputs.some((i) => i.witness.length > 0);
  const inputs = tx.inputs.map((i) => concatBytes(encodeOutpoint(i.txid, i.vout), varBytes(i.scriptSig), u32le(i.sequence)));
  return concatBytes(
    i32le(tx.version),
    segwit ? Uint8Array.of(0x00, 0x01) : new Uint8Array(0),
    compactSize(tx.inputs.length),
    ...inputs,
    encodeOutputs(tx.outputs),
    ...(segwit ? tx.inputs.map((i) => encodeWitness(i.witness)) : []),
    u32le(tx.locktime),
  );
}

/** Transaction ID (internal byte order): sha256d of the serialization without witnesses. */
export function txid(tx: Tx): Uint8Array {
  return sha256d(serializeTx(tx, false));
}

/** Transaction weight (BIP141): length without witnesses × 3 + full length. */
export function txWeight(tx: Tx): number {
  return serializeTx(tx, false).length * 3 + serializeTx(tx, true).length;
}

/** Virtual size: ⌈weight / 4⌉. */
export function txVsize(tx: Tx): number {
  return Math.ceil(txWeight(tx) / 4);
}

/** Parses the network serialization of a transaction (the BIP144 segwit format is supported), requiring the input to be fully consumed. */
export function parseTx(bytes: Uint8Array): Tx {
  const reader = new ByteReader(bytes, 'transaction');
  const version = reader.i32();
  let count = reader.compactSize();
  let segwit = false;
  if (count === 0) {
    const flag = reader.u8();
    if (flag !== 0x01) throw new Error('the transaction has no inputs, or the segwit marker is invalid');
    segwit = true;
    count = reader.compactSize();
  }
  const inputs: TxIn[] = [];
  for (let i = 0; i < count; i++) {
    const txid = reader.read(32);
    const vout = reader.u32();
    const scriptSig = reader.varBytes();
    const sequence = reader.u32();
    inputs.push({ txid, vout, scriptSig, sequence, witness: [] });
  }
  const outCount = reader.compactSize();
  const outputs: TxOut[] = [];
  for (let i = 0; i < outCount; i++) {
    const value = reader.u64();
    const script = reader.varBytes();
    outputs.push({ value, script });
  }
  if (segwit) {
    for (const input of inputs) input.witness = readWitness(reader);
    if (inputs.every((i) => i.witness.length === 0)) throw new Error('a segwit-format transaction has no witness at all');
  }
  const locktime = reader.u32();
  reader.finish();
  return { version, inputs, outputs, locktime };
}

// ---------------------------------------------------------------------------------------------
// Sighashes

/** Sighash types. */
export const SIGHASH_DEFAULT = 0x00;
export const SIGHASH_ALL = 0x01;
export const SIGHASH_NONE = 0x02;
export const SIGHASH_SINGLE = 0x03;
export const SIGHASH_ANYONECANPAY = 0x80;

function checkInputIndex(tx: Tx, index: number): void {
  if (!Number.isInteger(index) || index < 0 || index >= tx.inputs.length) {
    throw new Error(`input index ${index} is out of range`);
  }
}

/**
 * BIP143 (witness v0) sighash. `scriptCode` is the scriptCode of the input (for P2WPKH the corresponding P2PKH script, without
 * length prefix), `amount` is the value of the output being spent. With `SIGHASH_SINGLE` and no output at the same index,
 * hashOutputs is 0 (the same as Bitcoin Core).
 */
export function sighashBip143(tx: Tx, index: number, scriptCode: Uint8Array, amount: bigint, hashType: number): Uint8Array {
  checkInputIndex(tx, index);
  if (!Number.isInteger(hashType) || hashType < 0 || hashType > 0xffff_ffff) throw new Error('invalid sighash type');
  const base = hashType & 0x1f;
  const anyoneCanPay = (hashType & SIGHASH_ANYONECANPAY) !== 0;
  const zero: Uint8Array = new Uint8Array(32);
  const hashPrevouts = anyoneCanPay ? zero : sha256d(concatBytes(...tx.inputs.map((i) => encodeOutpoint(i.txid, i.vout))));
  const hashSequence =
    anyoneCanPay || base === SIGHASH_SINGLE || base === SIGHASH_NONE
      ? zero
      : sha256d(concatBytes(...tx.inputs.map((i) => u32le(i.sequence))));
  let hashOutputs: Uint8Array = zero;
  if (base !== SIGHASH_SINGLE && base !== SIGHASH_NONE) {
    hashOutputs = sha256d(concatBytes(...tx.outputs.map(encodeOutput)));
  } else if (base === SIGHASH_SINGLE && index < tx.outputs.length) {
    hashOutputs = sha256d(encodeOutput(tx.outputs[index]!));
  }
  const input = tx.inputs[index]!;
  return sha256d(
    concatBytes(
      i32le(tx.version),
      hashPrevouts,
      hashSequence,
      encodeOutpoint(input.txid, input.vout),
      varBytes(scriptCode),
      u64le(amount),
      u32le(input.sequence),
      hashOutputs,
      u32le(tx.locktime),
      u32le(hashType),
    ),
  );
}

/** The sighash types defined by BIP341 (key-path accepts only these values). */
export function isValidTaprootSighashType(hashType: number): boolean {
  return [0x00, 0x01, 0x02, 0x03, 0x81, 0x82, 0x83].includes(hashType);
}

/**
 * BIP341 key-path sighash (no annex): the `TapSighash` tagged hash of `0x00 ‖ SigMsg`.
 * `prevouts` are the outputs spent by all inputs (amount and locking script), one per input in the same order.
 */
export function sighashBip341KeyPath(tx: Tx, index: number, prevouts: readonly TxOut[], hashType: number): Uint8Array {
  checkInputIndex(tx, index);
  if (!isValidTaprootSighashType(hashType)) throw new Error(`invalid BIP341 sighash type 0x${hashType.toString(16)}`);
  if (prevouts.length !== tx.inputs.length) throw new Error('the number of prevouts must equal the number of inputs');
  const outputType = hashType === SIGHASH_DEFAULT ? SIGHASH_ALL : hashType & 0x03;
  const anyoneCanPay = (hashType & SIGHASH_ANYONECANPAY) !== 0;
  if (outputType === SIGHASH_SINGLE && index >= tx.outputs.length) {
    throw new Error('SIGHASH_SINGLE without an output at the same index (BIP341 does not define the sighash in this case)');
  }
  const parts: Uint8Array[] = [
    Uint8Array.of(0x00), // epoch
    Uint8Array.of(hashType),
    i32le(tx.version),
    u32le(tx.locktime),
  ];
  if (!anyoneCanPay) {
    parts.push(
      sha256(concatBytes(...tx.inputs.map((i) => encodeOutpoint(i.txid, i.vout)))),
      sha256(concatBytes(...prevouts.map((p) => u64le(p.value)))),
      sha256(concatBytes(...prevouts.map((p) => varBytes(p.script)))),
      sha256(concatBytes(...tx.inputs.map((i) => u32le(i.sequence)))),
    );
  }
  if (outputType === SIGHASH_ALL) {
    parts.push(sha256(concatBytes(...tx.outputs.map(encodeOutput))));
  }
  parts.push(Uint8Array.of(0x00)); // spend_type: key-path, no annex
  if (anyoneCanPay) {
    const input = tx.inputs[index]!;
    const prevout = prevouts[index]!;
    parts.push(encodeOutpoint(input.txid, input.vout), u64le(prevout.value), varBytes(prevout.script), u32le(input.sequence));
  } else {
    parts.push(u32le(index));
  }
  if (outputType === SIGHASH_SINGLE) {
    parts.push(sha256(encodeOutput(tx.outputs[index]!)));
  }
  return taggedHash('TapSighash', concatBytes(...parts));
}
