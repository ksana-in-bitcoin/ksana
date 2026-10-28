/**
 * Wallets (D-052, D-053, D-055, D-092, D-106): account derivation and address checks (account = output key of the Taproot address,
 * account ↔ address conversion), the signing behavior of the mock wallet (Unisat interface) (BIP322 messages and PSBT inputs), the
 * Unisat and Xverse adapters (the BTC network the wallet says it is set to included), `signVmopWithWallet`, and path B with the
 * mock wallet (wallet BIP322 signature → `ksana_submitOp`), with a mock fetch standing in for the node.
 */

import assert from 'node:assert/strict';
import { test } from 'node:test';

import { base64 } from '@scure/base';
import { Transaction } from '@scure/btc-signer';

import {
  type Action,
  DEVNET,
  KsanaRpcClient,
  MockUnisatWallet,
  SCURE_TX_OPTIONS,
  SIGNET,
  type UnsignedVmOp,
  UnisatAdapter,
  type UnisatProvider,
  VMOP_VERSION,
  type VmOp,
  type WalletAdapter,
  XverseAdapter,
  XverseError,
  type XverseBitcoinProvider,
  accountFromAddress,
  accountOf,
  accountToAddress,
  addressToScript,
  bip322Message,
  buildNativeTx,
  compressedPublicKey,
  decodeBip322Signature,
  decodeVmOp,
  deriveWalletAccount,
  extractTransaction,
  fromHex,
  ksaAssetId,
  nativeFirstInput,
  nativeOpFromTx,
  opId,
  p2pkhScriptFromHash,
  p2shScriptFromHash,
  p2trScript,
  p2wpkhScript,
  parseAccount,
  scriptToAddress,
  signVmopWithWallet,
  signedVmop,
  taprootOutputKey,
  toHex,
  verifyBip322ForAccount,
  verifyVmop,
  verifyVmopBip322,
  xOnlyPublicKey,
} from '../src/index.js';
import { pseudoRandomBytes, testKey } from './helpers.js';

const transfer: Action = { kind: 'AssetTransfer', asset: ksaAssetId(), to: new Uint8Array(32).fill(9), amount: 5n };

function unsignedOp(account: Uint8Array): UnsignedVmOp {
  return {
    version: 1,
    chain_id: DEVNET.chain_id,
    account,
    nonce: 3n,
    fee: { kind: 'Token', amount: 2n },
    max_gas: 100_000n,
    action: transfer,
  };
}

