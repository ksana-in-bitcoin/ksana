/**
 * The token standards KS-20 and KS-721 (D-120) against the shared vectors `tests/vectors/standards.json`, which the contract
 * SDK's reference implementation writes (`crates/ksana-contract-sdk/examples/gen_standard_vectors`): every method's arguments
 * through the operation builders and the views, every answer through the views, the events both ways and the bytes that are no
 * event, KS-721 token IDs and `AssetRef`; then the reasons and capabilities of `docs/STANDARDS.md`, what the builders refuse
 * and how the views fail. A fake node stands in for `ksana_call`.
 */

import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { test } from 'node:test';

import {
  type Action,
  type CallRequest,
  CodecError,
  KS20,
  KS20_UNLIMITED,
  KS721,
  KS721_ENUMERABLE,
  KS721_MAX_PAGE,
  type Receipt,
  REJECTED_PREFIX,
  STANDARD_CAPABILITIES,
  STANDARD_REASONS,
  StandardCallError,
  type StandardReader,
  assetTransfer,
  concatBytes,
  decodeAssetRef,
  decodeKs20Event,
  decodeKs721Event,
  decodeStandardEvent,
  decodeTransferAndCallAnswer,
  encodeAssetRef,
  encodeKs20Event,
  encodeKs721Event,
  fromHex,
  isExpired,
  ks20Allowance,
  ks20AllowanceInfo,
  ks20Approve,
  ks20BalanceOf,
  ks20Burn,
  ks20BurnFrom,
  ks20Cap,
  ks20Metadata,
  ks20Mint,
  ks20Minter,
  ks20SetMinter,
  ks20TotalSupply,
  ks20Transfer,
  ks20TransferAndCall,
  ks20TransferFrom,
  ks721Approve,
  ks721Approved,
  ks721BalanceOf,
  ks721Burn,
  ks721IsOperator,
  ks721Metadata,
  ks721OperatorInfo,
  ks721OwnerOf,
  ks721Serial,
  ks721SetOperator,
  ks721TokenId,
  ks721TokenUri,
  ks721Tokens,
  ks721TokensOf,
  ks721TotalSupply,
  ks721Transfer,
  ks721TransferAndCall,
  ks721TransferFrom,
  readCapabilities,
  readKs20,
  readKs721,
  readStandards,
  standardOf,
  toHex,
  u32le,
  utf8,
} from '../src/index.js';
import { loadVectors, repoFile } from './helpers.js';

const vectors = loadVectors('standards.json');

/** The contract the vectors' calls go to: no account of the vectors. */
const CONTRACT = Uint8Array.from({ length: 32 }, (_, i) => 0xd0 + i);
const ZERO = new Uint8Array(32);
const ALICE = Uint8Array.from({ length: 32 }, (_, i) => 0x10 + i);
const BOB = Uint8Array.from({ length: 32 }, (_, i) => 0x40 + i);

const b = (hex: string): Uint8Array => fromHex(hex);
const ob = (hex: string | null): Uint8Array | null => (hex === null ? null : fromHex(hex));
const n = (decimal: string): bigint => BigInt(decimal);
const on = (decimal: string | null): bigint | null => (decimal === null ? null : BigInt(decimal));

/** A value as the vectors write it (D-007): bytes as hex, u64 / u128 as decimal strings. */
function json(value: unknown): unknown {
  if (value instanceof Uint8Array) return toHex(value);
  if (typeof value === 'bigint') return value.toString();
  if (Array.isArray(value)) return value.map(json);
  if (value !== null && typeof value === 'object') return Object.fromEntries(Object.entries(value).map(([k, v]) => [k, json(v)]));
  return value;
}

/** An event as the vectors write it: externally tagged, the declaration and `uri` as Rust's tuple variants. */
function eventJson(event: { kind: string }): unknown {
  const { kind, ...fields } = event as { kind: string } & Record<string, unknown>;
  if (kind === 'Standards') return { Standards: fields['ids'] };
  if (kind === 'Uri') return { Uri: json(fields['token_id']) };
  return { [kind]: json(fields) };
}

