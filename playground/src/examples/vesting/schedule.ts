/** The vesting contract's rules as pure functions of a schedule and a time (Unix seconds), and the time inputs of its page. */
import { isEnumValue, type Value } from '../../abi/types';
import { dateOf, formatDate } from '../../chain/time';

export { dateLocale, dateOf, formatDateTime } from '../../chain/time';

/** The longest `duration` the contract takes: 100 years of 365 days, in seconds (`MAX_DURATION` in its source) */
export const MAX_DURATION = 3_153_600_000n;
/** The most schedules `by_beneficiary` and `by_creator` return per call (`MAX_PAGE`) */
export const MAX_PAGE = 50;
export const U64_MAX = (1n << 64n) - 1n;

export const MINUTE = 60n;
export const HOUR = 3_600n;
export const DAY = 86_400n;
/** 365 days, the contract's year */
export const YEAR = 365n * DAY;

/** What the vesting rule reads of a schedule */
export interface Terms {
  total: bigint;
  released: bigint;
  start: bigint;
  cliff: bigint;
  duration: bigint;
  revoked_at: bigint | null;
}

/** A schedule as the views return it (the ABI block's `struct Schedule`) */
export interface Schedule extends Terms {
  id: bigint;
  creator: Uint8Array;
  beneficiary: Uint8Array;
  asset: Uint8Array;
  revocable: boolean;
}

function isRecord(v: Value | undefined): v is { [field: string]: Value } {
  return typeof v === 'object' && v !== null && !Array.isArray(v) && !(v instanceof Uint8Array) && !isEnumValue(v);
}

function isId(v: Value | undefined): v is Uint8Array {
  return v instanceof Uint8Array && v.length === 32;
}

/** A decoded `Schedule`, or null when the value is not one (a contract deployed from edited code may differ) */
export function asSchedule(value: Value | undefined): Schedule | null {
  if (!isRecord(value)) return null;
  const { id, creator, beneficiary, asset, total, released, start, cliff, duration, revocable, revoked_at } = value;
  if (typeof id !== 'bigint' || typeof total !== 'bigint' || typeof released !== 'bigint') return null;
  if (typeof start !== 'bigint' || typeof cliff !== 'bigint' || typeof duration !== 'bigint') return null;
  if (!isId(creator) || !isId(beneficiary) || !isId(asset) || typeof revocable !== 'boolean') return null;
  if (revoked_at !== null && typeof revoked_at !== 'bigint') return null;
  return { id, creator, beneficiary, asset, total, released, start, cliff, duration, revocable, revoked_at };
}

/** The schedules of a `Vec<Schedule>`, leaving out what is not one; null when the value is not a list */
export function asSchedules(value: Value | null): Schedule[] | null {
  if (!Array.isArray(value)) return null;
  return value.map((v) => asSchedule(v)).filter((s): s is Schedule => s !== null);
}

export function cliffAt(s: Pick<Terms, 'start' | 'cliff'>): bigint {
  return s.start + s.cliff;
}

export function endAt(s: Pick<Terms, 'start' | 'duration'>): bigint {
  return s.start + s.duration;
}

/**
 * What has vested by the time `at`, as `Schedule::vested_at`: all of the (reduced) total once revoked, nothing before
 * `start + cliff`, everything from `start + duration` on, and `floor(total · (at − start) / duration)` in between. Bigints are
 * exact, as the contract's `mul_div` is.
 */
export function vestedAt(s: Terms, at: bigint): bigint {
  if (s.revoked_at !== null) return s.total;
  if (at < cliffAt(s)) return 0n;
  const elapsed = at - s.start;
  if (elapsed >= s.duration) return s.total;
  return (s.total * elapsed) / s.duration;
}

/** What a release at the time `now` pays: what has vested and has not been released (never below 0) */
export function releasableAt(s: Terms, now: bigint): bigint {
  const due = vestedAt(s, now) - s.released;
  return due > 0n ? due : 0n;
}

export interface RevokeSplit {
  /** What stays the beneficiary's */
  kept: bigint;
  /** What goes back to the creator */
  refund: bigint;
  /** What of `kept` is still to be released */
  owed: bigint;
}

/** A revoke when `vested` has vested, as `revoke` splits the total: never less than what was released is kept */
export function splitAt(s: Pick<Terms, 'total' | 'released'>, vested: bigint): RevokeSplit {
  const kept = vested > s.released ? vested : s.released;
  const refund = s.total > kept ? s.total - kept : 0n;
  return { kept, refund, owed: kept - s.released };
}