test('deriveWalletAccount: account = output key of the Taproot address (D-092); every other address type is refused', () => {
  for (const byte of [1, 2, 3, 4, 5, 6]) {
    const secret = testKey(byte);
    const compressed = compressedPublicKey(secret);
    const internal = xOnlyPublicKey(secret);
    const outputKey = taprootOutputKey(internal);
    const trAddress = scriptToAddress(p2trScript(internal), 'signet');
    // Unisat gives a 33-byte compressed key (prefix 02 or 03); Xverse's ordinals address gives a 32-byte x-only key
    for (const publicKey of [toHex(compressed), toHex(internal)]) {
      const account = deriveWalletAccount(trAddress, publicKey);
      assert.deepEqual(account, {
        address: trAddress,
        publicKey,
        addressType: 'p2tr',
        internalKey: toHex(internal),
        account: toHex(outputKey),
      });
    }
    // The account is the 32-byte witness program in the address: same as the local key's account; converts to and from the address
    assert.deepEqual(outputKey, accountOf(secret));
    assert.deepEqual(addressToScript(trAddress).subarray(2), outputKey);
    assert.equal(accountToAddress(outputKey, 'signet'), trAddress);
    assert.deepEqual(accountFromAddress(trAddress), outputKey);
    assert.deepEqual(accountFromAddress(trAddress, 'signet'), outputKey);
    assert.deepEqual(accountFromAddress(trAddress, 'testnet'), outputKey, 'tb1p belongs to both testnet and signet');
    assert.throws(() => accountFromAddress(trAddress, 'regtest'), /regtest/);
    assert.deepEqual(parseAccount(trAddress, 'signet'), outputKey);
    assert.deepEqual(parseAccount(` ${trAddress.toUpperCase()} `), outputKey);
    assert.deepEqual(parseAccount(toHex(outputKey)), outputKey);
    assert.deepEqual(parseAccount(`0x${toHex(outputKey).toUpperCase()}`, 'regtest'), outputKey);
    // Whitespace and the prefix as Rust's parse_account reads them (D-133): what str::trim removes, U+0085 among it and
    // U+FEFF not, and only a lower-case 0x
    assert.deepEqual(parseAccount(`\u0085\u3000${trAddress}\u2029`, 'signet'), outputKey);
    assert.deepEqual(accountFromAddress(`${trAddress}\u0085`, 'signet'), outputKey);
    assert.deepEqual(parseAccount(`\t${toHex(outputKey)}\u0085`), outputKey);
    assert.throws(() => parseAccount(`\ufeff${trAddress}`, 'signet'));
    assert.throws(() => accountFromAddress(`${trAddress}\ufeff`, 'signet'));
    assert.throws(() => parseAccount(`0X${toHex(outputKey)}`));
    // A P2WPKH address is no longer an account (the P2WPKH accounts of D-052 were removed by D-092)
    const wpkhAddress = scriptToAddress(p2wpkhScript(compressed), 'regtest');
    assert.throws(() => deriveWalletAccount(wpkhAddress, toHex(compressed)), /Taproot/);
    assert.throws(() => accountFromAddress(wpkhAddress), /Taproot/);
    assert.throws(() => parseAccount(wpkhAddress, 'regtest'), /Taproot/);
  }
  const secret = testKey(7);
  const other = compressedPublicKey(testKey(8));
  const trAddress = scriptToAddress(p2trScript(xOnlyPublicKey(secret)), 'mainnet');
  assert.throws(() => deriveWalletAccount(trAddress, toHex(other)), /BIP86/);
  for (const script of [p2pkhScriptFromHash(new Uint8Array(20)), p2shScriptFromHash(new Uint8Array(20)), Uint8Array.of(0, 0x20, ...new Uint8Array(32))]) {
    assert.throws(() => deriveWalletAccount(scriptToAddress(script, 'mainnet'), toHex(other)), /Taproot/);
    assert.throws(() => accountFromAddress(scriptToAddress(script, 'mainnet')), /Taproot/);
  }
  // Arbitrary 32 bytes such as a contract ID: can be written as hex but has no address; a witness program off the curve is not an account
  const contractId = new Uint8Array(32).fill(0xff);
  assert.deepEqual(parseAccount(toHex(contractId)), contractId);
  assert.throws(() => accountToAddress(contractId, 'regtest'), /contract ID/i);
  assert.throws(() => accountFromAddress(scriptToAddress(Uint8Array.of(0x51, 0x20, ...contractId), 'regtest')), /curve/);
  assert.throws(() => parseAccount('ab'.repeat(31)), /62/);
  assert.throws(() => parseAccount(' '), /empty/);
  assert.throws(() => parseAccount('0x'), /empty/);
});

test('mock wallet: address, public key and the Unisat adapter; a wallet with a P2WPKH address cannot be an account', async () => {
  const secret = testKey(11);
  const tr = new MockUnisatWallet({ secretKey: secret });
  const wpkh = new MockUnisatWallet({ secretKey: secret, addressType: 'p2wpkh', network: 'signet' });
  assert.match(tr.address, /^bcrt1p/);
  assert.equal(tr.addressType, 'p2tr');
  assert.match(wpkh.address, /^tb1q/);
  assert.equal(await tr.getPublicKey(), toHex(compressedPublicKey(secret)));
  assert.deepEqual(await tr.requestAccounts(), [tr.address]);
  const account = await new UnisatAdapter(tr).getAccount();
  assert.equal(account.account, toHex(accountOf(secret)));
  assert.equal(account.address, accountToAddress(accountOf(secret), 'regtest'));
  assert.deepEqual(addressToScript(account.address), tr.script);
  await assert.rejects(new UnisatAdapter(wpkh).getAccount(), /Taproot/);
  await assert.rejects(wpkh.signMessage('m', 'bip322-simple'), /Taproot/);
  await assert.rejects(tr.signMessage('m', 'ecdsa'), /bip322-simple/);
  assert.throws(() => new UnisatAdapter(), /Unisat/);
});