function receipt(answer: { output?: Uint8Array; error?: string }): Receipt {
  return {
    op_id: null,
    success: answer.error === undefined,
    error: answer.error ?? null,
    gas_used: 1_000n,
    output: answer.output ?? new Uint8Array(0),
    asset_changes: [],
    events: [],
  };
}

/** A node whose read-only call always answers `answer`, recording the requests. */
function node(answer: { output?: Uint8Array; error?: string }): StandardReader & { requests: CallRequest[] } {
  const requests: CallRequest[] = [];
  return {
    requests,
    async call(request) {
      requests.push(request);
      return receipt(answer);
    },
  };
}

/** A contract answering each view from `answers` and refusing the others with `unknown method`; at most one call at a time is checked. */
function contract(answers: Record<string, Uint8Array>): StandardReader & { methods: string[]; busiest: number } {
  let running = 0;
  const state = {
    methods: [] as string[],
    busiest: 0,
    async call(request: CallRequest): Promise<Receipt> {
      running += 1;
      state.busiest = Math.max(state.busiest, running);
      state.methods.push(request.method);
      await new Promise((resolve) => setTimeout(resolve, 1));
      running -= 1;
      const output = answers[request.method];
      return receipt(output === undefined ? { error: 'unknown method' } : { output });
    },
  };
  return state;
}

/** The views, by `<standard> <method>`, with the arguments `v` of a call vector. */
const VIEWS: Record<string, (rpc: StandardReader, v: any) => Promise<unknown>> = {
  'KS-20 standards': (rpc) => readStandards(rpc, CONTRACT),
  'KS-20 metadata': (rpc) => ks20Metadata(rpc, CONTRACT),
  'KS-20 capabilities': (rpc) => readCapabilities(rpc, CONTRACT),
  'KS-20 total_supply': (rpc) => ks20TotalSupply(rpc, CONTRACT),
  'KS-20 balance_of': (rpc, v) => ks20BalanceOf(rpc, CONTRACT, b(v.account)),
  'KS-20 allowance': (rpc, v) => ks20Allowance(rpc, CONTRACT, b(v.owner), b(v.spender)),
  'KS-20 allowance_info': (rpc, v) => ks20AllowanceInfo(rpc, CONTRACT, b(v.owner), b(v.spender)),
  'KS-20 minter': (rpc) => ks20Minter(rpc, CONTRACT),
  'KS-20 cap': (rpc) => ks20Cap(rpc, CONTRACT),
  'KS-721 standards': (rpc) => readStandards(rpc, CONTRACT),
  'KS-721 metadata': (rpc) => ks721Metadata(rpc, CONTRACT),
  'KS-721 capabilities': (rpc) => readCapabilities(rpc, CONTRACT),
  'KS-721 owner_of': (rpc, v) => ks721OwnerOf(rpc, CONTRACT, b(v.token_id)),
  'KS-721 balance_of': (rpc, v) => ks721BalanceOf(rpc, CONTRACT, b(v.owner)),
  'KS-721 token_uri': (rpc, v) => ks721TokenUri(rpc, CONTRACT, b(v.token_id)),
  'KS-721 approved': (rpc, v) => ks721Approved(rpc, CONTRACT, b(v.token_id)),
  'KS-721 is_operator': (rpc, v) => ks721IsOperator(rpc, CONTRACT, b(v.owner), b(v.operator)),
  'KS-721 operator_info': (rpc, v) => ks721OperatorInfo(rpc, CONTRACT, b(v.owner), b(v.operator)),
  'KS-721 total_supply': (rpc) => ks721TotalSupply(rpc, CONTRACT),
  'KS-721 tokens': (rpc, v) => ks721Tokens(rpc, CONTRACT, n(v.start), v.limit),
  'KS-721 tokens_of': (rpc, v) => ks721TokensOf(rpc, CONTRACT, b(v.owner), n(v.start), v.limit),
};

