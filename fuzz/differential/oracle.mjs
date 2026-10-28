// The TypeScript side of the differential test (D-133, fuzz/src/differential/): reads one JSON request per line on
// stdin and writes one JSON answer per line on stdout, describing what the SDK (sdk/ts, built into sdk/ts/dist) and the
// contract playground's ABI codec (playground/src/abi/codec.ts) make of the input, in the same form as the Rust side
// (fuzz/src/differential/rust.rs). `{"kind": "batch", "requests": [...]}` answers an array. The first request, `hello`,
// hands over the test's ABI types, checked against what the playground's parser (playground/src/abi/parse.ts) makes of their
// declarations.
//
// KSANA_DIFF_SDK names another build of the SDK (its dist/src/index.js), to compare an older or a patched one.

import { Buffer } from 'node:buffer';
import { createRequire } from 'node:module';
import path from 'node:path';
import { createInterface } from 'node:readline';
import { fileURLToPath, pathToFileURL } from 'node:url';

const here = path.dirname(fileURLToPath(import.meta.url));
const root = path.resolve(here, '../..');
const sdkPath = process.env.KSANA_DIFF_SDK ?? path.join(root, 'sdk/ts/dist/src/index.js');
const sdk = await import(pathToFileURL(sdkPath).href);

// The playground's ABI codec and parser are TypeScript inside a Vite project: bundle them with the SDK's esbuild
const esbuild = createRequire(path.join(root, 'sdk/ts/package.json'))('esbuild');
const bundled = esbuild.buildSync({
  stdin: {
    contents: "export * from './codec.ts';\nexport { parseAbi } from './parse.ts';\n",
    resolveDir: path.join(root, 'playground/src/abi'),
    loader: 'ts',
  },
  bundle: true,
  format: 'esm',
  platform: 'neutral',
  write: false,
  logLevel: 'silent',
});
const abiCodec = await import(`data:text/javascript;base64,${Buffer.from(bundled.outputFiles[0].text).toString('base64')}`);

const REFUSED = Object.freeze({ refused: true });
const FIRST_INPUT = Object.freeze({ txid: new Uint8Array(32).fill(0xc1), vout: 1 });
const MAX_BORSH_BYTES = 1 << 20;

const hex = (bytes) => sdk.toHex(bytes);
const bytes = (text) => sdk.fromHex(text);
const hexOrNull = (value) => (value === null || value === undefined ? null : hex(value));
const dec = (value) => (value === null || value === undefined ? null : value.toString());

/** The value of `f()`, or `null` when it throws. */
function attempt(f) {
  try {
    return f();
  } catch {
    return null;
  }
}

function params(network) {
  const p = sdk.NETWORKS[network];
  if (p === undefined) throw new Error(`unknown network ${network}`);
  return p;
}

function verifyOp(op, firstInput) {
  return op.sig.kind === 'Bip340' ? sdk.verifyVmop(op, firstInput) === 'valid' : sdk.verifyVmopBip322(op, firstInput);
}

function vmop(req) {
  let op;
  try {
    op = sdk.decodeVmOp(bytes(req.bytes), sdk.DEVNET);
  } catch {
    return REFUSED;
  }
  const out = {
    value: sdk.toJson('VmOp', op),
    op_id: hex(sdk.opId(op)),
    sighash: attempt(() => hex(sdk.vmopSighash(op))),
    native_sighash: attempt(() => hex(sdk.vmopSighash(op, FIRST_INPUT))),
    verify: verifyOp(op, undefined),
    verify_native: verifyOp(op, FIRST_INPUT),
  };
  if (req.sign !== null && req.sign !== undefined) {
    const secret = bytes(req.sign.secret);
    const aux = bytes(req.sign.aux);
    const first = op.fee.kind === 'BtcNative' ? FIRST_INPUT : undefined;
    out.signatures = {
      bip340: attempt(() => hex(sdk.signVmop(op, secret, aux, first).bytes)),
      bip322: attempt(() => hex(sdk.signVmopBip322(op, secret, aux, first).sig.bytes)),
    };
  }
  return out;
}

