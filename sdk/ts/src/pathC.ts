/**
 * Path C: native transactions (SPEC §5.3, §7.1, §7.4; D-032, D-036). Mirrors `crates/ksana-btc/src/data.rs` and `native.rs`:
 *
 * - `ksn1DataScript`: `OP_RETURN <magic> <chunk>…`; the chunks are the payload in 520-byte pieces (the last one may be shorter),
 *   written like rust-bitcoin's `push_slice` (a 1-byte chunk is also written as `OP_PUSHBYTES_1`).
 * - `parseKsn1DataScript` / `txData` / `txPayload` / `nativeOpFromTx`: parsing rules as in D-032 and D-081 (non-minimal pushes
 *   accepted; the KSN1 outputs and the envelopes in the inputs' witnesses (`envelope.ts`) together exactly one; a KSN1 output
 *   carries only a path C payload; a coinbase is not a native transaction, D-086).
 * - `nativePayload`: the Borsh encoding of `Payload::NativeOp { op }` (`op.fee` must be `BtcNative`, `op.chain_id` the network's).
 * - `nativeFirstInput` / `nativeTxFirstInput`: the first input of the carrying transaction that the operation's signature is bound
 *   to (D-086): before building, sign the operation with `nativeFirstInput(utxos[0])` as `firstInput` (`signVmop` /
 *   `signVmopBip322` / `signVmopWithWallet`).
 * - `buildNativeTx`: given UTXOs, a change script and a fee rate, builds the unsigned PSBT with the data output (and optional
 *   payment outputs, e.g. a top-up, D-078) for the wallet to sign with `SIGHASH_ALL` (the equivalent `SIGHASH_DEFAULT` for P2TR);
 *   the operation's signature must be bound to `utxos[0]`, otherwise building is refused; `signNativeTx` / `extractTransaction`
 *   sign with a local private key (tests).
 *
 * Every function takes the parameters of the network the transaction is for: the node's (`KsanaRpcClient.getParams`). An
 * operation signed for another network's chain ID would still be force-included there, and fail while its BTC payment
 * confirms, so building refuses it (A2-11, D-106).
 */

import { Transaction } from '@scure/btc-signer';

import { verifyVmopBip322 } from './bip322.js';
import { assertBytes, compactSize, concatBytes, equalBytes, reverseBytes } from './bytes.js';
import { decodePayload, encodePayload, encodeVmOp } from './codec.js';
import { ChainMismatchError, type Params } from './params.js';
import { verifyVmop } from './sign.js';
import { OP_RETURN, dustLimit, isP2tr, isP2wpkh, isStandardOutputScript, pushData, readPush } from './btc/script.js';
import { isKsn1Envelope, parseKsn1Envelope, witnessLeaf } from './envelope.js';
import { SCURE_TX_OPTIONS, signPsbtWithKey } from './btc/signer.js';
import { type Tx, type TxOut, parseTx } from './btc/tx.js';
import type { OutPoint, Payload, VmOp } from './types.js';

/** Maximum length of one data chunk (SPEC §7.1; also the limit of a single push in a BTC script). */
export const MAX_CHUNK = 520;

/** Bitcoin Core's standard transaction weight limit (`MAX_STANDARD_TX_WEIGHT`). */
export const MAX_STANDARD_TX_WEIGHT = 400_000;

/**
 * Locking script of a KSN1 data output: `OP_RETURN <magic> <chunk>…` (D-032). The payload cannot be empty (the Borsh encoding of
 * any payload is at least 1 byte).
 */
export function ksn1DataScript(magic: Uint8Array, payload: Uint8Array): Uint8Array {
  assertBytes(magic, 4, 'magic');
  assertBytes(payload, undefined, 'payload');
  if (payload.length === 0) throw new Error('payload is empty');
  const parts = [Uint8Array.of(OP_RETURN), pushData(magic)];
  for (let offset = 0; offset < payload.length; offset += MAX_CHUNK) {
    parts.push(pushData(payload.subarray(offset, offset + MAX_CHUNK)));
  }
  return concatBytes(...parts);
}

/** If the script starts with `OP_RETURN <magic>` (any push encoding), returns the position after it. */
function ksn1Body(magic: Uint8Array, script: Uint8Array): number | null {
  if (script[0] !== OP_RETURN) return null;
  const push = readPush(script, 1);
  if (push === null || !equalBytes(push[0], magic)) return null;
  return push[1];
}