/** The operations, by `<standard> <method>`, with the arguments `v` of a call vector. */
const OPERATIONS: Record<string, (v: any) => Action> = {
  'KS-20 transfer': (v) => ks20Transfer(CONTRACT, b(v.to), n(v.amount)),
  'KS-20 transfer_from': (v) => ks20TransferFrom(CONTRACT, b(v.from), b(v.to), n(v.amount)),
  'KS-20 approve': (v) => ks20Approve(CONTRACT, b(v.spender), n(v.amount), on(v.expires_at)),
  'KS-20 transfer_and_call': (v) => ks20TransferAndCall(CONTRACT, b(v.to), n(v.amount), b(v.data)),
  'KS-20 burn': (v) => ks20Burn(CONTRACT, n(v.amount)),
  'KS-20 burn_from': (v) => ks20BurnFrom(CONTRACT, b(v.from), n(v.amount)),
  'KS-20 mint': (v) => ks20Mint(CONTRACT, b(v.to), n(v.amount)),
  'KS-20 set_minter': (v) => ks20SetMinter(CONTRACT, ob(v.minter)),
  'KS-721 transfer': (v) => ks721Transfer(CONTRACT, b(v.to), b(v.token_id)),
  'KS-721 transfer_from': (v) => ks721TransferFrom(CONTRACT, b(v.from), b(v.to), b(v.token_id)),
  'KS-721 approve': (v) => ks721Approve(CONTRACT, ob(v.spender), b(v.token_id)),
  'KS-721 set_operator': (v) => ks721SetOperator(CONTRACT, b(v.operator), v.approved, on(v.expires_at)),
  'KS-721 transfer_and_call': (v) => ks721TransferAndCall(CONTRACT, b(v.to), b(v.token_id), b(v.data)),
  'KS-721 burn': (v) => ks721Burn(CONTRACT, b(v.token_id)),
};

const keyOf = (v: { standard: string; method: string }): string => `${v.standard} ${v.method}`;

/** The arguments of a view's first call vector, for asking it in the answer tests. */
function argumentsOf(key: string): unknown {
  const call = vectors.calls.find((v: any) => keyOf(v) === key);
  assert.ok(call !== undefined, `no call vector for ${key}`);
  return call.value;
}

test('standards.json: every call is a view or an operation here, and every view and operation has a call and an answer', () => {
  const calls = new Set<string>(vectors.calls.map(keyOf));
  for (const key of calls) {
    assert.ok((key in VIEWS) !== (key in OPERATIONS), `${key} must be exactly one of a view and an operation`);
  }
  for (const key of [...Object.keys(VIEWS), ...Object.keys(OPERATIONS)]) assert.ok(calls.has(key), `no call vector for ${key}`);
  const answers = new Set<string>(vectors.answers.map(keyOf));
  for (const key of Object.keys(VIEWS)) assert.ok(answers.has(key), `no answer vector for ${key}`);
  for (const key of answers) assert.ok(key in VIEWS || key.endsWith(' transfer_and_call'), `an answer of ${key}, which is no view`);
  assert.ok(vectors.calls.length >= 42 && vectors.answers.length >= 35);
  assert.ok(vectors.events.length >= 20 && vectors.invalid_events.length >= 12);
});

for (const v of vectors.calls) {
  test(`standards.json call ${keyOf(v)} (${v.name}): the arguments`, async () => {
    const key = keyOf(v);
    const operation = OPERATIONS[key];
    if (operation !== undefined) {
      const action = operation(v.value);
      assert.equal(action.kind, 'Call');
      if (action.kind !== 'Call') return;
      assert.deepEqual(action.contract, CONTRACT);
      assert.equal(action.method, v.method);
      assert.equal(toHex(action.args), v.borsh);
      assert.deepEqual(action.funds, []);
      return;
    }
    const view = VIEWS[key];
    assert.ok(view !== undefined, `no view for ${key}`);
    const rpc = node({ error: 'unknown method' });
    await view(rpc, v.value).catch((e: unknown) => {
      if (!(e instanceof StandardCallError)) throw e;
    });
    assert.equal(rpc.requests.length, 1);
    const [request] = rpc.requests;
    assert.deepEqual(request?.contract, CONTRACT);
    assert.equal(request?.method, v.method);
    assert.equal(toHex(request?.args ?? new Uint8Array(0)), v.borsh);
    assert.equal(request?.funds, undefined);
    assert.equal(request?.caller, undefined);
  });
}

