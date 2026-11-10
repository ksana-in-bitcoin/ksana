/**
 * The first page's workbench: an example's contract, readable at once. Its tabs pick the example; init's arguments are a form
 * started from the example's presets, and the deployment they make is written out as they change; the methods of the ABI jump
 * to where the source handles them; one button opens the example's deploy dialog with the arguments as they stand.
 */
import { useEffect, useMemo, useState } from 'react';
import { fromHex } from '@ksana/sdk';
import { encodeArgs } from '../abi/codec';
import { emptyDraft, formatValue, valueOf, type Draft, type TextContext } from '../abi/text';
import { typeName, type Abi, type Field, type MethodDef, type Value } from '../abi/types';
import { useConnection } from '../chain/connection';
import { useWallet } from '../chain/wallet';
import { CodeView } from '../editor/Editor';
import { EXAMPLES, exampleAbi, type ExampleDef, type ExampleId } from '../examples';
import { codeStart, methodLine } from '../examples/outline';
import { useT } from '../i18n';
import { initialDrafts } from '../ui/Deploy';
import { Button, CopyButton } from '../ui/ui';
import { ValueEditor } from '../ui/values';
import { requestDeploy } from './deploys';
import { createStore, useStore } from './store';

/**
 * The example the workbench shows: kept while the page is open, so coming back to the first page finds it as it was left. It
 * opens on the asset, the simplest example that has settings to change.
 */
const pickedStore = createStore<ExampleId>('token');

function kib(bytes: number): string {
  return `${(bytes / 1024).toFixed(1)} KiB`;
}

/** Each argument's value, undefined while its field does not hold one yet */
function argValues(abi: Abi, params: Field[], drafts: Draft[], ctx: TextContext): (Value | undefined)[] {
  return params.map((p, i) => {
    try {
      return valueOf(abi, p.type, drafts[i] ?? emptyDraft(abi, p.type), ctx, p.name);
    } catch {
      return undefined;
    }
  });
}

function signature(m: MethodDef): string {
  return `${m.name}(${m.params.map((p) => `${p.name}: ${typeName(p.type)}`).join(', ')})${m.returns ? ` -> ${typeName(m.returns)}` : ''}`;
}

export function Workbench() {
  const m = useT().core;
  const picked = useStore(pickedStore);
  const def = EXAMPLES.find((e) => e.id === picked) ?? EXAMPLES[0]!;
  const onKey = (e: React.KeyboardEvent<HTMLDivElement>) => {
    const step = e.key === 'ArrowRight' ? 1 : e.key === 'ArrowLeft' ? -1 : 0;
    if (step === 0) return;
    e.preventDefault();
    const next = EXAMPLES[(EXAMPLES.indexOf(def) + step + EXAMPLES.length) % EXAMPLES.length]!;
    pickedStore.set(next.id);
    e.currentTarget.querySelector<HTMLElement>(`[data-example="${next.id}"]`)?.focus();
  };
  return <section className="bench" aria-label={m.home.benchLabel}>
    <div className="bench-tabs" role="tablist" aria-label={m.nav.examples} onKeyDown={onKey}>
      {EXAMPLES.map((e) => <button key={e.id} type="button" role="tab" id={`bench-tab-${e.id}`} data-example={e.id} aria-selected={e.id === def.id}
        aria-controls="bench-panel" tabIndex={e.id === def.id ? 0 : -1} className={`bench-tab${e.id === def.id ? ' is-active' : ''}`}
        onClick={() => pickedStore.set(e.id)}>
        {m.examples[e.id].title}
      </button>)}
    </div>
    <Bench key={def.id} def={def} />
  </section>;
}

