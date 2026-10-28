/** The token cards: one you may move, with what you can do with it; one for sale, with its Buy; one in the list of every token. */
import { useEffect, useState, type CSSProperties, type ReactNode } from 'react';
import { fromHex, toHex } from '@ksana/sdk';
import { accountText } from '../../abi/text';
import type { Abi } from '../../abi/types';
import { useAccount } from '../../chain/account';
import { KSA_HEX, formatUnits, useAssetMeta, type AssetMeta } from '../../chain/assets';
import { useConnection } from '../../chain/connection';
import { useIsContract } from '../../chain/contracts';
import type { OpRequest } from '../../chain/run';
import { useWallet } from '../../chain/wallet';
import { useT, type Messages } from '../../i18n';
import { Banner, Button } from '../../ui/ui';
import { AccountField, AccountText, UnitsField, accountOf, sameAccount } from '../../ui/values';
import { callAction, useSend } from '../kit';
import { type Token, assetHexOf, hueOf, httpsLink, idArg, isZero, isZeroAmount, priceOf } from './tokens';

type Text = Messages['nft'];

/** What the connected account may do with a token: the owner everything, an operator all but selling, the approved account send and burn */
export interface Role {
  owner: boolean;
  approved: boolean;
  operator: boolean;
}

export const OWNER: Role = { owner: true, approved: false, operator: false };

/** Why a token cannot go to this account, or null: the contract refuses the zero account and itself */
export function recipientProblem(account: Uint8Array, contract: string, m: Text): string | null {
  if (isZero(account)) return m.errZero;
  if (toHex(account) === contract) return m.errContract;
  return null;
}

export function TokenBadge({ id }: { id: bigint }) {
  return <span className="nft-badge" style={{ '--nft-hue': String(hueOf(id)) } as CSSProperties}>#{id.toString()}</span>;
}

const URI_PREVIEW = 96;

/** A token's metadata URI as text, never as markup or an image; an https one is a link */
export function Uri({ uri }: { uri: string | null }) {
  const m = useT().nft;
  const [all, setAll] = useState(false);
  if (!uri) return <p className="nft-uri muted">{m.noUri}</p>;
  const href = httpsLink(uri);
  const chars = Array.from(uri);
  const long = chars.length > URI_PREVIEW;
  const text = long && !all ? `${chars.slice(0, URI_PREVIEW).join('')}…` : uri;
  return <p className="nft-uri">
    {href ? <a href={href} target="_blank" rel="noreferrer noopener" title={uri}>{text}</a> : <span title={uri}>{text}</span>}
    {long && <> <button type="button" className="link-btn" onClick={() => setAll(!all)}>{all ? m.showLess : m.showAll}</button></>}
  </p>;
}

/**
 * The name and decimals of this asset once the node has said them: `useAssetMeta` keeps the previous asset's while it reads a
 * new one, and a price must never be read with another asset's decimals.
 */
function useAssetInfo(asset: string | null): AssetMeta | null {
  const conn = useConnection();
  const meta = useAssetMeta(conn, asset);
  return meta && meta.id === asset ? meta : null;
}

/** Whether the node answered for an asset it does not know (its placeholder has no issuer): nobody can hold any */
function isUnknown(meta: AssetMeta | null): boolean {
  return !!meta && meta.id !== KSA_HEX && meta.issuer === null;
}

/** An amount of a built-in asset in its units with its name; `…` until the node has said its decimals */
export function Price({ value, asset }: { value: bigint; asset: string }) {
  const meta = useAssetInfo(asset);
  if (!meta) return <span className="muted">…</span>;
  return <span className="amount"><span className="mono">{formatUnits(value, meta.decimals)}</span> <span className="unit">{meta.name}</span></span>;
}

function Fact({ label, children }: { label: ReactNode; children: ReactNode }) {
  return <div className="nft-fact"><span>{label}</span><span className="nft-fact-value">{children}</span></div>;
}

type Form = 'send' | 'approve' | 'sell';

