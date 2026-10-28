//! Inputs for the stable sweep (`examples/differential.rs`): the shared vectors (`tests/vectors/`), the SDK's BIP322
//! vectors and the fuzz corpus as seeds ([`Seeds`]), requests built from random values of each kind's structure
//! ([`generate`]), and byte-level mutations of both ([`mutate`]). Everything follows from the seed of an [`Rng`], so
//! a sweep can be run again exactly.

use std::fs;
use std::path::Path;
use std::str::FromStr;

use bitcoin::absolute::LockTime;
use bitcoin::consensus::serialize;
use bitcoin::script::ScriptBuf;
use bitcoin::transaction::Version;
use bitcoin::{Address, Amount, Sequence, Transaction, TxIn, TxOut, Witness, XOnlyPublicKey};
use ksana_btc::data::data_script;
use ksana_btc::envelope::envelope_script;
use ksana_primitives::codec::encode;
use ksana_primitives::{
    Action, Block, BlockHeader, Body, BtcAnchor, Entry, EntryData, FeeMode, Funds, Network,
    OutPoint, Params, Payload, SettlementTxs, SigScheme, VmOp, DEVNET,
};
use serde_json::Value;

use super::abi::{abi_defs, ABI_EVENTS, ABI_TYPES};
use super::request::{BorshType, BtcNet, Request, Signing, DECLARATIONS, FIRST_INPUT, KINDS};
use super::rust::WRITE_KEY;

/// A small deterministic generator (SplitMix64).
pub struct Rng(u64);

impl Rng {
    /// A generator from a seed.
    pub fn new(seed: u64) -> Rng {
        Rng(seed)
    }

    /// The next 64 random bits.
    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    /// A number below `n` (0 when `n` is 0).
    pub fn below(&mut self, n: usize) -> usize {
        if n == 0 {
            return 0;
        }
        usize::try_from(self.next_u64() % u64::try_from(n).unwrap_or(u64::MAX)).unwrap_or(0)
    }

    /// True `percent` times out of 100.
    pub fn chance(&mut self, percent: u64) -> bool {
        self.next_u64() % 100 < percent
    }

    /// `n` random bytes.
    pub fn bytes(&mut self, n: usize) -> Vec<u8> {
        (0..n).map(|_| self.next_u64().to_le_bytes()[0]).collect()
    }

    /// Up to `max` random bytes, short lengths more often.
    pub fn some_bytes(&mut self, max: usize) -> Vec<u8> {
        let len = if self.chance(70) {
            self.below(max.min(40) + 1)
        } else {
            self.below(max + 1)
        };
        self.bytes(len)
    }

    /// A random array.
    pub fn array<const N: usize>(&mut self) -> [u8; N] {
        let mut out = [0u8; N];
        for b in &mut out {
            *b = self.next_u64().to_le_bytes()[0];
        }
        out
    }

    /// One of `items` (which must not be empty).
    pub fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        let i = self.below(items.len());
        &items[i.min(items.len().saturating_sub(1))]
    }

    /// A u64, edge values often.
    pub fn u64_edge(&mut self) -> u64 {
        match self.below(8) {
            0 => *self.pick(&[
                0,
                1,
                2,
                0xff,
                0x100,
                u64::from(u32::MAX),
                u64::MAX,
                u64::MAX - 1,
            ]),
            1 => 1u64 << self.below(64),
            2 => self.next_u64() % 1_000,
            _ => self.next_u64(),
        }
    }

    /// A u128, edge values often.
    pub fn u128_edge(&mut self) -> u128 {
        match self.below(8) {
            0 => *self.pick(&[
                0,
                1,
                u128::from(u64::MAX),
                u128::from(u64::MAX) + 1,
                u128::MAX,
                u128::MAX - 1,
            ]),
            1 => 1u128 << self.below(128),
            2 => u128::from(self.next_u64() % 100_000),
            _ => (u128::from(self.next_u64()) << 64) | u128::from(self.next_u64()),
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Mutation

const INTERESTING_BYTES: [u8; 10] = [0x00, 0x01, 0x02, 0x4b, 0x4c, 0x4d, 0x4e, 0x7f, 0x80, 0xff];

/// Changes `bytes` by one to four random edits: bit flips, interesting bytes, inserted, deleted and duplicated runs,
/// u32 length fields set to edge values, a cut, or a splice with one of `others`.
pub fn mutate(rng: &mut Rng, bytes: &mut Vec<u8>, others: &[Vec<u8>]) {
    for _ in 0..=rng.below(4) {
        let len = bytes.len();
        let at = rng.below(len + 1);
        match rng.below(10) {
            0 if len > 0 => {
                let i = rng.below(len);
                bytes[i] ^= 1 << rng.below(8);
            }
            1 if len > 0 => {
                let i = rng.below(len);
                bytes[i] = *rng.pick(&INTERESTING_BYTES);
            }
            2 => {
                let extra = rng.some_bytes(8);
                bytes.splice(at..at, extra);
            }
            3 if len > 0 => {
                let end = (at + 1 + rng.below(8)).min(len);
                bytes.drain(at.min(len - 1)..end);
            }
            4 if len > 0 => {
                let start = rng.below(len);
                let end = (start + 1 + rng.below(16)).min(len);
                let run = bytes[start..end].to_vec();
                bytes.splice(at..at, run);
            }
            5 if len >= 4 => {
                let i = rng.below(len - 3);
                let rest = u32::try_from(len - i - 4).unwrap_or(u32::MAX);
                let value = *rng.pick(&[
                    0,
                    1,
                    rest,
                    rest.wrapping_add(1),
                    rest.wrapping_sub(1),
                    u32::MAX,
                    0x8000_0000,
                ]);
                bytes[i..i + 4].copy_from_slice(&value.to_le_bytes());
            }
            6 if len > 0 => bytes.truncate(rng.below(len)),
            7 if !others.is_empty() => {
                let other = rng.pick(others);
                let from = rng.below(other.len() + 1);
                bytes.truncate(at);
                bytes.extend_from_slice(&other[from..]);
            }
            8 => bytes.push(*rng.pick(&INTERESTING_BYTES)),
            _ => {
                let n = 1 + rng.below(3);
                let extra = rng.bytes(n);
                bytes.splice(at..at, extra);
            }
        }
    }
}

/// The byte string of a request a mutation changes.
fn bytes_of(request: &mut Request) -> Option<&mut Vec<u8>> {
    match request {
        Request::VmOp { bytes, .. }
        | Request::Payload { bytes, .. }
        | Request::Borsh { bytes, .. }
        | Request::Tx { bytes, .. }
        | Request::Abi { bytes, .. }
        | Request::AbiEvent { bytes } => Some(bytes),
        Request::Script { script } => Some(script),
        Request::Write { payload } => Some(payload),
        Request::Pow { header, .. } => Some(header),
        Request::Event { data, .. } => Some(data),
        Request::Bip322 { signature, .. } => Some(signature),
        _ => None,
    }
}

// ---------------------------------------------------------------------------------------------
// Seeds

/// What the sweep starts from: the repository's vectors and the fuzz corpus.
#[derive(Default)]
pub struct Seeds {
    /// VmOp encodings (valid and invalid).
    pub vmops: Vec<Vec<u8>>,
    /// Payload encodings for devnet.
    pub payloads: Vec<Vec<u8>>,
    /// Borsh encodings of the consensus types.
    pub borsh: Vec<(BorshType, Vec<u8>)>,
    /// BTC transactions.
    pub txs: Vec<Vec<u8>>,
    /// Block header encodings.
    pub headers: Vec<Vec<u8>>,
    /// Token standard events (valid and invalid).
    pub events: Vec<Vec<u8>>,
    /// BIP322 simple signatures: challenge script, message, signature.
    pub bip322: Vec<(Vec<u8>, Vec<u8>, Vec<u8>)>,
    /// Data scripts and envelope leaves.
    pub scripts: Vec<Vec<u8>>,
}

fn read_json(path: &Path) -> Option<Value> {
    serde_json::from_str(&fs::read_to_string(path).ok()?).ok()
}

fn hex_field(case: &Value, key: &str) -> Option<Vec<u8>> {
    hex::decode(case.get(key)?.as_str()?).ok()
}

fn corpus(dir: &Path) -> Vec<Vec<u8>> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut files: Vec<_> = entries.filter_map(|e| e.ok().map(|e| e.path())).collect();
    files.sort();
    files
        .iter()
        .filter_map(|f| fs::read(f).ok())
        .filter(|b| b.len() <= 1 << 16)
        .collect()
}

/// Standard base64 (a BIP322 signature as wallets give it).
fn base64(text: &str) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let mut acc = 0u32;
    let mut bits = 0;
    for c in text.bytes().filter(|&c| c != b'=') {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        };
        acc = (acc << 6) | u32::from(v);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(u8::try_from((acc >> bits) & 0xff).ok()?);
        }
    }
    Some(out)
}

