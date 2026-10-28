/**
 * Browser wallet adapter interface (SPEC §11.3, D-052, D-053, D-092, D-106).
 *
 * The account is the output key of the wallet's Taproot address (D-092): the wallet must use a Taproot (P2TR, BIP86 key-path)
 * address, and the Ksana account and this address are two spellings of the same bytes. Other address types such as native segwit
 * (P2WPKH) cannot be accounts. An adapter can also tell which BTC network the wallet is set to (`getNetwork`, D-106).
 */

import { equalBytes, fromHex, toHex } from '../bytes.js';
import { type BtcNetwork, decodeAddress } from '../btc/address.js';
import { isP2tr, p2trScript } from '../btc/script.js';
import { toXOnly } from '../keys.js';

/** Address type of a wallet account: only Taproot since D-092. */
export type WalletAddressType = 'p2tr';

/** A wallet account. */
export interface WalletAccount {
  /** Taproot address (`bc1p…` / `tb1p…` / `bcrt1p…`). */
  address: string;
  /** Public key given by the wallet (hex: a 33-byte compressed key, or a 32-byte x-only internal key). */
  publicKey: string;
  /** Address type (always `p2tr`). */
  addressType: WalletAddressType;
  /** x-only internal key (hex): the `tapInternalKey` of PSBT inputs. */
  internalKey: string;
  /** Ksana account (hex): the address's 32-byte output key, i.e. the internal key after the BIP86 tweak, as an x-only key (D-092). */
  account: string;
}

/**
 * The BTC network a wallet says it is set to (`WalletAdapter.getNetwork`, A2-11, D-106). The address does not always pin it:
 * `tb1…` is testnet3's, testnet4's and signet's, and Fractal Bitcoin's addresses are mainnet's.
 */
export interface WalletNetwork {
  /** The SDK's network when the wallet's chain is one of them (`testnet` is testnet3); absent for one that is not, such as testnet4 or Fractal Bitcoin. */
  network?: BtcNetwork;
  /** The wallet's own name of its chain, for messages (`Bitcoin Testnet4 (Beta)`, `Testnet4`). */
  name: string;
}

/** A PSBT input for the wallet to sign. */
export interface SignInput {
  /** Input index. */
  index: number;
  /** Required sighash type; defaults to the wallet's default type (`SIGHASH_DEFAULT` for P2TR). */
  sighash?: number;
}

/** A wallet adapter. */
export interface WalletAdapter {
  /** Wallet name. */
  readonly name: string;
  /** Connects the wallet and gets the current account (checks that the address and the public key agree). */
  getAccount(): Promise<WalletAccount>;
  /** Has the wallet sign the given PSBT inputs (without broadcasting); returns the signed PSBT (possibly not finalized). */
  signPsbt(psbt: Uint8Array, inputs: readonly SignInput[]): Promise<Uint8Array>;
  /** Has the wallet BIP322 simple-sign a text message; returns the consensus encoding of the witness stack (D-052). */
  signBip322(message: string): Promise<Uint8Array>;
  /**
   * The BTC network the wallet says it is set to (Unisat `getChain`, Xverse `wallet_getNetwork`); `undefined` when it cannot tell.
   * Optional: an adapter without it cannot tell either.
   */
  getNetwork?(): Promise<WalletNetwork | undefined>;
}

/**
 * Derives the Ksana account from the address and public key given by the wallet and checks that they agree (D-092): the address
 * must be Taproot (P2TR), the public key is the x-only internal key (a 33-byte compressed key without its prefix byte, or 32-byte
 * x-only), and the address must be its BIP86-tweaked P2TR (no script tree); the account is the address's 32-byte output key.
 * Other address types throw.
 */
export function deriveWalletAccount(address: string, publicKey: string): WalletAccount {
  const { script } = decodeAddress(address);
  const key = fromHex(publicKey, undefined, 'wallet public key');
  if (!isP2tr(script)) {
    throw new Error('an account can only be a Taproot (P2TR) address (D-092): switch to a Taproot address in the wallet');
  }
  const internal = toXOnly(key);
  if (!equalBytes(p2trScript(internal), script)) {
    throw new Error('the P2TR address is not the BIP86-tweaked address of this public key (the wallet may use another derivation)');
  }
  return {
    address,
    publicKey: toHex(key),
    addressType: 'p2tr',
    internalKey: toHex(internal),
    account: toHex(script.subarray(2)),
  };
}
