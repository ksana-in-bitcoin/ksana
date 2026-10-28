//! ksana-primitives unit tests: encoding round trips, canonicality, length limits, encoding layout.

use borsh::{BorshDeserialize, BorshSerialize};
use proptest::prelude::*;

use crate::codec::{decode, encode, CodecError};
use crate::strategies;
use crate::*;

/// Round trip: `decode(encode(v)) == v`, and the JSON representation round-trips as well.
fn assert_roundtrip<T>(value: &T)
where
    T: BorshSerialize
        + BorshDeserialize
        + serde::Serialize
        + serde::de::DeserializeOwned
        + PartialEq
        + std::fmt::Debug,
{
    let bytes = encode(value).unwrap();
    let back: T = decode(&bytes, usize::MAX).unwrap();
    assert_eq!(&back, value);
    let json = serde_json::to_string(value).unwrap();
    let back: T = serde_json::from_str(&json).unwrap();
    assert_eq!(&back, value);
}

/// Canonicality: make one change to a valid encoding (change a byte, truncate or append);
/// if it still decodes after the change, re-encoding must yield exactly the changed bytes.
fn assert_canonical_under_mutation<T>(bytes: &[u8], index: usize, byte: u8, kind: u8)
where
    T: BorshSerialize + BorshDeserialize,
{
    let mut mutated = bytes.to_vec();
    match kind % 3 {
        0 if !mutated.is_empty() => {
            let i = index % mutated.len();
            mutated[i] = byte;
        }
        1 => mutated.truncate(index % (bytes.len() + 1)),
        _ => mutated.push(byte),
    }
    if let Ok(v) = decode::<T>(&mutated, usize::MAX) {
        assert_eq!(encode(&v).unwrap(), mutated);
    }
}

/// Payload canonicality (D-075, D-079, D-080): same as [`assert_canonical_under_mutation`], with the payload's own
/// encoding.
fn assert_payload_canonical_under_mutation(bytes: &[u8], index: usize, byte: u8, kind: u8) {
    let mut mutated = bytes.to_vec();
    match kind % 3 {
        0 if !mutated.is_empty() => {
            let i = index % mutated.len();
            mutated[i] = byte;
        }
        1 => mutated.truncate(index % (bytes.len() + 1)),
        _ => mutated.push(byte),
    }
    let params = Params {
        max_block_data_bytes: usize::MAX,
        ..DEVNET
    };
    if let Ok(v) = Payload::decode(&mutated, &params) {
        assert_eq!(v.encode(&params).unwrap(), mutated);
    }
}