/** A token the connected account may move: its facts, and the actions its role allows, each opening a small form or asking first */
export function TokenCard({ contract, abi, token, role, showOwner = false, badges, children }: {
  contract: string; abi: Abi; token: Token; role: Role; showOwner?: boolean; badges?: ReactNode; children?: ReactNode;
}) {
  const m = useT().nft;
  const conn = useConnection();
  const ownerIsContract = useIsContract(token.owner);
  const sender = useSend();
  const [form, setForm] = useState<Form | null>(null);
  const [doing, setDoing] = useState<string | null>(null);
  const id = token.id.toString();
  const arg = idArg(token.id);
  const canMove = role.owner || role.approved || role.operator;
  const canApprove = role.owner || role.operator;
  const run = (what: string, request: OpRequest) => {
    setDoing(what);
    void sender.send(request).then((done) => {
      setDoing(null);
      if (done) setForm(null);
    });
  };
  const toggle = (next: Form) => {
    sender.clear();
    setForm(form === next ? null : next);
  };
  const busy = (what: string) => sender.busy && doing === what;
  return <article className="nft-card">
    <div className="nft-card-head">
      <TokenBadge id={token.id} />
      <span className="nft-badges">{token.listing && <span className="pill pill--ok">{m.forSale}</span>}{badges}</span>
    </div>
    <Uri uri={token.uri} />
    <div className="nft-facts">
      {showOwner && <Fact label={m.owner}><AccountText value={token.owner} network={conn?.network} chars={6} /></Fact>}
      <Fact label={m.approved}>{token.approved ? <AccountText value={token.approved} network={conn?.network} chars={6} /> : <span className="muted">{m.nobody}</span>}</Fact>
      <Fact label={m.price}>{token.listing ? <Price value={token.listing.price} asset={toHex(token.listing.asset)} /> : <span className="muted">{m.notForSale}</span>}</Fact>
    </div>
    {canMove && <div className="method-actions">
      <Button size="sm" aria-expanded={form === 'send'} disabled={sender.busy} onClick={() => toggle('send')}>{m.send}</Button>
      {canApprove && <Button size="sm" aria-expanded={form === 'approve'} disabled={sender.busy} onClick={() => toggle('approve')}>{m.approve}</Button>}
      {role.owner && <Button size="sm" aria-expanded={form === 'sell'} disabled={sender.busy} onClick={() => toggle('sell')}>{token.listing ? m.changePrice : m.sell}</Button>}
      {role.owner && token.listing && <Button size="sm" disabled={sender.busy} busy={busy('unlist')}
        onClick={() => run('unlist', { title: m.unlistTitle(id), action: callAction(abi, contract, 'unlist', [arg]) })}>{m.unlist}</Button>}
      <Button size="sm" variant="danger" disabled={sender.busy} busy={busy('burn')}
        onClick={() => window.confirm(m.burnConfirm(id)) && run('burn', { title: m.burnTitle(id), action: callAction(abi, contract, 'burn', [arg]) })}>{m.burn}</Button>
    </div>}
    {/* the owner sends with `transfer`; its approved account and its operators with `transfer_from`, naming the owner */}
    {form === 'send' && <SendForm contract={contract} token={token} owner={role.owner} busy={busy('send')} disabled={sender.busy}
      onSend={(to, text) => run('send', {
        title: m.sendTitle(id),
        action: role.owner ? callAction(abi, contract, 'transfer', [to, arg]) : callAction(abi, contract, 'transfer_from', [token.owner, to, arg]),
        details: [
          ...(role.owner ? [] : [{ label: m.from, value: accountText(token.owner, conn?.network, ownerIsContract) }]),
          { label: m.to, value: text },
        ],
      })} />}
    {form === 'approve' && <ApproveForm token={token} busy={busy('approve')} clearing={busy('clear')} disabled={sender.busy}
      onApprove={(spender, text) => run('approve', {
        title: m.approveTitle(id), action: callAction(abi, contract, 'approve', [spender, arg]), details: [{ label: m.spender, value: text }],
      })}
      onClear={() => run('clear', { title: m.clearTitle(id), action: callAction(abi, contract, 'approve', [null, arg]) })} />}
    {form === 'sell' && <SellForm token={token} busy={busy('sell')} disabled={sender.busy}
      onList={(asset, price, priceText) => run('sell', {
        title: token.listing ? m.relistTitle(id, priceText) : m.listTitle(id, priceText),
        action: callAction(abi, contract, 'list', [arg, fromHex(asset, 32), price]),
        details: [{ label: m.price, value: priceText }],
      })} />}
    {sender.error && <Banner tone="danger">{sender.error}</Banner>}
    {children}
  </article>;
}

function SendForm({ contract, token, owner, busy, disabled, onSend }: {
  contract: string; token: Token; owner: boolean; busy: boolean; disabled: boolean; onSend: (to: Uint8Array, text: string) => void;
}) {
  const m = useT().nft;
  const conn = useConnection();
  const [to, setTo] = useState('');
  const target = accountOf(to, conn?.network).account;
  const problem = target ? recipientProblem(target, contract, m) ?? (sameAccount(target, token.owner) ? m.errOwner : null) : null;
  return <div className="form-grid nft-form">
    <p className="field-hint">{owner ? m.sendHint : m.sendFromHint}</p>
    <AccountField label={m.to} value={to} onChange={setTo} />
    {problem && <p className="field-error">{problem}</p>}
    <div className="method-actions">
      <Button variant="primary" size="sm" disabled={!target || !!problem || disabled} busy={busy}
        onClick={() => target && onSend(target, to.trim())}>{m.sendButton(token.id.toString())}</Button>
    </div>
  </div>;
}

