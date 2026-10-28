/**
 * The Code tab: the example's source in an editor, compiled by the compile service (when the copy has one) or built locally
 * and uploaded; the compiler's diagnostics in place and in a list; the ABI block checked as it is typed; a build ready to deploy.
 */
import { useEffect, useMemo, useRef, useState } from 'react';
import { AbiError, parseAbi } from '../abi/parse';
import { codeContext } from '../ai/reference';
import {
  compileSource, resetSource, setSource, uploadModule, useCompilerAvailable, useWorkspace,
} from '../app/workspace';
import type { Diagnostic } from '../compile/client';
import { Editor } from '../editor/Editor';
import type { ExampleDef } from '../examples';
import { useLocale, useT } from '../i18n';
import en from '../i18n/en';
import { errorText } from './errors';
import { Banner, Button, CopyButton, Section, copyText, saveFile } from './ui';

function lineOf(d: Diagnostic): number | null {
  return (d.spans.find((s) => s.primary) ?? d.spans[0])?.lineStart ?? null;
}

export function CodePanel({ def, onDeploy }: { def: ExampleDef; onDeploy: () => void }) {
  const t = useT();
  const m = t.core.code;
  const w = useWorkspace(def.id, def.source);
  const available = useCompilerAvailable();
  const [goto, setGoto] = useState<{ line: number; n: number } | null>(null);
  const file = useRef<HTMLInputElement>(null);
  const edited = w.source !== def.source;
  const abiState = useMemo(() => {
    try {
      const abi = parseAbi(w.source);
      return abi ? { ok: true as const, methods: abi.methods.length + (abi.init ? 1 : 0), events: abi.events.length } : { ok: false as const, missing: true, line: null, message: '' };
    } catch (e) {
      return { ok: false as const, missing: false, line: e instanceof AbiError ? e.line : null, message: e instanceof Error ? e.message : String(e) };
    }
  }, [w.source]);
  const errors = w.diagnostics.filter((d) => d.level === 'error');
  const warnings = w.diagnostics.filter((d) => d.level === 'warning');
  const build = w.build;
  const current = build !== null && (build.source === null || build.source === w.source);
  const crate = def.crate;
  const localCommand = `cd contracts && cargo rustc --release --locked --target wasm32-unknown-unknown -p ${crate} --crate-type cdylib`;
  const jump = (line: number) => setGoto({ line, n: (goto?.n ?? 0) + 1 });

  return <div className="code-layout">
    <Section className="code-editor" title={<>{m.source} <span className="mono muted">contracts/examples/{crate}/src/lib.rs</span></>}
      actions={<>
        <span className={`pill ${edited ? 'pill--warn' : 'pill--quiet'}`}>{edited ? m.edited : m.unchanged}</span>
        {edited && <Button size="sm" variant="ghost" onClick={() => { if (window.confirm(m.resetConfirm)) resetSource(def.id, def.source); }}>{m.reset}</Button>}
      </>}>
      <Editor value={w.source} onChange={(v) => setSource(def.id, def.source, v)} diagnostics={w.diagnostics} goto={goto} label={m.source} />
    </Section>
    <div className="code-side">
      <Section title={m.compile}>
        {available === false && <Banner tone="info">{m.noService}</Banner>}
        <div className="method-actions">
          <Button variant="primary" disabled={!available || w.compiling} busy={w.compiling} onClick={() => void compileSource(def.id, def.source, def.crate)}>
            {w.compiling ? m.compiling : m.compile}
          </Button>
          <Button onClick={() => file.current?.click()}>{m.upload}</Button>
          <input ref={file} type="file" accept=".wasm,application/wasm" hidden onChange={(e) => {
            const f = e.target.files?.[0];
            if (f) void uploadModule(def.id, def.source, f);
            e.target.value = '';
          }} />
        </div>
        {abiState.ok
          ? <p className="field-hint">{m.abiOk(abiState.methods, abiState.events)}</p>
          : abiState.missing
            ? <Banner tone="warn">{m.abiMissing}</Banner>
            : <Banner tone="warn">
              {m.abiError(abiState.message)}{' '}
              {abiState.line !== null && <button type="button" className="link-btn" onClick={() => jump(abiState.line!)}>{m.gotoLine(abiState.line)}</button>}
            </Banner>}
        {w.error !== null && w.error !== undefined && <Banner tone="danger">{errorText(w.error, t)}</Banner>}
        {w.diagnostics.length > 0 && <div className="diagnostics">
          <div className="diag-summary">
            {errors.length > 0 && <span className="pill pill--danger">{m.errors(errors.length)}</span>}
            {warnings.length > 0 && <span className="pill pill--warn">{m.warnings(warnings.length)}</span>}
          </div>
          <ul className="diag-list">{w.diagnostics.map((d, i) => {
            const line = lineOf(d);
            return <li key={i} className={`diag diag--${d.level}`}>
              <div className="diag-head">
                <span className="diag-level">{d.level}</span>
                {line !== null && <button type="button" className="link-btn" onClick={() => jump(line)}>{m.gotoLine(line)}</button>}
              </div>
              <pre className="diag-text">{d.rendered || d.message}</pre>
            </li>;
          })}</ul>
        </div>}
        {build && <div className="build">
          <Banner tone={current ? 'ok' : 'warn'} title={build.file ? m.uploaded(build.file) : m.compiled(build.wasm.length.toLocaleString(), build.ms.toLocaleString())}>
            {!current && m.stale}
          </Banner>
          <div className="method-actions">
            <Button variant="primary" disabled={!current} onClick={onDeploy}>{m.deployBuild}</Button>
            <Button onClick={() => saveFile(`${crate.replaceAll('-', '_')}.wasm`, new Uint8Array(build.wasm), 'application/wasm')}>{m.download}</Button>
          </div>
        </div>}
      </Section>
      <AskAi def={def} source={w.source} edited={edited} diagnostics={w.checked === w.source ? w.diagnostics : []} stale={w.diagnostics.length > 0 && w.checked !== w.source} />
      <Section title={m.localBuild}>
        <p className="field-hint">{m.howto}</p>
        <div className="command"><code>{localCommand}</code><CopyButton text={localCommand} /></div>
      </Section>
    </div>
  </div>;
}

