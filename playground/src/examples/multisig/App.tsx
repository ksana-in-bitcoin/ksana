/** The multisig wallet: its owners and threshold, what it holds, deposits, and the proposals its owners approve and execute together. */
import { useState } from 'react';
import type { Value } from '../../abi/types';
import { KSA_HEX } from '../../chain/assets';
import { useView } from '../../chain/views';
import { useWallet } from '../../chain/wallet';
import { useT } from '../../i18n';
import { ContractEvents } from '../../ui/Events';
import { Banner, Section, Stat } from '../../ui/ui';
import { HowItWorks } from '../Prose';
import type { AppProps } from '../kit';
import { eventOf, fundsNeeded, walletOf } from './actions';
import { BalancesPanel, DepositPanel } from './Funds';
import { NewProposal } from './NewProposal';
import { ProposalsPanel } from './Proposals';
import { EventWords, Who } from './parts';
import { useBalances, useHoldings, useMetas, useProposals } from './reads';
import './multisig.css';

export default function App({ contract, abi }: AppProps) {
  const t = useT();
  const m = t.multisig;
  const wallet = useWallet();
  const me = wallet ? wallet.account.account.toLowerCase() : null;
  const view = useView<Value>(contract, abi, 'wallet');
  const info = view.data !== null ? walletOf(view.data) : null;
  const proposals = useProposals(contract, abi, info?.latest ?? null);
  const holdings = useHoldings(contract);
  const [watched, setWatched] = useState<string[]>([]);

  // the assets listed: KSA, those the wallet holds, those its pending proposals take, then the ones watched
  const list = proposals.data?.list ?? null;
  const found = new Set<string>([KSA_HEX, ...(holdings.data ?? [])]);
  for (const p of list ?? []) if (p.status === 'Pending') for (const f of fundsNeeded(p.action)) found.add(f.asset);
  const assets = [...new Set([...found, ...watched])];
  const balances = useBalances(contract, abi, assets);
  const metas = useMetas(assets);

  if (!info) {
    return <Section title={m.overview}>
      {view.error ? <Banner tone="danger">{view.error}</Banner> : view.data !== null ? <Banner tone="danger">{m.unexpected}</Banner>
        : <p className="muted">{t.core.common.loading}</p>}
    </Section>;
  }
  const isOwner = me !== null && info.owners.includes(me);
  const pending = list?.filter((p) => p.status === 'Pending') ?? null;
  const needYou = pending?.filter((p) => !p.approvals.includes(me ?? '') && p.approvals.length < info.threshold).length ?? null;
  return <div className="app-grid">
    <div className="app-col">
      <Section title={m.overview}>
        <div className="stats">
          <Stat label={m.approvalsNeeded} value={m.kOfN(String(info.threshold), String(info.owners.length))} />
          <Stat label={m.pending} value={pending ? pending.length.toLocaleString() : '…'} sub={m.pendingOf(info.latest.toLocaleString())} />
          {isOwner && <Stat label={m.needYou} value={needYou !== null ? needYou.toLocaleString() : '…'} />}
        </div>
        <h3 className="sub-title">{m.owners}</h3>
        <ul className="multisig-owners">{info.owners.map((o) => <li key={o}><Who account={o} me={me} /></li>)}</ul>
        {!wallet ? <p className="field-hint multisig-gap">{t.core.errors.connectFirst}</p>
          : isOwner ? <p className="field-hint multisig-gap">{m.youOwner}</p>
            : <Banner tone="info">{m.notOwner}</Banner>}
      </Section>
      <ProposalsPanel contract={contract} abi={abi} info={info} me={me} proposals={proposals} balances={balances.data} metas={metas} />
      {(!wallet || isOwner) && <NewProposal contract={contract} abi={abi} info={info} me={me} assets={assets} balances={balances.data} />}
    </div>
    <div className="app-col">
      <BalancesPanel assets={assets} balances={balances} watched={watched} found={found}
        onWatch={(a) => setWatched((w) => (w.includes(a) ? w : [...w, a]))} onUnwatch={(a) => setWatched((w) => w.filter((x) => x !== a))} />
      <DepositPanel contract={contract} abi={abi} />
      <ContractEvents contract={contract} abi={abi} title={m.recent} render={(e) => {
        const ev = eventOf(e.name, e.values);
        return ev ? <EventWords event={ev} me={me} /> : null;
      }} />
      <HowItWorks title={m.howTitle} items={m.how} />
    </div>
  </div>;
}
