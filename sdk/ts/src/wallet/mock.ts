/**
 * Mock wallet: implements the interface Unisat injects (`requestAccounts`, `getPublicKey`, `signPsbt`,
 * `signMessage(msg, "bip322-simple")`) on top of a local private key, signing exactly like a real wallet, for Node tests and the
 * regtest integration tests (D-055):
 *
 * - The address defaults to Taproot (BIP86 key-path), i.e. the Ksana account's address (D-092); `addressType: 'p2wpkh'` simulates
 *   a wallet switched to a native segwit address: it can still sign its own inputs in a PSBT, but it cannot be a Ksana account
 *   (the adapter refuses it in `getAccount`, `signMessage` throws).
 * - `signPsbt`: signs only the inputs belonging to this account; the sighash type is the PSBT input's `sighash_type` (default
 *   `SIGHASH_DEFAULT` for P2TR, `SIGHASH_ALL` for P2WPKH) and must be within `toSignInputs[].sighashTypes` (like bitcoinjs-lib);
 *   `autoFinalized` (default `true`) decides between returning a finalized PSBT and one with `tap_key_sig` / `partial_sigs`.
 * - `signMessage(msg, "bip322-simple")`: returns the base64 BIP322 simple signature (BIP86-tweaked P2TR key-path,
 *   `SIGHASH_DEFAULT`).
 * - `getChain`: the chain of the address's network as Unisat names it (`BITCOIN_SIGNET`; Unisat has no regtest, so regtest's is the
 *   mock wallet's own `BITCOIN_REGTEST`), or the `chain` option, which simulates a wallet set to another chain than its address
 *   shows (D-106).
 *
 * Never use it with real funds.
 */

import { fromHex, randomBytes, toHex } from '../bytes.js';
import { encodeBip322Signature, signBip322Simple } from '../bip322.js';
import { type BtcNetwork, scriptToAddress } from '../btc/address.js';
import { p2trScript, p2wpkhScript } from '../btc/script.js';
import { signPsbtWithKey } from '../btc/signer.js';
import { compressedPublicKey, xOnlyPublicKey } from '../keys.js';
import type { UnisatChain, UnisatProvider, UnisatSignPsbtOptions } from './unisat.js';

/**
 * Address type of the mock wallet: Taproot (the account's address, the default) or native segwit (can only sign BTC inputs; not a
 * Ksana account).
 */
export type MockWalletAddressType = 'p2tr' | 'p2wpkh';

/** Options of the mock wallet. */
export interface MockWalletOptions {
  /** 32-byte private key. */
  secretKey: Uint8Array;
  /** Address type (default `p2tr`). */
  addressType?: MockWalletAddressType;
  /** Network of the address (default `regtest`). */
  network?: BtcNetwork;
  /**
   * The chain `getChain` reports, as Unisat names it (default: the chain of `network`): another one, such as `BITCOIN_TESTNET4` with
   * a `tb1…` address, simulates a wallet set to another chain than its address shows.
   */
  chain?: string;
  /** BIP340 auxiliary randomness: fixed 32 bytes or a generator function; fresh randomness every time by default. */
  auxRand?: Uint8Array | (() => Uint8Array);
}

/** Unisat's chain of each network; Unisat has no regtest, so regtest's `BITCOIN_REGTEST` is this mock wallet's own. */
const CHAIN_OF: Record<BtcNetwork, string> = {
  mainnet: 'BITCOIN_MAINNET',
  testnet: 'BITCOIN_TESTNET',
  signet: 'BITCOIN_SIGNET',
  regtest: 'BITCOIN_REGTEST',
};

/** The names Unisat gives its chains (`getChain().name`, the labels in `@unisat/wallet-shared` 0.8.0), with the mock wallet's regtest. */
const CHAIN_NAMES: Record<string, string> = {
  BITCOIN_MAINNET: 'Bitcoin',
  BITCOIN_TESTNET: 'Bitcoin Testnet',
  BITCOIN_TESTNET4: 'Bitcoin Testnet4 (Beta)',
  BITCOIN_SIGNET: 'Bitcoin Signet',
  BITCOIN_REGTEST: 'Bitcoin Regtest',
  FRACTAL_BITCOIN_MAINNET: 'Fractal Bitcoin',
  FRACTAL_BITCOIN_TESTNET: 'Fractal Bitcoin Testnet',
};

