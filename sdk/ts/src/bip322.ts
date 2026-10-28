/**
 * BIP322 simple signatures (SPEC §4.2, D-052): browser wallets use them to sign VmOps.
 *
 * - **Message**: the lowercase hex string of the VmOp sighash (64 ASCII bytes, `bip322Message`; the sighash of a path C
 *   operation is bound to the first input of the carrying transaction, D-086);
 *   `message_hash = tagged_hash("BIP0322-signed-message", message)`.
 * - **Signature bytes**: the Bitcoin consensus encoding of the witness stack of the `to_sign` input (the base64 returned by
 *   the wallet, decoded, without the `smp` prefix), at most 1,024 bytes.
 * - **Address**: the account's Taproot address `OP_1 <account>` (the account is the BIP86 output key, D-092); the witness has
 *   exactly 1 element, a P2TR key-path signature (64 bytes, or 65 bytes with `SIGHASH_ALL` as the last byte). The P2WPKH
 *   accounts that D-052 originally accepted were dropped with D-092.
 *
 * Consensus verification is on the Rust side (`crates/ksana-crypto/src/bip322.rs`); `verifyBip322Simple` /
 * `verifyBip322ForAccount` here implement the same rules and are only a local self-check (for example of the signature a
 * wallet returned); the Rust side is authoritative. `verifyBip322Simple` can also verify signatures of P2WPKH addresses
 * (the official BIP322 vectors), but an account cannot be a P2WPKH address.
 */

import { schnorr } from '@noble/curves/secp256k1.js';
import { base64 } from '@scure/base';

import { assertBytes, concatBytes, equalBytes, randomBytes, toHex, utf8 } from './bytes.js';
import { hash160, taggedHash, vmopSighash } from './hash.js';
import { accountOf, isValidCompressedPublicKey, isValidXOnlyPublicKey, taprootTweakSecretKey } from './keys.js';
import { ecdsaVerifyDer } from './btc/ecdsa.js';
import { OP_RETURN, isP2tr, isP2wpkh, p2pkhScriptFromHash, p2trScriptFromOutputKey, p2wpkhScript } from './btc/script.js';
import {
  SIGHASH_ALL,
  SIGHASH_DEFAULT,
  type Tx,
  decodeWitness,
  encodeWitness,
  sighashBip143,
  sighashBip341KeyPath,
  txid,
} from './btc/tx.js';
import type { OutPoint, UnsignedVmOp, VmOp } from './types.js';

/** The tagged-hash tag of the BIP322 message hash. */
export const BIP322_TAG = 'BIP0322-signed-message';

/** Length limit of the `Bip322Simple` signature bytes (D-052). */
export const MAX_BIP322_SIG_BYTES = 1024;

/**
 * The BIP322 message of a VmOp: the lowercase hex string of the sighash (D-052), handed to the wallet's `signMessage`. A path C
 * operation must give the first input of the carrying transaction, `firstInput` (D-086).
 */
export function bip322Message(op: UnsignedVmOp, firstInput?: OutPoint): string {
  return toHex(vmopSighash(op, firstInput));
}

function messageBytes(message: string | Uint8Array): Uint8Array {
  return typeof message === 'string' ? utf8(message) : message;
}

/** BIP322's `message_hash = tagged_hash("BIP0322-signed-message", UTF-8 bytes of the message)`. */
export function bip322MessageHash(message: string | Uint8Array): Uint8Array {
  return taggedHash(BIP322_TAG, messageBytes(message));
}

/**
 * The BIP322 `to_spend` transaction: version 0, `nLockTime` 0; the only input spends `000…000:0xFFFFFFFF` with scriptSig
 * `OP_0 PUSH32[message_hash]` and `nSequence` 0; the only output has value 0 and `messageChallenge` (the locking script of
 * the signing address) as its locking script.
 */
export function bip322ToSpend(messageChallenge: Uint8Array, message: string | Uint8Array): Tx {
  return {
    version: 0,
    locktime: 0,
    inputs: [
      {
        txid: new Uint8Array(32),
        vout: 0xffff_ffff,
        scriptSig: concatBytes(Uint8Array.of(0x00, 0x20), bip322MessageHash(message)),
        sequence: 0,
        witness: [],
      },
    ],
    outputs: [{ value: 0n, script: Uint8Array.from(messageChallenge) }],
  };
}

