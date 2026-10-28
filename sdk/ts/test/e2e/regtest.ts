/**
 * M6 end-to-end: the SDK and the mock wallet (the Unisat injected API, called through `UnisatAdapter`) submit path B and path C
 * operations on regtest. Called by `tests/integration/tests/m6_regtest.rs` and `m13_regtest.rs`; the node, the funds and the
 * verification of the results are all on the Rust side:
 *
 *   node dist/test/e2e/regtest.js '<JSON input>'
 *
 * Input (`amount`, `fee` and `utxo.value` are decimal strings; the wallets are all Taproot addresses, D-092; `to` is the
 * account's 64 hex digits or a regtest Taproot address). The network is the node's (`getParams`, A2-11, D-106): `chain`, when
 * given, is the network expected, and a node on another one is refused before anything is signed.
 * - path B: `{ command: "path-b", rpc, chain?: "devnet" | "signet", secret_key, to, amount, fee }`, where `fee` is the KSA
 *   amount of the sponsored fee (`FeeMode::Token`); the account must have enough KSA;
 * - path C: `{ command: "path-c", rpc, chain?, secret_key, utxo: { txid (display byte order), vout, value }, to, amount,
 *   fee_rate? (sat/vB) }`.
 * - top-up (D-078, D-085): `{ command: "top-up", rpc, chain?, secret_key, utxo, exchange, seller_secret_key,
 *   ksa, price, expiry?, fee, sats, fee_rate? }`: first the seller's mock wallet calls the exchange contract's `lock` over path B
 *   (locking `ksa` for the buyer's account at `price` KSA base units per sat, paying to the seller wallet's address, with expiry
 *   height `expiry`, which defaults to the BTC height anchored by the node's latest block + 100, and a sponsored fee of `fee` KSA),
 *   then the buyer reads the lock and builds the payment + claim native transaction (the claim operation is signed by a one-time
 *   key); the buyer's wallet only signs this BTC transaction.
 * The path B / C VmOps are both KSA `AssetTransfer`s, signed by the wallet with BIP322.
 * Output (the last line of stdout, JSON): path B gives `{ op_id, nonce, status, block }` (after submission it waits until the
 * operation is no longer pending / skipped; `block` is `{ height, hash }` or `null`); path C gives `{ op_id, raw_tx, fee }` (the
 * caller broadcasts the raw transaction); top-up gives
 * `{ op_id, raw_tx, fee, vsize, ksa, one_time_account, beneficiary, lock, lock_op_id, seller, seller_script, expiry }`
 * (the caller broadcasts the raw transaction).
 */

import {
  type Action,
  ByteReader,
  KsanaRpcClient,
  MockUnisatWallet,
  NETWORKS,
  type OpStatus,
  type Params,
  UnisatAdapter,
  VMOP_VERSION,
  buildNativeTx,
  buildTopUp,
  equalBytes,
  extractTransaction,
  fromHex,
  getLock,
  ksaAssetId,
  lockArgs,
  opId,
  parseAccount,
  signVmopWithWallet,
  toHex,
  txidFromRpc,
} from '../../src/index.js';

interface CommonInput {
  rpc: string;
  chain?: 'devnet' | 'signet';
  secret_key: string;
  to: string;
  amount: string;
}

/** Path B: the fee is in KSA. */
interface PathBInput extends CommonInput {
  command: 'path-b';
  fee: string;
}

/** Path C: the native transaction spends one of the wallet's UTXOs. */
interface PathCInput extends CommonInput {
  command: 'path-c';
  utxo: { txid: string; vout: number; value: string };
  fee_rate?: number;
}

/** Top-up: the seller locks KSA for the buyer, the buyer pays the lock and claims the KSA (D-078, D-085). */
interface TopUpInput {
  command: 'top-up';
  rpc: string;
  chain?: 'devnet' | 'signet';
  /** Buyer wallet: funds the payment; the beneficiary account is its account. */
  secret_key: string;
  utxo: { txid: string; vout: number; value: string };
  exchange: string;
  /** Seller wallet: calls `lock` over path B; the payment goes to its address. */
  seller_secret_key: string;
  /** The locked KSA (base units) and the price (how many KSA base units per sat). */
  ksa: string;
  price: string;
  /** The BTC height at which the lock expires; defaults to the height anchored by the node's latest block + 100. */
  expiry?: number;
  /** The sponsored fee (KSA) of the seller's lock operation. */
  fee: string;
  /** The sats the buyer pays. */
  sats: string;
  fee_rate?: number;
}

