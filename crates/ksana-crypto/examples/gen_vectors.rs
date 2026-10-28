//! Generate the shared test vectors under `tests/vectors/` (shared by Rust and the TS SDK, SPEC §11.3, §14 M0).
//!
//! Usage: `cargo run -p ksana-crypto --example gen_vectors [output directory]`
//! Defaults to `tests/vectors/` at the repository root. Once committed, the vectors are the fixed reference:
//! regenerate them only when the encoding is changed deliberately (a consensus change), and record it in DECISIONS.md.

use std::path::PathBuf;

use bitcoin::hashes::Hash;
use ksana_crypto::{
    account_of, bip322, entries_root, keypair_from_secret, merkle_root, native_vmop_sighash,
    sign_native_vmop, sign_vmop, sign_vmop_bip322_p2tr, tagged_hash, verify_vmop, vmop_sighash,
    VMOP_SIGHASH_TAG,
};
use ksana_primitives::codec::encode;
use ksana_primitives::{
    compact, Action, Block, BlockHeader, Body, BtcAnchor, Entry, EntryData, FeeMode, Funds,
    OutPoint, Payload, SettlementTxs, SigScheme, VmOp, BLOCK_VERSION, DEVNET, KSA_ASSET_ID,
    VMOP_VERSION,
};
use serde::Serialize;
use serde_json::{json, Value};

fn hexs(bytes: &[u8]) -> String {
    hex::encode(bytes)
}

fn h32(byte: u8) -> [u8; 32] {
    [byte; 32]
}

/// Fixed "random-looking" 32 bytes (sha256 of label), to avoid all-repeated bytes.
fn fixed32(label: &str) -> [u8; 32] {
    ksana_primitives::hash::sha256(label.as_bytes())
}

struct Key {
    secret: [u8; 32],
    aux: [u8; 32],
}

fn keys() -> Vec<Key> {
    vec![
        Key {
            secret: fixed32("ksana/vectors/key/alice"),
            aux: [0u8; 32],
        },
        Key {
            secret: fixed32("ksana/vectors/key/bob"),
            aux: fixed32("ksana/vectors/aux/bob"),
        },
        Key {
            secret: fixed32("ksana/vectors/key/carol"),
            aux: fixed32("ksana/vectors/aux/carol"),
        },
        Key {
            secret: fixed32("ksana/vectors/key/dave"),
            aux: [0xFF; 32],
        },
    ]
}

/// Unsigned sample ops (sig is a placeholder) covering both paths (path A is deprecated, D-078) and the three actions.
fn unsigned_ops() -> Vec<(&'static str, VmOp)> {
    let base = |fee: FeeMode, action: Action, nonce: u64, max_gas: u64| VmOp {
        version: VMOP_VERSION,
        chain_id: DEVNET.chain_id,
        account: [0; 32],
        nonce,
        fee,
        max_gas,
        action,
        sig: SigScheme::Bip340([0; 64]),
    };
    vec![
        (
            "path B: sponsored, contract call (method name with non-ASCII)",
            base(
                FeeMode::Token { amount: 2_500 },
                Action::Call {
                    contract: fixed32("ksana/vectors/contract/token"),
                    method: "überweisen".to_string(),
                    args: vec![0x01, 0x02, 0x03],
                    funds: vec![
                        Funds {
                            asset: KSA_ASSET_ID,
                            amount: 2_000,
                        },
                        Funds {
                            asset: fixed32("ksana/vectors/asset/issued"),
                            amount: u128::MAX,
                        },
                    ],
                },
                0,
                200_000,
            ),
        ),
        (
            "path B: sponsored, transfer of the project token",
            base(
                FeeMode::Token { amount: 1_000 },
                Action::AssetTransfer {
                    asset: KSA_ASSET_ID,
                    to: fixed32("ksana/vectors/account/recipient"),
                    amount: 123_456_789,
                },
                7,
                50_000,
            ),
        ),
        (
            "path C: native transaction, contract deployment",
            base(
                FeeMode::BtcNative,
                Action::Deploy {
                    code: b"\0asm\x01\0\0\0".to_vec(),
                    init_args: vec![],
                    funds: Vec::new(),
                },
                1,
                DEVNET.native_max_gas,
            ),
        ),
        (
            "extremes: u64 / u128 maximum values",
            base(
                FeeMode::Token { amount: u128::MAX },
                Action::AssetTransfer {
                    asset: h32(0xAB),
                    to: h32(0xCD),
                    amount: u128::MAX,
                },
                u64::MAX,
                u64::MAX,
            ),
        ),
    ]
}

/// The first input of the carrying transaction that the path C sample op is bound to (D-086).
fn first_input() -> OutPoint {
    OutPoint {
        txid: fixed32("ksana/vectors/native-tx/input"),
        vout: 1,
    }
}

fn signed_ops() -> Vec<(&'static str, VmOp, Key)> {
    unsigned_ops()
        .into_iter()
        .zip(keys())
        .map(|((name, mut op), key)| {
            let kp = keypair_from_secret(&key.secret).unwrap();
            op.account = account_of(&kp);
            op.sig = match op.fee {
                FeeMode::Token { .. } => sign_vmop(&op, &kp, &key.aux).unwrap(),
                FeeMode::BtcNative => sign_native_vmop(&op, &first_input(), &kp, &key.aux).unwrap(),
            };
            (name, op, key)
        })
        .collect()
}

fn header(height: u64, parents: Vec<[u8; 32]>, seed: &str) -> BlockHeader {
    BlockHeader {
        version: BLOCK_VERSION,
        height,
        parents,
        btc_anchor: BtcAnchor {
            height: 101 + u32::try_from(height).unwrap(),
            hash: fixed32(&format!("{seed}/btc")),
        },
        entries_root: fixed32(&format!("{seed}/entries")),
        state_root: fixed32(&format!("{seed}/state")),
        producer: fixed32("ksana/vectors/producer"),
        timestamp: 1_790_000_000 + height * 5,
        bits: 0x207f_ffff,
        nonce: 42 + height,
    }
}

