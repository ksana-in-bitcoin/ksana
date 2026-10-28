//! Generates the initial fuzzing corpus (`fuzz/corpus/<target>/`): VmOps with valid signatures, the three kinds of
//! payload, data scripts and envelope leaves, signed settlement transactions (the commit / reveal pair in a block and
//! a single reveal transaction, D-081), command sequences covering the success path of every host function, and the
//! requests of the differential test of the TypeScript SDK (D-133).
//! Random bytes almost never hit these structures; with seeds, mutation can reach deep into the validation logic.
//!
//! Run: `cargo run --manifest-path fuzz/Cargo.toml --example seeds` (see fuzz/README.md).

#[path = "../../crates/ksana-btc/tests/common/mod.rs"]
mod btc_common;

use std::fs;
use std::path::PathBuf;

use bitcoin::consensus::encode::serialize;
use btc_common::{settle, settlement_payload};
use ksana_btc::data::data_script;
use ksana_btc::envelope::envelope_script;
use ksana_fuzz::differential::gen::{generate, kinds, Rng, Seeds};
use ksana_primitives::{
    compact, Action, BlockHeader, Body, BtcAnchor, EntryData, FeeMode, Payload, SigScheme, VmOp,
    DEVNET,
};

fn write(target: &str, name: &str, bytes: &[u8]) {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("corpus")
        .join(target);
    fs::create_dir_all(&dir).expect("create the corpus directory");
    fs::write(dir.join(name), bytes).expect("write the seed");
}

fn sample_ops() -> Vec<VmOp> {
    let keypair =
        ksana_crypto::keypair_from_secret(&[0x33; 32]).expect("the fixed secret key is valid");
    let account = ksana_crypto::account_of(&keypair);
    let base = |nonce: u64, fee: FeeMode, action: Action| VmOp {
        version: 1,
        chain_id: DEVNET.chain_id,
        account,
        nonce,
        fee,
        max_gas: 1_000_000,
        action,
        sig: SigScheme::Bip340([0; 64]),
    };
    let ops = vec![
        base(
            0,
            FeeMode::Token { amount: 3 },
            Action::AssetTransfer {
                asset: [0; 32],
                to: [9; 32],
                amount: 5,
            },
        ),
        base(
            1,
            FeeMode::Token { amount: 4 },
            Action::Call {
                contract: [1; 32],
                method: "transfer".into(),
                args: vec![1, 2, 3],
                funds: Vec::new(),
            },
        ),
        base(
            2,
            FeeMode::BtcNative,
            Action::Deploy {
                code: b"\0asm\x01\0\0\0".to_vec(),
                init_args: vec![],
                funds: Vec::new(),
            },
        ),
    ];
    // A path-C op is bound to the first input of the carrying transaction (D-086); the seed only needs the right
    // format, so the input is a fixed value
    let first_input = ksana_primitives::OutPoint {
        txid: [0xc1; 32],
        vout: 1,
    };
    ops.into_iter()
        .map(|mut op| {
            op.sig = match op.fee {
                FeeMode::Token { .. } => {
                    ksana_crypto::sign_vmop(&op, &keypair, &[7; 32]).expect("sign")
                }
                FeeMode::BtcNative => {
                    ksana_crypto::sign_native_vmop(&op, &first_input, &keypair, &[7; 32])
                        .expect("sign")
                }
            };
            op
        })
        .collect()
}

/// Parameter slot of the host function test contract (see fuzz_targets/host_params.rs).
#[derive(Clone, Copy)]
enum Arg {
    Raw(u64),
    Ptr(u16),
    Small(u16),
}

fn cmd(selector: u8, args: &[Arg]) -> Vec<u8> {
    let mut out = vec![selector];
    for k in 0..6 {
        let (tag, value) = match args.get(k).copied().unwrap_or(Arg::Raw(0)) {
            Arg::Raw(v) => (0u8, v),
            Arg::Ptr(o) => (1, u64::from(o)),
            Arg::Small(s) => (2, u64::from(s)),
        };
        out.push(tag);
        out.extend(value.to_le_bytes());
    }
    out
}

/// Borsh-encoded `Vec<Funds>`: one item.
fn funds_bytes(asset: &[u8; 32], amount: u128) -> Vec<u8> {
    let mut out = 1u32.to_le_bytes().to_vec();
    out.extend(asset);
    out.extend(amount.to_le_bytes());
    out
}

