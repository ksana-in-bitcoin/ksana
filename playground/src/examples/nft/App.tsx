/** The NFT collection: its supply and minter, your tokens and what you can do with each, the marketplace, every token, minting and operators. */
import { useId, useRef, useState } from 'react';
import { fromHex, toHex } from '@ksana/sdk';
import { tokenIdText } from '../../abi/text';
import type { Value } from '../../abi/types';
import { useConnection } from '../../chain/connection';
import { dateLocale, formatDateTime, useChainTime } from '../../chain/time';
import { useView } from '../../chain/views';
import { useWallet } from '../../chain/wallet';
import { useLocale, useT } from '../../i18n';
import { ContractEvents, type ContractEvent } from '../../ui/Events';
import { ExpiryField, expiryOf, isExpired, useExpiryText, type ExpiryChoice } from '../../ui/expiry';
import { Banner, Button, Empty, Field, Meter, Rows, Section, Sentence, Stat } from '../../ui/ui';
import { AccountField, AccountText, accountOf, sameAccount } from '../../ui/values';
import { HowItWorks } from '../Prose';
import { callAction, useSend, type AppProps } from '../kit';
import { BrowseCard, BuyPanel, ListingCard, OWNER, Price, TokenCard, recipientProblem } from './cards';
import { type Collection, type TokenList, MAX_URI_BYTES, count, isZero, typedId, useToken, useTokenList, utf8Bytes } from './tokens';
import './nft.css';

const EVENTS = new Set(['standards', 'metadata', 'transfer', 'approve', 'operator', 'list', 'unlist', 'sale', 'minter']);

export default function App({ contract, abi }: AppProps) {
  const t = useT();
  const m = t.nft;
  const conn = useConnection();
  const wallet = useWallet();
  const info = useView<Collection>(contract, abi, 'collection');
  const me = wallet ? fromHex(wallet.account.account, 32) : null;
  const held = useView<bigint>(contract, abi, 'balance_of', me ? [me] : [], { enabled: !!me });
  const c = info.data;
  if (!c) {
    return <Section title={m.collection}>{info.error ? <Banner tone="danger">{info.error}</Banner> : <p className="muted">{t.core.common.loading}</p>}</Section>;
  }
  const isMinter = sameAccount(c.minter, me);
  const full = c.max_supply !== null && c.minted >= c.max_supply;
  const share = c.max_supply ? Number((c.minted * 10_000n) / c.max_supply) / 10_000 : null;
  return <div className="app-grid">
    <div className="app-col">
      <Section title={<>{c.name} <span className="muted mono">{c.symbol}</span></>}>
        <div className="stats nft-stats">
          <Stat label={m.supply} value={count(c.supply)} sub={m.burnedSub(count(c.minted - c.supply))} />
          <Stat label={m.minted} value={count(c.minted)} sub={c.max_supply !== null ? m.ofMax(count(c.max_supply)) : m.noMax} />
          <Stat label={m.yours} value={me ? (held.data !== null ? count(held.data) : '…') : '—'} />
        </div>
        {share !== null && <Meter value={share} label={m.mintedOfMax} tone={full ? 'warn' : 'qing'} />}
        <Rows rows={[
          {
            label: m.minter,
            value: c.minter
              ? <span className="id"><AccountText value={c.minter} network={conn?.network} />{isMinter && <span className="pill pill--ok">{t.core.common.you}</span>}</span>
              : <span className="muted">{m.noMinter}</span>,
          },
          {
            label: m.minting,
            value: c.minter === null ? <span className="pill pill--quiet">{m.mintingEnded}</span>
              : full ? <span className="pill pill--warn">{m.mintingFull}</span>
                : <span className="pill pill--ok">{m.mintingOpen}</span>,
          },
        ]} />
        {me && c.minter && !isMinter && !full && <p className="field-hint">{m.onlyMinter}</p>}
        {held.error && <Banner tone="danger">{held.error}</Banner>}
      </Section>
      {isMinter && <MintPanel contract={contract} abi={abi} info={c} />}
      <YourTokens contract={contract} abi={abi} me={me} held={held.data} />
      <Market contract={contract} abi={abi} me={me} />
      <AllTokens contract={contract} abi={abi} me={me} info={c} />
    </div>
    <div className="app-col">
      <Operators contract={contract} abi={abi} me={me} />
      {isMinter && <MinterPanel contract={contract} abi={abi} me={me} />}
      <ContractEvents contract={contract} abi={abi} title={m.recent} render={(e) => (EVENTS.has(e.name) ? <EventLine e={e} /> : null)} />
      <HowItWorks title={m.howTitle} items={m.how} />
    </div>
  </div>;
}

