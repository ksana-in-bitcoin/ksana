/** The AMM: the pool's two sides, reserves and prices, your position in it, swapping and providing liquidity, and the latest trades. */
import { useEffect, useMemo, useState } from 'react';
import { fromHex, toHex } from '@ksana/sdk';
import { requestDeploy } from '../../app/deploys';
import { useAccount } from '../../chain/account';
import { formatUnits, useAssetMeta } from '../../chain/assets';
import { useConnection } from '../../chain/connection';
import { explorerLink } from '../../chain/deployment';
import { assetInstead } from '../../chain/ids';
import { useView } from '../../chain/views';
import { useWallet } from '../../chain/wallet';
import { useT, type Messages } from '../../i18n';
import { idProblemText } from '../../ui/errors';
import { ContractEvents, type ContractEvent } from '../../ui/Events';
import { Banner, Button, Id, Rows, Section, Stat } from '../../ui/ui';
import { AccountText, AssetName, sameAccount } from '../../ui/values';
import { HowItWorks } from '../Prose';
import type { AppProps } from '../kit';
import { percentText, unitPrice, withdraw } from './math';
import { amountText, balanceOf, numberText, pairOf, poolSide, reservesOf, type Pool, type PoolSide, type Side } from './pool';
import { sideValue, useSideCheck, useTokenHoldings, type SideCheck } from './token';
import { TradePanel } from './Trade';
import './amm.css';

/** The latest value a view read, kept through a failed re-read so the forms below keep what was typed */
function useLastGood<T>(value: T | null): T | null {
  const [kept, setKept] = useState<T | null>(value);
  useEffect(() => {
    if (value !== null) setKept(value);
  }, [value]);
  return value ?? kept;
}

