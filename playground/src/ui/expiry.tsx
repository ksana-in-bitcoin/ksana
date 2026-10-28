/**
 * The expiry of an allowance (KS-20 `approve`) or of an operator (KS-721 `set_operator`): `expires_at` in Unix seconds, which
 * counts while the block's timestamp is below it (`docs/STANDARDS.md` §2), chosen as a span from the chain's time, or never.
 */
import { useId } from 'react';
import { dateLocale, formatDateTime, type ChainTime } from '../chain/time';
import { useLocale, useT } from '../i18n';
import { Field } from './ui';

export const EXPIRY_CHOICES = ['hour', 'day', 'week', 'month', 'never'] as const;
export type ExpiryChoice = (typeof EXPIRY_CHOICES)[number];

const SPANS: Record<Exclude<ExpiryChoice, 'never'>, bigint> = { hour: 3_600n, day: 86_400n, week: 604_800n, month: 2_592_000n };

/** The `expires_at` a choice gives at the chain's time `now`; null for never */
export function expiryOf(choice: ExpiryChoice, now: bigint): bigint | null {
  return choice === 'never' ? null : now + SPANS[choice];
}

/** Whether an allowance or an operator expiring at `expiresAt` no longer counts at `now` (`standard::expired`) */
export function isExpired(expiresAt: bigint | null, now: bigint): boolean {
  return expiresAt !== null && now >= expiresAt;
}

/** The choice of an expiry, with the date it comes to */
export function ExpiryField({ value, onChange, time, disabled = false }: {
  value: ExpiryChoice; onChange: (choice: ExpiryChoice) => void; time: ChainTime; disabled?: boolean;
}) {
  const m = useT().core.expiry;
  const locale = dateLocale(useLocale());
  const id = useId();
  const at = expiryOf(value, time.now);
  const hint = at === null ? m.neverHint : time.chain ? m.until(formatDateTime(at, locale)) : m.about(formatDateTime(at, locale));
  return <Field label={m.label} htmlFor={id} hint={hint}>
    <select id={id} className="input" value={value} disabled={disabled} onChange={(e) => onChange(e.target.value as ExpiryChoice)}>
      {EXPIRY_CHOICES.map((choice) => <option key={choice} value={choice}>{m.choices[choice]}</option>)}
    </select>
  </Field>;
}

/** An expiry as one phrase: until when it counts, since when it no longer does, or that it never expires */
export function useExpiryText(): (expiresAt: bigint | null, now: bigint) => string {
  const m = useT().core.expiry;
  const locale = dateLocale(useLocale());
  return (expiresAt, now) => (expiresAt === null ? m.never
    : isExpired(expiresAt, now) ? m.expired(formatDateTime(expiresAt, locale)) : m.until(formatDateTime(expiresAt, locale)));
}