function More({ list }: { list: TokenList }) {
  const t = useT();
  return <div className="method-actions nft-more">
    <Button size="sm" disabled={list.growing} busy={list.growing} onClick={list.showMore}>{t.core.common.more}</Button>
  </div>;
}

function MintPanel({ contract, abi, info }: AppProps & { info: Collection }) {
  const m = useT().nft;
  const conn = useConnection();
  const wallet = useWallet();
  const uriId = useId();
  const [to, setTo] = useState(wallet?.account.address ?? '');
  const [uri, setUri] = useState('');
  const sender = useSend();
  const target = accountOf(to, conn?.network).account;
  const problem = target ? recipientProblem(target, contract, m) : null;
  const bytes = utf8Bytes(uri);
  const tooLong = bytes > MAX_URI_BYTES;
  const left = info.max_supply !== null ? info.max_supply - info.minted : null;
  const next = (info.minted + 1n).toString();
  if (left !== null && left <= 0n) return <Section title={m.mint}><Banner tone="warn">{m.capReached}</Banner></Section>;
  return <Section title={m.mint}>
    <p className="field-hint nft-hint">{m.mintHint}{left !== null && <> {m.left(count(left))}</>}</p>
    <div className="form-grid">
      <AccountField label={m.to} value={to} onChange={setTo} />
      {problem && <p className="field-error">{problem}</p>}
      <Field label={m.uri} htmlFor={uriId} hint={m.uriHint(String(bytes))} error={tooLong ? m.uriTooLong(String(bytes)) : null}>
        <textarea id={uriId} className="input" rows={2} spellCheck={false} autoCapitalize="off" value={uri} placeholder="https://…"
          onChange={(e) => setUri(e.target.value)} />
      </Field>
      <div className="method-actions">
        <Button variant="primary" disabled={!target || !!problem || tooLong || sender.busy} busy={sender.busy}
          onClick={() => target && void sender.send({
            title: m.mintTitle(next),
            action: callAction(abi, contract, 'mint', [target, uri === '' ? null : uri]),
            details: [{ label: m.to, value: to.trim() }, { label: m.uri, value: uri || m.noUri }],
          }).then((a) => { if (a) setUri(''); })}>{m.mintButton(next)}</Button>
      </div>
      {sender.error && <Banner tone="danger">{sender.error}</Banner>}
    </div>
  </Section>;
}

function YourTokens({ contract, abi, me, held }: AppProps & { me: Uint8Array | null; held: bigint | null }) {
  const t = useT();
  const m = t.nft;
  const list = useTokenList(contract, abi, me ? { view: 'details_of', owner: me } : null);
  const tokens = list.tokens;
  const shown = me && tokens && held !== null && held > 0n ? <span className="muted">{m.shown(count(BigInt(tokens.length)), count(held))}</span> : undefined;
  return <Section title={m.yourTokens} actions={shown}>
    {!me ? <p className="field-hint">{t.core.errors.connectFirst}</p>
      : list.error ? <Banner tone="danger">{list.error}</Banner>
        : tokens === null ? <p className="muted">{t.core.common.loading}</p>
          : tokens.length === 0 ? <Empty>{m.noTokens}</Empty>
            : <div className="nft-grid">
              {tokens.map((token) => <TokenCard key={token.id.toString()} contract={contract} abi={abi} token={token} role={OWNER} />)}
            </div>}
    {me && list.more && <More list={list} />}
  </Section>;
}

