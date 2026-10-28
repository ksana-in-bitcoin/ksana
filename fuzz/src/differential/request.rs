//! The inputs of the differential test: [`Request`], its JSON form for the oracle, and its reading from the raw bytes
//! a fuzzer gives.

use ksana_primitives::{Network, OutPoint};
use serde_json::{json, Value};

use super::abi::ABI_TYPES;
use super::rust::WRITE_KEY;

/// The first input a path C op is signed against and verified with in this test (the value of the fuzz seeds).
pub const FIRST_INPUT: OutPoint = OutPoint {
    txid: [0xc1; 32],
    vout: 1,
};

/// A consensus type whose Borsh encoding is the same on every network (the SDK's `BorshTypeName`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BorshType {
    OutPoint,
    FeeMode,
    Funds,
    Action,
    SigScheme,
    VmOp,
    BtcAnchor,
    BlockHeader,
    Entry,
    EntryData,
    Block,
}

impl BorshType {
    /// Every type.
    pub const ALL: [BorshType; 11] = [
        BorshType::OutPoint,
        BorshType::FeeMode,
        BorshType::Funds,
        BorshType::Action,
        BorshType::SigScheme,
        BorshType::VmOp,
        BorshType::BtcAnchor,
        BorshType::BlockHeader,
        BorshType::Entry,
        BorshType::EntryData,
        BorshType::Block,
    ];

    /// The type's name, as `type` in `tests/vectors/codec.json` and the SDK's `TypeName` have it.
    pub fn name(self) -> &'static str {
        match self {
            BorshType::OutPoint => "OutPoint",
            BorshType::FeeMode => "FeeMode",
            BorshType::Funds => "Funds",
            BorshType::Action => "Action",
            BorshType::SigScheme => "SigScheme",
            BorshType::VmOp => "VmOp",
            BorshType::BtcAnchor => "BtcAnchor",
            BorshType::BlockHeader => "BlockHeader",
            BorshType::Entry => "Entry",
            BorshType::EntryData => "EntryData",
            BorshType::Block => "Block",
        }
    }

    /// The type of a name.
    pub fn from_name(name: &str) -> Option<BorshType> {
        BorshType::ALL.into_iter().find(|t| t.name() == name)
    }
}

/// A BTC network as an address names it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BtcNet {
    Mainnet,
    Testnet,
    Signet,
    Regtest,
}

impl BtcNet {
    /// Every network.
    pub const ALL: [BtcNet; 4] = [
        BtcNet::Mainnet,
        BtcNet::Testnet,
        BtcNet::Signet,
        BtcNet::Regtest,
    ];

    /// The SDK's `BtcNetwork`.
    pub fn name(self) -> &'static str {
        match self {
            BtcNet::Mainnet => "mainnet",
            BtcNet::Testnet => "testnet",
            BtcNet::Signet => "signet",
            BtcNet::Regtest => "regtest",
        }
    }

    /// rust-bitcoin's network.
    pub fn bitcoin(self) -> bitcoin::Network {
        match self {
            BtcNet::Mainnet => bitcoin::Network::Bitcoin,
            BtcNet::Testnet => bitcoin::Network::Testnet,
            BtcNet::Signet => bitcoin::Network::Signet,
            BtcNet::Regtest => bitcoin::Network::Regtest,
        }
    }
}

/// The keys a [`Request::VmOp`] is signed with by both sides.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Signing {
    /// The untweaked secret key.
    pub secret: [u8; 32],
    /// BIP340's auxiliary randomness.
    pub aux: [u8; 32],
}