function payload(req) {
  try {
    return { value: sdk.toJson('Payload', sdk.decodePayload(bytes(req.bytes), params(req.network))) };
  } catch {
    return REFUSED;
  }
}

function borsh(req) {
  let value;
  try {
    value = sdk.decode(req.type, bytes(req.bytes), MAX_BORSH_BYTES);
  } catch {
    return REFUSED;
  }
  const out = { value: sdk.toJson(req.type, value) };
  if (req.type === 'VmOp') out.op_id = hex(sdk.opId(value));
  if (req.type === 'BlockHeader') out.hash = hex(sdk.blockHash(value));
  if (req.type === 'Block') {
    out.hash = hex(sdk.blockHash(value.header));
    out.entries_root = hex(sdk.entriesRoot(value.entries));
  }
  return out;
}

function tx(req) {
  let parsed;
  try {
    parsed = sdk.parseTx(bytes(req.bytes));
  } catch {
    return REFUSED;
  }
  const p = params(req.network);
  const data = sdk.txData(p.magic, parsed);
  const found = sdk.txPayload(parsed, p);
  const op = sdk.nativeOpFromTx(parsed, p);
  const first = sdk.nativeTxFirstInput(parsed);
  return {
    tx: {
      txid: hex(sdk.txid(parsed)),
      version: parsed.version,
      lock_time: parsed.locktime,
      inputs: parsed.inputs.map((i) => ({
        txid: hex(i.txid),
        vout: i.vout,
        script_sig: hex(i.scriptSig),
        sequence: i.sequence,
        witness: i.witness.map(hex),
      })),
      outputs: parsed.outputs.map((o) => ({ value: o.value.toString(), script: hex(o.script) })),
      bytes: hex(sdk.serializeTx(parsed)),
    },
    first_input: first === null ? null : { txid: hex(first.txid), vout: first.vout },
    data: data.kind === 'one' ? { carrier: data.carrier, payload: hex(data.payload) } : data.kind,
    payload: found === null ? null : { carrier: found.carrier, payload: sdk.toJson('Payload', found.payload) },
    native_op: op === null ? null : sdk.toJson('VmOp', op),
  };
}

function script(req) {
  const s = bytes(req.script);
  const magic = sdk.DEVNET.magic;
  const envelope = sdk.parseKsn1Envelope(magic, s);
  return {
    output: { is_ksn1: sdk.isKsn1Output(magic, s), payload: hexOrNull(sdk.parseKsn1DataScript(magic, s)) },
    envelope: {
      is_ksn1: sdk.isKsn1Envelope(magic, s),
      key: envelope === null ? null : hex(envelope.key),
      payload: envelope === null ? null : hex(envelope.payload),
    },
  };
}

function witness(req) {
  return { leaf: hexOrNull(sdk.witnessLeaf(req.items.map(bytes))) };
}

function write(req) {
  const p = bytes(req.payload);
  const magic = sdk.DEVNET.magic;
  return {
    data_script: attempt(() => hex(sdk.ksn1DataScript(magic, p))),
    envelope_script: attempt(() => hex(sdk.ksn1EnvelopeScript(bytes(req.key), magic, p))),
  };
}

function address(req) {
  return {
    from_address: attempt(() => hex(sdk.accountFromAddress(req.text, req.network))),
    parse_account: attempt(() => hex(sdk.parseAccount(req.text, req.network))),
  };
}

function account(req) {
  return { address: attempt(() => sdk.accountToAddress(bytes(req.account), req.network)) };
}

function bip322(req) {
  const message = bytes(req.message);
  const signature = bytes(req.signature);
  return {
    simple: sdk.verifyBip322Simple(bytes(req.challenge), message, signature),
    for_account: sdk.verifyBip322ForAccount(bytes(req.account), message, signature),
  };
}

const hex256 = (value) => value.toString(16).padStart(64, '0');

function bits(req) {
  return {
    target: attempt(() => hex256(sdk.targetFromBits(req.bits, req.pow_limit_bits))),
    effective: attempt(() => hex256(sdk.effectiveTarget(req.bits, req.pow_limit_bits, BigInt(req.weight)))),
  };
}

