/**
 * Token and pool on a Ksana node, from the command line: deploys the `asset-token` example contract as a token (a VM built-in
 * asset, D-073), a pool of the `amm` example between two assets, adds liquidity and swaps. Every op is path B, signed with a
 * secret key file (BIP340, the same file as `ksana-keytool` and `ksana-cli`) and submitted with `ksana_submitOp`; each command
 * waits for its op to be executed and prints what the op did. The network is the node's (`getParams`, D-106).
 *
 * The node decides much of what gets signed, so it is checked where it can be: `--chain` refuses a node on another network,
 * `--max-fee` caps the fee it asks for, and `add` attaches no more than the amounts given whatever its quote says. The minimums
 * of `add` and `swap` still come from its quotes: use a node you trust, over HTTPS when it is on another machine. Text from the
 * chain, which anyone can write (asset names, revert reasons), is escaped before it is printed.
 *
 * `npm run build` compiles it to `dist/examples/token-swap/main.js`; the contracts are built in `contracts/` with
 * `cargo rustc --release --locked --target wasm32-unknown-unknown --package <asset-token | amm> --crate-type cdylib`.
 * See the README.md in this directory: the Wizards demo on the public signet ran these commands.
 */

import { readFileSync, writeFileSync, existsSync } from 'node:fs';
import { basename } from 'node:path';
import { parseArgs } from 'node:util';
import {
  type Action,
  type Funds,
  KsanaRpcClient,
  NETWORKS,
  type NetworkName,
  type OpStatus,
  type Params,
  type Receipt,
  RpcError,
  RpcTransportError,
  RPC_ERROR_CODES,
  VMOP_VERSION,
  accountOf,
  accountToAddress,
  btcNetworkOf,
  concatBytes,
  encodeVmOp,
  equalBytes,
  fromHex,
  isValidSecretKey,
  ksaAssetId,
  networkOfChainId,
  opId,
  parseAccount,
  randomBytes,
  sha256,
  signedVmop,
  toHex,
  txidToRpc,
  utf8,
} from '../../src/index.js';

const USAGE = `token-swap: a token and an AMM pool on a Ksana node (path B ops signed with a key file)

  key <file>                                            create (when missing) or read a secret key; print its account
  token   --key <file> --code <asset_token.wasm> --name <name> --symbol <symbol> --decimals <n> --supply <amount>
  pool    --key <file> --code <amm.wasm> --a <asset> --b <asset>
  add     --key <file> --pool <contract> --a <amount> --b <amount> [--slippage <percent> | --min-shares <shares>]
  swap    --key <file> --pool <contract> --in <asset> --amount <amount> [--slippage <percent>]
  balance <account>                                     balances with the assets' names
  settlement <op_id>...                                 the block of each op and the BTC transaction that settled it

  <asset> is KSA or an asset ID (64 hex digits); amounts are in whole units of the asset (decimals allowed).
  --rpc <url>          the node (default $KSANA_RPC or http://127.0.0.1:8545)
  --chain <network>    devnet, signet or mainnet (default $KSANA_CHAIN): refuse a node on another network
  --max-fee <KSA>      the most an op pays, whatever the node asks (default 0.01)
  --max-gas <gas>      the gas limit of each op (default 1000000)
  --slippage <percent> how far below its quote the shares of add and the output of swap may fall (default 1)

  Exit status: 0 done, 1 an error, 2 the op may still execute: check it with settlement before running a command again.`;

/** The `sub_id` under which the asset-token example issues its asset. */
const ASSET_SUB_ID = new Uint8Array(32);
/** KSA's decimals (its metadata at genesis). */
const KSA_DECIMALS = 8;
/** How often the op's status is read while waiting. */
const POLL_MS = 250;
/** How long a command waits for its op. */
const WAIT_MS = 180_000;
/** The exit status of a command that stopped waiting while its op may still execute. */
const EXIT_WAITING = 2;
/** What a pool of the `amm` example built before `add_liquidity` took a minimum answers to three arguments. */
const NO_MINIMUM = 'revert: malformed arguments';

// ---------------------------------------------------------------------------------------------
// Borsh of the contracts' arguments (docs of contracts/examples/*/src/lib.rs)

function u8(value: number): Uint8Array {
  return Uint8Array.of(value);
}

function u64le(value: bigint): Uint8Array {
  const out = new Uint8Array(8);
  new DataView(out.buffer).setBigUint64(0, value, true);
  return out;
}

