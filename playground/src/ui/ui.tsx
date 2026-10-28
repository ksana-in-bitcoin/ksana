/** The small building blocks every page uses. */
import { Fragment, useEffect, useId, useRef, useState, type ButtonHTMLAttributes, type ReactNode } from 'react';
import { useT } from '../i18n';

type Variant = 'primary' | 'secondary' | 'ghost' | 'danger';

export function Button({ variant = 'secondary', size = 'md', busy = false, className = '', children, ...rest }:
  ButtonHTMLAttributes<HTMLButtonElement> & { variant?: Variant; size?: 'sm' | 'md'; busy?: boolean }) {
  return <button type="button" className={`btn btn--${variant} btn--${size}${busy ? ' is-busy' : ''} ${className}`} aria-busy={busy || undefined} {...rest}>
    {children}
  </button>;
}

export function Section({ title, actions, children, className = '' }: { title?: ReactNode; actions?: ReactNode; children: ReactNode; className?: string }) {
  return <section className={`panel ${className}`}>
    {(title || actions) && <div className="panel-head">{title && <h2 className="panel-title">{title}</h2>}{actions && <div className="panel-actions">{actions}</div>}</div>}
    {children}
  </section>;
}

export function Field({ label, hint, error, children, htmlFor }: { label: ReactNode; hint?: ReactNode; error?: string | null; children: ReactNode; htmlFor?: string }) {
  return <div className="field">
    <label className="field-label" htmlFor={htmlFor}>{label}</label>
    {children}
    {error ? <span className="field-error" role="alert">{error}</span> : hint ? <span className="field-hint">{hint}</span> : null}
  </div>;
}

export function Banner({ tone = 'info', title, children }: { tone?: 'info' | 'warn' | 'danger' | 'ok'; title?: ReactNode; children?: ReactNode }) {
  return <div className={`banner banner--${tone}`} role={tone === 'danger' ? 'alert' : 'status'}>
    {title && <div className="banner-title">{title}</div>}
    {children && <div className="banner-body">{children}</div>}
  </div>;
}

export function Tabs<T extends string>({ value, options, onChange, label }: { value: T; options: { value: T; label: ReactNode }[]; onChange: (v: T) => void; label: string }) {
  return <div className="tabs" role="tablist" aria-label={label}>
    {options.map((o) => <button key={o.value} type="button" role="tab" aria-selected={o.value === value} className={`tab${o.value === value ? ' is-active' : ''}`} onClick={() => onChange(o.value)}>{o.label}</button>)}
  </div>;
}

export function Modal({ open, title, onClose, children, footer, wide = false }: { open: boolean; title: ReactNode; onClose: () => void; children: ReactNode; footer?: ReactNode; wide?: boolean }) {
  const m = useT().core.common;
  const ref = useRef<HTMLDialogElement>(null);
  const titleId = useId();
  useEffect(() => {
    const d = ref.current;
    if (!d) return;
    if (open && !d.open) d.showModal();
    if (!open && d.open) d.close();
  }, [open]);
  return <dialog ref={ref} className={`modal${wide ? ' modal--wide' : ''}`} aria-labelledby={titleId} onCancel={(e) => { e.preventDefault(); onClose(); }}>
    {open && <>
      <div className="modal-head"><h2 id={titleId} className="modal-title">{title}</h2>
        <button type="button" className="icon-btn" aria-label={m.close} onClick={onClose}>×</button></div>
      <div className="modal-body">{children}</div>
      {footer && <div className="modal-foot">{footer}</div>}
    </>}
  </dialog>;
}

export async function copyText(text: string): Promise<boolean> {
  try {
    await navigator.clipboard.writeText(text);
    return true;
  } catch {
    return false;
  }
}

/** Hands `data` to the browser as a file to save */
export function saveFile(name: string, data: BlobPart, type: string): void {
  const url = URL.createObjectURL(new Blob([data], { type }));
  const a = document.createElement('a');
  a.href = url;
  a.download = name;
  a.click();
  setTimeout(() => URL.revokeObjectURL(url), 1_000);
}

export function CopyButton({ text, label }: { text: string; label?: string }) {
  const m = useT().core.common;
  const [done, setDone] = useState(false);
  useEffect(() => {
    if (!done) return;
    const t = setTimeout(() => setDone(false), 1500);
    return () => clearTimeout(t);
  }, [done]);
  return <button type="button" className="copy-btn" onClick={() => void copyText(text).then(setDone)} aria-label={label ?? m.copy}>
    {done ? m.copied : label ?? m.copy}
  </button>;
}

/** A long ID shortened in the middle, whole on hover and when copied */
export function Id({ value, href, copy = true, chars = 8 }: { value: string; href?: string | null; copy?: boolean; chars?: number }) {
  const short = value.length > chars * 2 + 1 ? `${value.slice(0, chars)}…${value.slice(-chars)}` : value;
  return <span className="id">
    {href ? <a href={href} target="_blank" rel="noreferrer" title={value} className="mono">{short}</a> : <span className="mono" title={value}>{short}</span>}
    {copy && <CopyButton text={value} />}
  </span>;
}

export function Spinner({ label }: { label?: string }) {
  return <span className="spinner" role="status" aria-label={label}><span aria-hidden="true" /></span>;
}

export function Rows({ rows }: { rows: ({ label: ReactNode; value: ReactNode } | false | null | undefined)[] }) {
  return <dl className="rows">
    {rows.filter((r): r is { label: ReactNode; value: ReactNode } => !!r).map((r, i) => <div key={i} className="row-item"><dt>{r.label}</dt><dd>{r.value}</dd></div>)}
  </dl>;
}

export function Stat({ label, value, sub }: { label: ReactNode; value: ReactNode; sub?: ReactNode }) {
  return <div className="stat"><div className="stat-label">{label}</div><div className="stat-value">{value}</div>{sub && <div className="stat-sub">{sub}</div>}</div>;
}

export function Empty({ children }: { children: ReactNode }) {
  return <p className="empty">{children}</p>;
}

/** A bar from 0 to 100% (utilization, vesting progress) */
export function Meter({ value, label, tone = 'qing' }: { value: number; label?: string; tone?: 'qing' | 'warn' | 'danger' }) {
  const v = Math.max(0, Math.min(1, Number.isFinite(value) ? value : 0));
  return <div className={`meter meter--${tone}`} role="meter" aria-valuemin={0} aria-valuemax={100} aria-valuenow={Math.round(v * 100)} aria-label={label}>
    <span style={{ width: `${v * 100}%` }} />
  </div>;
}

/** Text from the pack with `{name}` slots filled with nodes (accounts, amounts, token IDs) */
export function Sentence({ text, parts }: { text: string; parts: Record<string, ReactNode> }) {
  return <>{text.split(/\{(\w+)\}/g).map((piece, i) => <Fragment key={i}>{i % 2 === 1 ? parts[piece] ?? piece : piece}</Fragment>)}</>;
}