function Market({ contract, abi, me }: AppProps & { me: Uint8Array | null }) {
  const t = useT();
  const m = t.nft;
  const list = useTokenList(contract, abi, { view: 'listings' });
  const tokens = list.tokens;
  return <Section title={m.market}>
    <p className="field-hint nft-hint">{m.marketHint}</p>
    {list.error ? <Banner tone="danger">{list.error}</Banner>
      : tokens === null ? <p className="muted">{t.core.common.loading}</p>
        : tokens.length === 0 ? <Empty>{m.noListings}</Empty>
          : <div className="nft-grid">
            {tokens.map((token) => <ListingCard key={token.id.toString()} contract={contract} abi={abi} token={token} me={me} />)}
          </div>}
    {list.more && <More list={list} />}
  </Section>;
}

function reducedMotion(): boolean {
  try {
    return window.matchMedia('(prefers-reduced-motion: reduce)').matches;
  } catch {
    return false;
  }
}

function AllTokens({ contract, abi, me, info }: AppProps & { me: Uint8Array | null; info: Collection }) {
  const t = useT();
  const m = t.nft;
  const list = useTokenList(contract, abi, { view: 'details' });
  const [text, setText] = useState('');
  const [lookId, setLookId] = useState<bigint | null>(null);
  const findRef = useRef<HTMLFormElement>(null);
  const inputId = useId();
  const typed = typedId(text);
  const tokens = list.tokens;
  const open = (id: bigint) => {
    setText(id.toString());
    setLookId(id);
    findRef.current?.scrollIntoView({ block: 'start', behavior: reducedMotion() ? 'auto' : 'smooth' });
  };
  const shown = tokens && info.supply > 0n ? <span className="muted">{m.shown(count(BigInt(tokens.length)), count(info.supply))}</span> : undefined;
  return <Section title={m.allTokens} actions={shown}>
    <form ref={findRef} className="nft-find" onSubmit={(e) => {
      e.preventDefault();
      if (typed !== null) setLookId(typed);
    }}>
      <Field label={m.findLabel} htmlFor={inputId} error={text.trim() !== '' && typed === null ? m.badId : null}>
        <div className="input-row">
          <input id={inputId} className="input mono" inputMode="numeric" autoComplete="off" value={text} placeholder="1" onChange={(e) => setText(e.target.value)} />
          <Button type="submit" disabled={typed === null}>{m.find}</Button>
        </div>
      </Field>
    </form>
    {lookId !== null && <Found key={lookId.toString()} contract={contract} abi={abi} id={lookId} me={me} minted={info.minted}
      onClose={() => { setLookId(null); setText(''); }} />}
    <h3 className="sub-title">{m.everyToken}</h3>
    {list.error ? <Banner tone="danger">{list.error}</Banner>
      : tokens === null ? <p className="muted">{t.core.common.loading}</p>
        : tokens.length === 0 ? <Empty>{info.minted === 0n ? m.noneMinted : m.allBurned}</Empty>
          : <div className="nft-grid">{tokens.map((token) => <BrowseCard key={token.id.toString()} token={token} me={me} onOpen={open} />)}</div>}
    {list.more && <More list={list} />}
  </Section>;
}