function ApproveForm({ token, busy, clearing, disabled, onApprove, onClear }: {
  token: Token; busy: boolean; clearing: boolean; disabled: boolean; onApprove: (spender: Uint8Array, text: string) => void; onClear: () => void;
}) {
  const m = useT().nft;
  const conn = useConnection();
  const [text, setText] = useState('');
  const spender = accountOf(text, conn?.network).account;
  const problem = !spender ? null
    : isZero(spender) ? m.errZero
      : sameAccount(spender, token.owner) ? m.errSpenderOwner
        : sameAccount(spender, token.approved) ? m.errAlreadyApproved : null;
  return <div className="form-grid nft-form">
    <p className="field-hint">{m.approveHint}</p>
    <AccountField label={m.spender} value={text} onChange={setText} />
    {problem && <p className="field-error">{problem}</p>}
    <div className="method-actions">
      <Button variant="primary" size="sm" disabled={!spender || !!problem || disabled} busy={busy}
        onClick={() => spender && onApprove(spender, text.trim())}>{m.approveButton}</Button>
      {token.approved && <Button size="sm" disabled={disabled} busy={clearing} onClick={onClear}>{m.clearApproval}</Button>}
    </div>
  </div>;
}

function SellForm({ token, busy, disabled, onList }: {
  token: Token; busy: boolean; disabled: boolean; onList: (asset: string, price: bigint, priceText: string) => void;
}) {
  const m = useT().nft;
  const listed = token.listing ? toHex(token.listing.asset) : null;
  const [kind, setKind] = useState<'ksa' | 'other'>(listed && listed !== KSA_HEX ? 'other' : 'ksa');
  const [assetText, setAssetText] = useState(listed && listed !== KSA_HEX ? listed : '');
  const [price, setPrice] = useState('');
  const [filled, setFilled] = useState(false);
  const asset = kind === 'ksa' ? KSA_HEX : assetHexOf(assetText);
  const meta = useAssetInfo(asset);
  const unknown = isUnknown(meta);
  const ready = !!meta && !unknown;
  // a listed token's form starts at its price, once the asset's decimals are known
  useEffect(() => {
    if (filled || !token.listing || !meta || meta.id !== listed) return;
    setPrice(formatUnits(token.listing.price, meta.decimals).replace(/,/g, ''));
    setFilled(true);
  }, [filled, token.listing, meta, listed]);
  const value = ready ? priceOf(price, meta.decimals) : null;
  const zero = ready && isZeroAmount(price, meta.decimals);
  const priceText = value !== null && meta ? `${formatUnits(value, meta.decimals)} ${meta.name}` : '';
  return <div className="form-grid nft-form">
    <p className="field-hint">{m.sellHint}</p>
    <div className="vfield">
      <span className="field-label">{m.paidIn}</span>
      <div className="segmented" role="group" aria-label={m.paidIn}>
        <button type="button" className={kind === 'ksa' ? 'is-active' : ''} aria-pressed={kind === 'ksa'} onClick={() => setKind('ksa')}>KSA</button>
        <button type="button" className={kind === 'other' ? 'is-active' : ''} aria-pressed={kind === 'other'} onClick={() => setKind('other')}>{m.otherAsset}</button>
      </div>
    </div>
    {kind === 'other' && <div className="vfield">
      <label className="field-label" htmlFor={`nft-asset-${token.id}`}>{m.assetId}</label>
      <input id={`nft-asset-${token.id}`} className="input mono" spellCheck={false} autoCapitalize="off" autoCorrect="off" value={assetText}
        placeholder={m.assetIdHint} onChange={(e) => setAssetText(e.target.value)} />
      {assetText.trim() && !asset ? <span className="field-error">{m.badAssetId}</span>
        : unknown ? <span className="field-error">{m.unknownAsset}</span>
          : meta && asset !== KSA_HEX ? <span className="field-hint">{m.assetFound(meta.name, String(meta.decimals))}</span> : null}
    </div>}
    {/* the price waits for an asset the node knows: its decimals say how to read it */}
    <UnitsField label={m.price} unit={ready ? meta.name : '…'} decimals={ready ? meta.decimals : 0} value={ready ? price : ''} onChange={setPrice} disabled={!ready} />
    {zero && <p className="field-error">{m.pricePositive}</p>}
    <div className="method-actions">
      <Button variant="primary" size="sm" disabled={!asset || value === null || disabled} busy={busy}
        onClick={() => asset && value !== null && onList(asset, value, priceText)}>{token.listing ? m.relistButton : m.listButton}</Button>
    </div>
  </div>;
}

