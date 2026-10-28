//! KSN1 envelope (D-081): construction and parsing of the leaf script, the recognition prefix and format, the BIP341
//! annex rule, the commitment output and control block, and the rules that OP_RETURN outputs and envelopes in a
//! transaction together are "exactly one" and that the carrier matches the payload kind.

mod common;

use bitcoin::key::{Secp256k1, XOnlyPublicKey};
use bitcoin::opcodes::all::{
    OP_CHECKSIG, OP_ENDIF, OP_IF, OP_PUSHBYTES_32, OP_PUSHDATA1, OP_PUSHDATA2, OP_PUSHDATA4,
    OP_PUSHNUM_1,
};
use bitcoin::taproot::{LeafVersion, TapLeafHash, TapNodeHash};
use bitcoin::{absolute, transaction, ScriptBuf, Sequence, Transaction, TxIn, TxOut, Witness};
use common::*;
use ksana_btc::data::{data_script, tx_data, tx_payload, Carrier, TxData, MAX_CHUNK};
use ksana_btc::envelope::{
    commitment, envelope_script, is_ksn1_envelope, parse_envelope, witness_leaf,
};
use ksana_primitives::{Action, FeeMode, Payload, SigScheme, VmOp, DEVNET};
use proptest::prelude::*;

const MAGIC: [u8; 4] = *b"KSN1";

fn key() -> XOnlyPublicKey {
    producer_key().x_only_public_key().0
}

/// Envelope prefix: `OP_PUSHBYTES_32 <key> OP_CHECKSIG OP_FALSE OP_IF`, followed by `rest` (hand-written bytes).
fn leaf_bytes(rest: &[u8]) -> ScriptBuf {
    let mut bytes = vec![OP_PUSHBYTES_32.to_u8()];
    bytes.extend(key().serialize());
    bytes.extend([OP_CHECKSIG.to_u8(), 0x00, OP_IF.to_u8()]);
    bytes.extend_from_slice(rest);
    ScriptBuf::from_bytes(bytes)
}

/// `magic` written with OP_PUSHBYTES_4, followed by `rest`.
fn magic_then(rest: &[u8]) -> Vec<u8> {
    let mut bytes = vec![0x04];
    bytes.extend(MAGIC);
    bytes.extend_from_slice(rest);
    bytes
}

const ENDIF: u8 = OP_ENDIF.to_u8();

fn payload_of_len(len: usize) -> Vec<u8> {
    (0..len)
        .map(|i| u8::try_from((i * 17 + 3) % 253).unwrap())
        .collect()
}

/// An input whose witness is `[64-byte signature, leaf, control block]` (syntax only; signature and control block are
/// placeholders).
fn envelope_input(leaf: &ScriptBuf) -> TxIn {
    TxIn {
        previous_output: outpoint("envelope", 0),
        script_sig: ScriptBuf::new(),
        sequence: Sequence::MAX,
        witness: Witness::from_slice(&[vec![1u8; 64], leaf.to_bytes(), vec![0xc0; 33]]),
    }
}

fn key_path_input(tag: &str) -> TxIn {
    TxIn {
        previous_output: outpoint(tag, 0),
        script_sig: ScriptBuf::new(),
        sequence: Sequence::MAX,
        witness: Witness::from_slice(&[vec![2u8; 64]]),
    }
}

fn tx(input: Vec<TxIn>, output: Vec<TxOut>) -> Transaction {
    Transaction {
        version: transaction::Version::TWO,
        lock_time: absolute::LockTime::ZERO,
        input,
        output,
    }
}

fn p2a_out() -> TxOut {
    txout(240, ScriptBuf::new_p2a())
}

fn native_op() -> VmOp {
    VmOp {
        version: 2,
        chain_id: DEVNET.chain_id,
        account: [3; 32],
        nonce: 0,
        fee: FeeMode::BtcNative,
        max_gas: 50_000,
        action: Action::AssetTransfer {
            asset: [0; 32],
            to: [4; 32],
            amount: 1,
        },
        sig: SigScheme::Bip340([5; 64]),
    }
}