/**
 * A revoke at the time `now`, as `revoke` counts it: what has vested (never less than what was released) stays the
 * beneficiary's (`kept`, of which `owed` is still to be released), and the rest of the total is refunded to the creator.
 */
export function revokeSplit(s: Terms, now: bigint): RevokeSplit {
  return splitAt(s, vestedAt(s, now));
}

export type Phase = 'waiting' | 'cliff' | 'vesting' | 'vested' | 'done' | 'revoked';

/** Where a schedule stands at the time `now` */
export function phaseAt(s: Terms, now: bigint): Phase {
  if (s.revoked_at !== null) return 'revoked';
  if (s.released >= s.total) return 'done';
  if (now < s.start) return 'waiting';
  if (now < cliffAt(s)) return 'cliff';
  if (now < endAt(s)) return 'vesting';
  return 'vested';
}

export type TermsProblem = 'duration' | 'cliff' | 'startEarly' | 'startFar' | 'overflow';

/**
 * The first rule of `create` the terms break, in the contract's order, or null. `start` null means now (the contract's 0).
 * `now` is the chain's time; the contract checks the start against its own, which is never earlier, so a start that passes
 * here passes there.
 */
export function checkTerms(terms: { start: bigint | null; cliff: bigint; duration: bigint }, now: bigint): TermsProblem | null {
  const { start, cliff, duration } = terms;
  if (duration < 1n || duration > MAX_DURATION) return 'duration';
  if (cliff < 0n || cliff > duration) return 'cliff';
  if (start === null) return null;
  // 0 would mean now, and a u64 has no time before 1970
  if (start < 1n) return 'startEarly';
  if (start > now + MAX_DURATION) return 'startFar';
  if (start + duration > U64_MAX) return 'overflow';
  return null;
}

export const PRESETS = { none: 0n, hour: HOUR, day: DAY, month: 30n * DAY, year: YEAR } as const;
export type Preset = keyof typeof PRESETS | 'custom';
export const PRESET_ORDER: readonly Preset[] = ['none', 'hour', 'day', 'month', 'year', 'custom'];

export const UNITS = { minutes: MINUTE, hours: HOUR, days: DAY } as const;
export type Unit = keyof typeof UNITS;

/** A whole number of minutes, hours or days typed by a person, in seconds; null unless it is a whole number above 0 */
export function customSeconds(text: string, unit: Unit): bigint | null {
  const t = text.trim();
  if (!/^\d{1,15}$/.test(t)) return null;
  const n = BigInt(t);
  return n > 0n ? n * UNITS[unit] : null;
}

/** The seconds a preset or a custom length stands for; null when the custom one is not valid */
export function choiceSeconds(choice: Preset, custom: string, unit: Unit): bigint | null {
  return choice === 'custom' ? customSeconds(custom, unit) : PRESETS[choice];
}

const LOCAL_INPUT = /^(\d{4})-(\d{2})-(\d{2})T(\d{2}):(\d{2})(?::(\d{2})(?:\.\d{1,3})?)?$/;

/**
 * The value of a `datetime-local` input (`YYYY-MM-DDTHH:mm`, seconds optional) as Unix seconds, read in this browser's time
 * zone; null when it is not a date and time. A time a clock change skips is moved on as the Date constructor does.
 */
export function parseLocalDateTime(text: string): bigint | null {
  const m = LOCAL_INPUT.exec(text.trim());
  if (!m) return null;
  const [year, month, day, hours, minutes, seconds] = [m[1], m[2], m[3], m[4], m[5], m[6] ?? '0'].map(Number) as
    [number, number, number, number, number, number];
  if (month < 1 || month > 12 || day < 1 || day > 31 || hours > 23 || minutes > 59 || seconds > 59) return null;
  const date = new Date(year, month - 1, day, hours, minutes, seconds);
  // The constructor reads the years 0 to 99 as 1900 to 1999
  if (year < 100) date.setFullYear(year);
  // A day the month does not have (31 April) rolls over into the next month
  if (date.getFullYear() !== year || date.getMonth() !== month - 1 || date.getDate() !== day) return null;
  const ms = date.getTime();
  return Number.isFinite(ms) ? BigInt(Math.floor(ms / 1000)) : null;
}