/** The token looked up by ID, with what the connected account may do with it as its owner, approved account or an operator */
function Found({ contract, abi, id, me, minted, onClose }: AppProps & { id: bigint; me: Uint8Array | null; minted: bigint; onClose: () => void }) {
  const t = useT();
  const m = t.nft;
  const { token, error } = useToken(contract, abi, id);
  const owner = !!token && sameAccount(token.owner, me);
  const operator = useView<boolean>(contract, abi, 'is_operator', token && me ? [token.owner, me] : [], { enabled: !!token && !!me && !owner });
  const close = <div className="method-actions"><Button size="sm" variant="ghost" onClick={onClose}>{t.core.common.close}</Button></div>;
  if (error) return <div className="nft-found"><Banner tone="danger">{error}</Banner>{close}</div>;
  if (token === undefined) return <div className="nft-found"><p className="muted">{t.core.common.loading}</p></div>;
  if (token === null) {
    return <div className="nft-found"><Empty>{id > minted ? m.notMinted(id.toString(), count(minted)) : m.burned(id.toString())}</Empty>{close}</div>;
  }
  const role = { owner, approved: !owner && sameAccount(token.approved, me), operator: !owner && operator.data === true };
  const badges = <>
    {role.owner && <span className="pill pill--quiet">{m.youOwn}</span>}
    {role.approved && <span className="pill pill--quiet">{m.youApproved}</span>}
    {role.operator && <span className="pill pill--quiet">{m.youOperator}</span>}
  </>;
  return <div className="nft-found">
    <TokenCard contract={contract} abi={abi} token={token} role={role} showOwner badges={badges}>
      {!owner && token.listing && <BuyPanel contract={contract} abi={abi} token={token} />}
    </TokenCard>
    {operator.error && <Banner tone="danger">{operator.error}</Banner>}
    {close}
  </div>;
}

function Operators({ contract, abi, me }: AppProps & { me: Uint8Array | null }) {
  const m = useT().nft;
  const conn = useConnection();
  const time = useChainTime();
  const expiryText = useExpiryText();
  const [text, setText] = useState('');
  const [expiry, setExpiry] = useState<ExpiryChoice>('week');
  const [doing, setDoing] = useState<'allow' | 'revoke' | null>(null);
  const sender = useSend();
  const op = accountOf(text, conn?.network).account;
  const problem = !op ? null : isZero(op) ? m.errZero : sameAccount(op, me) ? m.errSelfOperator : null;
  const asked = !!me && !!op && !problem;
  // as set, expired or not: an expired operator can move nothing, and taking it back clears it
  const info = useView<[boolean, bigint | null]>(contract, abi, 'operator_info', me && op ? [me, op] : [], { enabled: asked });
  const known = asked && info.data !== null;
  const [approved, expiresAt] = info.data ?? [false, null];
  const active = approved && !isExpired(expiresAt, time.now);
  const set = (allow: boolean) => {
    if (!op) return;
    const until = allow ? expiryOf(expiry, time.now) : null;
    setDoing(allow ? 'allow' : 'revoke');
    void sender.send({
      title: allow ? m.allowTitle : m.revokeTitle,
      action: callAction(abi, contract, 'set_operator', [op, allow, until]),
      details: [
        { label: m.operator, value: text.trim() },
        ...(allow ? [{ label: m.expires, value: expiryText(until, time.now) }] : []),
      ],
    }).then(() => setDoing(null));
  };
  return <Section title={m.operators}>
    <p className="field-hint nft-hint">{m.operatorsHint}</p>
    <div className="form-grid">
      <AccountField label={m.operator} value={text} onChange={setText} />
      {problem && <p className="field-error">{problem}</p>}
      {asked && !info.error && <p className="field-hint">{!known ? '…' : !approved ? m.notOperator
        : active ? m.isOperator(expiryText(expiresAt, time.now)) : m.operatorExpired(expiryText(expiresAt, time.now))}</p>}
      {info.error && <Banner tone="danger">{info.error}</Banner>}
      <ExpiryField value={expiry} onChange={setExpiry} time={time} disabled={sender.busy} />
      <div className="method-actions">
        <Button variant="primary" disabled={!known || sender.busy} busy={sender.busy && doing === 'allow'} onClick={() => set(true)}>{approved ? m.renew : m.allow}</Button>
        <Button variant="danger" disabled={!known || !approved || sender.busy} busy={sender.busy && doing === 'revoke'} onClick={() => set(false)}>{m.revoke}</Button>
      </div>
      {sender.error && <Banner tone="danger">{sender.error}</Banner>}
    </div>
  </Section>;
}

