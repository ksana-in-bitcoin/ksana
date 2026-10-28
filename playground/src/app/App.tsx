/** The page: the header (examples, node, wallet, settings), the list of examples, an example's tabs, and the footer. */
import { useEffect, useMemo, useState } from 'react';
import { parseAbi } from '../abi/parse';
import type { Abi, Value } from '../abi/types';
import { AiPage } from '../ai/AiPage';
import { connect, useConnection, useConnectionState, useTip } from '../chain/connection';
import { endpoints } from '../chain/deployment';
import { EXAMPLES, exampleAbi, exampleById, type ExampleDef } from '../examples';
import { identify } from '../examples/identify';
import { LOCALES, setLocale, useLocale, useT } from '../i18n';
import { ActivityList } from '../ui/Activity';
import { CodePanel } from '../ui/Code';
import { DeployDialog } from '../ui/Deploy';
import { MethodsPanel } from '../ui/Methods';
import { ReviewDialog } from '../ui/Review';
import { SettingsDialog } from '../ui/Settings';
import { Toasts } from '../ui/Toasts';
import { Banner, Button, Empty, Field, Modal, Section, Spinner, Tabs, copyText } from '../ui/ui';
import { WalletButton } from '../ui/Wallet';
import { deployRequestStore } from './deploys';
import { addInstance, forgetInstance, selectInstance, useInstances, type Instance } from './instances';
import { TABS, navigate, shareLink, useRoute, type Tab } from './route';
import { useStore } from './store';

const REPO = 'https://github.com/ksana-in-bitcoin/Ksana';

function docsLink(locale: string): string {
  return locale === 'zh-CN' ? 'https://ksana.network/zh/docs/playground/' : 'https://ksana.network/docs/playground/';
}

export function App() {
  const route = useRoute();
  const def = exampleById(route.example);
  const ai = route.example === 'ai';
  useEffect(() => {
    const t = def ? `${def.id}` : ai ? 'AI assistants' : '';
    document.title = t ? `${t} · Ksana Playground` : 'Ksana Playground';
  }, [def, ai]);
  return <>
    <Header />
    <main className="page" id="main">
      {route.example === 'open' ? <OpenPage contract={route.contract} /> : ai ? <AiPage /> : def ? <ExamplePage key={def.id} def={def} tab={route.tab} contract={route.contract} /> : <Home />}
    </main>
    <Footer />
    <ReviewDialog />
    <Toasts />
  </>;
}