function Bench({ def }: { def: ExampleDef }) {
  const t = useT();
  const m = t.core;
  const h = m.home;
  const conn = useConnection();
  const wallet = useWallet();
  const abi = useMemo(() => exampleAbi(def), [def]);
  const params = abi?.init?.params ?? [];
  const me = wallet?.account.account ?? null;
  const network = conn?.network ?? null;
  const [drafts, setDrafts] = useState<Draft[]>(() => initialDrafts(def, abi, me ? fromHex(me, 32) : null, network, null));
  // a wallet or a node that arrives later starts the form again: the presets name the wallet's account
  useEffect(() => {
    setDrafts(initialDrafts(def, abi, me ? fromHex(me, 32) : null, network, null));
  }, [def, abi, me, network]);
  const [jump, setJump] = useState<{ name: string; line: number; n: number } | null>(null);

  const values = useMemo(() => (abi ? argValues(abi, params, drafts, { network: network ?? undefined }) : []), [abi, params, drafts, network]);
  const ready = values.every((v) => v !== undefined);
  const bytes = useMemo(() => {
    if (!abi || !ready) return null;
    try {
      return encodeArgs(abi, params.map((p) => p.type), values as Value[]).length;
    } catch {
      return null;
    }
  }, [abi, params, values, ready]);
  const deploy = () => {
    // an argument that holds no value yet is left out, and the dialog starts it empty (the request is exact)
    const presets: Record<string, Value> = {};
    params.forEach((p, i) => {
      const v = values[i];
      if (v !== undefined) presets[p.name] = v;
    });
    requestDeploy(def.id, presets, true);
  };
  const methods = abi ? [...(abi.init ? [abi.init] : []), ...abi.methods] : [];
  const lines = useMemo(() => def.source.split('\n').length, [def]);
  const start = useMemo(() => codeStart(def.source), [def]);

  return <div className="bench-body" role="tabpanel" id="bench-panel" aria-labelledby={`bench-tab-${def.id}`}>
    <div className="bench-side">
      <div className="bench-scroll">
        <div className="bench-about">
          <h2>{m.examples[def.id].title}</h2>
          <p>{m.examples[def.id].summary}</p>
          {def.uses && <p className="field-hint">{m.instance.builds(def.uses.map((u) => m.examples[u].title).join(', '))}</p>}
        </div>
        <h3 className="bench-label">{h.settings}</h3>
        {abi && params.length > 0
          ? <div className="bench-form">
            {params.map((p, i) => <ValueEditor key={p.name} abi={abi} type={p.type} label={p.name} draft={drafts[i] ?? emptyDraft(abi, p.type)}
              onChange={(d) => setDrafts(drafts.map((x, j) => (j === i ? d : x)))} />)}
          </div>
          : <p className="field-hint">{h.noSettings}</p>}
      </div>
      <div className="bench-foot">
        <h3 className="bench-label">
          {h.preview}
          {params.length > 0 && <span className={bytes === null ? 'bench-missing' : ''}>{bytes === null ? h.argsMissing : h.argsBytes(bytes.toLocaleString())}</span>}
        </h3>
        <pre className="bench-call mono" aria-live="polite">
          <span className="bench-call-line"><b>deploy</b> {def.crate}.wasm{def.build && <i> · {kib(def.build.bytes)}</i>}</span>
          <span className="bench-call-line"><b>init</b>({params.length === 0 && ')'}</span>
          {abi && params.map((p, i) => {
            const v = values[i];
            const text = v === undefined ? null : formatValue(abi, p.type, v, { network: network ?? undefined });
            return <span key={`${p.name}=${text}`} className="bench-call-line bench-call-arg">
              {'  '}{p.name}: {text === null ? <em>{typeName(p.type)}</em> : text},
            </span>;
          })}
          {params.length > 0 && <span className="bench-call-line">)</span>}
        </pre>
        <div className="bench-actions">
          <Button variant="primary" onClick={deploy}>{conn?.chain === 'devnet' ? h.deployDevnet : h.deploy}</Button>
          <a className="btn btn--secondary btn--md" href={`#/${def.id}`}>{h.openExample}</a>
        </div>
      </div>
    </div>
    <div className="bench-code">
      <div className="bench-code-head">
        <span className="bench-file mono" title={`contracts/examples/${def.crate}/src/lib.rs`}>{def.crate}/src/lib.rs<i> · {h.lines(lines.toLocaleString())}</i></span>
        <span className="bench-code-actions">
          <CopyButton text={def.source} label={h.copy} />
          <a className="btn btn--secondary btn--sm" href={`#/${def.id}/code`}>{h.edit}</a>
        </span>
      </div>
      {methods.length > 0 && <div className="bench-jumps" role="group" aria-label={h.jump}>
        {methods.map((x) => <button key={x.name} type="button" title={`${signature(x)}${x.doc ? `\n${x.doc}` : ''}`}
          className={`bench-jump bench-jump--${x.kind}${jump?.name === x.name ? ' is-on' : ''}`} aria-pressed={jump?.name === x.name}
          onClick={() => setJump({ name: x.name, line: methodLine(def.source, x.name) ?? x.line, n: (jump?.n ?? 0) + 1 })}>
          {x.name}
        </button>)}
      </div>}
      <CodeView value={def.source} start={start} line={jump} label={h.source(m.examples[def.id].title)} />
    </div>
  </div>;
}
