//! Settlement transactions (SPEC §7.2, D-034, D-037, D-081; producer inputs only since D-078): the assemble →
//! sign → validate round trip of the commit and reveal transactions, the minimum fee of each, and the invalidity
//! verdict when each rule is violated.

mod common;

use std::collections::BTreeMap;

use bitcoin::consensus::encode::serialize;
use bitcoin::hashes::Hash;
use bitcoin::key::{Keypair, Secp256k1};
use bitcoin::sighash::{EcdsaSighashType, TapSighashType};
use bitcoin::{
    absolute, transaction, Amount, OutPoint, ScriptBuf, Transaction, TxIn, TxOut, Witness,
};
use common::*;
use ksana_btc::data::{data_script, tx_data, tx_payload, Carrier, TxData};
use ksana_btc::envelope::{commitment, envelope_script, parse_envelope};
use ksana_btc::settlement::{
    cpfp_child, decode_tx, draft, empty_op_return, finalize, p2a_script, validate_attached,
    Checked, PrevoutSource, SettlementError, SettlementPair, COMMITMENT_OUTPUT,
    MAX_SETTLEMENT_TX_BYTES, P2A_AMOUNT_SAT, PRODUCER_SEQUENCE,
};
use ksana_btc::spend::{taproot_signature, verify_taproot_key_spend};
use ksana_primitives::{Params, Payload, SettlementTxs, DEVNET};

fn attached(s: &Settled, pair: &SettlementPair) -> Result<Checked, SettlementError> {
    validate_attached(&DEVNET, pair, &s.payload, &s.source())
}

fn is_invalid<T: std::fmt::Debug>(result: Result<T, SettlementError>) -> bool {
    matches!(result, Err(SettlementError::Invalid(_)))
}

fn invalid_with<T: std::fmt::Debug>(result: Result<T, SettlementError>, text: &str) -> bool {
    matches!(result, Err(SettlementError::Invalid(m)) if m.contains(text))
}

/// One tampering of the commit transaction and its prevouts.
type CommitTamper = Box<dyn FnOnce(&mut Transaction, &mut Vec<TxOut>)>;

/// One tampering of the reveal transaction.
type RevealTamper = Box<dyn FnOnce(&mut Transaction)>;

/// Tamper with the commit transaction, then re-sign its inputs and point the reveal transaction at the new commitment
/// output and re-sign it, so that only the rule under test is violated.
fn tamper_commit(
    s: &Settled,
    f: impl FnOnce(&mut Transaction, &mut Vec<TxOut>),
) -> (SettlementPair, Vec<TxOut>) {
    let mut pair = s.pair.clone();
    let mut prevouts = s.prevouts.clone();
    f(&mut pair.commit, &mut prevouts);
    if !pair.commit.input.is_empty() && pair.commit.output.len() > 1 {
        relink(&mut pair, &prevouts, &s.key);
    }
    (pair, prevouts)
}

/// Tamper with the reveal transaction, then re-sign it via the script path (`SIGHASH_DEFAULT`).
fn tamper_reveal(s: &Settled, f: impl FnOnce(&mut Transaction)) -> SettlementPair {
    let mut pair = s.pair.clone();
    f(&mut pair.reveal);
    if pair.reveal.input.len() == 1 && pair.reveal.input[0].witness.len() >= 2 {
        let committed = pair.commit.output[1].clone();
        resign_reveal(
            &mut pair.reveal,
            &committed,
            &s.key,
            TapSighashType::Default,
        );
    }
    pair
}

/// Rebuild the commitment output (internal key = the key in the leaf; `signer`'s key when the leaf is not an envelope)
/// and the reveal transaction's witness from `leaf`, and sign the reveal transaction with `signer`; when `control` is
/// not `None` it replaces the correct control block.
fn with_leaf(
    s: &Settled,
    leaf: ScriptBuf,
    signer: &Keypair,
    control: Option<Vec<u8>>,
) -> SettlementPair {
    let mut pair = s.pair.clone();
    let internal = parse_envelope(&DEVNET.magic, &leaf)
        .and_then(|(key, _)| bitcoin::key::XOnlyPublicKey::from_slice(&key).ok())
        .unwrap_or_else(|| signer.x_only_public_key().0);
    let c = commitment(&internal, &leaf).unwrap();
    pair.commit.output[1].script_pubkey = c.script_pubkey;
    resign_producer(
        &mut pair.commit,
        &s.prevouts,
        0,
        &s.key,
        TapSighashType::Default,
    );
    pair.reveal.input[0].previous_output = OutPoint {
        txid: pair.commit.compute_txid(),
        vout: COMMITMENT_OUTPUT,
    };
    pair.reveal.input[0].witness = Witness::from_slice(&[
        vec![0u8; 64],
        leaf.to_bytes(),
        control.unwrap_or(c.control_block),
    ]);
    let committed = pair.commit.output[1].clone();
    resign_reveal(
        &mut pair.reveal,
        &committed,
        signer,
        TapSighashType::Default,
    );
    pair
}