function u128le(value: bigint): Uint8Array {
  if (value < 0n || value >= 1n << 128n) throw new Error(`${value} does not fit in a u128`);
  return concatBytes(u64le(value & 0xffff_ffff_ffff_ffffn), u64le(value >> 64n));
}

function readU128(bytes: Uint8Array, offset = 0): bigint {
  if (bytes.length < offset + 16) throw new Error('the output is too short for a u128');
  const view = new DataView(bytes.buffer, bytes.byteOffset + offset, 16);
  return view.getBigUint64(0, true) | (view.getBigUint64(8, true) << 64n);
}

function borshString(text: string): Uint8Array {
  const bytes = utf8(text);
  const len = new Uint8Array(4);
  new DataView(len.buffer).setUint32(0, bytes.length, true);
  return concatBytes(len, bytes);
}

/** One side of an AMM pair: a token contract (variant 0) or a built-in asset (variant 1). */
interface Side {
  kind: 'Token' | 'Asset';
  id: Uint8Array;
}

function encodeSide(side: Side): Uint8Array {
  return concatBytes(u8(side.kind === 'Token' ? 0 : 1), side.id);
}

function decodeSides(bytes: Uint8Array): [Side, Side] {
  if (bytes.length !== 66) throw new Error('the pool returned malformed sides');
  const side = (offset: number): Side => ({
    kind: bytes[offset] === 0 ? 'Token' : 'Asset',
    id: bytes.slice(offset + 1, offset + 33),
  });
  return [side(0), side(33)];
}

// ---------------------------------------------------------------------------------------------
// Identifiers (ABI §6)

/** `sha256(deployer ‖ nonce as u64 little-endian ‖ sha256(code))`. */
function contractId(deployer: Uint8Array, nonce: bigint, code: Uint8Array): Uint8Array {
  return sha256(concatBytes(deployer, u64le(nonce), sha256(code)));
}

/** `sha256(issuing contract ‖ sub_id)`. */
function assetId(issuer: Uint8Array, subId: Uint8Array): Uint8Array {
  return sha256(concatBytes(issuer, subId));
}

// ---------------------------------------------------------------------------------------------
// Amounts

/** `"1,234.5"` from base units and decimals. */
function formatUnits(value: bigint, decimals: number): string {
  const unit = 10n ** BigInt(decimals);
  const whole = (value / unit).toString().replace(/\B(?=(\d{3})+(?!\d))/g, ',');
  const frac = decimals === 0 ? '' : (value % unit).toString().padStart(decimals, '0').replace(/0+$/, '');
  return frac === '' ? whole : `${whole}.${frac}`;
}

/** Base units from `"1234.5"` and decimals. */
function parseUnits(text: string, decimals: number): bigint {
  const m = /^(\d+)(?:\.(\d+))?$/.exec(text.replace(/[,_]/g, ''));
  if (!m || (m[2] ?? '').length > decimals) throw new Error(`not an amount with at most ${decimals} decimals: ${text}`);
  const value = BigInt(m[1] ?? '0') * 10n ** BigInt(decimals) + BigInt((m[2] ?? '').padEnd(decimals, '0') || '0');
  if (value === 0n) throw new Error('the amount must be above 0');
  return value;
}

/** A whole number of pool shares. */
function parseShares(text: string): bigint {
  const digits = text.replace(/[,_]/g, '');
  if (!/^\d+$/.test(digits)) throw new Error(`--min-shares must be a whole number of shares: ${text}`);
  return BigInt(digits);
}

/** `--slippage` in basis points. */
function basisPoints(slippage: string): bigint {
  const percent = Number(slippage);
  if (!(percent >= 0 && percent < 100)) throw new Error('--slippage must be a percentage from 0 to 100');
  return BigInt(Math.round(percent * 100));
}

/** `expected` less `bps` basis points, rounded down, and at least 1. */
function atLeast(expected: bigint, bps: bigint): bigint {
  const minimum = (expected * (10_000n - bps)) / 10_000n;
  return minimum > 0n ? minimum : 1n;
}

// ---------------------------------------------------------------------------------------------
// Text from the chain

/**
 * `text` made safe for a terminal. Anyone can write text on chain (an asset's name, a revert reason), and its control
 * characters would reach the terminal as they are: they can move the cursor, rewrite the lines printed above and, in some
 * terminals, set the clipboard. Control, format, private-use and unassigned characters and every space but U+0020 become
 * `\u{…}`, `\` becomes `\\`, and text longer than `max` characters is cut.
 */