/** Whether this is a KSN1 output: starts with `OP_RETURN` followed by a push whose data is exactly `magic` (only the start is checked). */
export function isKsn1Output(magic: Uint8Array, script: Uint8Array): boolean {
  return ksn1Body(magic, script) !== null;
}

/**
 * Parses a KSN1 data script per D-032 and returns the payload, i.e. the data chunks concatenated in order: after the magic only
 * data pushes may follow (`OP_PUSHBYTES_0..75`, `OP_PUSHDATA1/2/4`, non-minimal encodings accepted), each at most 520 bytes;
 * otherwise returns `null`.
 */
export function parseKsn1DataScript(magic: Uint8Array, script: Uint8Array): Uint8Array | null {
  let at = ksn1Body(magic, script);
  if (at === null) return null;
  const chunks: Uint8Array[] = [];
  while (at < script.length) {
    const push = readPush(script, at);
    if (push === null || push[0].length > MAX_CHUNK) return null;
    chunks.push(push[0]);
    at = push[1];
  }
  return concatBytes(...chunks);
}

/**
 * Where a transaction carries its KSN1 data (D-081): a data output (`Output`, by output index) or an envelope in the witness of an
 * input (`Input`, by input index). `Carrier` in `crates/ksana-btc/src/data.rs`.
 */
export interface Carrier {
  kind: 'Output' | 'Input';
  index: number;
}

/** The KSN1 data of a transaction (D-032, D-081). */
export type TxData =
  /** No data carrier. */
  | { kind: 'none' }
  /** Exactly one data carrier, and it is well-formed: a KSN1 output's data script or an input's envelope. */
  | { kind: 'one'; carrier: Carrier; payload: Uint8Array }
  /** More than one data carrier, or the only one is malformed: the transaction is not recognized. */
  | { kind: 'invalid' };

/**
 * Extracts the KSN1 data of a transaction as `tx_data` does: its two kinds of data carrier, the KSN1 outputs (an OP_RETURN that
 * does not start with the magic does not count) and the inputs whose witness leaf is a KSN1 envelope (`isKsn1Envelope`), counted
 * together; `one` when they total exactly one and that one is well-formed. A transaction with a KSN1 output and an envelope is
 * not recognized at all.
 */
export function txData(magic: Uint8Array, tx: Tx): TxData {
  let found: { carrier: Carrier; script: Uint8Array } | null = null;
  for (let index = 0; index < tx.outputs.length; index++) {
    const script = tx.outputs[index]!.script;
    if (!isKsn1Output(magic, script)) continue;
    if (found !== null) return { kind: 'invalid' };
    found = { carrier: { kind: 'Output', index }, script };
  }
  for (let index = 0; index < tx.inputs.length; index++) {
    const leaf = witnessLeaf(tx.inputs[index]!.witness);
    if (leaf === null || !isKsn1Envelope(magic, leaf)) continue;
    if (found !== null) return { kind: 'invalid' };
    found = { carrier: { kind: 'Input', index }, script: leaf };
  }
  if (found === null) return { kind: 'none' };
  const payload = found.carrier.kind === 'Output' ? parseKsn1DataScript(magic, found.script) : (parseKsn1Envelope(magic, found.script)?.payload ?? null);
  return payload === null ? { kind: 'invalid' } : { kind: 'one', carrier: found.carrier, payload };
}

/**
 * The KSN1 payload of a transaction, as `tx_payload` gives it: returned when the transaction has exactly one well-formed data
 * carrier (`txData`), the payload is within `MAX_BLOCK_DATA_BYTES` and decodes completely, and the carrier is the payload's own
 * (D-081): a KSN1 output carries only a path C payload (`NativeOp`), an envelope only a settlement or a republish payload
 * (`Settlement`, `Headers`).
 */
export function txPayload(tx: Tx | Uint8Array, params: Params): { carrier: Carrier; payload: Payload } | null {
  let parsed: Tx;
  try {
    parsed = tx instanceof Uint8Array ? parseTx(tx) : tx;
  } catch {
    return null;
  }
  const data = txData(params.magic, parsed);
  if (data.kind !== 'one') return null;
  let payload: Payload;
  try {
    payload = decodePayload(data.payload, params);
  } catch {
    return null;
  }
  const fits = data.carrier.kind === 'Output' ? payload.kind === 'NativeOp' : payload.kind !== 'NativeOp';
  return fits ? { carrier: data.carrier, payload } : null;
}