#[test]
fn roundtrip_pays_the_minimum_fee() {
    for (ops, feerate) in [(0, 1), (3, 25), (40, 2)] {
        let s = settle(ops, feerate);
        let SettlementPair { commit, reveal } = &s.pair;
        let checked = attached(&s, &s.pair).unwrap();
        // Every signature is valid at its actual position in the transaction (independently of ksana_btc's checks)
        verify_in_place(commit, &s.prevouts).unwrap();
        verify_reveal_in_place(reveal, &commit.output[1]).unwrap();
        assert_eq!(checked.txid, reveal.compute_txid().to_byte_array());
        assert_eq!(checked.commit_txid, commit.compute_txid().to_byte_array());
        assert_eq!(checked.fee, s.fee);
        assert_eq!(checked.change, s.change);
        // The fee of each of the two is exactly the minimum at that fee rate
        let commit_vsize = commit.weight().to_vbytes_ceil();
        let reveal_vsize = reveal.weight().to_vbytes_ceil();
        assert_eq!(checked.vsize, commit_vsize + reveal_vsize);
        assert_eq!(s.reveal_fee, reveal_vsize * feerate);
        assert_eq!(s.fee, (commit_vsize + reveal_vsize) * feerate);

        // Commit transaction: v2, nLockTime 0, two producer inputs, two outputs: change and commitment
        assert_eq!(commit.version, transaction::Version::TWO);
        assert_eq!(commit.lock_time, absolute::LockTime::ZERO);
        assert_eq!(commit.input.len(), 2);
        for input in &commit.input {
            assert_eq!(input.sequence.0, PRODUCER_SEQUENCE);
            assert_eq!(input.witness.len(), 1);
            assert_eq!(input.witness.nth(0).unwrap().len(), 64);
        }
        assert_eq!(commit.output.len(), 2);
        assert_eq!(commit.output[0].script_pubkey, producer_script(&s.key));
        assert_eq!(commit.output[0].value.to_sat(), s.change);
        assert!(commit.output[1].script_pubkey.is_p2tr());
        assert_eq!(
            commit.output[1].value.to_sat(),
            s.reveal_fee + P2A_AMOUNT_SAT
        );
        let total_in: u64 = s.prevouts.iter().map(|o| o.value.to_sat()).sum();
        assert_eq!(
            total_in - s.change - commit.output[1].value.to_sat(),
            commit_vsize * feerate
        );

        // Reveal transaction: spends the commitment output, witness = signature, envelope leaf and 33-byte control
        // block, outputs = P2A and an empty OP_RETURN
        assert_eq!(reveal.version, transaction::Version::TWO);
        assert_eq!(reveal.lock_time, absolute::LockTime::ZERO);
        assert_eq!(reveal.input.len(), 1);
        assert_eq!(
            reveal.input[0].previous_output,
            OutPoint {
                txid: commit.compute_txid(),
                vout: COMMITMENT_OUTPUT
            }
        );
        let witness = reveal.input[0].witness.to_vec();
        assert_eq!(witness.len(), 3);
        assert_eq!(witness[0].len(), 64);
        assert_eq!(witness[2].len(), 33);
        let key = s.key.x_only_public_key().0;
        assert_eq!(
            witness[1],
            envelope_script(&key, &DEVNET.magic, &s.payload)
                .unwrap()
                .to_bytes()
        );
        assert_eq!(
            reveal.output,
            vec![
                TxOut {
                    value: Amount::from_sat(P2A_AMOUNT_SAT),
                    script_pubkey: p2a_script()
                },
                TxOut {
                    value: Amount::ZERO,
                    script_pubkey: empty_op_return()
                },
            ]
        );
        // Non-witness part 74 bytes: not below Bitcoin Core's 65-byte minimum
        assert_eq!(reveal.base_size(), 74);

        // The tracker recognizes the payload in the reveal transaction; the commit transaction carries no data
        assert_eq!(
            tx_data(&DEVNET.magic, reveal),
            TxData::One {
                carrier: Carrier::Input(0),
                payload: s.payload.clone()
            }
        );
        assert_eq!(
            tx_payload(&DEVNET, reveal).map(|(_, p)| p),
            Some(Payload::decode(&s.payload, &DEVNET).unwrap())
        );
        assert_eq!(tx_data(&DEVNET.magic, commit), TxData::None);
        assert_eq!(
            ksana_btc::payload::attached_payload(&DEVNET, &s.pair),
            Some(s.payload.clone())
        );

        // Round trip of the form carried in a block
        let txs = s.pair.encode();
        assert_eq!(txs.commit, serialize(commit));
        assert_eq!(SettlementPair::decode(&txs).unwrap(), s.pair);

        // With this fee rate as the consensus floor: either one paying 1 sat less is invalid
        let mut params: Params = DEVNET.clone();
        params.min_settlement_feerate_sat_vb = feerate;
        assert!(validate_attached(&params, &s.pair, &s.payload, &s.source()).is_ok());
        let (cheap_commit, _) =
            tamper_commit(&s, |tx, _| tx.output[0].value += Amount::from_sat(1));
        assert!(invalid_with(
            validate_attached(&params, &cheap_commit, &s.payload, &s.source()),
            "the fee of the commit transaction"
        ));
        // Commitment amount 1 sat short: the reveal transaction pays 1 sat less (change 1 sat more, the commit
        // transaction still pays enough)
        let (cheap_reveal, _) = tamper_commit(&s, |tx, _| {
            tx.output[1].value -= Amount::from_sat(1);
            tx.output[0].value += Amount::from_sat(1);
        });
        assert!(invalid_with(
            validate_attached(&params, &cheap_reveal, &s.payload, &s.source()),
            "the fee of the reveal transaction"
        ));
    }
}

