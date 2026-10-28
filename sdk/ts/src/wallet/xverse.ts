/**
 * Xverse wallet adapter (D-053): calls the injected `window.XverseProviders.BitcoinProvider.request(method, params)` directly,
 * with the same JSON-RPC methods sats-connect uses, without depending on sats-connect:
 *
 * - `getAccounts`: `{ purposes: ['ordinals', 'payment'] }`;
 * - `signPsbt`: `{ psbt: base64, signInputs: { [address]: [input indices] }, broadcast: false }`; Xverse signs with the PSBT
 *   input's `sighash_type` (the default type when absent);
 * - `signMessage`: `{ address, message, protocol: 'BIP322' }`;
 * - `wallet_getNetwork`: no parameters, the BTC network in `bitcoin.name` (`Mainnet`, `Testnet`, `Testnet4`, `Signet`, `Regtest`).
 *
 * The response may be `{ result }` (JSON-RPC) or `{ status: 'success' | 'error', result | error }` (sats-connect style); both are
 * handled. The ordinals (P2TR) address is used by default; it is the Ksana account's address (D-092). Xverse's payment address is
 * usually P2SH-P2WPKH and cannot be an account.
 */

import { base64 } from '@scure/base';

import { decodeBip322Signature } from '../bip322.js';
import type { BtcNetwork } from '../btc/address.js';
import { type SignInput, type WalletAccount, type WalletAdapter, type WalletNetwork, deriveWalletAccount } from './types.js';

/** The Bitcoin provider Xverse injects. */
export interface XverseBitcoinProvider {
  request(method: string, params?: unknown): Promise<unknown>;
}

/** Options of the Xverse adapter. */
export interface XverseAdapterOptions {
  /** Defaults to the injected `window.XverseProviders.BitcoinProvider`. */
  provider?: XverseBitcoinProvider;
  /** Which address to use (default `ordinals`, i.e. P2TR). */
  purpose?: 'ordinals' | 'payment';
  /** The explanation shown in the `getAccounts` popup. */
  message?: string;
}

/** The Xverse Bitcoin provider injected into the page (`undefined` when absent). */
export function injectedXverse(): XverseBitcoinProvider | undefined {
  return (globalThis as { XverseProviders?: { BitcoinProvider?: XverseBitcoinProvider } }).XverseProviders?.BitcoinProvider;
}

/** An error returned by Xverse. */
export class XverseError extends Error {
  constructor(
    message: string,
    readonly code?: number,
  ) {
    super(message);
    this.name = 'XverseError';
  }
}

function unwrap(method: string, response: unknown): Record<string, unknown> | unknown[] {
  if (typeof response !== 'object' || response === null) throw new XverseError(`${method}: malformed response`);
  const r = response as { status?: unknown; result?: unknown; error?: unknown };
  const fail = (error: unknown): never => {
    const e = (typeof error === 'object' && error !== null ? error : {}) as { message?: unknown; code?: unknown };
    throw new XverseError(
      `${method}: ${typeof e.message === 'string' ? e.message : JSON.stringify(error)}`,
      typeof e.code === 'number' ? e.code : undefined,
    );
  };
  if (r.status !== undefined && r.status !== 'success') fail(r.error);
  if (r.error !== undefined && r.error !== null) fail(r.error);
  const result = r.result;
  if (typeof result !== 'object' || result === null) throw new XverseError(`${method}: response is missing result`);
  return result as Record<string, unknown> | unknown[];
}

/** The SDK's network of an Xverse BTC network; undefined for one that is none of them (`Testnet4`). */
function xverseNetwork(name: string): BtcNetwork | undefined {
  switch (name) {
    case 'Mainnet':
      return 'mainnet';
    case 'Testnet':
      return 'testnet';
    case 'Signet':
      return 'signet';
    case 'Regtest':
      return 'regtest';
    default:
      return undefined;
  }
}

interface XverseAddress {
  address: string;
  publicKey: string;
  purpose?: string;
  addressType?: string;
}

/** Xverse wallet adapter. */
export class XverseAdapter implements WalletAdapter {
  readonly name = 'xverse';
  private readonly provider: XverseBitcoinProvider;
  private readonly purpose: 'ordinals' | 'payment';
  private readonly message: string | undefined;

  constructor(options: XverseAdapterOptions = {}) {
    const found = options.provider ?? injectedXverse();
    if (found === undefined) throw new Error('Xverse wallet not detected (window.XverseProviders.BitcoinProvider)');
    this.provider = found;
    this.purpose = options.purpose ?? 'ordinals';
    this.message = options.message;
  }

  private async call(method: string, params: unknown): Promise<Record<string, unknown> | unknown[]> {
    return unwrap(method, await this.provider.request(method, params));
  }

  async getAccount(): Promise<WalletAccount> {
    const result = await this.call('getAccounts', {
      purposes: ['ordinals', 'payment'],
      ...(this.message === undefined ? {} : { message: this.message }),
    });
    const list = Array.isArray(result) ? result : (result.addresses ?? result.accounts);
    if (!Array.isArray(list)) throw new XverseError('getAccounts: response has no address list');
    const addresses = list.filter(
      (a): a is XverseAddress =>
        typeof a === 'object' &&
        a !== null &&
        typeof (a as XverseAddress).address === 'string' &&
        typeof (a as XverseAddress).publicKey === 'string',
    );
    const chosen = addresses.find((a) => a.purpose === this.purpose);
    if (chosen === undefined) throw new XverseError(`getAccounts: no ${this.purpose} address`);
    return deriveWalletAccount(chosen.address, chosen.publicKey);
  }

  /**
   * Has Xverse sign the given inputs (without broadcasting). The sighash type is decided by the PSBT input's `sighash_type`;
   * `inputs[].sighash` is not passed to the wallet.
   */
  async signPsbt(psbt: Uint8Array, inputs: readonly SignInput[]): Promise<Uint8Array> {
    const account = await this.getAccount();
    const result = await this.call('signPsbt', {
      psbt: base64.encode(psbt),
      signInputs: { [account.address]: inputs.map((i) => i.index) },
      broadcast: false,
    });
    const signed = Array.isArray(result) ? undefined : result.psbt;
    if (typeof signed !== 'string') throw new XverseError('signPsbt: response has no psbt');
    return base64.decode(signed);
  }

  async signBip322(message: string): Promise<Uint8Array> {
    const account = await this.getAccount();
    const result = await this.call('signMessage', { address: account.address, message, protocol: 'BIP322' });
    const signature = Array.isArray(result) ? undefined : result.signature;
    if (typeof signature !== 'string') throw new XverseError('signMessage: response has no signature');
    return decodeBip322Signature(signature);
  }

  /**
   * The BTC network Xverse is set to (`wallet_getNetwork`, D-106); undefined when it cannot tell: a version without the method, a
   * site it does not let read the network, or a reply without `bitcoin.name`.
   */
  async getNetwork(): Promise<WalletNetwork | undefined> {
    let result: Record<string, unknown> | unknown[];
    try {
      result = await this.call('wallet_getNetwork', null);
    } catch {
      return undefined;
    }
    const bitcoin = Array.isArray(result) ? undefined : result.bitcoin;
    const name = typeof bitcoin === 'object' && bitcoin !== null ? (bitcoin as { name?: unknown }).name : undefined;
    if (typeof name !== 'string') return undefined;
    const network = xverseNetwork(name);
    return network === undefined ? { name } : { network, name };
  }
}
