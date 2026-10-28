/** The vesting page's arithmetic against the contract's rules and tests (cliff, end, rounding, revokes, terms) and its time inputs. */
import { describe, expect, it } from 'vitest';
import {
  DAY, HOUR, MAX_DURATION, MINUTE, PRESETS, U64_MAX, YEAR, asSchedule, asSchedules, checkTerms, choiceSeconds, curve,
  customSeconds, dateLocale, dateOf, durationParts, formatDateTime, formatTick, localInputValue, parseId, parseLocalDateTime, percentText, phaseAt,
  previewOffsets, releasableAt, revokeSplit, share, vestedAt, type Terms,
} from './schedule';

/** The block timestamp the contract's tests deploy at */
const T0 = 1_800_000_000n;
const U128_MAX = (1n << 128n) - 1n;

function terms(total: bigint, start: bigint, cliff: bigint, duration: bigint, extra: Partial<Terms> = {}): Terms {
  return { total, released: 0n, start, cliff, duration, revoked_at: null, ...extra };
}

/** `mul_div` as the contract computes it: `q · numerator + floor(r · numerator / denominator)` with `value = q · denominator + r` */
function mulDiv(value: bigint, numerator: bigint, denominator: bigint): bigint {
  return (value / denominator) * numerator + ((value % denominator) * numerator) / denominator;
}

/** A 64-bit LCG, as the contract's tests use, for values of every magnitude */
function rng(seed: bigint) {
  let state = seed;
  const mask = (1n << 64n) - 1n;
  const half = () => {
    state = (state * 6_364_136_223_846_793_005n + 1_442_695_040_888_963_407n) & mask;
    return state >> 32n;
  };
  const bits = () => (half() << 32n) | half();
  return {
    below: (bound: bigint) => bits() % bound,
    u128: () => (((bits() << 64n) | bits()) >> (bits() % 128n)),
  };
}

