/** A paragraph or list item of an example's text, with `code` spans set in the code font. */
import type { ReactNode } from 'react';

export function Prose({ text }: { text: string }): ReactNode {
  const parts = text.split(/(`[^`]+`)/g);
  return <>{parts.map((p, i) => (p.startsWith('`') && p.endsWith('`') && p.length > 2 ? <code key={i} className="mono">{p.slice(1, -1)}</code> : p))}</>;
}

export function HowItWorks({ title, items }: { title: string; items: readonly string[] }) {
  return <section className="panel how">
    <h2 className="panel-title">{title}</h2>
    <ul className="how-list">{items.map((s, i) => <li key={i}><Prose text={s} /></li>)}</ul>
  </section>;
}