impl Seeds {
    /// Loads the seeds of the repository at `root` (missing files give fewer seeds).
    pub fn load(root: &Path) -> Seeds {
        let mut seeds = Seeds::default();
        let vectors = root.join("tests/vectors");
        if let Some(codec) = read_json(&vectors.join("codec.json")) {
            for case in ["valid", "invalid"]
                .iter()
                .filter_map(|k| codec.get(*k)?.as_array())
                .flatten()
            {
                let (Some(ty), Some(bytes)) = (case["type"].as_str(), hex_field(case, "borsh"))
                else {
                    continue;
                };
                match ty {
                    "Payload" => seeds.payloads.push(bytes),
                    "VmOp" => {
                        seeds.vmops.push(bytes.clone());
                        seeds.borsh.push((BorshType::VmOp, bytes));
                    }
                    "BlockHeader" => {
                        seeds.headers.push(bytes.clone());
                        seeds.borsh.push((BorshType::BlockHeader, bytes));
                    }
                    other => {
                        if let Some(t) = BorshType::from_name(other) {
                            seeds.borsh.push((t, bytes));
                        }
                    }
                }
            }
        }
        if let Some(signing) = read_json(&vectors.join("signing.json")) {
            for case in signing["cases"].as_array().into_iter().flatten() {
                if let Some(bytes) = hex_field(case, "borsh") {
                    seeds.vmops.push(bytes);
                }
            }
            for case in signing["bip322"].as_array().into_iter().flatten() {
                if let Some(bytes) = hex_field(case, "op_borsh") {
                    seeds.vmops.push(bytes);
                }
            }
        }
        if let Some(pow) = read_json(&vectors.join("pow.json")) {
            for case in pow["cases"].as_array().into_iter().flatten() {
                if let Ok(header) = serde_json::from_value::<BlockHeader>(case["header"].clone()) {
                    if let Ok(bytes) = encode(&header) {
                        seeds.headers.push(bytes);
                    }
                }
            }
        }
        if let Some(standards) = read_json(&vectors.join("standards.json")) {
            for case in ["events", "invalid_events"]
                .iter()
                .filter_map(|k| standards.get(*k)?.as_array())
                .flatten()
            {
                if let Some(bytes) = hex_field(case, "borsh") {
                    seeds.events.push(bytes);
                }
            }
        }
        let bip322 = root.join("sdk/ts/test/data/bip322");
        for file in ["basic-test-vectors.json", "generated-test-vectors.json"] {
            let Some(data) = read_json(&bip322.join(file)) else {
                continue;
            };
            for case in data["simple"].as_array().into_iter().flatten() {
                let (Some(address), Some(message)) =
                    (case["address"].as_str(), case["message"].as_str())
                else {
                    continue;
                };
                let Ok(address) = Address::from_str(address) else {
                    continue;
                };
                let script = address.assume_checked().script_pubkey().to_bytes();
                for sig in case["bip322_signatures"].as_array().into_iter().flatten() {
                    let text = sig.as_str().unwrap_or_default();
                    if let Some(sig) = base64(text.strip_prefix("smp").unwrap_or(text)) {
                        seeds
                            .bip322
                            .push((script.clone(), message.as_bytes().to_vec(), sig));
                    }
                }
            }
        }
        let fuzz = root.join("fuzz/corpus");
        seeds.vmops.extend(corpus(&fuzz.join("vmop_decode")));
        for bytes in corpus(&fuzz.join("payload_decode")) {
            seeds.scripts.push(bytes.clone());
            seeds.payloads.push(bytes);
        }
        seeds.txs.extend(corpus(&fuzz.join("btc_inputs")));
        seeds
    }

    /// Every seed as a request of its own, unchanged: payloads and transactions on every network, headers' PoW on
    /// every network, each event with each declaration's standard.
    pub fn requests(&self) -> Vec<Request> {
        let mut out = Vec::new();
        for bytes in &self.vmops {
            out.push(Request::VmOp {
                bytes: bytes.clone(),
                sign: None,
            });
        }
        for network in Network::ALL {
            for bytes in &self.payloads {
                out.push(Request::Payload {
                    network,
                    bytes: bytes.clone(),
                });
            }
            for bytes in &self.txs {
                out.push(Request::Tx {
                    network,
                    bytes: bytes.clone(),
                });
            }
            for header in &self.headers {
                out.push(Request::Pow {
                    network,
                    header: header.clone(),
                    weight: 1,
                });
            }
            out.push(Request::Params { network });
        }
        for (ty, bytes) in &self.borsh {
            out.push(Request::Borsh {
                ty: *ty,
                bytes: bytes.clone(),
            });
        }
        for (i, data) in self.events.iter().enumerate() {
            let standards = DECLARATIONS[i % DECLARATIONS.len()];
            out.push(Request::Event {
                standards: standards.iter().map(|s| s.to_string()).collect(),
                data: data.clone(),
            });
        }
        for (challenge, message, signature) in &self.bip322 {
            out.push(Request::Bip322 {
                challenge: challenge.clone(),
                account: challenge
                    .get(2..34)
                    .and_then(|a| a.try_into().ok())
                    .unwrap_or([0; 32]),
                message: message.clone(),
                signature: signature.clone(),
            });
        }
        for script in &self.scripts {
            out.push(Request::Script {
                script: script.clone(),
            });
        }
        out
    }

