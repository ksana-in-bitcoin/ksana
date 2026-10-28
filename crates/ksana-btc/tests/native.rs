//! Recognition of native transactions (path C), the first input (D-086), the forced-inclusion window and the entry
//! prefix (SPEC §7.4, D-036).

use bitcoin::block::{Header, Version as BlockVersion};
use bitcoin::hashes::Hash;
use bitcoin::{
    absolute, transaction, Amount, Block, BlockHash, CompactTarget, ScriptBuf, Sequence,
    Transaction, TxIn, TxMerkleNode, TxOut, Witness,
};
use ksana_btc::data::data_script;
use ksana_btc::native::{
    check_forced_prefix, first_input, forced_entries, forced_heights, inclusion_height, native_op,
    native_txs, outpoint_of, tx_outputs, with_native_op, NativeTx,
};
use ksana_primitives::{
    Action, BlockHeader, BtcAnchor, Entry, FeeMode, OutPoint, Params, Payload, SigScheme, VmOp,
    DEVNET,
};

fn op(nonce: u64, fee: FeeMode) -> VmOp {
    VmOp {
        version: 1,
        chain_id: DEVNET.chain_id,
        account: [9; 32],
        nonce,
        fee,
        max_gas: 10_000,
        action: Action::Call {
            contract: [1; 32],
            method: "ping".into(),
            args: vec![],
            funds: Vec::new(),
        },
        sig: SigScheme::Bip340([2; 64]),
    }
}

fn header() -> BlockHeader {
    BlockHeader {
        version: 1,
        height: 1,
        parents: vec![[0; 32]],
        btc_anchor: BtcAnchor {
            height: 1,
            hash: [0; 32],
        },
        entries_root: [0; 32],
        state_root: [0; 32],
        producer: [0; 32],
        timestamp: 0,
        bits: 0,
        nonce: 0,
    }
}

fn data_out(bytes: &[u8]) -> TxOut {
    TxOut {
        value: Amount::ZERO,
        script_pubkey: data_script(&DEVNET.magic, bytes).unwrap(),
    }
}

fn payment_out() -> TxOut {
    TxOut {
        value: Amount::from_sat(5_000),
        script_pubkey: ScriptBuf::from_bytes([&[0x00, 0x14][..], &[0x77; 20]].concat()),
    }
}

/// A transaction spending one output determined by `seed`, with the given outputs.
fn tx(seed: u8, output: Vec<TxOut>) -> Transaction {
    Transaction {
        version: transaction::Version::TWO,
        lock_time: absolute::LockTime::ZERO,
        input: vec![TxIn {
            previous_output: bitcoin::OutPoint {
                txid: bitcoin::Txid::from_byte_array([seed; 32]),
                vout: 0,
            },
            script_sig: ScriptBuf::new(),
            sequence: Sequence::MAX,
            witness: Witness::from_slice(&[[seed; 64]]),
        }],
        output,
    }
}

fn coinbase(output: Vec<TxOut>) -> Transaction {
    Transaction {
        version: transaction::Version::TWO,
        lock_time: absolute::LockTime::ZERO,
        input: vec![TxIn {
            previous_output: bitcoin::OutPoint::null(),
            script_sig: ScriptBuf::from_bytes(vec![0x01, 0x64]),
            sequence: Sequence::MAX,
            witness: Witness::new(),
        }],
        output,
    }
}

fn native_payload(op: VmOp) -> Vec<u8> {
    Payload::NativeOp { op }.encode(&DEVNET).unwrap()
}

fn block(txdata: Vec<Transaction>) -> Block {
    Block {
        header: Header {
            version: BlockVersion::TWO,
            prev_blockhash: BlockHash::all_zeros(),
            merkle_root: TxMerkleNode::all_zeros(),
            time: 0,
            bits: CompactTarget::from_consensus(0x207f_ffff),
            nonce: 0,
        },
        txdata,
    }
}

fn txid(tx: &Transaction) -> [u8; 32] {
    tx.compute_txid().to_byte_array()
}

