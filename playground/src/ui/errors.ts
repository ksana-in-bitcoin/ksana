/** What an error says to the person, in their language. */
import { RpcError, RpcTransportError } from '@ksana/sdk';
import { AmountError } from '../chain/assets';
import { CompileServiceError } from '../compile/client';
import { UnsupportedNetworkError, currentConnection } from '../chain/connection';
import type { IdProblem } from '../chain/ids';
import { PlaygroundError } from '../chain/ops';
import { CancelledError, OpFailedError } from '../chain/run';
import { ViewError, failureText } from '../chain/views';
import { WalletError, isWalletRejection } from '../chain/wallet';
import type { Messages } from '../i18n';

export function errorText(error: unknown, m: Messages): string {
  const core = m.core;
  if (error instanceof CancelledError) return '';
  if (error instanceof PlaygroundError) {
    if (error.problem === 'walletNetwork') return core.errors.walletNetwork(error.detail);
    return core.errors[error.problem];
  }
  if (error instanceof WalletError) {
    if (error.reason === 'notTaproot') return core.wallet.errors.notTaproot(error.detail);
    if (error.reason === 'wrongNetwork') {
      const chain = currentConnection()?.chain;
      return core.wallet.errors.wrongNetwork(error.detail, chain ? core.network[chain] : '?');
    }
    return core.wallet.errors.noKey;
  }
  if (error instanceof UnsupportedNetworkError) return core.network.unsupported(error.chainId);
  if (error instanceof OpFailedError) return core.methods.failed(failureText(error.activity.status?.receipt?.error));
  if (error instanceof ViewError) return failureText(error.message);
  if (error instanceof AmountError) {
    return error.problem === 'decimals' ? core.form.decimals(error.decimals) : core.form[error.problem];
  }
  if (error instanceof CompileServiceError) {
    if (error.status === 429) return core.code.tooMany;
    if (error.status === 503) return core.code.busy;
    if (error.status === 413) return core.code.tooLarge;
    return error.message || core.code.serviceDown;
  }
  if (error instanceof RpcError) return error.message;
  if (error instanceof RpcTransportError) return `${core.network.error}: ${error.message}`;
  const message = error instanceof Error ? error.message : String(error);
  if (isWalletRejection(message)) return core.wallet.errors.rejected;
  return message;
}

/** An ID that names the wrong thing, in words: what it names instead, and the asset it leads to */
export function idProblemText(m: Messages['core']['ids'], p: IdProblem): string {
  const id = `${p.id.slice(0, 8)}…`;
  const issues = p.asset ? m.issues(p.asset.name) : '';
  switch (p.kind) {
    case 'assetIsContract':
      return `${m.assetIsContract(id)}${issues}`;
    case 'assetIsToken':
      return m.assetIsToken(id);
    case 'assetUnknown':
      return m.assetUnknown(id);
    case 'tokenIsAsset':
      return m.tokenIsAsset(id, p.asset?.name ?? id);
    case 'tokenIsNft':
      return m.tokenIsNft(id);
    case 'notToken':
      return `${m.notToken(id)}${issues}`;
    case 'tokenMissing':
      return m.tokenMissing(id);
  }
}