proptest! {
    #[test]
    fn roundtrip_outpoint(v in strategies::outpoint()) { assert_roundtrip(&v); }

    #[test]
    fn roundtrip_fee_mode(v in strategies::fee_mode()) { assert_roundtrip(&v); }

    #[test]
    fn roundtrip_action(v in strategies::action()) { assert_roundtrip(&v); }

    #[test]
    fn roundtrip_sig_scheme(v in strategies::sig_scheme()) { assert_roundtrip(&v); }

    #[test]
    fn roundtrip_vmop(v in strategies::vmop()) {
        assert_roundtrip(&v);
        let bytes = v.encode().unwrap();
        prop_assert_eq!(VmOp::decode(&bytes, &DEVNET).unwrap(), v);
    }

    #[test]
    fn roundtrip_btc_anchor(v in strategies::btc_anchor()) { assert_roundtrip(&v); }

    #[test]
    fn roundtrip_block_header(v in strategies::block_header()) { assert_roundtrip(&v); }

    #[test]
    fn roundtrip_entry(v in strategies::entry()) { assert_roundtrip(&v); }

    #[test]
    fn roundtrip_entry_data(v in strategies::entry_data()) { assert_roundtrip(&v); }

    #[test]
    fn roundtrip_block(v in strategies::block()) { assert_roundtrip(&v); }

    #[test]
    fn roundtrip_payload(v in strategies::payload()) {
        let bytes = v.encode(&DEVNET).unwrap();
        prop_assert_eq!(Payload::decode(&bytes, &DEVNET).unwrap(), v.clone());
        let json = serde_json::to_string(&v).unwrap();
        prop_assert_eq!(serde_json::from_str::<Payload>(&json).unwrap(), v);
    }

    #[test]
    fn vmop_decoding_is_canonical(
        v in strategies::vmop(), index in any::<usize>(), byte in any::<u8>(), kind in any::<u8>()
    ) {
        assert_canonical_under_mutation::<VmOp>(&v.encode().unwrap(), index, byte, kind);
    }

    #[test]
    fn block_decoding_is_canonical(
        v in strategies::block(), index in any::<usize>(), byte in any::<u8>(), kind in any::<u8>()
    ) {
        assert_canonical_under_mutation::<Block>(&encode(&v).unwrap(), index, byte, kind);
    }

    #[test]
    fn payload_decoding_is_canonical(
        v in strategies::payload(), index in any::<usize>(), byte in any::<u8>(), kind in any::<u8>()
    ) {
        assert_payload_canonical_under_mutation(&v.encode(&DEVNET).unwrap(), index, byte, kind);
    }

    #[test]
    fn arbitrary_bytes_never_panic(bytes in proptest::collection::vec(any::<u8>(), 0..512)) {
        let _ = VmOp::decode(&bytes, &DEVNET);
        let _ = Payload::decode(&bytes, &DEVNET);
        let _ = decode::<Block>(&bytes, usize::MAX);
    }

    /// The hand-written header encoding is byte for byte identical to the Borsh encoding.
    #[test]
    fn header_to_bytes_matches_borsh(h in strategies::block_header()) {
        let borsh_bytes = encode(&h).unwrap();
        prop_assert_eq!(borsh_bytes.len(), h.encoded_len());
        prop_assert_eq!(borsh_bytes.len(), BlockHeader::BASE_ENCODED_LEN + 32 * h.parents.len());
        prop_assert_eq!(h.to_bytes(), borsh_bytes);
        prop_assert_eq!(h.hash(), hash::sha256d(&h.to_bytes()));
    }

    /// The signing message content is the prefix of the full encoding without the trailing `borsh(sig)`.
    #[test]
    fn unsigned_encoding_is_prefix(op in strategies::vmop()) {
        let mut expected = op.encode_unsigned().unwrap();
        expected.extend(encode(&op.sig).unwrap());
        prop_assert_eq!(op.encode().unwrap(), expected);
    }

    /// Except for `Forced`, the encodings of `EntryData` and `Entry` are exactly the same.
    #[test]
    fn entry_data_encoding_matches_entry(e in strategies::entry()) {
        let data = EntryData::from(&e);
        let data_bytes = encode(&data).unwrap();
        match &e {
            Entry::Forced { btc_txid, .. } => {
                let mut expected = vec![0u8];
                expected.extend_from_slice(btc_txid);
                prop_assert_eq!(data_bytes, expected);
            }
            _ => prop_assert_eq!(data_bytes, encode(&e).unwrap()),
        }
    }

    #[test]
    fn op_id_is_sha256_of_full_encoding(op in strategies::vmop()) {
        prop_assert_eq!(op.op_id().unwrap(), hash::sha256(&op.encode().unwrap()));
    }
}

fn sample_op() -> VmOp {
    VmOp {
        version: VMOP_VERSION,
        chain_id: 1001,
        account: [0x11; 32],
        nonce: 7,
        fee: FeeMode::Token { amount: 5 },
        max_gas: 1_000_000,
        action: Action::AssetTransfer {
            asset: KSA_ASSET_ID,
            to: [0x22; 32],
            amount: 1000,
        },
        sig: SigScheme::Bip340([0x33; 64]),
    }
}