for (const v of vectors.answers) {
  test(`standards.json answer ${keyOf(v)} (${v.name}): decoded, and refused with a byte more`, async () => {
    const key = keyOf(v);
    const bytes = b(v.borsh);
    const longer = concatBytes(bytes, new Uint8Array([0]));
    if (v.method === 'transfer_and_call') {
      assert.equal(toHex(decodeTransferAndCallAnswer(bytes)), v.value);
      assert.throws(() => decodeTransferAndCallAnswer(longer), CodecError);
      return;
    }
    const view = VIEWS[key];
    assert.ok(view !== undefined, `no view for ${key}`);
    assert.deepEqual(json(await view(node({ output: bytes }), argumentsOf(key))), v.value);
    if (v.method === 'standards') {
      assert.equal(await view(node({ output: longer }), argumentsOf(key)), null);
    } else {
      await assert.rejects(view(node({ output: longer }), argumentsOf(key)), CodecError);
    }
  });
}

for (const v of vectors.events) {
  test(`standards.json event ${v.standard} (${v.name}): decoded and encoded`, () => {
    const bytes = b(v.borsh);
    if (v.standard === KS20) {
      const event = decodeKs20Event(bytes);
      assert.ok(event !== null);
      assert.deepEqual(eventJson(event), v.value);
      assert.equal(toHex(encodeKs20Event(event)), v.borsh);
    } else {
      assert.equal(v.standard, KS721);
      const event = decodeKs721Event(bytes);
      assert.ok(event !== null);
      assert.deepEqual(eventJson(event), v.value);
      assert.equal(toHex(encodeKs721Event(event)), v.borsh);
    }
    const found = decodeStandardEvent([v.standard], bytes);
    assert.equal(found?.standard, v.standard);
    assert.deepEqual(eventJson(found?.event ?? { kind: 'none' }), v.value);
  });
}

for (const v of vectors.invalid_events) {
  test(`standards.json invalid event ${v.standard} (${v.name}): no event of the standard`, () => {
    const bytes = b(v.borsh);
    assert.equal(v.standard === KS20 ? decodeKs20Event(bytes) : decodeKs721Event(bytes), null);
    assert.equal(decodeStandardEvent([v.standard], bytes), null);
  });
}

test('an event whose name starts with a byte order mark is no event of the standards, as in Rust (D-133)', () => {
  for (const v of vectors.events) {
    const bytes = b(v.borsh);
    const length = new DataView(bytes.buffer, bytes.byteOffset, 4).getUint32(0, true);
    const name = concatBytes(utf8('\ufeff'), bytes.subarray(4, 4 + length));
    const marked = concatBytes(u32le(name.length), name, bytes.subarray(4 + length));
    assert.equal(decodeKs20Event(marked), null, v.name);
    assert.equal(decodeKs721Event(marked), null, v.name);
    assert.equal(decodeStandardEvent([v.standard], marked), null, v.name);
  }
});

test('standards.json token IDs: serial numbers big-endian in the last 16 bytes, and IDs without one', () => {
  for (const v of vectors.token_ids.serials) {
    assert.equal(toHex(ks721TokenId(n(v.serial))), v.token_id);
    assert.equal(ks721Serial(b(v.token_id)), n(v.serial));
  }
  for (const id of vectors.token_ids.without_serial) assert.equal(ks721Serial(b(id)), null);
  assert.ok(vectors.token_ids.serials.length >= 6 && vectors.token_ids.without_serial.length >= 3);
  assert.throws(() => ks721TokenId(-1n), RangeError);
  assert.throws(() => ks721TokenId(1n << 128n), RangeError);
  assert.throws(() => ks721Serial(new Uint8Array(31)));
});