describe('vestedAt', () => {
  it('follows the cliff and the line (vesting_follows_the_cliff_and_the_line)', () => {
    const start = T0 + 1_000n;
    const s = terms(1_000_000n, start, 250n, 1_000n);
    const table: [bigint, bigint][] = [
      [0n, 0n], [T0, 0n], [start - 1n, 0n], [start, 0n], [start + 249n, 0n], [start + 250n, 250_000n], [start + 251n, 251_000n],
      [start + 999n, 999_000n], [start + 1_000n, 1_000_000n], [start + 1_001n, 1_000_000n], [U64_MAX, 1_000_000n],
    ];
    for (const [at, expected] of table) expect(vestedAt(s, at), `at ${at}`).toBe(expected);
  });

  it('rounds down and the parts add up (rounding_goes_down_and_the_parts_add_up)', () => {
    // 10 over 3 s: 3, then 6, then all 10; released one second at a time: 3, 3, 4
    const s = terms(10n, T0, 0n, 3n);
    const paid: bigint[] = [];
    for (const second of [1n, 2n, 3n]) {
      const due = releasableAt(s, T0 + second);
      paid.push(due);
      s.released += due;
    }
    expect(paid).toEqual([3n, 3n, 4n]);
    // 1 over 2 s: nothing until the end
    expect([0n, 1n, 2n].map((d) => vestedAt(terms(1n, T0, 0n, 2n), T0 + d))).toEqual([0n, 0n, 1n]);
    // A cliff as long as the duration: nothing, then everything
    expect([99n, 100n].map((d) => vestedAt(terms(1_000n, T0, 100n, 100n), T0 + d))).toEqual([0n, 1_000n]);
    // No cliff: linear from the start
    expect([0n, 1n, 99n].map((d) => vestedAt(terms(1_000n, T0, 0n, 100n), T0 + d))).toEqual([0n, 10n, 990n]);
  });

  it('is the reduced total, whatever the time, once revoked', () => {
    const s = terms(450n, T0, 100n, 1_000n, { revoked_at: T0 + 450n, released: 200n });
    for (const at of [0n, T0, T0 + 450n, U64_MAX]) expect(vestedAt(s, at)).toBe(450n);
    expect(releasableAt(s, T0)).toBe(250n);
    expect(releasableAt({ ...s, released: 450n }, U64_MAX)).toBe(0n);
  });

  it('is exact near u128::MAX (totals_near_u128_max_vest_exactly)', () => {
    const total = U128_MAX - 7n;
    const s = terms(total, T0, 0n, MAX_DURATION);
    for (const elapsed of [0n, 1n, 2n, 3n, 999_999_937n, MAX_DURATION / 3n, MAX_DURATION / 2n, MAX_DURATION - 1n]) {
      expect(vestedAt(s, T0 + elapsed), `${elapsed}`).toBe(mulDiv(total, elapsed, MAX_DURATION));
    }
    expect(vestedAt(s, T0 + MAX_DURATION)).toBe(total);
    // Released at a third, revoked at half, released again: the parts add up to what was kept
    const first = releasableAt(s, T0 + MAX_DURATION / 3n);
    s.released = first;
    const { kept, refund } = revokeSplit(s, T0 + MAX_DURATION / 2n);
    expect(kept).toBe(mulDiv(total, MAX_DURATION / 2n, MAX_DURATION));
    expect(refund).toBe(total - kept);
    const revoked = { ...s, total: kept, revoked_at: T0 + MAX_DURATION / 2n };
    expect(first + releasableAt(revoked, T0 + MAX_DURATION / 2n)).toBe(kept);
    // A small one with its cliff at half time: floor(7 / 2), then the rest at the end
    const small = terms(7n, T0, MAX_DURATION / 2n, MAX_DURATION);
    expect(vestedAt(small, T0 + MAX_DURATION / 2n)).toBe(3n);
    expect(vestedAt(small, T0 + MAX_DURATION)).toBe(7n);
  });

  it('agrees with mul_div, never exceeds the total and never decreases (vested_at_follows_the_rule)', () => {
    const r = rng(42n);
    for (let i = 0; i < 2_000; i++) {
      const duration = r.below(MAX_DURATION) + 1n;
      const cliff = r.below(duration + 1n);
      const start = r.below(1n << 40n);
      const s = terms(r.u128(), start, cliff, duration);
      const times = [0n, start, start + cliff, start + cliff + 1n, start + duration - 1n, start + duration, start + r.below(duration), U64_MAX]
        .sort((a, b) => (a < b ? -1 : a > b ? 1 : 0));
      let previous = 0n;
      for (const at of times) {
        const vested = vestedAt(s, at);
        const expected = at < start + cliff ? 0n : at - start >= duration ? s.total : mulDiv(s.total, at - start, duration);
        expect(vested).toBe(expected);
        expect(previous <= vested && vested <= s.total).toBe(true);
        expect(releasableAt(s, at)).toBe(vested);
        previous = vested;
      }
    }
  });
});

describe('revokeSplit', () => {
  it('refunds everything before the cliff (revoke_before_the_cliff_refunds_everything)', () => {
    expect(revokeSplit(terms(1_000n, T0, 100n, 1_000n), T0 + 99n)).toEqual({ kept: 0n, refund: 1_000n, owed: 0n });
  });

  it('keeps what has vested in the middle (revoke_in_the_middle_keeps_what_has_vested)', () => {
    const s = terms(1_000n, T0, 100n, 1_000n, { released: 200n });
    expect(revokeSplit(s, T0 + 450n)).toEqual({ kept: 450n, refund: 550n, owed: 250n });
  });

  it('refunds nothing after the end (revoke_after_the_end_refunds_nothing)', () => {
    expect(revokeSplit(terms(1_000n, T0, 0n, 1_000n), T0 + 1_000n)).toEqual({ kept: 1_000n, refund: 0n, owed: 1_000n });
  });

  it('never takes back what was released, even at an earlier time (an_earlier_timestamp_counts_as_no_time_passed)', () => {
    // Released 700 by T0 + 700; a revoke counted at T0 + 650 refunds 300, not 350
    const s = terms(1_000n, T0, 0n, 1_000n, { released: 700n });
    expect(revokeSplit(s, T0 + 650n)).toEqual({ kept: 700n, refund: 300n, owed: 0n });
  });
});