/** The contract with the reference (or without it, for a conversation that has it), copied for an AI assistant */
function AskAi({ def, source, edited, diagnostics, stale }: { def: ExampleDef; source: string; edited: boolean; diagnostics: Diagnostic[]; stale: boolean }) {
  const m = useT().core.code;
  const locale = useLocale();
  const [copied, setCopied] = useState<'full' | 'code' | null>(null);
  /** Whether the copy that failed had the reference */
  const [failed, setFailed] = useState<boolean | null>(null);
  useEffect(() => {
    if (!copied) return;
    const timer = setTimeout(() => setCopied(null), 2_000);
    return () => clearTimeout(timer);
  }, [copied]);
  const text = (withReference: boolean) =>
    codeContext({ page: en.core.examples[def.id].title, route: `#/${def.id}/code`, crate: def.crate, source, edited, diagnostics, locale, withReference });
  const copy = (withReference: boolean) => {
    void copyText(text(withReference)).then((ok) => {
      setCopied(ok ? (withReference ? 'full' : 'code') : null);
      setFailed(ok ? null : withReference);
    });
  };
  return <Section title={m.ai}>
    <p className="field-hint">{m.aiHint}</p>
    <div className="method-actions">
      <Button onClick={() => copy(true)}>{copied === 'full' ? m.aiCopied : m.aiCopy}</Button>
      <Button variant="ghost" onClick={() => copy(false)}>{copied === 'code' ? m.aiCopied : m.aiCopyCode}</Button>
    </div>
    {stale && <p className="field-hint">{m.aiStale}</p>}
    {failed !== null && <Banner tone="warn">
      {m.aiFailed}{' '}
      <button type="button" className="link-btn" onClick={() => saveFile(`${def.crate}-for-ai.md`, text(failed), 'text/markdown')}>{m.aiDownload}</button>
    </Banner>}
    <p className="field-hint"><a href="#/ai">{m.aiAbout}</a></p>
  </Section>;
}
