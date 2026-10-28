//! KSN1 data output (D-032): construction / parsing round trip, push encoding, malformed scripts, the number of KSN1
//! outputs in a transaction, the payload length limit and full decoding.
//! For the envelope in the witness and the "exactly one in total" rule over the two carriers see `envelope.rs` (D-081).

use bitcoin::opcodes::all::{
    OP_CHECKSIG, OP_PUSHBYTES_0, OP_PUSHBYTES_1, OP_PUSHDATA1, OP_PUSHDATA2, OP_PUSHDATA4,
    OP_PUSHNUM_1, OP_PUSHNUM_16, OP_PUSHNUM_NEG1, OP_RETURN,
};
use bitcoin::script::Instruction;
use bitcoin::{absolute, transaction, Amount, ScriptBuf, Transaction, TxOut};
use ksana_btc::data::{
    data_script, is_ksn1_output, parse_data_script, tx_data, tx_payload, Carrier, DataError,
    TxData, MAX_CHUNK,
};
use ksana_primitives::{Action, FeeMode, Payload, SigScheme, VmOp, DEVNET};
use proptest::prelude::*;

const MAGIC: [u8; 4] = *b"KSN1";

fn payload_of_len(len: usize) -> Vec<u8> {
    (0..len)
        .map(|i| u8::try_from((i * 31 + 7) % 251).unwrap())
        .collect()
}

/// Hand-written script bytes: `OP_RETURN <magic>` followed by `rest`.
fn ksn1_bytes(rest: &[u8]) -> ScriptBuf {
    let mut bytes = vec![OP_RETURN.to_u8(), 0x04];
    bytes.extend_from_slice(&MAGIC);
    bytes.extend_from_slice(rest);
    ScriptBuf::from_bytes(bytes)
}

fn out(script: ScriptBuf) -> TxOut {
    TxOut {
        value: Amount::ZERO,
        script_pubkey: script,
    }
}

fn tx(outputs: Vec<TxOut>) -> Transaction {
    Transaction {
        version: transaction::Version::TWO,
        lock_time: absolute::LockTime::ZERO,
        input: vec![],
        output: outputs,
    }
}

fn p2tr_out() -> TxOut {
    TxOut {
        value: Amount::from_sat(10_000),
        script_pubkey: ScriptBuf::from_bytes([&[0x51, 0x20][..], &[0xab; 32]].concat()),
    }
}

fn native_op() -> VmOp {
    VmOp {
        version: 1,
        chain_id: DEVNET.chain_id,
        account: [3; 32],
        nonce: 0,
        fee: FeeMode::BtcNative,
        max_gas: 50_000,
        action: Action::AssetTransfer {
            asset: [0; 32],
            to: [4; 32],
            amount: 5,
        },
        sig: SigScheme::Bip340([6; 64]),
    }
}

#[test]
fn roundtrip_at_chunk_boundaries() {
    for len in [1, 519, 520, 521, 1040, 1041, 81_920] {
        let payload = payload_of_len(len);
        let script = data_script(&MAGIC, &payload).unwrap();
        assert!(is_ksn1_output(&MAGIC, &script), "len {len}");
        assert_eq!(
            parse_data_script(&MAGIC, &script).as_deref(),
            Some(&payload[..]),
            "len {len}"
        );
        // Data chunks: 520 bytes each, the last one may be shorter
        let chunks: Vec<usize> = script
            .instructions()
            .skip(2)
            .map(|i| match i.unwrap() {
                Instruction::PushBytes(p) => p.len(),
                Instruction::Op(op) => panic!("len {len}: unexpected opcode {op}"),
            })
            .collect();
        assert_eq!(chunks.len(), len.div_ceil(MAX_CHUNK), "len {len}");
        assert!(chunks.iter().all(|&c| c <= MAX_CHUNK));
        assert!(chunks[..chunks.len() - 1].iter().all(|&c| c == MAX_CHUNK));
        assert_eq!(chunks.iter().sum::<usize>(), len);
    }
}

#[test]
fn max_payload_script_size_matches_d010() {
    // Measured for D-010: a payload of exactly MAX_BLOCK_DATA_BYTES makes a script of about 82,400 bytes
    let script = data_script(&MAGIC, &payload_of_len(DEVNET.max_block_data_bytes)).unwrap();
    assert_eq!(script.len(), 1 + 5 + 157 * (3 + 520) + (3 + 280));
    assert_eq!(script.len(), 82_400);
}

#[test]
fn one_byte_last_chunk_is_written_as_pushbytes_1_and_parses() {
    for value in (0x01..=0x10).chain([0x81, 0x00, 0x4f]) {
        for prefix_len in [0, MAX_CHUNK, 2 * MAX_CHUNK] {
            let mut payload = payload_of_len(prefix_len);
            payload.push(value);
            let script = data_script(&MAGIC, &payload).unwrap();
            // The last two bytes are OP_PUSHBYTES_1 <value>, not OP_1..OP_16 / OP_1NEGATE
            let bytes = script.as_bytes();
            assert_eq!(
                &bytes[bytes.len() - 2..],
                &[OP_PUSHBYTES_1.to_u8(), value],
                "value {value:#04x}"
            );
            assert_eq!(parse_data_script(&MAGIC, &script), Some(payload));
        }
    }
}