describe('phaseAt', () => {
  it('names where a schedule stands', () => {
    const s = terms(1_000n, T0, 100n, 1_000n);
    expect(phaseAt(s, T0 - 1n)).toBe('waiting');
    expect(phaseAt(s, T0)).toBe('cliff');
    expect(phaseAt(s, T0 + 99n)).toBe('cliff');
    expect(phaseAt(s, T0 + 100n)).toBe('vesting');
    expect(phaseAt(s, T0 + 1_000n)).toBe('vested');
    expect(phaseAt({ ...s, released: 1_000n }, T0 + 1_000n)).toBe('done');
    expect(phaseAt({ ...s, revoked_at: T0 + 5n }, T0 + 1_000n)).toBe('revoked');
  });
});

describe('checkTerms', () => {
  const now = T0;
  it('refuses what create refuses (create_checks_its_terms), in its order', () => {
    expect(checkTerms({ start: null, cliff: 0n, duration: 0n }, now)).toBe('duration');
    expect(checkTerms({ start: null, cliff: 0n, duration: MAX_DURATION + 1n }, now)).toBe('duration');
    expect(checkTerms({ start: null, cliff: 101n, duration: 100n }, now)).toBe('cliff');
    expect(checkTerms({ start: now + MAX_DURATION + 1n, cliff: 0n, duration: 100n }, now)).toBe('startFar');
    // A start in milliseconds
    expect(checkTerms({ start: now * 1_000n, cliff: 0n, duration: 100n }, now)).toBe('startFar');
    expect(checkTerms({ start: 0n, cliff: 0n, duration: 100n }, now)).toBe('startEarly');
    expect(checkTerms({ start: -5n, cliff: 0n, duration: 100n }, now)).toBe('startEarly');
    expect(checkTerms({ start: U64_MAX - 5n, cliff: 0n, duration: 100n }, U64_MAX)).toBe('overflow');
    // The first broken rule wins
    expect(checkTerms({ start: 0n, cliff: 5n, duration: 0n }, now)).toBe('duration');
  });

  it('takes the edges themselves', () => {
    expect(checkTerms({ start: null, cliff: MAX_DURATION, duration: MAX_DURATION }, now)).toBeNull();
    expect(checkTerms({ start: now + MAX_DURATION, cliff: 0n, duration: MAX_DURATION }, now)).toBeNull();
    expect(checkTerms({ start: null, cliff: 0n, duration: 1n }, now)).toBeNull();
    // A start of 1 is a start in 1970: all of it has vested at once
    expect(checkTerms({ start: 1n, cliff: 0n, duration: 100n }, now)).toBeNull();
    expect(vestedAt(terms(100n, 1n, 0n, 100n), now)).toBe(100n);
  });
});