function Header() {
  const t = useT();
  const m = t.core;
  const route = useRoute();
  const locale = useLocale();
  const [settings, setSettings] = useState(false);
  const [menu, setMenu] = useState(false);
  const other = LOCALES.find((l) => l.code !== locale) ?? LOCALES[0]!;
  useEffect(() => setMenu(false), [route.example]);
  return <header className="topbar">
    <div className="topbar-inner">
      <a className="brand" href="#/" aria-label={m.title}>
        <svg className="brand-mark" viewBox="0 0 32 32" aria-hidden="true"><rect x="3" y="3" width="26" height="26" rx="3" /><path d="M11 9v14M11 16l9-7M14 14l7 9" /></svg>
        <span className="brand-name">Ksana <b>{m.brand}</b></span>
      </a>
      <nav className={`nav${menu ? ' is-open' : ''}`} aria-label={m.nav.examples}>
        {EXAMPLES.map((e) => <a key={e.id} href={`#/${e.id}`} className={route.example === e.id ? 'is-active' : ''} aria-current={route.example === e.id ? 'page' : undefined}>
          {m.examples[e.id].title}
        </a>)}
        <a href="#/ai" className={`nav-ai${route.example === 'ai' ? ' is-active' : ''}`} aria-current={route.example === 'ai' ? 'page' : undefined}>{m.nav.ai}</a>
      </nav>
      <div className="topbar-tools">
        <NetworkPill onSettings={() => setSettings(true)} />
        <WalletButton />
        <button type="button" className="icon-btn lang-btn" onClick={() => setLocale(other.code)} aria-label={m.nav.language} title={other.name}>
          {other.mark}
        </button>
        <button type="button" className="icon-btn" onClick={() => setSettings(true)} aria-label={m.nav.settings} title={m.nav.settings}>
          <svg viewBox="0 0 24 24" aria-hidden="true"><path d="M12 15.5a3.5 3.5 0 1 0 0-7 3.5 3.5 0 0 0 0 7Z" /><path d="M19.4 15a1.7 1.7 0 0 0 .3 1.8l.1.1a2 2 0 1 1-2.8 2.8l-.1-.1a1.7 1.7 0 0 0-1.8-.3 1.7 1.7 0 0 0-1 1.5V21a2 2 0 1 1-4 0v-.1a1.7 1.7 0 0 0-1.1-1.5 1.7 1.7 0 0 0-1.8.3l-.1.1a2 2 0 1 1-2.8-2.8l.1-.1a1.7 1.7 0 0 0 .3-1.8 1.7 1.7 0 0 0-1.5-1H3a2 2 0 1 1 0-4h.1a1.7 1.7 0 0 0 1.5-1.1 1.7 1.7 0 0 0-.3-1.8l-.1-.1a2 2 0 1 1 2.8-2.8l.1.1a1.7 1.7 0 0 0 1.8.3H9a1.7 1.7 0 0 0 1-1.5V3a2 2 0 1 1 4 0v.1a1.7 1.7 0 0 0 1 1.5 1.7 1.7 0 0 0 1.8-.3l.1-.1a2 2 0 1 1 2.8 2.8l-.1.1a1.7 1.7 0 0 0-.3 1.8V9a1.7 1.7 0 0 0 1.5 1H21a2 2 0 1 1 0 4h-.1a1.7 1.7 0 0 0-1.5 1Z" /></svg>
        </button>
        <button type="button" className="icon-btn menu-btn" aria-expanded={menu} onClick={() => setMenu(!menu)} aria-label={m.nav.menu}>
          <svg viewBox="0 0 24 24" aria-hidden="true"><path d="M4 7h16M4 12h16M4 17h16" /></svg>
        </button>
      </div>
    </div>
    <SettingsDialog open={settings} onClose={() => setSettings(false)} />
  </header>;
}

function NetworkPill({ onSettings }: { onSettings: () => void }) {
  const m = useT().core.network;
  const state = useConnectionState();
  const tip = useTip();
  if (state.state === 'none') return <button type="button" className="net net--warn" onClick={onSettings}>{m.none}</button>;
  if (state.state === 'connecting') return <span className="net"><Spinner /> {m.connecting}</span>;
  if (state.state === 'error') return <button type="button" className="net net--danger" onClick={() => void connect()} title={state.error}>{m.error} · {m.retry}</button>;
  const c = state.connection;
  return <span className="net net--ok" title={c.url}>
    <span className="net-dot" aria-hidden="true" />
    {c.chain === 'signet' ? m.signet : m.devnet}
    {tip && <span className="net-height mono">{m.block(tip.tip.height.toLocaleString())}</span>}
  </span>;
}

function Footer() {
  const t = useT();
  const m = t.core;
  const locale = useLocale();
  const explorer = endpoints().explorer;
  return <footer className="footer">
    <div className="footer-inner">
      <p className="footer-note">{m.footer.testnet}</p>
      <nav className="footer-links">
        <a href={locale === 'zh-CN' ? 'https://ksana.network/zh/' : 'https://ksana.network/'}>{m.nav.home}</a>
        <a href={docsLink(locale)}>{m.nav.docs}</a>
        {explorer && <a href={explorer}>{m.nav.explorer}</a>}
        <a href={`${REPO}/tree/main/playground`}>{m.nav.source}</a>
        {__KSANA_COMMIT__ && <a className="mono" href={`${REPO}/commit/${__KSANA_COMMIT__}`}>{m.footer.built(__KSANA_COMMIT__)}</a>}
      </nav>
    </div>
  </footer>;
}