function printable(text: string, max: number): string {
  const chars = [...text];
  const kept = chars.length > max ? [...chars.slice(0, max), '…'] : chars;
  return kept
    .map((c) => (c === '\\' ? '\\\\' : c !== ' ' && /[\p{C}\p{Z}]/u.test(c) ? `\\u{${(c.codePointAt(0) ?? 0).toString(16)}}` : c))
    .join('');
}

// ---------------------------------------------------------------------------------------------
// The node

interface AssetMeta {
  id: Uint8Array;
  decimals: number;
  /** How it is printed: KSA alone, recognized by its ID, as `KSA`; any other asset by its name and the start of its ID. */
  label: string;
}

/** The parameters of `--chain`. */
function networkParams(name: string): Params {
  if (!Object.hasOwn(NETWORKS, name)) throw new Error(`--chain must be devnet, signet or mainnet, not ${name}`);
  return NETWORKS[name as NetworkName];
}

class KsanaNode {
  private readonly assets = new Map<string, AssetMeta>();

  private constructor(
    readonly rpc: KsanaRpcClient,
    readonly params: Params,
  ) {}

  /**
   * Connects to the node at `url` and reads its network; with `expected`, a node on another network is refused
   * (`ChainMismatchError`, D-106). Plain HTTP to another machine gets a warning: anyone on the way could change the answers
   * this tool signs by.
   */
  static async connect(url: string, expected: Params | undefined): Promise<KsanaNode> {
    const { protocol, hostname } = new URL(url);
    if (protocol === 'http:' && !/^(localhost|127(\.\d{1,3}){3}|\[::1\])$/.test(hostname)) {
      console.error(`warning: ${hostname} is reached over plain HTTP, so anyone on the way can change what the node answers; use https`);
    }
    const rpc = new KsanaRpcClient(url, { timeoutMs: 30_000 });
    return new KsanaNode(rpc, await rpc.getParams(expected));
  }

  address(account: Uint8Array): string {
    return accountToAddress(account, btcNetworkOf(this.params));
  }

  /** `KSA` or 64 hex digits. */
  parseAsset(text: string): Uint8Array {
    return text.toUpperCase() === 'KSA' ? ksaAssetId() : fromHex(text.replace(/^0x/, ''), 32, 'asset ID');
  }

  async asset(id: Uint8Array): Promise<AssetMeta> {
    const key = toHex(id);
    let meta = this.assets.get(key);
    if (meta === undefined) {
      if (equalBytes(id, ksaAssetId())) {
        meta = { id, decimals: KSA_DECIMALS, label: 'KSA' };
      } else {
        // Any contract may name its asset "KSA": every asset but KSA itself shows the start of its ID
        const info = await this.rpc.getAsset(id);
        const name = info.name === '' ? '' : `${printable(info.name, 32)} `;
        meta = { id, decimals: info.decimals, label: `${name}(${key.slice(0, 8)}…)` };
      }
      this.assets.set(key, meta);
    }
    return meta;
  }

  /**
   * The path B fee of an op of `encodedLen` bytes and `maxGas`: what the node asks by the cost rule of its entry pool, at most
   * `maxFee`. Nothing checks what the node asks, so a node asking more is refused before anything is signed. Paying above the
   * ask would not stay a margin: the ask follows the fee rates recent blocks took, so on a quiet chain an op paying twice it
   * doubles the next ask.
   */
  async fee(encodedLen: number, maxGas: bigint, maxFee: bigint): Promise<bigint> {
    const estimate = await this.rpc.estimateFee();
    const byGas = (maxGas * BigInt(this.params.max_block_data_bytes)) / this.params.block_gas_limit;
    const cost = BigInt(encodedLen) > byGas ? BigInt(encodedLen) : byGas;
    const byRate = (cost * estimate.path_b.per_kib + 1023n) / 1024n;
    const asked = estimate.path_b.amount > byRate ? estimate.path_b.amount : byRate;
    if (asked > maxFee) {
      throw new Error(`the node asks ${formatUnits(asked, KSA_DECIMALS)} KSA for this op, more than --max-fee ${formatUnits(maxFee, KSA_DECIMALS)} KSA`);
    }
    return asked > 0n ? asked : 1n;
  }
}

// ---------------------------------------------------------------------------------------------
// Keys and ops

interface Signer {
  secretKey: Uint8Array;
  account: Uint8Array;
}

/** The limits of every op: its gas, and the most it pays in fees. */
interface Limits {
  maxGas: bigint;
  maxFee: bigint;
}

function readKey(file: string): Signer {
  const secretKey = fromHex(readFileSync(file, 'utf8').trim(), 32, 'secret key');
  if (!isValidSecretKey(secretKey)) throw new Error(`${file} does not hold a valid secret key`);
  return { secretKey, account: accountOf(secretKey) };
}