test('standards.json AssetRef: a token or a built-in asset, and bytes that are neither', () => {
  for (const v of vectors.asset_refs.valid) {
    const ref = decodeAssetRef(b(v.borsh));
    assert.deepEqual({ [ref.kind]: toHex(ref.id) }, v.value, v.name);
    assert.equal(toHex(encodeAssetRef(ref)), v.borsh, v.name);
  }
  for (const v of vectors.asset_refs.invalid) assert.throws(() => decodeAssetRef(b(v.borsh)), CodecError, v.name);
  assert.ok(vectors.asset_refs.valid.length >= 3 && vectors.asset_refs.invalid.length >= 4);
});

test('the constants match the vectors: an unlimited allowance and the largest page', () => {
  const unlimited = vectors.calls.find((v: any) => v.method === 'approve' && v.name === 'unlimited, with an expiry');
  assert.equal(unlimited.value.amount, KS20_UNLIMITED.toString());
  const page = vectors.calls.find((v: any) => v.method === 'tokens');
  assert.equal(page.value.limit, KS721_MAX_PAGE);
});

/** The rows of the table under a heading of `docs/STANDARDS.md`, each as its cells. */
function tableUnder(heading: string): string[][] {
  const lines = readFileSync(repoFile('docs/STANDARDS.md'), 'utf8').split('\n');
  const start = lines.indexOf(heading);
  assert.ok(start >= 0, `no heading ${heading}`);
  const rows: string[][] = [];
  for (const line of lines.slice(start + 1)) {
    if (line.startsWith('#')) break;
    if (!line.startsWith('|')) {
      if (rows.length > 0) break;
      continue;
    }
    rows.push(
      line
        .slice(1, -1)
        .split('|')
        .map((cell) => cell.trim()),
    );
  }
  return rows.slice(2);
}