    /// Every seed as one list of byte strings (what mutations splice from).
    pub fn all_bytes(&self) -> Vec<Vec<u8>> {
        let mut out: Vec<Vec<u8>> = Vec::new();
        out.extend(self.vmops.iter().cloned());
        out.extend(self.payloads.iter().cloned());
        out.extend(self.txs.iter().cloned());
        out.extend(self.events.iter().cloned());
        out.extend(self.headers.iter().cloned());
        out
    }
}

// ---------------------------------------------------------------------------------------------
// Values

/// The secret keys ops are signed with (valid keys).
const SECRETS: [[u8; 32]; 3] = [[0x33; 32], [0x01; 32], [0x77; 32]];

/// Strings with the characters decoders tell apart: a byte order mark, NUL, non-ASCII letters and symbols, the
/// largest code point, line and paragraph separators, a next-line control.
const STRINGS: &[&str] = &[
    "",
    "transfer",
    "\u{feff}transfer",
    "transfer\u{feff}",
    "\u{feff}",
    "\u{feff}\u{feff}a",
    "überweisen",
    "ñ",
    "ṇ",
    "₿",
    "🦀",
    "\u{0}",
    "a\u{0}b",
    "\u{fffd}",
    "\u{10ffff}",
    "\u{d7ff}\u{e000}",
    "\u{2028}\u{2029}",
    "\u{85}",
    "e\u{301}",
    "KS-20",
    "KS-721",
];

fn gen_string(rng: &mut Rng) -> String {
    match rng.below(4) {
        0 | 1 => rng.pick(STRINGS).to_string(),
        2 => {
            let n = rng.below(12);
            (0..n)
                .filter_map(|_| {
                    let c = match rng.below(4) {
                        0 => rng.next_u64() % 0x80,
                        1 => rng.next_u64() % 0x800,
                        2 => rng.next_u64() % 0x1_0000,
                        _ => rng.next_u64() % 0x11_0000,
                    };
                    char::from_u32(u32::try_from(c).ok()?)
                })
                .collect()
        }
        _ => {
            let n = *rng.pick(&[64, 255, 256, 1_000, 5_000]);
            "a".repeat(n)
        }
    }
}

fn gen_funds(rng: &mut Rng) -> Vec<Funds> {
    let n = if rng.chance(60) {
        rng.below(3)
    } else {
        rng.below(18)
    };
    (0..n)
        .map(|i| Funds {
            asset: if rng.chance(30) {
                let mut a = [0u8; 32];
                a[31] = u8::try_from(i).unwrap_or(0);
                a
            } else {
                rng.array()
            },
            amount: rng.u128_edge(),
        })
        .collect()
}

fn gen_action(rng: &mut Rng) -> Action {
    match rng.below(3) {
        0 => Action::Deploy {
            code: if rng.chance(50) {
                [b"\0asm\x01\0\0\0".to_vec(), rng.some_bytes(200)].concat()
            } else {
                rng.some_bytes(300)
            },
            init_args: rng.some_bytes(64),
            funds: gen_funds(rng),
        },
        1 => Action::Call {
            contract: rng.array(),
            method: gen_string(rng),
            args: rng.some_bytes(100),
            funds: gen_funds(rng),
        },
        _ => Action::AssetTransfer {
            asset: if rng.chance(50) { [0; 32] } else { rng.array() },
            to: rng.array(),
            amount: rng.u128_edge(),
        },
    }
}

/// A VmOp for `params`, signed properly most of the time, and the secret key of its account when it has one.
fn gen_op(rng: &mut Rng, params: &Params, native: Option<bool>) -> (VmOp, Option<[u8; 32]>) {
    let secret = *rng.pick(&SECRETS);
    let keypair = ksana_crypto::keypair_from_secret(&secret).ok();
    let own = keypair.is_some() && rng.chance(85);
    let account = match (&keypair, own) {
        (Some(keypair), true) => ksana_crypto::account_of(keypair),
        _ => rng.array(),
    };
    let native = native.unwrap_or_else(|| rng.chance(40));
    let mut op = VmOp {
        version: *rng.pick(&[2, 2, 2, 2, 1, 0, 3, 255]),
        chain_id: if rng.chance(90) {
            params.chain_id
        } else {
            u32::try_from(rng.next_u64() & 0xffff_ffff).unwrap_or(0)
        },
        account,
        nonce: rng.u64_edge(),
        fee: if native {
            FeeMode::BtcNative
        } else {
            FeeMode::Token {
                amount: rng.u128_edge(),
            }
        },
        max_gas: rng.u64_edge(),
        action: gen_action(rng),
        sig: if rng.chance(50) {
            SigScheme::Bip340(rng.array())
        } else {
            SigScheme::Bip322Simple(rng.some_bytes(120))
        },
    };
    if let (Some(keypair), true) = (&keypair, rng.chance(70)) {
        let aux: [u8; 32] = rng.array();
        let bip322 = rng.chance(40);
        let signed = match (&op.fee, bip322) {
            (FeeMode::Token { .. }, false) => ksana_crypto::sign_vmop(&op, keypair, &aux),
            (FeeMode::Token { .. }, true) => {
                ksana_crypto::sign_vmop_bip322_p2tr(&op, keypair, &aux)
            }
            (FeeMode::BtcNative, false) => {
                ksana_crypto::sign_native_vmop(&op, &FIRST_INPUT, keypair, &aux)
            }
            (FeeMode::BtcNative, true) => {
                ksana_crypto::sign_native_vmop_bip322_p2tr(&op, &FIRST_INPUT, keypair, &aux)
            }
        };
        if let Ok(sig) = signed {
            op.sig = sig;
        }
    }
    (op, own.then_some(secret))
}

