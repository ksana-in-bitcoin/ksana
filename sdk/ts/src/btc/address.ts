/**
 * Conversion between Bitcoin addresses and locking scripts: bech32 (witness v0), bech32m (witness v1+, BIP350) and
 * base58check (P2PKH / P2SH); conversion between Ksana accounts and Taproot addresses (D-092).
 */

import { bech32, bech32m, createBase58check } from '@scure/base';
import { sha256 } from '@noble/hashes/sha2.js';

import { concatBytes, fromHex } from '../bytes.js';
import { isValidXOnlyPublicKey } from '../keys.js';
import { ChainMismatchError, type Params, networkOfChainId } from '../params.js';
import {
  OP_0,
  OP_1,
  isP2pkh,
  isP2sh,
  isP2tr,
  p2pkhScriptFromHash,
  p2shScriptFromHash,
  p2trScriptFromOutputKey,
  witnessProgram,
} from './script.js';

/** Bitcoin network (determines the address prefix). */
export type BtcNetwork = 'mainnet' | 'testnet' | 'signet' | 'regtest';

interface NetworkPrefixes {
  bech32: string;
  pubKeyHash: number;
  scriptHash: number;
}

const PREFIXES: Record<BtcNetwork, NetworkPrefixes> = {
  mainnet: { bech32: 'bc', pubKeyHash: 0x00, scriptHash: 0x05 },
  testnet: { bech32: 'tb', pubKeyHash: 0x6f, scriptHash: 0xc4 },
  signet: { bech32: 'tb', pubKeyHash: 0x6f, scriptHash: 0xc4 },
  regtest: { bech32: 'bcrt', pubKeyHash: 0x6f, scriptHash: 0xc4 },
};

const base58check = createBase58check(sha256);

/**
 * Removes leading and trailing whitespace as Rust's `str::trim` does (the Unicode `White_Space` property): JavaScript's `trim` also
 * removes U+FEFF and keeps U+0085, so the same text would read differently here and in the node or the CLI (D-133).
 */
function trimText(text: string): string {
  return text.replace(/^\p{White_Space}+|\p{White_Space}+$/gu, '');
}

/** The locking script decoded from an address, and the networks the address prefix belongs to. */
export interface DecodedAddress {
  /** Locking script. */
  script: Uint8Array;
  /** Possible networks (the `tb` prefix corresponds to both testnet and signet). */
  networks: BtcNetwork[];
}

function networksFor(predicate: (p: NetworkPrefixes) => boolean): BtcNetwork[] {
  return (Object.keys(PREFIXES) as BtcNetwork[]).filter((n) => predicate(PREFIXES[n]));
}

/** Decodes an address (mainnet, testnet / signet, regtest) and returns its locking script; throws for an unrecognized address. */
export function decodeAddress(address: string): DecodedAddress {
  const lower = address.toLowerCase();
  const hrp = networksFor((p) => lower.startsWith(`${p.bech32}1`));
  if (hrp.length > 0) {
    // bcrt1… also starts with "b"; take the longest prefix
    const prefix = hrp.map((n) => PREFIXES[n].bech32).sort((a, b) => b.length - a.length)[0]!;
    let decoded;
    let isBech32m = false;
    try {
      decoded = bech32.decode(address as `${string}1${string}`, 90);
    } catch {
      decoded = bech32m.decode(address as `${string}1${string}`, 90);
      isBech32m = true;
    }
    if (decoded.prefix !== prefix) throw new Error(`invalid address prefix ${decoded.prefix}`);
    const [version, ...rest] = decoded.words;
    if (version === undefined || version > 16) throw new Error('invalid witness version');
    if ((version === 0) === isBech32m) {
      throw new Error(version === 0 ? 'a witness v0 address must be bech32-encoded' : 'a witness v1 or later address must be bech32m-encoded (BIP350)');
    }
    const program = Uint8Array.from(bech32.fromWords(rest));
    if (program.length < 2 || program.length > 40) throw new Error('invalid witness program length');
    if (version === 0 && program.length !== 20 && program.length !== 32) throw new Error('a witness v0 program must be 20 or 32 bytes');
    const script = concatBytes(Uint8Array.of(version === 0 ? OP_0 : OP_1 + version - 1, program.length), program);
    return { script, networks: networksFor((p) => p.bech32 === prefix) };
  }
  const payload = base58check.decode(address);
  if (payload.length !== 21) throw new Error('invalid base58 address length');
  const version = payload[0]!;
  const hash = payload.slice(1);
  const pkh = networksFor((p) => p.pubKeyHash === version);
  if (pkh.length > 0) return { script: p2pkhScriptFromHash(hash), networks: pkh };
  const sh = networksFor((p) => p.scriptHash === version);
  if (sh.length > 0) return { script: p2shScriptFromHash(hash), networks: sh };
  throw new Error(`invalid base58 address version byte 0x${version.toString(16)}`);
}