/** Thrown when a command stops waiting while its op may still execute: not a failure (exit status `EXIT_WAITING`). */
class StillWaiting extends Error {}

/**
 * Prints what is known of an op still waiting when the wait ends, how to follow it and how to cancel it; returns the
 * `StillWaiting` to throw. `forgotten`: the node's latest answer was that it does not know the op, after it had known it.
 */
function stillWaiting(status: OpStatus | null, forgotten: boolean, id: Uint8Array, nonce: bigint, fee: bigint): StillWaiting {
  const wait = `${WAIT_MS / 1000} s`;
  const state =
    status === null
      ? `no status of the op from the node for ${wait}: it may still execute`
      : forgotten
        ? `the node no longer knows the op: it left the entry pool unexecuted, as when another op takes its nonce, and executes only if another node still holds it`
        : status.status === 'skipped'
          ? `skipped at block #${status.block?.height ?? '?'}, so not executed and not charged: the op returns to the entry pool while it stays valid, and may still execute`
          : `still in the entry pool after ${wait}: the op has not failed and may still execute`;
  console.log(`  …        ${state}`);
  console.log(`           check it with \`token-swap settlement ${toHex(id)}\`: once it executes, running this command again repeats it`);
  console.log(`           to cancel it, replace nonce ${nonce} with any op paying more than ${formatUnits(fee, KSA_DECIMALS)} KSA (ksana-cli takes --nonce)`);
  return new StillWaiting('the op is still waiting');
}

/**
 * Signs `action` as the next op of `signer`, submits it and waits until it is executed. An op the main chain only skipped goes
 * back to the entry pool while it stays valid (D-063), so it is waited for like a pending one, and so is an op whose submission
 * got no answer; when the wait ends first, the op may still execute, and `StillWaiting` is thrown after saying how to follow it.
 */
async function submit(node: KsanaNode, signer: Signer, action: Action, limits: Limits) {
  const account = await node.rpc.getAccount(signer.account);
  const unsigned = {
    version: VMOP_VERSION,
    chain_id: node.params.chain_id,
    account: signer.account,
    nonce: account.nonce,
    max_gas: limits.maxGas,
    action,
  };
  const draft = signedVmop({ ...unsigned, fee: { kind: 'Token', amount: 1n } }, signer.secretKey);
  const fee = await node.fee(encodeVmOp(draft).length, limits.maxGas, limits.maxFee);
  const op = signedVmop({ ...unsigned, fee: { kind: 'Token', amount: fee } }, signer.secretKey);
  const id = opId(op);
  console.log(`  op       ${toHex(id)}`);
  console.log(`           nonce ${op.nonce}, fee ${formatUnits(fee, KSA_DECIMALS)} KSA, ${encodeVmOp(op).length.toLocaleString('en-US')} bytes`);
  const started = Date.now();
  try {
    const { op_id } = await node.rpc.submitOp(op);
    if (!equalBytes(op_id, id)) throw new Error(`the node gave the op another ID, ${toHex(op_id)}`);
  } catch (e) {
    // A refusal is an answer; a lost answer leaves it to the status reads whether the op got in
    if (!(e instanceof RpcTransportError)) throw e;
  }
  let status: OpStatus | null = null;
  let forgotten = false;
  for (;;) {
    try {
      status = await node.rpc.getOpStatus(id);
      forgotten = false;
    } catch (e) {
      // A failed read says nothing about the op: the last status read stands, as in ksana-cli. An op the node knew and no
      // longer knows left its entry pool without being executed.
      if (e instanceof RpcError && e.code === RPC_ERROR_CODES.NOT_FOUND) forgotten = status !== null;
    }
    if (status !== null && status.status !== 'pending' && status.status !== 'skipped') break;
    if (Date.now() - started > WAIT_MS) throw stillWaiting(status, forgotten, id, op.nonce, fee);
    await new Promise((resolve) => setTimeout(resolve, POLL_MS));
  }
  const seconds = ((Date.now() - started) / 1000).toFixed(1);
  const height = status.block === null ? '?' : `#${status.block.height}`;
  if (status.status === 'failed' || status.receipt === null || !status.receipt.success) {
    throw new Error(`the op ${status.status} in block ${height}: ${status.receipt?.error ?? 'no receipt'}`);
  }
  console.log(`  ✓        executed in block ${height}, ${seconds} s after it was submitted`);
  return { op, opId: id, receipt: status.receipt, nonce: account.nonce };
}