fn gen_header(rng: &mut Rng, previous: Option<&BlockHeader>) -> BlockHeader {
    let same = |rng: &mut Rng| previous.is_some() && rng.chance(60);
    let parents = match previous {
        Some(p) if rng.chance(70) => {
            let mut parents = vec![p.hash()];
            if rng.chance(20) {
                parents.push(rng.array());
            }
            parents
        }
        _ => (0..rng.below(4)).map(|_| rng.array()).collect(),
    };
    BlockHeader {
        version: if same(rng) {
            previous.map_or(2, |p| p.version)
        } else {
            *rng.pick(&[2, 2, 2, 1, 3, u32::MAX])
        },
        height: previous.map_or_else(|| rng.u64_edge(), |p| p.height.wrapping_add(1)),
        parents,
        btc_anchor: if same(rng) {
            previous.map_or(
                BtcAnchor {
                    height: 0,
                    hash: [0; 32],
                },
                |p| p.btc_anchor.clone(),
            )
        } else {
            BtcAnchor {
                height: u32::try_from(rng.u64_edge() & 0xffff_ffff).unwrap_or(0),
                hash: rng.array(),
            }
        },
        entries_root: rng.array(),
        state_root: if same(rng) {
            previous.map_or([0; 32], |p| p.state_root)
        } else {
            rng.array()
        },
        producer: if same(rng) {
            previous.map_or([0; 32], |p| p.producer)
        } else {
            rng.array()
        },
        timestamp: match previous {
            Some(p) if rng.chance(70) => p
                .timestamp
                .wrapping_add(rng.next_u64() % 20)
                .wrapping_sub(5),
            _ => rng.u64_edge(),
        },
        bits: if same(rng) {
            previous.map_or(0, |p| p.bits)
        } else {
            *rng.pick(&[DEVNET.pow_limit_bits, 0x1f00_ffff, 0x1d00_ffff, u32::MAX, 0])
        },
        nonce: rng.u64_edge(),
    }
}

fn gen_headers(rng: &mut Rng, max: usize) -> Vec<BlockHeader> {
    let n = rng.below(max + 1);
    let mut out: Vec<BlockHeader> = Vec::with_capacity(n);
    for _ in 0..n {
        let header = gen_header(rng, out.last());
        out.push(header);
    }
    out
}

fn gen_entry_data(rng: &mut Rng, params: &Params) -> EntryData {
    match rng.below(3) {
        0 => EntryData::Forced {
            btc_txid: rng.array(),
        },
        1 => {
            let (mut op, _) = gen_op(rng, params, Some(false));
            op.chain_id = params.chain_id;
            EntryData::Sponsored { op }
        }
        _ => EntryData::Unlock {
            height: rng.u64_edge(),
            index: u32::try_from(rng.u64_edge() & 0xffff_ffff).unwrap_or(0),
        },
    }
}

fn gen_bodies(rng: &mut Rng, params: &Params, headers: usize) -> Vec<Body> {
    let mut out = Vec::new();
    let mut index = 0u32;
    while rng.chance(40) && out.len() < 3 {
        index = index.saturating_add(u32::try_from(rng.below(2)).unwrap_or(0));
        if usize::try_from(index).unwrap_or(usize::MAX) >= headers && rng.chance(80) {
            break;
        }
        let entries = (0..1 + rng.below(3))
            .map(|_| gen_entry_data(rng, params))
            .collect();
        out.push(Body {
            header: index,
            entries,
        });
        index = index.saturating_add(1);
    }
    out
}

fn gen_payload(rng: &mut Rng, params: &Params) -> Payload {
    match rng.below(3) {
        0 => {
            let native = rng.chance(85);
            Payload::NativeOp {
                op: gen_op(rng, params, Some(native)).0,
            }
        }
        1 => {
            let headers = gen_headers(rng, 4);
            let bodies = gen_bodies(rng, params, headers.len().saturating_sub(1));
            let entries = (0..rng.below(4))
                .map(|_| gen_entry_data(rng, params))
                .collect();
            Payload::Settlement {
                headers,
                entries,
                bodies,
            }
        }
        _ => {
            let headers = gen_headers(rng, 4);
            let bodies = gen_bodies(rng, params, headers.len());
            Payload::Headers {
                headers,
                bodies,
                payee: if rng.chance(30) { [0; 32] } else { rng.array() },
            }
        }
    }
}

fn gen_entry(rng: &mut Rng) -> Entry {
    match rng.below(3) {
        0 => Entry::Forced {
            btc_txid: rng.array(),
            op: gen_op(rng, &DEVNET, Some(true)).0,
        },
        1 => Entry::Sponsored {
            op: gen_op(rng, &DEVNET, Some(false)).0,
        },
        _ => Entry::Unlock {
            height: rng.u64_edge(),
            index: u32::try_from(rng.u64_edge() & 0xffff_ffff).unwrap_or(0),
        },
    }
}

fn gen_borsh(rng: &mut Rng, ty: BorshType) -> Vec<u8> {
    let encoded = match ty {
        BorshType::OutPoint => encode(&OutPoint {
            txid: rng.array(),
            vout: u32::try_from(rng.u64_edge() & 0xffff_ffff).unwrap_or(0),
        }),
        BorshType::FeeMode => encode(&if rng.chance(50) {
            FeeMode::BtcNative
        } else {
            FeeMode::Token {
                amount: rng.u128_edge(),
            }
        }),
        BorshType::Funds => encode(&Funds {
            asset: rng.array(),
            amount: rng.u128_edge(),
        }),
        BorshType::Action => encode(&gen_action(rng)),
        BorshType::SigScheme => encode(&gen_op(rng, &DEVNET, None).0.sig),
        BorshType::VmOp => encode(&gen_op(rng, &DEVNET, None).0),
        BorshType::BtcAnchor => encode(&BtcAnchor {
            height: u32::try_from(rng.u64_edge() & 0xffff_ffff).unwrap_or(0),
            hash: rng.array(),
        }),
        BorshType::BlockHeader => encode(&gen_header(rng, None)),
        BorshType::Entry => encode(&gen_entry(rng)),
        BorshType::EntryData => encode(&gen_entry_data(rng, &DEVNET)),
        BorshType::Block => encode(&Block {
            header: gen_header(rng, None),
            entries: (0..rng.below(4)).map(|_| gen_entry(rng)).collect(),
            settlement: rng.chance(30).then(|| SettlementTxs {
                commit: rng.some_bytes(200),
                reveal: rng.some_bytes(200),
            }),
        }),
    };
    encoded.unwrap_or_default()
}

// ---------------------------------------------------------------------------------------------
// BTC transactions and scripts

fn write_key() -> Option<XOnlyPublicKey> {
    XOnlyPublicKey::from_slice(&WRITE_KEY).ok()
}

/// Rewrites a data push of `data` with a longer opcode than needed (a non-minimal push).
fn push_long(data: &[u8], rng: &mut Rng) -> Vec<u8> {
    let len = data.len();
    let mut out = match rng.below(3) {
        0 if len <= 0xff => vec![0x4c, u8::try_from(len).unwrap_or(0)],
        1 if len <= 0xffff => {
            let mut v = vec![0x4d];
            v.extend(u16::try_from(len).unwrap_or(0).to_le_bytes());
            v
        }
        _ => {
            let mut v = vec![0x4e];
            v.extend(u32::try_from(len).unwrap_or(0).to_le_bytes());
            v
        }
    };
    out.extend_from_slice(data);
    out
}