function pow(req) {
  let header;
  try {
    header = sdk.decode('BlockHeader', bytes(req.header), MAX_BORSH_BYTES);
  } catch {
    return REFUSED;
  }
  const p = params(req.network);
  let check;
  try {
    check = sdk.checkPow(p, header, BigInt(req.weight)) ? 'ok' : 'insufficient';
  } catch {
    check = 'invalid';
  }
  return { hash: attempt(() => hex(sdk.powHash(p, header))), check };
}

function rewards(req) {
  const p = params(req.network);
  const daa = BigInt(req.daa_score);
  const split = sdk.blockRewardsSplitAt(p, daa, BigInt(req.weight));
  const rules = sdk.rulesAt(p, daa);
  return {
    period: dec(sdk.rewardPeriodAt(p, daa)),
    reward: dec(sdk.blockRewardAt(p, daa)),
    share: dec(sdk.blockShareAt(p, daa)),
    carrier: dec(sdk.blockCarrierAt(p, daa)),
    carrier_fee: dec(sdk.carrierFeeAt(p, daa, BigInt(req.amount))),
    split: { producer: dec(split.producer), share: dec(split.share), carrier: dec(split.carrier) },
    max_total: dec(sdk.maxTotalRewards(p)),
    rules: Object.fromEntries(Object.entries(rules).map(([k, v]) => [k, dec(v)])),
  };
}

/** A token standard's event in the externally tagged JSON of `tests/vectors/standards.json`. */
function eventJson(event) {
  if (event === null) return null;
  const { kind, ...fields } = event;
  if (kind === 'Standards') return { Standards: fields.ids };
  if (kind === 'Uri') return { Uri: hexOrNull(fields.token_id) };
  const out = {};
  for (const [k, v] of Object.entries(fields)) {
    out[k] = v instanceof Uint8Array ? hex(v) : typeof v === 'bigint' ? v.toString() : v;
  }
  return { [kind]: out };
}

function event(req) {
  const data = bytes(req.data);
  return {
    ks20: eventJson(sdk.decodeKs20Event(data)),
    ks721: eventJson(sdk.decodeKs721Event(data)),
    standard: sdk.standardOf(req.standards),
  };
}

// The structs and enums of the ABI types (fuzz/src/differential/abi.rs), and the events of ABI_EVENTS
let abiDefs = null;
let abiEvents = null;

function abiOf(events) {
  return {
    structs: new Map(Object.entries(abiDefs.structs)),
    enums: new Map(Object.entries(abiDefs.enums)),
    init: null,
    methods: [],
    events,
  };
}

/** A value of the playground's codec in the form of the Rust side (fuzz/src/differential/abi.rs). */
function valueJson(value) {
  if (typeof value === 'bigint') return value.toString();
  if (value instanceof Uint8Array) return hex(value);
  if (value === null || typeof value === 'boolean' || typeof value === 'string') return value;
  if (Array.isArray(value)) return value.map(valueJson);
  if (abiCodec.isEnumValue?.(value) ?? (typeof value.variant === 'string' && Array.isArray(value.values))) {
    return { variant: value.variant, values: value.values.map(valueJson) };
  }
  return Object.fromEntries(Object.entries(value).map(([k, v]) => [k, valueJson(v)]));
}

function abi(req) {
  const abiValue = abiOf([]);
  const data = bytes(req.bytes);
  let value;
  try {
    value = abiCodec.decodeValue(abiValue, req.type, data);
  } catch {
    return REFUSED;
  }
  const again = attempt(() => abiCodec.encodeValue(abiValue, req.type, value));
  return { value: valueJson(value), canonical: again !== null && sdk.equalBytes(again, data) };
}

function abiEvent(req) {
  const decoded = abiCodec.decodeEvent(abiOf(abiEvents), bytes(req.bytes));
  if (decoded === null) return REFUSED;
  return { name: decoded.name, fields: decoded.fields.map((f) => [f.name, valueJson(f.value)]) };
}

function merkle(req) {
  return { root: hex(sdk.merkleRoot(req.items.map(bytes))) };
}