#[test]
fn envelope_roundtrips_across_chunk_boundaries() {
    for len in [
        1,
        75,
        76,
        255,
        256,
        MAX_CHUNK - 1,
        MAX_CHUNK,
        MAX_CHUNK + 1,
        3 * MAX_CHUNK,
        DEVNET.max_block_data_bytes,
    ] {
        let payload = payload_of_len(len);
        let script = envelope_script(&key(), &MAGIC, &payload).unwrap();
        assert!(is_ksn1_envelope(&MAGIC, &script), "{len}");
        assert_eq!(
            parse_envelope(&MAGIC, &script),
            Some((key().serialize(), payload.clone())),
            "{len}"
        );
        // The script length depends only on the payload length: a different key leaves the length unchanged
        let other =
            envelope_script(&secret_key_xonly("another envelope key"), &MAGIC, &payload).unwrap();
        assert_eq!(other.len(), script.len(), "{len}");
        // Each chunk is at most 520 bytes (pushes in the unexecuted branch are still bound by this limit)
        for instruction in script.instructions() {
            if let Ok(bitcoin::script::Instruction::PushBytes(push)) = instruction {
                assert!(push.len() <= MAX_CHUNK);
            }
        }
    }
}

fn secret_key_xonly(tag: &str) -> XOnlyPublicKey {
    bitcoin::key::Keypair::from_secret_key(&Secp256k1::new(), &secret(tag))
        .x_only_public_key()
        .0
}

#[test]
fn push_encodings_and_malformed_envelopes() {
    // Data pushes accept non-minimal encodings (same as the OP_RETURN data script, D-032)
    let pushdata1 = leaf_bytes(&magic_then(&[OP_PUSHDATA1.to_u8(), 2, 7, 8, ENDIF]));
    let pushdata2 = leaf_bytes(&magic_then(&[OP_PUSHDATA2.to_u8(), 2, 0, 7, 8, ENDIF]));
    let pushdata4 = leaf_bytes(&magic_then(&[
        OP_PUSHDATA4.to_u8(),
        2,
        0,
        0,
        0,
        7,
        8,
        ENDIF,
    ]));
    for script in [&pushdata1, &pushdata2, &pushdata4] {
        assert_eq!(
            parse_envelope(&MAGIC, script),
            Some((key().serialize(), vec![7, 8]))
        );
    }
    // The magic itself may also use another push encoding
    let mut magic_pushdata1 = vec![OP_PUSHDATA1.to_u8(), 4];
    magic_pushdata1.extend(MAGIC);
    magic_pushdata1.extend([0x01, 9, ENDIF]);
    assert_eq!(
        parse_envelope(&MAGIC, &leaf_bytes(&magic_pushdata1)),
        Some((key().serialize(), vec![9]))
    );
    // No chunks: well-formed, empty payload (rejected when the payload is decoded)
    assert_eq!(
        parse_envelope(&MAGIC, &leaf_bytes(&magic_then(&[ENDIF]))),
        Some((key().serialize(), vec![]))
    );

    // An envelope (correct prefix) but malformed
    let mut too_long = vec![OP_PUSHDATA2.to_u8()];
    too_long.extend(u16::try_from(MAX_CHUNK + 1).unwrap().to_le_bytes());
    too_long.extend(vec![0u8; MAX_CHUNK + 1]);
    too_long.push(ENDIF);
    let malformed = [
        ("missing OP_ENDIF", magic_then(&[0x01, 9])),
        ("bytes after OP_ENDIF", magic_then(&[0x01, 9, ENDIF, 0x00])),
        (
            "other opcode among the chunks",
            magic_then(&[OP_PUSHNUM_1.to_u8(), ENDIF]),
        ),
        ("truncated push", magic_then(&[0x05, 1, 2])),
        ("chunk over 520 bytes", magic_then(&too_long)),
    ];
    for (name, rest) in malformed {
        let script = leaf_bytes(&rest);
        assert!(is_ksn1_envelope(&MAGIC, &script), "{name}");
        assert_eq!(parse_envelope(&MAGIC, &script), None, "{name}");
    }

    // Wrong prefix: not an envelope
    let mut wrong_magic = vec![0x04];
    wrong_magic.extend(*b"KSN2");
    wrong_magic.extend([0x01, 9, ENDIF]);
    let mut pushdata1_key = vec![OP_PUSHDATA1.to_u8(), 32];
    pushdata1_key.extend(key().serialize());
    pushdata1_key.extend([OP_CHECKSIG.to_u8(), 0x00, OP_IF.to_u8()]);
    pushdata1_key.extend(magic_then(&[0x01, 9, ENDIF]));
    let mut no_false = vec![OP_PUSHBYTES_32.to_u8()];
    no_false.extend(key().serialize());
    no_false.extend([OP_CHECKSIG.to_u8(), OP_IF.to_u8()]);
    no_false.extend(magic_then(&[0x01, 9, ENDIF]));
    for (name, script) in [
        ("different magic", leaf_bytes(&wrong_magic)),
        (
            "public key not pushed with OP_PUSHBYTES_32",
            ScriptBuf::from_bytes(pushdata1_key),
        ),
        ("missing OP_FALSE", ScriptBuf::from_bytes(no_false)),
        ("empty script", ScriptBuf::new()),
        ("OP_RETURN data script", data_script(&MAGIC, b"x").unwrap()),
    ] {
        assert!(!is_ksn1_envelope(&MAGIC, &script), "{name}");
        assert_eq!(parse_envelope(&MAGIC, &script), None, "{name}");
    }
}

