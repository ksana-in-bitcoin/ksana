/**
 * Wallet demo page (M6 acceptance 2, D-055; path B since D-078): connects Unisat or Xverse, queries the account and the
 * suggested fee, has the wallet sign the VmOp with BIP322, submits a KSA transfer paying the fee in KSA (`FeeMode::Token`), and
 * tracks the operation and settlement status.
 *
 * `npm run build:demo` bundles it with esbuild into `examples/wallet-demo/dist/wallet-demo.js`; see the README.md in this
 * directory for usage.
 * The account is the wallet's Taproot address (D-092): the page shows the address and its hex form (the account), and the
 * recipient account can be given as a Taproot address or 64 hex digits.
 * The network chosen is checked against the node's chain ID before signing (`getParams`, A2-11, D-106).
 * Page URL parameters: `rpc` and `chain` (`devnet` | `signet`) prefill the form; `mock=p2tr&key=<private key>[&network=regtest]`
 * injects a mock Unisat wallet (for local regtest testing and `test/e2e/demo-page.ts` only).
 */

import {
  type BtcNetwork,
  DEVNET,
  KsanaRpcClient,
  MockUnisatWallet,
  type OpStatus,
  type Params,
  SIGNET,
  UnisatAdapter,
  VMOP_VERSION,
  type WalletAccount,
  type WalletAdapter,
  XverseAdapter,
  btcNetworkOf,
  equalBytes,
  fromHex,
  ksaAssetId,
  opId,
  parseAccount,
  signVmopWithWallet,
  toHex,
} from '../../src/index.js';

function byId<T extends HTMLElement>(id: string): T {
  const el = document.getElementById(id);
  if (el === null) throw new Error(`The page has no #${id}`);
  return el as T;
}
const field = (id: string): HTMLInputElement => byId<HTMLInputElement>(id);
const button = (id: string): HTMLButtonElement => byId<HTMLButtonElement>(id);
const show = (id: string, value: string): void => {
  byId(id).textContent = value;
};

let wallet: WalletAdapter | null = null;
let account: WalletAccount | null = null;
/** The operation currently tracked; incremented when a new operation is submitted, which stops the old polling. */
let tracking = 0;

function log(message: string): void {
  const el = byId<HTMLPreElement>('log');
  el.textContent = `${new Date().toLocaleTimeString()}  ${message}\n${el.textContent ?? ''}`;
}