export default function App({ contract, abi }: AppProps) {
  const t = useT();
  const m = t.amm;
  const conn = useConnection();
  const wallet = useWallet();
  const me = wallet ? fromHex(wallet.account.account, 32) : null;
  const tokensView = useView(contract, abi, 'tokens');
  const reservesView = useView(contract, abi, 'reserves');
  const totalView = useView(contract, abi, 'total_shares');
  const mineView = useView(contract, abi, 'shares_of', me ? [me] : [], { enabled: !!me });
  const tokens = useLastGood(tokensView.data);
  const reserves = useLastGood(reservesView.data);
  const total = useLastGood(totalView.data);
  const pair = useMemo(() => pairOf(tokens), [tokens]);
  const account = useAccount();
  const metaA = useAssetMeta(conn, pair?.[0].kind === 'Asset' ? pair[0].hex : null);
  const metaB = useAssetMeta(conn, pair?.[1].kind === 'Asset' ? pair[1].hex : null);
  const checkA = useSideCheck(pair?.[0] ?? null, contract);
  const checkB = useSideCheck(pair?.[1] ?? null, contract);
  const tokenA = useTokenHoldings(pair?.[0] ?? null, checkA, contract, me);
  const tokenB = useTokenHoldings(pair?.[1] ?? null, checkB, contract, me);
  const amounts = reservesOf(reserves);
  const error = tokensView.error ?? reservesView.error ?? totalView.error ?? mineView.error;
  if (!pair || !amounts || typeof total !== 'bigint') {
    const unreadable = tokens !== null && !pair;
    return <Section title={m.overview}>
      {error ? <Banner tone="danger">{error}</Banner>
        : unreadable ? <Banner tone="danger">{m.unreadable}</Banner>
          : <p className="muted">{t.core.common.loading}</p>}
    </Section>;
  }
  const facts = (side: Side, meta: typeof metaA, check: SideCheck | null, token: typeof tokenA) => ({
    meta,
    ...(side.kind === 'Token' && check ? { token: check.kind === 'ok' ? check.token : null, ks20: check.kind === 'ok' && check.ks20 } : {}),
    held: side.kind === 'Asset' ? balanceOf(account, side) : token.held,
    allowance: token.allowance,
  });
  const pool: Pool = {
    a: poolSide(pair[0], facts(pair[0], metaA, checkA, tokenA), m.tokenName),
    b: poolSide(pair[1], facts(pair[1], metaB, checkB, tokenB), m.tokenName),
    reserveA: amounts[0],
    reserveB: amounts[1],
    totalShares: total,
  };
  const mine = me && typeof mineView.data === 'bigint' ? mineView.data : null;
  const { a, b } = pool;
  const empty = pool.reserveA === 0n || pool.reserveB === 0n;
  const broken = checkA?.kind === 'broken' || checkB?.kind === 'broken';
  // the KS-20 sides, whose transfer_and_call reaches the pool: the ops of one-step swaps and deposits are listed under them
  const oneStep = [a, b].filter((side) => side.kind === 'Token' && side.ks20).map((side) => side.hex);
  const renderEvent = (e: ContractEvent) => {
    const text = eventText(e, pool, m);
    if (text === null) return null;
    const who = e.values.trader ?? e.values.provider;
    const account = who instanceof Uint8Array ? who : null;
    return <p className="amm-event">
      {account && <AccountText value={account} network={conn?.network} />}
      {sameAccount(account, me) && <span className="pill pill--ok">{t.core.common.you}</span>}
      {' '}{text}
    </p>;
  };
  return <div className="app-grid">
    <div className="app-col">
      {error && <div><Banner tone="danger">{error}</Banner></div>}
      <Section title={<>{a.name} / {b.name} <span className="muted">{m.pool}</span></>}>
        <div className="form-grid">
          <div className="stats">
            <Stat label={m.inPool(a.name)} value={numberText(a, pool.reserveA)} sub={a.baseUnits ? m.baseUnits : undefined} />
            <Stat label={m.inPool(b.name)} value={numberText(b, pool.reserveB)} sub={b.baseUnits ? m.baseUnits : undefined} />
          </div>
          <Rows rows={[
            { label: m.price, value: empty ? <span className="muted">{m.noPrice}</span> : <Prices pool={pool} /> },
            { label: m.totalShares, value: <span className="mono">{formatUnits(pool.totalShares, 0)}</span> },
            { label: m.fee, value: m.feeValue },
            { label: m.sideA, value: <SideId side={a} /> },
            { label: m.sideB, value: <SideId side={b} /> },
          ]} />
        </div>
        {me && !(broken && pool.totalShares === 0n) && <Position pool={pool} mine={mine} />}
      </Section>
      {broken && <BrokenPool pool={pool} checks={[checkA, checkB]} />}
      {(!broken || pool.totalShares > 0n) && <TradePanel contract={contract} abi={abi} pool={pool} mine={mine} />}
    </div>
    <div className="app-col">
      <ContractEvents contract={contract} abi={abi} title={m.recent} render={renderEvent} via={oneStep} />
      <HowItWorks title={m.howTitle} items={m.how} />
    </div>
  </div>;
}

/** The price both ways, at the pool's current reserves */
function Prices({ pool }: { pool: Pool }) {
  const { a, b } = pool;
  if (a.decimals === null || b.decimals === null) return <span className="muted">…</span>;
  const ab = unitPrice(pool.reserveA, pool.reserveB, a.decimals, b.decimals);
  const ba = unitPrice(pool.reserveB, pool.reserveA, b.decimals, a.decimals);
  return <div className="amm-prices">
    <span className="mono">1 {a.name} = {ab ?? '…'} {b.name}</span>
    <span className="mono">1 {b.name} = {ba ?? '…'} {a.name}</span>
  </div>;
}

function SideId({ side }: { side: PoolSide }) {
  const m = useT().amm;
  if (side.kind === 'Token') {
    // the symbol once the token's metadata is read; without it the name is the ID again
    const named = side.decimals !== null && !side.baseUnits;
    return <span className="id">{named && <span className="unit">{side.name}</span>}<span className="muted">{m.tokenContract}</span>
      <Id value={side.hex} href={explorerLink('contract', side.hex)} /></span>;
  }
  return <span className="id"><AssetName asset={side.hex} /><Id value={side.hex} /></span>;
}

