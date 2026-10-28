/** Short notes in the corner when an operation lands or fails (the Activity tab keeps the record). */
import { useEffect } from 'react';
import { createStore, useStore } from '../app/store';
import { explorerLink } from '../chain/deployment';
import { useT } from '../i18n';

export interface Toast {
  id: number;
  tone: 'ok' | 'warn' | 'danger' | 'info';
  title: string;
  body?: string;
  opId?: string;
}

const toastStore = createStore<Toast[]>([]);
let next = 0;

export function toast(t: Omit<Toast, 'id'>): void {
  const id = ++next;
  toastStore.update((list) => [...list, { ...t, id }].slice(-4));
}

function dismiss(id: number) {
  toastStore.update((list) => list.filter((t) => t.id !== id));
}

export function Toasts() {
  const list = useStore(toastStore);
  return <div className="toasts" aria-live="polite">{list.map((t) => <ToastItem key={t.id} t={t} />)}</div>;
}

function ToastItem({ t }: { t: Toast }) {
  const m = useT().core;
  useEffect(() => {
    const timer = setTimeout(() => dismiss(t.id), t.tone === 'danger' ? 12_000 : 6_000);
    return () => clearTimeout(timer);
  }, [t.id, t.tone]);
  const link = t.opId ? explorerLink('op', t.opId) : null;
  return <div className={`toast toast--${t.tone}`} role={t.tone === 'danger' ? 'alert' : 'status'}>
    <div className="toast-title">{t.title}</div>
    {t.body && <div className="toast-body">{t.body}</div>}
    {link && <a className="toast-link" href={link} target="_blank" rel="noreferrer">{m.activity.explorer}</a>}
    <button type="button" className="icon-btn toast-close" aria-label={m.common.close} onClick={() => dismiss(t.id)}>×</button>
  </div>;
}