test("the wallet's BTC network (D-106): Unisat's getChain and the mock wallet's chain", async () => {
  const secret = testKey(13);
  // The mock wallet reports the chain of its address's network; regtest's BITCOIN_REGTEST is its own (Unisat has no regtest)
  for (const network of ['mainnet', 'testnet', 'signet', 'regtest'] as const) {
    assert.equal((await new UnisatAdapter(new MockUnisatWallet({ secretKey: secret, network })).getNetwork())?.network, network);
  }
  const signet = new MockUnisatWallet({ secretKey: secret, network: 'signet' });
  assert.deepEqual(await signet.getChain(), { enum: 'BITCOIN_SIGNET', name: 'Bitcoin Signet', network: 'testnet' });
  assert.deepEqual(await new UnisatAdapter(signet).getNetwork(), { network: 'signet', name: 'Bitcoin Signet' });
  // The address does not pin the chain: the same tb1p address on testnet3 or testnet4, a bc1p address on Fractal Bitcoin
  for (const [network, chain, expected] of [
    ['signet', 'BITCOIN_TESTNET', { network: 'testnet', name: 'Bitcoin Testnet' }],
    ['signet', 'BITCOIN_TESTNET4', { name: 'Bitcoin Testnet4 (Beta)' }],
    ['mainnet', 'FRACTAL_BITCOIN_MAINNET', { name: 'Fractal Bitcoin' }],
    ['mainnet', 'FRACTAL_BITCOIN_TESTNET', { name: 'Fractal Bitcoin Testnet' }],
  ] as const) {
    const wallet = new MockUnisatWallet({ secretKey: secret, network, chain });
    assert.equal(wallet.address, new MockUnisatWallet({ secretKey: secret, network }).address);
    assert.deepEqual(await new UnisatAdapter(wallet).getNetwork(), expected);
  }
  // A version without getChain, or a reply without the chain's enum, cannot tell; a reply without a name is named by its enum
  const provider = (getChain?: () => Promise<unknown>): UnisatProvider =>
    ({
      requestAccounts: () => signet.requestAccounts(),
      getPublicKey: () => signet.getPublicKey(),
      signPsbt: (psbt: string) => signet.signPsbt(psbt),
      signMessage: (message: string) => signet.signMessage(message, 'bip322-simple'),
      ...(getChain === undefined ? {} : { getChain }),
    }) as UnisatProvider;
  assert.equal(await new UnisatAdapter(provider()).getNetwork(), undefined);
  for (const reply of [null, 'BITCOIN_SIGNET', { name: 'Bitcoin Signet' }, { enum: 7 }]) {
    assert.equal(await new UnisatAdapter(provider(async () => reply)).getNetwork(), undefined);
  }
  assert.deepEqual(await new UnisatAdapter(provider(async () => ({ enum: 'BITCOIN_SIGNET' }))).getNetwork(), { network: 'signet', name: 'BITCOIN_SIGNET' });
  assert.deepEqual(await new UnisatAdapter(provider(async () => ({ enum: 'constructor', name: '' }))).getNetwork(), { name: 'constructor' });
});