type Input = PathBInput | PathCInput | TopUpInput;

const MAX_GAS = 100_000n;
/** Gas limit for calling the exchange contract's `lock` (wasm contract execution). */
const LOCK_MAX_GAS = 5_000_000n;

function parseInput(): Input {
  const text = process.argv[2];
  if (text === undefined) throw new Error('Usage: node dist/test/e2e/regtest.js <JSON input>');
  const input = JSON.parse(text) as Input;
  if (input.command !== 'path-b' && input.command !== 'path-c' && input.command !== 'top-up') {
    throw new Error(`unknown command ${JSON.stringify((input as { command?: unknown }).command)} (expected path-b, path-c or top-up)`);
  }
  return input;
}

async function waitIncluded(rpc: KsanaRpcClient, id: Uint8Array, timeoutMs = 60_000): Promise<OpStatus> {
  const deadline = Date.now() + timeoutMs;
  for (;;) {
    const status = await rpc.getOpStatus(id);
    // skipped: included only by a merged block and not executed; an operation that is still valid returns to the entry pool and
    // is included again later (D-063)
    if (status.status !== 'pending' && status.status !== 'skipped') return status;
    if (Date.now() > deadline) throw new Error(`timed out waiting for operation ${toHex(id)} to be included`);
    await new Promise((resolve) => setTimeout(resolve, 200));
  }
}

async function main(): Promise<void> {
  const input = parseInput();
  const rpc = new KsanaRpcClient(input.rpc);
  const params = await rpc.getParams(input.chain === undefined ? undefined : NETWORKS[input.chain]);
  const mock = new MockUnisatWallet({
    secretKey: fromHex(input.secret_key, 32, 'secret_key'),
    network: 'regtest',
  });
  const wallet = new UnisatAdapter(mock);
  if (input.command === 'top-up') {
    await topUp(input, params, rpc, mock, wallet);
    return;
  }
  const action: Action = {
    kind: 'AssetTransfer',
    asset: ksaAssetId(),
    to: parseAccount(input.to, 'regtest'),
    amount: BigInt(input.amount),
  };

  if (input.command === 'path-b') {
    // Path B: the wallet signs the VmOp with BIP322, the fee is in KSA (the producer sponsors the BTC fee), submitted directly to
    // the node
    const account = await wallet.getAccount();
    const accountKey = fromHex(account.account, 32, 'wallet account');
    const { nonce } = await rpc.getAccount(accountKey);
    const op = await signVmopWithWallet(
      {
        version: VMOP_VERSION,
        chain_id: params.chain_id,
        account: accountKey,
        nonce,
        fee: { kind: 'Token', amount: BigInt(input.fee) },
        max_gas: MAX_GAS,
        action,
      },
      wallet,
    );
    const id = opId(op);
    const { op_id } = await rpc.submitOp(op);
    if (!equalBytes(op_id, id)) throw new Error(`op_id ${toHex(op_id)} returned by the node differs from the locally computed ${toHex(id)}`);
    const status = await waitIncluded(rpc, id);
    console.log(
      JSON.stringify({
        op_id: toHex(id),
        nonce: op.nonce.toString(),
        status: status.status,
        block: status.block === null ? null : { height: status.block.height.toString(), hash: toHex(status.block.hash) },
      }),
    );
    return;
  }

  const outpoint = { txid: txidFromRpc(input.utxo.txid), vout: input.utxo.vout };
  const value = BigInt(input.utxo.value);

  // Path C: the wallet likewise signs the VmOp with BIP322, with the signature bound to the native transaction's first input
  // (D-086); the wallet signs the native transaction (SIGHASH_DEFAULT)
  const account = await wallet.getAccount();
  const accountKey = fromHex(account.account, 32, 'wallet account');
  const { nonce } = await rpc.getAccount(accountKey);
  const op = await signVmopWithWallet(
    {
      version: VMOP_VERSION,
      chain_id: params.chain_id,
      account: accountKey,
      nonce,
      fee: { kind: 'BtcNative' },
      max_gas: MAX_GAS,
      action,
    },
    wallet,
    { firstInput: outpoint },
  );
  const draft = buildNativeTx({
    op,
    utxos: [
      {
        txid: outpoint.txid,
        vout: outpoint.vout,
        value,
        script: mock.script,
        tapInternalKey: fromHex(account.internalKey, 32, 'internal key'),
      },
    ],
    changeScript: mock.script,
    feeRate: input.fee_rate ?? 2,
    params,
  });
  const signed = await wallet.signPsbt(
    draft.psbt,
    draft.inputs.map((_, index) => ({ index })),
  );
  console.log(
    JSON.stringify({ op_id: toHex(opId(op)), raw_tx: toHex(extractTransaction(signed)), fee: draft.fee.toString() }),
  );
}

