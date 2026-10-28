/**
 * Unisat wallet adapter (the injected `window.unisat`): `requestAccounts`, `getPublicKey`,
 * `signPsbt(hex, { autoFinalized: false, toSignInputs })`, `signMessage(msg, "bip322-simple")`, `getChain`.
 *
 * The wallet must use a Taproot address (D-092: the account is the output key of the Taproot address); native segwit (P2WPKH) and
 * other addresses are refused in `getAccount`.
 */

import { fromHex, toHex } from '../bytes.js';
import { decodeBip322Signature } from '../bip322.js';
import type { BtcNetwork } from '../btc/address.js';
import { type SignInput, type WalletAccount, type WalletAdapter, type WalletNetwork, deriveWalletAccount } from './types.js';

/** An item of the `toSignInputs` of Unisat `signPsbt`. */
export interface UnisatToSignInput {
  index: number;
  address?: string;
  publicKey?: string;
  sighashTypes?: number[];
  disableTweakSigner?: boolean;
}

/** Options of Unisat `signPsbt`. */
export interface UnisatSignPsbtOptions {
  autoFinalized?: boolean;
  toSignInputs?: UnisatToSignInput[];
}

/** The chain Unisat is set to (`getChain`): `enum` such as `BITCOIN_SIGNET`, `name` such as `Bitcoin Signet`, `network` `livenet` or `testnet`. */
export interface UnisatChain {
  enum: string;
  name: string;
  network: string;
}

/** The interface Unisat injects (the part this adapter uses). */
export interface UnisatProvider {
  requestAccounts(): Promise<string[]>;
  getPublicKey(): Promise<string>;
  signPsbt(psbtHex: string, options?: UnisatSignPsbtOptions): Promise<string>;
  signMessage(message: string, type?: 'ecdsa' | 'bip322-simple'): Promise<string>;
  /** The chain the wallet is set to; versions without it cannot tell. */
  getChain?(): Promise<UnisatChain>;
}

/**
 * The SDK's network of a Unisat chain; undefined for a chain that is none of them (`BITCOIN_TESTNET4`, `FRACTAL_BITCOIN_MAINNET`,
 * `FRACTAL_BITCOIN_TESTNET`). Unisat has no regtest: `BITCOIN_REGTEST` is the mock wallet's (`MockUnisatWallet`).
 */
function unisatNetwork(chain: string): BtcNetwork | undefined {
  switch (chain) {
    case 'BITCOIN_MAINNET':
      return 'mainnet';
    case 'BITCOIN_TESTNET':
      return 'testnet';
    case 'BITCOIN_SIGNET':
      return 'signet';
    case 'BITCOIN_REGTEST':
      return 'regtest';
    default:
      return undefined;
  }
}

/** The `window.unisat` injected into the page (`undefined` when absent). */
export function injectedUnisat(): UnisatProvider | undefined {
  return (globalThis as { unisat?: UnisatProvider }).unisat;
}

/** Unisat wallet adapter. */
export class UnisatAdapter implements WalletAdapter {
  readonly name = 'unisat';
  private readonly provider: UnisatProvider;

  /** `provider` defaults to the injected `window.unisat`. */
  constructor(provider?: UnisatProvider) {
    const found = provider ?? injectedUnisat();
    if (found === undefined) throw new Error('Unisat wallet not detected (window.unisat)');
    this.provider = found;
  }

  async getAccount(): Promise<WalletAccount> {
    const accounts = await this.provider.requestAccounts();
    const address = accounts[0];
    if (typeof address !== 'string') throw new Error('Unisat returned no account');
    const publicKey = await this.provider.getPublicKey();
    return deriveWalletAccount(address, publicKey);
  }

  /** Has Unisat sign the given inputs (`autoFinalized: false`; returns a PSBT with `tap_key_sig` / `partial_sigs`). */
  async signPsbt(psbt: Uint8Array, inputs: readonly SignInput[]): Promise<Uint8Array> {
    const account = await this.getAccount();
    const toSignInputs = inputs.map((input) => ({
      index: input.index,
      publicKey: account.publicKey,
      ...(input.sighash === undefined ? {} : { sighashTypes: [input.sighash] }),
    }));
    const signed = await this.provider.signPsbt(toHex(psbt), { autoFinalized: false, toSignInputs });
    return fromHex(signed, undefined, 'PSBT returned by Unisat');
  }

  async signBip322(message: string): Promise<Uint8Array> {
    return decodeBip322Signature(await this.provider.signMessage(message, 'bip322-simple'));
  }

  /** The chain Unisat is set to (`getChain`, D-106); undefined from a version without it or a reply without the chain's `enum`. */
  async getNetwork(): Promise<WalletNetwork | undefined> {
    if (typeof this.provider.getChain !== 'function') return undefined;
    const chain: unknown = await this.provider.getChain();
    const id = typeof chain === 'object' && chain !== null ? (chain as { enum?: unknown }).enum : undefined;
    if (typeof id !== 'string') return undefined;
    const label = (chain as { name?: unknown }).name;
    const name = typeof label === 'string' && label !== '' ? label : id;
    const network = unisatNetwork(id);
    return network === undefined ? { name } : { network, name };
  }
}