function Home() {
  const t = useT();
  const m = t.core;
  const steps = ['wallet', 'faucet', 'deploy', 'edit'] as const;
  return <div className="home">
    <section className="hero">
      <h1>{m.title}</h1>
      <p className="lead">{m.home.lead}</p>
      <div className="hero-actions">
        <a className="btn btn--primary btn--md" href="#/counter">{m.home.start}</a>
        <a className="btn btn--secondary btn--md" href="#/ai">{m.home.ai}</a>
        <a className="btn btn--ghost btn--md" href={docsLink(useLocale())}>{m.nav.docs}</a>
      </div>
    </section>
    <Section title={m.home.examplesTitle}>
      <ul className="cards">
        {EXAMPLES.map((e) => <li key={e.id}>
          <a className="card" href={`#/${e.id}`}>
            <span className="card-title">{m.examples[e.id].title}</span>
            <span className="card-body">{m.examples[e.id].summary}</span>
            <span className="card-meta mono">{e.crate}{e.build ? ` · ${(e.build.bytes / 1024).toFixed(1)} KiB` : ''}</span>
            {e.uses && <span className="card-meta">{m.home.uses}: {e.uses.map((u) => m.examples[u].title).join(', ')}</span>}
          </a>
        </li>)}
        <li>
          <a className="card card--plain" href="#/open">
            <span className="card-title">{m.home.contract}</span>
            <span className="card-body">{m.home.contractBody}</span>
          </a>
        </li>
      </ul>
    </Section>
    <Section title={m.home.stepsTitle}>
      <ol className="steps">
        {steps.map((s, i) => <li key={s}><span className="step-n">{i + 1}</span><div><h3>{m.home.steps[s].title}</h3><p>{m.home.steps[s].body}</p></div></li>)}
      </ol>
    </Section>
  </div>;
}

function instanceAbi(def: ExampleDef, instance: Instance | null): { abi: Abi | null; custom: boolean } {
  if (instance?.abi) {
    try {
      return { abi: parseAbi(instance.abi), custom: true };
    } catch {
      return { abi: null, custom: true };
    }
  }
  return { abi: exampleAbi(def), custom: false };
}

function ExamplePage({ def, tab, contract: asked }: { def: ExampleDef; tab: Tab; contract: string | null }) {
  const t = useT();
  const m = t.core;
  const conn = useConnection();
  const { list, current } = useInstances(conn, def.id);
  const [deploying, setDeploying] = useState(false);
  const [presets, setPresets] = useState<Record<string, Value> | null>(null);
  const [opening, setOpening] = useState(false);

  // another page asked for a deployment of this example with some arguments filled in
  const request = useStore(deployRequestStore);
  useEffect(() => {
    if (!request || request.example !== def.id) return;
    deployRequestStore.set(null);
    setPresets(request.presets);
    setDeploying(true);
  }, [request, def.id]);
  const deployNew = () => {
    setPresets(null);
    setDeploying(true);
  };

  // a link naming a contract adds it to the list (once the node's network is known)
  const known = list.map((i) => i.id).join();
  const currentId = current?.id ?? null;
  useEffect(() => {
    if (!asked || !conn) return;
    if (!known.split(',').includes(asked)) addInstance(conn, def.id, { id: asked, origin: 'opened', at: Date.now() });
    else if (currentId !== asked) selectInstance(conn, def.id, asked);
  }, [asked, conn, def.id, known, currentId]);

  const contract = asked ?? current?.id ?? null;
  const instance = list.find((i) => i.id === contract) ?? null;
  const { abi, custom } = useMemo(() => instanceAbi(def, instance), [def, instance]);
  const App = def.App;
  const choose = (id: string) => {
    if (conn) selectInstance(conn, def.id, id);
    navigate({ contract: id }, true);
  };
  return <div className="example">
    <div className="example-head">
      <div>
        <h1>{m.examples[def.id].title}</h1>
        <p className="lead">{m.examples[def.id].summary}</p>
        {def.uses && <p className="field-hint">{m.instance.builds(def.uses.map((u) => m.examples[u].title).join(', '))}</p>}
      </div>
    </div>
    <InstanceBar def={def} list={list} contract={contract} onChoose={choose} onDeploy={deployNew} onOpen={() => setOpening(true)} />
    <Tabs<Tab> value={tab} label={m.title} onChange={(v) => navigate({ tab: v }, true)}
      options={TABS.map((v) => ({ value: v, label: m.tabs[v] }))} />
    <div className="tab-body">
      {tab === 'app' && (!conn ? <NoNode /> : contract && abi
        ? <>
          {custom && <Banner tone="warn">{m.instance.customAbi}</Banner>}
          <App key={contract} contract={contract} abi={abi} />
        </>
        : <NoContract onDeploy={deployNew} onOpen={() => setOpening(true)} />)}
      {tab === 'code' && <CodePanel def={def} onDeploy={deployNew} />}
      {tab === 'methods' && (!conn ? <NoNode /> : contract ? <MethodsPanel contract={contract} abi={abi} />
        : <NoContract onDeploy={deployNew} onOpen={() => setOpening(true)} />)}
      {tab === 'activity' && <ActivityList />}
    </div>
    {deploying && <DeployDialog def={def} open={deploying} presets={presets} onClose={() => setDeploying(false)} />}
    <OpenDialog def={def} open={opening} onClose={() => setOpening(false)} />
  </div>;
}