#[test]
fn witness_leaf_follows_bip341() {
    let leaf = envelope_script(&key(), &MAGIC, b"abc").unwrap();
    let base = envelope_input(&leaf);
    assert_eq!(witness_leaf(&base.witness), Some(leaf.as_script()));
    // annex: the last element starts with 0x50; once it is removed the second-to-last element is still the leaf
    let mut with_annex = base.witness.to_vec();
    with_annex.push(vec![0x50, 1]);
    assert_eq!(
        witness_leaf(&Witness::from_slice(&with_annex)),
        Some(leaf.as_script())
    );
    // A single element (key-path) or no elements: no leaf
    assert_eq!(witness_leaf(&Witness::from_slice(&[leaf.to_bytes()])), None);
    assert_eq!(witness_leaf(&Witness::new()), None);
}

#[test]
fn commitment_matches_bip341() {
    let leaf = envelope_script(&key(), &MAGIC, &payload_of_len(1_000)).unwrap();
    let c = commitment(&key(), &leaf).unwrap();
    // With a single leaf the Merkle root is the leaf hash
    let leaf_hash = TapLeafHash::from_script(&leaf, LeafVersion::TapScript);
    assert_eq!(c.leaf_hash, leaf_hash);
    let root = TapNodeHash::from(leaf_hash);
    let expected = ScriptBuf::new_p2tr(&Secp256k1::verification_only(), key(), Some(root));
    assert_eq!(c.script_pubkey, expected);
    // Control block: 33 bytes, leaf version 0xc0 plus the parity of the output key, then the internal key
    assert_eq!(c.control_block.len(), 33);
    assert_eq!(c.control_block[0] & 0xfe, 0xc0);
    assert_eq!(&c.control_block[1..], &key().serialize());
    // The commitment depends on the payload
    let other = envelope_script(&key(), &MAGIC, &payload_of_len(999)).unwrap();
    assert_ne!(
        commitment(&key(), &other).unwrap().script_pubkey,
        c.script_pubkey
    );
}

#[test]
fn carriers_are_counted_together() {
    let leaf = envelope_script(&key(), &MAGIC, b"abc").unwrap();
    let ksn1_out = txout(0, data_script(&MAGIC, b"xyz").unwrap());

    // The envelope is in input 1
    let one = tx(
        vec![key_path_input("a"), envelope_input(&leaf)],
        vec![p2a_out()],
    );
    assert_eq!(
        tx_data(&MAGIC, &one),
        TxData::One {
            carrier: Carrier::Input(1),
            payload: b"abc".to_vec()
        }
    );
    // Two envelopes, or one envelope plus one KSN1 output: neither is recognized
    let two = tx(
        vec![envelope_input(&leaf), envelope_input(&leaf)],
        vec![p2a_out()],
    );
    assert_eq!(tx_data(&MAGIC, &two), TxData::Invalid);
    let mixed = tx(vec![envelope_input(&leaf)], vec![ksn1_out.clone()]);
    assert_eq!(tx_data(&MAGIC, &mixed), TxData::Invalid);
    // The only envelope is malformed
    let broken = tx(
        vec![envelope_input(&leaf_bytes(&magic_then(&[0x01, 9])))],
        vec![p2a_out()],
    );
    assert_eq!(tx_data(&MAGIC, &broken), TxData::Invalid);
    // A leaf before an annex is recognized as well; the same bytes in a key-path witness are not a leaf
    let mut annexed = envelope_input(&leaf);
    let mut elements = annexed.witness.to_vec();
    elements.push(vec![0x50]);
    annexed.witness = Witness::from_slice(&elements);
    assert!(matches!(
        tx_data(&MAGIC, &tx(vec![annexed], vec![p2a_out()])),
        TxData::One {
            carrier: Carrier::Input(0),
            ..
        }
    ));
    let mut single = key_path_input("b");
    single.witness = Witness::from_slice(&[leaf.to_bytes()]);
    assert_eq!(
        tx_data(&MAGIC, &tx(vec![single], vec![p2a_out()])),
        TxData::None
    );
}

