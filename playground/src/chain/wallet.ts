/**
 * The signing wallet: Unisat (OneKey injects the same interface) or Xverse through the SDK's adapters, or on a local regtest
 * devnet a test key given in the address bar (`?wallet=mock&key=<64 hex digits>`, read once and removed from the address bar; any
 * other network never uses it). The page never sees a private key of a real wallet: every operation is signed in the wallet.
 * The account is the wallet's Taproot address (D-092), on the node's BTC network.
 */
import {
  MockUnisatWallet, UnisatAdapter, XverseAdapter, decodeAddress, fromHex, injectedUnisat, injectedXverse, isValidSecretKey,
  type WalletAccount, type WalletAdapter,
} from '@ksana/sdk';
import { createStore, loadPref, savePref, useStore } from '../app/store';
import { checkWalletNetwork } from './ops';
import type { Connection } from './connection';

export type WalletKind = 'unisat' | 'xverse' | 'mock';

export interface ConnectedWallet {
  kind: WalletKind;
  name: string;
  adapter: WalletAdapter;
  account: WalletAccount;
}

export const walletStore = createStore<ConnectedWallet | null>(null);

let mockKey: { secret: Uint8Array | null } | null = null;

function readMockKey() {
  if (mockKey || typeof window === 'undefined') return;
  try {
    const url = new URL(window.location.href);
    if (url.searchParams.get('wallet') !== 'mock' || !url.searchParams.has('key')) return;
    const key = url.searchParams.get('key') ?? '';
    const secret = /^[0-9a-fA-F]{64}$/.test(key) ? fromHex(key, 32) : null;
    mockKey = { secret: secret && isValidSecretKey(secret) ? secret : null };
    url.searchParams.delete('key');
    window.history.replaceState(window.history.state, '', url.toString());
  } catch {
    /* the address bar is not writable */
  }
}

readMockKey();

function testKey(conn: Connection): Uint8Array | null {
  return conn.network === 'regtest' ? (mockKey?.secret ?? null) : null;
}

/** OneKey injects `window.unisat` too, with its own `window.$onekey.btc` being the same provider */
function unisatIsOneKey(): boolean {
  const unisat = injectedUnisat() as { requestAccounts?: unknown } | undefined;
  const onekey = (globalThis as { $onekey?: { btc?: { requestAccounts?: unknown } } }).$onekey?.btc;
  return typeof unisat?.requestAccounts === 'function' && unisat.requestAccounts === onekey?.requestAccounts;
}

export function walletName(kind: WalletKind): string {
  if (kind === 'unisat') return unisatIsOneKey() ? 'OneKey' : 'Unisat';
  if (kind === 'xverse') return 'Xverse';
  return 'Test key';
}

export interface WalletOption {
  kind: WalletKind;
  name: string;
  available: boolean;
}

export function walletOptions(conn: Connection): WalletOption[] {
  const options: WalletOption[] = [
    { kind: 'unisat', name: walletName('unisat'), available: !!injectedUnisat() },
    { kind: 'xverse', name: walletName('xverse'), available: !!injectedXverse() },
  ];
  if (testKey(conn)) options.push({ kind: 'mock', name: walletName('mock'), available: true });
  return options;
}

export class WalletError extends Error {
  constructor(readonly reason: 'notTaproot' | 'wrongNetwork' | 'noKey', readonly detail = '') {
    super(detail ? `${reason}: ${detail}` : reason);
    this.name = 'WalletError';
  }
}

function makeAdapter(kind: WalletKind, conn: Connection): WalletAdapter {
  if (kind === 'unisat') return new UnisatAdapter();
  if (kind === 'xverse') return new XverseAdapter({ message: 'Connect your Taproot account to the Ksana playground' });
  const secret = testKey(conn);
  if (!secret) throw new WalletError('noKey');
  return new UnisatAdapter(new MockUnisatWallet({ secretKey: secret, network: conn.network }));
}

function onNetwork(account: WalletAccount, conn: Connection): boolean {
  try {
    return decodeAddress(account.address).networks.includes(conn.network);
  } catch {
    return false;
  }
}

export async function connectWallet(kind: WalletKind, conn: Connection): Promise<ConnectedWallet> {
  const adapter = makeAdapter(kind, conn);
  const account = await adapter.getAccount();
  if (account.addressType !== 'p2tr') throw new WalletError('notTaproot', account.address);
  if (!onNetwork(account, conn)) throw new WalletError('wrongNetwork', account.address);
  await checkWalletNetwork(adapter, conn.network);
  const wallet: ConnectedWallet = { kind, name: walletName(kind), adapter, account };
  walletStore.set(wallet);
  if (kind !== 'mock') savePref('wallet', kind);
  return wallet;
}

export function disconnectWallet(): void {
  walletStore.set(null);
  savePref('wallet', null);
}

/** Drops the wallet when the node moves to a network its address is not on */
export function checkWalletFor(conn: Connection | null): void {
  const w = walletStore.get();
  if (w && (!conn || !onNetwork(w.account, conn))) walletStore.set(null);
}

export function lastWallet(): WalletKind | null {
  return loadPref<WalletKind | null>('wallet', null, (v): v is WalletKind | null => v === 'unisat' || v === 'xverse' || v === null);
}

/** Whether a wallet's error means the user declined (closed the prompt or pressed reject) */
export function isWalletRejection(message: string): boolean {
  return /reject|cancel|denied|\u62d2\u7edd|\u53d6\u6d88/i.test(message);
}

export function useWallet(): ConnectedWallet | null {
  return useStore(walletStore);
}