#[test]
fn native_transactions_are_identified_in_block_order() {
    let token_fee = FeeMode::Token { amount: u128::MAX };
    let mut with_trailing = native_payload(op(8, FeeMode::BtcNative));
    with_trailing.push(0);
    let settlement = Payload::Settlement {
        headers: vec![header()],
        entries: vec![],
        bodies: vec![],
    }
    .encode(&DEVNET)
    .unwrap();
    let headers = Payload::Headers {
        headers: vec![header()],
        bodies: vec![],
        payee: [0; 32],
    }
    .encode(&DEVNET)
    .unwrap();

    let txs = vec![
        // 0: a coinbase is not a native transaction (D-086: no first input to bind to)
        coinbase(vec![
            payment_out(),
            data_out(&native_payload(op(0, FeeMode::BtcNative))),
        ]),
        // 1: an ordinary payment
        tx(1, vec![payment_out()]),
        // 2: fee is not BtcNative
        tx(
            2,
            vec![data_out(&native_payload(op(
                2,
                FeeMode::Token { amount: 1 },
            )))],
        ),
        // 3: a native transaction, the data output is not the first one
        tx(
            3,
            vec![
                payment_out(),
                data_out(&native_payload(op(3, FeeMode::BtcNative))),
            ],
        ),
        // 4: the payload cannot be decoded
        tx(4, vec![data_out(&[0xff; 40])]),
        // 5, 6: settlement and republish payloads
        tx(5, vec![data_out(&settlement)]),
        tx(6, vec![data_out(&headers)]),
        // 7: two KSN1 outputs
        tx(
            7,
            vec![
                data_out(&native_payload(op(7, FeeMode::BtcNative))),
                data_out(&native_payload(op(7, FeeMode::BtcNative))),
            ],
        ),
        // 8: trailing bytes
        tx(8, vec![data_out(&with_trailing)]),
        // 9: another Token fee
        tx(9, vec![data_out(&native_payload(op(9, token_fee)))]),
        // 10: a native transaction
        tx(
            10,
            vec![data_out(&native_payload(op(10, FeeMode::BtcNative)))],
        ),
    ];
    let expected: Vec<NativeTx> = [(3u8, 3u64), (10, 10)]
        .into_iter()
        .map(|(seed, nonce)| NativeTx {
            txid: txid(&txs[usize::from(seed)]),
            op: op(nonce, FeeMode::BtcNative),
            // `tx(seed, …)` spends `[seed; 32]:0`
            first_input: OutPoint {
                txid: [seed; 32],
                vout: 0,
            },
            outputs: tx_outputs(&txs[usize::from(seed)]),
        })
        .collect();
    for (index, t) in txs.iter().enumerate() {
        let is_native = [3, 10].contains(&index);
        assert_eq!(native_op(&DEVNET, t).is_some(), is_native, "tx {index}");
    }
    let found = native_txs(&DEVNET, &block(txs));
    assert_eq!(found, expected);
    // The transaction ID is in internal byte order: the reverse of the hex shown by RPC
    let display = bitcoin::Txid::from_byte_array(found[0].txid).to_string();
    let mut raw = found[0].txid;
    raw.reverse();
    assert_eq!(display, hex::encode(raw));
}

#[test]
fn native_payload_over_the_limit_is_ignored() {
    let payload = native_payload(op(0, FeeMode::BtcNative));
    let t = tx(1, vec![data_out(&payload)]);
    let mut params = DEVNET.clone();
    params.max_block_data_bytes = payload.len() - 1;
    assert!(native_txs(&params, &block(vec![t.clone()])).is_empty());
    params.max_block_data_bytes = payload.len();
    assert_eq!(native_txs(&params, &block(vec![t])).len(), 1);
}

fn params_with_confirmations(n: u32) -> Params {
    let mut params = DEVNET.clone();
    params.native_confirmations = n;
    params
}

#[test]
fn forced_heights_with_one_confirmation() {
    let p = params_with_confirmations(1);
    assert_eq!(inclusion_height(&p, 0), 0);
    assert_eq!(inclusion_height(&p, 7), 7);
    // (h(parent block), h(B)] = (parent, anchor]
    assert_eq!(forced_heights(&p, 0, 0), None);
    assert_eq!(forced_heights(&p, 0, 1), Some(1..=1));
    assert_eq!(forced_heights(&p, 1, 2), Some(2..=2));
    assert_eq!(forced_heights(&p, 0, 2), Some(1..=2));
    assert_eq!(forced_heights(&p, 5, 5), None);
    assert_eq!(forced_heights(&p, 5, 9), Some(6..=9));
    // When the anchor height decreases (invalid block) the window is empty
    assert_eq!(forced_heights(&p, 9, 5), None);
    assert_eq!(
        forced_heights(&p, u32::MAX - 1, u32::MAX),
        Some(u32::MAX..=u32::MAX)
    );
}

