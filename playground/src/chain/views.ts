/** Read-only calls (`ksana_call`) decoded by the contract's ABI, and a hook that re-reads them as the chain moves. */
import { useCallback, useEffect, useRef, useState } from 'react';
import { fromHex, type Funds, type Receipt } from '@ksana/sdk';
import { decodeReturn, encodeArgs } from '../abi/codec';
import type { Abi, Value } from '../abi/types';
import { createStore, useStore } from '../app/store';
import { useConnection, useTip, type Connection } from './connection';
import { useWallet } from './wallet';

export class ViewError extends Error {
  constructor(message: string, readonly receipt?: Receipt) {
    super(message);
    this.name = 'ViewError';
  }
}

/** The reason of a failed receipt, without the node's `revert: ` prefix */
export function failureText(error: string | null | undefined): string {
  if (!error) return 'failed';
  return error.startsWith('revert: ') ? error.slice(8) : error;
}

export interface ViewOptions {
  caller?: string | null | undefined;
  funds?: Funds[];
}

/** Runs a method read-only and decodes what it returns */
export async function readMethod(conn: Connection, contract: string, abi: Abi, name: string, args: Value[], opts: ViewOptions = {}): Promise<Value> {
  const method = abi.methods.find((m) => m.name === name);
  if (!method) throw new ViewError(`the ABI has no method ${name}`);
  const receipt = await conn.node.call({
    contract,
    method: name,
    args: encodeArgs(abi, method.params.map((p) => p.type), args),
    ...(opts.funds?.length ? { funds: opts.funds } : {}),
    ...(opts.caller ? { caller: fromHex(opts.caller, 32) } : {}),
  });
  if (!receipt.success) throw new ViewError(failureText(receipt.error), receipt);
  return decodeReturn(abi, method.returns, receipt.output);
}

/** Bumped when an operation changed a contract, so pages re-read it at once */
export const changesStore = createStore(0);

export function contractChanged(): void {
  changesStore.update((n) => n + 1);
}

export interface ViewState<T> {
  data: T | null;
  error: string | null;
  loading: boolean;
  /** Whether a read of this very view (contract, method, arguments, caller) has answered: `data` null is then a decoded None */
  ready: boolean;
  reload: () => void;
}

/**
 * A view of a contract, re-read on every new block and after every operation of this page. `args` is compared by its JSON
 * (bigints and bytes as text), so a new array with the same values does not read again. A re-read keeps what the last one
 * answered until it answers; another contract, method, argument or caller starts again from nothing.
 */
export function useView<T = Value>(contract: string | null, abi: Abi | null, name: string, args: Value[] = [], opts: ViewOptions & { enabled?: boolean } = {}): ViewState<T> {
  const conn = useConnection();
  const tip = useTip();
  const wallet = useWallet();
  const changes = useStore(changesStore);
  const [state, setState] = useState<{ data: T | null; error: string | null; loading: boolean; ready: boolean }>(
    { data: null, error: null, loading: false, ready: false });
  const [nonce, setNonce] = useState(0);
  const key = JSON.stringify(args, (_k, v) => (typeof v === 'bigint' ? `${v}n` : v instanceof Uint8Array ? Array.from(v).join(',') : v));
  const fundsKey = JSON.stringify(opts.funds ?? [], (_k, v) => (typeof v === 'bigint' ? `${v}n` : v instanceof Uint8Array ? Array.from(v).join(',') : v));
  const caller = opts.caller === undefined ? wallet?.account.account : opts.caller;
  const argsRef = useRef(args);
  argsRef.current = args;
  const fundsRef = useRef(opts.funds);
  fundsRef.current = opts.funds;
  const enabled = opts.enabled ?? true;
  const identity = useRef<unknown[]>([]);
  useEffect(() => {
    let alive = true;
    const current = [conn, contract, abi, name, key, fundsKey, caller, enabled];
    const same = current.every((v, i) => Object.is(v, identity.current[i]));
    identity.current = current;
    if (!conn || !contract || !abi || !enabled) {
      setState({ data: null, error: null, loading: false, ready: false });
      return;
    }
    setState((s) => (same ? { ...s, loading: true } : { data: null, error: null, loading: true, ready: false }));
    readMethod(conn, contract, abi, name, argsRef.current, { caller, ...(fundsRef.current ? { funds: fundsRef.current } : {}) }).then(
      (data) => { if (alive) setState({ data: data as T, error: null, loading: false, ready: true }); },
      (e: unknown) => { if (alive) setState({ data: null, error: e instanceof Error ? e.message : String(e), loading: false, ready: true }); },
    );
    return () => { alive = false; };
  }, [conn, contract, abi, name, key, fundsKey, caller, enabled, tip?.tip.height, changes, nonce]);
  const reload = useCallback(() => setNonce((n) => n + 1), []);
  return { ...state, reload };
}