fn push(data: &[u8]) -> Vec<u8> {
    let len = data.len();
    let mut out = if len <= 0x4b {
        vec![u8::try_from(len).unwrap_or(0)]
    } else if len <= 0xff {
        vec![0x4c, u8::try_from(len).unwrap_or(0)]
    } else if len <= 0xffff {
        let mut v = vec![0x4d];
        v.extend(u16::try_from(len).unwrap_or(0).to_le_bytes());
        v
    } else {
        let mut v = vec![0x4e];
        v.extend(u32::try_from(len).unwrap_or(0).to_le_bytes());
        v
    };
    out.extend_from_slice(data);
    out
}

/// A payload to carry: a native op, a settlement or republish payload, or arbitrary bytes.
fn carried(rng: &mut Rng, params: &Params) -> Vec<u8> {
    if rng.chance(15) {
        return rng.some_bytes(80);
    }
    gen_payload(rng, params)
        .encode(params)
        .unwrap_or_else(|_| vec![1])
}

/// A data script or an envelope leaf, often with a non-minimal push, a stray opcode, a cut, an oversized chunk or
/// bytes after the end.
fn gen_script(rng: &mut Rng, params: &Params) -> Vec<u8> {
    let payload = carried(rng, params);
    let magic = &params.magic;
    let envelope = rng.chance(50);
    let mut chunks: Vec<Vec<u8>> = payload.chunks(520).map(<[u8]>::to_vec).collect();
    if rng.chance(15) {
        chunks.push(Vec::new());
    }
    if rng.chance(10) {
        chunks.push(rng.bytes(521));
    }
    let mut body = Vec::new();
    for chunk in &chunks {
        body.extend(if rng.chance(15) {
            push_long(chunk, rng)
        } else {
            push(chunk)
        });
    }
    if rng.chance(8) {
        body.push(*rng.pick(&[0x51, 0x4f, 0x60, 0x6a, 0x68]));
    }
    let magic_push = if rng.chance(10) {
        push_long(magic, rng)
    } else if rng.chance(5) {
        push(b"KSN2")
    } else {
        push(magic)
    };
    let mut script = if envelope {
        let mut s = Vec::new();
        if rng.chance(5) {
            s.extend(push(&rng.bytes(33)));
        } else {
            s.extend(push(&WRITE_KEY));
        }
        s.extend([0xac, 0x00, 0x63]);
        s.extend(magic_push);
        s.extend(body);
        if !rng.chance(5) {
            s.push(0x68);
        }
        s
    } else {
        let mut s = vec![0x6a];
        s.extend(magic_push);
        s.extend(body);
        s
    };
    if rng.chance(8) {
        script.extend(rng.some_bytes(3));
    }
    if rng.chance(8) && !script.is_empty() {
        let cut = rng.below(script.len());
        script.truncate(cut);
    }
    script
}

fn envelope_witness(rng: &mut Rng, leaf: Vec<u8>) -> Witness {
    let mut items = vec![
        rng.bytes(64),
        leaf,
        [vec![0xc0], WRITE_KEY.to_vec()].concat(),
    ];
    if rng.chance(15) {
        items.push([vec![0x50], rng.some_bytes(10)].concat());
    }
    Witness::from_slice(&items)
}

fn gen_witness(rng: &mut Rng, params: &Params) -> Witness {
    match rng.below(7) {
        0 => Witness::new(),
        1 => Witness::from_slice(&[rng.bytes(64)]),
        2 => Witness::from_slice(&[rng.bytes(65)]),
        3 | 4 => {
            let leaf = match rng.below(3) {
                0 => gen_script(rng, params),
                _ => {
                    let payload = carried(rng, params);
                    write_key()
                        .and_then(|key| envelope_script(&key, &params.magic, &payload).ok())
                        .map(|s| s.to_bytes())
                        .unwrap_or_default()
                }
            };
            envelope_witness(rng, leaf)
        }
        5 => Witness::from_slice(&[rng.bytes(72), rng.bytes(33)]),
        _ => Witness::from_slice(
            &(0..rng.below(5))
                .map(|_| rng.some_bytes(40))
                .collect::<Vec<_>>(),
        ),
    }
}

fn data_output(rng: &mut Rng, params: &Params) -> ScriptBuf {
    if rng.chance(25) {
        return ScriptBuf::from_bytes(gen_script(rng, params));
    }
    let payload = carried(rng, params);
    data_script(&params.magic, &payload).unwrap_or_default()
}

fn gen_tx(rng: &mut Rng, params: &Params) -> Vec<u8> {
    let inputs = 1 + rng.below(3);
    let coinbase = rng.chance(5);
    let input: Vec<TxIn> = (0..inputs)
        .map(|i| TxIn {
            previous_output: if coinbase && i == 0 {
                bitcoin::OutPoint::null()
            } else if i == 0 && rng.chance(50) {
                bitcoin::OutPoint {
                    txid: bitcoin::Txid::from_byte_array_compat(FIRST_INPUT.txid),
                    vout: FIRST_INPUT.vout,
                }
            } else {
                bitcoin::OutPoint {
                    txid: bitcoin::Txid::from_byte_array_compat(rng.array()),
                    vout: u32::try_from(rng.u64_edge() & 0xffff_ffff).unwrap_or(0),
                }
            },
            script_sig: ScriptBuf::from_bytes(if rng.chance(20) {
                rng.some_bytes(30)
            } else {
                Vec::new()
            }),
            sequence: Sequence(*rng.pick(&[u32::MAX, 0xffff_fffd, 0, 1])),
            witness: gen_witness(rng, params),
        })
        .collect();
    let outputs = rng.below(4);
    let output: Vec<TxOut> = (0..outputs)
        .map(|_| TxOut {
            value: Amount::from_sat(rng.u64_edge() % 2_100_000_000_000_000),
            script_pubkey: match rng.below(5) {
                0 | 1 => data_output(rng, params),
                2 => ScriptBuf::from_bytes([vec![0x6a], push(&rng.some_bytes(30))].concat()),
                3 => ScriptBuf::from_bytes([vec![0x51, 0x20], rng.bytes(32)].concat()),
                _ => ScriptBuf::from_bytes(rng.some_bytes(30)),
            },
        })
        .collect();
    let tx = Transaction {
        version: Version(*rng.pick(&[1, 2, 2, 3, -1])),
        lock_time: LockTime::from_consensus(if rng.chance(70) {
            0
        } else {
            u32::try_from(rng.next_u64() & 0xffff_ffff).unwrap_or(0)
        }),
        input,
        output,
    };
    serialize(&tx)
}

/// Builds a txid from bytes in internal order.
trait TxidCompat {
    fn from_byte_array_compat(bytes: [u8; 32]) -> Self;
}

impl TxidCompat for bitcoin::Txid {
    fn from_byte_array_compat(bytes: [u8; 32]) -> Self {
        use bitcoin::hashes::Hash;
        bitcoin::Txid::from_byte_array(bytes)
    }
}