// ---------------------------------------------------------------------------------------------
// Commands

async function cmdKey(file: string, network: () => Promise<Params>) {
  if (!existsSync(file)) {
    let secretKey: Uint8Array;
    do secretKey = randomBytes(32);
    while (!isValidSecretKey(secretKey));
    writeFileSync(file, `${toHex(secretKey)}\n`, { mode: 0o600, flag: 'wx' });
    console.log(`created ${file}`);
  }
  const { account } = readKey(file);
  console.log(`account  ${toHex(account)}`);
  const params = await network().catch((e: unknown) => {
    throw new Error(`the address needs --chain or a node: ${e instanceof Error ? e.message : String(e)}`);
  });
  console.log(`address  ${accountToAddress(account, btcNetworkOf(params))}`);
}

async function cmdToken(
  node: KsanaNode,
  signer: Signer,
  code: Uint8Array,
  codeName: string,
  token: { name: string; symbol: string; decimals: number },
  supply: string,
  limits: Limits,
) {
  const { name, symbol, decimals } = token;
  const units = parseUnits(supply, decimals);
  console.log(`Deploy ${codeName}: ${name} (${symbol}), supply ${formatUnits(units, decimals)} (${decimals} decimals)`);
  console.log(`  deployer ${node.address(signer.account)}`);
  const initArgs = concatBytes(borshString(name), borshString(symbol), u8(decimals), u128le(units));
  const { nonce } = await submit(node, signer, { kind: 'Deploy', code, init_args: initArgs, funds: [] }, limits);
  const contract = contractId(signer.account, nonce, code);
  const asset = assetId(contract, ASSET_SUB_ID);
  const info = await node.rpc.getAsset(asset);
  console.log(`  contract ${toHex(contract)}`);
  console.log(`  asset    ${toHex(asset)}`);
  console.log(
    `           ${printable(info.name, 32)} (${printable(info.symbol, 16)}): ${formatUnits(info.total_supply, info.decimals)} issued to the deployer`,
  );
}

async function cmdPool(node: KsanaNode, signer: Signer, code: Uint8Array, codeName: string, a: Uint8Array, b: Uint8Array, limits: Limits) {
  const [metaA, metaB] = [await node.asset(a), await node.asset(b)];
  console.log(`Deploy ${codeName}: a ${metaA.label} / ${metaB.label} pool`);
  const initArgs = concatBytes(encodeSide({ kind: 'Asset', id: a }), encodeSide({ kind: 'Asset', id: b }));
  const { nonce } = await submit(node, signer, { kind: 'Deploy', code, init_args: initArgs, funds: [] }, limits);
  console.log(`  pool     ${toHex(contractId(signer.account, nonce, code))}`);
}

/** The pool's two sides, which must both be built-in assets. */
async function poolAssets(node: KsanaNode, pool: Uint8Array): Promise<[AssetMeta, AssetMeta]> {
  const receipt = await node.rpc.call({ contract: pool, method: 'tokens', args: new Uint8Array() });
  if (!receipt.success) throw new Error(`reading the pool failed: ${receipt.error ?? ''}`);
  const [a, b] = decodeSides(receipt.output);
  if (a.kind !== 'Asset' || b.kind !== 'Asset') throw new Error('this tool only handles pools of two built-in assets');
  return [await node.asset(a.id), await node.asset(b.id)];
}

/** Attached funds in ascending asset ID order (D-073). */
function sortedFunds(funds: Funds[]): Funds[] {
  return [...funds].sort((x, y) => (toHex(x.asset) < toHex(y.asset) ? -1 : 1));
}

async function reserves(node: KsanaNode, pool: Uint8Array): Promise<[bigint, bigint]> {
  const receipt = await node.rpc.call({ contract: pool, method: 'reserves', args: new Uint8Array() });
  if (!receipt.success) throw new Error(`reading the reserves failed: ${receipt.error ?? ''}`);
  return [readU128(receipt.output, 0), readU128(receipt.output, 16)];
}

/** The shares and the amounts a deposit takes, from a receipt of `add_liquidity`: its output and its event. */
function depositOf(receipt: Receipt, pool: Uint8Array): { shares: bigint; taken: [bigint, bigint] } {
  // ("add_liquidity", provider, amount_a, amount_b, shares)
  const topic = borshString('add_liquidity');
  const event = receipt.events.find(
    (e) => equalBytes(e.contract, pool) && e.data.length === topic.length + 80 && equalBytes(e.data.subarray(0, topic.length), topic),
  );
  if (event === undefined) throw new Error('the quote has no add_liquidity event');
  const at = topic.length + 32;
  return { shares: readU128(receipt.output), taken: [readU128(event.data, at), readU128(event.data, at + 16)] };
}