/// The encoding assembled by hand per the Borsh rules, verifying the field layout independently of the borsh crate
/// (the TS SDK must match it).
#[test]
fn vmop_layout_matches_borsh_spec() {
    let mut expected = Vec::new();
    expected.push(2u8); // version
    expected.extend(1001u32.to_le_bytes()); // chain_id
    expected.extend([0x11; 32]); // account
    expected.extend(7u64.to_le_bytes()); // nonce
    expected.push(0); // FeeMode::Token (0 since path A was deprecated, D-078)
    expected.extend(5u128.to_le_bytes());
    expected.extend(1_000_000u64.to_le_bytes()); // max_gas
    expected.push(2); // Action::AssetTransfer
    expected.extend([0u8; 32]);
    expected.extend([0x22; 32]);
    expected.extend(1000u128.to_le_bytes());
    expected.push(0); // SigScheme::Bip340
    expected.extend([0x33; 64]);
    assert_eq!(sample_op().encode().unwrap(), expected);
}

#[test]
fn call_layout_matches_borsh_spec() {
    let op = VmOp {
        fee: FeeMode::Token { amount: 2500 },
        action: Action::Call {
            contract: [0x55; 32],
            method: "transfer".to_string(),
            args: vec![0xAA, 0xBB],
            funds: vec![Funds {
                asset: [0x66; 32],
                amount: 9,
            }],
        },
        sig: SigScheme::Bip322Simple(vec![0x01, 0x02, 0x03]),
        ..sample_op()
    };
    let mut expected = Vec::new();
    expected.push(2u8);
    expected.extend(1001u32.to_le_bytes());
    expected.extend([0x11; 32]);
    expected.extend(7u64.to_le_bytes());
    expected.push(0); // FeeMode::Token
    expected.extend(2500u128.to_le_bytes());
    expected.extend(1_000_000u64.to_le_bytes());
    expected.push(1); // Action::Call
    expected.extend([0x55; 32]);
    expected.extend(8u32.to_le_bytes());
    expected.extend(b"transfer");
    expected.extend(2u32.to_le_bytes());
    expected.extend([0xAA, 0xBB]);
    expected.extend(1u32.to_le_bytes()); // funds: 1 item
    expected.extend([0x66; 32]);
    expected.extend(9u128.to_le_bytes());
    expected.push(1); // SigScheme::Bip322Simple
    expected.extend(3u32.to_le_bytes());
    expected.extend([0x01, 0x02, 0x03]);
    assert_eq!(op.encode().unwrap(), expected);

    let native = VmOp {
        fee: FeeMode::BtcNative,
        ..sample_op()
    };
    let bytes = native.encode().unwrap();
    assert_eq!(
        bytes[4 + 1 + 32 + 8],
        1,
        "the variant index of FeeMode::BtcNative is 1, with no fields (D-078)"
    );
}

#[test]
fn header_layout_matches_borsh_spec() {
    let h = BlockHeader {
        version: 1,
        height: 2,
        parents: vec![[3; 32], [12; 32]],
        btc_anchor: BtcAnchor {
            height: 4,
            hash: [5; 32],
        },
        entries_root: [6; 32],
        state_root: [7; 32],
        producer: [8; 32],
        timestamp: 9,
        bits: 10,
        nonce: 11,
    };
    let mut expected = Vec::new();
    expected.extend(1u32.to_le_bytes());
    expected.extend(2u64.to_le_bytes());
    expected.extend(2u32.to_le_bytes());
    expected.extend([3; 32]);
    expected.extend([12; 32]);
    expected.extend(4u32.to_le_bytes());
    expected.extend([5; 32]);
    expected.extend([6; 32]);
    expected.extend([7; 32]);
    expected.extend([8; 32]);
    expected.extend(9u64.to_le_bytes());
    expected.extend(10u32.to_le_bytes());
    expected.extend(11u64.to_le_bytes());
    assert_eq!(expected.len(), 232);
    assert_eq!(h.encoded_len(), 232);
    assert_eq!(h.selected_parent(), Some(&[3; 32]));
    assert_eq!(h.to_bytes(), expected);
    assert_eq!(encode(&h).unwrap(), expected);
}