#[test]
fn a_single_producer_input_is_enough() {
    let s = settle_with(2, 2, &[60_000]);
    assert_eq!(s.pair.commit.input.len(), 1);
    let checked = attached(&s, &s.pair).unwrap();
    assert_eq!(checked.fee, checked.vsize * 2);
}

#[test]
fn finalize_requires_equal_payload_length_and_the_producer_key() {
    let key = producer_key();
    let xonly = key.x_only_public_key().0;
    let producer = producer_utxos(&key, &[40_000]);
    let payload = settlement_payload(2, 0);
    let make = || {
        draft(
            &DEVNET,
            &producer,
            producer_script(&key),
            &xonly,
            &payload,
            1,
        )
        .unwrap()
    };
    let longer = [payload.clone(), vec![0]].concat();
    assert!(is_invalid(finalize(&DEVNET, make(), &longer, &key)));
    assert!(is_invalid(finalize(
        &DEVNET,
        make(),
        &payload[..payload.len() - 1],
        &key
    )));
    let other_key = Keypair::from_secret_key(&Secp256k1::new(), &secret("not the producer"));
    assert!(is_invalid(finalize(&DEVNET, make(), &payload, &other_key)));
    // Another payload of the same length: the transactions are valid, but match only the new payload
    let other = settlement_payload(2, 42);
    let pair = finalize(&DEVNET, make(), &other, &key).unwrap();
    let source = source(&producer);
    validate_attached(&DEVNET, &pair, &other, &source).unwrap();
    assert!(invalid_with(
        validate_attached(&DEVNET, &pair, &payload, &source),
        "the payload in the envelope does not match the block contents"
    ));
    // It does not matter that the draft estimated the size with another key: finalize recomputes the commitment from
    // the signing key
    let stranger = Keypair::from_secret_key(&Secp256k1::new(), &secret("draft key"));
    let d = draft(
        &DEVNET,
        &producer,
        producer_script(&key),
        &stranger.x_only_public_key().0,
        &payload,
        1,
    )
    .unwrap();
    let pair = finalize(&DEVNET, d, &payload, &key).unwrap();
    validate_attached(&DEVNET, &pair, &payload, &source).unwrap();
}

#[test]
fn draft_rejects_bad_inputs() {
    let key = producer_key();
    let xonly = key.x_only_public_key().0;
    let producer = producer_utxos(&key, &[40_000]);
    let payload = settlement_payload(2, 0);
    let change = producer_script(&key);
    let run = |producer: &[_], change: ScriptBuf, payload: &[u8], feerate| {
        draft(&DEVNET, producer, change, &xonly, payload, feerate)
    };

    assert!(run(&producer, change.clone(), &payload, 1).is_ok());
    // Fee rate below the consensus floor
    assert!(is_invalid(run(&producer, change.clone(), &payload, 0)));
    // No producer UTXO / insufficient producer funds (commitment amount, the fees of both and change above dust)
    assert!(matches!(
        run(&[], change.clone(), &payload, 1),
        Err(SettlementError::InsufficientFunds(_))
    ));
    let tiny = producer_utxos(&key, &[900]);
    assert!(matches!(
        run(&tiny, change.clone(), &payload, 1),
        Err(SettlementError::InsufficientFunds(_))
    ));
    assert!(matches!(
        run(&producer, change.clone(), &payload, 1_000),
        Err(SettlementError::InsufficientFunds(_))
    ));
    // The producer UTXO is not P2TR
    let mut p2wpkh = producer.clone();
    p2wpkh[0].txout.script_pubkey = p2wpkh_script(&secret("producer"));
    assert!(is_invalid(run(&p2wpkh, change.clone(), &payload, 1)));
    // The change script is not a standard type / is a KSN1 output
    assert!(is_invalid(run(&producer, p2a_script(), &payload, 1)));
    let ksn1 = data_script(&DEVNET.magic, b"x").unwrap();
    assert!(is_invalid(run(&producer, ksn1, &payload, 1)));
    // The payload is empty or too long
    assert!(is_invalid(run(&producer, change.clone(), &[], 1)));
    let huge = vec![1u8; DEVNET.max_block_data_bytes + 1];
    assert!(is_invalid(run(&producer, change.clone(), &huge, 1)));
    // Duplicate inputs
    let twice = [producer.clone(), producer.clone()].concat();
    assert!(is_invalid(run(&twice, change, &payload, 1)));
}