function errorText(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

function params(): Params {
  return byId<HTMLSelectElement>('chain').value === 'signet' ? SIGNET : DEVNET;
}

function rpc(): KsanaRpcClient {
  return new KsanaRpcClient(field('rpc').value.trim());
}

function connected(): { wallet: WalletAdapter; account: WalletAccount } {
  if (wallet === null || account === null) throw new Error('Connect a wallet first');
  return { wallet, account };
}

function integer(id: string, what: string): bigint {
  const text = field(id).value.trim();
  if (!/^[0-9]+$/.test(text)) throw new Error(`${what} must be a non-negative integer`);
  return BigInt(text);
}

/** `mock=p2tr&key=<private key>` in the page URL: injects a mock Unisat wallet (Taproot address, D-092). */
function installMockWallet(query: URLSearchParams): void {
  const type = query.get('mock');
  if (type === null) return;
  if (type !== 'p2tr') throw new Error('mock must be p2tr (since D-092 accounts are Taproot addresses only)');
  const key = query.get('key');
  if (key === null) throw new Error('mock requires the key parameter (hex of a 32-byte private key)');
  const network = (query.get('network') ?? 'regtest') as BtcNetwork;
  if (!['mainnet', 'testnet', 'signet', 'regtest'].includes(network)) throw new Error('invalid network parameter');
  (globalThis as { unisat?: unknown }).unisat = new MockUnisatWallet({
    secretKey: fromHex(key, 32, 'private key'),
    network,
  });
  byId('mock-banner').hidden = false;
}

async function connect(kind: 'unisat' | 'xverse'): Promise<void> {
  const adapter = kind === 'unisat' ? new UnisatAdapter() : new XverseAdapter({ message: 'Ksana wallet demo' });
  const acc = await adapter.getAccount();
  wallet = adapter;
  account = acc;
  show('wallet-name', adapter.name);
  show('wallet-address', acc.address);
  show('wallet-account', acc.account);
  for (const id of ['refresh', 'estimate', 'submit']) button(id).disabled = false;
  log(`Connected ${adapter.name}: ${acc.address} (account ${acc.account}, the same bytes in hex)`);
  await refresh();
}

/** The account's nonce and available KSA balance (also shown on the page). */
async function refresh(): Promise<{ nonce: bigint; balance: bigint }> {
  const { account: acc } = connected();
  const info = await rpc().getAccount(acc.account);
  const balance = info.balances.find((b) => equalBytes(b.asset, ksaAssetId()))?.amount ?? 0n;
  show('nonce', info.nonce.toString());
  show('balance', balance.toString());
  return { nonce: info.nonce, balance };
}

/** The suggested path B fee from `ksana_estimateFee`, filled into the form. */
async function estimate(): Promise<bigint> {
  const fee = await rpc().estimateFee();
  field('fee').value = fee.path_b.amount.toString();
  show(
    'fee-info',
    `Suggested fee ${fee.path_b.amount} (KSA base units, based on the chain tip at height ${fee.at.height}); ` +
      `settlement transaction fee rate ${fee.feerate_sat_vb} sat/vB, sponsored by the producer`,
  );
  log(`Suggested fee ${fee.path_b.amount}`);
  return fee.path_b.amount;
}

async function submit(): Promise<void> {
  const { wallet: w, account: acc } = connected();
  // A node on another network than the one chosen is refused before anything is signed
  const network = await rpc().getParams(params());
  const action = {
    kind: 'AssetTransfer' as const,
    asset: ksaAssetId(),
    to: parseAccount(field('to').value, btcNetworkOf(network)),
    amount: integer('amount', 'amount'),
  };
  const maxGas = integer('max-gas', 'max_gas');
  const fee = field('fee').value.trim() === '' ? await estimate() : integer('fee', 'fee');
  const { nonce, balance } = await refresh();
  // Both the amount and the fee are paid from the KSA balance: an operation with an insufficient balance is rejected by the node,
  // or fails at execution with the fee still charged
  if (balance < action.amount + fee) {
    throw new Error(`KSA balance ${balance} cannot cover amount ${action.amount} plus fee ${fee}`);
  }
  log('Confirm the BIP322 message signature in the wallet (the message is the hex of the VmOp sighash)');
  const op = await signVmopWithWallet(
    {
      version: VMOP_VERSION,
      chain_id: network.chain_id,
      account: fromHex(acc.account, 32, 'wallet account'),
      nonce,
      fee: { kind: 'Token', amount: fee },
      max_gas: maxGas,
      action,
    },
    w,
  );
  const id = opId(op);
  const { op_id } = await rpc().submitOp(op);
  if (!equalBytes(op_id, id)) throw new Error(`op_id ${toHex(op_id)} returned by the node differs from the locally computed ${toHex(id)}`);
  show('op-id', toHex(id));
  show('op-status', 'pending');
  show('op-block', '—');
  show('settlement', '—');
  log(`Submitted: op_id ${toHex(id)} (nonce ${nonce}, fee ${fee})`);
  track(id);
}

function showStatus(status: OpStatus): void {
  show('op-status', status.status);
  if (status.block !== null) {
    show('op-block', `${toHex(status.block.hash)} (accepted at height ${status.block.height})`);
  }
}

/**
 * The settlement transaction of the block that included the operation (queried by block hash: merged blocks each carry their own
 * settlement transaction, D-063; the body of a block without a settlement transaction is carried by the settlement transaction
 * of some later block, D-077).
 */
async function showSettlement(block: Uint8Array): Promise<void> {
  const s = await rpc().getSettlement(block);
  const confirmations = s.confirmed === null ? 'unconfirmed' : `${s.confirmed.confirmations} confirmations`;
  const carrier = s.carrier === null ? '' : `, carried by the block at height ${s.carrier.height}`;
  show('settlement', `${s.txid_rpc ?? 'not yet carried by a settlement transaction'} (${s.status}, ${confirmations}${carrier})`);
}

/**
 * Queries the operation and settlement status every 2 seconds until the operation is BTC-final or failed. `skipped` (included
 * only by a merged block and not executed) is not a final state: an operation that is still valid returns to the entry pool and
 * is included again later (D-063).
 */
function track(id: Uint8Array): void {
  const run = ++tracking;
  let last = '';
  const tick = async (): Promise<void> => {
    if (run !== tracking) return;
    try {
      const status = await rpc().getOpStatus(id);
      showStatus(status);
      if (status.status !== last) {
        last = status.status;
        log(`Operation status: ${status.status}`);
        await refresh();
      }
      if (status.block !== null) await showSettlement(status.block.hash);
      if (status.status === 'btc_final' || status.status === 'failed') return;
    } catch (error) {
      log(`Status query failed: ${errorText(error)}`);
    }
    setTimeout(() => void tick(), 2000);
  };
  void tick();
}

/** Runs `action` on click, disables the button while it runs, and logs errors. */
function onClick(id: string, action: () => Promise<unknown>): void {
  const el = button(id);
  el.addEventListener('click', () => {
    el.disabled = true;
    action()
      .catch((error: unknown) => log(`Error: ${errorText(error)}`))
      .finally(() => {
        el.disabled = false;
      });
  });
}

function init(): void {
  const query = new URLSearchParams(location.search);
  const rpcUrl = query.get('rpc');
  if (rpcUrl !== null) field('rpc').value = rpcUrl;
  const chain = query.get('chain');
  if (chain === 'devnet' || chain === 'signet') byId<HTMLSelectElement>('chain').value = chain;
  try {
    installMockWallet(query);
  } catch (error) {
    log(`Mock wallet: ${errorText(error)}`);
  }
  onClick('connect-unisat', () => connect('unisat'));
  onClick('connect-xverse', () => connect('xverse'));
  onClick('refresh', refresh);
  onClick('estimate', estimate);
  onClick('submit', submit);
  log('Page loaded');
}

init();
