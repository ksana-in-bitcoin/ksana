/**
 * Signs the inputs of a PSBT with a local secret key (`@scure/btc-signer`), behaving like a wallet: only the inputs belonging
 * to the key are signed (BIP86 P2TR key-path, or P2WPKH of the compressed public key), the sighash type is taken from the PSBT
 * input's `sighash_type` (defaulting to `SIGHASH_DEFAULT` for P2TR and `SIGHASH_ALL` for P2WPKH) and must be within the
 * allowed set.
 *
 * ECDSA is RFC 6979 deterministic and low-S; BIP340 signatures are determined by `auxRand` (fresh randomness when not given).
 */

import { Transaction } from '@scure/btc-signer';

import { equalBytes } from '../bytes.js';
import { compressedPublicKey, xOnlyPublicKey } from '../keys.js';
import { p2trScript, p2wpkhScript } from './script.js';
import { SIGHASH_ALL, SIGHASH_DEFAULT } from './tx.js';

/** Options for parsing an external PSBT: allow arbitrary output scripts (OP_RETURN data outputs and the like). */
export const SCURE_TX_OPTIONS = Object.freeze({ allowUnknownOutputs: true });

/** Options of `signPsbtWithKey`. */
export interface SignPsbtOptions {
  /** Indices of the inputs to sign; defaults to all inputs belonging to the key. */
  inputs?: readonly number[];
  /** Allowed sighash types; defaults to each input's default type (`SIGHASH_DEFAULT` for P2TR, `SIGHASH_ALL` for P2WPKH). */
  sighashTypes?: readonly number[];
  /** BIP340 auxiliary randomness (32 bytes); fresh randomness is drawn each time when not given. */
  auxRand?: Uint8Array;
  /** Whether to finalize after signing (write `final_script_witness`); defaults to `true`. */
  finalize?: boolean;
}

/** Which kind of output of the local key an input belongs to. */
export type OwnedKind = 'p2tr' | 'p2wpkh';

/** Signs a PSBT with a local secret key and returns the new PSBT (v0). */
export function signPsbtWithKey(psbt: Uint8Array, secretKey: Uint8Array, options: SignPsbtOptions = {}): Uint8Array {
  const tx = Transaction.fromPSBT(psbt, SCURE_TX_OPTIONS);
  const internalKey = xOnlyPublicKey(secretKey);
  const trScript = p2trScript(internalKey);
  const wpkhScript = p2wpkhScript(compressedPublicKey(secretKey));
  const kindOf = (index: number): OwnedKind | null => {
    const prev = tx.getInput(index).witnessUtxo;
    if (prev === undefined) return null;
    if (equalBytes(prev.script, trScript)) return 'p2tr';
    if (equalBytes(prev.script, wpkhScript)) return 'p2wpkh';
    return null;
  };
  const all = Array.from({ length: tx.inputsLength }, (_, i) => i);
  const targets = options.inputs ?? all.filter((i) => kindOf(i) !== null);
  if (targets.length === 0) throw new Error('the PSBT has no input belonging to this key');
  for (const index of targets) {
    if (!Number.isInteger(index) || index < 0 || index >= tx.inputsLength) {
      throw new Error(`input index ${index} is out of range`);
    }
    const input = tx.getInput(index);
    if (input.witnessUtxo === undefined) throw new Error(`input ${index} has no witness_utxo`);
    const kind = kindOf(index);
    if (kind === null) {
      throw new Error(`input ${index} does not belong to this key (only BIP86 P2TR key-path and P2WPKH of a compressed public key are supported)`);
    }
    if (kind === 'p2tr') {
      if (input.tapInternalKey === undefined) {
        tx.updateInput(index, { tapInternalKey: internalKey });
      } else if (!equalBytes(input.tapInternalKey, internalKey)) {
        throw new Error(`the tap_internal_key of input ${index} is not the public key of this key`);
      }
    }
    const defaultType = kind === 'p2tr' ? SIGHASH_DEFAULT : SIGHASH_ALL;
    const sighash = tx.getInput(index).sighashType ?? defaultType;
    const allowed = options.sighashTypes ?? [defaultType];
    if (!allowed.includes(sighash)) {
      throw new Error(`the sighash type 0x${sighash.toString(16)} of input ${index} is not in the allowed set`);
    }
    tx.signIdx(secretKey, index, [sighash], options.auxRand);
    if (options.finalize ?? true) tx.finalizeIdx(index);
  }
  return tx.toPSBT(0);
}

/** Finalizes all inputs of the PSBT and extracts the broadcastable transaction (network serialization). */
export function extractTransaction(psbt: Uint8Array): Uint8Array {
  const tx = Transaction.fromPSBT(psbt, SCURE_TX_OPTIONS);
  for (let i = 0; i < tx.inputsLength; i++) {
    const input = tx.getInput(i);
    if (input.finalScriptWitness === undefined && input.finalScriptSig === undefined) tx.finalizeIdx(i);
  }
  return tx.extract();
}