fn entries(ops: &[(&'static str, VmOp, Key)]) -> Vec<(&'static str, Entry)> {
    vec![
        (
            "Forced (path C)",
            Entry::Forced {
                btc_txid: fixed32("ksana/vectors/native-tx"),
                op: ops[2].1.clone(),
            },
        ),
        (
            "Sponsored (path B)",
            Entry::Sponsored {
                op: ops[1].1.clone(),
            },
        ),
        (
            "Sponsored (path B, contract call)",
            Entry::Sponsored {
                op: ops[0].1.clone(),
            },
        ),
        (
            "Unlock",
            Entry::Unlock {
                height: 1_234_567,
                index: 3,
            },
        ),
    ]
}

/// A payload case: payloads have their own encoding (compressed headers and entries, D-075, D-079, D-080), with
/// devnet's chain ID.
fn payload_case(name: &str, value: &Payload) -> Value {
    json!({
        "type": "Payload",
        "name": name,
        "value": serde_json::to_value(value).unwrap(),
        "borsh": hexs(&value.encode(&DEVNET).unwrap()),
    })
}

fn case<T: Serialize + borsh::BorshSerialize>(ty: &str, name: &str, value: &T) -> Value {
    json!({
        "type": ty,
        "name": name,
        "value": serde_json::to_value(value).unwrap(),
        "borsh": hexs(&encode(value).unwrap()),
    })
}

fn codec_vectors() -> Value {
    let ops = signed_ops();
    let entries = entries(&ops);
    let genesis = header(0, vec![], "ksana/vectors/h0");
    let h1 = header(1, vec![genesis.hash()], "ksana/vectors/h1");
    // The payee account of a republish payload (D-095): the 32 bytes following the bodies
    let payee = fixed32("ksana/vectors/carrier/payee");
    let side = header(1, vec![genesis.hash()], "ksana/vectors/h1-side");
    let h2 = header(2, vec![h1.hash(), side.hash()], "ksana/vectors/h2");
    let mut valid = Vec::new();

    valid.push(case(
        "OutPoint",
        "all zeros",
        &OutPoint {
            txid: [0; 32],
            vout: 0,
        },
    ));
    valid.push(case(
        "OutPoint",
        "vout maximum",
        &OutPoint {
            txid: fixed32("ksana/vectors/outpoint"),
            vout: u32::MAX,
        },
    ));
    for (name, fee) in [
        ("Token", FeeMode::Token { amount: 5 }),
        ("BtcNative", FeeMode::BtcNative),
    ] {
        valid.push(case("FeeMode", name, &fee));
    }
    for (name, action) in [
        (
            "Deploy empty code",
            Action::Deploy {
                code: vec![],
                init_args: vec![],
                funds: Vec::new(),
            },
        ),
        (
            "Call empty method name",
            Action::Call {
                contract: h32(0x55),
                method: String::new(),
                args: vec![],
                funds: Vec::new(),
            },
        ),
        (
            "Call non-ASCII method name",
            Action::Call {
                contract: h32(0x55),
                method: "mint_ñ_kṣaṇa".to_string(),
                args: vec![0xAA, 0xBB],
                funds: Vec::new(),
            },
        ),
        (
            "Call with attached funds",
            Action::Call {
                contract: h32(0x55),
                method: "swap".to_string(),
                args: vec![0x01],
                funds: vec![Funds {
                    asset: KSA_ASSET_ID,
                    amount: 7,
                }],
            },
        ),
        (
            "Deploy with attached funds",
            Action::Deploy {
                code: vec![0x00],
                init_args: vec![],
                funds: vec![Funds {
                    asset: h32(0x66),
                    amount: 1,
                }],
            },
        ),
        (
            "AssetTransfer",
            Action::AssetTransfer {
                asset: KSA_ASSET_ID,
                to: h32(0x22),
                amount: 1_000,
            },
        ),
    ] {
        valid.push(case("Action", name, &action));
    }
    valid.push(case("SigScheme", "Bip340", &SigScheme::Bip340([0x33; 64])));
    valid.push(case(
        "SigScheme",
        "Bip322Simple",
        &SigScheme::Bip322Simple(vec![0x01, 0x02, 0x03]),
    ));
    valid.push(case(
        "SigScheme",
        "Bip322Simple empty",
        &SigScheme::Bip322Simple(vec![]),
    ));

    for (name, op, _) in &ops {
        let mut c = case("VmOp", name, op);
        c["op_id"] = json!(hexs(&op.op_id().unwrap()));
        valid.push(c);
    }

    valid.push(case(
        "BtcAnchor",
        "regtest height 101",
        &BtcAnchor {
            height: 101,
            hash: fixed32("ksana/vectors/anchor"),
        },
    ));
    let max_header = BlockHeader {
        version: u32::MAX,
        height: u64::MAX,
        parents: vec![h32(0xFF); 3],
        btc_anchor: BtcAnchor {
            height: u32::MAX,
            hash: h32(0xFF),
        },
        entries_root: h32(0xFF),
        state_root: h32(0xFF),
        producer: h32(0xFF),
        timestamp: u64::MAX,
        bits: u32::MAX,
        nonce: u64::MAX,
    };
    for (name, h) in [
        ("genesis form (height 0, no parents)", &genesis),
        ("height 1 (one parent)", &h1),
        ("height 2 (two parents)", &h2),
        ("all maximum values (three parents)", &max_header),
    ] {
        let mut c = case("BlockHeader", name, h);
        c["block_hash"] = json!(hexs(&h.hash()));
        valid.push(c);
    }

    for (name, e) in &entries {
        valid.push(case("Entry", name, e));
    }
    for (name, e) in &entries {
        valid.push(case("EntryData", name, &EntryData::from(e)));
    }

    valid.push(case(
        "Block",
        "empty block, no settlement transactions",
        &Block {
            header: genesis.clone(),
            entries: vec![],
            settlement: None,
        },
    ));
    valid.push(case(
        "Block",
        "all entry types, with settlement transactions (commit and reveal, D-081)",
        &Block {
            header: h2.clone(),
            entries: entries.iter().map(|(_, e)| e.clone()).collect(),
            settlement: Some(SettlementTxs {
                commit: vec![0x02, 0x00, 0x00, 0x00, 0x00, 0x01],
                reveal: vec![0x02, 0x00, 0x00, 0x00, 0x00, 0x02],
            }),
        },
    ));

    valid.push(payload_case(
        "Settlement",
        &Payload::Settlement {
            headers: vec![h1.clone(), h2.clone()],
            entries: entries.iter().map(|(_, e)| EntryData::from(e)).collect(),
            bodies: vec![],
        },
    ));
    // Carrying bodies of other blocks (D-077): at least one entry other than forced inclusions
    let body_entries: Vec<EntryData> = entries.iter().map(|(_, e)| EntryData::from(e)).collect();
    valid.push(payload_case(
        "Settlement carrying bodies of other blocks",
        &Payload::Settlement {
            headers: vec![genesis.clone(), h1.clone(), h2.clone()],
            entries: entries
                .iter()
                .take(1)
                .map(|(_, e)| EntryData::from(e))
                .collect(),
            bodies: vec![
                Body {
                    header: 0,
                    entries: body_entries.clone(),
                },
                Body {
                    header: 1,
                    entries: body_entries[1..2].to_vec(),
                },
            ],
        },
    ));
    valid.push(payload_case(
        "Headers carrying bodies (republish)",
        &Payload::Headers {
            headers: vec![h1.clone(), h2.clone()],
            bodies: vec![Body {
                header: 1,
                entries: body_entries.clone(),
            }],
            payee,
        },
    ));
    valid.push(payload_case(
        "NativeOp",
        &Payload::NativeOp {
            op: ops[2].1.clone(),
        },
    ));
    valid.push(payload_case(
        "Headers",
        &Payload::Headers {
            headers: vec![genesis.clone(), h1.clone(), h2.clone()],
            bodies: vec![],
            payee,
        },
    ));
    valid.push(payload_case(
        "Headers empty list",
        &Payload::Headers {
            headers: vec![],
            bodies: vec![],
            payee: [0; 32],
        },
    ));

    // Compressed headers in a payload (D-075): references to parents and producers within the sequence (the most
    // recent one), parents outside the sequence, empty entries root, anchor / bits / version unchanged and changed,
    // height and timestamp deltas negative or wrapping modulo 2^64
    let empty = compact::empty_entries_root();
    let other = fixed32("ksana/vectors/compact/other-producer");
    let mut dag: Vec<BlockHeader> = Vec::new();
    let mut a = header(
        10,
        vec![fixed32("ksana/vectors/compact/outside")],
        "ksana/vectors/compact/a",
    );
    a.entries_root = empty;
    dag.push(a);
    let mut b = header(11, vec![dag[0].hash()], "ksana/vectors/compact/b");
    b.btc_anchor = dag[0].btc_anchor.clone();
    b.producer = other;
    dag.push(b);
    let mut c = header(11, vec![dag[0].hash()], "ksana/vectors/compact/c");
    c.btc_anchor = dag[0].btc_anchor.clone();
    c.entries_root = empty;
    dag.push(c);
    let mut d = header(
        12,
        vec![
            dag[2].hash(),
            dag[1].hash(),
            fixed32("ksana/vectors/compact/outside-2"),
        ],
        "ksana/vectors/compact/d",
    );
    d.version = BLOCK_VERSION + 1;
    d.bits = 0x1f00_ffff;
    d.producer = other;
    dag.push(d);
    let mut e = header(5, vec![dag[3].hash()], "ksana/vectors/compact/e");
    e.version = BLOCK_VERSION + 1;
    e.bits = 0x1f00_ffff;
    e.timestamp = u64::MAX;
    dag.push(e);
    let mut f = header(13, vec![dag[4].hash()], "ksana/vectors/compact/f");
    f.height = u64::MAX;
    f.timestamp = 3;
    dag.push(f);
    valid.push(payload_case(
        "Headers compressed: parallel blocks and merges, references, empty entries root, field changes, negative and wrapping deltas",
        &Payload::Headers {
            headers: dag.clone(),
            bodies: vec![],
            payee,
        },
    ));
    valid.push(payload_case(
        "Settlement compressed: carrying headers of parallel blocks",
        &Payload::Settlement {
            headers: dag[..4].to_vec(),
            entries: entries
                .iter()
                .take(2)
                .map(|(_, e)| EntryData::from(e))
                .collect(),
            bodies: vec![],
        },
    ));
    valid.push(payload_case(
        "Headers compressed: all maximum values (three identical parents outside the sequence)",
        &Payload::Headers {
            headers: vec![max_header.clone()],
            bodies: vec![],
            payee,
        },
    ));
    // Unchanged state root (D-079): an empty block's state root equals its parent's, and the following headers do not
    // write the state root
    let sa = header(
        1,
        vec![fixed32("ksana/vectors/compact/y")],
        "ksana/vectors/compact/sa",
    );
    let mut sb = header(2, vec![sa.hash()], "ksana/vectors/compact/sb");
    sb.btc_anchor = sa.btc_anchor.clone();
    sb.entries_root = empty;
    sb.state_root = sa.state_root;
    let mut sc = header(3, vec![sb.hash()], "ksana/vectors/compact/sc");
    sc.btc_anchor = sa.btc_anchor.clone();
    sc.entries_root = empty;
    sc.state_root = sa.state_root;
    valid.push(payload_case(
        "Headers compressed: empty blocks whose state root equals the previous one (D-079)",
        &Payload::Headers {
            headers: vec![sa.clone(), sb.clone(), sc.clone()],
            bodies: vec![],
            payee,
        },
    ));

    // Entry compression in a payload (D-080): key dictionary (written in full on first occurrence, referenced
    // afterwards, KSA preset as the 1st), varints, VmOp without chain ID; the bodies come before this block's entries
    // and share one dictionary
    let alice = fixed32("ksana/vectors/compress/alice");
    let bob = fixed32("ksana/vectors/compress/bob");
    let token = fixed32("ksana/vectors/compress/token");
    let op_with =
        |account: [u8; 32], nonce: u64, fee: FeeMode, action: Action, sig: SigScheme| VmOp {
            version: VMOP_VERSION,
            chain_id: DEVNET.chain_id,
            account,
            nonce,
            fee,
            max_gas: 100_000,
            action,
            sig,
        };
    let transfer_to = |account, nonce, to, amount| EntryData::Sponsored {
        op: op_with(
            account,
            nonce,
            FeeMode::Token { amount: 2 },
            Action::AssetTransfer {
                asset: KSA_ASSET_ID,
                to,
                amount,
            },
            SigScheme::Bip340([0x5a; 64]),
        ),
    };
    let compress_call = EntryData::Sponsored {
        op: op_with(
            bob,
            0,
            FeeMode::BtcNative,
            Action::Call {
                contract: token,
                method: "transfer".into(),
                args: vec![1, 2, 3],
                funds: vec![
                    Funds {
                        asset: KSA_ASSET_ID,
                        amount: u128::MAX,
                    },
                    Funds {
                        asset: token,
                        amount: 300,
                    },
                ],
            },
            SigScheme::Bip322Simple(vec![0x6b; 66]),
        ),
    };
    let compress_deploy = EntryData::Sponsored {
        op: op_with(
            alice,
            u64::MAX,
            FeeMode::Token { amount: u128::MAX },
            Action::Deploy {
                code: vec![0xc0; 200],
                init_args: vec![],
                funds: vec![],
            },
            SigScheme::Bip340([0x5b; 64]),
        ),
    };
    let ch1 = header(1, vec![genesis.hash()], "ksana/vectors/compress/h1");
    let ch2 = header(2, vec![ch1.hash()], "ksana/vectors/compress/h2");
    valid.push(payload_case(
        "entry compression: bodies and this block's entries share the key dictionary; all actions, signatures and integer limits",
        &Payload::Settlement {
            headers: vec![ch1, ch2],
            entries: vec![
                EntryData::Forced {
                    btc_txid: fixed32("ksana/vectors/compress/native"),
                },
                EntryData::Unlock {
                    height: u64::MAX,
                    index: u32::MAX,
                },
                transfer_to(alice, 1, bob, 1_000),
                transfer_to(bob, 7, alice, 128),
                compress_call,
                compress_deploy,
            ],
            bodies: vec![Body {
                header: 0,
                entries: vec![transfer_to(alice, 0, bob, 5)],
            }],
        },
    ));

    // Invalid encodings: decoding must fail.
    let op_bytes = encode(&ops[1].1).unwrap();
    let fee_tag = 1 + 4 + 32 + 8;
    let mut trailing = op_bytes.clone();
    trailing.push(0);
    let mut bad_fee = op_bytes.clone();
    bad_fee[fee_tag] = 2;
    let mut bad_sig = op_bytes.clone();
    let sig_tag = op_bytes.len() - 65;
    bad_sig[sig_tag] = 2;
    let call_op = &ops[0].1;
    let call_bytes = encode(call_op).unwrap();
    let method_len_at = 1 + 4 + 32 + 8 + (1 + 16) + 8 + 1 + 32;
    let mut bad_utf8 = call_bytes.clone();
    bad_utf8[method_len_at + 4] = 0xFF;
    let mut bad_action = call_bytes.clone();
    bad_action[method_len_at - 33] = 3;
    let block_none = encode(&Block {
        header: genesis.clone(),
        entries: vec![],
        settlement: None,
    })
    .unwrap();
    let mut bad_option = block_none.clone();
    let last = bad_option.len() - 1;
    bad_option[last] = 2;

    // Non-canonical compressed headers (D-075): starting from a canonical sequence, corrupt one item at a time; `error`
    // is the error message fragment shared by Rust and the TS SDK. A writes out all fields (190 bytes); B has the same
    // version, anchor and bits as A and references A as parent and producer (78 bytes); C in turn references B, with
    // the empty root as entries root (46 bytes).
    let ca = header(
        1,
        vec![fixed32("ksana/vectors/compact/x")],
        "ksana/vectors/compact/ca",
    );
    let mut cb = header(2, vec![ca.hash()], "ksana/vectors/compact/cb");
    cb.btc_anchor = ca.btc_anchor.clone();
    let mut cc = header(3, vec![cb.hash()], "ksana/vectors/compact/cc");
    cc.btc_anchor = ca.btc_anchor.clone();
    cc.entries_root = empty;
    let canonical = Payload::Headers {
        headers: vec![ca.clone(), cb.clone(), cc.clone()],
        bodies: vec![],
        payee,
    }
    .encode(&DEVNET)
    .unwrap();
    let (a_at, b_at, c_at) = (5, 5 + 190, 5 + 190 + 78);
    // Then 1 byte for the number of bodies (D-077, varint, D-080), and the last 32 bytes are the payee account (D-095)
    assert_eq!(canonical.len(), c_at + 46 + 1 + 32);
    let patched = |at: usize, remove: usize, insert: &[u8]| -> String {
        let mut out = canonical[..at].to_vec();
        out.extend_from_slice(insert);
        out.extend_from_slice(&canonical[at + remove..]);
        hexs(&out)
    };
    let with_flags = |at: usize, flags: u8, insert_at: usize, insert: &[u8]| -> String {
        let mut out = canonical.clone();
        out[at] = flags;
        out.splice(insert_at..insert_at, insert.iter().copied());
        hexs(&out)
    };
    let mut anchor_bytes = ca.btc_anchor.height.to_le_bytes().to_vec();
    anchor_bytes.extend_from_slice(&ca.btc_anchor.hash);
    let mut full_parent = vec![0];
    full_parent.extend_from_slice(&ca.hash());
    let mut full_producer = vec![0];
    full_producer.extend_from_slice(&ca.producer);
    let mut overflow = vec![0xff; 9];
    overflow.push(0x02);
    let compact_invalid = [
        (
            "first header has a \"same as previous\" flag set",
            patched(a_at, 1, &[0x01]),
            "the first header cannot refer to a previous one",
        ),
        (
            "unknown flag bit",
            patched(b_at, 1, &[0x27]),
            "unknown flag bit",
        ),
        (
            "version equal to the previous one but written out",
            with_flags(b_at, 0x06, b_at + 1, &ca.version.to_le_bytes()),
            "version equal to the previous one must use the flag",
        ),
        (
            "anchor equal to the previous one but written out",
            with_flags(b_at, 0x05, b_at + 4, &anchor_bytes),
            "anchor equal to the previous one must use the flag",
        ),
        (
            "bits equal to the previous one but written out",
            with_flags(b_at, 0x03, b_at + 70, &ca.bits.to_le_bytes()),
            "bits equal to the previous one must use the flag",
        ),
        (
            "empty entries root written out",
            with_flags(c_at, 0x07, c_at + 4, &empty),
            "the root of an empty entry list must use the flag",
        ),
        (
            "parent within the sequence but hash written out",
            patched(b_at + 3, 1, &full_parent),
            "a parent within the sequence must be referenced",
        ),
        (
            "producer already seen but written out",
            patched(b_at + 68, 1, &full_producer),
            "a producer already seen in the sequence must be referenced",
        ),
        (
            "parent reference out of range",
            patched(b_at + 3, 1, &[0x02]),
            "reference out of range",
        ),
        (
            "producer reference is not the most recent one",
            patched(c_at + 36, 1, &[0x02]),
            "reference is not the most recent equal element",
        ),
        (
            "varint is not minimally encoded",
            patched(b_at + 1, 1, &[0x82, 0x00]),
            "varint is not minimally encoded",
        ),
        (
            "varint exceeds u64",
            patched(a_at + 5, 1, &overflow),
            "varint exceeds u64",
        ),
        (
            "parent count exceeds 255",
            patched(a_at + 6, 1, &[0x80, 0x02]),
            "too many parents",
        ),
    ];

    let mut invalid = vec![
        json!({"type": "VmOp", "name": "1 trailing byte", "borsh": hexs(&trailing)}),
        json!({"type": "VmOp", "name": "truncated by 1 byte", "borsh": hexs(&op_bytes[..op_bytes.len() - 1])}),
        json!({"type": "VmOp", "name": "FeeMode variant index 2", "borsh": hexs(&bad_fee)}),
        json!({"type": "VmOp", "name": "Action variant index 3", "borsh": hexs(&bad_action)}),
        json!({"type": "VmOp", "name": "SigScheme variant index 2", "borsh": hexs(&bad_sig)}),
        json!({"type": "VmOp", "name": "method name is not valid UTF-8", "borsh": hexs(&bad_utf8)}),
        json!({"type": "VmOp", "name": "empty input", "borsh": ""}),
        json!({"type": "Block", "name": "Option tag is 2", "borsh": hexs(&bad_option)}),
        json!({"type": "Entry", "name": "Entry variant index 3", "borsh": "03"}),
        json!({"type": "EntryData", "name": "EntryData variant index 3", "borsh": "03"}),
        json!({"type": "Payload", "name": "Payload variant index 3", "borsh": "03"}),
        json!({"type": "Payload", "name": "Headers length prefix exceeds the actual data", "borsh": "02ffffffff"}),
        json!({"type": "Payload", "name": "Headers compressed: truncated by 1 byte", "borsh": hexs(&canonical[..canonical.len() - 1])}),
        json!({"type": "Payload", "name": "Headers compressed: 1 trailing byte", "borsh": patched(canonical.len(), 0, &[0])}),
        json!({"type": "Payload", "name": "Headers missing the payee account (D-095)", "borsh": hexs(&canonical[..canonical.len() - 32])}),
    ];
    // The state root flag (D-079): SA writes out all fields (190 bytes); SB has the same version, anchor, bits and
    // state root as SA, with the empty root as entries root (14 bytes)
    let same_root = Payload::Headers {
        headers: vec![sa.clone(), sb.clone()],
        bodies: vec![],
        payee,
    }
    .encode(&DEVNET)
    .unwrap();
    let sb_at = 5 + 190;
    assert_eq!(same_root.len(), sb_at + 14 + 1 + 32);
    // SB without the flag, writing out the 32-byte state root (after the flags, height delta, parent count and
    // reference)
    let mut root_written = same_root[..sb_at].to_vec();
    root_written.push(same_root[sb_at] & !0x10);
    root_written.extend_from_slice(&same_root[sb_at + 1..sb_at + 4]);
    root_written.extend_from_slice(&sa.state_root);
    root_written.extend_from_slice(&same_root[sb_at + 4..]);
    let mut first_same_root = same_root.clone();
    first_same_root[5] = 0x10;
    let compact_invalid = compact_invalid.into_iter().chain([
        (
            "first header has the \"state root equal to the previous one\" flag set",
            hexs(&first_same_root),
            "the first header cannot refer to a previous one",
        ),
        (
            "state root equal to the previous one but written out",
            hexs(&root_written),
            "state root equal to the previous one must use the flag",
        ),
    ]);
    // Non-canonical entry compression (D-080): one entry in a settlement payload with no headers and no bodies
    let one_entry = |entry: &[u8]| -> String {
        let mut out = vec![0, 0, 0, 0, 0, 0, 1];
        out.extend_from_slice(entry);
        hexs(&out)
    };
    // A path B transfer: account A written in full on first occurrence, the asset is KSA (reference 1), the recipient
    // written in full (the parts before and after the `to` field are given separately)
    let transfer_bytes =
        |nonce: &[u8], fee: &[u8], asset: &[u8], to: &[u8], amount: &[u8], sig: &[u8]| {
            let mut out = vec![1, VMOP_VERSION, 0];
            out.extend_from_slice(&alice);
            out.extend_from_slice(nonce);
            out.extend_from_slice(fee);
            out.push(1); // max_gas 1
            out.push(2); // AssetTransfer
            out.extend_from_slice(asset);
            out.extend_from_slice(to);
            out.extend_from_slice(amount);
            out.extend_from_slice(sig);
            out
        };
    let mut bob_full = vec![0];
    bob_full.extend_from_slice(&bob);
    let mut alice_full = vec![0];
    alice_full.extend_from_slice(&alice);
    let mut ksa_full = vec![0];
    ksa_full.extend_from_slice(&KSA_ASSET_ID);
    let mut bip340 = vec![0];
    bip340.extend_from_slice(&[0x5a; 64]);
    let token_fee: &[u8] = &[0, 2];
    let good = transfer_bytes(&[0], token_fee, &[1], &bob_full, &[5], &bip340);
    assert!(
        Payload::decode(&hex::decode(one_entry(&good)).unwrap(), &DEVNET).is_ok(),
        "the entry is valid before being corrupted"
    );
    let mut u128_over = vec![0xff; 18];
    u128_over.push(0x04);
    let mut call_bad_utf8 = vec![1, VMOP_VERSION, 0];
    call_bad_utf8.extend_from_slice(&alice);
    call_bad_utf8.extend_from_slice(&[0, 1, 1, 1, 1]); // nonce, BtcNative, max_gas, Call, contract references KSA
    call_bad_utf8.extend_from_slice(&[1, 0xff, 0, 0]); // method name 1 byte 0xff, empty args, no attached funds
    call_bad_utf8.extend_from_slice(&bip340);
    let entry_invalid = [
        (
            "a key already in the dictionary written in full",
            one_entry(&transfer_bytes(
                &[0],
                token_fee,
                &[1],
                &alice_full,
                &[5],
                &bip340,
            )),
            "a key already in the dictionary must be referenced",
        ),
        (
            "the preset KSA asset ID written in full",
            one_entry(&transfer_bytes(
                &[0],
                token_fee,
                &ksa_full,
                &bob_full,
                &[5],
                &bip340,
            )),
            "a key already in the dictionary must be referenced",
        ),
        (
            "key reference out of range",
            one_entry(&transfer_bytes(
                &[0],
                token_fee,
                &[3],
                &bob_full,
                &[5],
                &bip340,
            )),
            "key reference out of range",
        ),
        (
            "nonce is not a minimal varint",
            one_entry(&transfer_bytes(
                &[0x80, 0x00],
                token_fee,
                &[1],
                &bob_full,
                &[5],
                &bip340,
            )),
            "varint is not minimally encoded",
        ),
        (
            "amount exceeds u128",
            one_entry(&transfer_bytes(
                &[0],
                token_fee,
                &[1],
                &bob_full,
                &u128_over,
                &bip340,
            )),
            "varint exceeds u128",
        ),
        (
            "unknown fee mode",
            one_entry(&transfer_bytes(&[0], &[2], &[1], &bob_full, &[5], &bip340)),
            "unknown fee mode",
        ),
        (
            "unknown signature scheme",
            one_entry(&transfer_bytes(
                &[0],
                token_fee,
                &[1],
                &bob_full,
                &[5],
                &[2],
            )),
            "unknown signature scheme",
        ),
        (
            "unknown entry variant",
            one_entry(&[3]),
            "unknown entry variant",
        ),
        (
            "unknown action",
            one_entry(&{
                let mut bad = good.clone();
                bad[1 + 1 + 33 + 1 + 2 + 1] = 3;
                bad
            }),
            "unknown action",
        ),
        (
            "acceptance index exceeds u32",
            one_entry(&[2, 1, 0xff, 0xff, 0xff, 0xff, 0x10]),
            "varint exceeds u32",
        ),
        (
            "method name is not valid UTF-8",
            one_entry(&call_bad_utf8),
            "method name is not valid UTF-8",
        ),
    ];
    for (name, borsh, error) in entry_invalid {
        invalid.push(json!({
            "type": "Payload",
            "name": format!("entry compression: {name}"),
            "borsh": borsh,
            "error": error,
        }));
    }
    for (name, borsh, error) in compact_invalid {
        invalid.push(json!({
            "type": "Payload",
            "name": format!("Headers compressed: {name}"),
            "borsh": borsh,
            "error": error,
        }));
    }

    // Non-canonical bodies (D-077): the Borsh encoding itself is complete but violates the canonicality rules
    let sponsored = body_entries
        .iter()
        .find(|e| matches!(e, EntryData::Sponsored { .. }))
        .cloned()
        .unwrap();
    let forced_only: Vec<EntryData> = body_entries
        .iter()
        .filter(|e| matches!(e, EntryData::Forced { .. }))
        .cloned()
        .collect();
    let body = |header: u32, entries: Vec<EntryData>| Body { header, entries };
    let three = vec![ca.clone(), cb.clone(), cc.clone()];
    let bad_bodies = [
        (
            "indices not increasing",
            Payload::Headers {
                headers: three.clone(),
                bodies: vec![
                    body(1, vec![sponsored.clone()]),
                    body(1, vec![sponsored.clone()]),
                ],
                payee,
            },
            "body indices must be strictly increasing",
        ),
        (
            "index out of range",
            Payload::Headers {
                headers: three.clone(),
                bodies: vec![body(3, vec![sponsored.clone()])],
                payee,
            },
            "body index exceeds the number of headers",
        ),
        (
            "body in a settlement payload points at this block",
            Payload::Settlement {
                headers: three.clone(),
                entries: vec![sponsored.clone()],
                bodies: vec![body(2, vec![sponsored.clone()])],
            },
            "a body in a settlement payload cannot point at this block's own header",
        ),
        (
            "body contains only forced-inclusion entries",
            Payload::Headers {
                headers: three.clone(),
                bodies: vec![body(0, forced_only)],
                payee,
            },
            "a body must contain entries other than forced inclusions",
        ),
        (
            "body is empty",
            Payload::Headers {
                headers: three,
                bodies: vec![body(0, vec![])],
                payee,
            },
            "a body must contain entries other than forced inclusions",
        ),
    ];
    for (name, payload, error) in bad_bodies {
        invalid.push(json!({
            "type": "Payload",
            "name": format!("body: {name}"),
            "borsh": hexs(&payload.encode(&DEVNET).unwrap()),
            "error": error,
        }));
    }

    json!({
        "description": "Borsh encoding vectors of the Ksana consensus types. valid: value (JSON representation) and borsh (hex) must convert into each other; VmOp additionally has op_id = sha256(borsh), BlockHeader additionally has block_hash = sha256d(borsh); Payload has its own encoding (the field is still named borsh): the header sequence is compressed per D-075, D-079, the bodies (bodies, D-077) and entries are compressed per D-080 (VmOp without chain ID, restored with devnet's chain_id 1001). invalid: decoding must fail; when error is given, the error message must contain it.",
        "valid": valid,
        "invalid": invalid,
    })
}

fn signing_vectors() -> Value {
    let mut cases = Vec::new();
    for (name, op, key) in signed_ops() {
        let mut case = json!({
            "name": name,
            "secret_key": hexs(&key.secret),
            "aux_rand": hexs(&key.aux),
            "account": hexs(&op.account),
            "unsigned_borsh": hexs(&op.encode_unsigned().unwrap()),
            "op": serde_json::to_value(&op).unwrap(),
            "borsh": hexs(&op.encode().unwrap()),
            "op_id": hexs(&op.op_id().unwrap()),
        });
        // Path C (D-086): signing message = unsigned_borsh ‖ first_input (txid ‖ vout little-endian, 36 bytes)
        case["sighash"] = match op.fee {
            FeeMode::Token { .. } => json!(hexs(&vmop_sighash(&op).unwrap())),
            FeeMode::BtcNative => {
                let input = first_input();
                case["first_input"] = json!({ "txid": hexs(&input.txid), "vout": input.vout });
                json!(hexs(&native_vmop_sighash(&op, &input).unwrap()))
            }
        };
        cases.push(case);
    }

    // Tamper with the first op field by field: verification must fail.
    let (_, op, _) = signed_ops().remove(0);
    let mut tampered = Vec::new();
    let mut push = |name: &str, f: &dyn Fn(&mut VmOp)| {
        let mut t = op.clone();
        f(&mut t);
        assert_ne!(t, op);
        tampered.push(json!({"name": name, "op": serde_json::to_value(&t).unwrap()}));
    };
    push("version", &|t| t.version = 3);
    push("chain_id", &|t| t.chain_id = 1002);
    push("account", &|t| t.account[31] ^= 1);
    push("nonce", &|t| t.nonce = 1);
    push("fee variant", &|t| t.fee = FeeMode::BtcNative);
    push("fee.amount", &|t| {
        if let FeeMode::Token { amount } = &mut t.fee {
            *amount = 2_501;
        }
    });
    push("max_gas", &|t| t.max_gas = 200_001);
    push("action variant", &|t| {
        t.action = Action::Deploy {
            code: vec![],
            init_args: vec![],
            funds: Vec::new(),
        }
    });
    push("action.contract", &|t| {
        if let Action::Call { contract, .. } = &mut t.action {
            contract[0] ^= 1;
        }
    });
    push("action.method", &|t| {
        if let Action::Call { method, .. } = &mut t.action {
            *method = "überwiesen".to_string();
        }
    });
    push("action.args", &|t| {
        if let Action::Call { args, .. } = &mut t.action {
            args.push(0x04);
        }
    });
    push("action.funds", &|t| {
        if let Action::Call { funds, .. } = &mut t.action {
            if let Some(f) = funds.first_mut() {
                f.amount = f.amount.wrapping_add(1);
            }
        }
    });
    push("sig bytes", &|t| {
        if let SigScheme::Bip340(sig) = &mut t.sig {
            sig[63] ^= 1;
        }
    });
    push("sig scheme", &|t| {
        if let SigScheme::Bip340(sig) = &t.sig {
            t.sig = SigScheme::Bip322Simple(sig.to_vec());
        }
    });

    // BIP322 simple (D-052; since D-092 accounts only correspond to P2TR): one P2TR key-path case. The signature is
    // deterministic: BIP340 with a fixed aux_rand.
    let mut bip322_cases = Vec::new();
    {
        let address_type = "p2tr";
        let secret = fixed32(&format!("ksana/vectors/bip322/{address_type}"));
        let aux = fixed32("ksana/vectors/bip322/aux");
        let kp = keypair_from_secret(&secret).unwrap();
        let (_, mut op) = unsigned_ops().remove(1);
        op.account = account_of(&kp);
        let sighash = vmop_sighash(&op).unwrap();
        let message = bip322::vmop_message(&sighash);
        op.sig = sign_vmop_bip322_p2tr(&op, &kp, &aux).unwrap();
        verify_vmop(&op).unwrap();
        let SigScheme::Bip322Simple(witness) = &op.sig else {
            unreachable!()
        };
        // The address `OP_1 <account>`: the account is the output public key (D-092)
        let script = bip322::account_script(&op.account).unwrap();
        assert_eq!(&script.as_bytes()[2..], op.account);
        let spend = bip322::to_spend(&script, &message);
        let sign = bip322::to_sign(
            spend.compute_txid(),
            bitcoin::consensus::deserialize(witness).unwrap(),
        );
        bip322_cases.push(json!({
            "name": "BIP322 simple: P2TR key-path (address is the account's Taproot address, SIGHASH_DEFAULT)",
            "secret_key": hexs(&secret),
            "account": hexs(&op.account),
            "address_type": address_type,
            "script_pubkey": hexs(script.as_bytes()),
            "aux_rand": hexs(&aux),
            "op": serde_json::to_value(&op).unwrap(),
            "sighash": hexs(&sighash),
            "message": String::from_utf8(message.clone()).unwrap(),
            "message_hash": hexs(&bip322::message_hash(&message)),
            "to_spend_txid": hexs(&spend.compute_txid().to_byte_array()),
            "to_sign_txid": hexs(&sign.compute_txid().to_byte_array()),
            "witness": hexs(witness),
            "op_borsh": hexs(&op.encode().unwrap()),
            "op_id": hexs(&op.op_id().unwrap()),
        }));
    }

    let tagged = [
        (VMOP_SIGHASH_TAG, Vec::new()),
        (VMOP_SIGHASH_TAG, b"abc".to_vec()),
        ("BIP0340/challenge", vec![0u8; 32]),
    ]
    .iter()
    .map(|(tag, msg)| json!({"tag": tag, "msg": hexs(msg), "hash": hexs(&tagged_hash(tag, msg))}))
    .collect::<Vec<_>>();

    json!({
        "description": "VmOp signing vectors (SPEC §4.2). sighash = tagged_hash(\"Ksana/VmOp/v1\", unsigned_borsh), where unsigned_borsh is the Borsh encoding of the VmOp without sig; for path C (fee is BtcNative, D-086) the message is unsigned_borsh ‖ first_input.txid ‖ first_input.vout (little-endian u32), where first_input is the output spent by the first input of the carrying transaction, and verification with a different input must fail; the account is the x-only public key of secret_key tweaked per BIP86 (D-092: i.e. the output public key of that secret key's Taproot address), and the signature is BIP340(tweaked secret key, sighash, aux_rand). Every op in tampered must fail verification. bip322 (D-052, D-092): message is the lowercase hex of sighash, message_hash = tagged_hash(\"BIP0322-signed-message\", message), to_spend / to_sign are built per BIP322 (transaction IDs in Bitcoin internal byte order, D-001), the address is OP_1 <account> (script_pubkey), witness is the consensus encoding of the to_sign witness stack, i.e. the Bip322Simple signature bytes, a P2TR key-path signature made with the tweaked key per BIP340(aux_rand); only P2TR, P2WPKH no longer corresponds to an account.",
        "tag": VMOP_SIGHASH_TAG,
        "tagged_hash": tagged,
        "cases": cases,
        "tampered": tampered,
        "bip322": bip322_cases,
    })
}

fn merkle_vectors() -> Value {
    let leaves: Vec<Vec<u8>> = [
        "",
        "00",
        "10",
        "2021",
        "3031",
        "40414243",
        "5051525354555657",
        "606162636465666768696a6b6c6d6e6f",
    ]
    .iter()
    .map(|h| hex::decode(h).unwrap())
    .collect();
    let rfc6962 = (0..=leaves.len())
        .map(|n| {
            json!({
                "leaves": leaves[..n].iter().map(|l| hexs(l)).collect::<Vec<_>>(),
                "root": hexs(&merkle_root(&leaves[..n])),
            })
        })
        .collect::<Vec<_>>();

    let ops = signed_ops();
    let all: Vec<Entry> = entries(&ops).into_iter().map(|(_, e)| e).collect();
    let entry_cases = [
        ("empty block", vec![]),
        ("single entry", vec![all[2].clone()]),
        ("three entries", all[..3].to_vec()),
        ("all entries (three variants)", all.clone()),
    ]
    .iter()
    .map(|(name, es)| {
        json!({
            "name": name,
            "entries": serde_json::to_value(es).unwrap(),
            "entries_root": hexs(&entries_root(es).unwrap()),
        })
    })
    .collect::<Vec<_>>();

    json!({
        "description": "entries_root uses the RFC 6962 Merkle Tree Hash (DECISIONS D-004): MTH({}) = sha256(\"\"), leaf sha256(0x00 ‖ d), internal node sha256(0x01 ‖ left ‖ right), the left subtree takes the largest power of 2 less than n leaves. The leaves d of entries_root are the Borsh encodings of the entries.",
        "rfc6962": rfc6962,
        "entries_root": entry_cases,
    })
}

fn main() {
    let out = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/vectors"));
    std::fs::create_dir_all(&out).unwrap();
    for (file, value) in [
        ("codec.json", codec_vectors()),
        ("signing.json", signing_vectors()),
        ("merkle.json", merkle_vectors()),
    ] {
        let mut text = serde_json::to_string_pretty(&value).unwrap();
        text.push('\n');
        std::fs::write(out.join(file), text).unwrap();
        println!("wrote {}", out.join(file).display());
    }
}