// ---------------------------------------------------------------------------------------------
// Text

/// Whitespace on one side only of Rust's `str::trim` and JavaScript's `String.prototype.trim`, and around both.
const SPACES: &[&str] = &[
    " ", "\t", "\n", "\r", "\u{b}", "\u{c}", "\u{85}", "\u{a0}", "\u{1680}", "\u{2000}",
    "\u{200a}", "\u{2028}", "\u{2029}", "\u{202f}", "\u{205f}", "\u{3000}", "\u{feff}", "\u{180e}",
    "\u{200b}",
];

fn gen_address_text(rng: &mut Rng) -> String {
    let network = *rng.pick(&BtcNet::ALL);
    let script: Vec<u8> = match rng.below(9) {
        0..=3 => {
            let secret = *rng.pick(&SECRETS);
            let account = ksana_crypto::keypair_from_secret(&secret)
                .map(|k| ksana_crypto::account_of(&k))
                .unwrap_or([0; 32]);
            [vec![0x51, 0x20], account.to_vec()].concat()
        }
        4 => [vec![0x51, 0x20], rng.bytes(32)].concat(),
        5 => [vec![0x00, 0x14], rng.bytes(20)].concat(),
        6 => [vec![0x00, 0x20], rng.bytes(32)].concat(),
        7 => [vec![0x52, 0x10], rng.bytes(16)].concat(),
        _ => [vec![0x76, 0xa9, 0x14], rng.bytes(20), vec![0x88, 0xac]].concat(),
    };
    let mut text = Address::from_script(&ScriptBuf::from_bytes(script), network.bitcoin())
        .map(|a| a.to_string())
        .unwrap_or_default();
    if rng.chance(25) {
        // An account's text as hex, with or without a prefix
        let digits: usize = *rng.pick(&[64, 64, 64, 63, 65, 0]);
        text = hex::encode(rng.bytes(digits.div_ceil(2)))[..digits].to_string();
        if rng.chance(30) {
            text = text.to_uppercase();
        }
        if rng.chance(50) {
            text = format!("{}{text}", rng.pick(&["0x", "0X", "0x0x", "x"]));
        }
    }
    match rng.below(10) {
        0 => text = text.to_uppercase(),
        1 if !text.is_empty() => {
            // One character's case changed
            let i = rng.below(text.len());
            text = text
                .char_indices()
                .map(|(j, c)| if j == i { c.to_ascii_uppercase() } else { c })
                .collect();
        }
        2 if !text.is_empty() => {
            // One character changed (a bad checksum)
            let i = rng.below(text.len());
            text = text
                .char_indices()
                .map(|(j, c)| {
                    if j == i {
                        if c == 'q' {
                            'p'
                        } else {
                            'q'
                        }
                    } else {
                        c
                    }
                })
                .collect();
        }
        _ => {}
    }
    if rng.chance(40) {
        let before = rng.pick(SPACES);
        let after = rng.pick(SPACES);
        text = match rng.below(3) {
            0 => format!("{before}{text}"),
            1 => format!("{text}{after}"),
            _ => format!("{before}{text}{after}"),
        };
    }
    text
}

// ---------------------------------------------------------------------------------------------
// Events and ABI values

fn borsh_string(s: &[u8]) -> Vec<u8> {
    let mut out = u32::try_from(s.len()).unwrap_or(0).to_le_bytes().to_vec();
    out.extend_from_slice(s);
    out
}

fn gen_name(rng: &mut Rng, names: &[&str]) -> Vec<u8> {
    let name = rng.pick(names).as_bytes().to_vec();
    match rng.below(12) {
        0 => [b"\xef\xbb\xbf".to_vec(), name].concat(),
        1 => [name, vec![0]].concat(),
        2 => name.to_ascii_uppercase(),
        3 => [b"\xef\xbb\xbf\xef\xbb\xbf".to_vec(), name].concat(),
        4 => [name, b"\xef\xbb\xbf".to_vec()].concat(),
        _ => name,
    }
}

/// Borsh bytes for a playground `AbiType` (as JSON), with random values: mostly well-formed, sometimes a bad tag,
/// a bad length or invalid UTF-8.
fn gen_abi_value(rng: &mut Rng, ty: &Value, defs: &Value, depth: usize) -> Vec<u8> {
    if depth > 6 {
        return Vec::new();
    }
    let kind = ty["kind"].as_str().unwrap_or_default();
    match kind {
        "int" => {
            let bits: usize = ty["name"].as_str().unwrap_or("u8")[1..]
                .parse()
                .unwrap_or(8);
            let mut v = (u128::from(rng.next_u64()) << 64 | u128::from(rng.next_u64()))
                .to_le_bytes()
                .to_vec();
            if rng.chance(30) {
                v = rng.u128_edge().to_le_bytes().to_vec();
            }
            v.truncate(bits / 8);
            v
        }
        "bool" => vec![if rng.chance(90) {
            u8::from(rng.chance(50))
        } else {
            *rng.pick(&[2, 0xff])
        }],
        "string" => {
            if rng.chance(10) {
                // Invalid UTF-8: a lone continuation byte, an overlong encoding, a surrogate
                const BAD: [&[u8]; 4] = [
                    &[0x80],
                    &[0xc0, 0xaf],
                    &[0xed, 0xa0, 0x80],
                    &[0xf4, 0x90, 0x80, 0x80],
                ];
                borsh_string(rng.pick(&BAD))
            } else {
                borsh_string(gen_string(rng).as_bytes())
            }
        }
        "bytes" => borsh_string(&rng.some_bytes(40)),
        "fixed" => rng.bytes(usize::try_from(ty["len"].as_u64().unwrap_or(1)).unwrap_or(1)),
        "id" => rng.bytes(32),
        "option" => match rng.below(10) {
            0..=3 => vec![0],
            9 => vec![*rng.pick(&[2, 0xff])],
            _ => [vec![1], gen_abi_value(rng, &ty["inner"], defs, depth + 1)].concat(),
        },
        "vec" => {
            let n = if rng.chance(90) {
                rng.below(4)
            } else {
                *rng.pick(&[5, 100, 0xffff_ffff])
            };
            let mut out = u32::try_from(n).unwrap_or(u32::MAX).to_le_bytes().to_vec();
            for _ in 0..n.min(4) {
                out.extend(gen_abi_value(rng, &ty["inner"], defs, depth + 1));
            }
            out
        }
        "tuple" => ty["items"]
            .as_array()
            .into_iter()
            .flatten()
            .flat_map(|item| gen_abi_value(rng, item, defs, depth + 1))
            .collect(),
        "struct" => {
            let name = ty["name"].as_str().unwrap_or_default();
            defs["structs"][name]["fields"]
                .as_array()
                .into_iter()
                .flatten()
                .flat_map(|f| gen_abi_value(rng, &f["type"], defs, depth + 1))
                .collect()
        }
        "enum" => {
            let name = ty["name"].as_str().unwrap_or_default();
            let variants = defs["enums"][name]["variants"]
                .as_array()
                .cloned()
                .unwrap_or_default();
            let tag = if rng.chance(90) {
                rng.below(variants.len())
            } else {
                variants.len() + rng.below(3)
            };
            let mut out = vec![u8::try_from(tag).unwrap_or(0xff)];
            if let Some(v) = variants.get(tag) {
                for f in v["fields"].as_array().into_iter().flatten() {
                    out.extend(gen_abi_value(rng, f, defs, depth + 1));
                }
            }
            out
        }
        _ => Vec::new(),
    }
}