#[test]
fn non_minimal_pushes_are_accepted() {
    let mut rest = vec![OP_PUSHDATA1.to_u8(), 3, 1, 2, 3];
    rest.extend_from_slice(&[OP_PUSHDATA2.to_u8(), 2, 0, 4, 5]);
    rest.extend_from_slice(&[OP_PUSHDATA4.to_u8(), 1, 0, 0, 0, 6]);
    rest.push(OP_PUSHBYTES_0.to_u8()); // empty chunk
    rest.extend_from_slice(&[OP_PUSHDATA1.to_u8(), 1, 0x05]); // 1-byte data with value 5, not minimally encoded
    rest.extend_from_slice(&[OP_PUSHDATA2.to_u8(), 0, 0]); // empty chunk, PUSHDATA2
    let script = ksn1_bytes(&rest);
    assert_eq!(
        parse_data_script(&MAGIC, &script),
        Some(vec![1, 2, 3, 4, 5, 6, 5])
    );

    // A 520-byte chunk written with PUSHDATA4 is accepted as well
    let chunk = payload_of_len(MAX_CHUNK);
    let mut rest = vec![OP_PUSHDATA4.to_u8(), 0x08, 0x02, 0, 0];
    rest.extend_from_slice(&chunk);
    assert_eq!(parse_data_script(&MAGIC, &ksn1_bytes(&rest)), Some(chunk));
}

#[test]
fn malformed_data_scripts_are_rejected() {
    let cases: Vec<(&str, ScriptBuf)> = vec![
        (
            "OP_1 chunk",
            ksn1_bytes(&[0x01, 0xaa, OP_PUSHNUM_1.to_u8()]),
        ),
        ("OP_16 chunk", ksn1_bytes(&[OP_PUSHNUM_16.to_u8()])),
        ("OP_1NEGATE chunk", ksn1_bytes(&[OP_PUSHNUM_NEG1.to_u8()])),
        ("OP_RESERVED", ksn1_bytes(&[0x50])),
        (
            "trailing non-push opcode",
            ksn1_bytes(&[0x02, 1, 2, OP_CHECKSIG.to_u8()]),
        ),
        (
            "second OP_RETURN",
            ksn1_bytes(&[0x01, 1, OP_RETURN.to_u8()]),
        ),
        (
            "521-byte chunk",
            ksn1_bytes(&[&[OP_PUSHDATA2.to_u8(), 0x09, 0x02][..], &[7; 521]].concat()),
        ),
        ("truncated direct push", ksn1_bytes(&[0x0a, 1, 2, 3])),
        (
            "truncated PUSHDATA1",
            ksn1_bytes(&[OP_PUSHDATA1.to_u8(), 5, 1, 2]),
        ),
        (
            "PUSHDATA2 missing its length",
            ksn1_bytes(&[OP_PUSHDATA2.to_u8(), 1]),
        ),
        (
            "huge PUSHDATA4",
            ksn1_bytes(&[OP_PUSHDATA4.to_u8(), 0xff, 0xff, 0xff, 0xff, 1, 2]),
        ),
    ];
    for (name, script) in cases {
        // Still a KSN1 output (counts toward "exactly one"), but malformed
        assert!(is_ksn1_output(&MAGIC, &script), "{name}");
        assert_eq!(parse_data_script(&MAGIC, &script), None, "{name}");
    }
}

#[test]
fn scripts_without_the_magic_are_not_ksn1() {
    let wrong_magic = data_script(b"KSN2", b"payload").unwrap();
    let no_return = ScriptBuf::from_bytes([&[0x04][..], &MAGIC, &[0x01, 0x01]].concat());
    let magic_prefix_only =
        ScriptBuf::from_bytes([&[OP_RETURN.to_u8(), 0x05][..], &MAGIC, &[0x00]].concat());
    let truncated_magic = ScriptBuf::from_bytes(vec![OP_RETURN.to_u8(), 0x04, b'K', b'S']);
    let bare_return = ScriptBuf::from_bytes(vec![OP_RETURN.to_u8()]);
    let empty = ScriptBuf::new();
    for script in [
        wrong_magic,
        no_return,
        magic_prefix_only,
        truncated_magic,
        bare_return,
        empty,
    ] {
        assert!(!is_ksn1_output(&MAGIC, &script), "{script:?}");
        assert_eq!(parse_data_script(&MAGIC, &script), None, "{script:?}");
    }
    // Only the magic, no chunks: well-formed, empty payload
    assert_eq!(parse_data_script(&MAGIC, &ksn1_bytes(&[])), Some(vec![]));
}

#[test]
fn empty_payload_cannot_be_written() {
    assert_eq!(data_script(&MAGIC, &[]), Err(DataError::Empty));
}