#[test]
fn a_full_size_payload_fits_in_the_witness() {
    // A payload of MAX_BLOCK_DATA_BYTES: the reveal transaction's weight is far below the standard transaction limit,
    // about 0.25 vB per payload byte
    let key = producer_key();
    let xonly = key.x_only_public_key().0;
    let producer = producer_utxos(&key, &[10_000_000]);
    let payload = vec![7u8; DEVNET.max_block_data_bytes];
    let d = draft(
        &DEVNET,
        &producer,
        producer_script(&key),
        &xonly,
        &payload,
        1,
    )
    .unwrap();
    let pair = finalize(&DEVNET, d, &payload, &key).unwrap();
    validate_attached(&DEVNET, &pair, &payload, &source(&producer)).unwrap();
    let vsize = pair.reveal.weight().to_vbytes_ceil();
    assert!(
        vsize < u64::try_from(payload.len()).unwrap() / 4 + 400,
        "{vsize}"
    );
    assert!(pair.reveal.weight().to_wu() < 400_000);
}

#[test]
fn commit_output_rules() {
    let s = settle(3, 2);
    let violations: Vec<(&str, CommitTamper)> = vec![
        (
            "one output too many",
            Box::new(|tx, _| {
                tx.output
                    .push(txout(1_000, producer_script(&producer_key())))
            }),
        ),
        (
            "one output too few",
            Box::new(|tx, _| tx.output.truncate(1)),
        ),
        (
            "change is a KSN1 output",
            Box::new(|tx, _| {
                tx.output[0].script_pubkey = data_script(&DEVNET.magic, b"second").unwrap();
            }),
        ),
        (
            "change below the dust limit",
            Box::new(|tx, _| tx.output[0].value = Amount::from_sat(329)),
        ),
        (
            "change locking script is not a standard type",
            Box::new(|tx, _| tx.output[0].script_pubkey = p2a_script()),
        ),
        (
            "outputs exceed inputs",
            Box::new(|tx, _| tx.output[0].value += Amount::from_sat(10_000_000)),
        ),
        (
            "commitment output and change swapped",
            Box::new(|tx, _| tx.output.swap(0, 1)),
        ),
        (
            "commitment output is not P2TR",
            Box::new(|tx, _| {
                tx.output[1].script_pubkey = p2wpkh_script(&secret("not a commitment"))
            }),
        ),
        (
            "commitment output is the producer's own P2TR address (no envelope committed)",
            Box::new(|tx, _| tx.output[1].script_pubkey = producer_script(&producer_key())),
        ),
    ];
    for (name, f) in violations {
        let (pair, _) = tamper_commit(&s, f);
        assert!(is_invalid(attached(&s, &pair)), "{name}");
    }
    // The expected payload exceeds MAX_BLOCK_DATA_BYTES
    let mut small: Params = DEVNET.clone();
    small.max_block_data_bytes = s.payload.len() - 1;
    assert!(is_invalid(validate_attached(
        &small,
        &s.pair,
        &s.payload,
        &s.source()
    )));
}

#[test]
fn version_lock_time_and_input_rules() {
    let s = settle(3, 2);
    let violations: Vec<(&str, CommitTamper)> = vec![
        (
            "version 1",
            Box::new(|tx, _| tx.version = transaction::Version::ONE),
        ),
        (
            "version 3",
            Box::new(|tx, _| tx.version = transaction::Version(3)),
        ),
        (
            "nLockTime 1",
            Box::new(|tx, _| tx.lock_time = absolute::LockTime::from_consensus(1)),
        ),
        (
            "duplicate inputs",
            Box::new(|tx, prevouts| {
                tx.input.push(tx.input[0].clone());
                prevouts.push(prevouts[0].clone());
            }),
        ),
        (
            "no inputs",
            Box::new(|tx, prevouts| {
                tx.input.clear();
                prevouts.clear();
            }),
        ),
        (
            "input with a scriptSig",
            Box::new(|tx, _| tx.input[1].script_sig = ScriptBuf::from_bytes(vec![0x00])),
        ),
    ];
    for (name, f) in violations {
        let (pair, _) = tamper_commit(&s, f);
        assert!(
            is_invalid(attached(&s, &pair)),
            "commit transaction: {name}"
        );
    }
    let reveal_violations: Vec<(&str, RevealTamper)> = vec![
        (
            "version 1",
            Box::new(|tx: &mut Transaction| tx.version = transaction::Version::ONE),
        ),
        (
            "nLockTime 1",
            Box::new(|tx: &mut Transaction| tx.lock_time = absolute::LockTime::from_consensus(1)),
        ),
    ];
    for (name, f) in reveal_violations {
        let pair = tamper_reveal(&s, f);
        assert!(
            is_invalid(attached(&s, &pair)),
            "reveal transaction: {name}"
        );
    }

    // An oversized transaction is rejected already at decoding; each of the two must be fully consumed
    assert!(is_invalid(decode_tx(&vec![
        0u8;
        MAX_SETTLEMENT_TX_BYTES + 1
    ])));
    let mut trailing = serialize(&s.pair.reveal);
    trailing.push(0);
    assert!(is_invalid(decode_tx(&trailing)));
    let txs = s.pair.encode();
    assert!(is_invalid(SettlementPair::decode(&SettlementTxs {
        commit: txs.commit.clone(),
        reveal: trailing,
    })));
    assert!(is_invalid(SettlementPair::decode(&SettlementTxs {
        commit: vec![],
        reveal: txs.reveal,
    })));
    assert!(is_invalid(decode_tx(&[0x02, 0x00, 0x00, 0x00])));
}