test('signVmopWithWallet yields a verifiable Bip322Simple signature', async () => {
  const secret = testKey(12);
  const wallet = new UnisatAdapter(new MockUnisatWallet({ secretKey: secret }));
  const unsigned = unsignedOp(accountOf(secret));
  const op = await signVmopWithWallet(unsigned, wallet);
  assert.equal(op.sig.kind, 'Bip322Simple');
  assert.ok(verifyVmopBip322(op));
  assert.equal(verifyVmop(op), 'unsupported');
  assert.ok(verifyBip322ForAccount(op.account, bip322Message(op), op.sig.bytes));
  assert.ok(!verifyVmopBip322({ ...op, nonce: 4n }), 'tampered nonce');
  assert.ok(!verifyVmopBip322({ ...op, chain_id: SIGNET.chain_id }), 'tampered chain ID');
  // Refused when the account is not the wallet account: another key's account, or this key's internal key (the pre-D-092 account)
  await assert.rejects(signVmopWithWallet(unsignedOp(accountOf(testKey(14))), wallet), /does not match/);
  await assert.rejects(signVmopWithWallet(unsignedOp(xOnlyPublicKey(secret)), wallet), /does not match/);
});

test('the self-check rejects a wrong signature returned by the wallet', async () => {
  const secret = testKey(15);
  const honest = new MockUnisatWallet({ secretKey: secret });
  const liar: WalletAdapter = {
    name: 'liar',
    getAccount: () => new UnisatAdapter(honest).getAccount(),
    signPsbt: async () => new Uint8Array(0),
    // Signs a different message
    signBip322: async () => decodeBip322Signature(await honest.signMessage('other', 'bip322-simple')),
  };
  await assert.rejects(signVmopWithWallet(unsignedOp(accountOf(secret)), liar), /self-check/);
  const op = await signVmopWithWallet(unsignedOp(accountOf(secret)), liar, { verify: false });
  assert.ok(!verifyVmopBip322(op));
});

/**
 * Unsigned PSBT of a native transaction (path C) whose inputs have no `sighash_type`: input 0 belongs to `owner` (not enough to pay
 * the fee and the change), so input 1, belonging to `other`, is selected as well.
 */
function nativePsbt(ownerSecret: Uint8Array, owner: MockUnisatWallet, other: MockUnisatWallet): { psbt: Uint8Array; op: VmOp } {
  const utxos = [
    { txid: pseudoRandomBytes(16, 32), vout: 0, value: 300n, script: owner.script },
    { txid: pseudoRandomBytes(17, 32), vout: 1, value: 50_000n, script: other.script },
  ];
  // The operation's signature is bound to the first UTXO (D-086)
  const op = signedVmop(
    {
      version: VMOP_VERSION,
      chain_id: DEVNET.chain_id,
      account: accountOf(ownerSecret),
      nonce: 0n,
      fee: { kind: 'BtcNative' },
      max_gas: 1_000n,
      action: transfer,
    },
    ownerSecret,
    new Uint8Array(32),
    nativeFirstInput(utxos[0]!),
  );
  const draft = buildNativeTx({ params: DEVNET, op, utxos, changeScript: owner.script, feeRate: 1 });
  assert.equal(draft.inputs.length, 2);
  return { psbt: draft.psbt, op };
}