const quoted = (cell: string): string[] => [...cell.matchAll(/`([^`]+)`/g)].map((m) => m[1] ?? '');

test('the revert reasons and the capabilities are those of docs/STANDARDS.md, in its order', () => {
  const reasons = tableUnder('### 5.1 Revert reasons').flatMap((row) => quoted(row[1] ?? ''));
  assert.deepEqual(reasons, [...STANDARD_REASONS, `${REJECTED_PREFIX}<reason>`]);
  const capabilities = tableUnder('### 5.2 Capabilities').flatMap((row) => quoted(row[0] ?? ''));
  assert.deepEqual(capabilities, [...STANDARD_CAPABILITIES]);
});

test('standardOf: the first ID names the standard, and a declaration of both names none', () => {
  assert.equal(standardOf([KS20]), KS20);
  assert.equal(standardOf([KS20, 'KS-20-burnable', 'KS-20-mintable']), KS20);
  assert.equal(standardOf([KS721, KS721_ENUMERABLE]), KS721);
  assert.equal(standardOf(['KS-20-burnable', KS20]), null);
  assert.equal(standardOf([KS20, KS721]), null);
  assert.equal(standardOf([KS721, 'KS-721-burnable', KS20]), null);
  assert.equal(standardOf(['KS-1155']), null);
  assert.equal(standardOf([]), null);
});

test('isExpired: from the expiry on, and never without one', () => {
  assert.equal(isExpired(null, 1n << 63n), false);
  assert.equal(isExpired(100n, 99n), false);
  assert.equal(isExpired(100n, 100n), true);
  assert.equal(isExpired(100n, 101n), true);
});

test('the operations refuse what every token refuses before an op is signed', () => {
  assert.throws(() => ks20Transfer(CONTRACT, BOB, 0n), /zero amount/);
  assert.throws(() => ks20Transfer(CONTRACT, ZERO, 1n), /zero account/);
  assert.throws(() => ks20Transfer(CONTRACT, CONTRACT, 1n), /recipient is this contract/);
  assert.throws(() => ks20TransferFrom(CONTRACT, ALICE, ZERO, 1n), /zero account/);
  assert.throws(() => ks20TransferAndCall(CONTRACT, CONTRACT, 1n), /recipient is this contract/);
  assert.throws(() => ks20Mint(CONTRACT, ZERO, 1n), /zero account/);
  assert.throws(() => ks20Burn(CONTRACT, 0n), /zero amount/);
  assert.throws(() => ks20BurnFrom(CONTRACT, ALICE, 0n), /zero amount/);
  assert.throws(() => ks20Approve(CONTRACT, ZERO, 1n), /zero account/);
  assert.throws(() => ks20SetMinter(CONTRACT, ZERO), /zero account/);
  assert.throws(() => ks721Transfer(CONTRACT, ZERO, ks721TokenId(1n)), /zero account/);
  assert.throws(() => ks721TransferFrom(CONTRACT, ALICE, CONTRACT, ks721TokenId(1n)), /recipient is this contract/);
  assert.throws(() => ks721TransferAndCall(CONTRACT, ZERO, ks721TokenId(1n)), /zero account/);
  assert.throws(() => ks721Approve(CONTRACT, ZERO, ks721TokenId(1n)), /zero account/);
  assert.throws(() => ks721SetOperator(CONTRACT, ZERO, true), /zero account/);
  // past u128, and a token ID of another length, do not encode
  assert.throws(() => ks20Transfer(CONTRACT, BOB, 1n << 128n), CodecError);
  assert.throws(() => ks721Burn(CONTRACT, new Uint8Array(31)), CodecError);
  // what the standards allow: a revocation, the end of minting, a cleared approval
  assert.equal(ks20Approve(CONTRACT, BOB, 0n).kind, 'Call');
  assert.equal(ks20SetMinter(CONTRACT, null).kind, 'Call');
  assert.equal(ks721Approve(CONTRACT, null, ks721TokenId(1n)).kind, 'Call');
});

test('transfer_and_call passes the attached funds on', () => {
  const funds = [{ asset: ZERO, amount: 7n }];
  const token = ks20TransferAndCall(CONTRACT, BOB, 5n, new Uint8Array([1]), funds);
  const nft = ks721TransferAndCall(CONTRACT, BOB, ks721TokenId(9n), new Uint8Array(0), funds);
  for (const action of [token, nft]) {
    assert.equal(action.kind, 'Call');
    if (action.kind === 'Call') assert.deepEqual(action.funds, funds);
  }
});

test('assetTransfer: a token by its transfer, a built-in asset by AssetTransfer', () => {
  assert.deepEqual(assetTransfer({ kind: 'Token', id: CONTRACT }, BOB, 5n), ks20Transfer(CONTRACT, BOB, 5n));
  assert.deepEqual(assetTransfer({ kind: 'Asset', id: ZERO }, BOB, 5n), { kind: 'AssetTransfer', asset: ZERO, to: BOB, amount: 5n });
  assert.throws(() => assetTransfer({ kind: 'Asset', id: ZERO }, BOB, 0n), /zero amount/);
  assert.throws(() => assetTransfer({ kind: 'Token', id: CONTRACT }, CONTRACT, 1n), /recipient is this contract/);
});

test('a view the contract refuses throws its reason, and a page out of range is not asked', async () => {
  const refused = node({ error: `${REJECTED_PREFIX}asset is paused` });
  await assert.rejects(ks20BalanceOf(refused, CONTRACT, ALICE), (e: unknown) => {
    assert.ok(e instanceof StandardCallError);
    assert.equal(e.method, 'balance_of');
    assert.equal(e.reason, 'rejected: asset is paused');
    return true;
  });
  const quiet = node({ output: new Uint8Array(4) });
  await assert.rejects(ks721Tokens(quiet, CONTRACT, 0n, 0), RangeError);
  await assert.rejects(ks721Tokens(quiet, CONTRACT, 0n, KS721_MAX_PAGE + 1), RangeError);
  await assert.rejects(ks721TokensOf(quiet, CONTRACT, ALICE, 0n, 1.5), RangeError);
  assert.equal(quiet.requests.length, 0);
  assert.deepEqual(await ks721Tokens(quiet, CONTRACT, 0n), []);
});

test('readStandards: null for a contract that refuses or answers something else, but a failure of the node throws', async () => {
  assert.equal(await readStandards(node({ error: 'unknown method' }), CONTRACT), null);
  assert.equal(await readStandards(node({ output: new Uint8Array([0xff]) }), CONTRACT), null);
  const down: StandardReader = {
    async call() {
      throw new Error('connection refused');
    },
  };
  await assert.rejects(readStandards(down, CONTRACT), /connection refused/);
});

/** The answer of `standards()` declaring `ids`: a Borsh `Vec<String>`. */
function declaration(ids: string[]): Uint8Array {
  return concatBytes(u32le(ids.length), ...ids.flatMap((id) => [u32le(utf8(id).length), utf8(id)]));
}

/** The `Ok` bytes of an answer vector. */
function answer(standard: string, method: string, name: string): Uint8Array {
  const v = vectors.answers.find((a: any) => a.standard === standard && a.method === method && a.name === name);
  assert.ok(v !== undefined, `no answer ${standard} ${method} (${name})`);
  return b(v.borsh);
}

test('readKs20: the declaration, the metadata, the total supply and the capabilities, one view at a time', async () => {
  const token = contract({
    standards: answer(KS20, 'standards', 'the reference token'),
    metadata: answer(KS20, 'metadata', 'non-ASCII text'),
    total_supply: answer(KS20, 'total_supply', 'every byte differs'),
    capabilities: answer(KS20, 'capabilities', "in the standard's order"),
  });
  assert.deepEqual(await readKs20(token, CONTRACT), {
    standards: ['KS-20', 'KS-20-burnable', 'KS-20-mintable'],
    name: 'Ksana Ñame ₿',
    symbol: 'KSÑ',
    decimals: 8,
    total_supply: 0x0102030405060708090a0b0c0d0e0f10n,
    capabilities: ['mint', 'pause', 'fee'],
  });
  assert.deepEqual(token.methods, ['standards', 'metadata', 'total_supply', 'capabilities']);
  assert.equal(token.busiest, 1);
  const collection = contract({ standards: answer(KS721, 'standards', 'the test collection') });
  assert.equal(await readKs20(collection, CONTRACT), null);
  assert.deepEqual(collection.methods, ['standards']);
  assert.equal(await readKs20(contract({}), CONTRACT), null);
});

test('readKs721: the total supply only of an enumerable collection', async () => {
  const answers = {
    standards: answer(KS721, 'standards', 'the test collection'),
    metadata: answer(KS721, 'metadata', 'non-ASCII text'),
    capabilities: answer(KS721, 'capabilities', 'minting'),
    total_supply: answer(KS721, 'total_supply', 'a count'),
  };
  const enumerable = contract(answers);
  assert.deepEqual(await readKs721(enumerable, CONTRACT), {
    standards: ['KS-721', 'KS-721-burnable', 'KS-721-enumerable'],
    name: 'Ksana Ärt 🎨',
    symbol: 'KÄRT',
    capabilities: ['mint'],
    total_supply: (1n << 64n) - 1n,
  });
  assert.deepEqual(enumerable.methods, ['standards', 'metadata', 'capabilities', 'total_supply']);
  assert.equal(enumerable.busiest, 1);
  const plain = contract({ ...answers, standards: declaration([KS721, 'KS-721-burnable']) });
  assert.equal((await readKs721(plain, CONTRACT))?.total_supply, null);
  assert.deepEqual(plain.methods, ['standards', 'metadata', 'capabilities']);
  const token = contract({ standards: answer(KS20, 'standards', 'no extensions') });
  assert.equal(await readKs721(token, CONTRACT), null);
});

test('decodeStandardEvent: only by a declaration of one standard', () => {
  const transfer = encodeKs20Event({ kind: 'Transfer', from: ALICE, to: BOB, amount: 5n });
  assert.deepEqual(decodeStandardEvent([KS20, 'KS-20-burnable'], transfer), {
    standard: KS20,
    event: { kind: 'Transfer', from: ALICE, to: BOB, amount: 5n },
  });
  assert.equal(decodeStandardEvent([KS721], transfer), null);
  assert.equal(decodeStandardEvent([KS20, KS721], transfer), null);
  assert.equal(decodeStandardEvent([], transfer), null);
  assert.throws(() => encodeKs20Event({ kind: 'Swap' } as never), TypeError);
});