/**
 * The BIP322 `to_sign` transaction (simple format): version 0, `nLockTime` 0; the only input spends `to_spend:0`
 * (`toSpendTxid` in internal byte order) with `nSequence` 0 and the signature as its witness; the only output has value 0 and
 * `OP_RETURN` as its locking script.
 */
export function bip322ToSign(toSpendTxid: Uint8Array, witness: Uint8Array[] = []): Tx {
  return {
    version: 0,
    locktime: 0,
    inputs: [{ txid: Uint8Array.from(toSpendTxid), vout: 0, scriptSig: new Uint8Array(0), sequence: 0, witness }],
    outputs: [{ value: 0n, script: Uint8Array.of(OP_RETURN) }],
  };
}

/** The sighash of `to_sign`: BIP341 for P2TR (`SIGHASH_DEFAULT` or `SIGHASH_ALL`), BIP143 for P2WPKH (`SIGHASH_ALL`). */
function toSignSighash(challenge: Uint8Array, message: string | Uint8Array, hashType: number, publicKey?: Uint8Array): Uint8Array {
  const toSign = bip322ToSign(txid(bip322ToSpend(challenge, message)));
  if (isP2tr(challenge)) {
    return sighashBip341KeyPath(toSign, 0, [{ value: 0n, script: challenge }], hashType);
  }
  if (publicKey === undefined) throw new Error('the P2WPKH sighash needs the public key');
  return sighashBip143(toSign, 0, p2pkhScriptFromHash(hash160(publicKey)), 0n, hashType);
}

/**
 * BIP322 simple signature of `message` by the account's Taproot address with a local (untweaked) secret key: P2TR key path,
 * BIP86-tweaked secret key, `SIGHASH_DEFAULT`; returns the consensus encoding of the witness (1 element, a 64-byte
 * signature). The signature is determined by `(secretKey, message, auxRand)`; without `auxRand`, fresh random bytes are used.
 */
export function signBip322Simple(secretKey: Uint8Array, message: string | Uint8Array, auxRand?: Uint8Array): Uint8Array {
  const challenge = p2trScriptFromOutputKey(accountOf(secretKey));
  const sighash = toSignSighash(challenge, message, SIGHASH_DEFAULT);
  const aux = auxRand === undefined ? randomBytes(32) : assertBytes(auxRand, 32, 'auxRand');
  return encodeWitness([schnorr.sign(sighash, taprootTweakSecretKey(secretKey), aux)]);
}

/** The old name of `signBip322Simple` (since D-092 there are only Taproot accounts). */
export const signBip322SimpleP2tr = signBip322Simple;

/**
 * BIP322 simple-sign a VmOp with a local secret key; returns a copy carrying the `Bip322Simple` signature (for wallet signing
 * see `signVmopWithWallet`). `op.account` must equal the account of the secret key (`accountOf`). A path C operation gives the
 * first input of the carrying transaction, `firstInput` (D-086).
 */
export function signVmopBip322(op: UnsignedVmOp, secretKey: Uint8Array, auxRand?: Uint8Array, firstInput?: OutPoint): VmOp {
  if (!equalBytes(op.account, accountOf(secretKey))) throw new Error('the secret key does not match the VmOp account');
  const { account, action, chain_id, fee, max_gas, nonce, version } = op;
  const bytes = signBip322Simple(secretKey, bip322Message(op, firstInput), auxRand);
  return { version, chain_id, account, nonce, fee, max_gas, action, sig: { kind: 'Bip322Simple', bytes } };
}

/**
 * BIP322 signature text returned by a wallet → simple signature bytes: strip the optional `smp` prefix, then base64-decode.
 * The `ful` / `pof` (full, proof of funds) formats are not accepted (D-052 uses simple only).
 */
export function decodeBip322Signature(signature: string): Uint8Array {
  if (typeof signature !== 'string') throw new TypeError('the BIP322 signature must be a base64 string');
  const body = signature.startsWith('smp') ? signature.slice(3) : signature;
  if (body.startsWith('ful') || body.startsWith('pof')) {
    throw new Error('only BIP322 simple signatures are accepted (the wallet returned the full or proof-of-funds format)');
  }
  let bytes: Uint8Array;
  try {
    bytes = base64.decode(body);
  } catch {
    throw new Error('the BIP322 signature is not valid base64');
  }
  if (bytes.length === 0) throw new Error('the BIP322 signature is empty');
  return bytes;
}

