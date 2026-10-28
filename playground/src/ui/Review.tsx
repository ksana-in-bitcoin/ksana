/** The review every operation is signed from: what it does, what it costs, and how a read-only run of it ends now. */
import { sha256, toHex } from '@ksana/sdk';
import { failureText } from '../chain/views';
import { KSA_HEX } from '../chain/assets';
import { explorerLink } from '../chain/deployment';
import { cancelReview, confirmReview, useReview } from '../chain/run';
import { useConnection } from '../chain/connection';
import { useWallet } from '../chain/wallet';
import { useT } from '../i18n';
import { errorText } from './errors';
import { Banner, Button, Id, Modal, Rows, Spinner } from './ui';
import { AccountText, Amount } from './values';

export function ReviewDialog() {
  const t = useT();
  const m = t.core.review;
  const review = useReview();
  const conn = useConnection();
  const wallet = useWallet();
  if (!review) return <Modal open={false} title="" onClose={cancelReview}>{null}</Modal>;
  const { request, phase, prepared, simulation, error } = review;
  const action = prepared?.unsigned.action ?? request.action;
  const busy = phase === 'signing';
  const simFailed = simulation !== undefined && !simulation.success;
  const footer = phase === 'failed'
    ? <Button onClick={cancelReview}>{m.close}</Button>
    : <>
      <Button variant="ghost" onClick={cancelReview} disabled={busy}>{m.cancel}</Button>
      <Button variant={simFailed ? 'danger' : 'primary'} onClick={() => void confirmReview()} disabled={phase !== 'ready'} busy={busy}>
        {busy ? m.signing : simFailed ? m.signAnyway : m.sign}
      </Button>
    </>;
  const kind = action.kind === 'Deploy' ? m.deploy : action.kind === 'Call' ? m.call : m.transfer;
  return <Modal open title={request.title} onClose={busy ? () => {} : cancelReview} footer={footer}>
    {phase === 'preparing' && <p className="muted"><Spinner /> {m.preparing}</p>}
    {phase === 'failed' && <Banner tone="danger" title={t.core.common.error}>{errorText(error, t)}</Banner>}
    {(phase === 'ready' || phase === 'signing' || (phase === 'failed' && prepared)) && <>
      <Rows rows={[
        { label: m.action, value: kind },
        action.kind === 'Deploy' && prepared && { label: m.newContract, value: <Id value={prepared.contract} /> },
        action.kind === 'Deploy' && { label: m.codeSize, value: t.core.deploy.size(action.code.length.toLocaleString()) },
        action.kind === 'Deploy' && { label: m.codeHash, value: <Id value={toHex(sha256(action.code))} /> },
        action.kind === 'Call' && { label: m.contract, value: <Id value={toHex(action.contract)} href={explorerLink('contract', toHex(action.contract))} /> },
        action.kind === 'Call' && { label: m.method, value: <span className="mono">{action.method}</span> },
        ...(request.details ?? []).map((d) => ({ label: d.label, value: <span className="break">{d.value}</span> })),
        action.kind !== 'AssetTransfer' && action.funds.length > 0 && {
          label: m.attached,
          value: <ul className="plain">{action.funds.map((f) => <li key={toHex(f.asset)}><Amount value={f.amount} asset={toHex(f.asset)} /></li>)}</ul>,
        },
        action.kind === 'AssetTransfer' && { label: m.amount, value: <Amount value={action.amount} asset={toHex(action.asset)} /> },
        action.kind === 'AssetTransfer' && { label: m.to, value: <AccountText value={action.to} network={conn?.network} /> },
        prepared && { label: m.fee, value: <Amount value={prepared.fee} asset={KSA_HEX} /> },
        prepared && { label: m.gas, value: <span className="mono">{prepared.unsigned.max_gas.toLocaleString()}</span> },
        prepared && { label: m.nonce, value: <span className="mono">{prepared.unsigned.nonce.toString()}</span> },
        wallet && { label: m.account, value: <span className="mono break">{wallet.account.address}</span> },
      ]} />
      {simulation && (simulation.success
        ? <Banner tone="ok">{m.simOk('')}</Banner>
        : <Banner tone="warn">{m.simFailed(failureText(simulation.error))}</Banner>)}
      <p className="field-hint">{m.feeNote}</p>
    </>}
  </Modal>;
}