/** Whether this is a coinbase transaction: its only input spends `000…000:0xFFFFFFFF`. */
function isCoinbase(tx: Tx): boolean {
  const input = tx.inputs[0];
  return tx.inputs.length === 1 && input !== undefined && input.vout === 0xffff_ffff && input.txid.every((b) => b === 0);
}

/**
 * The output spent by the transaction's first input (internal byte order): the value a path C operation's signature is bound to
 * (D-086). Returns `null` for a coinbase transaction (it has no real input).
 */
export function nativeTxFirstInput(tx: Tx | Uint8Array): OutPoint | null {
  let parsed: Tx;
  try {
    parsed = tx instanceof Uint8Array ? parseTx(tx) : tx;
  } catch {
    return null;
  }
  const input = parsed.inputs[0];
  if (input === undefined || isCoinbase(parsed)) return null;
  return { txid: Uint8Array.from(input.txid), vout: input.vout };
}

/**
 * The outpoint of the UTXO that is the native transaction's first input (D-086): the `firstInput` when signing a path C operation.
 */
export function nativeFirstInput(utxo: Pick<Utxo, 'txid' | 'vout'>): OutPoint {
  if (!Number.isInteger(utxo.vout) || utxo.vout < 0 || utxo.vout > 0xffff_ffff) throw new RangeError('UTXO vout is out of u32 range');
  return { txid: Uint8Array.from(assertBytes(utxo.txid, 32, 'UTXO txid')), vout: utxo.vout };
}

/**
 * Whether the transaction is a native transaction (§7.4): if so, returns its VmOp (the payload is `NativeOp` and `fee` is
 * `BtcNative`; a coinbase is not one, D-086). The signature is not checked: verify it against `nativeTxFirstInput(tx)` with
 * `verifyVmop` / `verifyVmopBip322`.
 */
export function nativeOpFromTx(tx: Tx | Uint8Array, params: Params): VmOp | null {
  let parsed: Tx;
  try {
    parsed = tx instanceof Uint8Array ? parseTx(tx) : tx;
  } catch {
    return null;
  }
  if (isCoinbase(parsed)) return null;
  const found = txPayload(parsed, params);
  if (found === null || found.payload.kind !== 'NativeOp' || found.payload.op.fee.kind !== 'BtcNative') return null;
  return found.payload.op;
}

/**
 * Whether the operation's signature is bound to the outpoint `firstInput` (D-086): `verifyVmop` for `Bip340`, the local
 * self-check `verifyVmopBip322` for `Bip322Simple`.
 */
export function isBoundTo(op: VmOp, firstInput: OutPoint): boolean {
  return op.sig.kind === 'Bip340' ? verifyVmop(op, firstInput) === 'valid' : verifyVmopBip322(op, firstInput);
}

/**
 * Payload of a native transaction on the network of `params`: the Borsh encoding of `Payload::NativeOp { op }`. `op.fee` must
 * be `BtcNative`, the VmOp encoding must be within `MAX_OP_BYTES` and the payload within `MAX_BLOCK_DATA_BYTES` (otherwise
 * nodes do not recognize it), and `op.chain_id` must be the network's (otherwise `ChainMismatchError`: the node would include
 * the operation and fail it while the BTC transaction confirms, A2-11).
 */
export function nativePayload(op: VmOp, params: Params): Uint8Array {
  if (params === undefined) throw new TypeError("params is required: the parameters of the node's network (KsanaRpcClient.getParams)");
  if (op.fee.kind !== 'BtcNative') throw new Error('the fee of a path C VmOp must be BtcNative');
  if (op.chain_id !== params.chain_id) {
    throw new ChainMismatchError(
      op.chain_id,
      params.chain_id,
      `the VmOp is signed for chain ID ${op.chain_id}, not ${params.chain_id} of this network: it would be included and fail while the BTC transaction confirms`,
    );
  }
  const opBytes = encodeVmOp(op).length;
  if (opBytes > params.max_op_bytes) throw new Error(`VmOp encodes to ${opBytes} bytes, exceeding MAX_OP_BYTES (${params.max_op_bytes})`);
  const payload = encodePayload({ kind: 'NativeOp', op }, params);
  if (payload.length > params.max_block_data_bytes) {
    throw new Error(`payload is ${payload.length} bytes, exceeding MAX_BLOCK_DATA_BYTES (${params.max_block_data_bytes})`);
  }
  return payload;
}

/** The KSN1 data output of a native transaction on the network of `params` (value 0). */
export function nativeDataOutput(op: VmOp, params: Params): TxOut {
  const payload = nativePayload(op, params);
  return { value: 0n, script: ksn1DataScript(params.magic, payload) };
}

