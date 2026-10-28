/** The connected account's nonce and balances, refreshed every few seconds and after each operation. */
import { toHex, type AccountInfo } from '@ksana/sdk';
import { createStore, useStore } from '../app/store';
import { connectionStore, currentConnection } from './connection';
import { walletStore } from './wallet';

export interface AccountView {
  account: string;
  nonce: bigint;
  /** Balances by asset ID (hex) */
  balances: Map<string, bigint>;
  info: AccountInfo;
}

export const accountStore = createStore<AccountView | null>(null);

let timer: ReturnType<typeof setTimeout> | undefined;
let generation = 0;

export async function refreshAccount(): Promise<void> {
  const id = generation;
  const conn = currentConnection();
  const wallet = walletStore.get();
  if (!conn || !wallet) {
    accountStore.set(null);
    return;
  }
  try {
    const info = await conn.node.getAccount(wallet.account.account);
    if (id !== generation) return;
    accountStore.set({
      account: wallet.account.account,
      nonce: info.nonce,
      balances: new Map(info.balances.map((b) => [toHex(b.asset), b.amount])),
      info,
    });
  } catch {
    /* kept as it was; the next refresh tries again */
  }
}

function schedule() {
  if (timer) clearTimeout(timer);
  const id = generation;
  timer = setTimeout(() => {
    void refreshAccount().finally(() => {
      if (id === generation) schedule();
    });
  }, 6_000);
}

function restart() {
  generation++;
  accountStore.set(null);
  if (timer) clearTimeout(timer);
  if (currentConnection() && walletStore.get()) {
    void refreshAccount();
    schedule();
  }
}

connectionStore.subscribe(restart);
walletStore.subscribe(restart);

export function useAccount(): AccountView | null {
  return useStore(accountStore);
}

export function balanceOf(view: AccountView | null, asset: string): bigint {
  return view?.balances.get(asset) ?? 0n;
}