/// One input of the differential test, and what each side is asked about it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Request {
    /// A VmOp's bytes: decoded within devnet's `MAX_OP_BYTES`, its op ID, its signing hashes for path B and for
    /// path C (with [`FIRST_INPUT`]), its signature checked both ways, and, with `sign`, signed again both ways.
    VmOp {
        bytes: Vec<u8>,
        sign: Option<Signing>,
    },
    /// A payload's bytes on a network (compact headers and entries, D-075, D-079, D-080).
    Payload { network: Network, bytes: Vec<u8> },
    /// The Borsh bytes of a consensus type, within 1 MiB: the value, and the hashes of a header, a block and a VmOp.
    Borsh { ty: BorshType, bytes: Vec<u8> },
    /// A BTC transaction's bytes: the transaction, its KSN1 data, payload and native op on a network, its first input
    /// (D-032, D-081, D-086).
    Tx { network: Network, bytes: Vec<u8> },
    /// A script read as a KSN1 data output and as a KSN1 envelope leaf.
    Script { script: Vec<u8> },
    /// A witness stack: its leaf script (BIP341, the annex removed).
    Witness { items: Vec<Vec<u8>> },
    /// A payload written as a KSN1 data output and as an envelope leaf.
    Write { payload: Vec<u8> },
    /// Text read as an account: as a Taproot address, and as an account's text (hex or address).
    Address { network: BtcNet, text: String },
    /// An account written as a Taproot address.
    Account { network: BtcNet, account: [u8; 32] },
    /// A BIP322 simple signature: verified for a challenge script, and for an account.
    Bip322 {
        challenge: Vec<u8>,
        account: [u8; 32],
        message: Vec<u8>,
        signature: Vec<u8>,
    },
    /// Compact bits: the target under a PoW limit, and the effective target of a weight (D-025, D-084).
    Bits {
        bits: u32,
        pow_limit_bits: u32,
        weight: u64,
    },
    /// A block header's Borsh bytes: its Argon2id hash and its PoW check at a weight (D-094).
    Pow {
        network: Network,
        header: Vec<u8>,
        weight: u64,
    },
    /// The reward schedule at a DAA score: the halving period, the slot's reward and its split, the rules
    /// (D-074, D-089, D-095, D-105).
    Rewards {
        network: Network,
        daa_score: u64,
        weight: u64,
        amount: u128,
    },
    /// An event's bytes read as KS-20 and as KS-721, and the standard of a declaration (D-120).
    Event {
        standards: Vec<String>,
        data: Vec<u8>,
    },
    /// Borsh bytes decoded as one of [`ABI_TYPES`] by the contract SDK and by the playground's ABI codec.
    Abi { ty: usize, bytes: Vec<u8> },
    /// An event's bytes decoded by the playground's ABI codec with the events of [`super::ABI_EVENTS`].
    AbiEvent { bytes: Vec<u8> },
    /// The RFC 6962 root of leaves.
    Merkle { leaves: Vec<Vec<u8>> },
    /// A network's parameters, in the fields both sides have.
    Params { network: Network },
}

/// Every kind, in the order [`Request::from_fuzz`] selects them.
pub const KINDS: [&str; 18] = [
    "vmop",
    "payload",
    "borsh",
    "tx",
    "script",
    "witness",
    "write",
    "address",
    "account",
    "bip322",
    "bits",
    "pow",
    "rewards",
    "event",
    "abi",
    "abi_event",
    "merkle",
    "params",
];

/// The declarations a fuzzed [`Request::Event`] picks from.
pub const DECLARATIONS: [&[&str]; 7] = [
    &["KS-20"],
    &["KS-721"],
    &["KS-20", "KS-20-burnable", "KS-20-mintable"],
    &["KS-721", "KS-721-burnable", "KS-721-enumerable"],
    &["KS-20", "KS-721"],
    &["KS-721-burnable", "KS-721"],
    &[],
];

fn hex(bytes: &[u8]) -> String {
    hex::encode(bytes)
}

/// Splits `data` into byte strings: each a length byte and that many bytes (the last one may be short).
fn items(mut data: &[u8]) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    while let Some((&len, rest)) = data.split_first() {
        let (item, tail) = rest.split_at(usize::from(len).min(rest.len()));
        out.push(item.to_vec());
        data = tail;
    }
    out
}

fn array<const N: usize>(data: &[u8]) -> [u8; N] {
    let mut out = [0u8; N];
    for (dst, src) in out.iter_mut().zip(data) {
        *dst = *src;
    }
    out
}

fn network(byte: u8) -> Network {
    Network::ALL[usize::from(byte) % Network::ALL.len()]
}

