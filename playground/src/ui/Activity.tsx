/** The operations sent from this page (and the ones the journal remembers from earlier visits), with their status. */
import { useState } from 'react';
import { failureText } from '../chain/views';
import { explorerLink } from '../chain/deployment';
import { refreshActivity, releaseActivity, useActivity, type Activity } from '../chain/run';
import { useT } from '../i18n';
import { errorText } from './errors';
import { Banner, Button, Empty, Id, Section } from './ui';

function stateOf(a: Activity): string {
  if (a.status) return a.status.status;
  return a.entry.state;
}

function toneOf(state: string): string {
  if (state === 'failed' || state === 'rejected') return 'danger';
  if (state === 'unknown') return 'warn';
  if (state === 'pending' || state === 'accepted' || state === 'submitting' || state === 'skipped') return 'busy';
  return 'ok';
}

export function ActivityList({ filter, title }: { filter?: (a: Activity) => boolean; title?: string }) {
  const t = useT();
  const m = t.core.activity;
  const all = useActivity();
  const list = filter ? all.filter(filter) : all;
  return <Section title={title ?? m.title}>
    {list.length === 0 ? <Empty>{m.empty}</Empty> : <ul className="activity">{list.map((a) => <ActivityRow key={a.opId} a={a} />)}</ul>}
  </Section>;
}

function ActivityRow({ a }: { a: Activity }) {
  const t = useT();
  const m = t.core.activity;
  const [busy, setBusy] = useState(false);
  const [problem, setProblem] = useState<string | null>(null);
  const state = stateOf(a);
  const label = (m.states as Record<string, string>)[state] ?? state;
  const receipt = a.status?.receipt;
  const height = a.status?.block?.height;
  const failed = receipt && !receipt.success;
  const release = async () => {
    if (!window.confirm(m.releaseWarning)) return;
    setBusy(true);
    setProblem(null);
    try {
      await releaseActivity(a.opId);
    } catch (e) {
      setProblem(errorText(e, t));
    } finally {
      setBusy(false);
    }
  };
  return <li className="activity-row">
    <div className="activity-main">
      <span className={`pill pill--${toneOf(state)}`}>{label}</span>
      <span className="activity-title">{a.title}</span>
      <span className="muted">{m.kinds[a.entry.kind]}</span>
    </div>
    <div className="activity-meta">
      <Id value={a.opId} href={explorerLink('op', a.opId)} />
      {height !== undefined && height !== null && <span className="muted">#{height.toString()}</span>}
      {receipt && <span className="muted">{t.core.methods.gasUsed(receipt.gas_used.toLocaleString())}</span>}
      <Button size="sm" variant="ghost" disabled={busy} onClick={() => { setBusy(true); void refreshActivity(a.opId).finally(() => setBusy(false)); }}>{m.refresh}</Button>
      {a.entry.state === 'unknown' && !a.status && <Button size="sm" variant="danger" disabled={busy} onClick={() => void release()}>{m.release}</Button>}
    </div>
    {failed && <p className="activity-error">{t.core.methods.failed(failureText(receipt.error))}</p>}
    {a.entry.state === 'unknown' && !a.status && <p className="field-hint">{m.unknownNote}</p>}
    {a.missing && <p className="field-hint">{m.notFound}</p>}
    {a.error !== undefined && !failed && !a.missing && errorText(a.error, t) && <p className="field-hint">{errorText(a.error, t)}</p>}
    {problem && <Banner tone="danger">{problem}</Banner>}
  </li>;
}