/// A KS-20 or KS-721 event, built field by field (the reference layouts of `docs/STANDARDS.md`).
fn gen_event(rng: &mut Rng) -> Vec<u8> {
    let ks721 = rng.chance(50);
    let names: &[&str] = if ks721 {
        &[
            "standards",
            "metadata",
            "transfer",
            "approve",
            "operator",
            "uri",
        ]
    } else {
        &["standards", "metadata", "transfer", "approve", "minter"]
    };
    let name = gen_name(rng, names);
    let plain = String::from_utf8_lossy(&name)
        .trim_start_matches('\u{feff}')
        .to_ascii_lowercase();
    let account = |rng: &mut Rng| -> Vec<u8> {
        if rng.chance(20) {
            vec![0; 32]
        } else {
            rng.bytes(32)
        }
    };
    let option = |rng: &mut Rng, inner: Vec<u8>| -> Vec<u8> {
        match rng.below(10) {
            0..=3 => vec![0],
            9 => vec![2],
            _ => [vec![1], inner].concat(),
        }
    };
    let mut out = borsh_string(&name);
    match plain.trim_end_matches('\0') {
        "standards" => {
            let ids: Vec<&str> = rng.pick(&DECLARATIONS).to_vec();
            out.extend(u32::try_from(ids.len()).unwrap_or(0).to_le_bytes());
            for id in ids {
                out.extend(borsh_string(id.as_bytes()));
            }
        }
        "metadata" => {
            out.extend(borsh_string(gen_string(rng).as_bytes()));
            out.extend(borsh_string(gen_string(rng).as_bytes()));
            if !ks721 {
                out.push(u8::try_from(rng.below(40)).unwrap_or(0));
            }
        }
        "transfer" => {
            out.extend(account(rng));
            out.extend(account(rng));
            if ks721 {
                out.extend(rng.bytes(32));
            } else {
                out.extend(rng.u128_edge().to_le_bytes());
            }
        }
        "approve" => {
            out.extend(account(rng));
            if ks721 {
                let spender = account(rng);
                out.extend(option(rng, spender));
                out.extend(rng.bytes(32));
            } else {
                out.extend(account(rng));
                out.extend(rng.u128_edge().to_le_bytes());
                let expiry = rng.u64_edge().to_le_bytes().to_vec();
                out.extend(option(rng, expiry));
            }
        }
        "operator" => {
            out.extend(account(rng));
            out.extend(account(rng));
            out.push(if rng.chance(90) {
                u8::from(rng.chance(50))
            } else {
                2
            });
            let expiry = rng.u64_edge().to_le_bytes().to_vec();
            out.extend(option(rng, expiry));
        }
        "uri" => {
            let id = rng.bytes(32);
            out.extend(option(rng, id));
        }
        "minter" => {
            let previous = account(rng);
            out.extend(option(rng, previous));
            let minter = account(rng);
            out.extend(option(rng, minter));
        }
        _ => out.extend(rng.some_bytes(70)),
    }
    if rng.chance(5) {
        out.push(0);
    }
    out
}

fn gen_abi_event(rng: &mut Rng, events: &Value, defs: &Value) -> Vec<u8> {
    let list = events.as_array().cloned().unwrap_or_default();
    let names: Vec<&str> = list
        .iter()
        .filter_map(|e| e["name"].as_str())
        .chain(["unknown"])
        .collect();
    let name = gen_name(rng, &names);
    let plain = String::from_utf8_lossy(&name)
        .trim_start_matches('\u{feff}')
        .to_string();
    let mut out = borsh_string(&name);
    if let Some(event) = list
        .iter()
        .find(|e| e["name"].as_str() == Some(plain.as_str()))
    {
        for f in event["fields"].as_array().into_iter().flatten() {
            out.extend(gen_abi_value(rng, &f["type"], defs, 0));
        }
    } else {
        out.extend(rng.some_bytes(40));
    }
    if rng.chance(5) {
        out.push(0);
    }
    out
}

// ---------------------------------------------------------------------------------------------
// Requests