#[test]
fn payload_kind_must_match_the_carrier() {
    let settlement = Payload::decode(&settlement_payload(2, 9), &DEVNET).unwrap();
    let settlement_bytes = settlement.encode(&DEVNET).unwrap();
    let native = Payload::NativeOp { op: native_op() };
    let native_bytes = native.encode(&DEVNET).unwrap();

    let in_envelope = |bytes: &[u8]| {
        let leaf = envelope_script(&key(), &MAGIC, bytes).unwrap();
        tx(vec![envelope_input(&leaf)], vec![p2a_out()])
    };
    let in_output = |bytes: &[u8]| {
        tx(
            vec![key_path_input("c")],
            vec![txout(0, data_script(&MAGIC, bytes).unwrap())],
        )
    };
    // Settlement and republish payloads are recognized only in an envelope, native-transaction payloads only in an
    // OP_RETURN output
    assert_eq!(
        tx_payload(&DEVNET, &in_envelope(&settlement_bytes)),
        Some((Carrier::Input(0), settlement.clone()))
    );
    assert_eq!(tx_payload(&DEVNET, &in_output(&settlement_bytes)), None);
    assert_eq!(
        tx_payload(&DEVNET, &in_output(&native_bytes)),
        Some((Carrier::Output(0), native.clone()))
    );
    assert_eq!(tx_payload(&DEVNET, &in_envelope(&native_bytes)), None);
    let Payload::Settlement { headers, .. } = &settlement else {
        panic!("settlement payload");
    };
    let headers = Payload::Headers {
        headers: headers.clone(),
        bodies: vec![],
        payee: [0; 32],
    };
    let headers_bytes = headers.encode(&DEVNET).unwrap();
    assert_eq!(
        tx_payload(&DEVNET, &in_envelope(&headers_bytes)),
        Some((Carrier::Input(0), headers))
    );
    assert_eq!(tx_payload(&DEVNET, &in_output(&headers_bytes)), None);
    // A payload in an envelope is likewise checked against the length limit first and must decode fully
    let mut small = DEVNET.clone();
    small.max_block_data_bytes = settlement_bytes.len() - 1;
    assert_eq!(tx_payload(&small, &in_envelope(&settlement_bytes)), None);
    let mut trailing = settlement_bytes.clone();
    trailing.push(0);
    assert_eq!(tx_payload(&DEVNET, &in_envelope(&trailing)), None);
}

proptest! {
    #[test]
    fn roundtrip_random_payloads(payload in proptest::collection::vec(any::<u8>(), 1..4_000)) {
        let script = envelope_script(&key(), &MAGIC, &payload).unwrap();
        prop_assert_eq!(parse_envelope(&MAGIC, &script), Some((key().serialize(), payload.clone())));
        let t = tx(vec![key_path_input("d"), envelope_input(&script)], vec![p2a_out()]);
        prop_assert_eq!(tx_data(&MAGIC, &t), TxData::One { carrier: Carrier::Input(1), payload });
    }

    /// No byte string makes parsing panic; when it parses it is an envelope, and rebuilt canonically the payload is the
    /// same.
    #[test]
    fn parsing_arbitrary_leaves_is_total(rest in proptest::collection::vec(any::<u8>(), 0..600)) {
        let raw = ScriptBuf::from_bytes(rest.clone());
        if parse_envelope(&MAGIC, &raw).is_some() {
            prop_assert!(is_ksn1_envelope(&MAGIC, &raw));
        }
        let prefixed = leaf_bytes(&magic_then(&rest));
        prop_assert!(is_ksn1_envelope(&MAGIC, &prefixed));
        if let Some((found, payload)) = parse_envelope(&MAGIC, &prefixed) {
            prop_assert_eq!(found, key().serialize());
            if !payload.is_empty() {
                let rebuilt = envelope_script(&key(), &MAGIC, &payload).unwrap();
                prop_assert_eq!(parse_envelope(&MAGIC, &rebuilt), Some((found, payload)));
            }
        }
    }
}