/**
 * Refuses a deposit quote that takes none of an amount given, or more than it. The op attaches what the quote says the pool
 * takes, so without this the node, or the pool, would decide how much of the account's balance the op attaches. A pool of the
 * `amm` example takes some of each amount and never more: all of both in the first deposit, and in a later one what its ratio
 * takes, `ceil(shares · R / S)`, which the shares, rounded down, keep within the amount.
 */
function checkTaken(meta: AssetMeta, given: bigint, taken: bigint) {
  const amount = (value: bigint) => `${formatUnits(value, meta.decimals)} ${meta.label}`;
  if (taken === 0n) throw new Error(`the quote says the pool takes none of the ${amount(given)} given: an amm always takes some, so nothing was signed`);
  if (taken > given) {
    throw new Error(`the quote says the pool takes ${amount(taken)}, more than the ${amount(given)} given: an amm never does, so nothing was signed`);
  }
}

/**
 * Adds liquidity with a minimum of shares. The quote is the same deposit as a read-only call on the latest state, as this
 * account, and the op offers only the amounts the pool's ratio takes of those given, never more (`checkTaken`): amounts in that
 * ratio get fewer shares whichever way a swap just before the deposit moves the price, so the minimum stops such a deposit,
 * where the rest of an amount would be taken at the moved price. `--min-shares 0` sends no minimum, the two arguments a pool
 * built before it takes.
 */
async function cmdAdd(node: KsanaNode, signer: Signer, pool: Uint8Array, amountA: string, amountB: string, bps: bigint, minSharesFlag: string | undefined, limits: Limits) {
  const fixed = minSharesFlag === undefined ? undefined : parseShares(minSharesFlag);
  const withMinimum = fixed !== 0n;
  const [a, b] = await poolAssets(node, pool);
  const given: [bigint, bigint] = [parseUnits(amountA, a.decimals), parseUnits(amountB, b.decimals)];
  const args = (amounts: [bigint, bigint], minShares: bigint) =>
    concatBytes(u128le(amounts[0]), u128le(amounts[1]), withMinimum ? u128le(minShares) : new Uint8Array());
  const funds = (amounts: [bigint, bigint]) =>
    sortedFunds([
      { asset: a.id, amount: amounts[0] },
      { asset: b.id, amount: amounts[1] },
    ]);
  console.log(`Add liquidity: ${formatUnits(given[0], a.decimals)} ${a.label} + ${formatUnits(given[1], b.decimals)} ${b.label}`);
  const quote = await node.rpc.call({ contract: pool, method: 'add_liquidity', args: args(given, 0n), funds: funds(given), caller: signer.account });
  if (!quote.success) {
    if (withMinimum && quote.error === NO_MINIMUM) {
      throw new Error('the pool takes no minimum of shares, as an amm built before the minimum: add with --min-shares 0 to add without one');
    }
    throw new Error(`the quote failed: ${quote.error ?? ''}`);
  }
  const { shares, taken } = depositOf(quote, pool);
  checkTaken(a, given[0], taken[0]);
  checkTaken(b, given[1], taken[1]);
  const minShares = fixed ?? atLeast(shares, bps);
  if (shares < minShares) throw new Error(`the quote gives ${formatUnits(shares, 0)} shares, fewer than --min-shares ${formatUnits(minShares, 0)}`);
  console.log(`  quote    ${formatUnits(shares, 0)} shares, ${withMinimum ? `at least ${formatUnits(minShares, 0)}` : 'no minimum'}`);
  if (taken[0] !== given[0] || taken[1] !== given[1]) {
    console.log(`  deposit  ${formatUnits(taken[0], a.decimals)} ${a.label} + ${formatUnits(taken[1], b.decimals)} ${b.label}, what the pool's ratio takes`);
  }
  const action: Action = { kind: 'Call', contract: pool, method: 'add_liquidity', args: args(taken, minShares), funds: funds(taken) };
  const { receipt } = await submit(node, signer, action, limits);
  const [ra, rb] = await reserves(node, pool);
  console.log(`  shares   ${formatUnits(readU128(receipt.output), 0)}`);
  console.log(`  reserves ${formatUnits(ra, a.decimals)} ${a.label} / ${formatUnits(rb, b.decimals)} ${b.label}`);
}