/// Every input of both transactions has `nSequence` = `PRODUCER_SEQUENCE` (D-100): a relative time lock (BIP68)
/// on the commit transaction's producer inputs or on the reveal transaction's input would hold the settlement off BTC
/// until it expires, and a final sequence is refused as well (one value, as the producer always writes it).
#[test]
fn input_sequences_must_be_the_producer_sequence() {
    let s = settle(3, 2);
    assert!(attached(&s, &s.pair).is_ok());
    for sequence in [10, 0x0040_0001, 0xFFFF_FFFE, 0xFFFF_FFFF] {
        let (pair, _) = tamper_commit(&s, |tx, _| {
            tx.input[1].sequence = bitcoin::Sequence(sequence);
        });
        assert!(
            invalid_with(
                attached(&s, &pair),
                "nSequence of input 1 of the commit transaction"
            ),
            "commit transaction, nSequence {sequence:#x}"
        );
        let pair = tamper_reveal(&s, |tx| tx.input[0].sequence = bitcoin::Sequence(sequence));
        assert!(
            invalid_with(
                attached(&s, &pair),
                "nSequence of input 0 of the reveal transaction"
            ),
            "reveal transaction, nSequence {sequence:#x}"
        );
    }
    assert_eq!(PRODUCER_SEQUENCE, 0xFFFF_FFFD);
}

#[test]
fn reveal_output_and_input_rules() {
    let s = settle(3, 2);
    let violations: Vec<(&str, RevealTamper)> = vec![
        (
            "P2A amount 241",
            Box::new(|tx| tx.output[0].value += Amount::from_sat(1)),
        ),
        (
            "P2A amount 239",
            Box::new(|tx| tx.output[0].value -= Amount::from_sat(1)),
        ),
        (
            "first output is not P2A",
            Box::new(|tx| tx.output[0].script_pubkey = producer_script(&producer_key())),
        ),
        ("no empty OP_RETURN", Box::new(|tx| tx.output.truncate(1))),
        (
            "OP_RETURN with data",
            Box::new(|tx| tx.output[1].script_pubkey = ScriptBuf::new_op_return([1u8])),
        ),
        (
            "OP_RETURN is a KSN1 output",
            Box::new(|tx| tx.output[1].script_pubkey = data_script(&DEVNET.magic, b"x").unwrap()),
        ),
        (
            "OP_RETURN amount is not 0",
            Box::new(|tx| {
                tx.output[1].value = Amount::from_sat(1);
            }),
        ),
        (
            "one output too many",
            Box::new(|tx| tx.output.push(txout(0, empty_op_return()))),
        ),
        ("outputs swapped", Box::new(|tx| tx.output.swap(0, 1))),
        (
            "spends output 0 of the commit transaction",
            Box::new(|tx| tx.input[0].previous_output.vout = 0),
        ),
        (
            "spends another transaction",
            Box::new(|tx| tx.input[0].previous_output = outpoint("elsewhere", 1)),
        ),
        (
            "two inputs",
            Box::new(|tx| {
                let mut extra = tx.input[0].clone();
                extra.previous_output = outpoint("extra", 0);
                tx.input.push(extra);
            }),
        ),
        (
            "input with a scriptSig",
            Box::new(|tx| tx.input[0].script_sig = ScriptBuf::from_bytes(vec![0x00])),
        ),
    ];
    for (name, f) in violations {
        let pair = tamper_reveal(&s, f);
        assert!(is_invalid(attached(&s, &pair)), "{name}");
    }
}

#[test]
fn envelope_rules() {
    let s = settle(3, 2);
    let key = s.key.x_only_public_key().0;
    let leaf = envelope_script(&key, &DEVNET.magic, &s.payload).unwrap();

    // The key in the leaf may be any key: the commitment, control block and signature only have to agree
    let stranger = Keypair::from_secret_key(&Secp256k1::new(), &secret("another envelope key"));
    let other_leaf =
        envelope_script(&stranger.x_only_public_key().0, &DEVNET.magic, &s.payload).unwrap();
    attached(&s, &with_leaf(&s, other_leaf.clone(), &stranger, None)).unwrap();
    // But the signature must come from the key in the leaf
    assert!(invalid_with(
        attached(&s, &with_leaf(&s, other_leaf, &s.key, None)),
        "signature verification failed"
    ));

    // A non-canonical leaf (the only chunk written with OP_PUSHDATA4 instead; commitment and signature both agree)
    let (parsed_key, payload) = parse_envelope(&DEVNET.magic, &leaf).unwrap();
    assert_eq!(parsed_key, key.serialize());
    assert!(payload.len() <= 520);
    let mut bytes = leaf.to_bytes();
    bytes.truncate(1 + 32 + 3 + 5);
    bytes.push(0x4e);
    bytes.extend(u32::try_from(payload.len()).unwrap().to_le_bytes());
    bytes.extend(&payload);
    bytes.push(0x68);
    let noncanonical = ScriptBuf::from_bytes(bytes);
    assert_eq!(
        parse_envelope(&DEVNET.magic, &noncanonical),
        Some((key.serialize(), s.payload.clone()))
    );
    let result = attached(&s, &with_leaf(&s, noncanonical, &s.key, None));
    assert!(
        invalid_with(result.clone(), "canonical construction"),
        "{result:?}"
    );

    // An envelope with a different payload
    let other_payload = settlement_payload(3, 7);
    let other = envelope_script(&key, &DEVNET.magic, &other_payload).unwrap();
    assert!(invalid_with(
        attached(&s, &with_leaf(&s, other, &s.key, None)),
        "the payload in the envelope does not match the block contents"
    ));

    // Control block: wrong parity, internal key is not the key in the leaf, an extra Merkle path element
    let good = commitment(&key, &leaf).unwrap().control_block;
    let mut parity = good.clone();
    parity[0] ^= 1;
    let mut internal = good.clone();
    internal[1..].copy_from_slice(&stranger.x_only_public_key().0.serialize());
    let mut path = good.clone();
    path.extend([0u8; 32]);
    for (name, control) in [
        ("parity", parity),
        ("internal key", internal),
        ("Merkle path", path),
    ] {
        assert!(
            invalid_with(
                attached(&s, &with_leaf(&s, leaf.clone(), &s.key, Some(control))),
                "control block"
            ),
            "{name}"
        );
    }

    // Witness shape: one element too few, an extra annex, the leaf replaced by an OP_RETURN data script
    let mut short = s.pair.clone();
    let elements = short.reveal.input[0].witness.to_vec();
    short.reveal.input[0].witness = Witness::from_slice(&elements[1..]);
    assert!(is_invalid(attached(&s, &short)));
    let mut annexed = s.pair.clone();
    let mut with_annex = elements.clone();
    with_annex.push(vec![0x50]);
    annexed.reveal.input[0].witness = Witness::from_slice(&with_annex);
    assert!(is_invalid(attached(&s, &annexed)));
    let data = data_script(&DEVNET.magic, &s.payload).unwrap();
    assert!(invalid_with(
        attached(&s, &with_leaf(&s, data, &s.key, None)),
        "KSN1 envelope"
    ));
}