/** A spendable UTXO (P2TR key-path or P2WPKH). */
export interface Utxo {
  /**
   * Transaction ID (internal byte order, D-001; the display byte order returned by bitcoind RPC / wallets is converted with
   * `txidFromRpc`).
   */
  txid: Uint8Array;
  /** Output index. */
  vout: number;
  /** Value (sats). */
  value: bigint;
  /** Locking script. */
  script: Uint8Array;
  /** Internal key of a P2TR output (x-only), written into the PSBT to help the wallet sign. */
  tapInternalKey?: Uint8Array;
}

/** Arguments of `buildNativeTx`. */
export interface BuildNativeTxArgs {
  /**
   * The signed VmOp (`fee` is `BtcNative`); its signature is bound to `utxos[0]` (signed with `nativeFirstInput(utxos[0])` as
   * `firstInput`, D-086).
   */
  op: VmOp;
  /**
   * Candidate UTXOs, selected in the given order; the first one is always selected, and the operation's signature must be bound
   * to it.
   */
  utxos: readonly Utxo[];
  /** Locking script of the change output. */
  changeScript: Uint8Array;
  /** Fee rate (sat/vB; may be fractional, rounded up at a precision of one thousandth of a sat). */
  feeRate: number;
  /**
   * Network parameters (chain ID, magic, length limits): the node's, from `KsanaRpcClient.getParams`; `op.chain_id` must be
   * this network's (A2-11, D-106).
   */
  params: Params;
  /** `nSequence` of every input; defaults to `0xFFFFFFFD` (RBF enabled, so the fee can be raised). */
  sequence?: number;
  /**
   * Further outputs, placed in the given order after the data output and before the change: e.g. the amount paid to the payment
   * script of an exchange-contract lock in a top-up (D-078, D-085). The locking script must be a standard script and the value
   * not below its dust limit.
   */
  payments?: readonly TxOut[];
}

/** Result of `buildNativeTx`. */
export interface NativeTxDraft {
  /** The unsigned PSBT (v0): output 0 is the KSN1 data output, followed by `payments` and the change (if any). */
  psbt: Uint8Array;
  /** The selected UTXOs (the PSBT inputs, in the same order). */
  inputs: Utxo[];
  /** The change output; omitted when below the dust limit (the excess goes to the fee). */
  change: TxOut | null;
  /** Fee (sats). */
  fee: bigint;
  /** Virtual size estimated from the witnesses after signing. */
  vsize: number;
}

/**
 * Estimated weight of a signed input: P2TR key-path with a 65-byte signature (including the explicit sighash byte), P2WPKH with
 * a 72-byte DER signature.
 */
function inputWeight(script: Uint8Array): number {
  const base = (32 + 4 + 1 + 4) * 4;
  if (isP2tr(script)) return base + 1 + 1 + 65;
  if (isP2wpkh(script)) return base + 1 + 1 + 72 + 1 + 33;
  throw new Error('only P2TR and P2WPKH UTXOs are supported');
}

function outputWeight(script: Uint8Array): number {
  return (8 + compactSize(script.length).length + script.length) * 4;
}

function feeFor(vsize: number, feeRate: number): bigint {
  const milli = BigInt(Math.round(feeRate * 1000));
  return (BigInt(vsize) * milli + 999n) / 1000n;
}

/**
 * Builds a path C native transaction (unsigned PSBT, v2, `nLockTime = 0`): output 0 is the KSN1 data output, then `payments`,
 * then the change. Selects `utxos` in order until they cover the payments, the fee and a non-dust change; when all are used up
 * and there is still not enough for the change but enough for the payments and the fee, the change is omitted.
 * The operation's signature must be bound to the first UTXO (`isBoundTo(op, nativeFirstInput(utxos[0]))`, D-086), otherwise the
 * node decides that it failed authentication; its chain ID must be that of `params` (`ChainMismatchError`, A2-11).
 */