async function cmdSwap(node: KsanaNode, signer: Signer, pool: Uint8Array, assetIn: Uint8Array, amount: string, bps: bigint, limits: Limits) {
  const [a, b] = await poolAssets(node, pool);
  const [inMeta, outMeta] = equalBytes(assetIn, a.id) ? [a, b] : equalBytes(assetIn, b.id) ? [b, a] : [null, null];
  if (inMeta === null || outMeta === null) throw new Error('the pool does not trade that asset');
  const units = parseUnits(amount, inMeta.decimals);
  const funds = [{ asset: inMeta.id, amount: units }];
  // The quote: the same swap as a read-only call on the latest state, as this account
  const quoteArgs = concatBytes(inMeta.id, u128le(units), u128le(1n));
  const quote = await node.rpc.call({ contract: pool, method: 'swap', args: quoteArgs, funds, caller: signer.account });
  if (!quote.success) throw new Error(`the quote failed: ${quote.error ?? ''}`);
  const expected = readU128(quote.output);
  const minOut = atLeast(expected, bps);
  console.log(`Swap ${formatUnits(units, inMeta.decimals)} ${inMeta.label} for ${outMeta.label}`);
  console.log(`  quote    ${formatUnits(expected, outMeta.decimals)} ${outMeta.label}, at least ${formatUnits(minOut, outMeta.decimals)}`);
  const args = concatBytes(inMeta.id, u128le(units), u128le(minOut));
  const { receipt } = await submit(node, signer, { kind: 'Call', contract: pool, method: 'swap', args, funds }, limits);
  console.log(`  received ${formatUnits(readU128(receipt.output), outMeta.decimals)} ${outMeta.label}`);
}

async function cmdBalance(node: KsanaNode, text: string) {
  const account = parseAccount(text, btcNetworkOf(node.params));
  const info = await node.rpc.getAccount(account);
  console.log(`${node.address(account)}  (nonce ${info.nonce}, block #${info.at.height})`);
  for (const balance of info.balances) {
    const meta = await node.asset(balance.asset);
    console.log(`  ${formatUnits(balance.amount, meta.decimals).padStart(24)}  ${meta.label}`);
  }
}

async function cmdSettlement(node: KsanaNode, opIds: string[]) {
  const network = btcNetworkOf(node.params);
  const explorer = network === 'signet' ? 'https://mempool.space/signet/tx/' : network === 'mainnet' ? 'https://mempool.space/tx/' : null;
  for (const text of opIds) {
    const id = toHex(fromHex(text.replace(/^0x/, ''), 32, 'op_id'));
    const status = await node.rpc.getOpStatus(id);
    if (status.block === null) {
      console.log(`${id}: ${status.status}, in no block yet`);
      continue;
    }
    const s = await node.rpc.getSettlement(toHex(status.block.hash));
    console.log(`op ${id}`);
    console.log(`  block    #${status.block.height} ${toHex(status.block.hash)}  (${status.status})`);
    if (s.txid === null) {
      console.log(`  BTC      no settlement transaction carries it yet (${s.status})`);
      continue;
    }
    // The IDs from their bytes, which the SDK checks, rather than the display strings the node sends with them
    const reveal = txidToRpc(s.txid);
    const carrier = s.carrier === null ? 'its own' : `block #${s.carrier.height}'s`;
    console.log(`  BTC      ${s.status}, carried by ${carrier} settlement`);
    console.log(`  reveal   ${reveal}`);
    if (s.commit_txid !== null) console.log(`  commit   ${txidToRpc(s.commit_txid)}`);
    if (s.confirmed !== null) {
      console.log(`  mined    BTC #${s.confirmed.height}, ${s.confirmed.confirmations} confirmation${s.confirmed.confirmations === 1 ? '' : 's'}${s.btc_final ? ', BTC-final' : ''}`);
    }
    if (explorer !== null) console.log(`  link     ${explorer}${reveal}`);
  }
}

// ---------------------------------------------------------------------------------------------

