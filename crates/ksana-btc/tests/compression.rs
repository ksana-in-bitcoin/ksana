//! M14 cost comparison (D-079, D-080, D-081): the bytes and vB on BTC when the same chain segment (the headers and
//! bodies carried by one settlement) lands on BTC under the rules before and after compression.
//!
//! - Before compression (the M13 rules): empty blocks also changed the state root (block rewards were booked right
//!   away), every header wrote out its state root; entries were written as Borsh (a path-B transfer is 217 bytes); the
//!   counts in the payload and the header indices of the bodies were 4-byte integers; the payload sat in the OP_RETURN
//!   data output of the settlement transaction (1 byte = 1 vB), and the settlement transaction had one funding input
//!   and three outputs: change, data, P2A.
//! - After compression: empty blocks do not change the state root (D-079, saving 32 bytes per header), entries are
//!   compressed (D-080), the payload sits in the envelope of the reveal transaction's witness (D-081, 1 byte =
//!   0.25 vB), two transactions: commit and reveal.
//!
//! Headers, entries and transactions are all computed with the real encoders and transaction construction (not an
//! estimation formula); the pre-compression payload is measured item by item in the format of that time, and the
//! transaction is assembled in the structure of that time and weighed by rust-bitcoin.

mod common;

use bitcoin::{
    absolute, transaction, Amount, ScriptBuf, Sequence, Transaction, TxIn, TxOut, Witness,
};
use common::*;
use ksana_btc::data::data_script;
use ksana_btc::settlement::{self, p2a_script};
use ksana_primitives::{
    compact, Action, BlockHeader, Body, BtcAnchor, EntryData, FeeMode, Payload, SigScheme, VmOp,
    DEVNET, KSA_ASSET_ID,
};

/// A chain segment: `blocks` blocks (one every 5 seconds, three producers in turn), of which `with_ops` blocks each
/// contain `per_block` path-B KSA transfers (among `accounts` accounts); the single settlement carried by the last
/// block carries all the headers and bodies.
struct Scenario {
    name: &'static str,
    blocks: u64,
    with_ops: u64,
    per_block: u64,
    accounts: u8,
}

/// The result of one scenario: payload bytes and the vB landed on BTC.
struct Cost {
    bytes: usize,
    vsize: usize,
}

fn account(i: u8) -> [u8; 32] {
    [0x10 + i; 32]
}

fn transfer(from: u8, to: u8, nonce: u64, amount: u128) -> EntryData {
    EntryData::Sponsored {
        op: VmOp {
            version: ksana_primitives::VMOP_VERSION,
            chain_id: DEVNET.chain_id,
            account: account(from),
            nonce,
            fee: FeeMode::Token { amount: 2_000 },
            max_gas: 50_000,
            action: Action::AssetTransfer {
                asset: KSA_ASSET_ID,
                to: account(to),
                amount,
            },
            sig: SigScheme::Bip340([0x5a; 64]),
        },
    }
}

/// The scenario's headers (when `keep_root` is true, blocks without entries keep the parent's state root) and the
/// entries of each block.
fn chain(s: &Scenario, keep_root: bool) -> (Vec<BlockHeader>, Vec<Vec<EntryData>>) {
    // Blocks with ops are spread evenly; the last one falls on the last block of the segment (the block carrying the
    // settlement transaction) or before it
    let spacing = s.blocks.checked_div(s.with_ops).unwrap_or(0);
    let mut headers: Vec<BlockHeader> = Vec::new();
    let mut entries = Vec::new();
    let mut nonces = vec![0u64; usize::from(s.accounts)];
    let mut state_root = [0x77; 32];
    for i in 0..s.blocks {
        let has_ops = spacing > 0 && (i + 1) % spacing == 0 && (i + 1) / spacing <= s.with_ops;
        let own: Vec<EntryData> = if has_ops {
            (0..s.per_block)
                .map(|k| {
                    let from = u8::try_from((i + k) % u64::from(s.accounts)).unwrap();
                    let to = (from + 1) % s.accounts;
                    let nonce = nonces[usize::from(from)];
                    nonces[usize::from(from)] += 1;
                    transfer(from, to, nonce, 1_000 + u128::from(i * 7 + k))
                })
                .collect()
        } else {
            Vec::new()
        };
        if !keep_root || !own.is_empty() {
            state_root =
                ksana_primitives::hash::sha256(&[&state_root[..], &i.to_le_bytes()].concat());
        }
        let parent = headers.last().map_or([0x01; 32], BlockHeader::hash);
        headers.push(BlockHeader {
            version: ksana_primitives::BLOCK_VERSION,
            height: 1_000 + i,
            parents: vec![parent],
            btc_anchor: BtcAnchor {
                height: 900,
                hash: [0x02; 32],
            },
            entries_root: if own.is_empty() {
                compact::empty_entries_root()
            } else {
                ksana_primitives::hash::sha256(&i.to_le_bytes())
            },
            state_root,
            producer: [0x30 + u8::try_from(i % 3).unwrap(); 32],
            timestamp: 1_790_000_000 + i * 5,
            bits: 0x207f_ffff,
            nonce: i * 13 + 5,
        });
        entries.push(own);
    }
    (headers, entries)
}

