/** The chain's time (the latest block's timestamp, which contracts read as `block_timestamp`) and dates as the pages write them. */
import { useEffect, useState } from 'react';
import { toHex } from '@ksana/sdk';
import { useConnection, useTip } from './connection';

/** This browser's clock in Unix seconds */
export function secondsNow(): bigint {
  return BigInt(Math.floor(Date.now() / 1000));
}

export interface ChainTime {
  /** Unix seconds: the latest block's timestamp, or this browser's clock until that is known */
  now: bigint;
  /** Whether `now` is the latest block's timestamp */
  chain: boolean;
  /** The latest block's height, when known */
  height: bigint | null;
}

/**
 * The time contracts run at: the timestamp of the latest block (`ksana_getChainInfo` names the tip but carries no time, so
 * the tip's header is read once per tip). A read-only call runs with the tip's timestamp too.
 */
export function useChainTime(): ChainTime {
  const conn = useConnection();
  const tip = useTip();
  const hash = tip ? toHex(tip.tip.hash) : null;
  const [block, setBlock] = useState<{ time: bigint; height: bigint } | null>(null);
  useEffect(() => {
    if (!conn || !hash) return;
    let alive = true;
    conn.node.getBlock(hash).then(
      (b) => { if (alive) setBlock({ time: b.header.timestamp, height: b.header.height }); },
      () => { /* kept as it was; the next block tries again */ },
    );
    return () => { alive = false; };
  }, [conn, hash]);
  if (block) return { now: block.time, chain: true, height: block.height };
  return { now: secondsNow(), chain: false, height: tip?.tip.height ?? null };
}

/** The longest time a Date can hold, in seconds either side of 1970 */
const DATE_LIMIT = 8_640_000_000_000n;

/** Unix seconds as a Date; null outside what a Date can hold */
export function dateOf(seconds: bigint): Date | null {
  if (seconds > DATE_LIMIT || seconds < -DATE_LIMIT) return null;
  return new Date(Number(seconds) * 1000);
}

/** A date and time in the given language (the time zone is this browser's unless one is given); the seconds as such outside a Date's range */
export function formatDateTime(seconds: bigint, locale: string, timeZone?: string): string {
  const d = dateOf(seconds);
  if (!d) return `${seconds} s`;
  return formatDate(d, locale, { dateStyle: 'medium', timeStyle: 'short' }, timeZone) ?? d.toISOString();
}

/** A date with Intl's options in the given language, else in plain English; null when Intl refuses both (a time zone it does not know) */
export function formatDate(d: Date, locale: string, options: Intl.DateTimeFormatOptions, timeZone?: string): string | null {
  for (const tag of [locale, 'en']) {
    try {
      return new Intl.DateTimeFormat(tag, { ...options, ...(timeZone ? { timeZone } : {}) }).format(d);
    } catch {
      // A locale Intl refuses is tried again in plain English; a time zone it refuses leaves the caller's fallback
    }
  }
  return null;
}

/**
 * The locale dates are written in: the page's language, and on an English page the browser's own English (en-GB puts
 * the day first) when Intl takes its tag; some browsers report tags it refuses, such as en-US@posix
 */
export function dateLocale(locale: string, own: string = typeof navigator === 'undefined' ? '' : navigator.language): string {
  if (locale !== 'en' || !/^en(-|$)/i.test(own)) return locale;
  try {
    return Intl.DateTimeFormat.supportedLocalesOf(own)[0] ?? locale;
  } catch {
    return locale;
  }
}