test('mock wallet signPsbt: honors the sighash type and autoFinalized; signs only the inputs of this account', async () => {
  const secret = testKey(16);
  for (const addressType of ['p2tr', 'p2wpkh'] as const) {
    const wallet = new MockUnisatWallet({ secretKey: secret, addressType, auxRand: new Uint8Array(32) });
    const other = new MockUnisatWallet({ secretKey: testKey(17), addressType: addressType === 'p2tr' ? 'p2wpkh' : 'p2tr', auxRand: new Uint8Array(32) });
    const { psbt, op } = nativePsbt(secret, wallet, other);
    const hex = toHex(psbt);
    const input = (signed: string, index = 0) => Transaction.fromPSBT(fromHex(signed), SCURE_TX_OPTIONS).getInput(index);
    // Inputs without sighash_type: signed with the default type (SIGHASH_DEFAULT for P2TR, SIGHASH_ALL for P2WPKH); refused when another type is required
    const defaultType = addressType === 'p2tr' ? 0x00 : 0x01;
    await assert.rejects(wallet.signPsbt(hex, { toSignInputs: [{ index: 0, sighashTypes: [0x02] }] }), /0x[01]\b/);
    // Not finalized: tap_key_sig (64 bytes, SIGHASH_DEFAULT) / the only partial_sig
    const partial = input(
      await wallet.signPsbt(hex, { autoFinalized: false, toSignInputs: [{ index: 0, publicKey: wallet.publicKey, sighashTypes: [defaultType] }] }),
    );
    assert.equal(partial.finalScriptWitness, undefined);
    let expected: Uint8Array[];
    if (addressType === 'p2tr') {
      assert.equal(partial.tapKeySig?.length, 64);
      expected = [partial.tapKeySig!];
    } else {
      assert.equal(partial.partialSig?.length, 1);
      const [publicKey, signature] = partial.partialSig![0]!;
      assert.deepEqual(publicKey, compressedPublicKey(secret));
      assert.equal(signature[signature.length - 1], 0x01);
      expected = [signature, publicKey];
    }
    // Finalized (default): without toSignInputs only this account's inputs (input 0) are signed; the witness equals the non-finalized signature
    const signedHex = await wallet.signPsbt(hex);
    assert.deepEqual(input(signedHex).finalScriptWitness, expected);
    assert.equal(input(signedHex, 1).finalScriptWitness, undefined, 'input 1 does not belong to this account');
    // After the other party signs input 1 the native transaction is complete
    const raw = extractTransaction(fromHex(await other.signPsbt(signedHex)));
    assert.deepEqual(nativeOpFromTx(raw, DEVNET), op);
    // Public key / address is not this account
    const stranger = toHex(compressedPublicKey(testKey(18)));
    await assert.rejects(wallet.signPsbt(hex, { toSignInputs: [{ index: 0, publicKey: stranger }] }), /public key is not this account/);
    await assert.rejects(wallet.signPsbt(hex, { toSignInputs: [{ index: 0, address: 'bcrt1qxyz' }] }), /address is not this account/);
    // An input not belonging to this account
    await assert.rejects(wallet.signPsbt(hex, { toSignInputs: [{ index: 1 }] }), /BIP86/);
    const nobody = new MockUnisatWallet({ secretKey: testKey(19), addressType });
    await assert.rejects(nobody.signPsbt(hex), /PSBT/);
  }
});

/** Mock node: the account's nonce, the suggested path B fee; records the parameters `ksana_submitOp` receives. */
function fakeNode(state: { nonce: bigint; fee: bigint; submitted: unknown[][] }): typeof fetch {
  const at = { height: '1', hash: '00'.repeat(32) };
  return (async (_input: unknown, init?: RequestInit) => {
    const body = JSON.parse(String(init?.body));
    let result: unknown;
    switch (body.method) {
      case 'ksana_getAccount':
        result = { account: body.params[0], nonce: state.nonce.toString(), balances: [{ asset: '00'.repeat(32), amount: '1000000' }], locked_fees: [], at };
        break;
      case 'ksana_estimateFee':
        assert.deepEqual(body.params, []);
        result = { feerate_sat_vb: '1', path_b: { amount: state.fee.toString() }, at };
        break;
      case 'ksana_submitOp':
        state.submitted.push(body.params);
        result = { op_id: toHex(opId(decodeVmOp(fromHex(body.params[0]), SIGNET))) };
        break;
      default:
        throw new Error(`unexpected ${body.method}`);
    }
    return new Response(JSON.stringify({ jsonrpc: '2.0', id: body.id, result }), { status: 200 });
  }) as typeof fetch;
}

