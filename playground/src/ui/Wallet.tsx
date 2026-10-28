/** The wallet in the header: connecting one, and the connected account's balances and the faucet. */
import { useState } from 'react';
import { useAccount } from '../chain/account';
import { KSA_HEX, formatUnits } from '../chain/assets';
import { useConnection } from '../chain/connection';
import { endpoints, explorerLink } from '../chain/deployment';
import { claimFromFaucet } from '../chain/faucet';
import { connectWallet, disconnectWallet, useWallet, walletOptions, type WalletKind } from '../chain/wallet';
import { useT } from '../i18n';
import { errorText } from './errors';
import { Banner, Button, CopyButton, Empty, Modal, Rows } from './ui';
import { Amount, AssetName } from './values';

export function WalletButton() {
  const t = useT();
  const m = t.core.wallet;
  const conn = useConnection();
  const wallet = useWallet();
  const account = useAccount();
  const [open, setOpen] = useState(false);
  if (!wallet) {
    return <>
      <Button variant="primary" size="sm" disabled={!conn} onClick={() => setOpen(true)}>{m.connect}</Button>
      <ConnectDialog open={open} onClose={() => setOpen(false)} />
    </>;
  }
  const ksa = account?.balances.get(KSA_HEX) ?? null;
  const address = wallet.account.address;
  return <>
    <button type="button" className="wallet-chip" onClick={() => setOpen(true)} title={address}>
      <span className="wallet-dot" aria-hidden="true" />
      <span className="mono">{address.slice(0, 6)}…{address.slice(-4)}</span>
      {ksa !== null && <span className="wallet-bal">{formatUnits(ksa, 8, 2)} KSA</span>}
    </button>
    <AccountDialog open={open} onClose={() => setOpen(false)} />
  </>;
}

/** Asks the wallet for the account (the wallet's own window asks the person) */
export function ConnectDialog({ open, onClose }: { open: boolean; onClose: () => void }) {
  const t = useT();
  const m = t.core.wallet;
  const conn = useConnection();
  const [busy, setBusy] = useState<WalletKind | null>(null);
  const [error, setError] = useState<string | null>(null);
  const options = conn ? walletOptions(conn) : [];
  const connect = async (kind: WalletKind) => {
    if (!conn) return;
    setBusy(kind);
    setError(null);
    try {
      await connectWallet(kind, conn);
      onClose();
    } catch (e) {
      setError(errorText(e, t));
    } finally {
      setBusy(null);
    }
  };
  return <Modal open={open} title={m.choose} onClose={onClose}>
    <p>{m.intro}</p>
    {!options.some((o) => o.available) && <Banner tone="warn">{m.noWallet}</Banner>}
    <div className="wallet-options">
      {options.map((o) => <Button key={o.kind} variant={o.available ? 'secondary' : 'ghost'} disabled={!o.available || busy !== null} busy={busy === o.kind}
        onClick={() => void connect(o.kind)}>
        {o.name}{!o.available && <span className="muted"> · {m.notInstalled}</span>}
      </Button>)}
    </div>
    {busy && <p className="muted">{m.connecting}</p>}
    {error && <Banner tone="danger">{error}</Banner>}
  </Modal>;
}

function AccountDialog({ open, onClose }: { open: boolean; onClose: () => void }) {
  const t = useT();
  const m = t.core.wallet;
  const wallet = useWallet();
  const account = useAccount();
  const conn = useConnection();
  const [faucet, setFaucet] = useState<'idle' | 'busy' | 'sent'>('idle');
  const [error, setError] = useState<string | null>(null);
  if (!wallet) return null;
  const faucetUrl = conn?.chain === 'signet' || conn?.chain === 'devnet' ? endpoints().faucet : '';
  const balances = account ? [...account.balances.entries()].filter(([, v]) => v > 0n) : [];
  balances.sort(([a], [b]) => (a === KSA_HEX ? -1 : b === KSA_HEX ? 1 : a.localeCompare(b)));
  const claim = async () => {
    setFaucet('busy');
    setError(null);
    try {
      await claimFromFaucet(faucetUrl, wallet.account.account);
      setFaucet('sent');
    } catch (e) {
      setFaucet('idle');
      setError(errorText(e, t));
    }
  };
  const link = explorerLink('account', wallet.account.account);
  return <Modal open={open} title={wallet.name} onClose={onClose}
    footer={<><Button variant="ghost" onClick={() => { disconnectWallet(); onClose(); }}>{m.disconnect}</Button><Button onClick={onClose}>{t.core.common.close}</Button></>}>
    <Rows rows={[
      { label: m.account, value: <span className="id"><span className="mono break">{wallet.account.address}</span><CopyButton text={wallet.account.address} label={m.copy} /></span> },
      link ? { label: t.core.nav.explorer, value: <a href={link} target="_blank" rel="noreferrer">{t.core.activity.explorer}</a> } : null,
    ]} />
    <h3 className="sub-title">{m.balance}</h3>
    {balances.length === 0 ? <Empty>0 KSA</Empty> : <ul className="balances">
      {balances.map(([asset, amount]) => <li key={asset}><AssetName asset={asset} /><Amount value={amount} asset={asset} /></li>)}
    </ul>}
    {faucetUrl && <div className="faucet">
      <Button variant="primary" disabled={faucet !== 'idle'} busy={faucet === 'busy'} onClick={() => void claim()}>
        {faucet === 'busy' ? m.faucetBusy : m.faucet}
      </Button>
      <p className="field-hint">{faucet === 'sent' ? m.faucetSent : m.faucetNote}</p>
    </div>}
    {error && <Banner tone="danger">{error}</Banner>}
  </Modal>;
}