async function main() {
  const { values, positionals } = parseArgs({
    allowPositionals: true,
    options: {
      rpc: { type: 'string', default: process.env.KSANA_RPC ?? 'http://127.0.0.1:8545' },
      chain: { type: 'string', default: process.env.KSANA_CHAIN ?? '' },
      key: { type: 'string' },
      code: { type: 'string' },
      name: { type: 'string' },
      symbol: { type: 'string' },
      decimals: { type: 'string', default: '8' },
      supply: { type: 'string' },
      pool: { type: 'string' },
      a: { type: 'string' },
      b: { type: 'string' },
      in: { type: 'string' },
      amount: { type: 'string' },
      slippage: { type: 'string', default: '1' },
      'min-shares': { type: 'string' },
      'max-fee': { type: 'string', default: '0.01' },
      'max-gas': { type: 'string', default: '1000000' },
      help: { type: 'boolean', short: 'h' },
    },
  });
  const [command, ...rest] = positionals;
  if (values.help || command === undefined) {
    console.log(USAGE);
    return;
  }
  const need = (value: string | undefined, what: string): string => {
    if (value === undefined) throw new Error(`${command} needs ${what}`);
    return value;
  };
  const chain = values.chain === '' ? undefined : networkParams(values.chain);
  let connected: Promise<KsanaNode> | undefined;
  const node = () => (connected ??= KsanaNode.connect(values.rpc, chain));
  // A command that signs: without --chain, it signs for whatever network the node reports
  const signingNode = async () => {
    const n = await node();
    if (chain === undefined) console.error(`warning: no --chain, so this signs for the network the node reports: ${networkOfChainId(n.params.chain_id)}`);
    return n;
  };
  const limits = (): Limits => ({ maxGas: BigInt(values['max-gas']), maxFee: parseUnits(values['max-fee'], KSA_DECIMALS) });
  const signer = () => readKey(need(values.key, '--key'));
  const code = () => {
    const file = need(values.code, '--code');
    return { bytes: new Uint8Array(readFileSync(file)), name: basename(file) };
  };
  const pool = () => fromHex(need(values.pool, '--pool').replace(/^0x/, ''), 32, 'pool');
  switch (command) {
    case 'key':
      return cmdKey(rest[0] ?? need(undefined, 'a key file'), async () => chain ?? (await node()).params);
    case 'token': {
      // The protocol's limits on an asset's metadata (D-121): a name of 1 to 64 bytes, a symbol of 1 to 16, at most 18 decimals
      const decimals = Number(values.decimals);
      if (!Number.isInteger(decimals) || decimals < 0 || decimals > 18) throw new Error('--decimals must be an integer from 0 to 18');
      const [s, c, l] = [signer(), code(), limits()];
      const [name, symbol, supply] = [need(values.name, '--name'), need(values.symbol, '--symbol'), need(values.supply, '--supply')];
      for (const [text, flag, max] of [[name, '--name', 64], [symbol, '--symbol', 16]] as const) {
        const length = utf8(text).length;
        if (length < 1 || length > max) throw new Error(`${flag} must be 1 to ${max} bytes`);
      }
      return cmdToken(await signingNode(), s, c.bytes, c.name, { name, symbol, decimals }, supply, l);
    }
    case 'pool': {
      const [s, c, l, a, b] = [signer(), code(), limits(), need(values.a, '--a'), need(values.b, '--b')];
      const n = await signingNode();
      const [idA, idB] = [n.parseAsset(a), n.parseAsset(b)];
      // The amm refuses two equal sides, but only once the deployment has paid its fee
      if (equalBytes(idA, idB)) throw new Error('--a and --b must be two different assets');
      return cmdPool(n, s, c.bytes, c.name, idA, idB, l);
    }
    case 'add': {
      const [s, p, l, bps, a, b] = [signer(), pool(), limits(), basisPoints(values.slippage), need(values.a, '--a'), need(values.b, '--b')];
      return cmdAdd(await signingNode(), s, p, a, b, bps, values['min-shares'], l);
    }
    case 'swap': {
      const [s, p, l, bps, assetIn, amount] = [signer(), pool(), limits(), basisPoints(values.slippage), need(values.in, '--in'), need(values.amount, '--amount')];
      const n = await signingNode();
      return cmdSwap(n, s, p, n.parseAsset(assetIn), amount, bps, l);
    }
    case 'balance':
      return cmdBalance(await node(), rest[0] ?? need(undefined, 'an account'));
    case 'settlement':
      if (rest.length === 0) throw new Error('settlement needs at least one op_id');
      return cmdSettlement(await node(), rest);
    default:
      throw new Error(`unknown command ${command}; --help lists the commands`);
  }
}

main().catch((e: unknown) => {
  if (e instanceof StillWaiting) {
    process.exitCode = EXIT_WAITING;
    return;
  }
  const message = e instanceof RpcError && e.code === RPC_ERROR_CODES.OP_REJECTED ? `the node refused the op: ${e.message}` : e instanceof Error ? e.message : String(e);
  // The message may carry text from the chain or the node: a revert reason, an RPC error
  console.error(`error: ${printable(message, 1_000)}`);
  process.exitCode = 1;
});