/// Bodies: the blocks that have entries (the last block is the one carrying the settlement transaction; its entries are
/// the payload's own-block entries and do not count as a body).
fn bodies(entries: &[Vec<EntryData>]) -> Vec<Body> {
    let last = entries.len() - 1;
    entries
        .iter()
        .enumerate()
        .filter(|(i, own)| *i != last && !own.is_empty())
        .map(|(i, own)| Body {
            header: u32::try_from(i).unwrap(),
            entries: own.clone(),
        })
        .collect()
}

/// Before compression: Borsh entries, 4-byte counts and indices, every header writes out its state root; the settlement
/// transaction in OP_RETURN (one P2TR funding input; three outputs: change, data, P2A).
fn before(s: &Scenario) -> Cost {
    let (headers, entries) = chain(s, false);
    let borsh = |e: &EntryData| ksana_primitives::codec::encode(e).unwrap().len();
    let own = entries.last().unwrap();
    let mut bytes = 1 + compact::encoded_len(&headers).unwrap() + 4 + 4;
    for body in bodies(&entries) {
        bytes += 4 + 4 + body.entries.iter().map(borsh).sum::<usize>();
    }
    bytes += own.iter().map(borsh).sum::<usize>();
    let tx = Transaction {
        version: transaction::Version::TWO,
        lock_time: absolute::LockTime::ZERO,
        input: vec![TxIn {
            previous_output: outpoint("before", 0),
            script_sig: ScriptBuf::new(),
            sequence: Sequence(settlement::PRODUCER_SEQUENCE),
            witness: Witness::from_slice(&[[0u8; 64]]),
        }],
        output: vec![
            txout(100_000, producer_script(&producer_key())),
            TxOut {
                value: Amount::ZERO,
                script_pubkey: data_script(&DEVNET.magic, &vec![0x55; bytes]).unwrap(),
            },
            txout(settlement::P2A_AMOUNT_SAT, p2a_script()),
        ],
    };
    Cost {
        bytes,
        vsize: usize::try_from(tx.weight().to_vbytes_ceil()).unwrap(),
    }
}

/// After compression: the real payload encoding and the two transactions, commit / reveal.
fn after(s: &Scenario) -> Cost {
    let (headers, entries) = chain(s, true);
    let payload = Payload::Settlement {
        headers,
        entries: entries.last().unwrap().clone(),
        bodies: bodies(&entries),
    }
    .encode(&DEVNET)
    .unwrap();
    let key = producer_key();
    let producer = producer_utxos(&key, &[10_000_000]);
    let draft = settlement::draft(
        &DEVNET,
        &producer,
        producer_script(&key),
        &key.x_only_public_key().0,
        &payload,
        1,
    )
    .unwrap();
    let pair = settlement::finalize(&DEVNET, draft, &payload, &key).unwrap();
    settlement::validate_attached(&DEVNET, &pair, &payload, &source(&producer)).unwrap();
    Cost {
        bytes: payload.len(),
        vsize: pair.commit.vsize() + pair.reveal.vsize(),
    }
}

#[test]
fn compression_cost_comparison() {
    let scenarios = [
        Scenario {
            name: "single-block settlement: 1 transfer, no other headers",
            blocks: 1,
            with_ops: 1,
            per_block: 1,
            accounts: 2,
        },
        Scenario {
            name: "10 minutes (120 blocks) without ops, headers only",
            blocks: 120,
            with_ops: 0,
            per_block: 0,
            accounts: 2,
        },
        Scenario {
            name: "10 minutes, 120 blocks, 1 transfer",
            blocks: 120,
            with_ops: 1,
            per_block: 1,
            accounts: 2,
        },
        Scenario {
            name: "10 minutes, 120 blocks, 20 blocks with 1 transfer each (4 accounts)",
            blocks: 120,
            with_ops: 20,
            per_block: 1,
            accounts: 4,
        },
        Scenario {
            name: "10 minutes, 120 blocks, 30 blocks with 10 transfers each (20 accounts)",
            blocks: 120,
            with_ops: 30,
            per_block: 10,
            accounts: 20,
        },
        Scenario {
            name: "500 headers (MAX_SETTLEMENT_HEADERS), 60 transfers (10 accounts)",
            blocks: 500,
            with_ops: 60,
            per_block: 1,
            accounts: 10,
        },
    ];
    eprintln!("Bytes and vB of the same chain segment landed on BTC before and after compression (D-079, D-080, D-081):");
    eprintln!("| scenario | before: bytes / vB | after: bytes / vB | vB ratio |");
    eprintln!("| --- | --- | --- | --- |");
    for s in &scenarios {
        let (old, new) = (before(s), after(s));
        eprintln!(
            "| {} | {} / {} | {} / {} | {}% |",
            s.name,
            old.bytes,
            old.vsize,
            new.bytes,
            new.vsize,
            new.vsize * 100 / old.vsize
        );
        assert!(
            new.bytes < old.bytes,
            "{}: the payload should shrink",
            s.name
        );
        if s.blocks > 1 {
            // With other headers present (the norm under settlement batching) a payload in the witness is far cheaper
            // than OP_RETURN; with only a single small payload the fixed overhead of two transactions may be higher
            assert!(new.vsize < old.vsize, "{}: vB should drop", s.name);
        }
    }
}
