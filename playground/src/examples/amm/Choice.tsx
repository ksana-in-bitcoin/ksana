/** A row of buttons that picks one of a few values, in the shared `segmented` style. */
import type { ReactNode } from 'react';

export function Choice<T extends string>({ label, value, options, onChange, className = '' }: {
  label: string; value: T; options: readonly { value: T; label: ReactNode }[]; onChange: (value: T) => void; className?: string;
}) {
  return <div className={`segmented ${className}`} role="group" aria-label={label}>
    {options.map((o) => <button key={o.value} type="button" aria-pressed={o.value === value} className={o.value === value ? 'is-active' : undefined}
      onClick={() => onChange(o.value)}>{o.label}</button>)}
  </div>;
}
