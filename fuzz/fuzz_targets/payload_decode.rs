//! Settlement payload decoding (SPEC §15): arbitrary bytes as a `Payload` encoding, as a KSN1 data script, as an
//! envelope leaf (D-081), and as a payload placed in a transaction.
//!
//! Property: no crash; when a `Payload` decodes successfully its encoding is canonical; the scripts written by
//! `data_script` / `envelope_script` can always be recovered by the corresponding parser; after placing the payload
//! in a transaction, `tx_payload` agrees with a direct `Payload::decode`, and recognizes it only when the carrier
//! matches the payload kind (an OP_RETURN output carries only native transaction payloads, an envelope only
//! settlement and republish payloads).

#![no_main]

use bitcoin::absolute::LockTime;
use bitcoin::key::{Keypair, Secp256k1};
use bitcoin::script::ScriptBuf;
use bitcoin::transaction::Version;
use bitcoin::{Amount, OutPoint, Sequence, Transaction, TxIn, TxOut, Witness};
use ksana_btc::data::{data_script, parse_data_script, tx_data, tx_payload, Carrier, TxData};
use ksana_btc::envelope::{envelope_script, parse_envelope};
use ksana_btc::payload::envelope_payload;
use ksana_primitives::{Payload, DEVNET};
use libfuzzer_sys::fuzz_target;

fn tx_with(inputs: Vec<Witness>, outputs: Vec<ScriptBuf>) -> Transaction {
    Transaction {
        version: Version::TWO,
        lock_time: LockTime::ZERO,
        input: inputs
            .into_iter()
            .map(|witness| TxIn {
                previous_output: OutPoint::null(),
                script_sig: ScriptBuf::new(),
                sequence: Sequence::MAX,
                witness,
            })
            .collect(),
        output: outputs
            .into_iter()
            .map(|script_pubkey| TxOut {
                value: Amount::ZERO,
                script_pubkey,
            })
            .collect(),
    }
}

/// The witness is `[placeholder signature, leaf, placeholder control block]`.
fn envelope_witness(leaf: &ScriptBuf) -> Witness {
    Witness::from_slice(&[vec![0u8; 64], leaf.to_bytes(), vec![0xc0; 33]])
}

fuzz_target!(|data: &[u8]| {
    let magic = &DEVNET.magic;
    let key = Keypair::from_seckey_slice(&Secp256k1::new(), &[0x42; 32])
        .expect("the fixed secret key is valid")
        .x_only_public_key()
        .0;

    // 1. As a payload encoding (compact headers and entries, D-075, D-079, D-080).
    let decoded = Payload::decode(data, &DEVNET);
    if let Ok(payload) = &decoded {
        assert_eq!(
            payload
                .encode(&DEVNET)
                .expect("a successfully decoded payload must encode"),
            data,
            "payload encoding is not canonical"
        );
    }

    // 2. As an arbitrary locking script / leaf script: parsing does not crash; when it parses, rewriting it the
    //    standard way yields the same payload.
    let script = ScriptBuf::from_bytes(data.to_vec());
    if let Some(parsed) = parse_data_script(magic, &script) {
        if !parsed.is_empty() {
            let rewritten = data_script(magic, &parsed)
                .expect("a non-empty payload must be writable as a data script");
            assert_eq!(parse_data_script(magic, &rewritten), Some(parsed));
        }
    }
    if let Some((_, parsed)) = parse_envelope(magic, &script) {
        if !parsed.is_empty() {
            let rewritten = envelope_script(&key, magic, &parsed)
                .expect("a non-empty payload must be writable as an envelope");
            assert_eq!(
                parse_envelope(magic, &rewritten),
                Some((key.serialize(), parsed))
            );
        }
    }
    let _ = tx_data(
        magic,
        &tx_with(vec![], vec![script.clone(), script.clone()]),
    );
    let _ = tx_data(
        magic,
        &tx_with(vec![envelope_witness(&script)], vec![script.clone()]),
    );

    if data.is_empty() {
        return;
    }
    let kind_native = matches!(decoded, Ok(Payload::NativeOp { .. }));

    // 3. As a payload placed in a KSN1 output: tx_data recovers the original bytes; only native transaction payloads
    //    are recognized (D-032, D-081).
    let out_tx = tx_with(
        vec![],
        vec![
            ScriptBuf::new(),
            data_script(magic, data)
                .expect("a non-empty payload must be writable as a data script"),
        ],
    );
    assert_eq!(
        tx_data(magic, &out_tx),
        TxData::One {
            carrier: Carrier::Output(1),
            payload: data.to_vec()
        }
    );
    assert_eq!(
        tx_payload(&DEVNET, &out_tx).map(|(_, p)| p),
        decoded.clone().ok().filter(|_| kind_native),
        "payload in OP_RETURN does not agree with direct decoding"
    );

    // 4. As a payload placed in an envelope: tx_data recovers the original bytes; only settlement and republish
    //    payloads are recognized.
    let leaf = envelope_script(&key, magic, data)
        .expect("a non-empty payload must be writable as an envelope");
    let env_tx = tx_with(vec![Witness::new(), envelope_witness(&leaf)], vec![]);
    assert_eq!(
        tx_data(magic, &env_tx),
        TxData::One {
            carrier: Carrier::Input(1),
            payload: data.to_vec()
        }
    );
    assert_eq!(envelope_payload(&DEVNET, &env_tx), Some(data.to_vec()));
    assert_eq!(
        tx_payload(&DEVNET, &env_tx).map(|(_, p)| p),
        decoded.ok().filter(|_| !kind_native),
        "payload in the envelope does not agree with direct decoding"
    );
});