#[test]
fn reveal_signature_rules() {
    let s = settle(3, 2);
    let committed = s.pair.commit.output[1].clone();
    let resigned = |ty: TapSighashType| {
        let mut pair = s.pair.clone();
        resign_reveal(&mut pair.reveal, &committed, &s.key, ty);
        pair
    };
    // An explicit SIGHASH_ALL (65 bytes, last byte 0x01) is accepted as well
    let all = resigned(TapSighashType::All);
    assert_eq!(all.reveal.input[0].witness.nth(0).unwrap().len(), 65);
    attached(&s, &all).unwrap();
    for ty in [
        TapSighashType::Single,
        TapSighashType::None,
        TapSighashType::AllPlusAnyoneCanPay,
        TapSighashType::NonePlusAnyoneCanPay,
        TapSighashType::SinglePlusAnyoneCanPay,
    ] {
        assert!(is_invalid(attached(&s, &resigned(ty))), "{ty:?}");
    }
    // Tampered signature, 65 bytes but sighash type 0x00, empty signature
    let mut elements = s.pair.reveal.input[0].witness.to_vec();
    let good = elements[0].clone();
    for bad in [
        {
            let mut flipped = good.clone();
            flipped[10] ^= 1;
            flipped
        },
        [good.clone(), vec![0x00]].concat(),
        vec![],
    ] {
        elements[0] = bad;
        let mut pair = s.pair.clone();
        pair.reveal.input[0].witness = Witness::from_slice(&elements);
        assert!(is_invalid(attached(&s, &pair)));
    }
    // The commitment output's amount differs from the one signed: the signature is invalid (the amount is in the
    // sighash)
    let mut pair = s.pair.clone();
    pair.commit.output[1].value += Amount::from_sat(1);
    pair.commit.output[0].value -= Amount::from_sat(1);
    resign_producer(
        &mut pair.commit,
        &s.prevouts,
        0,
        &s.key,
        TapSighashType::Default,
    );
    pair.reveal.input[0].previous_output.txid = pair.commit.compute_txid();
    assert!(invalid_with(
        attached(&s, &pair),
        "signature verification failed"
    ));
}

#[test]
fn producer_signature_rules() {
    let s = settle(3, 2);
    let resigned = |ty: TapSighashType| {
        let mut pair = s.pair.clone();
        resign_producer(&mut pair.commit, &s.prevouts, 0, &s.key, ty);
        // The commit transaction's ID does not change with the witness; the reveal transaction needs no change
        pair
    };
    let all = resigned(TapSighashType::All);
    assert_eq!(all.commit.input[0].witness.nth(0).unwrap().len(), 65);
    attached(&s, &all).unwrap();
    for ty in [
        TapSighashType::Single,
        TapSighashType::None,
        TapSighashType::AllPlusAnyoneCanPay,
        TapSighashType::NonePlusAnyoneCanPay,
        TapSighashType::SinglePlusAnyoneCanPay,
    ] {
        assert!(is_invalid(attached(&s, &resigned(ty))), "{ty:?}");
    }

    let commit_with = |f: &dyn Fn(&mut Transaction)| {
        let mut pair = s.pair.clone();
        f(&mut pair.commit);
        pair
    };
    // Tampered signature
    let good = s.pair.commit.input[0].witness.nth(0).unwrap().to_vec();
    let flipped = commit_with(&|tx| {
        let mut sig = tx.input[1].witness.nth(0).unwrap().to_vec();
        sig[10] ^= 1;
        tx.input[1].witness = Witness::from_slice(&[sig]);
    });
    assert!(is_invalid(attached(&s, &flipped)));
    // 65 bytes but sighash type 0x00; the witness has an extra element (annex) or is empty
    let zero_type = commit_with(&|tx| {
        tx.input[0].witness = Witness::from_slice(&[[good.clone(), vec![0x00]].concat()]);
    });
    assert!(is_invalid(attached(&s, &zero_type)));
    let annex = commit_with(&|tx| {
        tx.input[0].witness = Witness::from_slice(&[good.clone(), vec![0x50, 0x01]]);
    });
    assert!(is_invalid(attached(&s, &annex)));
    let empty = commit_with(&|tx| tx.input[0].witness = Witness::new());
    assert!(is_invalid(attached(&s, &empty)));
    // Signed with another key
    let stranger = Keypair::from_secret_key(&Secp256k1::new(), &secret("stranger"));
    let mut other = s.pair.clone();
    resign_producer(
        &mut other.commit,
        &s.prevouts,
        0,
        &stranger,
        TapSighashType::Default,
    );
    assert!(is_invalid(attached(&s, &other)));
}