test('path B with the mock wallet (Unisat interface): nonce and suggested fee → wallet BIP322 signature → submit with op only', async () => {
  const secret = testKey(21);
  const mock = new MockUnisatWallet({ secretKey: secret, network: 'signet', auxRand: new Uint8Array(32).fill(3) });
  const wallet = new UnisatAdapter(mock);
  const state = { nonce: 4n, fee: 1_500n, submitted: [] as unknown[][] };
  const rpc = new KsanaRpcClient('http://node', { fetch: fakeNode(state) });
  const account = fromHex((await wallet.getAccount()).account, 32);
  const { nonce } = await rpc.getAccount(account);
  const fee = await rpc.estimateFee();
  const op = await signVmopWithWallet(
    {
      version: VMOP_VERSION,
      chain_id: SIGNET.chain_id,
      account,
      nonce,
      fee: { kind: 'Token', amount: fee.path_b.amount },
      max_gas: 50_000n,
      action: transfer,
    },
    wallet,
  );
  const { op_id } = await rpc.submitOp(op);
  assert.deepEqual(op_id, opId(op));
  // The only parameter the node receives is this operation: account, nonce and fee come from the node; the signature is BIP322 simple
  assert.equal(state.submitted.length, 1);
  assert.equal(state.submitted[0]!.length, 1);
  const received = decodeVmOp(fromHex(state.submitted[0]![0] as string), SIGNET);
  assert.deepEqual(received, op);
  assert.deepEqual(received.account, accountOf(secret));
  assert.equal(received.nonce, 4n);
  assert.deepEqual(received.fee, { kind: 'Token', amount: 1_500n });
  assert.equal(received.sig.kind, 'Bip322Simple');
  assert.ok(verifyVmopBip322(received));
});

/**
 * Mock Xverse provider: the ordinals address is `wallet` (P2TR), the payment address is P2SH-P2WPKH, the BTC network is `network`;
 * `shape` selects the response format.
 */
function fakeXverse(wallet: MockUnisatWallet, shape: 'jsonrpc' | 'status', calls: string[] = [], network = 'Mainnet'): XverseBitcoinProvider {
  const wrap = (result: unknown): unknown => (shape === 'jsonrpc' ? { jsonrpc: '2.0', id: 'x', result } : { status: 'success', result });
  return {
    async request(method: string, params?: unknown): Promise<unknown> {
      calls.push(method);
      const p = params as Record<string, unknown>;
      switch (method) {
        case 'getAccounts':
          assert.deepEqual(p.purposes, ['ordinals', 'payment']);
          return wrap([
            { address: '3Nye4j1GUFqCEBR3do2KEFZAs9oLe8NZ6X', publicKey: wallet.publicKey, purpose: 'payment', addressType: 'p2sh' },
            { address: wallet.address, publicKey: wallet.publicKey.slice(2), purpose: 'ordinals', addressType: 'p2tr' },
          ]);
        case 'signPsbt': {
          assert.equal(p.broadcast, false);
          const indices = (p.signInputs as Record<string, number[]>)[wallet.address]!;
          const psbt = base64.decode(p.psbt as string);
          // Xverse signs with the PSBT input's sighash_type (SIGHASH_DEFAULT of P2TR when absent)
          const parsed = Transaction.fromPSBT(psbt, SCURE_TX_OPTIONS);
          const toSignInputs = indices.map((index) => ({
            index,
            sighashTypes: [parsed.getInput(index).sighashType ?? 0x00],
          }));
          const signed = await wallet.signPsbt(toHex(psbt), { autoFinalized: true, toSignInputs });
          return wrap({ psbt: base64.encode(fromHex(signed)) });
        }
        case 'signMessage':
          assert.equal(p.protocol, 'BIP322');
          assert.equal(p.address, wallet.address);
          return wrap({ signature: await wallet.signMessage(p.message as string, 'bip322-simple'), address: wallet.address, protocol: 'BIP322' });
        case 'wallet_getNetwork':
          assert.equal(params, null);
          return wrap({ bitcoin: { name: network }, stacks: { name: 'mainnet' }, spark: { name: 'mainnet' } });
        default:
          throw new Error(method);
      }
    },
  };
}