impl Request {
    /// The request's kind (one of [`KINDS`]).
    pub fn kind(&self) -> &'static str {
        match self {
            Request::VmOp { .. } => "vmop",
            Request::Payload { .. } => "payload",
            Request::Borsh { .. } => "borsh",
            Request::Tx { .. } => "tx",
            Request::Script { .. } => "script",
            Request::Witness { .. } => "witness",
            Request::Write { .. } => "write",
            Request::Address { .. } => "address",
            Request::Account { .. } => "account",
            Request::Bip322 { .. } => "bip322",
            Request::Bits { .. } => "bits",
            Request::Pow { .. } => "pow",
            Request::Rewards { .. } => "rewards",
            Request::Event { .. } => "event",
            Request::Abi { .. } => "abi",
            Request::AbiEvent { .. } => "abi_event",
            Request::Merkle { .. } => "merkle",
            Request::Params { .. } => "params",
        }
    }

    /// The request as the oracle reads it: byte strings in hex, 64- and 128-bit integers as decimal strings.
    pub fn to_json(&self) -> Value {
        let body = match self {
            Request::VmOp { bytes, sign } => json!({
                "bytes": hex(bytes),
                "sign": sign.as_ref().map(|s| json!({ "secret": hex(&s.secret), "aux": hex(&s.aux) })),
            }),
            Request::Payload { network, bytes } | Request::Tx { network, bytes } => {
                json!({ "network": network.name(), "bytes": hex(bytes) })
            }
            Request::Borsh { ty, bytes } => json!({ "type": ty.name(), "bytes": hex(bytes) }),
            Request::Script { script } => json!({ "script": hex(script) }),
            Request::Witness { items } | Request::Merkle { leaves: items } => {
                json!({ "items": items.iter().map(|i| hex(i)).collect::<Vec<_>>() })
            }
            Request::Write { payload } => {
                json!({ "payload": hex(payload), "key": hex(&WRITE_KEY) })
            }
            Request::Address { network, text } => {
                json!({ "network": network.name(), "text": text })
            }
            Request::Account { network, account } => {
                json!({ "network": network.name(), "account": hex(account) })
            }
            Request::Bip322 {
                challenge,
                account,
                message,
                signature,
            } => json!({
                "challenge": hex(challenge),
                "account": hex(account),
                "message": hex(message),
                "signature": hex(signature),
            }),
            Request::Bits {
                bits,
                pow_limit_bits,
                weight,
            } => json!({
                "bits": bits,
                "pow_limit_bits": pow_limit_bits,
                "weight": weight.to_string(),
            }),
            Request::Pow {
                network,
                header,
                weight,
            } => json!({
                "network": network.name(),
                "header": hex(header),
                "weight": weight.to_string(),
            }),
            Request::Rewards {
                network,
                daa_score,
                weight,
                amount,
            } => json!({
                "network": network.name(),
                "daa_score": daa_score.to_string(),
                "weight": weight.to_string(),
                "amount": amount.to_string(),
            }),
            Request::Event { standards, data } => {
                json!({ "standards": standards, "data": hex(data) })
            }
            Request::Abi { ty, bytes } => {
                let ty = ABI_TYPES.get(*ty).map(|t| t.abi_json());
                json!({ "type": ty, "bytes": hex(bytes) })
            }
            Request::AbiEvent { bytes } => json!({ "bytes": hex(bytes) }),
            Request::Params { network } => json!({ "network": network.name() }),
        };
        let mut body = body;
        if let Value::Object(map) = &mut body {
            map.insert("kind".into(), Value::from(self.kind()));
        }
        body
    }

    /// Reads a request from a fuzzer's raw bytes: the first byte picks the kind, the rest is the kind's input (see
    /// each arm). `None` for empty input.
    pub fn from_fuzz(data: &[u8]) -> Option<Request> {
        let (&selector, rest) = data.split_first()?;
        let head = rest.first().copied().unwrap_or(0);
        let tail = rest.get(1..).unwrap_or(&[]);
        Some(match KINDS[usize::from(selector) % KINDS.len()] {
            "vmop" => Request::VmOp {
                bytes: rest.to_vec(),
                sign: None,
            },
            "payload" => Request::Payload {
                network: network(head),
                bytes: tail.to_vec(),
            },
            "borsh" => Request::Borsh {
                ty: BorshType::ALL[usize::from(head) % BorshType::ALL.len()],
                bytes: tail.to_vec(),
            },
            "tx" => Request::Tx {
                network: network(head),
                bytes: tail.to_vec(),
            },
            "script" => Request::Script {
                script: rest.to_vec(),
            },
            "witness" => Request::Witness { items: items(rest) },
            "write" => Request::Write {
                payload: rest.to_vec(),
            },
            "address" => Request::Address {
                network: BtcNet::ALL[usize::from(head) % BtcNet::ALL.len()],
                text: String::from_utf8_lossy(tail).into_owned(),
            },
            "account" => Request::Account {
                network: BtcNet::ALL[usize::from(head) % BtcNet::ALL.len()],
                account: array(tail),
            },
            "bip322" => {
                // [challenge kind] [account: 32] [message length] [message] [signature]
                let account: [u8; 32] = array(tail);
                let after = tail.get(32..).unwrap_or(&[]);
                let (len, after) = after.split_first().map_or((0, after), |(l, a)| (*l, a));
                let (message, signature) = after.split_at(usize::from(len).min(after.len()));
                let challenge = match head % 3 {
                    0 => ksana_crypto::account_script(&account)
                        .map(|s| s.to_bytes())
                        .unwrap_or_default(),
                    1 => [&[0x00, 0x14][..], &account[..20]].concat(),
                    _ => [&[0x51, 0x20][..], &account[..]].concat(),
                };
                Request::Bip322 {
                    challenge,
                    account,
                    message: message.to_vec(),
                    signature: signature.to_vec(),
                }
            }
            "bits" => {
                let bits = u32::from_le_bytes(array(rest));
                let limit = rest.get(4).copied().unwrap_or(0);
                let pow_limit_bits = match limit % 4 {
                    0 => ksana_primitives::DEVNET.pow_limit_bits,
                    1 => ksana_primitives::MAINNET.pow_limit_bits,
                    2 => 0x1d00_ffff,
                    _ => u32::from_le_bytes(array(rest.get(5..).unwrap_or(&[]))),
                };
                let weight = u64::from_le_bytes(array(rest.get(9..).unwrap_or(&[])));
                Request::Bits {
                    bits,
                    pow_limit_bits,
                    weight: if limit & 0x80 == 0 {
                        weight % 256
                    } else {
                        weight
                    },
                }
            }
            "pow" => Request::Pow {
                network: network(head),
                weight: u64::from(tail.first().copied().unwrap_or(1)),
                header: tail.get(1..).unwrap_or(&[]).to_vec(),
            },
            "rewards" => Request::Rewards {
                network: network(head),
                daa_score: u64::from_le_bytes(array(tail)),
                weight: u64::from(u16::from_le_bytes(array(tail.get(8..).unwrap_or(&[])))) % 4_097,
                amount: u128::from_le_bytes(array(tail.get(10..).unwrap_or(&[]))),
            },
            "event" => Request::Event {
                standards: DECLARATIONS[usize::from(head) % DECLARATIONS.len()]
                    .iter()
                    .map(|s| s.to_string())
                    .collect(),
                data: tail.to_vec(),
            },
            "abi" => Request::Abi {
                ty: usize::from(head) % ABI_TYPES.len(),
                bytes: tail.to_vec(),
            },
            "abi_event" => Request::AbiEvent {
                bytes: rest.to_vec(),
            },
            "merkle" => Request::Merkle {
                leaves: items(rest),
            },
            _ => Request::Params {
                network: network(head),
            },
        })
    }

    /// The raw bytes [`Request::from_fuzz`] reads as this request, as near as its form allows (for the fuzz corpus):
    /// a VmOp's keys to sign with are left out, a BIP322 challenge that is not one of the three `from_fuzz` builds is
    /// the account's script, a message is cut to 255 bytes, a PoW weight to 255, an event's declaration not among
    /// [`DECLARATIONS`] is the first one.
    pub fn to_fuzz(&self) -> Vec<u8> {
        let selector = KINDS.iter().position(|k| *k == self.kind()).unwrap_or(0);
        let mut out = vec![u8::try_from(selector).unwrap_or(0)];
        let network_byte = |network: &Network| {
            let i = Network::ALL.iter().position(|n| n == network).unwrap_or(0);
            u8::try_from(i).unwrap_or(0)
        };
        let btc_byte = |network: &BtcNet| {
            let i = BtcNet::ALL.iter().position(|n| n == network).unwrap_or(0);
            u8::try_from(i).unwrap_or(0)
        };
        let joined = |items: &[Vec<u8>]| -> Vec<u8> {
            let mut out = Vec::new();
            for item in items {
                let item = &item[..item.len().min(255)];
                out.push(u8::try_from(item.len()).unwrap_or(0));
                out.extend_from_slice(item);
            }
            out
        };
        match self {
            Request::VmOp { bytes, .. } => out.extend_from_slice(bytes),
            Request::Payload { network, bytes } | Request::Tx { network, bytes } => {
                out.push(network_byte(network));
                out.extend_from_slice(bytes);
            }
            Request::Borsh { ty, bytes } => {
                let i = BorshType::ALL.iter().position(|t| t == ty).unwrap_or(0);
                out.push(u8::try_from(i).unwrap_or(0));
                out.extend_from_slice(bytes);
            }
            Request::Script { script } => out.extend_from_slice(script),
            Request::Witness { items } | Request::Merkle { leaves: items } => {
                out.extend(joined(items));
            }
            Request::Write { payload } => out.extend_from_slice(payload),
            Request::Address { network, text } => {
                out.push(btc_byte(network));
                out.extend_from_slice(text.as_bytes());
            }
            Request::Account { network, account } => {
                out.push(btc_byte(network));
                out.extend_from_slice(account);
            }
            Request::Bip322 {
                challenge,
                account,
                message,
                signature,
            } => {
                let kind = if challenge.len() == 22 && challenge.starts_with(&[0x00, 0x14]) {
                    1
                } else {
                    0
                };
                let message = &message[..message.len().min(255)];
                out.push(kind);
                out.extend_from_slice(account);
                out.push(u8::try_from(message.len()).unwrap_or(0));
                out.extend_from_slice(message);
                out.extend_from_slice(signature);
            }
            Request::Bits {
                bits,
                pow_limit_bits,
                weight,
            } => {
                out.extend_from_slice(&bits.to_le_bytes());
                out.push(0x83);
                out.extend_from_slice(&pow_limit_bits.to_le_bytes());
                out.extend_from_slice(&weight.to_le_bytes());
            }
            Request::Pow {
                network,
                header,
                weight,
            } => {
                out.push(network_byte(network));
                out.push(u8::try_from(*weight).unwrap_or(u8::MAX));
                out.extend_from_slice(header);
            }
            Request::Rewards {
                network,
                daa_score,
                weight,
                amount,
            } => {
                out.push(network_byte(network));
                out.extend_from_slice(&daa_score.to_le_bytes());
                out.extend_from_slice(&u16::try_from(*weight).unwrap_or(0).to_le_bytes());
                out.extend_from_slice(&amount.to_le_bytes());
            }
            Request::Event { standards, data } => {
                let i = DECLARATIONS
                    .iter()
                    .position(|d| d.iter().copied().eq(standards.iter().map(String::as_str)))
                    .unwrap_or(0);
                out.push(u8::try_from(i).unwrap_or(0));
                out.extend_from_slice(data);
            }
            Request::Abi { ty, bytes } => {
                out.push(u8::try_from(*ty).unwrap_or(0));
                out.extend_from_slice(bytes);
            }
            Request::AbiEvent { bytes } => out.extend_from_slice(bytes),
            Request::Params { network } => out.push(network_byte(network)),
        }
        out
    }
}