/// One request of `kind`, built from the seeds or from random values, mutated now and then.
pub fn generate(rng: &mut Rng, kind: &str, seeds: &Seeds, splice: &[Vec<u8>]) -> Request {
    let network = *rng.pick(&Network::ALL);
    let params = network.params();
    let from_seed = rng.chance(35);
    let mut request = match kind {
        "vmop" => {
            if from_seed && !seeds.vmops.is_empty() {
                Request::VmOp {
                    bytes: rng.pick(&seeds.vmops).clone(),
                    sign: None,
                }
            } else {
                let (op, secret) = gen_op(rng, &DEVNET, None);
                let sign = match secret {
                    Some(secret) if rng.chance(60) => Some(Signing {
                        secret,
                        aux: rng.array(),
                    }),
                    // A key of another account: both sides refuse to sign
                    _ if rng.chance(20) => Some(Signing {
                        secret: *rng.pick(&SECRETS),
                        aux: rng.array(),
                    }),
                    _ => None,
                };
                Request::VmOp {
                    bytes: encode(&op).unwrap_or_default(),
                    sign,
                }
            }
        }
        "payload" => Request::Payload {
            network,
            bytes: if from_seed && !seeds.payloads.is_empty() {
                rng.pick(&seeds.payloads).clone()
            } else {
                gen_payload(rng, params).encode(params).unwrap_or_default()
            },
        },
        "borsh" => {
            if from_seed && !seeds.borsh.is_empty() {
                let (ty, bytes) = rng.pick(&seeds.borsh).clone();
                Request::Borsh { ty, bytes }
            } else {
                let ty = *rng.pick(&BorshType::ALL);
                Request::Borsh {
                    ty,
                    bytes: gen_borsh(rng, ty),
                }
            }
        }
        "tx" => Request::Tx {
            network,
            bytes: if from_seed && !seeds.txs.is_empty() {
                rng.pick(&seeds.txs).clone()
            } else {
                gen_tx(rng, params)
            },
        },
        "script" => Request::Script {
            script: if from_seed && !seeds.scripts.is_empty() {
                rng.pick(&seeds.scripts).clone()
            } else {
                gen_script(rng, &DEVNET)
            },
        },
        "witness" => Request::Witness {
            items: gen_witness(rng, &DEVNET).to_vec(),
        },
        "write" => Request::Write {
            payload: {
                let len = *rng.pick(&[
                    0, 1, 75, 76, 255, 256, 519, 520, 521, 1_040, 1_041, 5_000, 70_000,
                ]);
                rng.bytes(len)
            },
        },
        "address" => Request::Address {
            network: *rng.pick(&BtcNet::ALL),
            text: gen_address_text(rng),
        },
        "account" => Request::Account {
            network: *rng.pick(&BtcNet::ALL),
            account: if rng.chance(50) {
                ksana_crypto::keypair_from_secret(rng.pick(&SECRETS))
                    .map(|k| ksana_crypto::account_of(&k))
                    .unwrap_or([0; 32])
            } else {
                rng.array()
            },
        },
        "bip322" => gen_bip322(rng, seeds),
        "bits" => Request::Bits {
            bits: if rng.chance(50) {
                let exponent = u32::try_from(rng.below(36)).unwrap_or(0);
                let mantissa =
                    *rng.pick(&[0, 1, 0x7f_ffff, 0x80_0000, 0x00_8000, 0xff_ffff, 0x12_3456]);
                (exponent << 24) | mantissa
            } else {
                u32::try_from(rng.next_u64() & 0xffff_ffff).unwrap_or(0)
            },
            pow_limit_bits: *rng.pick(&[
                DEVNET.pow_limit_bits,
                0x1f00_ffff,
                0x1d00_ffff,
                0x2100_ffff,
                0x0300_0001,
                0,
            ]),
            weight: *rng.pick(&[0, 1, 1, 2, 3, 12, 120, 1 << 32, u64::MAX]),
        },
        "pow" => Request::Pow {
            network,
            header: if from_seed && !seeds.headers.is_empty() {
                rng.pick(&seeds.headers).clone()
            } else {
                encode(&gen_header(rng, None)).unwrap_or_default()
            },
            weight: *rng.pick(&[1, 1, 1, 12, 24, 120, 0, 1 << 32, u64::MAX]),
        },
        "rewards" => gen_rewards(rng, network),
        "event" => Request::Event {
            standards: if rng.chance(85) {
                rng.pick(&DECLARATIONS)
                    .iter()
                    .map(|s| s.to_string())
                    .collect()
            } else {
                (0..rng.below(4)).map(|_| gen_string(rng)).collect()
            },
            data: if from_seed && !seeds.events.is_empty() {
                rng.pick(&seeds.events).clone()
            } else {
                gen_event(rng)
            },
        },
        "abi" => {
            let ty = rng.below(ABI_TYPES.len());
            let bytes = ABI_TYPES
                .get(ty)
                .map(|t| gen_abi_value(rng, &t.abi_json(), &abi_defs(), 0))
                .unwrap_or_default();
            Request::Abi { ty, bytes }
        }
        "abi_event" => {
            let events: Value = serde_json::from_str(ABI_EVENTS).unwrap_or(Value::Null);
            Request::AbiEvent {
                bytes: gen_abi_event(rng, &events, &abi_defs()),
            }
        }
        "merkle" => Request::Merkle {
            leaves: (0..rng.below(10)).map(|_| rng.some_bytes(80)).collect(),
        },
        _ => Request::Params { network },
    };
    if rng.chance(25) {
        if let Some(bytes) = bytes_of(&mut request) {
            mutate(rng, bytes, splice);
        }
    }
    request
}

fn gen_bip322(rng: &mut Rng, seeds: &Seeds) -> Request {
    if rng.chance(30) && !seeds.bip322.is_empty() {
        let (challenge, message, signature) = rng.pick(&seeds.bip322).clone();
        let account: [u8; 32] = challenge
            .get(2..34)
            .and_then(|a| a.try_into().ok())
            .unwrap_or([0; 32]);
        return Request::Bip322 {
            challenge,
            account,
            message,
            signature,
        };
    }
    let secret = *rng.pick(&SECRETS);
    let keypair = ksana_crypto::keypair_from_secret(&secret).ok();
    let account = match (&keypair, rng.below(6)) {
        (Some(k), 0..=3) => ksana_crypto::account_of(k),
        (_, 4) => [0xff; 32],
        _ => rng.array(),
    };
    let message = if rng.chance(50) {
        hex::encode(rng.bytes(32)).into_bytes()
    } else {
        rng.some_bytes(60)
    };
    let mut signature = match &keypair {
        Some(k) if rng.chance(70) => {
            ksana_crypto::bip322::sign_simple_p2tr(k, &message, &rng.array()).unwrap_or_default()
        }
        _ => serialize(&Witness::from_slice(
            &(0..rng.below(3))
                .map(|_| rng.some_bytes(70))
                .collect::<Vec<_>>(),
        )),
    };
    match rng.below(8) {
        // A 65-byte signature: SIGHASH_ALL, or another hash type
        0 if signature.len() == 66 => {
            signature[1] = 65;
            signature.push(*rng.pick(&[0x01, 0x00, 0x02, 0x81]));
        }
        // Two witness items
        1 => signature = serialize(&Witness::from_slice(&[rng.bytes(64), rng.bytes(33)])),
        // Over the length limit
        2 => signature = serialize(&Witness::from_slice(&[rng.bytes(1_100)])),
        _ => {}
    }
    let challenge = match rng.below(5) {
        0 => [vec![0x00, 0x14], rng.bytes(20)].concat(),
        1 => rng.some_bytes(40),
        _ => [vec![0x51, 0x20], account.to_vec()].concat(),
    };
    Request::Bip322 {
        challenge,
        account,
        message,
        signature,
    }
}

fn gen_rewards(rng: &mut Rng, network: Network) -> Request {
    let p = network.params();
    let interval = p.reward_halving_interval;
    // The DAA scores where a halving period or the block share ends, and around them
    let mut boundaries = vec![0u64, p.block_share_end];
    let mut start = 0u64;
    let mut len = interval;
    for k in 0..140u32 {
        start = start.saturating_add(len);
        boundaries.push(start);
        if k < p.reward_interval_doublings {
            len = len.saturating_mul(2);
        }
    }
    let daa_score = match rng.below(4) {
        0 => rng.u64_edge(),
        1 => rng.next_u64() % interval.saturating_mul(8).max(1),
        _ => rng
            .pick(&boundaries)
            .wrapping_add(rng.next_u64() % 5)
            .wrapping_sub(2),
    };
    Request::Rewards {
        network,
        daa_score,
        weight: *rng.pick(&[0, 1, 1, 2, 12, 24, 120, 4_096]),
        amount: match rng.below(3) {
            0 => *rng.pick(&[0, 1, 9_999, 10_000, 10_001]),
            _ => rng.u128_edge(),
        },
    }
}

/// Every kind's name, for the sweep's options.
pub fn kinds() -> &'static [&'static str] {
    &KINDS
}