/** Simple signature bytes → base64 (the wallet's return format, without a prefix). */
export function encodeBip322Signature(signature: Uint8Array): string {
  return base64.encode(signature);
}

function decodeSimple(signature: Uint8Array): Uint8Array[] | null {
  if (!(signature instanceof Uint8Array) || signature.length > MAX_BIP322_SIG_BYTES) return null;
  try {
    return decodeWitness(signature);
  } catch {
    return null;
  }
}

/**
 * Verify a BIP322 simple signature of `message` (local self-check, same rules as `verify_simple` on the Rust side):
 * `messageChallenge` is the locking script of the signing address; P2TR key path and P2WPKH are supported (the latter only
 * for the official BIP322 vectors; an account cannot be a P2WPKH address), everything else returns `false`.
 */
export function verifyBip322Simple(messageChallenge: Uint8Array, message: string | Uint8Array, signature: Uint8Array): boolean {
  const witness = decodeSimple(signature);
  if (witness === null) return false;
  try {
    if (isP2tr(messageChallenge)) {
      const [element, ...rest] = witness;
      if (element === undefined || rest.length > 0) return false;
      let sig: Uint8Array;
      let hashType: number;
      if (element.length === 64) {
        sig = element;
        hashType = SIGHASH_DEFAULT;
      } else if (element.length === 65 && element[64] === SIGHASH_ALL) {
        sig = element.slice(0, 64);
        hashType = SIGHASH_ALL;
      } else {
        return false;
      }
      const outputKey = messageChallenge.slice(2, 34);
      if (!isValidXOnlyPublicKey(outputKey)) return false;
      return schnorr.verify(sig, toSignSighash(messageChallenge, message, hashType), outputKey);
    }
    if (isP2wpkh(messageChallenge)) {
      const [sig, publicKey, ...rest] = witness;
      if (sig === undefined || publicKey === undefined || rest.length > 0) return false;
      if (publicKey.length !== 33 || !isValidCompressedPublicKey(publicKey)) return false;
      if (!equalBytes(p2wpkhScript(publicKey), messageChallenge)) return false;
      if (sig.length < 2 || sig[sig.length - 1] !== SIGHASH_ALL) return false;
      const sighash = toSignSighash(messageChallenge, message, SIGHASH_ALL, publicKey);
      return ecdsaVerifyDer(sig.slice(0, -1), sighash, publicKey);
    }
    return false;
  } catch {
    return false;
  }
}

/**
 * The `message_challenge` of an account signature (D-092): the account's Taproot address `OP_1 <account>`; the witness must
 * have exactly 1 element (P2TR key path). Returns `null` when the witness has the wrong shape or the account is not a valid
 * x-only public key (for example a contract ID).
 */
export function bip322ChallengeForAccount(account: Uint8Array, signature: Uint8Array): Uint8Array | null {
  const witness = decodeSimple(signature);
  if (witness === null || witness.length !== 1 || !isValidXOnlyPublicKey(account)) return null;
  return p2trScriptFromOutputKey(account);
}

/** Verify the BIP322 simple signature of `message` by account `account` (local self-check, same rules as `verify_for_account` on the Rust side). */
export function verifyBip322ForAccount(account: Uint8Array, message: string | Uint8Array, signature: Uint8Array): boolean {
  const challenge = bip322ChallengeForAccount(account, signature);
  return challenge !== null && verifyBip322Simple(challenge, message, signature);
}

/**
 * Local self-check of a VmOp carrying a `Bip322Simple` signature (not consensus verification; the Rust side is authoritative).
 * A path C operation is checked against the first input of the carrying transaction, `firstInput` (D-086): `false` when it is
 * not given or a different outpoint is given.
 */
export function verifyVmopBip322(op: VmOp, firstInput?: OutPoint): boolean {
  if (op.sig.kind !== 'Bip322Simple') return false;
  try {
    return verifyBip322ForAccount(op.account, bip322Message(op, firstInput), op.sig.bytes);
  } catch {
    return false;
  }
}
