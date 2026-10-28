/** The vesting page's reads besides `useView`: the chain's time, `releasable` of listed schedules, asset names, an event's asset. */
import { useEffect, useState } from 'react';
import { toHex } from '@ksana/sdk';
import type { Abi } from '../../abi/types';
import { assetMeta, type AssetMeta } from '../../chain/assets';
import { useConnection, type Connection } from '../../chain/connection';
import { readMethod } from '../../chain/views';
import { asSchedule, type Schedule } from './schedule';

export { secondsNow, useChainTime, type ChainTime } from '../../chain/time';

let tail: Promise<unknown> = Promise.resolve();
/** How long a read that does not answer holds back the ones after it */
const PATIENCE_MS = 15_000;

/** Runs reads one after another, so a page of schedules does not take every call a node runs at once (at most 4) */
function queued<T>(task: () => Promise<T>): Promise<T> {
  const run = tail.then(task, task);
  tail = Promise.race([run, new Promise((resolve) => setTimeout(resolve, PATIENCE_MS))]).then(() => undefined, () => undefined);
  return run;
}

/** A schedule's state: a release or a revoke changes it, and with it what is releasable */
export function stateKey(s: Schedule): string {
  return `${s.id}:${s.total}:${s.released}:${s.revoked_at ?? '-'}`;
}

/**
 * `releasable(id)` of each schedule, by its `stateKey`, read again whenever the list is (on every block and after every send).
 * One that could not be read is left out, and the page counts it itself from the chain's time.
 */
export function useReleasable(contract: string, abi: Abi, schedules: readonly Schedule[] | null): Map<string, bigint> {
  const conn = useConnection();
  const [values, setValues] = useState<Map<string, bigint>>(() => new Map());
  useEffect(() => {
    if (!conn || !schedules || schedules.length === 0) return;
    let alive = true;
    void (async () => {
      const next = new Map<string, bigint>();
      for (const s of schedules) {
        if (!alive) return;
        // All of it has been released: nothing is left to read
        if (s.released >= s.total) {
          next.set(stateKey(s), 0n);
          continue;
        }
        try {
          const value = await queued(() => readMethod(conn, contract, abi, 'releasable', [s.id]));
          if (typeof value === 'bigint') next.set(stateKey(s), value);
        } catch {
          /* counted by the page instead */
        }
      }
      if (alive) setValues(next);
    })();
    return () => { alive = false; };
  }, [conn, contract, abi, schedules]);
  return values;
}

/** The names and decimals of some assets, by asset ID (hex) */
export function useAssetMetas(hexes: readonly string[]): Map<string, AssetMeta> {
  const conn = useConnection();
  const key = hexes.join(',');
  const [metas, setMetas] = useState<Map<string, AssetMeta>>(() => new Map());
  useEffect(() => {
    if (!conn || !key) return;
    let alive = true;
    void Promise.all(key.split(',').map((hex) => assetMeta(conn, hex).then((meta) => [hex, meta] as const, () => null))).then((list) => {
      if (alive) setMetas(new Map(list.filter((x): x is readonly [string, AssetMeta] => x !== null)));
    });
    return () => { alive = false; };
  }, [conn, key]);
  return metas;
}

/** Each schedule's asset, which never changes: what the lists showed, or one read of `schedule(id)` */
const assets = new Map<string, Promise<string | null>>();

function assetKey(conn: Connection, contract: string, id: bigint): string {
  return `${conn.endpoint}:${contract}:${id}`;
}

/** Notes the assets of schedules the page has read, so their events need no read of their own */
export function rememberAssets(conn: Connection | null, contract: string, schedules: readonly Schedule[]): void {
  if (!conn) return;
  for (const s of schedules) assets.set(assetKey(conn, contract, s.id), Promise.resolve(toHex(s.asset)));
}

/** The asset (hex) of schedule `id`, or null while it is not known */
export function useScheduleAsset(contract: string, abi: Abi, id: bigint | null): string | null {
  const conn = useConnection();
  const [asset, setAsset] = useState<string | null>(null);
  useEffect(() => {
    if (!conn || id === null) return;
    let alive = true;
    const key = assetKey(conn, contract, id);
    let hit = assets.get(key);
    if (!hit) {
      hit = queued(() => readMethod(conn, contract, abi, 'schedule', [id])).then((value) => {
        const s = asSchedule(value);
        return s ? toHex(s.asset) : null;
      });
      assets.set(key, hit);
      // Asked again later rather than kept as unknown
      hit.then((a) => { if (a === null) assets.delete(key); }, () => assets.delete(key));
    }
    hit.then((a) => { if (alive) setAsset(a); }, () => { /* the event shows without its amount */ });
    return () => { alive = false; };
  }, [conn, contract, abi, id]);
  return asset;
}