/** Unix seconds as the value of a `datetime-local` input in this browser's time zone (to the minute) */
export function localInputValue(seconds: bigint): string {
  const d = dateOf(seconds);
  if (!d) return '';
  const pad = (n: number, width = 2) => String(n).padStart(width, '0');
  return `${pad(d.getFullYear(), 4)}-${pad(d.getMonth() + 1)}-${pad(d.getDate())}T${pad(d.getHours())}:${pad(d.getMinutes())}`;
}

/** A short date for the ends of a drawn schedule: with the time when it lasts under 30 days, with the year otherwise */
export function formatTick(seconds: bigint, locale: string, duration: bigint, timeZone?: string): string {
  const d = dateOf(seconds);
  if (!d) return '';
  const options: Intl.DateTimeFormatOptions = duration < 30n * DAY
    ? { month: 'short', day: 'numeric', hour: 'numeric', minute: '2-digit' }
    : { year: 'numeric', month: 'short', day: 'numeric' };
  return formatDate(d, locale, options, timeZone) ?? d.toISOString().slice(0, 10);
}

export type DurationUnit = 'year' | 'day' | 'hour' | 'minute' | 'second';

const DURATION_STEPS: readonly [DurationUnit, bigint][] = [['year', YEAR], ['day', DAY], ['hour', HOUR], ['minute', MINUTE], ['second', 1n]];

/** A length of time in its largest units (years of 365 days), at most `most` of them, leaving out those that are 0; empty for 0 */
export function durationParts(seconds: bigint, most = 2): [DurationUnit, bigint][] {
  const out: [DurationUnit, bigint][] = [];
  let rest = seconds > 0n ? seconds : 0n;
  for (const [unit, size] of DURATION_STEPS) {
    if (out.length >= most) break;
    const n = rest / size;
    if (n > 0n) {
      out.push([unit, n]);
      rest -= n * size;
    }
  }
  return out;
}

/** The times after the start to show the vested amount at: the cliff, the quarters of the duration past it, and the end */
export function previewOffsets(cliff: bigint, duration: bigint): bigint[] {
  const out = new Set<bigint>();
  if (cliff > 0n && cliff < duration) out.add(cliff);
  for (const quarter of [1n, 2n, 3n]) {
    const at = (duration * quarter) / 4n;
    if (at > cliff) out.add(at);
  }
  out.add(duration);
  return [...out].sort((a, b) => (a < b ? -1 : a > b ? 1 : 0));
}

/** `part` as a percentage of `whole` with one decimal, rounded down ("33.3", "100"); "0" when the whole is 0 */
export function percentText(part: bigint, whole: bigint): string {
  if (whole <= 0n || part <= 0n) return '0';
  const tenths = (part * 1000n) / whole;
  const units = tenths / 10n;
  const decimal = tenths % 10n;
  return decimal === 0n ? units.toString() : `${units}.${decimal}`;
}

/** `part / whole` from 0 to 1, for bars and drawings (not for amounts) */
export function share(part: bigint, whole: bigint): number {
  if (whole <= 0n || part <= 0n) return 0;
  if (part >= whole) return 1;
  return Number((part * 1_000_000n) / whole) / 1_000_000;
}

/** A schedule ID typed by a person: a whole number from 1 to the largest u64; null otherwise */
export function parseId(text: string): bigint | null {
  const t = text.trim();
  if (!/^\d{1,20}$/.test(t)) return null;
  const id = BigInt(t);
  return id >= 1n && id <= U64_MAX ? id : null;
}

export interface CurvePoint {
  at: bigint;
  /** The vested share of the total, 0 to 1 */
  share: number;
}

/**
 * The vested share over time, to draw: flat at 0 until the cliff, the jump to what has vested since the start, the line to the
 * whole at the end, and flat after it; from a little before the start to a little after the end.
 */
export function curve(terms: Pick<Terms, 'start' | 'cliff' | 'duration'>): { from: bigint; to: bigint; points: CurvePoint[] } {
  const { start, cliff, duration } = terms;
  const pad = duration / 20n > 0n ? duration / 20n : 1n;
  const from = start > pad ? start - pad : 0n;
  const end = start + duration;
  const to = end + pad;
  const points: CurvePoint[] = [{ at: from, share: 0 }, { at: start + cliff, share: 0 }];
  if (cliff > 0n) points.push({ at: start + cliff, share: share(cliff, duration) });
  points.push({ at: end, share: 1 }, { at: to, share: 1 });
  return { from, to, points };
}