/// Build a Deploy op whose encoded length is exactly `len`.
fn deploy_op_with_encoded_len(len: usize) -> VmOp {
    let mut op = VmOp {
        action: Action::Deploy {
            code: Vec::new(),
            init_args: Vec::new(),
            funds: Vec::new(),
        },
        ..sample_op()
    };
    let base = op.encode().unwrap().len();
    op.action = Action::Deploy {
        code: vec![0u8; len - base],
        init_args: Vec::new(),
        funds: Vec::new(),
    };
    assert_eq!(op.encode().unwrap().len(), len);
    op
}

#[test]
fn vmop_decode_enforces_max_op_bytes() {
    let max = DEVNET.max_op_bytes;
    let at_limit = deploy_op_with_encoded_len(max);
    let bytes = at_limit.encode().unwrap();
    assert_eq!(VmOp::decode(&bytes, &DEVNET).unwrap(), at_limit);

    let over = deploy_op_with_encoded_len(max + 1).encode().unwrap();
    assert_eq!(
        VmOp::decode(&over, &DEVNET),
        Err(CodecError::TooLong { len: max + 1, max })
    );
}

#[test]
fn payload_decode_enforces_max_block_data_bytes() {
    let max = DEVNET.max_block_data_bytes;
    let make = |len: usize| {
        // NativeOp payload = 1-byte variant index + VmOp encoding
        Payload::NativeOp {
            op: deploy_op_with_encoded_len(len - 1),
        }
        .encode(&DEVNET)
        .unwrap()
    };
    let at_limit = make(max);
    assert_eq!(at_limit.len(), max);
    assert!(Payload::decode(&at_limit, &DEVNET).is_ok());
    let over = make(max + 1);
    assert_eq!(
        Payload::decode(&over, &DEVNET),
        Err(CodecError::TooLong { len: max + 1, max })
    );
}

#[test]
fn decode_rejects_trailing_bytes() {
    let mut bytes = sample_op().encode().unwrap();
    bytes.push(0);
    assert!(matches!(
        VmOp::decode(&bytes, &DEVNET),
        Err(CodecError::Decode(_))
    ));
}

#[test]
fn decode_rejects_unknown_variant_tags() {
    let bytes = sample_op().encode().unwrap();
    let fee_tag = 1 + 4 + 32 + 8;
    let mut bad_fee = bytes.clone();
    bad_fee[fee_tag] = 3; // FeeMode only has 0..=2
    assert!(VmOp::decode(&bad_fee, &DEVNET).is_err());

    let mut bad_sig = bytes.clone();
    let sig_tag = bytes.len() - 65;
    assert_eq!(bad_sig[sig_tag], 0);
    bad_sig[sig_tag] = 2; // SigScheme only has 0..=1
    assert!(VmOp::decode(&bad_sig, &DEVNET).is_err());

    assert!(Payload::decode(&[3], &DEVNET).is_err()); // Payload only has 0..=2
    assert!(decode::<Entry>(&[5], usize::MAX).is_err()); // Entry only has 0..=4
}

#[test]
fn decode_rejects_invalid_utf8_method() {
    let op = VmOp {
        action: Action::Call {
            contract: [0; 32],
            method: "ab".to_string(),
            args: Vec::new(),
            funds: Vec::new(),
        },
        ..sample_op()
    };
    let mut bytes = op.encode().unwrap();
    let method_start = 1 + 4 + 32 + 8 + (1 + 16) + 8 + 1 + 32 + 4;
    assert_eq!(&bytes[method_start..method_start + 2], b"ab");
    bytes[method_start] = 0xFF;
    assert!(matches!(
        VmOp::decode(&bytes, &DEVNET),
        Err(CodecError::Decode(_))
    ));
}