/** Address → locking script. */
export function addressToScript(address: string): Uint8Array {
  return decodeAddress(address).script;
}

/**
 * The BTC network (address prefix) of the chain params: devnet runs on regtest, signet runs on signet (D-055), mainnet runs on
 * BTC mainnet (D-096). A chain ID of no network throws `ChainMismatchError` rather than falling back to one (A2-11, D-106).
 */
export function btcNetworkOf(params: Params): BtcNetwork {
  switch (networkOfChainId(params.chain_id)) {
    case 'devnet':
      return 'regtest';
    case 'signet':
      return 'signet';
    case 'mainnet':
      return 'mainnet';
    case null:
      throw new ChainMismatchError(params.chain_id, null, `chain ID ${params.chain_id} belongs to no Ksana network: its BTC network is unknown`);
  }
}

/**
 * Ksana account → Taproot address (D-092): the account is the 32-byte output key of the address; this merely rewrites it in
 * bech32m notation. Throws when the account is not a valid x-only public key (for example a contract ID): contracts have no
 * address.
 */
export function accountToAddress(account: Uint8Array, network: BtcNetwork): string {
  if (!isValidXOnlyPublicKey(account)) throw new Error('account is not a valid x-only public key (a contract ID has no address)');
  return scriptToAddress(p2trScriptFromOutputKey(account), network);
}

/**
 * Taproot address → Ksana account (D-092): the address must be P2TR (witness v1, 32-byte program), belong to `network` (any
 * network is accepted when it is not given), and its program must be a valid x-only public key (otherwise the address cannot be
 * spent on BTC either).
 */
export function accountFromAddress(address: string, network?: BtcNetwork): Uint8Array {
  const { script, networks } = decodeAddress(trimText(address));
  if (!isP2tr(script)) throw new Error('not a Taproot (P2TR) address: an account only corresponds to a bc1p / tb1p / bcrt1p address (D-092)');
  if (network !== undefined && !networks.includes(network)) {
    throw new Error(`the address does not belong to the ${network} network (its prefix corresponds to ${networks.join(' / ')})`);
  }
  const account = Uint8Array.from(script.subarray(2));
  if (!isValidXOnlyPublicKey(account)) throw new Error('the output key of the address is not a point on the curve');
  return account;
}

/**
 * Parses the text form of an account as `parse_account` does: 64 hex digits (any account, including contract IDs; a lower-case
 * `0x` prefix is allowed), or a Taproot address (which must belong to `network` when it is given), with the whitespace around
 * it that Rust's `str::trim` removes. Input consisting only of hex digits is parsed as hex (an address prefix always contains a
 * non-hex letter, so there is no ambiguity).
 */
export function parseAccount(text: string, network?: BtcNetwork): Uint8Array {
  const trimmed = trimText(text);
  const hex = trimmed.startsWith('0x') ? trimmed.slice(2) : trimmed;
  if (hex.length === 0) throw new Error('account is empty: enter a Taproot address or 64 hex digits');
  if (/^[0-9a-fA-F]+$/.test(hex)) {
    if (hex.length !== 64) throw new Error(`account must be 64 hex digits or a Taproot address, got ${hex.length} hex digits`);
    return fromHex(hex, 32, 'account');
  }
  return accountFromAddress(trimmed, network);
}

/** Locking script → address; scripts other than witness programs, P2PKH and P2SH have no address and throw. */
export function scriptToAddress(script: Uint8Array, network: BtcNetwork): string {
  const prefixes = PREFIXES[network];
  const wp = witnessProgram(script);
  if (wp !== null) {
    const words = [wp.version, ...bech32.toWords(wp.program)];
    return wp.version === 0 ? bech32.encode(prefixes.bech32, words, 90) : bech32m.encode(prefixes.bech32, words, 90);
  }
  if (isP2pkh(script)) return base58check.encode(concatBytes(Uint8Array.of(prefixes.pubKeyHash), script.slice(3, 23)));
  if (isP2sh(script)) return base58check.encode(concatBytes(Uint8Array.of(prefixes.scriptHash), script.slice(2, 22)));
  throw new Error('this locking script has no corresponding address');
}