function MinterPanel({ contract, abi, me }: AppProps & { me: Uint8Array | null }) {
  const m = useT().nft;
  const conn = useConnection();
  const [to, setTo] = useState('');
  const [doing, setDoing] = useState<'hand' | 'end' | null>(null);
  const sender = useSend();
  const target = accountOf(to, conn?.network).account;
  const problem = !target ? null
    : isZero(target) ? m.errZero
      : sameAccount(target, me) ? m.errAlreadyMinter
        : toHex(target) === contract ? m.errMinterContract : null;
  const run = (what: 'hand' | 'end', minter: Uint8Array | null) => {
    setDoing(what);
    void sender.send({
      title: minter ? m.handOverTitle : m.endTitle,
      action: callAction(abi, contract, 'set_minter', [minter]),
      ...(minter ? { details: [{ label: m.newMinter, value: to.trim() }] } : {}),
    }).then((a) => {
      setDoing(null);
      if (a) setTo('');
    });
  };
  return <Section title={m.minterRole}>
    <p className="field-hint nft-hint">{m.minterHint}</p>
    <div className="form-grid">
      <AccountField label={m.newMinter} value={to} onChange={setTo} />
      {problem && <p className="field-error">{problem}</p>}
      <div className="method-actions">
        <Button disabled={!target || !!problem || sender.busy} busy={sender.busy && doing === 'hand'} onClick={() => target && run('hand', target)}>{m.handOver}</Button>
        <Button variant="danger" disabled={sender.busy} busy={sender.busy && doing === 'end'}
          onClick={() => window.confirm(m.endConfirm) && run('end', null)}>{m.endMinting}</Button>
      </div>
      {sender.error && <Banner tone="danger">{sender.error}</Banner>}
    </div>
  </Section>;
}

/** An event of the collection as a sentence: a mint is a `transfer` from the zero account, a burn one to it (KS-721) */
function EventLine({ e }: { e: ContractEvent }) {
  const m = useT().nft.events;
  const conn = useConnection();
  const locale = dateLocale(useLocale());
  const v = e.values;
  const who = (x: Value | undefined) => (x instanceof Uint8Array ? <AccountText value={x} network={conn?.network} /> : <span className="muted">?</span>);
  const token = <span className="mono">{v.token_id instanceof Uint8Array ? tokenIdText(v.token_id) : '?'}</span>;
  const price = v.asset instanceof Uint8Array && typeof v.price === 'bigint' ? <Price value={v.price} asset={toHex(v.asset)} /> : <span className="muted">?</span>;
  const minted = e.name === 'transfer' && v.from instanceof Uint8Array && isZero(v.from);
  const burned = e.name === 'transfer' && v.to instanceof Uint8Array && isZero(v.to);
  const text = e.name === 'standards' ? m.standards
    : e.name === 'metadata' ? m.metadata
      : minted ? m.mint
        : burned ? m.burn
          : e.name === 'transfer' ? m.transfer
            : e.name === 'approve' ? (v.spender === null ? m.approveCleared : m.approve)
              : e.name === 'operator' ? (v.approved !== true ? m.operatorOff : typeof v.expires_at === 'bigint' ? m.operatorUntil : m.operatorOn)
                : e.name === 'list' ? m.list
                  : e.name === 'unlist' ? m.unlist
                    : e.name === 'sale' ? m.sale
                      : v.minter === null ? m.minterEnded : v.previous === null ? m.minterFirst : m.minterHandover;
  const standards = Array.isArray(v.ids) ? v.ids.filter((id) => typeof id === 'string').join(', ') : '?';
  return <p className="nft-event"><Sentence text={text} parts={{
    to: who(v.to), from: who(v.from), owner: who(burned ? v.from : v.owner), spender: who(v.spender), operator: who(v.operator),
    seller: who(v.seller), buyer: who(v.buyer), previous: who(v.previous), minter: who(v.minter), token, price,
    date: typeof v.expires_at === 'bigint' ? formatDateTime(v.expires_at, locale) : '?', standards: <span className="mono">{standards}</span>,
    name: typeof v.name === 'string' ? v.name : '?', symbol: <span className="mono">{typeof v.symbol === 'string' ? v.symbol : '?'}</span>,
  }} /></p>;
}