fn program(cmds: &[Vec<u8>], data: &[u8]) -> Vec<u8> {
    let mut out = vec![u8::try_from(cmds.len()).expect("at most 8 commands")];
    for c in cmds {
        out.extend(c);
    }
    out.extend(data);
    out
}

fn host_seeds() -> Vec<Vec<u8>> {
    use Arg::{Ptr, Raw, Small};
    // Selectors: 0 storage_get, 1 storage_set, 2 storage_remove, 3 caller, 4 self_id, 5 block_height,
    // 6 btc_anchor_height, 7 btc_header, 8 asset_balance, 9 asset_transfer, 10 asset_issue, 11 call_contract,
    // 12 emit_event, 13 sha256, 14 verify_schnorr, 15 call_funds, 16 try_call_contract, 17 delegate_call,
    // 18 asset_create, 19 asset_freeze, 20 asset_allow, 21 asset_pause, 22 asset_renounce, 23 asset_policy,
    // 24 asset_status (D-121)
    let mut assets = vec![9u8; 32]; // asset [9; 32]
    assets.extend([0x22; 32]); // recipient
    assets.extend(5u128.to_le_bytes()); // amount
    let mut issue = vec![0x01; 32]; // sub_id
    issue.extend([1, 0, 0, 0, b'T', 1, 0, 0, 0, b'T', 8]); // AssetMetaInput { name: "T", symbol: "T", decimals: 8 }
    issue.extend([0x22; 32]); // to
    issue.extend(1_000u128.to_le_bytes());
    // asset_create with every capability, then each management function and the two queries (D-121); the creation
    // writes the asset ID over the first sub_id, so the management functions read the copy at 91
    let mut manage = issue.clone();
    manage.extend([0x01; 32]); // sub_id again
    let mut nested = vec![1u8; 32]; // the contract's own ID
    nested.extend([3, 0, 0, 0, b'r', b'u', b'n', 1, 0, 0, 0, 0]); // ("run", [0]): the nested call executes no commands
    vec![
        program(
            &[
                cmd(1, &[Ptr(0), Small(4), Ptr(4), Small(8)]),
                cmd(0, &[Ptr(0), Small(4)]),
                cmd(2, &[Ptr(0), Small(4)]),
                cmd(0, &[Ptr(0), Small(4)]),
            ],
            b"key-value-bytes!",
        ),
        program(
            &[
                cmd(9, &[Ptr(0), Ptr(32), Ptr(64)]),
                cmd(8, &[Ptr(0), Ptr(32), Ptr(100)]),
            ],
            &[assets, vec![0; 16]].concat(),
        ),
        program(&[cmd(7, &[Small(100), Ptr(0)])], &[0; 80]),
        program(
            &[
                cmd(7, &[Small(294), Ptr(0)]),
                cmd(7, &[Small(299), Ptr(0)]),
                cmd(7, &[Raw(u64::MAX), Ptr(0)]),
            ],
            &[0; 80],
        ),
        program(
            &[
                cmd(13, &[Ptr(0), Small(64), Ptr(100)]),
                cmd(14, &[Ptr(0), Ptr(32), Ptr(64)]),
            ],
            &[0x5A; 140],
        ),
        program(
            &[cmd(
                10,
                &[Ptr(0), Ptr(32), Small(11), Ptr(43), Ptr(75), Ptr(95)],
            )],
            &[issue, vec![0; 40]].concat(),
        ),
        program(
            &[
                cmd(18, &[Ptr(0), Ptr(32), Small(11), Raw(15), Ptr(43), Ptr(75)]),
                cmd(19, &[Ptr(91), Ptr(43), Raw(1)]),
                cmd(20, &[Ptr(91), Ptr(43), Raw(1)]),
                cmd(21, &[Ptr(91), Raw(1)]),
                cmd(22, &[Ptr(91), Raw(8)]),
                cmd(23, &[Ptr(0)]),
                cmd(24, &[Ptr(0), Ptr(43)]),
            ],
            &manage,
        ),
        program(&[cmd(11, &[Ptr(0), Ptr(32), Small(12)])], &nested),
        // Catchable nested call (gas limit 50 000) and delegate call (D-073)
        program(
            &[
                cmd(
                    16,
                    &[Ptr(0), Ptr(32), Small(12), Raw(0), Raw(0), Raw(50_000)],
                ),
                cmd(
                    16,
                    &[Ptr(0), Ptr(32), Small(12), Ptr(44), Small(52), Raw(0)],
                ),
                cmd(17, &[Ptr(0), Ptr(32), Small(12)]),
            ],
            &[nested.clone(), funds_bytes(&[9; 32], 5)].concat(),
        ),
        // The nested call attaches 5 of asset [9; 32] (transferred out of the contract account, D-073); the callee
        // reads the attached funds
        program(
            &[
                cmd(11, &[Ptr(0), Ptr(32), Small(12), Ptr(44), Small(52)]),
                cmd(15, &[]),
            ],
            &[nested.clone(), funds_bytes(&[9; 32], 5)].concat(),
        ),
        program(
            &[
                cmd(3, &[Ptr(0)]),
                cmd(4, &[Ptr(32)]),
                cmd(5, &[]),
                cmd(6, &[]),
                cmd(12, &[Ptr(0), Small(64)]),
            ],
            &[0; 64],
        ),
        program(
            &[cmd(
                1,
                &[Raw(u64::from(u32::MAX)), Small(1), Ptr(0), Raw(1 << 31)],
            )],
            &[],
        ),
    ]
}

