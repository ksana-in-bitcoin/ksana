/**
 * BIP340 signing and verification of VmOps (SPEC §4.2). For BIP322 simple signatures see `bip322.ts`.
 *
 * The account is the output key of a Taproot address (D-092): verification uses `op.account` as the public key, signing uses
 * the BIP86-tweaked secret key (`taprootTweakSecretKey`).
 *
 * The signature of a path C operation (`fee` is `BtcNative`) is bound to the first input of the BTC transaction carrying it
 * (D-086): both signing and verification must be given `firstInput` (`nativeFirstInput(utxos[0])`); a path B operation must
 * not give it.
 */

import { schnorr } from '@noble/curves/secp256k1.js';

import { assertBytes, equalBytes, randomBytes } from './bytes.js';
import { vmopSighash } from './hash.js';
import { accountOf, taprootTweakSecretKey } from './keys.js';
import type { OutPoint, SigScheme, UnsignedVmOp, VmOp } from './types.js';

/**
 * Sign `op` with BIP340 and return the value to store in `op.sig`. `secretKey` is the untweaked secret key: `op.account` must
 * equal its account (`accountOf`), and the signature is made with the BIP86-tweaked secret key; the previous value of
 * `op.sig` does not affect the result.
 *
 * The signature is determined by `(secretKey, sighash, auxRand)`; without `auxRand`, 32 fresh random bytes are used (as
 * BIP340 recommends). A path C operation must give the first input of the carrying transaction, `firstInput` (D-086).
 */
export function signVmop(op: UnsignedVmOp, secretKey: Uint8Array, auxRand?: Uint8Array, firstInput?: OutPoint): SigScheme {
  if (!equalBytes(op.account, accountOf(secretKey))) {
    throw new Error('the secret key does not match the VmOp account');
  }
  const aux = auxRand === undefined ? randomBytes(32) : assertBytes(auxRand, 32, 'auxRand');
  return { kind: 'Bip340', bytes: schnorr.sign(vmopSighash(op, firstInput), taprootTweakSecretKey(secretKey), aux) };
}

/** Returns a signed (`Bip340`) copy of the VmOp (a path C operation gives `firstInput`, D-086). */
export function signedVmop(op: UnsignedVmOp, secretKey: Uint8Array, auxRand?: Uint8Array, firstInput?: OutPoint): VmOp {
  const { account, action, chain_id, fee, max_gas, nonce, version } = op;
  return { version, chain_id, account, nonce, fee, max_gas, action, sig: signVmop(op, secretKey, auxRand, firstInput) };
}

/** Verification result: `unsupported` means the SDK does not verify that signature scheme (`Bip322Simple` is verified only on the Rust side, D-052). */
export type VerifyResult = 'valid' | 'invalid' | 'unsupported';

/**
 * Verify that `op.sig` proves the caller controls `op.account`. A path C operation is verified against the first input of the
 * carrying transaction, `firstInput` (D-086): `invalid` when it is not given or a different outpoint is given.
 *
 * - `Bip340`: BIP340 verification.
 * - `Bip322Simple`: returns `unsupported`; consensus verification is on the Rust side (`ksana-crypto::bip322`), and the SDK's
 *   `verifyVmopBip322` is only a local self-check.
 */
export function verifyVmop(op: VmOp, firstInput?: OutPoint): VerifyResult {
  if (op.sig.kind === 'Bip322Simple') return 'unsupported';
  try {
    return schnorr.verify(op.sig.bytes, vmopSighash(op, firstInput), op.account) ? 'valid' : 'invalid';
  } catch {
    return 'invalid';
  }
}
