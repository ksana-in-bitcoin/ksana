/**
 * Which accounts are contracts. A contract's ID is a hash, not a key, yet about half of all hashes are valid x-only keys as well,
 * which the pages would write as Taproot addresses, as if someone held a key for them. The node's index of the standards (D-120)
 * knows every contract: `ksana_getContract` answers for one and `-32004` for any other account. The page asks once per account
 * and node and keeps the answer for its life; a node without the index, or still building it (`-32601`), is asked no more while
 * the page is open, and its accounts are written as before, an address for every key.
 */
import { useEffect, useSyncExternalStore } from 'react';
import { RPC_ERROR_CODES, RpcError, toHex } from '@ksana/sdk';
import { useConnection, type Connection } from './connection';

/** By node and account: true a contract, false any other account, a question under way as its answer to come */
const known = new Map<string, boolean | Promise<boolean | null>>();
/** The nodes, by endpoint, that have no index to ask */
const unindexed = new Set<string>();
const listeners = new Set<() => void>();
/** Counts the answers, so that a page can render again when one comes */
let answers = 0;

function hexOf(account: Uint8Array | string): string {
  return typeof account === 'string' ? account.toLowerCase() : toHex(account);
}

function subscribe(listener: () => void): () => void {
  listeners.add(listener);
  return () => { listeners.delete(listener); };
}

/** What the page knows of an account: true a contract, false any other account, null not known (yet) */
export function knownContract(conn: Connection, account: Uint8Array | string): boolean | null {
  const answer = known.get(`${conn.endpoint}:${hexOf(account)}`);
  return typeof answer === 'boolean' ? answer : null;
}

/** Asks the node whether an account is a contract, once; null when it cannot tell, and then it is asked again next time */
export function lookUpContract(conn: Connection, account: Uint8Array | string): Promise<boolean | null> {
  const id = hexOf(account);
  const key = `${conn.endpoint}:${id}`;
  const hit = known.get(key);
  if (hit !== undefined) return Promise.resolve(hit);
  if (unindexed.has(conn.endpoint)) return Promise.resolve(null);
  const asked = conn.node.getContract(id).then(
    () => true,
    (e: unknown) => {
      if (e instanceof RpcError && e.code === RPC_ERROR_CODES.NOT_FOUND) return false;
      if (e instanceof RpcError && e.code === RPC_ERROR_CODES.METHOD_NOT_FOUND) unindexed.add(conn.endpoint);
      return null;
    },
  ).then((answer) => {
    if (answer === null) {
      known.delete(key);
    } else {
      known.set(key, answer);
      answers += 1;
      for (const listener of listeners) listener();
    }
    return answer;
  });
  known.set(key, asked);
  return asked;
}

/** Whether an account is a contract, asking the node the first time; false until the answer comes, and from a node without the index */
export function useIsContract(account: Uint8Array | string | null): boolean {
  const conn = useConnection();
  const id = account === null ? null : hexOf(account);
  const answer = useSyncExternalStore(subscribe, () => (conn && id ? knownContract(conn, id) : null));
  useEffect(() => {
    if (conn && id && answer === null) void lookUpContract(conn, id);
  }, [conn, id, answer]);
  return answer === true;
}

/** Whether an account is a contract, as text that names accounts asks it */
export type IsContract = (account: Uint8Array | string) => boolean;

/**
 * Whether the page knows an account to be a contract, for text written at once (a review's details, an operation's title): it
 * asks nothing, as an account the page shows is asked about where it is shown, and the page renders again as answers come
 */
export function useKnownContracts(): IsContract {
  const conn = useConnection();
  useSyncExternalStore(subscribe, () => answers);
  return (account) => conn !== null && knownContract(conn, account) === true;
}

/**
 * Text written once that may name accounts the page has not shown, such as the review of what was typed in a form: `write` runs
 * first to find them, the node is asked about each, and it runs again with the answers
 */
export async function writeKnowingContracts<T>(conn: Connection, write: (isContract: IsContract) => T): Promise<T> {
  const named: (Uint8Array | string)[] = [];
  write((account) => {
    named.push(account);
    return false;
  });
  await Promise.all(named.map((account) => lookUpContract(conn, account)));
  return write((account) => knownContract(conn, account) === true);
}
