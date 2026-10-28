/**
 * Deploying an example: the published module or the person's own build from the Code tab, init's arguments in a form
 * generated from the ABI (started from the example's presets, or from the arguments another page asked for), then the review.
 * Asset IDs among the arguments must name built-in assets, and the example may check more (the AMM: a token side must be a
 * token contract), before anything is sent. Once a block includes the deployment, the new contract joins the example's list and opens.
 */
import { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { fromHex, type BtcNetwork, type Funds } from '@ksana/sdk';
import { encodeArgs } from '../abi/codec';
import { parseAbi } from '../abi/parse';
import { InputError, draftOf, emptyDraft, formatValue, valueOf, type Draft } from '../abi/text';
import type { Abi, Field, Value } from '../abi/types';
import { addInstance } from '../app/instances';
import { navigate } from '../app/route';
import { useWorkspace, type Build } from '../app/workspace';
import { assetMeta } from '../chain/assets';
import { useConnection, type Connection } from '../chain/connection';
import { writeKnowingContracts } from '../chain/contracts';
import { assetArgProblems, assetInstead, type InitProblem } from '../chain/ids';
import { useWallet } from '../chain/wallet';
import { exampleAbi, exampleWasm, type ExampleDef } from '../examples';
import { useSend } from '../examples/kit';
import { useT } from '../i18n';
import { errorText, idProblemText } from './errors';
import { Banner, Button, Modal } from './ui';
import { FundsEditor, ValueEditor, fundsOf, type FundsDraft } from './values';

type Choice = 'example' | 'build';

function initialDrafts(def: ExampleDef, abi: Abi | null, me: Uint8Array | null, network: BtcNetwork | null, asked: Record<string, Value> | null): Draft[] {
  const params = abi?.init?.params ?? [];
  const presets = { ...def.presets?.({ me, network }), ...asked };
  return params.map((p) => (p.name in presets ? draftOf(abi!, p.type, presets[p.name]!) : emptyDraft(abi!, p.type)));
}

/** The values the drafts stand for; null while one is not a value yet (deploying shows that field's error) */
function valuesOf(abi: Abi | null, params: Field[], drafts: Draft[], network: BtcNetwork | undefined): Value[] | null {
  if (!abi) return null;
  try {
    return params.map((p, i) => valueOf(abi, p.type, drafts[i] ?? emptyDraft(abi, p.type), { network }, p.name));
  } catch {
    return null;
  }
}

function valueKey(values: Value[] | null): string {
  return values === null ? '' : JSON.stringify(values, (_k, v) => (typeof v === 'bigint' ? `${v}n` : v instanceof Uint8Array ? Array.from(v).join(',') : v));
}

/** Asset IDs that name no asset, and what the example itself finds wrong with the arguments */
async function initProblems(conn: Connection, def: ExampleDef, abi: Abi, params: Field[], values: Value[]): Promise<InitProblem[]> {
  const own = def.check ? await def.check(conn, abi, params, values) : [];
  return [...await assetArgProblems(conn, abi, params, values), ...own];
}

/** The problems of the arguments as they stand, checked a moment after they change; `run` checks them at once */
function useInitCheck(conn: Connection | null, def: ExampleDef, abi: Abi | null, params: Field[], values: Value[] | null) {
  const key = valueKey(values);
  const latest = useRef(key);
  latest.current = key;
  const [state, setState] = useState<{ key: string; problems: InitProblem[] } | null>(null);
  const run = useCallback(async (): Promise<InitProblem[]> => {
    if (!conn || !abi || !values) return [];
    const problems = await initProblems(conn, def, abi, params, values);
    if (latest.current === key) setState({ key, problems });
    return problems;
  }, [conn, def, abi, key]); // params and values change with key
  useEffect(() => {
    const timer = setTimeout(() => { run().catch(() => { /* the node did not answer: the review finds out */ }); }, 300);
    return () => clearTimeout(timer);
  }, [run]);
  const settled = state !== null && state.key === key;
  return { problems: settled ? state.problems : [], settled, run };
}

export function DeployDialog({ def, open, onClose, presets = null }: {
  def: ExampleDef; open: boolean; onClose: () => void; presets?: Record<string, Value> | null;
}) {
  const t = useT();
  const m = t.core.deploy;
  const name = t.core.examples[def.id].title;
  const conn = useConnection();
  const wallet = useWallet();
  const workspace = useWorkspace(def.id, def.source);
  const build = workspace.build;
  const [choice, setChoice] = useState<Choice>('example');
  const using: Build | null = choice === 'build' ? build : null;
  const abi = useMemo<Abi | null>(() => {
    if (!using) return exampleAbi(def);
    try {
      return using.abi ? parseAbi(using.abi) : null;
    } catch {
      return null;
    }
  }, [using, def]);
  const me = wallet ? fromHex(wallet.account.account, 32) : null;
  const [drafts, setDrafts] = useState<Draft[]>(() => initialDrafts(def, abi, me, conn?.network ?? null, presets));
  const [funds, setFunds] = useState<FundsDraft[]>([]);
  const [problem, setProblem] = useState<string | null>(null);
  const sender = useSend();

  // a new ABI (another build) or a new wallet starts the form again
  useEffect(() => {
    setDrafts(initialDrafts(def, abi, wallet ? fromHex(wallet.account.account, 32) : null, conn?.network ?? null, presets));
    setFunds([]);
  }, [abi, def, wallet, conn, presets]);
  useEffect(() => {
    // only when the dialog opens: a build made while it is open does not change the choice
    if (open) setChoice(build ? 'build' : 'example');
  }, [open]);

  const params = abi?.init?.params ?? [];
  const argValues = useMemo(() => valuesOf(abi, params, drafts, conn?.network), [abi, params, drafts, conn]);
  const checks = useInitCheck(conn, def, abi, params, argValues);

  const deploy = async () => {
    if (!conn || !wallet) return;
    setProblem(null);
    let code: Uint8Array;
    let initArgs: Uint8Array;
    let attached: Funds[] = [];
    let details: { label: string; value: string }[];
    try {
      code = using ? using.wasm : await exampleWasm(def);
      const values = params.map((p, i) => valueOf(abi!, p.type, drafts[i] ?? emptyDraft(abi!, p.type), { network: conn.network }, p.name));
      initArgs = abi ? encodeArgs(abi, params.map((p) => p.type), values) : new Uint8Array();
      details = await writeKnowingContracts(conn, (isContract) => params.map((p, i) => ({
        label: p.name, value: formatValue(abi!, p.type, values[i] ?? null, { network: conn.network, contract: isContract }),
      })));
      if (abi?.init?.payable) attached = await fundsOf(funds, async (asset) => (await assetMeta(conn, asset)).decimals);
    } catch (e) {
      setProblem(e instanceof InputError ? e.message : errorText(e, t));
      return;
    }
    // the arguments as checked: the banner shows what is wrong with them
    const found = checks.settled ? checks.problems : await checks.run().catch(() => []);
    if (found.length > 0) return;
    const activity = await sender.send({
      title: m.title(name),
      action: { kind: 'Deploy', code, init_args: initArgs, funds: attached },
      details,
    });
    if (activity && activity.status?.receipt?.success !== false) {
      addInstance(conn, def.id, {
        id: activity.entry.contract, origin: 'mine', at: Date.now(), ...(using?.abi ? { abi: using.abi } : {}),
      });
      navigate({ example: def.id, tab: 'app', contract: activity.entry.contract });
      onClose();
    }
  };

  const mend = (fix: NonNullable<InitProblem['fix']>) => {
    if (!abi) return;
    setDrafts(drafts.map((d, i) => (i === fix.param && params[i] ? draftOf(abi, params[i].type, fix.value) : d)));
  };
  const note = (m.notes as Partial<Record<string, string>>)[def.id];
  return <Modal open={open} title={m.title(name)} onClose={sender.busy ? () => {} : onClose} wide
    footer={<>
      <Button variant="ghost" onClick={onClose} disabled={sender.busy}>{t.core.common.cancel}</Button>
      <Button variant="primary" onClick={() => void deploy()} busy={sender.busy}
        disabled={!conn || !wallet || sender.busy || (choice === 'build' && !build) || checks.problems.length > 0}>
        {m.button}
      </Button>
    </>}>
    <fieldset className="choice">
      <legend className="field-label">{m.build}</legend>
      <label className="radio"><input type="radio" name="build" checked={choice === 'example'} onChange={() => setChoice('example')} />
        <span>{m.exampleBuild}{def.build && <span className="muted"> · {m.size(def.build.bytes.toLocaleString())}</span>}</span></label>
      <label className={`radio${build ? '' : ' is-disabled'}`}><input type="radio" name="build" disabled={!build} checked={choice === 'build'} onChange={() => setChoice('build')} />
        <span>{build ? m.yourBuild(new Date(build.at).toLocaleTimeString()) : m.yourBuild('—')}
          {build && <span className="muted"> · {m.size(build.wasm.length.toLocaleString())}</span>}</span></label>
    </fieldset>
    {choice === 'build' && build && build.source !== null && build.source !== workspace.source && <Banner tone="warn">{t.core.code.stale}</Banner>}
    {!abi && <Banner tone="warn">{t.core.code.abiMissing}</Banner>}
    <p>{m.intro}</p>
    {note && <p className="field-hint">{note}</p>}
    <div className="method-form">
      {abi && params.map((p, i) => <ValueEditor key={`${p.name}-${i}`} abi={abi} type={p.type} label={p.name} draft={drafts[i] ?? emptyDraft(abi, p.type)}
        onChange={(d) => setDrafts(drafts.map((x, j) => (j === i ? d : x)))} />)}
      {abi?.init?.payable && <FundsEditor rows={funds} onChange={setFunds} />}
    </div>
    {abi?.init?.doc && <p className="field-hint">{abi.init.doc}</p>}
    {checks.problems.length > 0 && <Banner tone="danger">
      {checks.problems.map((p, i) => {
        const instead = p.fix?.as === 'asset' ? assetInstead(p.problem) : null;
        const label = p.fix?.as === 'token' ? t.core.ids.useToken : instead ? t.core.ids.useAsset(instead.name) : null;
        return <p key={`${p.path}-${i}`} className="deploy-problem">
          <span className="mono">{p.path}</span>: {idProblemText(t.core.ids, p.problem)}
          {p.fix && label && <Button size="sm" onClick={() => mend(p.fix!)}>{label}</Button>}
        </p>;
      })}
    </Banner>}
    {!wallet && <Banner tone="info">{t.core.errors.connectFirst}</Banner>}
    {problem && <Banner tone="danger">{problem}</Banner>}
    {sender.error && <Banner tone="danger">{sender.error}</Banner>}
    {sender.busy && <p className="muted">{t.core.activity.states.pending}…</p>}
  </Modal>;
}