for (const shape of ['jsonrpc', 'status'] as const) {
  test(`Xverse adapter (${shape} response format): ordinals address, PSBT and BIP322 signatures`, async () => {
    const secret = testKey(41);
    const mock = new MockUnisatWallet({ secretKey: secret, network: 'mainnet', auxRand: new Uint8Array(32) });
    const calls: string[] = [];
    const wallet = new XverseAdapter({ provider: fakeXverse(mock, shape, calls), message: 'Ksana' });
    const account = await wallet.getAccount();
    assert.equal(account.addressType, 'p2tr');
    assert.equal(account.address, mock.address);
    assert.equal(account.account, toHex(accountOf(secret)));
    // BIP322 signature of a VmOp
    const op = await signVmopWithWallet(unsignedOp(accountOf(secret)), wallet);
    assert.ok(verifyVmopBip322(op));
    // PSBT of a native transaction: Xverse signs with the input's default type (SIGHASH_DEFAULT) and returns a finalized PSBT
    const other = new MockUnisatWallet({ secretKey: testKey(42), addressType: 'p2wpkh' });
    const signed = await wallet.signPsbt(nativePsbt(secret, mock, other).psbt, [{ index: 0 }]);
    const input = Transaction.fromPSBT(signed, SCURE_TX_OPTIONS).getInput(0);
    assert.ok(input.finalScriptWitness !== undefined, 'Xverse returns a finalized PSBT');
    assert.equal(input.finalScriptWitness[0]!.length, 64);
    assert.ok(calls.includes('signPsbt') && calls.includes('signMessage'));
    // The payment (P2SH-P2WPKH) address is not an account (D-092)
    const payment = new XverseAdapter({ provider: fakeXverse(mock, shape), purpose: 'payment' });
    await assert.rejects(payment.getAccount(), /Taproot/);
  });
}

test("Xverse adapter: the wallet's BTC network (wallet_getNetwork, D-106)", async () => {
  const mock = new MockUnisatWallet({ secretKey: testKey(43), network: 'signet' });
  for (const shape of ['jsonrpc', 'status'] as const) {
    for (const [name, network] of [
      ['Mainnet', 'mainnet'],
      ['Testnet', 'testnet'],
      ['Signet', 'signet'],
      ['Regtest', 'regtest'],
    ] as const) {
      const calls: string[] = [];
      assert.deepEqual(await new XverseAdapter({ provider: fakeXverse(mock, shape, calls, name) }).getNetwork(), { network, name });
      assert.deepEqual(calls, ['wallet_getNetwork']);
    }
    // testnet4 shares signet's tb1 addresses but is no SDK network
    assert.deepEqual(await new XverseAdapter({ provider: fakeXverse(mock, shape, [], 'Testnet4') }).getNetwork(), { name: 'Testnet4' });
  }
  // A version without the method, a site not allowed to read the network, or a reply without bitcoin.name cannot tell
  for (const request of [
    async () => ({ jsonrpc: '2.0', id: 'x', error: { code: -32601, message: 'Method not found' } }),
    async () => ({ status: 'error', error: { code: -32002, message: 'Access denied' } }),
    async () => {
      throw new Error('unsupported method');
    },
    async () => ({ result: { stacks: { name: 'mainnet' } } }),
    async () => ({ result: { bitcoin: { name: 7 } } }),
    async () => ({ result: [] }),
  ]) {
    assert.equal(await new XverseAdapter({ provider: { request } }).getNetwork(), undefined);
  }
});

test('Xverse adapter: error responses', async () => {
  for (const response of [
    { status: 'error', error: { code: 4001, message: 'User rejected the request' } },
    { jsonrpc: '2.0', id: 'x', error: { code: 4001, message: 'User rejected the request' } },
  ]) {
    const wallet = new XverseAdapter({ provider: { request: async () => response } });
    await assert.rejects(wallet.getAccount(), (e: unknown) => e instanceof XverseError && e.code === 4001 && /User rejected/.test(e.message));
  }
  const empty = new XverseAdapter({ provider: { request: async () => ({ result: { addresses: [] } }) } });
  await assert.rejects(empty.getAccount(), /no ordinals address/);
  assert.throws(() => new XverseAdapter(), /Xverse/);
});