describe('lengths of time', () => {
  it('has the presets the form offers', () => {
    expect(PRESETS).toEqual({ none: 0n, hour: 3_600n, day: 86_400n, month: 2_592_000n, year: 31_536_000n });
    // 100 years of the contract are 100 of these years
    expect(100n * YEAR).toBe(MAX_DURATION);
  });

  it('reads custom whole numbers of minutes, hours and days', () => {
    expect(customSeconds('90', 'minutes')).toBe(5_400n);
    expect(customSeconds(' 6 ', 'hours')).toBe(6n * HOUR);
    expect(customSeconds('36500', 'days')).toBe(MAX_DURATION);
    for (const bad of ['', '0', '-1', '1.5', '1e3', 'abc', '1 000', '1234567890123456']) expect(customSeconds(bad, 'days'), bad).toBeNull();
    expect(choiceSeconds('none', 'x', 'days')).toBe(0n);
    expect(choiceSeconds('month', '', 'days')).toBe(30n * DAY);
    expect(choiceSeconds('custom', '2', 'days')).toBe(2n * DAY);
    expect(choiceSeconds('custom', '', 'days')).toBeNull();
  });

  it('writes lengths in their largest units', () => {
    expect(durationParts(0n)).toEqual([]);
    expect(durationParts(1n)).toEqual([['second', 1n]]);
    expect(durationParts(90n)).toEqual([['minute', 1n], ['second', 30n]]);
    expect(durationParts(HOUR)).toEqual([['hour', 1n]]);
    expect(durationParts(5_400n)).toEqual([['hour', 1n], ['minute', 30n]]);
    expect(durationParts(30n * DAY)).toEqual([['day', 30n]]);
    expect(durationParts(YEAR)).toEqual([['year', 1n]]);
    expect(durationParts(YEAR / 4n)).toEqual([['day', 91n], ['hour', 6n]]);
    expect(durationParts(YEAR + 3n * HOUR)).toEqual([['year', 1n], ['hour', 3n]]);
    expect(durationParts(MAX_DURATION)).toEqual([['year', 100n]]);
    expect(durationParts(DAY + HOUR + MINUTE, 3)).toEqual([['day', 1n], ['hour', 1n], ['minute', 1n]]);
  });

  it('picks the cliff, the quarters past it and the end to preview', () => {
    expect(previewOffsets(0n, 100n)).toEqual([25n, 50n, 75n, 100n]);
    expect(previewOffsets(30n * DAY, YEAR)).toEqual([30n * DAY, YEAR / 4n, YEAR / 2n, (YEAR * 3n) / 4n, YEAR]);
    expect(previewOffsets(YEAR / 2n, YEAR)).toEqual([YEAR / 2n, (YEAR * 3n) / 4n, YEAR]);
    expect(previewOffsets(100n, 100n)).toEqual([100n]);
    expect(previewOffsets(0n, 1n)).toEqual([1n]);
  });
});

describe('the time inputs', () => {
  it('reads a datetime-local value in this time zone', () => {
    expect(parseLocalDateTime('2026-10-19T14:30')).toBe(BigInt(new Date(2026, 9, 19, 14, 30).getTime() / 1000));
    expect(parseLocalDateTime('2026-10-19T14:30:15')).toBe(BigInt(new Date(2026, 9, 19, 14, 30, 15).getTime() / 1000));
    expect(parseLocalDateTime('2028-02-29T00:00')).toBe(BigInt(new Date(2028, 1, 29).getTime() / 1000));
    for (const bad of ['', '2026-10-19', '2026-10-19 14:30', '2026-13-01T00:00', '2026-04-31T10:00', '2027-02-29T10:00', '2026-10-19T24:00', 'soon']) {
      expect(parseLocalDateTime(bad), bad).toBeNull();
    }
  });

  it('writes a time back as a datetime-local value', () => {
    const at = parseLocalDateTime('2031-01-02T03:04')!;
    expect(localInputValue(at)).toBe('2031-01-02T03:04');
    expect(parseLocalDateTime(localInputValue(T0))).toBe(T0 - (T0 % 60n));
    expect(localInputValue(1n << 60n)).toBe('');
  });

  it('formats dates, and keeps times no Date holds as seconds', () => {
    expect(dateOf(T0)?.toISOString()).toBe('2027-01-15T08:00:00.000Z');
    expect(dateOf(U64_MAX)).toBeNull();
    const text = formatDateTime(T0, 'en-GB', 'UTC');
    expect(text).toContain('2027');
    expect(text).toContain('08:00');
    expect(formatDateTime(U64_MAX, 'en')).toBe(`${U64_MAX} s`);
    // A tag Intl refuses is written in plain English, not as an ISO string
    expect(formatDateTime(T0, 'en-US@posix', 'UTC')).toBe(formatDateTime(T0, 'en', 'UTC'));
  });

  it('writes the ends of a drawing with the time under 30 days and with the year from 30 days on', () => {
    const short = formatTick(T0, 'en', 30n * DAY - 1n, 'UTC');
    expect(short).toContain('Jan 15');
    expect(short).toContain('8:00');
    expect(short).not.toContain('2027');
    const long = formatTick(T0, 'en', 30n * DAY, 'UTC');
    expect(long).toContain('2027');
    expect(long).not.toContain('8:00');
    expect(formatTick(U64_MAX, 'en', DAY)).toBe('');
  });

  it('writes dates in the page language, in the browser\'s English when Intl takes its tag', () => {
    expect(dateLocale('zh-CN', 'en-GB')).toBe('zh-CN');
    expect(dateLocale('en', 'en-GB')).toBe('en-GB');
    expect(dateLocale('en', 'EN-au')).toBe('en-AU');
    expect(dateLocale('en', 'de-DE')).toBe('en');
    expect(dateLocale('en', 'en-US@posix')).toBe('en');
    expect(dateLocale('en', '')).toBe('en');
  });

  it('reads schedule IDs from 1 to the largest u64', () => {
    expect(parseId('1')).toBe(1n);
    expect(parseId(' 42 ')).toBe(42n);
    expect(parseId(U64_MAX.toString())).toBe(U64_MAX);
    for (const bad of ['', '0', '-1', '1.5', '+3', '0x10', (U64_MAX + 1n).toString()]) expect(parseId(bad), bad).toBeNull();
  });
});