export function buildNativeTx(args: BuildNativeTxArgs): NativeTxDraft {
  const params = args.params;
  if (!(args.feeRate > 0) || !Number.isFinite(args.feeRate)) throw new RangeError('fee rate must be a positive number (sat/vB)');
  const sequence = args.sequence ?? 0xffff_fffd;
  if (!Number.isInteger(sequence) || sequence < 0 || sequence > 0xffff_ffff) throw new RangeError('nSequence is out of u32 range');
  const data = nativeDataOutput(args.op, params);
  if (!isStandardOutputScript(args.changeScript)) {
    throw new Error('the change locking script must be P2PKH, P2SH, P2WPKH, P2WSH or P2TR');
  }
  const change: TxOut = { value: 0n, script: Uint8Array.from(args.changeScript) };
  const changeDust = dustLimit(change.script);
  const payments = (args.payments ?? []).map((p, i) => {
    if (!isStandardOutputScript(p.script)) {
      throw new Error(`the locking script of payment ${i} must be P2PKH, P2SH, P2WPKH, P2WSH or P2TR`);
    }
    if (typeof p.value !== 'bigint' || p.value < dustLimit(p.script)) {
      throw new RangeError(`the value of payment ${i} is below the dust limit of ${dustLimit(p.script)} sats`);
    }
    return { value: p.value, script: Uint8Array.from(p.script) };
  });
  const paid = payments.reduce((sum, p) => sum + p.value, 0n);
  const overhead = (4 + 4) * 4 + 2; // version, nLockTime, segwit marker
  const weightOf = (inputs: readonly Utxo[], withChange: boolean): number => {
    const outputs = withChange ? [data, ...payments, change] : [data, ...payments];
    return (
      overhead +
      compactSize(inputs.length).length * 4 +
      compactSize(outputs.length).length * 4 +
      inputs.reduce((w, u) => w + inputWeight(u.script), 0) +
      outputs.reduce((w, o) => w + outputWeight(o.script), 0)
    );
  };
  const selected: Utxo[] = [];
  let total = 0n;
  let result: { change: TxOut | null; fee: bigint; vsize: number } | null = null;
  for (const utxo of args.utxos) {
    inputWeight(utxo.script);
    if (typeof utxo.value !== 'bigint' || utxo.value < 0n) throw new RangeError('UTXO value must be a non-negative bigint');
    selected.push(utxo);
    total += utxo.value;
    const weight = weightOf(selected, true);
    if (weight > MAX_STANDARD_TX_WEIGHT) {
      throw new Error(`with ${selected.length} UTXOs selected the transaction exceeds the standard weight limit (${MAX_STANDARD_TX_WEIGHT} WU)`);
    }
    const vsize = Math.ceil(weight / 4);
    const fee = feeFor(vsize, args.feeRate);
    if (total >= paid + fee + changeDust) {
      result = { change: { value: total - paid - fee, script: change.script }, fee, vsize };
      break;
    }
  }
  if (result === null) {
    const vsize = Math.ceil(weightOf(selected, false) / 4);
    const fee = feeFor(vsize, args.feeRate);
    if (selected.length === 0 || total < paid + fee) {
      throw new Error(`insufficient funds: the UTXOs total ${total} sats; the payments of ${paid} sats plus the fee need at least ${paid + fee} sats`);
    }
    result = { change: null, fee: total - paid, vsize };
  }
  if (!isBoundTo(args.op, nativeFirstInput(selected[0]!))) {
    throw new Error('the VmOp signature is not bound to the first UTXO: pick utxos[0] first and sign with nativeFirstInput(utxos[0]) as firstInput (D-086)');
  }
  const tx = new Transaction({ version: 2, lockTime: 0, ...SCURE_TX_OPTIONS });
  for (const utxo of selected) {
    tx.addInput({
      txid: reverseBytes(assertBytes(utxo.txid, 32, 'UTXO txid')),
      index: utxo.vout,
      sequence,
      witnessUtxo: { script: utxo.script, amount: utxo.value },
      ...(utxo.tapInternalKey !== undefined ? { tapInternalKey: utxo.tapInternalKey } : {}),
    });
  }
  tx.addOutput({ script: data.script, amount: 0n });
  for (const payment of payments) tx.addOutput({ script: payment.script, amount: payment.value });
  if (result.change !== null) tx.addOutput({ script: result.change.script, amount: result.change.value });
  return { psbt: tx.toPSBT(0), inputs: selected, change: result.change, fee: result.fee, vsize: result.vsize };
}

/**
 * Signs the PSBT of a native transaction with a local private key (every input belonging to that key; `SIGHASH_DEFAULT` for P2TR,
 * `SIGHASH_ALL` for P2WPKH) and returns the finalized PSBT; use `extractTransaction` to get the broadcastable transaction.
 */
export function signNativeTx(psbt: Uint8Array, secretKey: Uint8Array, auxRand?: Uint8Array): Uint8Array {
  return signPsbtWithKey(psbt, secretKey, auxRand === undefined ? {} : { auxRand });
}
