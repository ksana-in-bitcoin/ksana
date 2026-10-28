/** Pieces the lending page's panels share: amounts, sentences with values in them, an amount input with its own Max, a mode switch. */
import { Fragment, createElement, useEffect, useId, useLayoutEffect, useState, type ReactNode, type RefObject } from 'react';
import type { BtcNetwork } from '@ksana/sdk';
import { KSA_HEX, formatUnits, parseUnits } from '../../chain/assets';
import { useT } from '../../i18n';
import { errorText } from '../../ui/errors';
import { Button } from '../../ui/ui';
import { AccountText, KSA_FEE_RESERVE, sameAccount } from '../../ui/values';
import { healthBps, healthState, healthText, type HealthState } from './math';
import type { Unit } from './reads';

/** An amount in the asset's units with its name, free to wrap on a narrow screen */
export function AmountView({ unit, value }: { unit: Unit; value: bigint }) {
  return <span className="lending-amount"><span className="mono">{formatUnits(value, unit.decimals)}</span> <span className="unit">{unit.name}</span></span>;
}

/** A sentence of the text pack whose values are nodes (an amount, an account), its parts in the language's order */
export function Words({ parts }: { parts: readonly ReactNode[] }) {
  return createElement(Fragment, null, ...parts);
}

/** An account in a sentence, marked when it is the connected wallet's */
export function Who({ account, me, network }: { account: Uint8Array; me: Uint8Array | null; network: BtcNetwork | undefined }) {
  const t = useT();
  return <span className="lending-who">
    <AccountText value={account} network={network} chars={6} />
    {sameAccount(account, me) && <span className="pill pill--ok">{t.core.common.you}</span>}
  </span>;
}

const PILLS: Record<HealthState, string> = { none: 'pill--quiet', safe: 'pill--ok', near: 'pill--warn', liquidatable: 'pill--danger' };

/** A position's health as a factor and its state in words */
export function HealthBadge({ debt, liquidationLimit }: { debt: bigint; liquidationLimit: bigint }) {
  const m = useT().lending;
  const state = healthState(debt, liquidationLimit);
  const health = healthBps(liquidationLimit, debt);
  return <span className="lending-badge">
    {health !== null && <span className="mono">{healthText(health)}</span>}
    <span className={`pill ${PILLS[state]}`}>{m.states[state]}</span>
  </span>;
}

/** The amount typed, in base units, when it is a valid amount above 0 */
export function positiveAmount(text: string, decimals: number): bigint | null {
  if (!text.trim()) return null;
  try {
    const v = parseUnits(text, decimals);
    return v > 0n ? v : null;
  } catch {
    return null;
  }
}

/** What a Max button may attach of the wallet's balance: all of it, but of KSA what the fee leaves (as the shared amount field does) */
export function spendable(unit: Unit, balance: bigint): bigint {
  if (unit.hex !== KSA_HEX) return balance;
  return balance > KSA_FEE_RESERVE ? balance - KSA_FEE_RESERVE : 0n;
}

/** Whether attaching `amount` of the asset leaves less KSA than the fee needs */
export function eatsFee(unit: Unit, amount: bigint | null, balance: bigint | null): boolean {
  return unit.hex === KSA_HEX && amount !== null && balance !== null && amount <= balance && amount + KSA_FEE_RESERVE > balance;
}

/** The text a Max button fills in for an amount */
export function amountInput(value: bigint, decimals: number): string {
  return formatUnits(value, decimals).replace(/,/g, '');
}

/**
 * An amount typed in an asset's units. Its Max button runs `onMax`, so a form can fill in a limit it works out (and remember that
 * the person asked for all of something); `problem` is the form's own reason the amount cannot be sent, shown in place of the hint.
 */
export function AmountInput({ label, unit, decimals, value, onChange, onMax, hint, problem, disabled }: {
  label: ReactNode; unit: string; decimals: number; value: string; onChange: (v: string) => void; onMax?: (() => void) | null;
  hint?: ReactNode; problem?: string | null; disabled?: boolean;
}) {
  const t = useT();
  const m = t.core;
  const id = useId();
  let error: string | null = null;
  if (value.trim()) {
    try {
      parseUnits(value, decimals);
    } catch (e) {
      error = errorText(e, t);
    }
  }
  const shown = error ?? problem ?? null;
  return <div className="vfield">
    <label className="field-label" htmlFor={id}>{label}</label>
    <div className="input-row">
      <input id={id} className="input mono" inputMode="decimal" autoComplete="off" disabled={disabled} value={value} placeholder="0"
        onChange={(e) => onChange(e.target.value)} aria-invalid={shown ? true : undefined} />
      <span className="unit">{unit}</span>
      {onMax && <Button size="sm" variant="ghost" disabled={disabled} onClick={onMax}>{m.common.max}</Button>}
    </div>
    {shown ? <span className="field-error">{shown}</span> : hint ? <span className="field-hint">{hint}</span> : null}
  </div>;
}

/** A row of buttons that picks one of a few modes, in the shared `segmented` style */
export function Modes<T extends string>({ label, value, options, onChange }: {
  label: string; value: T; options: readonly { value: T; label: ReactNode }[]; onChange: (value: T) => void;
}) {
  return <div className="segmented lending-modes" role="group" aria-label={label}>
    {options.map((o) => <button key={o.value} type="button" aria-pressed={o.value === value} className={o.value === value ? 'is-active' : undefined}
      onClick={() => onChange(o.value)}>{o.label}</button>)}
  </div>;
}

/** The latest value a view read, kept through a failed re-read so the forms keep what was typed */
export function useLastGood<T>(value: T | null): T | null {
  const [kept, setKept] = useState<T | null>(value);
  useEffect(() => {
    if (value !== null) setKept(value);
  }, [value]);
  return value ?? kept;
}

/** The element's width in CSS pixels, followed as it changes */
export function useWidth(ref: RefObject<HTMLElement | null>): number {
  const [width, setWidth] = useState(0);
  useLayoutEffect(() => {
    const el = ref.current;
    if (!el) return;
    setWidth(el.clientWidth);
    if (typeof ResizeObserver === 'undefined') return;
    const observer = new ResizeObserver((entries) => {
      const w = entries[0]?.contentRect.width;
      if (w) setWidth(Math.round(w));
    });
    observer.observe(el);
    return () => observer.disconnect();
  }, [ref]);
  return width;
}