#[test]
fn json_representation_is_stable() {
    let json = serde_json::to_value(sample_op()).unwrap();
    assert_eq!(
        json,
        serde_json::json!({
            "version": 2,
            "chain_id": 1001,
            "account": "11".repeat(32),
            "nonce": "7",
            "fee": {"Token": {"amount": "5"}},
            "max_gas": "1000000",
            "action": {"AssetTransfer": {
                "asset": "00".repeat(32),
                "to": "22".repeat(32),
                "amount": "1000"
            }},
            "sig": {"Bip340": "33".repeat(64)}
        })
    );
    let native: VmOp = serde_json::from_value(serde_json::json!({
        "version": 1, "chain_id": 1001, "account": "11".repeat(32), "nonce": "7",
        "fee": "BtcNative", "max_gas": "1000000",
        "action": {"AssetTransfer": {"asset": "00".repeat(32), "to": "22".repeat(32), "amount": "1000"}},
        "sig": {"Bip340": "33".repeat(64)}
    }))
    .unwrap();
    assert_eq!(native.fee, FeeMode::BtcNative);

    // Attached funds (D-073): the amount is a decimal string like every other u128
    let call = Action::Call {
        contract: [0x55; 32],
        method: "swap".to_string(),
        args: vec![0xAA],
        funds: vec![Funds {
            asset: KSA_ASSET_ID,
            amount: 1_000,
        }],
    };
    assert_eq!(
        serde_json::to_value(&call).unwrap(),
        serde_json::json!({"Call": {
            "contract": "55".repeat(32),
            "method": "swap",
            "args": "aa",
            "funds": [{"asset": "00".repeat(32), "amount": "1000"}]
        }})
    );
}

#[test]
fn json_rejects_unknown_fields_and_bad_hex() {
    let mut json = serde_json::to_value(sample_op()).unwrap();
    json["extra"] = serde_json::json!(1);
    assert!(serde_json::from_value::<VmOp>(json).is_err());

    let mut json = serde_json::to_value(sample_op()).unwrap();
    json["account"] = serde_json::json!("11".repeat(31));
    assert!(serde_json::from_value::<VmOp>(json).is_err());

    let mut json = serde_json::to_value(sample_op()).unwrap();
    json["nonce"] = serde_json::json!(7);
    assert!(serde_json::from_value::<VmOp>(json).is_err());
}

#[test]
fn funds_rules() {
    let f = |asset: u8, amount: u128| Funds {
        asset: [asset; 32],
        amount,
    };
    assert_eq!(check_funds(&[]), Ok(()));
    assert_eq!(check_funds(&[f(1, 5), f(2, 1)]), Ok(()));
    assert!(check_funds(&[f(1, 0)]).is_err(), "amount is 0");
    assert!(check_funds(&[f(2, 1), f(1, 1)]).is_err(), "descending");
    assert!(check_funds(&[f(1, 1), f(1, 2)]).is_err(), "duplicate");
    let max: Vec<Funds> = (0..MAX_CALL_FUNDS)
        .map(|i| f(u8::try_from(i).unwrap(), 1))
        .collect();
    assert_eq!(check_funds(&max), Ok(()));
    let mut over = max.clone();
    over.push(f(0xff, 1));
    assert!(check_funds(&over).is_err(), "exceeds the limit");
}

fn sample_header(height: u64) -> BlockHeader {
    BlockHeader {
        version: BLOCK_VERSION,
        height,
        parents: vec![[u8::try_from(height).unwrap(); 32]],
        btc_anchor: BtcAnchor {
            height: 100,
            hash: [7; 32],
        },
        entries_root: [1; 32],
        state_root: [2; 32],
        producer: [3; 32],
        timestamp: 1_700_000_000 + height,
        bits: 0x207f_ffff,
        nonce: height,
    }
}