#[test]
fn forced_heights_with_three_confirmations() {
    let p = params_with_confirmations(3);
    assert_eq!(inclusion_height(&p, 0), -2);
    assert_eq!(inclusion_height(&p, 1), -1);
    assert_eq!(inclusion_height(&p, 2), 0);
    // h = anchor − 2; the negative part is cut off
    assert_eq!(forced_heights(&p, 0, 0), None);
    assert_eq!(forced_heights(&p, 0, 1), None);
    assert_eq!(forced_heights(&p, 0, 2), Some(0..=0));
    assert_eq!(forced_heights(&p, 1, 2), Some(0..=0));
    assert_eq!(forced_heights(&p, 2, 2), None);
    assert_eq!(forced_heights(&p, 2, 3), Some(1..=1));
    assert_eq!(forced_heights(&p, 0, 5), Some(0..=3));
    assert_eq!(forced_heights(&p, 10, 10), None);
    assert_eq!(forced_heights(&p, 10, 13), Some(9..=11));
}

#[test]
fn consecutive_blocks_cover_every_height_exactly_once() {
    for confirmations in [1, 3] {
        let p = params_with_confirmations(confirmations);
        // A sequence of anchor heights (may stay the same, may jump)
        let anchors = [0u32, 0, 1, 1, 2, 5, 5, 6, 9, 12];
        let mut covered = vec![];
        for pair in anchors.windows(2) {
            if let Some(range) = forced_heights(&p, pair[0], pair[1]) {
                covered.extend(range);
            }
        }
        // Heights up to and including the first block's are the responsibility of earlier blocks
        let first = (inclusion_height(&p, anchors[0]) + 1).max(0);
        let last = inclusion_height(&p, *anchors.last().unwrap());
        let expected: Vec<u32> =
            (u32::try_from(first).unwrap()..=u32::try_from(last).unwrap()).collect();
        assert_eq!(covered, expected, "NATIVE_CONFIRMATIONS = {confirmations}");
    }
}

#[test]
fn first_input_is_the_first_spent_output_and_coinbase_has_none() {
    let t = tx(7, vec![payment_out()]);
    assert_eq!(
        first_input(&t),
        Some(OutPoint {
            txid: [7; 32],
            vout: 0
        })
    );
    assert_eq!(
        first_input(&t),
        Some(outpoint_of(&t.input[0].previous_output))
    );
    // Several inputs: only the first one counts
    let mut two = t.clone();
    two.input.push(TxIn {
        previous_output: bitcoin::OutPoint {
            txid: bitcoin::Txid::from_byte_array([8; 32]),
            vout: 3,
        },
        script_sig: ScriptBuf::new(),
        sequence: Sequence::MAX,
        witness: Witness::new(),
    });
    assert_eq!(first_input(&two), first_input(&t));
    assert_eq!(first_input(&coinbase(vec![payment_out()])), None);
}

#[test]
fn with_native_op_replaces_only_the_data_output() {
    let placeholder = op(0, FeeMode::BtcNative);
    let funded = tx(
        3,
        vec![
            payment_out(),
            data_out(&native_payload(placeholder.clone())),
            TxOut {
                value: Amount::from_sat(777),
                script_pubkey: ScriptBuf::from_bytes(vec![0x51]),
            },
        ],
    );
    assert_eq!(native_op(&DEVNET, &funded), Some(placeholder));
    let mut signed = op(5, FeeMode::BtcNative);
    signed.sig = SigScheme::Bip340([6; 64]);
    let replaced = with_native_op(&funded, &DEVNET, &signed).unwrap();
    assert_eq!(native_op(&DEVNET, &replaced), Some(signed.clone()));
    assert_eq!(replaced.input, funded.input);
    assert_eq!(replaced.output.len(), 3);
    assert_eq!(replaced.output[0], funded.output[0]);
    assert_eq!(replaced.output[2], funded.output[2]);
    assert_eq!(replaced.output[1].value, Amount::ZERO);
    assert_eq!(first_input(&replaced), first_input(&funded));
    // No data output, or two of them: cannot replace
    assert!(with_native_op(&tx(1, vec![payment_out()]), &DEVNET, &signed).is_err());
    let two = tx(
        2,
        vec![
            data_out(&native_payload(op(1, FeeMode::BtcNative))),
            data_out(&native_payload(op(2, FeeMode::BtcNative))),
        ],
    );
    assert!(with_native_op(&two, &DEVNET, &signed).is_err());
}