/** Buying a listed token: exactly the price attached, in the listed asset, once the wallet holds that much */
export function BuyPanel({ contract, abi, token }: { contract: string; abi: Abi; token: Token }) {
  const t = useT();
  const m = t.nft;
  const conn = useConnection();
  const sellerIsContract = useIsContract(token.owner);
  const wallet = useWallet();
  const account = useAccount();
  const sender = useSend();
  const listing = token.listing;
  const asset = listing ? toHex(listing.asset) : KSA_HEX;
  const meta = useAssetInfo(asset);
  if (!listing) return null;
  const id = token.id.toString();
  const priceText = meta && !isUnknown(meta) ? `${formatUnits(listing.price, meta.decimals)} ${meta.name}` : null;
  const balance = wallet && account && account.account === wallet.account.account ? account.balances.get(asset) ?? 0n : null;
  const enough = balance !== null && balance >= listing.price;
  // a price in an asset the node does not know can never be paid: nobody holds any
  if (isUnknown(meta)) return <div className="nft-buy"><span className="field-error">{m.unknownAsset}</span></div>;
  return <div className="nft-buy">
    <Button variant="primary" size="sm" disabled={!wallet || !priceText || !enough || sender.busy} busy={sender.busy}
      onClick={() => priceText && void sender.send({
        title: m.buyTitle(id, priceText),
        action: callAction(abi, contract, 'buy', [idArg(token.id)], [{ asset: listing.asset, amount: listing.price }]),
        details: [
          { label: m.token, value: `#${id}` },
          { label: m.seller, value: accountText(token.owner, conn?.network, sellerIsContract) },
          { label: m.price, value: priceText },
        ],
      })}>{priceText ? m.buyFor(priceText) : m.buy}</Button>
    {wallet && (balance === null || !meta ? <span className="field-hint">…</span>
      : enough ? <span className="field-hint">{m.youHave(formatUnits(balance, meta.decimals), meta.name)}</span>
        : <span className="field-error">{m.notEnough(formatUnits(balance, meta.decimals), meta.name)}</span>)}
    {sender.error && <Banner tone="danger">{sender.error}</Banner>}
  </div>;
}

/** A token in the marketplace: its seller and price, and Buy for anyone but the seller */
export function ListingCard({ contract, abi, token, me }: { contract: string; abi: Abi; token: Token; me: Uint8Array | null }) {
  const m = useT().nft;
  const conn = useConnection();
  const mine = sameAccount(token.owner, me);
  return <article className="nft-card">
    <div className="nft-card-head">
      <TokenBadge id={token.id} />
      {mine && <span className="pill pill--quiet">{m.yourListing}</span>}
    </div>
    <Uri uri={token.uri} />
    <div className="nft-facts">
      <Fact label={m.seller}><AccountText value={token.owner} network={conn?.network} chars={6} /></Fact>
      <Fact label={m.price}>{token.listing ? <Price value={token.listing.price} asset={toHex(token.listing.asset)} /> : <span className="muted">{m.notForSale}</span>}</Fact>
    </div>
    {mine ? <p className="field-hint">{m.manageInYours}</p> : <BuyPanel contract={contract} abi={abi} token={token} />}
  </article>;
}

/** A token in the list of every token: who holds it and its price; Details opens it with what you may do */
export function BrowseCard({ token, me, onOpen }: { token: Token; me: Uint8Array | null; onOpen: (id: bigint) => void }) {
  const m = useT().nft;
  const conn = useConnection();
  return <article className="nft-card">
    <div className="nft-card-head">
      <TokenBadge id={token.id} />
      <span className="nft-badges">{token.listing && <span className="pill pill--ok">{m.forSale}</span>}{sameAccount(token.owner, me) && <span className="pill pill--quiet">{m.youOwn}</span>}</span>
    </div>
    <Uri uri={token.uri} />
    <div className="nft-facts">
      <Fact label={m.owner}><AccountText value={token.owner} network={conn?.network} chars={6} /></Fact>
      {token.listing && <Fact label={m.price}><Price value={token.listing.price} asset={toHex(token.listing.asset)} /></Fact>}
    </div>
    <div className="method-actions"><Button size="sm" variant="ghost" onClick={() => onOpen(token.id)}>{m.details}</Button></div>
  </article>;
}