/** Mock wallet implementing the interface Unisat injects. */
export class MockUnisatWallet implements UnisatProvider {
  /** Account address. */
  readonly address: string;
  /** 33-byte compressed public key (hex, like Unisat's `getPublicKey`). */
  readonly publicKey: string;
  /** Locking script of the address. */
  readonly script: Uint8Array;
  /** Address type. */
  readonly addressType: MockWalletAddressType;
  /** The chain `getChain` reports (`enum`). */
  readonly chain: string;
  private readonly secretKey: Uint8Array;
  private readonly auxRand: Uint8Array | (() => Uint8Array) | undefined;

  constructor(options: MockWalletOptions) {
    this.secretKey = Uint8Array.from(options.secretKey);
    this.addressType = options.addressType ?? 'p2tr';
    this.auxRand = options.auxRand;
    const compressed = compressedPublicKey(this.secretKey);
    this.publicKey = toHex(compressed);
    this.script = this.addressType === 'p2tr' ? p2trScript(xOnlyPublicKey(this.secretKey)) : p2wpkhScript(compressed);
    this.address = scriptToAddress(this.script, options.network ?? 'regtest');
    this.chain = options.chain ?? CHAIN_OF[options.network ?? 'regtest'];
  }

  private nextAux(): Uint8Array {
    if (this.auxRand === undefined) return randomBytes(32);
    return typeof this.auxRand === 'function' ? this.auxRand() : this.auxRand;
  }

  async requestAccounts(): Promise<string[]> {
    return [this.address];
  }

  async getAccounts(): Promise<string[]> {
    return [this.address];
  }

  async getPublicKey(): Promise<string> {
    return this.publicKey;
  }

  async getChain(): Promise<UnisatChain> {
    const name = Object.hasOwn(CHAIN_NAMES, this.chain) ? CHAIN_NAMES[this.chain] : undefined;
    return { enum: this.chain, name: name ?? this.chain, network: this.chain.endsWith('MAINNET') ? 'livenet' : 'testnet' };
  }

  async signPsbt(psbtHex: string, options: UnisatSignPsbtOptions = {}): Promise<string> {
    let psbt = fromHex(psbtHex, undefined, 'PSBT');
    const finalize = options.autoFinalized ?? true;
    if (options.toSignInputs === undefined) {
      return toHex(signPsbtWithKey(psbt, this.secretKey, { auxRand: this.nextAux(), finalize }));
    }
    for (const input of options.toSignInputs) {
      if (input.publicKey !== undefined && input.publicKey.toLowerCase() !== this.publicKey) {
        throw new Error(`toSignInputs[${input.index}]: public key is not this account`);
      }
      if (input.address !== undefined && input.address !== this.address) {
        throw new Error(`toSignInputs[${input.index}]: address is not this account`);
      }
      if (input.disableTweakSigner === true) throw new Error('the mock wallet does not support disableTweakSigner (script path)');
      psbt = signPsbtWithKey(psbt, this.secretKey, {
        inputs: [input.index],
        ...(input.sighashTypes === undefined ? {} : { sighashTypes: input.sighashTypes }),
        auxRand: this.nextAux(),
        finalize,
      });
    }
    return toHex(psbt);
  }

  async signMessage(message: string, type: 'ecdsa' | 'bip322-simple' = 'ecdsa'): Promise<string> {
    if (type !== 'bip322-simple') throw new Error('the mock wallet only supports bip322-simple message signing');
    if (this.addressType !== 'p2tr') throw new Error('the mock wallet only BIP322-signs for a Taproot address (since D-092 an account has only a Taproot address)');
    return encodeBip322Signature(signBip322Simple(this.secretKey, message, this.nextAux()));
  }
}
