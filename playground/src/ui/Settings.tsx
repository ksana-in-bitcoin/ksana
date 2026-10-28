/** The settings: the endpoints this browser uses instead of the hosted ones, the language and the theme. */
import { useState } from 'react';
import { setTheme, useTheme, type Theme } from '../app/theme';
import { connect } from '../chain/connection';
import { currentDeployment, endpoints, saveEndpoints, type Endpoints } from '../chain/deployment';
import { LOCALES, setLocale, useLocale, useT } from '../i18n';
import { Button, Field, Modal } from './ui';

const KEYS = ['rpc', 'compile', 'faucet', 'explorer'] as const;

export function SettingsDialog({ open, onClose }: { open: boolean; onClose: () => void }) {
  const t = useT();
  const m = t.core.settings;
  const locale = useLocale();
  const theme = useTheme();
  const [draft, setDraft] = useState<Partial<Endpoints> | null>(null);
  const current = endpoints();
  const deployment = currentDeployment();
  const defaults: Endpoints = {
    rpc: deployment?.rpc ?? '', compile: deployment?.compile ?? '', faucet: deployment?.faucet ?? '', explorer: deployment?.explorer ?? '',
  };
  const values = draft ?? Object.fromEntries(KEYS.map((k) => [k, current[k] === defaults[k] ? '' : current[k]])) as Partial<Endpoints>;
  const close = () => {
    setDraft(null);
    onClose();
  };
  const save = () => {
    saveEndpoints(values);
    setDraft(null);
    onClose();
    void connect();
  };
  const reset = () => {
    saveEndpoints({});
    setDraft(null);
    onClose();
    void connect();
  };
  return <Modal open={open} title={m.title} onClose={close}
    footer={<><Button variant="ghost" onClick={reset}>{m.reset}</Button><Button variant="primary" onClick={save}>{m.save}</Button></>}>
    <div className="settings-grid">
      <Field label={m.language} htmlFor="set-lang">
        <select id="set-lang" className="input" value={locale} onChange={(e) => setLocale(e.target.value as typeof locale)}>
          {LOCALES.map((l) => <option key={l.code} value={l.code}>{l.name}</option>)}
        </select>
      </Field>
      <Field label={m.theme} htmlFor="set-theme">
        <select id="set-theme" className="input" value={theme} onChange={(e) => setTheme(e.target.value as Theme)}>
          {(['system', 'light', 'dark'] as const).map((v) => <option key={v} value={v}>{m.themes[v]}</option>)}
        </select>
      </Field>
    </div>
    {KEYS.map((k) => <Field key={k} label={m[k]} htmlFor={`set-${k}`}>
      <input id={`set-${k}`} className="input mono" spellCheck={false} autoCapitalize="off" value={values[k] ?? ''} placeholder={defaults[k] || '—'}
        onChange={(e) => setDraft({ ...values, [k]: e.target.value })} />
    </Field>)}
    <p className="field-hint">{m.hint}</p>
  </Modal>;
}