function NoNode() {
  const m = useT().core;
  const state = useConnectionState();
  return <Section>
    {state.state === 'error' ? <Banner tone="danger" title={m.network.error}>{state.error}</Banner> : state.state === 'connecting' ? <p><Spinner /> {m.network.connecting}</p> : <Banner tone="warn">{m.network.noNode}</Banner>}
  </Section>;
}

function NoContract({ onDeploy, onOpen }: { onDeploy: () => void; onOpen: () => void }) {
  const m = useT().core.instance;
  return <Section className="empty-state">
    <p>{m.needDeploy}</p>
    <div className="method-actions">
      <Button variant="primary" onClick={onDeploy}>{m.deployNew}</Button>
      <Button onClick={onOpen}>{m.open}</Button>
    </div>
  </Section>;
}

function InstanceBar({ def, list, contract, onChoose, onDeploy, onOpen }: {
  def: ExampleDef; list: Instance[]; contract: string | null; onChoose: (id: string) => void; onDeploy: () => void; onOpen: () => void;
}) {
  const t = useT();
  const m = t.core.instance;
  const conn = useConnection();
  const [copied, setCopied] = useState(false);
  useEffect(() => {
    if (!copied) return;
    const timer = setTimeout(() => setCopied(false), 1500);
    return () => clearTimeout(timer);
  }, [copied]);
  const label = (i: Instance) => `${i.id.slice(0, 8)}…${i.id.slice(-6)} · ${i.origin === 'shared' ? m.shared_badge : i.origin === 'mine' ? m.mine : m.open}`;
  const selected = list.find((i) => i.id === contract);
  return <div className="instance-bar">
    <label className="instance-select">
      <span className="field-label">{m.label}</span>
      <select className="input mono" value={contract ?? ''} disabled={!conn || list.length === 0} onChange={(e) => onChoose(e.target.value)} aria-label={m.pick}>
        {list.length === 0 && <option value="">{m.none}</option>}
        {list.map((i) => <option key={i.id} value={i.id}>{label(i)}</option>)}
      </select>
    </label>
    <div className="instance-actions">
      <Button size="sm" variant="primary" disabled={!conn} onClick={onDeploy}>{m.deployNew}</Button>
      <Button size="sm" disabled={!conn} onClick={onOpen}>{m.open}</Button>
      {contract && <Button size="sm" variant="ghost" onClick={() => void copyText(shareLink(def.id, contract)).then(setCopied)}>{copied ? m.shared : m.share}</Button>}
      {selected && selected.origin !== 'shared' && conn && <Button size="sm" variant="ghost" onClick={() => forgetInstance(conn, def.id, selected.id)}>{m.forget}</Button>}
    </div>
  </div>;
}