/** The connected account's shares, what they are worth now, and what the wallet holds of the two assets */
function Position({ pool, mine }: { pool: Pool; mine: bigint | null }) {
  const t = useT();
  const m = t.amm;
  const { a, b } = pool;
  const worth = mine !== null && mine > 0n ? withdraw(mine, pool.reserveA, pool.reserveB, pool.totalShares) : null;
  const held = [a, b].flatMap((side) => (side.held === null ? [] : [amountText(side, side.held)]));
  return <>
    <h3 className="sub-title">{m.yourPosition}</h3>
    <div className="form-grid">
      {mine === null ? <p className="muted">{t.core.common.loading}</p>
        : mine === 0n ? <p className="field-hint">{m.noShares}</p>
          : <div className="stats">
            <Stat label={m.yourShare} value={percentText(mine, pool.totalShares)} sub={m.sharesCount(formatUnits(mine, 0))} />
            <Stat label={m.yoursIn(a.name)} value={worth ? numberText(a, worth.a) : '…'} />
            <Stat label={m.yoursIn(b.name)} value={worth ? numberText(b, worth.b) : '…'} />
          </div>}
      {held.length > 0 && <p className="field-hint">{m.inWallet(held.join(' · '))}</p>}
    </div>
  </>;
}

/**
 * A pool one of whose sides names no asset or token contract it can take, as a token side given an asset's ID or an asset side
 * given a KS-20 token's: what each such side names, and a new pool with it mended when its ID leads to an asset, or to a token
 * taken as one. One that holds shares took deposits after all (a contract that does not answer as a token the page reads), so
 * the trade panel stays and nothing is said to fail.
 */
function BrokenPool({ pool, checks }: { pool: Pool; checks: [SideCheck | null, SideCheck | null] }) {
  const t = useT();
  const m = t.amm;
  const sides = [pool.a, pool.b] as const;
  const labels = [m.sideA, m.sideB];
  const mended = sides.map((side, i) => {
    const check = checks[i];
    if (check?.kind !== 'broken') return { value: sideValue(side.kind, side.id), name: null as string | null, token: false, ok: true };
    if (check.problem.kind === 'assetIsToken') return { value: sideValue('Token', side.id), name: null, token: true, ok: true };
    const instead = assetInstead(check.problem);
    return instead ? { value: sideValue('Asset', fromHex(instead.id, 32)), name: instead.name, token: false, ok: true } : { value: null, name: null, token: false, ok: false };
  });
  const names = mended.flatMap((x) => (x.name ? [x.name] : []));
  const asToken = mended.some((x) => x.token);
  const unused = pool.totalShares === 0n;
  const fixable = unused && mended.every((x) => x.ok) && (names.length > 0 || asToken);
  return <Section>
    <div className="form-grid">
      <Banner tone="danger" title={m.brokenTitle}>
        {sides.map((side, i) => {
          const check = checks[i];
          return check?.kind === 'broken' ? <p key={side.hex}>{labels[i]}: {idProblemText(t.core.ids, check.problem)}</p> : null;
        })}
        {unused && <p>{m.brokenFixed}</p>}
        {fixable && names.length > 0 && <p>{m.brokenInstead}</p>}
        {fixable && asToken && <p>{m.brokenAsToken}</p>}
      </Banner>
      {fixable && <div className="method-actions">
        <Button variant="primary" onClick={() => requestDeploy('amm', { side_a: mended[0]!.value!, side_b: mended[1]!.value! })}>
          {names.length > 0 ? m.deployInstead(names.join(', ')) : m.deployAsToken}
        </Button>
      </div>}
    </div>
  </Section>;
}

/** An event of the pool as a sentence; null for one the page cannot word (the generic rows show it) */
function eventText(e: ContractEvent, pool: Pool, m: Messages['amm']): string | null {
  const v = e.values;
  if (e.name === 'swap') {
    const id = v.id_in instanceof Uint8Array ? toHex(v.id_in) : null;
    const from = id === pool.a.hex ? pool.a : id === pool.b.hex ? pool.b : null;
    if (!from || typeof v.amount_in !== 'bigint' || typeof v.amount_out !== 'bigint') return null;
    return m.swapped(amountText(from, v.amount_in), amountText(from === pool.a ? pool.b : pool.a, v.amount_out));
  }
  if (typeof v.amount_a !== 'bigint' || typeof v.amount_b !== 'bigint' || typeof v.shares !== 'bigint') return null;
  const shares = formatUnits(v.shares, 0);
  if (e.name === 'add_liquidity') return m.added(amountText(pool.a, v.amount_a), amountText(pool.b, v.amount_b), shares);
  if (e.name === 'remove_liquidity') return m.removed(amountText(pool.a, v.amount_a), amountText(pool.b, v.amount_b), shares);
  return null;
}
