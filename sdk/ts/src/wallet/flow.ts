/**
 * Wallet flow: BIP322-signing a VmOp with a wallet (D-052). On path B the signed VmOp is submitted directly with `ksana_submitOp`;
 * on path C it is written into a native transaction (`buildNativeTx`) that the wallet then signs; a path C signature is bound to
 * the native transaction's first input (D-086), so pick `utxos[0]` first, sign with `nativeFirstInput(utxos[0])` as `firstInput`,
 * then build the transaction.
 */

import { toHex } from '../bytes.js';
import { bip322Message, verifyBip322ForAccount } from '../bip322.js';
import type { OutPoint, UnsignedVmOp, VmOp } from '../types.js';
import type { WalletAdapter } from './types.js';

/** Options of `signVmopWithWallet`. */
export interface SignWithWalletOptions {
  /**
   * Self-check the signature returned by the wallet with the SDK's BIP322 implementation (default `true`); the node's (Rust)
   * verification is authoritative.
   */
  verify?: boolean;
  /**
   * Path C: the first input of the BTC transaction carrying this operation (`nativeFirstInput(utxos[0])`, D-086); not given on
   * path B.
   */
  firstInput?: OutPoint;
}

/**
 * BIP322 simple-signs a VmOp with the wallet and returns the VmOp with a `Bip322Simple` signature.
 * The message is the lowercase hex of the sighash (`bip322Message`); `op.account` must be the wallet account. A path C operation
 * must provide `options.firstInput`.
 */
export async function signVmopWithWallet(
  op: UnsignedVmOp,
  wallet: WalletAdapter,
  options: SignWithWalletOptions = {},
): Promise<VmOp> {
  const account = await wallet.getAccount();
  if (toHex(op.account) !== account.account) {
    throw new Error(`VmOp account ${toHex(op.account)} does not match wallet account ${account.account}`);
  }
  const message = bip322Message(op, options.firstInput);
  const witness = await wallet.signBip322(message);
  if ((options.verify ?? true) && !verifyBip322ForAccount(op.account, message, witness)) {
    throw new Error('the BIP322 signature returned by the wallet failed the local self-check (only a P2TR key-path simple signature by the Taproot address of the account is accepted, D-052, D-092)');
  }
  const { version, chain_id, nonce, fee, max_gas, action } = op;
  return {
    version,
    chain_id,
    account: op.account,
    nonce,
    fee,
    max_gas,
    action,
    sig: { kind: 'Bip322Simple', bytes: witness },
  };
}