/// Append a P2WPKH producer input to the commit transaction (the other inputs are re-signed, the reveal transaction is
/// re-pointed); returns the settlement transactions and the new source.
fn with_p2wpkh_producer_input(
    s: &Settled,
    sighash_type: EcdsaSighashType,
) -> (SettlementPair, BTreeMap<OutPoint, TxOut>) {
    let key = secret("p2wpkh producer");
    let prevout = txout(30_000, p2wpkh_script(&key));
    let outpoint = outpoint("p2wpkh producer utxo", 0);
    let mut pair = s.pair.clone();
    let tx = &mut pair.commit;
    tx.input.push(TxIn {
        previous_output: outpoint,
        script_sig: ScriptBuf::new(),
        sequence: bitcoin::Sequence(PRODUCER_SEQUENCE),
        witness: Witness::new(),
    });
    tx.output[0].value += Amount::from_sat(30_000);
    let mut prevouts = s.prevouts.clone();
    prevouts.push(prevout.clone());
    let last = tx.input.len() - 1;
    // Sign the P2TR inputs first (up to the second-to-last), then the P2WPKH input
    let mut signed = tx.clone();
    resign_producer(&mut signed, &prevouts, 0, &s.key, TapSighashType::Default);
    for (txin, done) in tx.input.iter_mut().zip(&signed.input).take(last) {
        txin.witness = done.witness.clone();
    }
    let sig = p2wpkh_sig(&key, tx, last, &prevout, sighash_type);
    tx.input[last].witness = Witness::from_slice(&[sig, compressed_pubkey(&key)]);
    pair.reveal.input[0].previous_output.txid = pair.commit.compute_txid();
    let committed = pair.commit.output[1].clone();
    resign_reveal(
        &mut pair.reveal,
        &committed,
        &s.key,
        TapSighashType::Default,
    );
    let mut source = s.source();
    source.insert(outpoint, prevout);
    (pair, source)
}

#[test]
fn p2wpkh_producer_inputs_must_use_sighash_all() {
    let s = settle(3, 2);
    let (pair, source) = with_p2wpkh_producer_input(&s, EcdsaSighashType::All);
    validate_attached(&DEVNET, &pair, &s.payload, &source).unwrap();
    for ty in [
        EcdsaSighashType::Single,
        EcdsaSighashType::None,
        EcdsaSighashType::AllPlusAnyoneCanPay,
        EcdsaSighashType::SinglePlusAnyoneCanPay,
    ] {
        let (pair, source) = with_p2wpkh_producer_input(&s, ty);
        assert!(
            is_invalid(validate_attached(&DEVNET, &pair, &s.payload, &source)),
            "{ty:?}"
        );
    }
}

#[test]
fn producer_inputs_of_other_types_are_rejected() {
    let s = settle(3, 2);
    // The producer input's prevout is P2PKH (as given by the source): not accepted for a settlement carried by a block
    let mut source = s.source();
    source.insert(
        s.pair.commit.input[0].previous_output,
        txout(
            40_000,
            ScriptBuf::new_p2pkh(&bitcoin::PubkeyHash::from_byte_array([1; 20])),
        ),
    );
    assert!(is_invalid(validate_attached(
        &DEVNET, &s.pair, &s.payload, &source
    )));
}

struct Missing<'a> {
    inner: &'a BTreeMap<OutPoint, TxOut>,
    missing: OutPoint,
}

impl PrevoutSource for Missing<'_> {
    fn prevout(&self, outpoint: &OutPoint) -> Result<Option<TxOut>, String> {
        if *outpoint == self.missing {
            Ok(None)
        } else {
            self.inner.prevout(outpoint)
        }
    }
}