describe('shares', () => {
  it('writes percentages rounded down to a tenth', () => {
    expect(percentText(1n, 3n)).toBe('33.3');
    expect(percentText(2n, 3n)).toBe('66.6');
    expect(percentText(1n, 1n)).toBe('100');
    expect(percentText(999n, 1_000n)).toBe('99.9');
    expect(percentText(1n, 2_000n)).toBe('0');
    expect(percentText(0n, 0n)).toBe('0');
    expect(percentText(U128_MAX / 2n, U128_MAX)).toBe('49.9');
  });

  it('gives bars a share from 0 to 1', () => {
    expect(share(0n, 10n)).toBe(0);
    expect(share(5n, 10n)).toBe(0.5);
    expect(share(15n, 10n)).toBe(1);
    expect(share(1n, 0n)).toBe(0);
    expect(share(U128_MAX / 4n, U128_MAX)).toBeCloseTo(0.25, 5);
  });

  it('draws the curve flat to the cliff, up to the line, then to the whole', () => {
    const c = curve({ start: T0, cliff: 250n, duration: 1_000n });
    expect(c.from).toBe(T0 - 50n);
    expect(c.to).toBe(T0 + 1_050n);
    expect(c.points).toEqual([
      { at: T0 - 50n, share: 0 }, { at: T0 + 250n, share: 0 }, { at: T0 + 250n, share: 0.25 }, { at: T0 + 1_000n, share: 1 }, { at: T0 + 1_050n, share: 1 },
    ]);
    // No cliff: no jump; a start near 1970 does not draw before it
    expect(curve({ start: 1n, cliff: 0n, duration: 10n }).points).toEqual([
      { at: 0n, share: 0 }, { at: 1n, share: 0 }, { at: 11n, share: 1 }, { at: 12n, share: 1 },
    ]);
  });
});

describe('asSchedule', () => {
  const id32 = (b: number) => new Uint8Array(32).fill(b);
  const value = {
    id: 3n, creator: id32(0xa1), beneficiary: id32(0xb0), asset: id32(0), total: 1_000n, released: 0n,
    start: T0, cliff: 50n, duration: 1_000n, revocable: true, revoked_at: null,
  };

  it('takes the decoded struct and refuses other values', () => {
    expect(asSchedule(value)).toEqual(value);
    expect(asSchedule({ ...value, revoked_at: T0 + 5n })?.revoked_at).toBe(T0 + 5n);
    expect(asSchedule({ ...value, total: '1000' })).toBeNull();
    expect(asSchedule({ ...value, creator: new Uint8Array(31) })).toBeNull();
    const { revoked_at: _, ...missing } = value;
    expect(asSchedule(missing)).toBeNull();
    expect(asSchedule(null)).toBeNull();
    expect(asSchedule([value])).toBeNull();
    expect(asSchedules([value, null, { ...value, id: 4n }])?.map((s) => s.id)).toEqual([3n, 4n]);
    expect(asSchedules(null)).toBeNull();
  });
});