/** The SDK's parameters with every number as a decimal string and every byte string in hex. */
function paramsJson(value) {
  if (typeof value === 'number' || typeof value === 'bigint') return value.toString();
  if (value instanceof Uint8Array) return hex(value);
  if (value === null || value === undefined) return null;
  if (Array.isArray(value)) return value.map(paramsJson);
  if (typeof value === 'object') return Object.fromEntries(Object.entries(value).map(([k, v]) => [k, paramsJson(v)]));
  return value;
}

const UPGRADE_FIELDS = [
  'version',
  'daa_score',
  'block_gas_limit',
  'native_max_gas',
  'min_vmop_version',
  'max_vmop_version',
  'block_share_bps',
  'block_share_end',
  'carrier_bps',
  'carrier_fee_bps',
];

function paramsOf(req) {
  const p = params(req.network);
  const out = paramsJson(p);
  // An upgrade leaves out the rules it does not change; the Rust side writes them as null
  out.upgrades = p.upgrades.map((u) => Object.fromEntries(UPGRADE_FIELDS.map((k) => [k, paramsJson(u[k] ?? null)])));
  return out;
}

const HANDLERS = {
  vmop,
  payload,
  borsh,
  tx,
  script,
  witness,
  write,
  address,
  account,
  bip322,
  bits,
  pow,
  rewards,
  event,
  abi,
  abi_event: abiEvent,
  merkle,
  params: paramsOf,
};

/** A value as JSON with every object's keys sorted (the Rust side writes them sorted). */
function canonical(value) {
  if (Array.isArray(value)) return value.map(canonical);
  if (value instanceof Map) return canonical(Object.fromEntries(value));
  if (value !== null && typeof value === 'object') {
    return Object.fromEntries(Object.keys(value).sort().map((k) => [k, canonical(value[k])]));
  }
  return value;
}

/**
 * Where the playground's parser reads the test's `abi` block (fuzz/src/differential/abi.rs, `abi_block`) differently from the
 * structs, enums, events and types the requests use: each a line of text, none when they agree.
 */
function abiMismatches(req) {
  let parsed;
  try {
    parsed = abiCodec.parseAbi(req.abi_block);
  } catch (e) {
    return [`the block does not parse: ${e?.message ?? String(e)}`];
  }
  const out = [];
  const same = (a, b) => JSON.stringify(canonical(a)) === JSON.stringify(canonical(b));
  const plain = ({ line: _line, doc: _doc, ...rest }) => rest;
  const structs = Object.fromEntries([...parsed.structs].map(([k, v]) => [k, plain(v)]));
  const enums = Object.fromEntries([...parsed.enums].map(([k, v]) => [k, plain(v)]));
  if (!same(structs, req.abi_defs.structs)) out.push(`structs: ${JSON.stringify(canonical(structs))}`);
  if (!same(enums, req.abi_defs.enums)) out.push(`enums: ${JSON.stringify(canonical(enums))}`);
  const events = parsed.events.map(plain);
  if (!same(events, req.abi_events)) out.push(`events: ${JSON.stringify(canonical(events))}`);
  req.abi_types.forEach((type, i) => {
    const method = parsed.methods.find((m) => m.name === `t${i}`);
    const got = method?.params[0]?.type;
    if (!same(got, type)) out.push(`t${i}: ${JSON.stringify(canonical(got ?? null))}`);
  });
  return out;
}

function answer(req) {
  if (req.kind === 'hello') {
    abiDefs = req.abi_defs ?? abiDefs;
    abiEvents = req.abi_events ?? abiEvents;
    return { sdk: sdkPath, node: process.version, kinds: Object.keys(HANDLERS), abi_mismatches: abiMismatches(req) };
  }
  if (req.kind === 'batch') return req.requests.map(answer);
  const handler = HANDLERS[req.kind];
  if (handler === undefined) return { oracle_error: `unknown kind ${req.kind}` };
  try {
    return handler(req);
  } catch (e) {
    // An exception the description did not expect is an answer of its own: the two sides cannot agree on it
    return { oracle_error: `${e?.name ?? 'Error'}: ${e?.message ?? String(e)}` };
  }
}

const lines = createInterface({ input: process.stdin, crlfDelay: Infinity });
for await (const line of lines) {
  if (line.trim() === '') continue;
  process.stdout.write(`${JSON.stringify(answer(JSON.parse(line)))}\n`);
}