#[test]
fn prevout_source_results() {
    let s = settle(3, 2);
    let full = s.source();
    // A producer input not confirmed before the anchor height
    for index in [0, 1] {
        let source = Missing {
            inner: &full,
            missing: s.pair.commit.input[index].previous_output,
        };
        match validate_attached(&DEVNET, &s.pair, &s.payload, &source) {
            Err(SettlementError::Invalid(msg)) => {
                assert!(
                    msg.contains("not confirmed before the anchor height"),
                    "{msg}"
                )
            }
            other => panic!("input {index}: {other:?}"),
        }
    }
    // A source failure is a local fault, not invalidity
    assert!(matches!(
        validate_attached(&DEVNET, &s.pair, &s.payload, &FailingSource),
        Err(SettlementError::Fatal(_))
    ));
    // Structural and envelope errors are judged invalid before the source is queried
    let mut bad = s.pair.clone();
    bad.commit.version = transaction::Version::ONE;
    assert!(is_invalid(validate_attached(
        &DEVNET,
        &bad,
        &s.payload,
        &FailingSource
    )));
    let other = settlement_payload(3, 1);
    assert!(is_invalid(validate_attached(
        &DEVNET,
        &s.pair,
        &other,
        &FailingSource
    )));
    // The prevout amount given by the source differs from the one signed: the signature is invalid
    let mut wrong_value = full.clone();
    let producer = s.pair.commit.input[1].previous_output;
    let mut out = wrong_value[&producer].clone();
    out.value += Amount::from_sat(1);
    wrong_value.insert(producer, out);
    assert!(is_invalid(validate_attached(
        &DEVNET,
        &s.pair,
        &s.payload,
        &wrong_value
    )));
}

/// A CPFP child (D-102): spends the reveal's P2A output and a funding UTXO, pays the package up to the requested fee
/// rate (and at least the minimum given), signs the funding input; `None` when the UTXO cannot pay it.
#[test]
fn a_cpfp_child_pays_for_the_pair_through_the_p2a_output() {
    let s = settle(3, 2);
    let checked = attached(&s, &s.pair).unwrap();
    let utxo = producer_utxos(&s.key, &[50_000]).remove(0);
    let (child, fee) = cpfp_child(
        &DEVNET,
        &s.pair.reveal,
        checked.fee,
        checked.vsize,
        &utxo,
        20,
        0,
        &s.key,
    )
    .unwrap()
    .unwrap();
    let vsize = child.weight().to_vbytes_ceil();
    // the package (pair and child) pays 20 sat/vB exactly
    assert_eq!(fee, 20 * (checked.vsize + vsize) - checked.fee);
    assert_eq!(child.input.len(), 2);
    assert_eq!(
        child.input[0].previous_output,
        OutPoint {
            txid: s.pair.reveal.compute_txid(),
            vout: 0
        }
    );
    assert!(child.input[0].witness.is_empty());
    assert_eq!(child.input[1].previous_output, utxo.outpoint);
    assert!(child
        .input
        .iter()
        .all(|i| i.sequence.0 == PRODUCER_SEQUENCE));
    assert_eq!(child.output.len(), 1);
    assert_eq!(child.output[0].script_pubkey, utxo.txout.script_pubkey);
    assert_eq!(
        child.output[0].value.to_sat(),
        50_000 + P2A_AMOUNT_SAT - fee
    );
    // the funding input's signature verifies against both previous outputs
    let prevouts = [s.pair.reveal.output[0].clone(), utxo.txout.clone()];
    let (sig, sighash_type) = taproot_signature(child.input[1].witness.nth(0).unwrap()).unwrap();
    verify_taproot_key_spend(
        &child,
        1,
        &bitcoin::sighash::Prevouts::All(prevouts.as_slice()),
        &utxo.txout.script_pubkey,
        &sig,
        sighash_type,
    )
    .unwrap();

    // a pair already paying more needs only the child's own minimum, or the minimum fee given (a replacement)
    let (_, fee) = cpfp_child(
        &DEVNET,
        &s.pair.reveal,
        checked.fee,
        checked.vsize,
        &utxo,
        1,
        0,
        &s.key,
    )
    .unwrap()
    .unwrap();
    assert_eq!(fee, vsize * DEVNET.min_settlement_feerate_sat_vb);
    let (_, fee) = cpfp_child(
        &DEVNET,
        &s.pair.reveal,
        checked.fee,
        checked.vsize,
        &utxo,
        1,
        7_777,
        &s.key,
    )
    .unwrap()
    .unwrap();
    assert_eq!(fee, 7_777);
    // a UTXO too small for the fee and a change above dust
    let small = producer_utxos(&s.key, &[1_000]).remove(0);
    assert_eq!(
        cpfp_child(
            &DEVNET,
            &s.pair.reveal,
            checked.fee,
            checked.vsize,
            &small,
            50,
            0,
            &s.key
        )
        .unwrap(),
        None
    );
    // only a UTXO of the funding key, only a reveal with the P2A output
    let other = Keypair::from_secret_key(&Secp256k1::new(), &secret("someone else"));
    assert!(is_invalid(cpfp_child(
        &DEVNET,
        &s.pair.reveal,
        checked.fee,
        checked.vsize,
        &utxo,
        20,
        0,
        &other
    )));
    let mut reveal = s.pair.reveal.clone();
    reveal.output.swap(0, 1);
    assert!(is_invalid(cpfp_child(
        &DEVNET,
        &reveal,
        checked.fee,
        checked.vsize,
        &utxo,
        20,
        0,
        &s.key
    )));
}