/** The BTC height anchored by the node's latest block. */
async function anchorHeight(rpc: KsanaRpcClient): Promise<number> {
  return (await rpc.getBlock('latest')).header.btc_anchor.height;
}

/**
 * Top-up (D-078, D-085): the seller's mock wallet locks KSA for the buyer over path B (lock → until expiry only the buyer's
 * payment can move it), the buyer reads the lock and builds the payment + claim native transaction, and the raw transaction is
 * printed after the wallet signs it.
 */
async function topUp(
  input: TopUpInput,
  params: Params,
  rpc: KsanaRpcClient,
  mock: MockUnisatWallet,
  wallet: UnisatAdapter,
): Promise<void> {
  const buyer = await wallet.getAccount();
  const buyerAccount = fromHex(buyer.account, 32, 'wallet account');
  const exchange = fromHex(input.exchange, 32, 'exchange');

  // Seller: calls lock over path B (the seller wallet signs the VmOp with BIP322), attaching the KSA to lock
  const sellerMock = new MockUnisatWallet({
    secretKey: fromHex(input.seller_secret_key, 32, 'seller_secret_key'),
    network: 'regtest',
  });
  const sellerWallet = new UnisatAdapter(sellerMock);
  const sellerAccount = fromHex((await sellerWallet.getAccount()).account, 32, 'seller account');
  const ksa = BigInt(input.ksa);
  const expiry = input.expiry ?? (await anchorHeight(rpc)) + 100;
  const { nonce } = await rpc.getAccount(sellerAccount);
  const lockOp = await signVmopWithWallet(
    {
      version: VMOP_VERSION,
      chain_id: params.chain_id,
      account: sellerAccount,
      nonce,
      fee: { kind: 'Token', amount: BigInt(input.fee) },
      max_gas: LOCK_MAX_GAS,
      action: {
        kind: 'Call',
        contract: exchange,
        method: 'lock',
        args: lockArgs({ ksa, price: BigInt(input.price), script: sellerMock.script, beneficiary: buyerAccount, expiry }),
        funds: [{ asset: ksaAssetId(), amount: ksa }],
      },
    },
    sellerWallet,
  );
  const lockOpId = opId(lockOp);
  await rpc.submitOp(lockOp);
  const lockStatus = await waitIncluded(rpc, lockOpId);
  if (lockStatus.status === 'failed' || lockStatus.receipt === null || !lockStatus.receipt.success) {
    throw new Error(`lock operation ${toHex(lockOpId)} failed: ${lockStatus.status} ${lockStatus.receipt?.error ?? ''}`);
  }
  const reader = new ByteReader(lockStatus.receipt.output, 'lock ID');
  const lockId = reader.u64();
  reader.finish();

  // Buyer: reads the lock, verifies the beneficiary account, builds the payment + claim native transaction
  const lock = await getLock(rpc, exchange, lockId);
  const draft = buildTopUp({
    exchange,
    lockId,
    lock,
    beneficiary: buyerAccount,
    sats: BigInt(input.sats),
    currentBtcHeight: await anchorHeight(rpc),
    utxos: [
      {
        txid: txidFromRpc(input.utxo.txid),
        vout: input.utxo.vout,
        value: BigInt(input.utxo.value),
        script: mock.script,
        tapInternalKey: fromHex(buyer.internalKey, 32, 'internal key'),
      },
    ],
    changeScript: mock.script,
    feeRate: input.fee_rate ?? 2,
    params,
  });
  const signed = await wallet.signPsbt(
    draft.psbt,
    draft.inputs.map((_, index) => ({ index })),
  );
  console.log(
    JSON.stringify({
      op_id: toHex(draft.opId),
      raw_tx: toHex(extractTransaction(signed)),
      fee: draft.fee.toString(),
      vsize: draft.vsize,
      ksa: draft.ksa.toString(),
      one_time_account: toHex(draft.op.account),
      beneficiary: toHex(lock.beneficiary),
      lock: lockId.toString(),
      lock_op_id: toHex(lockOpId),
      seller: toHex(sellerAccount),
      seller_script: toHex(lock.script),
      expiry: lock.expiry,
    }),
  );
}

main().catch((error: unknown) => {
  console.error(error);
  process.exitCode = 1;
});