#[test]
fn only_a_single_well_formed_ksn1_output_is_recognized() {
    let other_return = out(ScriptBuf::new_op_return(b"hello"));
    let other_magic = out(data_script(b"XXXX", b"abc").unwrap());
    let ksn1 = out(data_script(&MAGIC, b"abc").unwrap());
    let malformed = out(ksn1_bytes(&[OP_PUSHNUM_1.to_u8()]));

    // 0 KSN1 outputs (other OP_RETURNs do not count)
    let none = tx(vec![p2tr_out(), other_return.clone(), other_magic.clone()]);
    assert_eq!(tx_data(&MAGIC, &none), TxData::None);
    assert_eq!(tx_payload(&DEVNET, &none), None);

    // Exactly 1
    let one = tx(vec![
        p2tr_out(),
        other_return.clone(),
        ksn1.clone(),
        other_magic.clone(),
    ]);
    assert_eq!(
        tx_data(&MAGIC, &one),
        TxData::One {
            carrier: Carrier::Output(2),
            payload: b"abc".to_vec()
        }
    );

    // 2: not recognized, even when both are well-formed
    let two = tx(vec![ksn1.clone(), p2tr_out(), ksn1.clone()]);
    assert_eq!(tx_data(&MAGIC, &two), TxData::Invalid);
    let two_with_malformed = tx(vec![ksn1.clone(), malformed.clone()]);
    assert_eq!(tx_data(&MAGIC, &two_with_malformed), TxData::Invalid);

    // The only KSN1 output is malformed
    let bad = tx(vec![other_return, malformed, other_magic]);
    assert_eq!(tx_data(&MAGIC, &bad), TxData::Invalid);
}

#[test]
fn tx_payload_checks_length_limit_and_full_consumption() {
    let payload = Payload::NativeOp { op: native_op() }
        .encode(&DEVNET)
        .unwrap();
    let with = |bytes: &[u8]| tx(vec![p2tr_out(), out(data_script(&MAGIC, bytes).unwrap())]);

    assert_eq!(
        tx_payload(&DEVNET, &with(&payload)),
        Some((Carrier::Output(1), Payload::NativeOp { op: native_op() }))
    );

    // Length limit: exactly the limit is fine, 1 byte over is not decoded
    let mut params = DEVNET.clone();
    params.max_block_data_bytes = payload.len();
    assert!(tx_payload(&params, &with(&payload)).is_some());
    params.max_block_data_bytes = payload.len() - 1;
    assert_eq!(tx_payload(&params, &with(&payload)), None);

    // Trailing bytes, truncation, undecodable
    let mut trailing = payload.clone();
    trailing.push(0);
    assert_eq!(tx_payload(&DEVNET, &with(&trailing)), None);
    assert_eq!(
        tx_payload(&DEVNET, &with(&payload[..payload.len() - 1])),
        None
    );
    assert_eq!(tx_payload(&DEVNET, &with(&[0xff, 0xff, 0xff])), None);

    // The magic comes from the params
    let mut other = DEVNET.clone();
    other.magic = *b"KSNX";
    assert_eq!(tx_payload(&other, &with(&payload)), None);
}

#[test]
fn payload_split_across_many_chunks_decodes() {
    // A payload larger than 520 bytes (deploying a longer piece of code) spans several chunks
    let mut op = native_op();
    op.action = Action::Deploy {
        code: payload_of_len(3_000),
        init_args: vec![],
        funds: Vec::new(),
    };
    let payload = Payload::NativeOp { op: op.clone() };
    let bytes = payload.encode(&DEVNET).unwrap();
    assert!(bytes.len() > 5 * MAX_CHUNK);
    let t = tx(vec![out(data_script(&MAGIC, &bytes).unwrap())]);
    assert_eq!(tx_payload(&DEVNET, &t), Some((Carrier::Output(0), payload)));
}

#[test]
fn ksn1_output_value_does_not_matter_for_recognition() {
    let mut data = out(data_script(&MAGIC, b"x").unwrap());
    data.value = Amount::from_sat(1);
    assert!(matches!(
        tx_data(&MAGIC, &tx(vec![data])),
        TxData::One { .. }
    ));
}

proptest! {
    #[test]
    fn roundtrip_random_payloads(payload in proptest::collection::vec(any::<u8>(), 1..4_000)) {
        let script = data_script(&MAGIC, &payload).unwrap();
        prop_assert_eq!(parse_data_script(&MAGIC, &script), Some(payload.clone()));
        let t = tx(vec![p2tr_out(), out(script)]);
        prop_assert_eq!(tx_data(&MAGIC, &t), TxData::One { carrier: Carrier::Output(1), payload });
    }

    /// No byte string makes parsing panic; when it parses, a script rewritten with push_slice parses to the same
    /// result.
    #[test]
    fn parsing_arbitrary_scripts_is_total(bytes in proptest::collection::vec(any::<u8>(), 0..600)) {
        let script = ScriptBuf::from_bytes(bytes);
        if let Some(payload) = parse_data_script(&MAGIC, &script) {
            prop_assert!(is_ksn1_output(&MAGIC, &script));
            if !payload.is_empty() {
                let rebuilt = data_script(&MAGIC, &payload).unwrap();
                prop_assert_eq!(parse_data_script(&MAGIC, &rebuilt), Some(payload));
            }
        }
    }
}