/// The `bodies` of settlement and republish payloads (D-077): canonical ones round-trip; indices not increasing, out of
/// range, a settlement payload pointing at this block, containing path A entries, or containing only
/// forced-inclusion entries all fail to decode.
#[test]
fn payload_bodies_must_be_canonical() {
    let headers: Vec<BlockHeader> = (1..=3).map(sample_header).collect();
    let sponsored = EntryData::Sponsored { op: sample_op() };
    let forced = EntryData::Forced { btc_txid: [9; 32] };
    let body = |header: u32, entries: Vec<EntryData>| Body { header, entries };
    let settlement = |bodies: Vec<Body>| Payload::Settlement {
        headers: headers.clone(),
        entries: vec![sponsored.clone()],
        bodies,
    };
    let republish = |bodies: Vec<Body>| Payload::Headers {
        headers: headers.clone(),
        bodies,
        payee: [7; 32],
    };
    let decoded = |p: &Payload| Payload::decode(&p.encode(&DEVNET).unwrap(), &DEVNET);

    for p in [
        settlement(vec![]),
        settlement(vec![
            body(0, vec![forced.clone(), sponsored.clone()]),
            body(1, vec![sponsored.clone()]),
        ]),
        republish(vec![body(2, vec![sponsored.clone()])]),
    ] {
        assert_eq!(decoded(&p), Ok(p));
    }
    let rejected = [
        (
            settlement(vec![
                body(1, vec![sponsored.clone()]),
                body(1, vec![sponsored.clone()]),
            ]),
            "body indices must be strictly increasing",
        ),
        (
            settlement(vec![body(2, vec![sponsored.clone()])]),
            "a body in a settlement payload cannot point at this block's own header",
        ),
        (
            republish(vec![body(3, vec![sponsored.clone()])]),
            "body index exceeds the number of headers",
        ),
        (
            republish(vec![body(0, vec![forced.clone()])]),
            "a body must contain entries other than forced inclusions",
        ),
        (
            republish(vec![body(0, vec![])]),
            "a body must contain entries other than forced inclusions",
        ),
    ];
    for (p, message) in rejected {
        match decoded(&p) {
            Err(CodecError::Decode(m)) => assert!(m.contains(message), "{m}"),
            other => panic!("must fail to decode ({message}): {other:?}"),
        }
    }
}

/// A header sequence holds at most `MAX_SETTLEMENT_HEADERS` headers in both settlement and republish payloads (§7.1,
/// D-100): one more fails to decode, before any header is read.
#[test]
fn payload_decode_enforces_max_settlement_headers() {
    let params = Params {
        max_settlement_headers: 3,
        ..DEVNET.clone()
    };
    let sponsored = EntryData::Sponsored { op: sample_op() };
    let settlement = |count: u64| Payload::Settlement {
        headers: (1..=count).map(sample_header).collect(),
        entries: vec![sponsored.clone()],
        bodies: Vec::new(),
    };
    let republish = |count: u64| Payload::Headers {
        headers: (1..=count).map(sample_header).collect(),
        bodies: Vec::new(),
        payee: [7; 32],
    };
    for (at_limit, over) in [(settlement(3), settlement(4)), (republish(3), republish(4))] {
        let bytes = at_limit.encode(&params).unwrap();
        assert_eq!(Payload::decode(&bytes, &params), Ok(at_limit));
        match Payload::decode(&over.encode(&params).unwrap(), &params) {
            Err(CodecError::Decode(m)) => assert!(m.contains("too many headers"), "{m}"),
            other => panic!("must fail to decode: {other:?}"),
        }
    }
    // the count is checked first: a count above the limit with nothing after it is rejected for the count
    let mut bytes = vec![2];
    bytes.extend_from_slice(&4u32.to_le_bytes());
    match Payload::decode(&bytes, &params) {
        Err(CodecError::Decode(m)) => assert!(m.contains("too many headers"), "{m}"),
        other => panic!("must fail to decode: {other:?}"),
    }
}