fn main() {
    // VmOp
    for (i, op) in sample_ops().iter().enumerate() {
        write(
            "vmop_decode",
            &format!("op{i}"),
            &op.encode().expect("encode"),
        );
    }

    // Payloads: settlement (2 path-B entries), native transaction, header republish
    let settlement = settlement_payload(2, 1);
    write("payload_decode", "settlement", &settlement);
    let native = sample_ops()
        .into_iter()
        .find(|op| op.fee == FeeMode::BtcNative)
        .expect("there is a path-C sample");
    write(
        "payload_decode",
        "native",
        &Payload::NativeOp { op: native }
            .encode(&DEVNET)
            .expect("encode"),
    );
    let header = BlockHeader {
        version: 1,
        height: 3,
        parents: vec![[1; 32]],
        btc_anchor: BtcAnchor {
            height: 100,
            hash: [2; 32],
        },
        entries_root: [3; 32],
        state_root: [4; 32],
        producer: [5; 32],
        timestamp: 1_700_000_000,
        bits: 0x207f_ffff,
        nonce: 9,
    };
    let headers = Payload::Headers {
        headers: vec![
            header.clone(),
            BlockHeader {
                height: 4,
                ..header.clone()
            },
        ],
        bodies: vec![],
        payee: [0; 32],
    };
    write(
        "payload_decode",
        "headers",
        &headers.encode(&DEVNET).expect("encode"),
    );
    // Compact headers (D-075): references to parents and producers within the sequence, empty entries root,
    // anchor / bits changes, timestamp going backwards
    let second = BlockHeader {
        height: 4,
        parents: vec![header.hash()],
        entries_root: compact::empty_entries_root(),
        ..header.clone()
    };
    let third = BlockHeader {
        height: 4,
        parents: vec![header.hash()],
        producer: [6; 32],
        bits: 0x1f00_ffff,
        ..header.clone()
    };
    let fourth = BlockHeader {
        height: 5,
        parents: vec![third.hash(), second.hash(), [7; 32]],
        btc_anchor: BtcAnchor {
            height: 101,
            hash: [8; 32],
        },
        timestamp: 1_699_999_990,
        ..header.clone()
    };
    let dag_headers = vec![header.clone(), second, third, fourth];
    let dag = Payload::Headers {
        headers: dag_headers.clone(),
        bodies: vec![],
        payee: [0; 32],
    };
    write(
        "payload_decode",
        "headers_dag",
        &dag.encode(&DEVNET).expect("encode"),
    );
    // Republish payload carrying bodies (D-077): forced-inclusion, path-B and unlock entries
    let sponsored = sample_ops()
        .into_iter()
        .find(|op| matches!(op.fee, FeeMode::Token { .. }))
        .expect("there is a path-B sample");
    let with_bodies = Payload::Headers {
        headers: dag_headers,
        bodies: vec![
            Body {
                header: 1,
                entries: vec![
                    EntryData::Forced { btc_txid: [9; 32] },
                    EntryData::Sponsored { op: sponsored },
                ],
            },
            Body {
                header: 3,
                entries: vec![EntryData::Unlock {
                    height: 12,
                    index: 1,
                }],
            },
        ],
        payee: [10; 32],
    };
    write(
        "payload_decode",
        "headers_bodies",
        &with_bodies.encode(&DEVNET).expect("encode"),
    );
    // The same payload written as a data script and as an envelope leaf (payload_decode also parses the input as a
    // locking script and as a leaf script)
    let script = data_script(&DEVNET.magic, &settlement).expect("data script");
    write("payload_decode", "script", script.as_bytes());
    let key = btc_common::producer_key().x_only_public_key().0;
    let leaf = envelope_script(&key, &DEVNET.magic, &settlement).expect("envelope");
    write("payload_decode", "envelope", leaf.as_bytes());

    // Settlement transactions: the pair in a block (Borsh SettlementTxs), and a single reveal transaction (a
    // transaction scanned in a BTC block)
    for (name, ops) in [("settlement_ops", 3), ("settlement_empty", 0)] {
        let s = settle(ops, 2);
        write(
            "btc_inputs",
            name,
            &ksana_primitives::codec::encode(&s.pair.encode()).expect("encode"),
        );
        write(
            "btc_inputs",
            &format!("{name}_reveal"),
            &serialize(&s.pair.reveal),
        );
    }

    // Host function command sequences
    for (i, seed) in host_seeds().iter().enumerate() {
        write("host_params", &format!("seq{i}"), seed);
    }

    // BIP322 witnesses (D-052, D-092): the first byte 0 selects the valid account, the rest is the witness encoding --
    // a valid signature of the fixed VmOp (both the 64-byte and the 65-byte SIGHASH_ALL form), a signature of another
    // message, a two-item witness
    let keypair =
        ksana_crypto::keypair_from_secret(&[0x33; 32]).expect("the fixed secret key is valid");
    let account = ksana_crypto::account_of(&keypair);
    let op = VmOp {
        version: 1,
        chain_id: DEVNET.chain_id,
        account,
        nonce: 3,
        fee: FeeMode::Token { amount: 2 },
        max_gas: 100_000,
        action: Action::AssetTransfer {
            asset: [0; 32],
            to: [9; 32],
            amount: 5,
        },
        sig: SigScheme::Bip340([0; 64]),
    };
    let SigScheme::Bip322Simple(valid) =
        ksana_crypto::sign_vmop_bip322_p2tr(&op, &keypair, &[7; 32]).expect("sign")
    else {
        panic!("sign_vmop_bip322_p2tr returns Bip322Simple");
    };
    write("bip322_witness", "valid", &[&[0u8][..], &valid].concat());
    let mut sighash_all = valid.clone();
    if let (Some(count), Some(len)) = (sighash_all.first().copied(), sighash_all.get(1).copied()) {
        assert_eq!((count, len), (1, 64));
        sighash_all[1] = 65;
        sighash_all.push(0x01);
    }
    write(
        "bip322_witness",
        "sighash_all",
        &[&[0u8][..], &sighash_all].concat(),
    );
    let other = ksana_crypto::bip322::sign_simple_p2tr(&keypair, b"fuzz", &[7; 32]).expect("sign");
    write(
        "bip322_witness",
        "other_message",
        &[&[0u8][..], &other].concat(),
    );
    let two_items = serialize(&bitcoin::Witness::from_slice(&[
        vec![0x30u8; 72],
        vec![0x02; 33],
    ]));
    write(
        "bip322_witness",
        "two_items",
        &[&[1u8][..], &two_items].concat(),
    );
    // The differential test of the TypeScript SDK (D-133): the repository's vectors, the seeds above among them, and
    // requests built from random values of each kind's structure, in the form `Request::from_fuzz` reads
    let differential = Seeds::load(&PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(".."));
    let splice = differential.all_bytes();
    for (i, request) in differential.requests().iter().enumerate() {
        write("ts_differential", &format!("vector{i}"), &request.to_fuzz());
    }
    let mut rng = Rng::new(1);
    for (i, kind) in kinds().iter().cycle().take(20 * kinds().len()).enumerate() {
        let request = generate(&mut rng, kind, &differential, &splice);
        write("ts_differential", &format!("{kind}{i}"), &request.to_fuzz());
    }

    println!(
        "seeds written to {}",
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("corpus")
            .display()
    );
}