fn natives(n: u8) -> Vec<NativeTx> {
    (0..n)
        .map(|i| NativeTx {
            txid: [i + 100; 32],
            op: op(u64::from(i), FeeMode::BtcNative),
            first_input: OutPoint {
                txid: [i + 200; 32],
                vout: u32::from(i),
            },
            outputs: Vec::new(),
        })
        .collect()
}

fn sponsored() -> Entry {
    Entry::Sponsored {
        op: op(50, FeeMode::Token { amount: 1 }),
    }
}

#[test]
fn forced_entries_keep_order() {
    let txs = natives(3);
    let entries = forced_entries(&txs);
    assert_eq!(entries.len(), 3);
    for (entry, tx) in entries.iter().zip(&txs) {
        assert_eq!(
            entry,
            &Entry::Forced {
                btc_txid: tx.txid,
                op: tx.op.clone()
            }
        );
    }
    assert!(forced_entries(&[]).is_empty());
}

#[test]
fn forced_prefix_is_accepted() {
    let expected = forced_entries(&natives(3));
    // Exactly the forced-inclusion entries
    assert_eq!(check_forced_prefix(&expected, &expected), Ok(()));
    // Followed by other entries
    let mut entries = expected.clone();
    entries.push(sponsored());
    entries.push(Entry::Unlock {
        height: 1,
        index: 0,
    });
    assert_eq!(check_forced_prefix(&entries, &expected), Ok(()));
    // No native transactions to include
    assert_eq!(check_forced_prefix(&[], &[]), Ok(()));
    assert_eq!(check_forced_prefix(&[sponsored()], &[]), Ok(()));
}

#[test]
fn forced_prefix_violations_are_rejected() {
    let expected = forced_entries(&natives(3));
    let mut cases: Vec<(&str, Vec<Entry>)> = vec![];

    cases.push(("block has no entries", vec![]));
    cases.push(("last one missing", expected[..2].to_vec()));
    let mut missing_middle = expected.clone();
    missing_middle.remove(1);
    missing_middle.push(sponsored());
    cases.push(("middle one missing", missing_middle));
    let mut swapped = expected.clone();
    swapped.swap(0, 1);
    cases.push(("wrong order", swapped));
    let mut other_op = expected.clone();
    if let Entry::Forced { op, .. } = &mut other_op[2] {
        op.nonce += 1;
    }
    cases.push(("same transaction ID, different VmOp", other_op));
    let mut other_txid = expected.clone();
    if let Entry::Forced { btc_txid, .. } = &mut other_txid[0] {
        btc_txid[0] ^= 1;
    }
    cases.push(("different transaction ID", other_txid));
    let mut not_first = vec![sponsored()];
    not_first.extend(expected.clone());
    cases.push(("forced-inclusion entries not at the start", not_first));
    let mut duplicate = expected.clone();
    duplicate.push(expected[2].clone());
    cases.push(("an extra duplicate forced-inclusion entry", duplicate));
    let mut later = expected.clone();
    later.push(sponsored());
    later.push(Entry::Forced {
        btc_txid: [200; 32],
        op: op(99, FeeMode::BtcNative),
    });
    cases.push(("a forced-inclusion entry after other entries", later));

    for (name, entries) in cases {
        let result = check_forced_prefix(&entries, &expected);
        assert!(result.is_err(), "{name}");
    }

    // When no forced inclusion is required, any Forced entry is superfluous
    let err = check_forced_prefix(&expected[..1], &[]).unwrap_err();
    assert!(err.contains("superfluous"), "{err}");
}