function OpenDialog({ def, open, onClose }: { def: ExampleDef; open: boolean; onClose: () => void }) {
  const m = useT().core.instance;
  const conn = useConnection();
  const [id, setId] = useState('');
  const valid = /^[0-9a-f]{64}$/.test(id.trim().toLowerCase());
  return <Modal open={open} title={m.openTitle} onClose={onClose}
    footer={<Button variant="primary" disabled={!valid || !conn} onClick={() => {
      const c = id.trim().toLowerCase();
      if (conn) addInstance(conn, def.id, { id: c, origin: 'opened', at: Date.now() });
      navigate({ contract: c, tab: 'app' }, true);
      setId('');
      onClose();
    }}>{m.openButton}</Button>}>
    <Field label={m.label} htmlFor="open-id" hint={m.openHint}>
      <input id="open-id" className="input mono" spellCheck={false} value={id} onChange={(e) => setId(e.target.value)} placeholder="0123…cdef" />
    </Field>
  </Modal>;
}

/** `#/open?c=<id>`: a contract from the explorer; an example's opens on its page, any other gets forms from an ABI pasted here */
function OpenPage({ contract }: { contract: string | null }) {
  const t = useT();
  const m = t.core.open;
  const conn = useConnection();
  const [id, setId] = useState(contract ?? '');
  const [state, setState] = useState<'idle' | 'looking' | 'unknown'>('idle');
  const [source, setSource] = useState('');
  const [abi, setAbi] = useState<Abi | null>(null);
  const [problem, setProblem] = useState<string | null>(null);
  useEffect(() => {
    if (!conn || !contract) return;
    let alive = true;
    setState('looking');
    void identify(conn, contract).then((found) => {
      if (!alive) return;
      if (found) navigate({ example: found, tab: 'app', contract }, true);
      else setState('unknown');
    });
    return () => { alive = false; };
  }, [conn, contract]);
  const valid = /^[0-9a-f]{64}$/.test(id.trim().toLowerCase());
  return <div className="example">
    <div className="example-head"><div><h1>{m.title}</h1></div></div>
    {!conn && <Banner tone="warn">{m.noNode}</Banner>}
    <Section>
      <form className="open-form" onSubmit={(e) => { e.preventDefault(); if (valid) navigate({ example: 'open', contract: id.trim().toLowerCase() }, true); }}>
        <Field label={m.id} htmlFor="open-page-id">
          <input id="open-page-id" className="input mono" spellCheck={false} value={id} onChange={(e) => setId(e.target.value)} placeholder="0123…cdef" />
        </Field>
        <Button variant="primary" type="submit" disabled={!valid || !conn}>{t.core.instance.openButton}</Button>
      </form>
      {state === 'looking' && <p className="muted"><Spinner /> {m.identifying}</p>}
    </Section>
    {state === 'unknown' && contract && <>
      <Section>
        <p>{m.unknown}</p>
        <Field label={m.abi} htmlFor="open-abi">
          <textarea id="open-abi" className="input mono" rows={8} spellCheck={false} value={source} onChange={(e) => setSource(e.target.value)} placeholder={'//! ```abi\n//! view get() -> u64\n//! ```'} />
        </Field>
        <Button onClick={() => {
          try {
            const text = source.includes('```abi') ? source : ['//! ```abi', ...source.split(/\r?\n/).map((l) => `//! ${l}`), '//! ```'].join('\n');
            const parsed = parseAbi(text);
            setAbi(parsed);
            setProblem(parsed ? null : t.core.code.abiMissing);
          } catch (e) {
            setAbi(null);
            setProblem(t.core.code.abiError(e instanceof Error ? e.message : String(e)));
          }
        }}>{m.apply}</Button>
        {problem && <Banner tone="warn">{problem}</Banner>}
      </Section>
      {abi && <MethodsPanel contract={contract} abi={abi} />}
    </>}
    {state === 'unknown' && !contract && <Empty>{m.unknown}</Empty>}
  </div>;
}
